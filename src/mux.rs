use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::sync::atomic::Ordering;
use nix::errno::Errno;
use nix::sys::select;
use nix::unistd;
use alacritty_terminal::term::TermMode;
use crate::pane::Pane;
use crate::pty::Pty;
use crate::SIGWINCH_RECEIVED;
use crate::get_winsize;

const ESC: u8 = 0x1B;

pub struct Mux {
    panes: BTreeMap<usize, Pane>,
    active: usize,
    pending_esc: bool,
    shell: CString,
}

impl Mux {
    pub fn new(initial: Pane, shell: CString) -> Self {
        let mut panes = BTreeMap::new();
        panes.insert(1, initial);
        Self { panes, active: 1, pending_esc: false, shell }
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

    /// Process stdin bytes, intercepting Alt+digit sequences.
    fn process_stdin(&mut self, buf: &[u8], stdin_fd: BorrowedFd, stdout_fd: BorrowedFd) -> Vec<u8> {
        let mut forward = Vec::new();
        let mut i = 0;

        while i < buf.len() {
            if self.pending_esc {
                self.pending_esc = false;
                if buf[i] >= b'1' && buf[i] <= b'9' {
                    let tab_num = (buf[i] - b'0') as usize;
                    self.handle_tab(tab_num, stdin_fd, stdout_fd);
                    i += 1;
                    continue;
                }
                forward.push(ESC);
            }

            if buf[i] == ESC {
                if i + 1 < buf.len() {
                    if buf[i + 1] >= b'1' && buf[i + 1] <= b'9' {
                        let tab_num = (buf[i + 1] - b'0') as usize;
                        self.handle_tab(tab_num, stdin_fd, stdout_fd);
                        i += 2;
                        continue;
                    }
                    forward.push(buf[i]);
                    i += 1;
                } else {
                    self.pending_esc = true;
                    i += 1;
                }
            } else {
                forward.push(buf[i]);
                i += 1;
            }
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
