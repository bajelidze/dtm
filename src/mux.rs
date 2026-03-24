use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::os::fd::{AsRawFd, RawFd};
use nix::errno::Errno;
use nix::unistd;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::TermMode;
use crate::bar::Bar;
use crate::keybinds::{Action, Keybinds};
use crate::layout::{self, Region};
use crate::pane::Pane;
use crate::pty::Pty;

const MASTER_RATIO: f64 = 0.5;

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
const SYNCED_MODES: &[(TermMode, &[u8], &[u8])] = &[
    (TermMode::APP_CURSOR,        b"\x1B[?1h",    b"\x1B[?1l"),
    (TermMode::MOUSE_DRAG,        b"\x1B[?1002h", b"\x1B[?1002l"),
    (TermMode::MOUSE_MOTION,      b"\x1B[?1003h", b"\x1B[?1003l"),
    (TermMode::BRACKETED_PASTE,   b"\x1B[?2004h", b"\x1B[?2004l"),
    (TermMode::FOCUS_IN_OUT,      b"\x1B[?1004h", b"\x1B[?1004l"),
];

struct Tab {
    /// Ordered pane IDs. First is master, rest are stacked.
    pane_ids: Vec<usize>,
    /// Focused pane ID.
    focused: usize,
}

pub struct Mux {
    panes: HashMap<usize, Pane>,
    tabs: BTreeMap<usize, Tab>,
    active: usize,
    next_pane_id: usize,
    keybinds: Keybinds,
    bar: Bar,
    shell: CString,
    synced: TermMode,
    rows: u16,
    cols: u16,
}

impl Mux {
    pub fn new(initial: Pane, shell: CString, rows: u16, cols: u16) -> Self {
        let pane_id = 1;
        let mut panes = HashMap::new();
        panes.insert(pane_id, initial);
        let mut tabs = BTreeMap::new();
        tabs.insert(1, Tab {
            pane_ids: vec![pane_id],
            focused: pane_id,
        });
        Self {
            panes, tabs, active: 1, next_pane_id: 2,
            keybinds: Keybinds::new(), bar: Bar::new(),
            shell, synced: TermMode::empty(), rows, cols,
        }
    }

    fn pane_area(&self) -> Region {
        Region { row: 0, col: 0, rows: self.rows.saturating_sub(1), cols: self.cols }
    }

    /// Compute layout regions for the active tab.
    fn active_regions(&self) -> Vec<(usize, Region)> {
        let area = self.pane_area();
        match self.tabs.get(&self.active) {
            Some(tab) => layout::master_stack(&tab.pane_ids, area, MASTER_RATIO),
            None => Vec::new(),
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
        let tabs: Vec<usize> = self.tabs.keys().copied().collect();
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
        if mouse_downgraded {
            buf.extend_from_slice(b"\x1B[?1000h");
        }
        self.synced = current & SYNCED_MODES.iter().fold(TermMode::empty(), |acc, &(f, _, _)| acc | f);
        buf
    }

    fn focused_pane_modes(&self) -> TermMode {
        self.tabs.get(&self.active)
            .and_then(|tab| self.panes.get(&tab.focused))
            .map(|p| p.term_modes())
            .unwrap_or(TermMode::empty())
    }

    fn focused_pane_has_mouse(&self) -> bool {
        self.tabs.get(&self.active)
            .and_then(|tab| self.panes.get(&tab.focused))
            .map(|p| p.term_modes().intersects(
                TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION
            ))
            .unwrap_or(false)
    }

    /// Render all visible panes, separators, and cursor for the active tab.
    fn render_active_tab(&self) -> Vec<u8> {
        let tab = match self.tabs.get(&self.active) {
            Some(t) => t,
            None => return Vec::new(),
        };

        let area = self.pane_area();
        let regions = layout::master_stack(&tab.pane_ids, area, MASTER_RATIO);
        let separators = layout::master_stack_separators(tab.pane_ids.len(), area, MASTER_RATIO);

        let mut buf = Vec::new();

        for &(id, region) in &regions {
            if let Some(pane) = self.panes.get(&id) {
                buf.extend_from_slice(&pane.render_at(region.row, region.col));
            }
        }

        let focused_region = regions.iter().find(|(id, _)| *id == tab.focused).map(|(_, r)| *r);
        let focused_idx = tab.pane_ids.iter().position(|&id| id == tab.focused).unwrap_or(0);
        buf.extend_from_slice(&layout::render_separators(
            &separators, tab.pane_ids.len(), focused_idx, focused_region,
        ));

        // Cursor for focused pane.
        if let Some(&(_, region)) = regions.iter().find(|(id, _)| *id == tab.focused) {
            if let Some(pane) = self.panes.get(&tab.focused) {
                buf.extend_from_slice(&pane.cursor_at(region.row, region.col));
            }
        }

        buf
    }

    /// Resize all panes in a tab to match their layout regions.
    fn resize_tab_panes(&mut self, tab_num: usize) {
        let area = self.pane_area();
        let regions = match self.tabs.get(&tab_num) {
            Some(tab) => layout::master_stack(&tab.pane_ids, area, MASTER_RATIO),
            None => return,
        };
        for (id, region) in regions {
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.resize(region.rows, region.cols);
            }
        }
    }

    fn handle_tab(&mut self, tab_num: usize) -> Vec<u8> {
        if tab_num == self.active {
            return Vec::new();
        }

        if !self.tabs.contains_key(&tab_num) {
            let area = self.pane_area();
            let ws = nix::pty::Winsize {
                ws_row: self.rows, ws_col: self.cols,
                ws_xpixel: 0, ws_ypixel: 0,
            };
            if let Ok(pty) = Pty::spawn(&ws, &self.shell) {
                let pane_id = self.next_pane_id;
                self.next_pane_id += 1;
                let pane = Pane::new(pty, area.rows, area.cols);
                self.panes.insert(pane_id, pane);
                self.tabs.insert(tab_num, Tab {
                    pane_ids: vec![pane_id],
                    focused: pane_id,
                });
            } else {
                return Vec::new();
            }
        }

        self.active = tab_num;

        let mut out = self.render_active_tab();
        out.extend_from_slice(&self.render_bar());
        let modes = self.focused_pane_modes();
        out.extend_from_slice(&self.sync_modes(modes));
        out
    }

    fn handle_new_pane(&mut self) -> Vec<u8> {
        let area = self.pane_area();

        // Check there's room (need >= 3 cols for master+sep+stack).
        if area.cols < 3 {
            return Vec::new();
        }

        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;

        // Add to the tab's pane list; compute where it will land.
        if let Some(tab) = self.tabs.get_mut(&self.active) {
            tab.pane_ids.push(pane_id);
            tab.focused = pane_id;
        }

        // Compute new layout, resize all panes, create the new one.
        let regions = match self.tabs.get(&self.active) {
            Some(tab) => layout::master_stack(&tab.pane_ids, area, MASTER_RATIO),
            None => return Vec::new(),
        };

        for &(id, region) in &regions {
            if id == pane_id {
                let ws = nix::pty::Winsize {
                    ws_row: region.rows, ws_col: region.cols,
                    ws_xpixel: 0, ws_ypixel: 0,
                };
                if let Ok(pty) = Pty::spawn(&ws, &self.shell) {
                    let pane = Pane::new(pty, region.rows, region.cols);
                    self.panes.insert(pane_id, pane);
                }
            } else if let Some(pane) = self.panes.get_mut(&id) {
                pane.resize(region.rows, region.cols);
            }
        }

        let mut out = self.render_active_tab();
        out.extend_from_slice(&self.render_bar());
        let modes = self.focused_pane_modes();
        out.extend_from_slice(&self.sync_modes(modes));
        out
    }

    fn handle_focus(&mut self, target: usize) -> Vec<u8> {
        match self.tabs.get(&self.active) {
            Some(tab) if tab.focused != target => {}
            _ => return Vec::new(),
        }

        if let Some(tab) = self.tabs.get_mut(&self.active) {
            tab.focused = target;
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"\x1B[?25l");

        let regions = self.active_regions();

        // Re-render separators to update the focus highlight.
        if let Some(tab) = self.tabs.get(&self.active) {
            if tab.pane_ids.len() > 1 {
                let area = self.pane_area();
                let seps = layout::master_stack_separators(tab.pane_ids.len(), area, MASTER_RATIO);
                let focused_idx = tab.pane_ids.iter().position(|&id| id == target).unwrap_or(0);
                let focused_region = regions.iter().find(|(id, _)| *id == target).map(|(_, r)| *r);
                out.extend_from_slice(&layout::render_separators(
                    &seps, tab.pane_ids.len(), focused_idx, focused_region,
                ));
            }
        }

        if let Some(&(_, region)) = regions.iter().find(|(id, _)| *id == target) {
            if let Some(pane) = self.panes.get(&target) {
                out.extend_from_slice(&pane.cursor_at(region.row, region.col));
            }
        }

        let modes = self.focused_pane_modes();
        out.extend_from_slice(&self.sync_modes(modes));
        out
    }

    fn handle_focus_step(&mut self, delta: isize) -> Vec<u8> {
        let next = self.tabs.get(&self.active).and_then(|tab| {
            let len = tab.pane_ids.len();
            if len <= 1 { return None; }
            let idx = tab.pane_ids.iter().position(|&id| id == tab.focused).unwrap_or(0);
            let next_idx = (idx as isize + delta).rem_euclid(len as isize) as usize;
            Some(tab.pane_ids[next_idx])
        });
        if let Some(next_id) = next {
            self.handle_focus(next_id)
        } else {
            Vec::new()
        }
    }

    fn scroll_active(&mut self, scroll: Scroll) -> Vec<u8> {
        let (focused_id, is_single) = match self.tabs.get(&self.active) {
            Some(tab) => (tab.focused, tab.pane_ids.len() == 1),
            None => return Vec::new(),
        };

        let region = if !is_single {
            let regions = self.active_regions();
            regions.into_iter().find(|(id, _)| *id == focused_id).map(|(_, r)| r)
        } else {
            None
        };

        let scroll_out = {
            let pane = match self.panes.get_mut(&focused_id) {
                Some(p) => p,
                None => return Vec::new(),
            };
            let old = pane.display_offset();
            pane.scroll(scroll);
            let new = pane.display_offset();
            if old == new {
                return Vec::new();
            }
            if is_single {
                pane.render_scroll(old, new)
            } else if let Some(region) = region {
                let mut out = pane.render_at(region.row, region.col);
                out.extend_from_slice(&pane.cursor_at(region.row, region.col));
                out
            } else {
                return Vec::new();
            }
        };

        let mut out = scroll_out;
        out.extend_from_slice(&self.render_bar());
        out
    }

    fn dispatch_action(&mut self, action: Action) -> (Vec<u8>, bool) {
        match action {
            Action::SwitchTab(n) => (self.handle_tab(n), false),
            Action::Detach => (Vec::new(), true),
            Action::ScrollPageUp => (self.scroll_active(Scroll::PageUp), false),
            Action::ScrollPageDown => (self.scroll_active(Scroll::PageDown), false),
            Action::NewPane => (self.handle_new_pane(), false),
            Action::FocusDown => (self.handle_focus_step(1), false),
            Action::FocusUp => (self.handle_focus_step(-1), false),
        }
    }

    /// Process mouse events. Returns (render_output, bytes_to_forward_to_pty).
    fn handle_mouse(&mut self, events: &[ParsedMouse]) -> (Vec<u8>, Vec<u8>) {
        let focused_has_mouse = self.focused_pane_has_mouse();
        let regions = self.active_regions();

        let mut focus_target: Option<usize> = None;
        let mut scroll_delta: i32 = 0;
        let mut mouse_fwd: Vec<u8> = Vec::new();

        for mouse in events {
            let row_0 = mouse.row.saturating_sub(1);
            let col_0 = mouse.col.saturating_sub(1);

            let target_pane = layout::pane_at(&regions, row_0, col_0);
            let current_focused = self.tabs.get(&self.active).map(|t| t.focused);

            // Click-to-focus.
            if mouse.press && mouse.button < 3 {
                if let (Some(target), Some(focused)) = (target_pane, current_focused) {
                    if focused != target {
                        focus_target = Some(target);
                    }
                }
            }

            // Wheel scroll when pane doesn't have mouse mode.
            if mouse.button >= 64 && mouse.button < 128 && !focused_has_mouse {
                if mouse.button & 1 == 0 {
                    scroll_delta += 3;
                } else {
                    scroll_delta -= 3;
                }
                continue;
            }

            // Forward mouse with translated coordinates.
            if focused_has_mouse {
                if let Some(focused) = current_focused {
                    if let Some(&(_, region)) = regions.iter().find(|(id, _)| *id == focused) {
                        let local_col = mouse.col.saturating_sub(region.col);
                        let local_row = mouse.row.saturating_sub(region.row);
                        let term = if mouse.press { 'M' } else { 'm' };
                        mouse_fwd.extend_from_slice(
                            format!("\x1B[<{};{};{}{}", mouse.button, local_col, local_row, term)
                                .as_bytes()
                        );
                    }
                }
            }
        }

        let mut output = Vec::new();
        if let Some(target) = focus_target {
            output.extend_from_slice(&self.handle_focus(target));
        }
        if scroll_delta != 0 {
            output.extend_from_slice(&self.scroll_active(Scroll::Delta(scroll_delta)));
        }
        (output, mouse_fwd)
    }

    /// Process stdin input through keybinds.
    pub fn process_stdin(&mut self, buf: &[u8]) -> InputResult {
        let (keys, mouse_events) = parse_input(buf);
        let (actions, mut forward) = self.keybinds.feed(&keys);

        let mut output = Vec::new();
        let mut detach = false;
        for action in actions {
            let (out, det) = self.dispatch_action(action);
            output.extend_from_slice(&out);
            detach |= det;
        }

        let (mouse_out, mouse_fwd) = self.handle_mouse(&mouse_events);
        output.extend_from_slice(&mouse_out);
        forward.extend_from_slice(&mouse_fwd);

        // Auto-scroll to bottom on PTY input.
        if !forward.is_empty() {
            let focused_id = self.tabs.get(&self.active).map(|t| t.focused);
            if let Some(id) = focused_id {
                if self.panes.get(&id).map(|p| p.display_offset() > 0).unwrap_or(false) {
                    output.extend_from_slice(&self.scroll_active(Scroll::Bottom));
                }
            }
        }

        InputResult { detach, output, forward }
    }

    /// Handle a terminal resize.
    pub fn handle_resize(&mut self, rows: u16, cols: u16) -> Vec<u8> {
        self.rows = rows;
        self.cols = cols;

        // Resize all panes in all tabs.
        let area = self.pane_area();
        let all_regions: Vec<(usize, Region)> = self.tabs.values()
            .flat_map(|tab| layout::master_stack(&tab.pane_ids, area, MASTER_RATIO))
            .collect();
        for (id, region) in all_regions {
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.resize(region.rows, region.cols);
            }
        }

        let mut out = Vec::new();
        let is_single = self.tabs.get(&self.active).map(|t| t.pane_ids.len() == 1).unwrap_or(true);
        if is_single {
            out.extend_from_slice(&self.set_scroll_region());
        }
        out.extend_from_slice(&self.render_active_tab());
        out.extend_from_slice(&self.render_bar());
        out
    }

    /// Remove dead panes from all tabs, update focus, drop empty tabs.
    /// Returns true if all tabs are gone (should exit).
    fn remove_dead_panes(&mut self, dead: &[usize]) -> bool {
        for &pane_id in dead {
            self.panes.remove(&pane_id);
        }

        for tab in self.tabs.values_mut() {
            if dead.contains(&tab.focused) {
                let idx = tab.pane_ids.iter().position(|&id| id == tab.focused).unwrap_or(0);
                // Pick the nearest pane before the dead one; skip other dead panes.
                let prev = tab.pane_ids.iter().take(idx).rev()
                    .chain(tab.pane_ids.iter().skip(idx + 1).rev())
                    .find(|id| !dead.contains(id))
                    .copied();
                tab.pane_ids.retain(|id| !dead.contains(id));
                tab.focused = prev
                    .or_else(|| tab.pane_ids.last().copied())
                    .unwrap_or(0);
            } else {
                tab.pane_ids.retain(|id| !dead.contains(id));
            }
        }

        self.tabs.retain(|_, tab| !tab.pane_ids.is_empty());

        if self.tabs.is_empty() {
            return true;
        }
        if !self.tabs.contains_key(&self.active) {
            self.active = *self.tabs.keys().next().unwrap();
        }
        false
    }

    /// Render the focused pane's cursor at its layout position.
    fn render_focused_cursor(&self) -> Vec<u8> {
        let focused_id = match self.tabs.get(&self.active) {
            Some(tab) => tab.focused,
            None => return Vec::new(),
        };
        let regions = self.active_regions();
        if let Some(&(_, region)) = regions.iter().find(|(id, _)| *id == focused_id) {
            if let Some(pane) = self.panes.get(&focused_id) {
                return pane.cursor_at(region.row, region.col);
            }
        }
        Vec::new()
    }

    /// Read output from ready panes and handle dead panes.
    pub fn read_panes(&mut self, ready_keys: &[(usize, RawFd)]) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let mut dead: Vec<usize> = Vec::new();

        let active_regions = self.active_regions();
        let active_pane_ids: Vec<usize> = active_regions.iter().map(|(id, _)| *id).collect();
        let mut any_rendered = false;

        // Read PTY data and render visible panes.
        for &(pane_id, _) in ready_keys {
            let pane = match self.panes.get_mut(&pane_id) {
                Some(p) => p,
                None => continue,
            };
            let mut buf = [0u8; 4096];
            let mut got_data = false;
            loop {
                match unistd::read(pane.pty().master_fd(), &mut buf) {
                    Ok(0) => { dead.push(pane_id); break; }
                    Ok(n) => { pane.process(&buf[..n]); got_data = true; }
                    Err(Errno::EAGAIN) => break,
                    Err(_) => { dead.push(pane_id); break; }
                }
            }
            if got_data && active_pane_ids.contains(&pane_id) {
                if pane.display_offset() > 0 {
                    pane.reset_damage();
                } else if let Some(&(_, region)) = active_regions.iter().find(|(id, _)| *id == pane_id) {
                    let (rendered, _) = pane.render_incremental_at(region.row, region.col);
                    out.extend_from_slice(&rendered);
                    any_rendered = true;
                }
            }
        }

        // Handle dead panes.
        if !dead.is_empty() {
            let active_affected = dead.iter().any(|id| active_pane_ids.contains(id));
            if self.remove_dead_panes(&dead) {
                return (out, true);
            }
            if active_affected {
                self.resize_tab_panes(self.active);
                out = Vec::new();
                out.extend_from_slice(b"\x1B[2J\x1B[H");
                out.extend_from_slice(&self.render_active_tab());
                any_rendered = false;
            }
            out.extend_from_slice(&self.render_bar());
        }

        if any_rendered {
            out.extend_from_slice(&self.render_focused_cursor());
        }

        let modes = self.focused_pane_modes();
        out.extend_from_slice(&self.sync_modes(modes));
        (out, false)
    }

    /// Full render for reattach.
    pub fn full_render(&mut self) -> Vec<u8> {
        let focused_id = self.tabs.get(&self.active).map(|t| t.focused);
        if let Some(id) = focused_id {
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.scroll(Scroll::Bottom);
            }
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"\x1B[2J\x1B[H");

        let is_single = self.tabs.get(&self.active).map(|t| t.pane_ids.len() == 1).unwrap_or(true);
        if is_single {
            out.extend_from_slice(&self.set_scroll_region());
        }

        out.extend_from_slice(&self.render_active_tab());
        out.extend_from_slice(&self.render_bar());

        let modes = self.focused_pane_modes();
        out.extend_from_slice(&self.sync_modes(modes));
        out
    }

    /// Return all PTY master fds for select().
    pub fn pty_fds(&self) -> Vec<(usize, RawFd)> {
        self.panes.iter()
            .map(|(&id, p)| (id, p.pty().master_fd().as_raw_fd()))
            .collect()
    }

    /// Write bytes to the focused pane's PTY.
    pub fn write_to_active(&self, data: &[u8]) {
        let focused_id = self.tabs.get(&self.active).map(|t| t.focused);
        if let Some(id) = focused_id {
            if let Some(pane) = self.panes.get(&id) {
                let _ = unistd::write(pane.pty().master_fd(), data);
            }
        }
    }
}

/// A parsed SGR mouse event.
struct ParsedMouse {
    button: u32,
    col: u16,
    row: u16,
    press: bool,
}

/// Parse input into non-mouse bytes and individual mouse events.
fn parse_input(input: &[u8]) -> (Vec<u8>, Vec<ParsedMouse>) {
    let mut keys = Vec::new();
    let mut mice = Vec::new();
    let mut i = 0;

    while i < input.len() {
        if i + 3 < input.len()
            && input[i] == 0x1B && input[i + 1] == b'[' && input[i + 2] == b'<'
        {
            let mut j = i + 3;
            while j < input.len() && input[j] != b'M' && input[j] != b'm' {
                if !input[j].is_ascii_digit() && input[j] != b';' {
                    break;
                }
                j += 1;
            }
            if j < input.len() && (input[j] == b'M' || input[j] == b'm') {
                let press = input[j] == b'M';
                j += 1;
                let params = &input[i + 3..j - 1];
                if let Some(mouse) = parse_sgr_params(params, press) {
                    mice.push(mouse);
                }
                i = j;
                continue;
            }
        }

        keys.push(input[i]);
        i += 1;
    }

    (keys, mice)
}

fn parse_sgr_params(params: &[u8], press: bool) -> Option<ParsedMouse> {
    let s = std::str::from_utf8(params).ok()?;
    let mut parts = s.split(';');
    let button: u32 = parts.next()?.parse().ok()?;
    let col: u16 = parts.next()?.parse().ok()?;
    let row: u16 = parts.next()?.parse().ok()?;
    Some(ParsedMouse { button, col, row, press })
}
