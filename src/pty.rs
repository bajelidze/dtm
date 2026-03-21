use std::ffi::CStr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use nix::pty::{self, Winsize};
use nix::unistd::{self, Pid};

pub struct Pty {
    master: OwnedFd,
    child: Pid,
}

impl Pty {
    /// Fork a PTY and exec `cmd` in the child. Returns the parent-side handle.
    pub fn spawn(winsize: &Winsize, cmd: &CStr) -> nix::Result<Self> {
        unsafe {
            match pty::forkpty(Some(winsize), None)? {
                pty::ForkptyResult::Child => {
                    unistd::execvp(cmd, &[cmd])?;
                    unreachable!()
                }
                pty::ForkptyResult::Parent { child, master } => {
                    Ok(Self { master, child })
                }
            }
        }
    }

    pub fn master_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }

    pub fn child_pid(&self) -> Pid {
        self.child
    }

    pub fn resize(&self, winsize: &Winsize) -> nix::Result<()> {
        unsafe {
            let ret = nix::libc::ioctl(
                std::os::unix::io::AsRawFd::as_raw_fd(&self.master),
                nix::libc::TIOCSWINSZ,
                winsize as *const Winsize,
            );
            if ret < 0 {
                Err(nix::Error::last())
            } else {
                Ok(())
            }
        }
    }
}
