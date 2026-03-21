mod keybinds;
mod mux;
mod pane;
mod pty;

use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use nix::pty::Winsize;
use nix::sys::signal;
use nix::sys::termios;
use nix::unistd;

static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigwinch(_: nix::libc::c_int) {
    // Just toggle the boolean and do the actual resizing
    // when select returns EINTR.
    SIGWINCH_RECEIVED.store(true, Ordering::Relaxed);
}

/// Query the terminal size from `fd` via ioctl(TIOCGWINSZ).
pub fn get_winsize(fd: BorrowedFd) -> Winsize {
    let mut ws: Winsize = unsafe { std::mem::zeroed() };
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
/// $SHELL → getpwuid() → /bin/sh
fn resolve_shell() -> CString {
    // 1. $SHELL environment variable.
    if let Ok(shell) = std::env::var("SHELL") {
        if check_shell(&shell) {
            return CString::new(shell).unwrap();
        }
    }

    // 2. passwd entry for current user.
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

    // 3. Fallback.
    CString::new("/bin/sh").unwrap()
}

fn main() -> io::Result<()> {
    if std::env::var_os("DTM").is_some() {
        eprintln!("dtm sessions cannot be nested");
        std::process::exit(1);
    }
    // Safe: single-threaded, called before spawning any child processes.
    unsafe { std::env::set_var("DTM", "1"); }

    let stdin = io::stdin();
    let stdout = io::stdout();

    let shell = resolve_shell();
    let winsize = get_winsize(stdin.as_fd());
    let initial_pty = pty::Pty::spawn(&winsize, &shell).unwrap();
    let initial_pane = pane::Pane::new(initial_pty, winsize.ws_row, winsize.ws_col);
    let mut mux = mux::Mux::new(initial_pane, shell);

    // Install SIGWINCH handler.
    let sa = signal::SigAction::new(
        signal::SigHandler::Handler(handle_sigwinch),
        signal::SaFlags::SA_RESTART,
        signal::SigSet::empty(),
    );
    unsafe { signal::sigaction(signal::Signal::SIGWINCH, &sa).unwrap(); }

    let orig_termios = enter_raw_mode(stdin.as_fd());

    // Enter alternate screen buffer and clear it.
    let _ = nix::unistd::write(stdout.as_fd(), b"\x1B[?1049h\x1B[2J\x1B[H");

    mux.run(stdin.as_fd(), stdout.as_fd());

    // Leave alternate screen buffer (restores original content) and show cursor.
    let _ = nix::unistd::write(stdout.as_fd(), b"\x1B[?1049l\x1B[?25h");
    let _ = termios::tcsetattr(stdin.as_fd(), termios::SetArg::TCSANOW, &orig_termios);

    Ok(())
}
