/// A rectangular screen region (0-based coordinates).
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
}

/// A separator line between panes.
pub struct Separator {
    pub row: u16,
    pub col: u16,
    /// true = horizontal line (─), false = vertical line (│).
    pub horizontal: bool,
    pub len: u16,
}

/// Compute master-stack layout regions.
/// First pane is master (left), rest are stacked on the right.
pub fn master_stack(pane_ids: &[usize], area: Region, master_ratio: f64) -> Vec<(usize, Region)> {
    if pane_ids.is_empty() {
        return Vec::new();
    }
    if pane_ids.len() == 1 {
        return vec![(pane_ids[0], area)];
    }

    let master_cols = ((area.cols.saturating_sub(1) as f64) * master_ratio).round().max(1.0) as u16;
    let stack_cols = area.cols.saturating_sub(master_cols + 1).max(1);

    let mut regions = Vec::new();

    // Master pane — full height, left side.
    regions.push((pane_ids[0], Region {
        row: area.row, col: area.col,
        rows: area.rows, cols: master_cols,
    }));

    // Stack panes — right side, divided vertically.
    let stack_col = area.col + master_cols + 1;
    let stack_count = pane_ids.len() - 1;
    let seps_in_stack = stack_count.saturating_sub(1);
    let available = (area.rows as usize).saturating_sub(seps_in_stack);
    let base = available / stack_count;
    let extra = available % stack_count;

    let mut cur_row = area.row;
    for (i, &id) in pane_ids[1..].iter().enumerate() {
        let h = (base + if i < extra { 1 } else { 0 }).max(1) as u16;
        regions.push((id, Region {
            row: cur_row, col: stack_col,
            rows: h, cols: stack_cols,
        }));
        cur_row += h;
        if i < stack_count - 1 {
            cur_row += 1; // separator row
        }
    }

    regions
}

/// Compute separators for the master-stack layout.
pub fn master_stack_separators(pane_count: usize, area: Region, master_ratio: f64) -> Vec<Separator> {
    if pane_count <= 1 {
        return Vec::new();
    }

    let master_cols = ((area.cols.saturating_sub(1) as f64) * master_ratio).round().max(1.0) as u16;
    let stack_cols = area.cols.saturating_sub(master_cols + 1).max(1);
    let stack_col = area.col + master_cols + 1;

    let mut seps = Vec::new();

    // Vertical separator between master and stack.
    seps.push(Separator {
        row: area.row,
        col: area.col + master_cols,
        horizontal: false,
        len: area.rows,
    });

    // Horizontal separators between stacked panes.
    if pane_count > 2 {
        let stack_count = pane_count - 1;
        let seps_in_stack = stack_count - 1;
        let available = (area.rows as usize).saturating_sub(seps_in_stack);
        let base = available / stack_count;
        let extra = available % stack_count;

        let mut cur_row = area.row;
        for i in 0..stack_count - 1 {
            let h = (base + if i < extra { 1 } else { 0 }) as u16;
            cur_row += h;
            seps.push(Separator {
                row: cur_row,
                col: stack_col,
                horizontal: true,
                len: stack_cols,
            });
            cur_row += 1;
        }
    }

    seps
}

/// Find which pane contains the given 0-based screen position.
pub fn pane_at(regions: &[(usize, Region)], row: u16, col: u16) -> Option<usize> {
    regions.iter().find(|(_, r)| {
        row >= r.row && row < r.row + r.rows
            && col >= r.col && col < r.col + r.cols
    }).map(|(id, _)| *id)
}

/// Render separator lines as ANSI bytes, highlighting the portions adjacent to
/// the focused pane.
///
/// - `pane_count`: total number of panes in the layout.
/// - `focused_idx`: 0-based position of the focused pane in `pane_ids`
///   (0 = master/left, ≥1 = stack/right).
/// - `focused_region`: the screen region of the focused pane.
///
/// Vertical separator highlight rules:
///   - Master focused (idx 0): top half highlighted.
///   - Single stack pane focused (2-pane layout): bottom half highlighted.
///   - Stack pane focused (3+ panes): the rows overlapping that pane's region.
///
/// Horizontal separator: highlighted when the focused pane is directly above or
/// below it (i.e. it forms a border of the focused pane).
pub fn render_separators(
    separators: &[Separator],
    pane_count: usize,
    focused_idx: usize,
    focused_region: Option<Region>,
) -> Vec<u8> {
    let mut buf = Vec::new();
    if separators.is_empty() {
        return buf;
    }
    buf.extend_from_slice(b"\x1B[0m");

    for sep in separators {
        if sep.horizontal {
            // Highlight if focused pane directly borders this separator.
            let hl = focused_region.map_or(false, |r| {
                r.row + r.rows == sep.row || r.row == sep.row + 1
            });
            if hl { buf.extend_from_slice(b"\x1B[36m"); }
            buf.extend_from_slice(
                format!("\x1B[{};{}H", sep.row + 1, sep.col + 1).as_bytes()
            );
            for _ in 0..sep.len {
                buf.extend_from_slice("─".as_bytes());
            }
            if hl { buf.extend_from_slice(b"\x1B[0m"); }
        } else {
            // Determine which rows of the vertical separator to highlight.
            let half = sep.len / 2;
            let (hi_start, hi_end) = if focused_idx == 0 && pane_count == 2 {
                // Master in 2-pane layout: top half.
                (0, half)
            } else if focused_idx == 0 {
                // Master in 3+ pane layout: full separator.
                (0, sep.len)
            } else if pane_count == 2 {
                // Single stack pane: bottom half.
                (half, sep.len)
            } else {
                // Stack pane in 3+ layout: rows overlapping the pane's region.
                focused_region.map_or((0, half), |r| {
                    let start = r.row.saturating_sub(sep.row);
                    let end = (r.row + r.rows).saturating_sub(sep.row).min(sep.len);
                    (start, end)
                })
            };

            let mut highlighted = false;
            for r in 0..sep.len {
                let should_hl = r >= hi_start && r < hi_end;
                if should_hl != highlighted {
                    buf.extend_from_slice(if should_hl { b"\x1B[36m" } else { b"\x1B[0m" });
                    highlighted = should_hl;
                }
                buf.extend_from_slice(
                    format!("\x1B[{};{}H│", sep.row + r + 1, sep.col + 1).as_bytes()
                );
            }
            if highlighted {
                buf.extend_from_slice(b"\x1B[0m");
            }
        }
    }

    // Draw junction characters at intersections.
    for v in separators.iter().filter(|s| !s.horizontal) {
        for h in separators.iter().filter(|s| s.horizontal) {
            if h.row >= v.row && h.row < v.row + v.len && h.col == v.col + 1 {
                // Highlight junction when focused pane borders the horizontal separator.
                let hl = focused_region.map_or(false, |r| {
                    r.row + r.rows == h.row || r.row == h.row + 1
                });
                if hl { buf.extend_from_slice(b"\x1B[36m"); }
                buf.extend_from_slice(
                    format!("\x1B[{};{}H├", h.row + 1, v.col + 1).as_bytes()
                );
                if hl { buf.extend_from_slice(b"\x1B[0m"); }
            }
        }
    }
    buf.extend_from_slice(b"\x1B[0m");
    buf
}
