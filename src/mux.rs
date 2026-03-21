use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::sync::atomic::Ordering;
use nix::errno::Errno;
use nix::sys::select;
use nix::unistd;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
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
            Self::render_pane(pane, stdout_fd);
        }
    }

    /// Render the given pane's screen to stdout.
    fn render_pane(pane: &Pane, stdout_fd: BorrowedFd) {
        let term = pane.term();
        let content = term.renderable_content();
        let rows = term.screen_lines();
        let cols = term.columns();

        let mut buf = Vec::with_capacity(rows * cols * 4);

        // Reset attributes and clear screen, home cursor.
        buf.extend_from_slice(b"\x1B[0m\x1B[2J\x1B[H");

        let mut prev_fg = Color::Named(NamedColor::Foreground);
        let mut prev_bg = Color::Named(NamedColor::Background);
        let mut prev_flags = Flags::empty();
        let mut prev_line: Option<i32> = None;

        for indexed in content.display_iter {
            let point = indexed.point;
            let cell = indexed.cell;

            // Move cursor to start of new line if needed.
            if prev_line != Some(point.line.0) {
                // Use absolute positioning: \x1B[row;1H (1-based).
                buf.extend_from_slice(
                    format!("\x1B[{};1H", point.line.0 + 1).as_bytes()
                );
                prev_line = Some(point.line.0);
            }

            // Skip wide char spacers.
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }

            // Emit SGR changes.
            let need_reset =
                (prev_flags.contains(Flags::BOLD) && !cell.flags.contains(Flags::BOLD))
                || (prev_flags.contains(Flags::DIM) && !cell.flags.contains(Flags::DIM))
                || (prev_flags.contains(Flags::ITALIC) && !cell.flags.contains(Flags::ITALIC))
                || (prev_flags.contains(Flags::UNDERLINE) && !cell.flags.contains(Flags::UNDERLINE))
                || (prev_flags.contains(Flags::INVERSE) && !cell.flags.contains(Flags::INVERSE));

            if need_reset {
                buf.extend_from_slice(b"\x1B[0m");
                prev_fg = Color::Named(NamedColor::Foreground);
                prev_bg = Color::Named(NamedColor::Background);
                prev_flags = Flags::empty();
            }

            if cell.flags.contains(Flags::BOLD) && !prev_flags.contains(Flags::BOLD) {
                buf.extend_from_slice(b"\x1B[1m");
            }
            if cell.flags.contains(Flags::DIM) && !prev_flags.contains(Flags::DIM) {
                buf.extend_from_slice(b"\x1B[2m");
            }
            if cell.flags.contains(Flags::ITALIC) && !prev_flags.contains(Flags::ITALIC) {
                buf.extend_from_slice(b"\x1B[3m");
            }
            if cell.flags.contains(Flags::UNDERLINE) && !prev_flags.contains(Flags::UNDERLINE) {
                buf.extend_from_slice(b"\x1B[4m");
            }
            if cell.flags.contains(Flags::INVERSE) && !prev_flags.contains(Flags::INVERSE) {
                buf.extend_from_slice(b"\x1B[7m");
            }

            if cell.fg != prev_fg {
                write_fg_color(&mut buf, &cell.fg);
                prev_fg = cell.fg;
            }
            if cell.bg != prev_bg {
                write_bg_color(&mut buf, &cell.bg);
                prev_bg = cell.bg;
            }
            prev_flags = cell.flags;

            // Write the character.
            let mut char_buf = [0u8; 4];
            let s = cell.c.encode_utf8(&mut char_buf);
            buf.extend_from_slice(s.as_bytes());
        }

        // Reset attributes.
        buf.extend_from_slice(b"\x1B[0m");

        // Restore cursor position.
        let cursor = content.cursor;
        // cursor.point.line is relative to viewport (0-based), column is 0-based.
        // Terminal escape uses 1-based row;col.
        buf.extend_from_slice(
            format!("\x1B[{};{}H", cursor.point.line.0 + 1, cursor.point.column.0 + 1).as_bytes()
        );

        // Restore cursor visibility based on the pane's terminal mode.
        if cursor.shape == CursorShape::Hidden {
            buf.extend_from_slice(b"\x1B[?25l");
        } else {
            buf.extend_from_slice(b"\x1B[?25h");
        }

        let _ = unistd::write(stdout_fd, &buf);
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
                                    // Alt screen toggled — re-render from grid
                                    // instead of passing raw bytes (which would
                                    // corrupt the real terminal's alt screen state).
                                    Self::render_pane(pane, stdout_fd);
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
                    Self::render_pane(pane, stdout_fd);
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

fn write_fg_color(buf: &mut Vec<u8>, color: &Color) {
    match color {
        Color::Named(c) => {
            let code = match c {
                NamedColor::Black => 30,
                NamedColor::Red => 31,
                NamedColor::Green => 32,
                NamedColor::Yellow => 33,
                NamedColor::Blue => 34,
                NamedColor::Magenta => 35,
                NamedColor::Cyan => 36,
                NamedColor::White => 37,
                NamedColor::BrightBlack => 90,
                NamedColor::BrightRed => 91,
                NamedColor::BrightGreen => 92,
                NamedColor::BrightYellow => 93,
                NamedColor::BrightBlue => 94,
                NamedColor::BrightMagenta => 95,
                NamedColor::BrightCyan => 96,
                NamedColor::BrightWhite => 97,
                NamedColor::Foreground => 39,
                _ => 39,
            };
            buf.extend_from_slice(format!("\x1B[{}m", code).as_bytes());
        }
        Color::Spec(rgb) => {
            buf.extend_from_slice(format!("\x1B[38;2;{};{};{}m", rgb.r, rgb.g, rgb.b).as_bytes());
        }
        Color::Indexed(idx) => {
            buf.extend_from_slice(format!("\x1B[38;5;{}m", idx).as_bytes());
        }
    }
}

fn write_bg_color(buf: &mut Vec<u8>, color: &Color) {
    match color {
        Color::Named(c) => {
            let code = match c {
                NamedColor::Black => 40,
                NamedColor::Red => 41,
                NamedColor::Green => 42,
                NamedColor::Yellow => 43,
                NamedColor::Blue => 44,
                NamedColor::Magenta => 45,
                NamedColor::Cyan => 46,
                NamedColor::White => 47,
                NamedColor::BrightBlack => 100,
                NamedColor::BrightRed => 101,
                NamedColor::BrightGreen => 102,
                NamedColor::BrightYellow => 103,
                NamedColor::BrightBlue => 104,
                NamedColor::BrightMagenta => 105,
                NamedColor::BrightCyan => 106,
                NamedColor::BrightWhite => 107,
                NamedColor::Background => 49,
                _ => 49,
            };
            buf.extend_from_slice(format!("\x1B[{}m", code).as_bytes());
        }
        Color::Spec(rgb) => {
            buf.extend_from_slice(format!("\x1B[48;2;{};{};{}m", rgb.r, rgb.g, rgb.b).as_bytes());
        }
        Color::Indexed(idx) => {
            buf.extend_from_slice(format!("\x1B[48;5;{}m", idx).as_bytes());
        }
    }
}
