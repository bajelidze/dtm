mod mux;
mod pane;
mod pty;

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use nix::pty::Winsize;
use nix::sys::signal;
use nix::sys::termios;

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

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();

    let winsize = get_winsize(stdin.as_fd());
    let initial_pty = pty::Pty::spawn(&winsize, c"bash").unwrap();
    let initial_pane = pane::Pane::new(initial_pty, winsize.ws_row, winsize.ws_col);
    let mut mux = mux::Mux::new(initial_pane);

    // Install SIGWINCH handler.
    let sa = signal::SigAction::new(
        signal::SigHandler::Handler(handle_sigwinch),
        signal::SaFlags::SA_RESTART,
        signal::SigSet::empty(),
    );
    unsafe { signal::sigaction(signal::Signal::SIGWINCH, &sa).unwrap(); }

    let orig_termios = enter_raw_mode(stdin.as_fd());

    mux.run(stdin.as_fd(), stdout.as_fd());

    let _ = termios::tcsetattr(stdin.as_fd(), termios::SetArg::TCSANOW, &orig_termios);

    Ok(())
}
