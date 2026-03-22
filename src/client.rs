use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::Ordering;
use nix::errno::Errno;
use nix::sys::select;
use nix::unistd;

use crate::protocol::{ClientMsg, FrameReader, ServerMsg, write_msg};
use crate::{SIGWINCH_RECEIVED, get_winsize};

pub struct Client {
    stream: UnixStream,
    reader: FrameReader,
}

impl Client {
    pub fn connect(socket_path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(socket_path)?;
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            reader: FrameReader::new(),
        })
    }

    /// Run the client event loop.
    /// Returns Some(exit_code) if the server sent Exit, None if disconnected.
    pub fn run(&mut self, stdin_fd: BorrowedFd, stdout_fd: BorrowedFd) -> io::Result<Option<i32>> {
        let stdin_raw: RawFd = stdin_fd.as_raw_fd();
        let stream_raw: RawFd = self.stream.as_raw_fd();

        // Send initial resize so server knows our terminal size.
        let ws = get_winsize(stdin_fd);
        let msg = ClientMsg::Resize { rows: ws.ws_row, cols: ws.ws_col }.encode();
        write_msg(self.stream.as_fd(), &msg)?;

        loop {
            let mut read_fds = select::FdSet::new();
            unsafe {
                read_fds.insert(BorrowedFd::borrow_raw(stdin_raw));
                read_fds.insert(BorrowedFd::borrow_raw(stream_raw));
            }

            match select::select(None, &mut read_fds, None, None, None) {
                Err(Errno::EINTR) => {
                    if SIGWINCH_RECEIVED.swap(false, Ordering::Relaxed) {
                        let ws = get_winsize(stdin_fd);
                        let msg = ClientMsg::Resize { rows: ws.ws_row, cols: ws.ws_col }.encode();
                        write_msg(self.stream.as_fd(), &msg)?;
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
                Ok(_) => {}
            }

            // Read from stdin, forward to server.
            let stdin_ready = unsafe { read_fds.contains(BorrowedFd::borrow_raw(stdin_raw)) };
            if stdin_ready {
                let mut buf = [0u8; 4096];
                match unistd::read(stdin_fd, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let msg = ClientMsg::Input(buf[..n].to_vec()).encode();
                        write_msg(self.stream.as_fd(), &msg)?;
                    }
                }
            }

            // Read from server, write to stdout.
            let stream_ready = unsafe { read_fds.contains(BorrowedFd::borrow_raw(stream_raw)) };
            if stream_ready {
                let mut buf = [0u8; 8192];
                match unistd::read(self.stream.as_fd(), &mut buf) {
                    Ok(0) => {
                        return Ok(None);
                    }
                    Ok(n) => {
                        self.reader.feed(&buf[..n]);
                        while let Some(frame) = self.reader.next_frame() {
                            match ServerMsg::decode(&frame) {
                                Some(ServerMsg::Output(data)) => {
                                    let _ = unistd::write(stdout_fd, &data);
                                }
                                Some(ServerMsg::Exit(code)) => {
                                    return Ok(Some(code));
                                }
                                None => {}
                            }
                        }
                    }
                    Err(Errno::EAGAIN) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }

        Ok(None)
    }
}
