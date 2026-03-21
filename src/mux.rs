use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::sync::atomic::Ordering;
use nix::errno::Errno;
use nix::sys::select;
use nix::unistd;
use alacritty_terminal::term::TermMode;
use crate::keybinds::{Action, Keybinds};
use crate::pane::Pane;
use crate::pty::Pty;
use crate::SIGWINCH_RECEIVED;
use crate::get_winsize;

pub struct Mux {
    panes: BTreeMap<usize, Pane>,
    active: usize,
    keybinds: Keybinds,
    shell: CString,
}

impl Mux {
    pub fn new(initial: Pane, shell: CString) -> Self {
        let mut panes = BTreeMap::new();
        panes.insert(1, initial);
        Self { panes, active: 1, keybinds: Keybinds::new(), shell }
    }

    fn handle_tab(&mut self, tab_num: usize, stdin_fd: BorrowedFd, stdout_fd: BorrowedFd) {
        if tab_num == self.active {
            return;
        }

        if !self.panes.contains_key(&tab_num) {
            let ws = get_winsize(stdin_fd);
            if let Ok(pty) = Pty::spawn(&ws, &self.shell) {
                let pane = Pane::new(pty, ws.ws_row, ws.ws_col);
                self.panes.insert(tab_num, pane);
            } else {
                return;
            }
        }

        self.active = tab_num;
        if let Some(pane) = self.panes.get(&self.active) {
            pane.render(stdout_fd);
        }
    }

    fn dispatch(&mut self, action: Action, stdin_fd: BorrowedFd, stdout_fd: BorrowedFd) {
        match action {
            Action::SwitchTab(tab_num) => self.handle_tab(tab_num, stdin_fd, stdout_fd),
        }
    }

    fn process_stdin(&mut self, buf: &[u8], stdin_fd: BorrowedFd, stdout_fd: BorrowedFd) -> Vec<u8> {
        let (actions, forward) = self.keybinds.feed(buf);
        for action in actions {
            self.dispatch(action, stdin_fd, stdout_fd);
        }
        forward
    }

    pub fn run(&mut self, stdin_fd: BorrowedFd, stdout_fd: BorrowedFd) {
        let stdin_raw: RawFd = stdin_fd.as_raw_fd();

        loop {
            let master_raws: Vec<(usize, RawFd)> = self.panes.iter()
                .map(|(&key, p)| (key, p.pty().master_fd().as_raw_fd()))
                .collect();

            let mut read_fds = select::FdSet::new();
            unsafe {
                read_fds.insert(BorrowedFd::borrow_raw(stdin_raw));
                for &(_, raw) in &master_raws {
                    read_fds.insert(BorrowedFd::borrow_raw(raw));
                }
            }

            match select::select(None, &mut read_fds, None, None, None) {
                Err(Errno::EINTR) => {
                    if SIGWINCH_RECEIVED.swap(false, Ordering::Relaxed) {
                        let ws = get_winsize(stdin_fd);
                        for pane in self.panes.values_mut() {
                            pane.resize(ws.ws_row, ws.ws_col);
                        }
                    }
                    continue;
                }
                Err(_) => break,
                Ok(_) => {}
            }

            let stdin_ready = unsafe { read_fds.contains(BorrowedFd::borrow_raw(stdin_raw)) };
            let ready_keys: Vec<(usize, RawFd)> = master_raws.iter()
                .filter(|(_, raw)| unsafe { read_fds.contains(BorrowedFd::borrow_raw(*raw)) })
                .copied()
                .collect();

            // Read output from ready panes.
            let mut dead = Vec::new();
            for (key, _) in &ready_keys {
                if let Some(pane) = self.panes.get_mut(key) {
                    let mut buf = [0u8; 4096];
                    match unistd::read(pane.pty().master_fd(), &mut buf) {
                        Ok(0) | Err(_) => dead.push(*key),
                        Ok(n) => {
                            let was_alt = pane.term().mode().contains(TermMode::ALT_SCREEN);
                            pane.process(&buf[..n]);
                            let is_alt = pane.term().mode().contains(TermMode::ALT_SCREEN);

                            if *key == self.active {
                                if was_alt != is_alt {
                                    pane.render(stdout_fd);
                                } else {
                                    let _ = unistd::write(stdout_fd, &buf[..n]);
                                }
                            }
                        }
                    }
                }
            }

            for key in &dead {
                self.panes.remove(key);
            }
            if self.panes.is_empty() {
                return;
            }
            if !self.panes.contains_key(&self.active) {
                self.active = *self.panes.keys().next().unwrap();
                if let Some(pane) = self.panes.get(&self.active) {
                    pane.render(stdout_fd);
                }
            }

            if stdin_ready {
                let mut buf = [0u8; 4096];
                match unistd::read(stdin_fd, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let forward = self.process_stdin(&buf[..n], stdin_fd, stdout_fd);
                        if !forward.is_empty() {
                            if let Some(pane) = self.panes.get(&self.active) {
                                let _ = unistd::write(pane.pty().master_fd(), &forward);
                            }
                        }
                    }
                }
            }
        }
    }
}
