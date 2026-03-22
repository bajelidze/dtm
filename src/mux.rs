use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, RawFd};
use nix::errno::Errno;
use nix::unistd;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::TermMode;
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

/// Terminal modes synced to the outer terminal.
/// Basic mouse tracking (?1000) and SGR encoding (?1006) are always enabled
/// by the client for scroll wheel support — not listed here.
const SYNCED_MODES: &[(TermMode, &[u8], &[u8])] = &[
    (TermMode::APP_CURSOR,        b"\x1B[?1h",    b"\x1B[?1l"),
    (TermMode::MOUSE_DRAG,        b"\x1B[?1002h", b"\x1B[?1002l"),
    (TermMode::MOUSE_MOTION,      b"\x1B[?1003h", b"\x1B[?1003l"),
    (TermMode::BRACKETED_PASTE,   b"\x1B[?2004h", b"\x1B[?2004l"),
    (TermMode::FOCUS_IN_OUT,      b"\x1B[?1004h", b"\x1B[?1004l"),
];

pub struct Mux {
    panes: BTreeMap<usize, Pane>,
    active: usize,
    keybinds: Keybinds,
    bar: Bar,
    shell: CString,
    synced: TermMode,
    rows: u16,
    cols: u16,
}

impl Mux {
    pub fn new(initial: Pane, shell: CString, rows: u16, cols: u16) -> Self {
        let mut panes = BTreeMap::new();
        panes.insert(1, initial);
        Self {
            panes, active: 1, keybinds: Keybinds::new(), bar: Bar::new(),
            shell, synced: TermMode::empty(), rows, cols,
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

    fn sync_modes(&mut self, current: TermMode) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut mouse_downgraded = false;
        for &(flag, on_seq, off_seq) in SYNCED_MODES {
            let was = self.synced.contains(flag);
            let now = current.contains(flag);
            if now && !was {
                buf.extend_from_slice(on_seq);
            } else if !now && was {
                buf.extend_from_slice(off_seq);
                if flag == TermMode::MOUSE_DRAG || flag == TermMode::MOUSE_MOTION {
                    mouse_downgraded = true;
                }
            }
        }
        // Disabling ?1002/?1003 can implicitly disable ?1000 on some terminals.
        // Re-enable basic button tracking so scroll wheel keeps working.
        if mouse_downgraded {
            buf.extend_from_slice(b"\x1B[?1000h");
        }
        self.synced = current & SYNCED_MODES.iter().fold(TermMode::empty(), |acc, &(f, _, _)| acc | f);
        buf
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
        let modes = if let Some(pane) = self.panes.get(&self.active) {
            out.extend_from_slice(&pane.render());
            pane.term_modes()
        } else {
            TermMode::empty()
        };
        out.extend_from_slice(&self.sync_modes(modes));
        out.extend_from_slice(&self.render_bar());
        out
    }

    fn active_pane_has_mouse(&self) -> bool {
        self.panes.get(&self.active)
            .map(|p| p.term_modes().intersects(
                TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION
            ))
            .unwrap_or(false)
    }

    fn scroll_active(&mut self, scroll: Scroll) -> Vec<u8> {
        if let Some(pane) = self.panes.get_mut(&self.active) {
            let old = pane.display_offset();
            pane.scroll(scroll);
            let new = pane.display_offset();
            if old != new {
                let mut out = pane.render_scroll(old, new);
                out.extend_from_slice(&self.render_bar());
                return out;
            }
        }
        Vec::new()
    }

    /// Process stdin input through keybinds.
    /// Returns detach flag, render output, and bytes to forward to PTY.
    pub fn process_stdin(&mut self, buf: &[u8]) -> InputResult {
        let (keys, mouse_fwd, scroll_delta) = parse_mouse_input(buf, self.active_pane_has_mouse());

        let (actions, mut forward) = self.keybinds.feed(&keys);
        forward.extend_from_slice(&mouse_fwd);

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
                Action::ScrollPageUp => {
                    output.extend_from_slice(&self.scroll_active(Scroll::PageUp));
                }
                Action::ScrollPageDown => {
                    output.extend_from_slice(&self.scroll_active(Scroll::PageDown));
                }
            }
        }

        // Handle mouse wheel scroll.
        if scroll_delta != 0 {
            output.extend_from_slice(&self.scroll_active(Scroll::Delta(scroll_delta)));
        }

        // Auto-scroll to bottom when user sends input to PTY.
        if !forward.is_empty() {
            if let Some(pane) = self.panes.get_mut(&self.active) {
                if pane.display_offset() > 0 {
                    output.extend_from_slice(&self.scroll_active(Scroll::Bottom));
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
                    if pane.display_offset() > 0 {
                        // Scrolled up — don't render new output, just discard damage.
                        pane.reset_damage();
                    } else {
                        let (rendered, full) = pane.render_incremental();
                        out.extend_from_slice(&rendered);
                        if full {
                            need_bar = true;
                        }
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

        let modes = self.panes.get(&self.active)
            .map(|p| p.term_modes()).unwrap_or(TermMode::empty());
        out.extend_from_slice(&self.sync_modes(modes));

        (out, false)
    }

    /// Full render for reattach: clear screen + scroll region + pane + bar.
    pub fn full_render(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\x1B[2J\x1B[H");
        out.extend_from_slice(&self.set_scroll_region());
        if let Some(pane) = self.panes.get_mut(&self.active) {
            pane.scroll(Scroll::Bottom);
            out.extend_from_slice(&pane.render());
        }
        out.extend_from_slice(&self.render_bar());
        // Sync all terminal modes so the outer terminal matches the pane's state.
        let modes = self.panes.get(&self.active)
            .map(|p| p.term_modes()).unwrap_or(TermMode::empty());
        out.extend_from_slice(&self.sync_modes(modes));
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

/// Parse SGR mouse events from raw input.
/// Returns (non_mouse_bytes, mouse_bytes_to_forward, scroll_delta).
/// Scroll delta > 0 means scroll up (into history), < 0 means scroll down.
fn parse_mouse_input(input: &[u8], pane_has_mouse: bool) -> (Vec<u8>, Vec<u8>, i32) {
    let mut keys = Vec::new();
    let mut mouse_fwd = Vec::new();
    let mut scroll_delta: i32 = 0;
    let mut i = 0;

    while i < input.len() {
        // Check for SGR mouse sequence: \x1B[<params;params;params[Mm]
        if i + 3 < input.len()
            && input[i] == 0x1B && input[i + 1] == b'[' && input[i + 2] == b'<'
        {
            let start = i;
            let mut j = i + 3;
            // Scan for terminator M (press) or m (release).
            while j < input.len() && input[j] != b'M' && input[j] != b'm' {
                // Validate: only digits and semicolons in params.
                if !input[j].is_ascii_digit() && input[j] != b';' {
                    break;
                }
                j += 1;
            }
            if j < input.len() && (input[j] == b'M' || input[j] == b'm') {
                j += 1; // include terminator
                let params = &input[i + 3..j - 1];
                let button = parse_sgr_button(params);

                if button >= 64 && button < 128 && !pane_has_mouse {
                    // Wheel event without inner mouse mode: scroll history.
                    if button & 1 == 0 {
                        scroll_delta += 3;
                    } else {
                        scroll_delta -= 3;
                    }
                } else if pane_has_mouse {
                    // Forward all mouse events (including wheel) to the PTY.
                    mouse_fwd.extend_from_slice(&input[start..j]);
                }
                i = j;
                continue;
            }
            // Not a valid SGR mouse sequence — fall through to normal byte.
        }

        keys.push(input[i]);
        i += 1;
    }

    (keys, mouse_fwd, scroll_delta)
}

/// Extract the button number (first parameter) from SGR mouse params.
fn parse_sgr_button(params: &[u8]) -> u32 {
    let mut val: u32 = 0;
    for &b in params {
        if b == b';' { break; }
        if b.is_ascii_digit() {
            val = val * 10 + (b - b'0') as u32;
        }
    }
    val
}
