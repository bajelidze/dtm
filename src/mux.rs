use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, RawFd};
use nix::errno::Errno;
use nix::unistd;
use crate::bar::Bar;
use crate::keybinds::{Action, Keybinds};
use crate::pane::Pane;
use crate::pty::Pty;

/// Result of processing stdin input through keybinds.
pub struct InputResult {
    /// Whether a detach was requested.
    pub detach: bool,
    /// ANSI output from keybind-triggered renders (tab switches, etc.).
    pub output: Vec<u8>,
    /// Bytes to forward to the active pane's PTY.
    pub forward: Vec<u8>,
}

pub struct Mux {
    panes: BTreeMap<usize, Pane>,
    active: usize,
    keybinds: Keybinds,
    bar: Bar,
    shell: CString,
    app_cursor: bool,
    rows: u16,
    cols: u16,
}

impl Mux {
    pub fn new(initial: Pane, shell: CString, rows: u16, cols: u16) -> Self {
        let mut panes = BTreeMap::new();
        panes.insert(1, initial);
        Self {
            panes, active: 1, keybinds: Keybinds::new(), bar: Bar::new(),
            shell, app_cursor: false, rows, cols,
        }
    }

    pub fn set_scroll_region(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"\x1B7");
        buf.extend_from_slice(format!("\x1B[1;{}r", self.rows - 1).as_bytes());
        buf.extend_from_slice(b"\x1B8");
        buf
    }

    pub fn render_bar(&self) -> Vec<u8> {
        let tabs: Vec<usize> = self.panes.keys().copied().collect();
        let bar_content = self.bar.render(self.rows, self.cols, &tabs, self.active);

        let mut buf = Vec::new();
        buf.extend_from_slice(b"\x1B7");
        buf.extend_from_slice(&bar_content);
        buf.extend_from_slice(b"\x1B8");
        buf
    }

    fn sync_app_cursor(&mut self, app_cursor: bool) -> Vec<u8> {
        if app_cursor != self.app_cursor {
            self.app_cursor = app_cursor;
            if app_cursor {
                return b"\x1B[?1h".to_vec();
            } else {
                return b"\x1B[?1l".to_vec();
            }
        }
        Vec::new()
    }

    fn handle_tab(&mut self, tab_num: usize) -> Vec<u8> {
        let mut out = Vec::new();

        if tab_num == self.active {
            return out;
        }

        if !self.panes.contains_key(&tab_num) {
            let ws = nix::pty::Winsize {
                ws_row: self.rows, ws_col: self.cols,
                ws_xpixel: 0, ws_ypixel: 0,
            };
            if let Ok(pty) = Pty::spawn(&ws, &self.shell) {
                let pane = Pane::new(pty, self.rows - 1, self.cols);
                self.panes.insert(tab_num, pane);
            } else {
                return out;
            }
        }

        self.active = tab_num;
        let app_cursor = if let Some(pane) = self.panes.get(&self.active) {
            out.extend_from_slice(&pane.render());
            pane.app_cursor()
        } else {
            false
        };
        out.extend_from_slice(&self.sync_app_cursor(app_cursor));
        out.extend_from_slice(&self.render_bar());
        out
    }

    /// Process stdin input through keybinds.
    /// Returns detach flag, render output, and bytes to forward to PTY.
    pub fn process_stdin(&mut self, buf: &[u8]) -> InputResult {
        let (actions, forward) = self.keybinds.feed(buf);
        let mut output = Vec::new();
        let mut detach = false;
        for action in actions {
            match action {
                Action::SwitchTab(tab_num) => {
                    output.extend_from_slice(&self.handle_tab(tab_num));
                }
                Action::Detach => {
                    detach = true;
                }
            }
        }
        InputResult { detach, output, forward }
    }

    /// Handle a terminal resize. Returns ANSI output.
    pub fn handle_resize(&mut self, rows: u16, cols: u16) -> Vec<u8> {
        self.rows = rows;
        self.cols = cols;
        for pane in self.panes.values_mut() {
            pane.resize(rows - 1, cols);
        }
        let mut out = self.set_scroll_region();
        out.extend_from_slice(&self.render_bar());
        out
    }

    /// Read output from ready panes and handle dead panes.
    /// Returns (output_bytes, all_dead).
    pub fn read_panes(&mut self, ready_keys: &[(usize, RawFd)]) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let mut dead = Vec::new();
        let mut need_bar = false;

        for &(key, _) in ready_keys {
            if let Some(pane) = self.panes.get_mut(&key) {
                let mut buf = [0u8; 4096];
                let mut got_data = false;
                loop {
                    match unistd::read(pane.pty().master_fd(), &mut buf) {
                        Ok(0) => { dead.push(key); break; }
                        Ok(n) => { pane.process(&buf[..n]); got_data = true; }
                        Err(Errno::EAGAIN) => break,
                        Err(_) => { dead.push(key); break; }
                    }
                }
                if got_data && key == self.active {
                    let (rendered, full) = pane.render_incremental();
                    out.extend_from_slice(&rendered);
                    if full {
                        need_bar = true;
                    }
                }
            }
        }

        for key in &dead {
            self.panes.remove(key);
        }
        if self.panes.is_empty() {
            return (out, true);
        }
        if !self.panes.contains_key(&self.active) {
            self.active = *self.panes.keys().next().unwrap();
            if let Some(pane) = self.panes.get(&self.active) {
                out.extend_from_slice(&pane.render());
            }
            need_bar = true;
        }
        if !dead.is_empty() || need_bar {
            out.extend_from_slice(&self.render_bar());
        }

        let app_cursor = self.panes.get(&self.active)
            .map(|p| p.app_cursor()).unwrap_or(false);
        out.extend_from_slice(&self.sync_app_cursor(app_cursor));

        (out, false)
    }

    /// Full render for reattach: clear screen + scroll region + pane + bar.
    pub fn full_render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\x1B[2J\x1B[H");
        out.extend_from_slice(&self.set_scroll_region());
        if let Some(pane) = self.panes.get(&self.active) {
            out.extend_from_slice(&pane.render());
        }
        out.extend_from_slice(&self.render_bar());
        out
    }

    /// Return all PTY master fds for use in select().
    pub fn pty_fds(&self) -> Vec<(usize, RawFd)> {
        self.panes.iter()
            .map(|(&key, p)| (key, p.pty().master_fd().as_raw_fd()))
            .collect()
    }

    /// Write bytes to the active pane's PTY.
    pub fn write_to_active(&self, data: &[u8]) {
        if let Some(pane) = self.panes.get(&self.active) {
            let _ = unistd::write(pane.pty().master_fd(), data);
        }
    }
}
