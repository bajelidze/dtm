use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, LineDamageBounds, Term, TermDamage, TermMode};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, CursorStyle, NamedColor, Processor};
use nix::unistd;

use crate::pty::Pty;

/// Simple Dimensions implementation for Term construction and resize.
struct TermSize {
    rows: usize,
    cols: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Writes terminal query responses (DA, DSR, etc.) back to the PTY.
struct PtyEventProxy {
    master_raw: RawFd,
}

impl EventListener for PtyEventProxy {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            let fd = unsafe { BorrowedFd::borrow_raw(self.master_raw) };
            let _ = unistd::write(fd, text.as_bytes());
        }
    }
}

pub struct Pane {
    pty: Pty,
    term: Term<PtyEventProxy>,
    processor: Processor,
}

impl Pane {
    pub fn new(pty: Pty, rows: u16, cols: u16) -> Self {
        let size = TermSize { rows: rows as usize, cols: cols as usize };
        let mut config = Config::default();
        config.scrolling_history = 5000;
        let proxy = PtyEventProxy { master_raw: pty.master_fd().as_raw_fd() };
        let term = Term::new(config, &size, proxy);
        let processor = Processor::new();
        // PTY may have been spawned with a different size; sync it to the grid.
        let ws = nix::pty::Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        let _ = pty.resize(&ws);
        Self { pty, term, processor }
    }

    /// Feed raw bytes from the PTY through the VTE parser into the terminal grid.
    pub fn process(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    pub fn pty(&self) -> &Pty {
        &self.pty
    }

    /// Return the current terminal mode flags relevant to the outer terminal.
    pub fn term_modes(&self) -> TermMode {
        *self.term.mode()
    }

    pub fn scroll(&mut self, scroll: Scroll) {
        self.term.scroll_display(scroll);
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    pub fn reset_damage(&mut self) {
        self.term.reset_damage();
    }

    /// Resize both the virtual terminal and the underlying PTY.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let size = TermSize { rows: rows as usize, cols: cols as usize };
        self.term.resize(size);
        let ws = nix::pty::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let _ = self.pty.resize(&ws);
    }

    /// Render this pane's full content with cursor handling.
    pub fn render(&self) -> Vec<u8> {
        let mut buf = self.render_at(0, 0);
        buf.extend_from_slice(&self.cursor_at(0, 0));
        buf
    }

    /// Render this pane's content at a screen offset (no cursor handling).
    pub fn render_at(&self, row_off: u16, col_off: u16) -> Vec<u8> {
        let content = self.term.renderable_content();
        let rows = self.term.screen_lines();
        let cols = self.term.columns();
        let offset = content.display_offset as i32;

        let mut buf = Vec::with_capacity(rows * cols * 2);

        // Clear the pane region by writing spaces.
        buf.extend_from_slice(b"\x1B[0m");
        for r in 0..rows {
            buf.extend_from_slice(
                format!("\x1B[{};{}H",
                    row_off as usize + r + 1,
                    col_off as usize + 1
                ).as_bytes()
            );
            for _ in 0..cols {
                buf.push(b' ');
            }
        }

        buf.extend_from_slice(b"\x1B[0m");
        let mut sgr = SgrState::new();
        let mut cur_row: i32 = -1;
        let mut cur_col: usize = usize::MAX;

        for indexed in content.display_iter {
            let point = indexed.point;
            let cell = indexed.cell;
            let screen_row = point.line.0 + offset;

            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }

            // Skip default cells — region is already cleared.
            if cell.c == ' '
                && cell.fg == Color::Named(NamedColor::Foreground)
                && cell.bg == Color::Named(NamedColor::Background)
                && cell.flags.is_empty()
            {
                continue;
            }

            if screen_row != cur_row || point.column.0 != cur_col {
                buf.extend_from_slice(
                    format!("\x1B[{};{}H",
                        row_off as i32 + screen_row + 1,
                        col_off as usize + point.column.0 + 1
                    ).as_bytes()
                );
            }

            write_cell(&mut buf, cell.c, cell.flags, cell.fg, cell.bg, &mut sgr);

            cur_row = screen_row;
            cur_col = point.column.0 + 1;
            if cell.flags.contains(Flags::WIDE_CHAR) {
                cur_col += 1;
            }
        }

        buf.extend_from_slice(b"\x1B[0m");
        buf
    }

    /// Position and show/hide cursor for this pane at a screen offset.
    pub fn cursor_at(&self, row_off: u16, col_off: u16) -> Vec<u8> {
        let mut buf = Vec::new();

        if self.term.grid().display_offset() > 0 {
            buf.extend_from_slice(b"\x1B[?25l");
            return buf;
        }

        let cursor = self.term.grid().cursor.point;
        let rows = self.term.screen_lines() as i32;

        if cursor.line.0 >= 0 && cursor.line.0 < rows {
            buf.extend_from_slice(
                format!("\x1B[{};{}H",
                    row_off as i32 + cursor.line.0 + 1,
                    col_off as usize + cursor.column.0 + 1
                ).as_bytes()
            );
            if self.term.mode().contains(TermMode::SHOW_CURSOR) {
                buf.extend_from_slice(b"\x1B[?25h");
                write_cursor_shape(&mut buf, self.term.cursor_style());
            } else {
                buf.extend_from_slice(b"\x1B[?25l");
            }
        } else {
            buf.extend_from_slice(b"\x1B[?25l");
        }
        buf
    }

    /// Incremental render at a screen offset (no cursor handling).
    /// Returns (output_bytes, was_full_redraw).
    pub fn render_incremental_at(&mut self, row_off: u16, col_off: u16) -> (Vec<u8>, bool) {
        let damage = match self.term.damage() {
            TermDamage::Full => None,
            TermDamage::Partial(iter) => Some(iter.collect::<Vec<_>>()),
        };
        self.term.reset_damage();

        match damage {
            None => {
                let buf = self.render_at(row_off, col_off);
                (buf, true)
            }
            Some(lines) => {
                let buf = self.render_damaged_at(&lines, row_off, col_off);
                (buf, false)
            }
        }
    }

    fn render_damaged_at(&self, lines: &[LineDamageBounds], row_off: u16, col_off: u16) -> Vec<u8> {
        if lines.is_empty() {
            return Vec::new();
        }

        let grid = self.term.grid();
        let mut buf = Vec::new();

        for damaged in lines {
            buf.extend_from_slice(
                format!("\x1B[{};{}H",
                    row_off as usize + damaged.line + 1,
                    col_off as usize + damaged.left + 1
                ).as_bytes()
            );

            buf.extend_from_slice(b"\x1B[0m");
            let mut sgr = SgrState::new();

            for col in damaged.left..=damaged.right {
                let cell = &grid[Line(damaged.line as i32)][Column(col)];

                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                write_cell(&mut buf, cell.c, cell.flags, cell.fg, cell.bg, &mut sgr);
            }
        }

        buf.extend_from_slice(b"\x1B[0m");
        buf
    }

    /// Incrementally render a scroll by shifting existing content and
    /// only drawing the newly revealed lines. Falls back to full render
    /// if the delta exceeds the screen height.
    pub fn render_scroll(&mut self, old_offset: usize, new_offset: usize) -> Vec<u8> {
        let rows = self.term.screen_lines();
        let cols = self.term.columns();
        let delta = new_offset as i32 - old_offset as i32;
        let abs_delta = delta.unsigned_abs() as usize;

        self.term.reset_damage();

        if abs_delta >= rows {
            return self.render();
        }

        let grid = self.term.grid();
        let display_offset = new_offset as i32;
        let mut buf = Vec::new();

        if delta > 0 {
            // Scrolling up: content shifts down, new lines at top.
            buf.extend_from_slice(format!("\x1B[{}T", abs_delta).as_bytes());
            for screen_row in 0..abs_delta {
                self.render_row(&mut buf, grid, screen_row, display_offset, cols);
            }
        } else {
            // Scrolling down: content shifts up, new lines at bottom.
            buf.extend_from_slice(format!("\x1B[{}S", abs_delta).as_bytes());
            for screen_row in (rows - abs_delta)..rows {
                self.render_row(&mut buf, grid, screen_row, display_offset, cols);
            }
        }

        buf.extend_from_slice(b"\x1B[0m");

        // Cursor: hidden when scrolled, shown when back at bottom.
        if new_offset > 0 {
            buf.extend_from_slice(b"\x1B[?25l");
        } else {
            let cursor = grid.cursor.point;
            buf.extend_from_slice(
                format!("\x1B[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1).as_bytes()
            );
            if self.term.mode().contains(TermMode::SHOW_CURSOR) {
                buf.extend_from_slice(b"\x1B[?25h");
                write_cursor_shape(&mut buf, self.term.cursor_style());
            } else {
                buf.extend_from_slice(b"\x1B[?25l");
            }
        }

        buf
    }

    fn render_row(
        &self,
        buf: &mut Vec<u8>,
        grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
        screen_row: usize,
        display_offset: i32,
        cols: usize,
    ) {
        let grid_line = Line(screen_row as i32 - display_offset);
        buf.extend_from_slice(
            format!("\x1B[{};1H\x1B[2K\x1B[0m", screen_row + 1).as_bytes()
        );
        let mut sgr = SgrState::new();
        for col in 0..cols {
            let cell = &grid[grid_line][Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            write_cell(buf, cell.c, cell.flags, cell.fg, cell.bg, &mut sgr);
        }
    }

}

fn write_cursor_shape(buf: &mut Vec<u8>, style: CursorStyle) {
    let n = match style.shape {
        CursorShape::Block | CursorShape::HollowBlock => if style.blinking { 1 } else { 2 },
        CursorShape::Underline => if style.blinking { 3 } else { 4 },
        CursorShape::Beam => if style.blinking { 5 } else { 6 },
        CursorShape::Hidden => return,
    };
    buf.extend_from_slice(format!("\x1B[{} q", n).as_bytes());
}

struct SgrState {
    fg: Color,
    bg: Color,
    flags: Flags,
}

impl SgrState {
    fn new() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }
}

fn write_cell(buf: &mut Vec<u8>, c: char, flags: Flags, fg: Color, bg: Color, sgr: &mut SgrState) {
    let need_reset =
        (sgr.flags.contains(Flags::BOLD) && !flags.contains(Flags::BOLD))
        || (sgr.flags.contains(Flags::DIM) && !flags.contains(Flags::DIM))
        || (sgr.flags.contains(Flags::ITALIC) && !flags.contains(Flags::ITALIC))
        || (sgr.flags.contains(Flags::UNDERLINE) && !flags.contains(Flags::UNDERLINE))
        || (sgr.flags.contains(Flags::INVERSE) && !flags.contains(Flags::INVERSE));

    if need_reset {
        buf.extend_from_slice(b"\x1B[0m");
        sgr.fg = Color::Named(NamedColor::Foreground);
        sgr.bg = Color::Named(NamedColor::Background);
        sgr.flags = Flags::empty();
    }

    if flags.contains(Flags::BOLD) && !sgr.flags.contains(Flags::BOLD) {
        buf.extend_from_slice(b"\x1B[1m");
    }
    if flags.contains(Flags::DIM) && !sgr.flags.contains(Flags::DIM) {
        buf.extend_from_slice(b"\x1B[2m");
    }
    if flags.contains(Flags::ITALIC) && !sgr.flags.contains(Flags::ITALIC) {
        buf.extend_from_slice(b"\x1B[3m");
    }
    if flags.contains(Flags::UNDERLINE) && !sgr.flags.contains(Flags::UNDERLINE) {
        buf.extend_from_slice(b"\x1B[4m");
    }
    if flags.contains(Flags::INVERSE) && !sgr.flags.contains(Flags::INVERSE) {
        buf.extend_from_slice(b"\x1B[7m");
    }

    if fg != sgr.fg {
        write_fg_color(buf, &fg);
        sgr.fg = fg;
    }
    if bg != sgr.bg {
        write_bg_color(buf, &bg);
        sgr.bg = bg;
    }
    sgr.flags = flags;

    let mut char_buf = [0u8; 4];
    let s = c.encode_utf8(&mut char_buf);
    buf.extend_from_slice(s.as_bytes());
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
