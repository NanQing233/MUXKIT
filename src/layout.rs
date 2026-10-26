//! Pane layout: split the canvas into vertical panes with 1-cell borders.
//!
//! OWNER: Lead.

use crate::proto::Rect;

/// Compute one rect per pane for a canvas of `cols` x `rows`, leaving the last
/// row for the status bar and one column of border between panes.
pub fn split_vertical(cols: u16, rows: u16, n: usize) -> Vec<Rect> {
    if n == 0 || cols == 0 || rows == 0 {
        return Vec::new();
    }
    let body_h = rows.saturating_sub(1);
    let n = n as u16;
    let border_total = n.saturating_sub(1);
    let usable = cols.saturating_sub(border_total);
    let base = usable / n;
    let mut rem = usable % n;
    let mut rects = Vec::with_capacity(n as usize);
    let mut x = 0u16;
    for _ in 0..n {
        let mut w = base;
        if rem > 0 {
            w += 1;
            rem -= 1;
        }
        rects.push(Rect::new(x, 0, w, body_h));
        x = x.saturating_add(w).saturating_add(1);
    }
    rects
}