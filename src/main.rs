mod bar;
mod cli;
mod client;
mod keybinds;
mod layout;
mod mux;
mod pane;
mod protocol;
mod pty;
mod server;
mod session;

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use nix::sys::signal;
use nix::sys::termios;

pub static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigwinch(_: nix::libc::c_int) {
    SIGWINCH_RECEIVED.store(true, Ordering::Relaxed);
}

/// Query the terminal size from `fd` via ioctl(TIOCGWINSZ).
pub fn get_winsize(fd: BorrowedFd) -> nix::pty::Winsize {
    let mut ws: nix::pty::Winsize = unsafe { std::mem::zeroed() };
    unsafe {
        nix::libc::ioctl(fd.as_raw_fd(), nix::libc::TIOCGWINSZ, &mut ws);
    }
    ws
}

/// Switch `fd` to raw mode and return the original settings for later restore.
fn enter_raw_mode(fd: BorrowedFd) -> termios::Termios {
    let orig = termios::tcgetattr(fd).unwrap();
    let mut raw = orig.clone();
    termios::cfmakeraw(&mut raw);
    termios::tcsetattr(fd, termios::SetArg::TCSANOW, &raw).unwrap();
    orig
}

/// Check if a shell path is valid: absolute path and executable.
fn check_shell(shell: &str) -> bool {
    shell.starts_with('/')
        && nix::unistd::access(shell, nix::unistd::AccessFlags::X_OK).is_ok()
}

/// Resolve the user's shell.
/// $SHELL -> getpwuid() -> /bin/sh
pub fn resolve_shell() -> std::ffi::CString {
    use std::ffi::CString;
    use nix::unistd;

    if let Ok(shell) = std::env::var("SHELL") {
        if check_shell(&shell) {
            return CString::new(shell).unwrap();
        }
    }
    if let Some(pw_shell) = unistd::User::from_uid(unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.shell)
    {
        if let Some(s) = pw_shell.to_str() {
            if check_shell(s) {
                return CString::new(s).unwrap();
            }
        }
    }
    CString::new("/bin/sh").unwrap()
}

/// Run as a client: raw mode, alternate screen, connect to server, forward I/O.
fn run_client(socket_path: &std::path::Path, session_name: &str) -> io::Result<()> {
    if std::env::var_os("DTM").is_some() {
        eprintln!("dtm sessions cannot be nested");
        std::process::exit(1);
    }

    let stdin = io::stdin();
    let stdout = io::stdout();

    // Install SIGWINCH handler.
    let sa = signal::SigAction::new(
        signal::SigHandler::Handler(handle_sigwinch),
        signal::SaFlags::SA_RESTART,
        signal::SigSet::empty(),
    );
    unsafe { signal::sigaction(signal::Signal::SIGWINCH, &sa).unwrap(); }

    let orig_termios = enter_raw_mode(stdin.as_fd());

    // Enter alternate screen buffer, clear it, enable mouse tracking (SGR mode).
    let _ = nix::unistd::write(stdout.as_fd(), b"\x1B[?1049h\x1B[2J\x1B[H\x1B[?1000h\x1B[?1006h");

    let mut client = client::Client::connect(socket_path)?;
    let result = client.run(stdin.as_fd(), stdout.as_fd());

    // Disable mouse tracking, leave alternate screen buffer, show cursor.
    let _ = nix::unistd::write(stdout.as_fd(), b"\x1B[?1006l\x1B[?1000l\x1B[?1049l\x1B[?25h");
    let _ = termios::tcsetattr(stdin.as_fd(), termios::SetArg::TCSANOW, &orig_termios);

    match result {
        Ok(Some(code)) => std::process::exit(code),
        Ok(None) => {
            println!("detached session: {}", session_name);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn main() -> io::Result<()> {
    let cmd = cli::parse_args();

    match cmd {
        cli::Command::Default => {
            // If sessions exist, attach to most recent alive one.
            let sessions = session::list_sessions();
            if let Some(s) = sessions.iter().rev().find(|s| s.alive) {
                let path = session::socket_path(&s.name);
                run_client(&path, &s.name)?;
            } else {
                // Clean up stale sessions.
                for s in &sessions {
                    if !s.alive {
                        session::cleanup_stale(&s.name);
                    }
                }
                // Create new session.
                let name = session::generate_name();
                let path = session::socket_path(&name);
                let shell = resolve_shell();
                server::start_server(&path, &name, &shell)?;
                run_client(&path, &name)?;
            }
        }
        cli::Command::New { name } => {
            let name = name.unwrap_or_else(|| session::generate_name());
            let path = session::socket_path(&name);
            if path.exists() {
                // Check if alive via PID file.
                if session::read_pid(&name).is_some_and(|pid| {
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
                }) {
                    eprintln!("session '{}' already exists", name);
                    std::process::exit(1);
                }
                // Stale socket, clean up.
                session::cleanup_stale(&name);
            }
            let shell = resolve_shell();
            server::start_server(&path, &name, &shell)?;
            run_client(&path, &name)?;
        }
        cli::Command::Attach { target } => {
            let sessions = session::list_sessions();
            let name = match target {
                Some(t) => t,
                None => {
                    // Attach to most recent alive session.
                    match sessions.iter().rev().find(|s| s.alive) {
                        Some(s) => s.name.clone(),
                        None => {
                            eprintln!("no sessions to attach to");
                            std::process::exit(1);
                        }
                    }
                }
            };
            let path = session::socket_path(&name);
            if !path.exists() || !session::read_pid(&name).is_some_and(|pid| {
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
            }) {
                eprintln!("session '{}' not found", name);
                std::process::exit(1);
            }
            run_client(&path, &name)?;
        }
        cli::Command::List => {
            let sessions = session::list_sessions();
            if sessions.is_empty() {
                println!("no sessions");
            } else {
                for s in &sessions {
                    let status = if s.alive { "" } else { " (dead)" };
                    println!("{}{}", s.name, status);
                }
            }
        }
        cli::Command::Kill { target } => {
            let sessions = session::list_sessions();
            let name = match target {
                Some(t) => t,
                None => {
                    match sessions.iter().rev().find(|s| s.alive) {
                        Some(s) => s.name.clone(),
                        None => {
                            eprintln!("no sessions to kill");
                            std::process::exit(1);
                        }
                    }
                }
            };
            let path = session::socket_path(&name);
            if !path.exists() {
                eprintln!("session '{}' not found", name);
                std::process::exit(1);
            }
            if let Some(pid) = session::read_pid(&name) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    signal::Signal::SIGTERM,
                );
            } else {
                // No PID file, stale session.
                session::cleanup_stale(&name);
            }
        }
    }

    Ok(())
}
