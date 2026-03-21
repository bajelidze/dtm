use std::os::fd::BorrowedFd;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor};
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

pub struct Pane {
    pty: Pty,
    term: Term<VoidListener>,
    processor: Processor,
}

impl Pane {
    pub fn new(pty: Pty, rows: u16, cols: u16) -> Self {
        let size = TermSize { rows: rows as usize, cols: cols as usize };
        let config = Config::default();
        let term = Term::new(config, &size, VoidListener);
        let processor = Processor::new();
        Self { pty, term, processor }
    }

    /// Feed raw bytes from the PTY through the VTE parser into the terminal grid.
    pub fn process(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    pub fn pty(&self) -> &Pty {
        &self.pty
    }

    pub fn term(&self) -> &Term<VoidListener> {
        &self.term
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

    /// Render this pane's screen to stdout.
    pub fn render(&self, stdout_fd: BorrowedFd) {
        let content = self.term.renderable_content();
        let rows = self.term.screen_lines();
        let cols = self.term.columns();

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
