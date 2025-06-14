use std::io;
use std::os::fd::AsFd;
use nix::pty;
use nix::sys::select;
use nix::unistd;
use nix::libc;
use nix::sys::signal;
use nix::sys::termios;

fn main() -> io::Result<()> {
    // Optional: set PTY window size (use actual terminal size in a full version)
    let winsize = pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // unsafe {
    //     if libc::ioctl(0, libc::TIOCNOTTY) < 0 {
    //         panic!("TIOCNOTTY failed");
    //     }
    // }

    // Fork into parent (controller) and child (shell) processes
    unsafe {
        match pty::forkpty(Some(&winsize), None) {
            Ok(fork_result) => {
                match fork_result {
                    pty::ForkptyResult::Child => {
                        let cmd = c"bash";
                        let args = vec![cmd];

                        libc::setsid();

                        if libc::ioctl(0, libc::TIOCSCTTY) < 0 {
                            panic!("child: TIOCSCTTY failed");
                        }

                        unistd::execvp(cmd, &args).unwrap();
                    }

                    pty::ForkptyResult::Parent { child, master } => {
                        let stdin = io::stdin();
                        let stdout = io::stdout();
                        let stdin_fd = stdin.as_fd();
                        let stdout_fd = stdout.as_fd();
                        let master_fd = master.as_fd();

                        let sa_default = signal::SigAction::new(
                            signal::SigHandler::SigDfl,
                            signal::SaFlags::SA_RESTART,
                            signal::SigSet::empty(),
                        );

                        for sig in vec![
                            signal::Signal::SIGPIPE,
                            signal::Signal::SIGTSTP,
                        ] {
                            signal::sigaction(sig, &sa_default).unwrap();
                        }

                        let sa_ignore = signal::SigAction::new(
                            signal::SigHandler::SigIgn,
                            signal::SaFlags::empty(),
                            signal::SigSet::empty(),
                        );

                        for sig in vec![
                            signal::Signal::SIGPIPE,
                            signal::Signal::SIGTSTP,
                            signal::Signal::SIGINT,
                            signal::Signal::SIGQUIT,
                            signal::Signal::SIGHUP,
                            signal::Signal::SIGCHLD,
                            signal::Signal::SIGCONT,
                            signal::Signal::SIGTERM,
                            signal::Signal::SIGUSR1,
                            signal::Signal::SIGUSR2,
                            signal::Signal::SIGWINCH,
                        ] {
                            signal::sigaction(sig, &sa_ignore).unwrap();
                        }


                        let mut read_fds_orig = select::FdSet::new();
                        read_fds_orig.insert(stdin_fd);
                        read_fds_orig.insert(master_fd);

                        loop {
                            let mut read_fds = read_fds_orig.clone();

                            select::select(None, &mut read_fds, None, None, None).unwrap();

                            if read_fds.contains(master_fd) {
                                let mut buf = [0u8; 1024];

                                match unistd::read(master_fd, &mut buf) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        let _ = unistd::write(stdout_fd, &buf[..n]);
                                    },
                                    Err(_) => break,
                                }
                            }

                            if read_fds.contains(stdin_fd) {
                                let mut buf = [0u8; 1024];

                                match unistd::read(stdin_fd, &mut buf) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        let _ = unistd::write(master_fd, &buf[..n]);
                                    },
                                    Err(err) => {
                                        println!("STDIN ERR: {}", err);
                                    },
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("forkpty failed: {}", e);
            }
        }
    }

    Ok(())
}
