//! Cell canvas: the server composes every frame (panes + borders + status bar)
//! into a canvas, and ships only the rows that changed.
//!
//! OWNER: teammate `render-keys`. Only this file (and keys.rs).
//!
//! The diff/encode half is what makes the multiplexer fast: one write() per
//! frame instead of one per cell.
//!
//! Conventions used here:
//! * Box drawing is single-line Unicode (`┌ ─ ┐ │ └ ┘`), not ASCII `+-|`.
//! * `cells` is row-major, `cells.len() == w as usize * h as usize`, and it is
//!   always repainted in full width by [`encode`]: a [`RowUpdate`] carries every
//!   column of the row, so the client never keeps a stale tail.
//! * A double-width glyph occupies two cells: the lead cell has `wide = true`
//!   and its glyph, the second cell has `ch == '\0'` and is skipped by the
//!   diff/encoder (one char is emitted for two columns, as a terminal wants).

use std::io::Write;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::proto::{Attrs, Cell, Color, Rect, RowUpdate, Run, Style};

// ---------------------------------------------------------------- canvas core

#[derive(Clone, Debug)]
pub struct Canvas {
    pub w: u16,
    pub h: u16,
    pub cells: Vec<Cell>,
}

impl Canvas {
    pub fn new(w: u16, h: u16) -> Self {
        Canvas {
            w,
            h,
            cells: vec![Cell::default(); w as usize * h as usize],
        }
    }

    /// Resize, keeping the overlapping top-left region and blanking the rest.
    pub fn resize(&mut self, w: u16, h: u16) {
        if w == self.w && h == self.h {
            return;
        }
        let mut cells = vec![Cell::default(); w as usize * h as usize];
        let copy_w = self.w.min(w) as usize;
        let rows = self.h.min(h) as usize;
        for y in 0..rows {
            let src = y * self.w as usize;
            let dst = y * w as usize;
            cells[dst..dst + copy_w].copy_from_slice(&self.cells[src..src + copy_w]);
            // A horizontal shrink may cut a wide pair in half; drop the orphan
            // lead so the row stays well formed.
            if w < self.w && copy_w > 0 && cells[dst + copy_w - 1].wide {
                let c = cells[dst + copy_w - 1];
                cells[dst + copy_w - 1] = Cell::blank(c.fg, c.bg);
            }
        }
        self.w = w;
        self.h = h;
        self.cells = cells;
    }

    /// Blank every cell using the *colours* of `style` (attributes are not
    /// carried over: the cleared screen is plain).
    pub fn clear(&mut self, style: Style) {
        let blank = Cell::blank(style.fg, style.bg);
        for c in &mut self.cells {
            *c = blank;
        }
    }

    fn index(&self, x: u16, y: u16) -> Option<usize> {
        if x >= self.w || y >= self.h {
            return None;
        }
        Some(y as usize * self.w as usize + x as usize)
    }

    /// Write one cell, clipped to the canvas.
    ///
    /// Overwriting one half of a double-width glyph blanks the other half first
    /// so the canvas never holds a dangling `wide` lead or continuation.
    pub fn put(&mut self, x: u16, y: u16, c: Cell) {
        let Some(i) = self.index(x, y) else { return };
        self.split_wide_at(x, y);
        self.cells[i] = c;
    }

    /// If (x, y) is one half of a wide glyph, blank both halves (keep colours).
    fn split_wide_at(&mut self, x: u16, y: u16) {
        let Some(i) = self.index(x, y) else { return };
        let cell = self.cells[i];
        if cell.wide {
            self.cells[i] = Cell::blank(cell.fg, cell.bg);
            if let Some(j) = self.index(x + 1, y) {
                let tail = self.cells[j];
                self.cells[j] = Cell::blank(tail.fg, tail.bg);
            }
        } else if cell.is_continuation() {
            self.cells[i] = Cell::blank(cell.fg, cell.bg);
            if x > 0 {
                if let Some(j) = self.index(x - 1, y) {
                    if self.cells[j].wide {
                        let lead = self.cells[j];
                        self.cells[j] = Cell::blank(lead.fg, lead.bg);
                    }
                }
            }
        }
    }

    /// Draw `text` at (x, y), clipped at `max_w`; returns the columns used.
    ///
    /// Double-width characters take two columns (lead + `'\0'` continuation) and
    /// are never split across the clip boundary. Control characters and
    /// zero-width combining marks are dropped.
    pub fn put_str(&mut self, x: u16, y: u16, text: &str, style: Style, max_w: u16) -> u16 {
        if y >= self.h || x >= self.w {
            return 0;
        }
        let mut cx = x;
        let mut used: u16 = 0;
        for ch in text.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cw == 0 {
                continue; // control char / combining mark: not representable
            }
            let cw = cw as u16;
            if cw > max_w.saturating_sub(used) {
                break; // would overflow the clip width (also covers wide + 1 col)
            }
            if cx >= self.w {
                break;
            }
            if cw == 2 && cx + 1 >= self.w {
                break; // no room for the continuation cell
            }
            let lead = Cell {
                ch,
                fg: style.fg,
                bg: style.bg,
                attr: style.attr,
                wide: cw == 2,
            };
            self.put(cx, y, lead);
            if cw == 2 {
                self.put(
                    cx + 1,
                    y,
                    Cell {
                        ch: '\0',
                        fg: style.fg,
                        bg: style.bg,
                        attr: style.attr,
                        wide: false,
                    },
                );
            }
            cx += cw;
            used += cw;
        }
        used
    }

    /// Blit `rows` into a rect (rows are `w` cells wide).
    ///
    /// Missing rows and short rows are padded with blanks, and everything is
    /// clipped to both the rect and the canvas. A double-width pair that the
    /// rect would cut in half is degraded to a blank instead of leaving a
    /// dangling lead/continuation (which would shift the rest of the row).
    pub fn blit(&mut self, x: u16, y: u16, w: u16, h: u16, rows: &[Vec<Cell>]) {
        for dy in 0..h {
            let src: &[Cell] = rows.get(dy as usize).map(Vec::as_slice).unwrap_or(&[]);
            for dx in 0..w {
                let cx = x.saturating_add(dx);
                let cy = y.saturating_add(dy);
                if cx >= self.w || cy >= self.h {
                    continue;
                }
                self.put(cx, cy, clipped_cell(src, dx, w));
            }
        }
    }

    /// Fill a rect with `style` (glyph `' '`, full style incl. attributes, so a
    /// reverse-video status bar works).
    pub fn fill(&mut self, r: Rect, style: Style) {
        let cell = Cell {
            ch: ' ',
            fg: style.fg,
            bg: style.bg,
            attr: style.attr,
            wide: false,
        };
        let y_max = r.bottom().min(self.h);
        let x_max = r.right().min(self.w);
        for y in r.y..y_max {
            for x in r.x..x_max {
                let Some(i) = self.index(x, y) else { continue };
                self.cells[i] = cell;
            }
        }
    }

    /// Draw a single-line Unicode box outline (`┌ ─ ┐ │ └ ┘`).
    ///
    /// The optional `title` is centred on the top edge and truncated to the
    /// inner width; degenerate boxes (one row/column) degrade to a line.
    pub fn draw_box(&mut self, r: Rect, style: Style, title: Option<&str>) {
        if r.w == 0 || r.h == 0 {
            return;
        }
        let cell = |ch: char| Cell {
            ch,
            fg: style.fg,
            bg: style.bg,
            attr: style.attr,
            wide: false,
        };
        let (x, y, w, h) = (r.x, r.y, r.w, r.h);
        let right = x.saturating_add(w) - 1;
        let bottom = y.saturating_add(h) - 1;

        if w == 1 {
            for yy in y..=bottom {
                self.put(x, yy, cell('│'));
            }
            return;
        }
        if h == 1 {
            for xx in x..=right {
                self.put(xx, y, cell('─'));
            }
            return;
        }

        for xx in x.saturating_add(1)..right {
            self.put(xx, y, cell('─'));
            self.put(xx, bottom, cell('─'));
        }
        for yy in y.saturating_add(1)..bottom {
            self.put(x, yy, cell('│'));
            self.put(right, yy, cell('│'));
        }
        self.put(x, y, cell('┌'));
        self.put(right, y, cell('┐'));
        self.put(x, bottom, cell('└'));
        self.put(right, bottom, cell('┘'));

        if let Some(title) = title {
            let inner = w - 2;
            if inner == 0 {
                return;
            }
            let title = title.replace('\n', " ").replace('\t', " ");
            let max = inner as usize;
            let shown = take_width(&title, max);
            let tw = shown.width() as u16;
            if tw == 0 {
                return;
            }
            let offset = (inner - tw) / 2; // centring gap inside the edge
            let start = x.saturating_add(1).saturating_add(offset);
            self.put_str(start, y, shown, style, inner - offset);
        }
    }

    /// Rows that differ from `prev` (row granularity is enough and cheap).
    ///
    /// Rows that exist only in `self` (grew downwards) count as changed. When
    /// `self` is narrower than `prev` only the surviving columns are compared
    /// and nothing outside the new canvas is emitted; when it is wider the row
    /// is changed because the camera gained columns.
    pub fn diff(&self, prev: &Canvas) -> Vec<RowUpdate> {
        let mut out = Vec::new();
        let w = self.w as usize;
        for y in 0..self.h {
            let changed = if y >= prev.h {
                true
            } else if self.w > prev.w {
                true
            } else {
                let a = y as usize * w;
                let b = y as usize * prev.w as usize;
                self.cells[a..a + w] != prev.cells[b..b + w]
            };
            if changed {
                out.push(RowUpdate {
                    y,
                    runs: self.row_runs(y),
                });
            }
        }
        out
    }

    /// One row as style runs; continuation cells contribute no character, so a
    /// wide glyph yields one char for its two columns.
    fn row_runs(&self, y: u16) -> Vec<Run> {
        let w = self.w as usize;
        let base = y as usize * w;
        let mut runs: Vec<Run> = Vec::new();
        for x in 0..w {
            let c = &self.cells[base + x];
            if c.is_continuation() {
                continue;
            }
            let st = Style::from_cell(c);
            match runs.last_mut() {
                Some(run) if run.st == st => run.s.push(c.ch),
                _ => runs.push(Run {
                    s: c.ch.to_string(),
                    st,
                }),
            }
        }
        runs
    }
}

/// The source cell at `dx` of a blitted row, made safe for the destination:
/// missing cells are blank, and either half of a wide pair that the rect would
/// cut in half becomes a blank cell.
fn clipped_cell(src: &[Cell], dx: u16, w: u16) -> Cell {
    let Some(&c) = src.get(dx as usize) else {
        return Cell::default();
    };
    if c.is_continuation() {
        // Only keep a continuation if its lead is copied too.
        let lead_copied = dx > 0
            && src
                .get(dx as usize - 1)
                .map(|lead| lead.wide)
                .unwrap_or(false);
        if !lead_copied {
            return Cell::blank(c.fg, c.bg);
        }
    }
    if c.wide && dx + 1 >= w {
        // The continuation column is outside the rect.
        return Cell::blank(c.fg, c.bg);
    }
    c
}

/// Longest prefix of `s` whose display width is `<= max`, never splitting a
/// wide character.
fn take_width(s: &str, max: usize) -> &str {
    let mut used = 0usize;
    for (i, ch) in s.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > max {
            return &s[..i];
        }
        used += cw;
    }
    s
}

// ------------------------------------------------------------------ ansi out

/// Encode row updates as ANSI and append them to `out`.
///
/// One cursor-addressing sequence + SGR per run, cells concatenated: this must
/// never emit one write per cell.
pub fn encode(updates: &[RowUpdate], out: &mut Vec<u8>) {
    for u in updates {
        let _ = write!(out, "\x1b[{};1H", u.y as u32 + 1);
        // `None` = "terminal is in the reset state" (true at the start of every
        // row, because each row ends with a reset).
        let mut cur: Option<&Style> = None;
        for run in &u.runs {
            if cur != Some(&run.st) {
                push_sgr(&run.st, out);
                cur = Some(&run.st);
            }
            out.extend_from_slice(run.s.as_bytes());
        }
        out.extend_from_slice(b"\x1b[0m");
    }
}

/// Emit `ESC[0;<attrs>;<fg>;<bg>m` — a reset plus the full attribute set, in one
/// escape.
fn push_sgr(st: &Style, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[0");
    let mut attr = |flag: u8, code: &[u8]| {
        if st.attr.has(flag) {
            out.extend_from_slice(code);
        }
    };
    attr(Attrs::BOLD, b";1");
    attr(Attrs::DIM, b";2");
    attr(Attrs::ITALIC, b";3");
    attr(Attrs::UNDERLINE, b";4");
    attr(Attrs::REVERSE, b";7");
    match st.fg {
        Color::Default => {}
        Color::Indexed(n) => {
            let _ = write!(out, ";38;5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";38;2;{r};{g};{b}");
        }
    }
    match st.bg {
        Color::Default => {}
        Color::Indexed(n) => {
            let _ = write!(out, ";48;5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";48;2;{r};{g};{b}");
        }
    }
    out.push(b'm');
}

/// Position/visibility of the hardware cursor.
pub fn encode_cursor(x: u16, y: u16, visible: bool, out: &mut Vec<u8>) {
    let _ = write!(out, "\x1b[{};{}H", y as u32 + 1, x as u32 + 1);
    out.extend_from_slice(if visible { b"\x1b[?25h" } else { b"\x1b[?25l" });
}

// ---------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn count_esc(s: &str) -> usize {
        s.matches("\x1b[").count()
    }

    fn as_str(buf: &[u8]) -> String {
        String::from_utf8(buf.to_vec()).expect("encoder must emit valid utf-8")
    }

    fn row_text(u: &RowUpdate) -> String {
        u.runs.iter().map(|r| r.s.as_str()).collect()
    }

    #[test]
    fn new_canvas_is_blank_and_row_major() {
        let c = Canvas::new(7, 3);
        assert_eq!(c.cells.len(), 21);
        assert!(c.cells.iter().all(|x| *x == Cell::default()));
    }

    #[test]
    fn put_clips_out_of_range() {
        let mut c = Canvas::new(4, 2);
        c.put(99, 99, Cell::blank(Color::Default, Color::Default));
        c.put(4, 0, Cell::blank(Color::Default, Color::Default));
        c.put(0, 2, Cell::blank(Color::Default, Color::Default));
        assert_eq!(c.cells.len(), 8);
        assert!(c.cells.iter().all(|x| *x == Cell::default()));
    }

    #[test]
    fn diff_detects_only_the_changed_row() {
        let mut c = Canvas::new(10, 4);
        let prev = c.clone();
        c.put_str(0, 2, "hi", Style::default(), 10);
        let updates = c.diff(&prev);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].y, 2);
        // One default style => a single run covering the whole row width, so the
        // client repaints the tail instead of keeping stale cells.
        assert_eq!(updates[0].runs.len(), 1);
        assert_eq!(row_text(&updates[0]).chars().count(), 10);
        assert!(row_text(&updates[0]).starts_with("hi"));
    }

    #[test]
    fn diff_of_identical_canvas_is_empty() {
        let mut c = Canvas::new(10, 4);
        c.put_str(1, 1, "x", Style::default(), 5);
        assert!(c.diff(&c.clone()).is_empty());
    }

    #[test]
    fn diff_grew_rows_are_all_changed() {
        let small = Canvas::new(4, 2);
        let mut big = Canvas::new(4, 4);
        big.put_str(0, 3, "z", Style::default(), 4);
        let updates = big.diff(&small);
        // Only the rows that did not exist before are unconditionally changed;
        // the two blank rows the old canvas already had compare equal.
        assert_eq!(updates.len(), 2);
        assert!(updates.iter().map(|u| u.y).eq(2..4));
        assert!(row_text(&updates[1]).starts_with('z'));
    }

    #[test]
    fn diff_ignores_rows_beyond_a_shrunk_canvas() {
        let mut big = Canvas::new(4, 6);
        big.put_str(0, 5, "q", Style::default(), 4);
        let small = Canvas::new(4, 3);
        let updates = small.diff(&big);
        assert!(updates.is_empty(), "nothing inside the new canvas changed");
        assert!(small.diff(&small).is_empty());
    }

    #[test]
    fn wide_char_takes_two_cells_and_one_glyph() {
        let mut c = Canvas::new(6, 1);
        let used = c.put_str(0, 0, "界", Style::default(), 6);
        assert_eq!(used, 2, "wide char uses two columns");
        assert_eq!(c.cells[0].ch, '界');
        assert!(c.cells[0].wide, "lead cell is flagged wide");
        assert_eq!(c.cells[1].ch, '\0', "continuation cell");
        assert!(c.cells[1].is_continuation());

        let updates = c.diff(&Canvas::new(6, 1));
        assert_eq!(updates.len(), 1);
        let text = row_text(&updates[0]);
        assert_eq!(text, "界    ", "one char for two columns + 4 blanks");
        assert_eq!(text.chars().count(), 5);
    }

    #[test]
    fn wide_char_is_not_split_at_the_edge() {
        let mut c = Canvas::new(3, 1);
        // Only 1 column left at x=2 -> the wide char must be dropped.
        let used = c.put_str(2, 0, "界", Style::default(), 3);
        assert_eq!(used, 0);
        assert_eq!(c.cells[2].ch, ' ');
        // Clip width of 3 starting at 0 fits it (2 columns) + 1 blank after.
        let used = c.put_str(0, 0, "界x", Style::default(), 3);
        assert_eq!(used, 3);
        assert_eq!(c.cells[2].ch, 'x');
    }

    #[test]
    fn overwriting_a_wide_pair_clears_both_halves() {
        let mut c = Canvas::new(4, 1);
        c.put_str(0, 0, "界", Style::default(), 4);
        c.put(1, 0, Cell::blank(Color::Indexed(1), Color::Default));
        assert!(!c.cells[0].wide, "lead was neutralised");
        assert_eq!(c.cells[0].ch, ' ');
        assert_eq!(c.cells[1].ch, ' ');
    }

    #[test]
    fn put_str_respects_max_w_and_returns_columns_used() {
        let mut c = Canvas::new(10, 1);
        assert_eq!(c.put_str(0, 0, "hello world", Style::default(), 5), 5);
        let updates = c.diff(&Canvas::new(10, 1));
        assert!(row_text(&updates[0]).starts_with("hello"));
    }

    #[test]
    fn encode_one_escape_per_run_not_per_cell() {
        let mut c = Canvas::new(20, 1);
        c.put_str(0, 0, "hello", Style::default(), 20);
        let updates = c.diff(&Canvas::new(20, 1));
        let mut out = Vec::new();
        encode(&updates, &mut out);
        let s = as_str(&out);
        assert!(s.contains("hello"));
        // cursor address + one SGR + trailing reset; a per-cell encoder would
        // have 20+ escapes here.
        assert!(count_esc(&s) <= 3, "escapes: {s:?}");
    }

    #[test]
    fn encode_skips_sgr_for_equal_neighbouring_runs() {
        let mut c = Canvas::new(6, 1);
        let red = Style::new(Color::Indexed(1), Color::Default, Attrs(0));
        c.put_str(0, 0, "aa", red, 6); // two runs would merge into one
        let updates = c.diff(&Canvas::new(6, 1));
        assert_eq!(updates[0].runs.len(), 2, "red run + default rest");
        let mut out = Vec::new();
        encode(&updates, &mut out);
        let s = as_str(&out);
        assert_eq!(count_esc(&s), 4, "address, SGR(red), SGR(default), reset");
        assert!(s.contains("\x1b[0;38;5;1m"));
        assert!(s.contains("aa"));
    }

    #[test]
    fn encode_carries_truecolor_and_attributes() {
        let st = Style::new(
            Color::Rgb(1, 2, 3),
            Color::Rgb(4, 5, 6),
            Attrs::new(Attrs::BOLD | Attrs::UNDERLINE | Attrs::REVERSE),
        );
        let mut c = Canvas::new(2, 1);
        c.put_str(0, 0, "x", st, 2);
        let updates = c.diff(&Canvas::new(2, 1));
        let mut out = Vec::new();
        encode(&updates, &mut out);
        let s = as_str(&out);
        assert!(s.contains("\x1b[0;1;4;7;38;2;1;2;3;48;2;4;5;6m"), "{s:?}");
    }

    #[test]
    fn encode_writes_cursor_address_from_row_one() {
        let mut c = Canvas::new(3, 3);
        c.put_str(0, 1, "y", Style::default(), 3);
        let updates = c.diff(&Canvas::new(3, 3));
        let mut out = Vec::new();
        encode(&updates, &mut out);
        assert!(as_str(&out).starts_with("\x1b[2;1H"));
    }

    #[test]
    fn encode_cursor_positions_and_toggles_visibility() {
        let mut out = Vec::new();
        encode_cursor(0, 0, true, &mut out);
        assert_eq!(as_str(&out), "\x1b[1;1H\x1b[?25h");
        out.clear();
        encode_cursor(4, 9, false, &mut out);
        assert_eq!(as_str(&out), "\x1b[10;5H\x1b[?25l");
    }

    #[test]
    fn clear_and_fill_use_colours() {
        let mut c = Canvas::new(3, 2);
        let st = Style::new(Color::Indexed(4), Color::Indexed(0), Attrs(0));
        c.clear(st);
        assert!(c.cells.iter().all(|x| x.ch == ' '
            && x.fg == Color::Indexed(4)
            && x.bg == Color::Indexed(0)));

        let rev = Style::new(Color::Indexed(7), Color::Indexed(1), Attrs::new(Attrs::REVERSE));
        c.fill(Rect::new(1, 0, 1, 1), rev);
        assert_eq!(c.cells[1].attr.bits(), Attrs::REVERSE);
        assert_eq!(c.cells[0].attr.bits(), 0, "outside the rect is untouched");
        // rect clipped to the canvas
        c.fill(Rect::new(2, 1, 9, 9), rev);
        assert_eq!(c.cells[5].attr.bits(), Attrs::REVERSE);
        assert_eq!(c.cells.len(), 6);
    }

    #[test]
    fn blit_pads_short_rows() {
        let mut c = Canvas::new(4, 3);
        c.clear(Style::new(Color::Indexed(3), Color::Default, Attrs(0)));
        let rows = vec![vec![Cell {
            ch: 'A',
            fg: Color::Default,
            bg: Color::Default,
            attr: Attrs(0),
            wide: false,
        }]];
        c.blit(1, 1, 3, 2, &rows);
        assert_eq!(c.cells[1 * 4 + 1].ch, 'A');
        assert_eq!(c.cells[1 * 4 + 2].ch, ' ', "missing cells are blank");
        assert_eq!(c.cells[2 * 4 + 1].ch, ' ', "missing row is blind-filled");
        assert_eq!(c.cells[0].ch, ' ', "outside the blit rect is untouched");
    }

    #[test]
    fn blit_does_not_leave_half_a_wide_glyph() {
        // Source row: "界" (lead + continuation) then "x".
        let mut scratch = Canvas::new(3, 1);
        scratch.put_str(0, 0, "界x", Style::default(), 3);
        let row = scratch.cells.clone();
        assert!(row[0].wide && row[1].is_continuation());

        // Rect starts at the continuation column: it must become a blank, not a
        // stray '\0' that would shift the rest of the row.
        let mut c = Canvas::new(3, 1);
        c.blit(0, 0, 3, 1, &[row[1..].to_vec()]);
        assert_eq!(c.cells[0].ch, ' ');
        assert_eq!(c.cells[1].ch, 'x');

        // Rect ends on the lead column of a pair: degrade the lead to a blank.
        let mut c = Canvas::new(3, 1);
        c.blit(0, 0, 1, 1, std::slice::from_ref(&row));
        assert!(!c.cells[0].wide);
        assert_eq!(c.cells[0].ch, ' ');

        // A fully copied pair survives.
        let mut c = Canvas::new(3, 1);
        c.blit(0, 0, 3, 1, std::slice::from_ref(&row));
        assert!(c.cells[0].wide);
        assert_eq!(c.cells[1].ch, '\0');
        assert_eq!(c.cells[2].ch, 'x');
    }

    #[test]
    fn resize_keeps_overlap_and_blanks_the_rest() {
        let mut c = Canvas::new(3, 2);
        c.put_str(0, 0, "ab", Style::default(), 3);
        c.resize(5, 3);
        assert_eq!(c.cells.len(), 15);
        assert_eq!(c.cells[0].ch, 'a');
        assert_eq!(c.cells[1].ch, 'b');
        assert_eq!(c.cells[3].ch, ' ', "new column blank");
        assert_eq!(c.cells[2 * 5].ch, ' ', "new row blank");
        c.resize(1, 1);
        assert_eq!(c.cells.len(), 1);
        assert_eq!(c.cells[0].ch, 'a');
    }

    #[test]
    fn resize_does_not_leave_a_half_wide_glyph() {
        let mut c = Canvas::new(4, 1);
        c.put_str(0, 0, "a界", Style::default(), 4);
        assert!(c.cells[1].wide);
        c.resize(2, 1); // cuts the pair between columns 1 and 2
        assert!(!c.cells[1].wide, "orphan lead dropped");
        assert_eq!(c.cells[1].ch, ' ');
    }

    #[test]
    fn draw_box_unicode_outline_with_centred_title() {
        let mut c = Canvas::new(12, 5);
        c.draw_box(Rect::new(0, 0, 12, 5), Style::default(), Some("hi"));
        let at = |c: &Canvas, x: usize, y: usize| c.cells[y * 12 + x].ch;
        assert_eq!(at(&c, 0, 0), '┌');
        assert_eq!(at(&c, 11, 0), '┐');
        assert_eq!(at(&c, 0, 4), '└');
        assert_eq!(at(&c, 11, 4), '┘');
        assert_eq!(at(&c, 0, 2), '│');
        assert_eq!(at(&c, 11, 2), '│');
        assert_eq!(at(&c, 0, 2 + 1), '│');
        // inner width is 10, title width 2 -> starts at 1 + (10-2)/2 = 5
        assert_eq!(at(&c, 5, 0), 'h');
        assert_eq!(at(&c, 6, 0), 'i');
        assert_eq!(at(&c, 4, 0), '─');
        assert_eq!(at(&c, 7, 0), '─');
        assert_eq!(at(&c, 0, 3), '│');
    }

    #[test]
    fn draw_box_degenerate_shapes() {
        let mut c = Canvas::new(4, 2);
        c.draw_box(Rect::new(0, 0, 1, 2), Style::default(), Some("t"));
        assert_eq!(c.cells[0].ch, '│');
        assert_eq!(c.cells[4].ch, '│');
        let mut c = Canvas::new(4, 2);
        c.draw_box(Rect::new(0, 0, 4, 1), Style::default(), None);
        assert_eq!(c.cells[0].ch, '─');
        assert_eq!(c.cells[3].ch, '─');
        // zero sized: no panic, no change
        let mut c = Canvas::new(4, 2);
        c.draw_box(Rect::new(0, 0, 0, 0), Style::default(), None);
        assert!(c.cells.iter().all(|x| *x == Cell::default()));
    }

    #[test]
    fn draw_box_title_is_truncated_to_inner_width() {
        let mut c = Canvas::new(6, 3);
        c.draw_box(Rect::new(0, 0, 6, 3), Style::default(), Some("abcdefghij"));
        // inner width 4 -> centred, clipped, corners intact
        assert_eq!(c.cells[0].ch, '┌');
        assert_eq!(c.cells[5].ch, '┐');
        let top: String = (0..6).map(|x| c.cells[x].ch).collect();
        assert_eq!(top.chars().count(), 6);
        assert!(top.starts_with('┌') && top.ends_with('┐'));
        assert!(!top.contains('j'), "title clipped to 4 columns: {top}");
    }

    #[test]
    fn frames_shrink_to_a_handful_of_writes() {
        // 80x24 full repaint, one style: 24 rows * (address + SGR + reset).
        let mut c = Canvas::new(80, 24);
        c.put_str(0, 0, "status", Style::default(), 80);
        let updates = c.diff(&Canvas::new(80, 24));
        let mut out = Vec::new();
        encode(&updates, &mut out);
        assert_eq!(updates.len(), 1);
        assert!(out.len() < 200, "row update should stay tiny: {} bytes", out.len());
    }
}