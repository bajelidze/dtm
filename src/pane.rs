use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

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
}
