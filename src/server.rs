use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use nix::errno::Errno;
use nix::sys::select;
use nix::sys::signal;
use nix::unistd;

use crate::mux::Mux;
use crate::pane::Pane;
use crate::protocol::{ClientMsg, FrameReader, ServerMsg, write_msg};
use crate::pty::Pty;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_shutdown(_: nix::libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

struct ClientConn {
    stream: UnixStream,
    reader: FrameReader,
}

pub struct Server {
    mux: Mux,
    listener: UnixListener,
    /// The active client (at most one). Only set after a connection sends Resize.
    active_client: Option<ClientConn>,
    /// Newly accepted connections that haven't sent Resize yet.
    /// These are probes or clients that haven't identified themselves.
    pending: Vec<ClientConn>,
    socket_path: PathBuf,
    session_name: String,
}

/// Create the server listener, fork, and return the socket path to the parent.
/// The child becomes the daemon and never returns from this function.
pub fn start_server(socket_path: &Path, session_name: &str, shell: &CString) -> io::Result<()> {
    // Bind before fork so parent can connect immediately.
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;

    match unsafe { unistd::fork() } {
        Ok(unistd::ForkResult::Parent { .. }) => {
            // Parent: drop listener (child owns it), return.
            drop(listener);
            Ok(())
        }
        Ok(unistd::ForkResult::Child) => {
            // Become session leader, detach from controlling terminal.
            unistd::setsid().ok();

            // Redirect stdin/stdout/stderr to /dev/null.
            if let Ok(devnull) = std::fs::File::open("/dev/null") {
                let fd = devnull.as_fd().as_raw_fd();
                unsafe {
                    nix::libc::dup2(fd, 0);
                    nix::libc::dup2(fd, 1);
                    nix::libc::dup2(fd, 2);
                }
            }

            // Ignore SIGPIPE, SIGHUP.
            unsafe {
                let ignore = signal::SigAction::new(
                    signal::SigHandler::SigIgn,
                    signal::SaFlags::empty(),
                    signal::SigSet::empty(),
                );
                let _ = signal::sigaction(signal::Signal::SIGPIPE, &ignore);
                let _ = signal::sigaction(signal::Signal::SIGHUP, &ignore);

                // Handle SIGTERM/SIGINT for graceful shutdown.
                // Do NOT use SA_RESTART so select() returns EINTR.
                let sa = signal::SigAction::new(
                    signal::SigHandler::Handler(handle_shutdown),
                    signal::SaFlags::empty(),
                    signal::SigSet::empty(),
                );
                let _ = signal::sigaction(signal::Signal::SIGTERM, &sa);
                let _ = signal::sigaction(signal::Signal::SIGINT, &sa);
            }

            // Write PID file.
            crate::session::write_pid(session_name, std::process::id());

            // Set DTM env to prevent nesting.
            unsafe { std::env::set_var("DTM", "1"); }

            // Create initial PTY and Mux.
            // Use a default size; the client will send Resize immediately.
            let ws = nix::pty::Winsize {
                ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0,
            };
            let pty = Pty::spawn(&ws, shell).expect("failed to spawn pty");
            let pane = Pane::new(pty, ws.ws_row - 1, ws.ws_col);
            let mux = Mux::new(pane, shell.clone(), ws.ws_row, ws.ws_col);

            let mut server = Server {
                mux,
                listener,
                active_client: None,
                pending: Vec::new(),
                socket_path: socket_path.to_path_buf(),
                session_name: session_name.to_string(),
            };

            server.run();
            std::process::exit(0);
        }
        Err(e) => Err(e.into()),
    }
}

impl Server {
    fn run(&mut self) {
        let listener_raw: RawFd = self.listener.as_raw_fd();

        loop {
            if SHUTDOWN.load(Ordering::Relaxed) {
                self.shutdown();
                return;
            }

            let pty_fds = self.mux.pty_fds();

            let mut read_fds = select::FdSet::new();
            unsafe {
                read_fds.insert(BorrowedFd::borrow_raw(listener_raw));
                for &(_, raw) in &pty_fds {
                    read_fds.insert(BorrowedFd::borrow_raw(raw));
                }
                if let Some(ref client) = self.active_client {
                    read_fds.insert(BorrowedFd::borrow_raw(client.stream.as_raw_fd()));
                }
                for pending in &self.pending {
                    read_fds.insert(BorrowedFd::borrow_raw(pending.stream.as_raw_fd()));
                }
            }

            match select::select(None, &mut read_fds, None, None, None) {
                Err(Errno::EINTR) => continue,
                Err(_) => break,
                Ok(_) => {}
            }

            // Check for new connections.
            let listener_ready = unsafe {
                read_fds.contains(BorrowedFd::borrow_raw(listener_raw))
            };
            if listener_ready {
                self.accept_connection();
            }

            // Process pending connections — promote or discard.
            self.process_pending(&mut read_fds);

            // Process PTY output.
            let ready_keys: Vec<(usize, RawFd)> = pty_fds.iter()
                .filter(|(_, raw)| unsafe { read_fds.contains(BorrowedFd::borrow_raw(*raw)) })
                .copied()
                .collect();

            if !ready_keys.is_empty() {
                let (output, all_dead) = self.mux.read_panes(&ready_keys);
                if !output.is_empty() {
                    self.send_output(&output);
                }
                if all_dead {
                    self.send_exit(0);
                    self.cleanup();
                    return;
                }
            }

            // Process active client input.
            if let Some(ref client) = self.active_client {
                let raw = client.stream.as_raw_fd();
                if unsafe { read_fds.contains(BorrowedFd::borrow_raw(raw)) } {
                    match self.read_active_client() {
                        ClientAction::Continue => {}
                        ClientAction::Disconnect => {
                            self.active_client = None;
                        }
                    }
                }
            }
        }
    }

    /// Accept a new connection into the pending list.
    /// Does NOT kick the active client.
    fn accept_connection(&mut self) {
        let (stream, _) = match self.listener.accept() {
            Ok(s) => s,
            Err(_) => return,
        };
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        self.pending.push(ClientConn {
            stream,
            reader: FrameReader::new(),
        });
    }

    /// Read from pending connections. If one sends Resize, promote it
    /// to active (kicking the old active client). If one closes or sends
    /// garbage, discard it.
    fn process_pending(&mut self, read_fds: &mut select::FdSet) {
        let mut promote_idx: Option<usize> = None;
        let mut promote_resize: Option<(u16, u16)> = None;
        let mut dead: Vec<usize> = Vec::new();

        for (i, pending) in self.pending.iter_mut().enumerate() {
            let raw = pending.stream.as_raw_fd();
            if !unsafe { read_fds.contains(BorrowedFd::borrow_raw(raw)) } {
                continue;
            }

            let mut buf = [0u8; 4096];
            let n = match unistd::read(pending.stream.as_fd(), &mut buf) {
                Ok(0) => { dead.push(i); continue; }
                Ok(n) => n,
                Err(Errno::EAGAIN) => continue,
                Err(_) => { dead.push(i); continue; }
            };

            pending.reader.feed(&buf[..n]);

            while let Some(frame) = pending.reader.next_frame() {
                if let Some(ClientMsg::Resize { rows, cols }) = ClientMsg::decode(&frame) {
                    promote_idx = Some(i);
                    promote_resize = Some((rows, cols));
                    break;
                }
                // Non-Resize messages from pending connections are ignored.
            }

            if promote_idx.is_some() {
                break;
            }
        }

        // Remove dead connections (reverse order to preserve indices).
        for &i in dead.iter().rev() {
            self.pending.remove(i);
        }

        // Promote one pending connection to active.
        if let Some(idx) = promote_idx {
            let new_client = self.pending.remove(idx);
            self.active_client = Some(new_client);
            self.pending.clear();

            // Handle the Resize immediately so the first render uses
            // the correct terminal dimensions.
            if let Some((rows, cols)) = promote_resize {
                self.mux.handle_resize(rows, cols);
                let output = self.mux.full_render();
                self.send_output(&output);
            }
        }
    }

    fn read_active_client(&mut self) -> ClientAction {
        let client = self.active_client.as_mut().unwrap();
        let mut buf = [0u8; 4096];
        let n = match unistd::read(client.stream.as_fd(), &mut buf) {
            Ok(0) => return ClientAction::Disconnect,
            Ok(n) => n,
            Err(Errno::EAGAIN) | Err(Errno::EINTR) => return ClientAction::Continue,
            Err(_) => return ClientAction::Disconnect,
        };

        client.reader.feed(&buf[..n]);

        let mut output = Vec::new();
        while let Some(frame) = client.reader.next_frame() {
            match ClientMsg::decode(&frame) {
                Some(ClientMsg::Input(data)) => {
                    let result = self.mux.process_stdin(&data);
                    if !result.output.is_empty() {
                        output.extend_from_slice(&result.output);
                    }
                    if !result.forward.is_empty() {
                        self.mux.write_to_active(&result.forward);
                    }
                    if result.detach {
                        return ClientAction::Disconnect;
                    }
                }
                Some(ClientMsg::Resize { rows, cols }) => {
                    self.mux.handle_resize(rows, cols);
                    output.extend_from_slice(&self.mux.full_render());
                }
                Some(ClientMsg::Detach) => {
                    return ClientAction::Disconnect;
                }
                None => {}
            }
        }

        if !output.is_empty() {
            self.send_output(&output);
        }

        ClientAction::Continue
    }

    fn send_output(&self, data: &[u8]) {
        if let Some(ref client) = self.active_client {
            let encoded = ServerMsg::Output(data.to_vec()).encode();
            let _ = write_msg(client.stream.as_fd(), &encoded);
        }
    }

    fn send_exit(&self, code: i32) {
        if let Some(ref client) = self.active_client {
            let encoded = ServerMsg::Exit(code).encode();
            let _ = write_msg(client.stream.as_fd(), &encoded);
        }
    }

    fn shutdown(&mut self) {
        self.send_exit(0);
        self.cleanup();
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(crate::session::pid_path(&self.session_name));
    }
}

enum ClientAction {
    Continue,
    Disconnect,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.cleanup();
    }
}
