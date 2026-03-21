pub struct Bar;

impl Bar {
    pub fn new() -> Self {
        Self
    }

    /// Build the bar content bytes (no cursor save/restore — caller handles that).
    pub fn render(&self, total_rows: u16, cols: u16,
                  tabs: &[usize], active: usize) -> Vec<u8> {
        let mut buf = Vec::new();

        // Move to the last row.
        buf.extend_from_slice(format!("\x1B[{};1H", total_rows).as_bytes());

        // Green background, black foreground.
        buf.extend_from_slice(b"\x1B[30;42m");

        // Build the tab content.
        let mut content = String::new();
        for &tab in tabs {
            if !content.is_empty() {
                content.push(' ');
            }
            if tab == active {
                content.push_str(&format!("[{}]", tab));
            } else {
                content.push_str(&format!("{}", tab));
            }
        }

        // Pad to full width.
        let pad = (cols as usize).saturating_sub(content.len());
        buf.extend_from_slice(content.as_bytes());
        for _ in 0..pad {
            buf.push(b' ');
        }

        // Reset attributes.
        buf.extend_from_slice(b"\x1B[0m");

        buf
    }
}
