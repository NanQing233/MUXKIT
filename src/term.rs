//! VT terminal emulation: parse a byte stream into a cell grid (+scrollback).
//!
//! OWNER: teammate `vt-term`. Only this file.
//!
//! Public API is fixed here because the server depends on it; fill in the
//! bodies. Use the `vte` crate for parsing and `unicode-width` for widths.
//!
//! Notes / deliberate simplifications:
//! * Combining marks and other zero-width characters are ignored: a `char`
//!   with `unicode-width` width 0 is dropped (there is no cell to attach it to
//!   in the shared `Cell` model).
//! * There is no reflow on resize: rows are truncated/padded on the right and
//!   the grid is truncated/padded at the bottom.
//! * Scrollback is only recorded for the primary screen, and only when the
//!   scroll region covers the whole screen (i.e. the row that leaves the
//!   screen top really is a screen row). Scrolling inside a DECSTBM region
//!   moves rows but does not push scrollback.
//! * Erase operations use the current SGR background color (BCE) with the
//!   default foreground color, matching xterm.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

use crate::proto::{Attrs, Cell, Color, Style};

/// Rows of scrollback kept above the visible screen.
pub const DEFAULT_SCROLLBACK: usize = 2000;

/// Upper bound for undrained query replies (the server drains them per frame).
const MAX_REPLIES: usize = 4096;

// Fixed capability answers. DA1 advertises a VT220 with ANSI color (62;22);
// DA2 is a fixed, conservative terminal-identification answer.
const DA1_REPLY: &[u8] = b"\x1b[?62;22c";
const DA2_REPLY: &[u8] = b"\x1b[>1;10;0c";
const DSR_OK_REPLY: &[u8] = b"\x1b[0n";

/// The primary screen saved while the alternate screen is active.
struct SavedScreen {
    lines: Vec<Vec<Cell>>,
    cursor: (u16, u16),
    style: Style,
    top: u16,
    bottom: u16,
    wrap_pending: bool,
}

/// Console flags we only need to remember (input handling lives in the client).
#[derive(Default)]
#[allow(dead_code)]
struct ConsoleFlags {
    bracketed_paste: bool,
    /// Last enabled mouse reporting mode (1000..=1006), 0 when off.
    mouse_mode: u16,
}

pub struct Screen {
    pub cols: u16,
    pub rows: u16,

    /// Active grid: always exactly `rows` rows of `cols` cells.
    lines: Vec<Vec<Cell>>,
    /// `Some` while the alternate screen is active (holds the primary screen).
    alt: Option<SavedScreen>,

    /// Rows scrolled off the top of the primary screen (oldest at the front).
    scrollback: VecDeque<Vec<Cell>>,
    /// 0 = live screen, `n` = scrolled `n` rows up into history.
    offset: usize,

    cx: u16,
    cy: u16,
    wrap_pending: bool,
    cursor_visible: bool,

    cur_fg: Color,
    cur_bg: Color,
    cur_attr: Attrs,

    saved_cursor: (u16, u16),
    saved_style: Style,

    /// Inclusive 0-based scroll region.
    top: u16,
    bottom: u16,

    title: String,
    /// Per visible row damage flags (cleared by `take_damage`).
    dirty: Vec<bool>,
    /// Pending query replies for the pty (drained by `take_replies`).
    replies: Vec<u8>,

    parser: vte::Parser,

    flags: ConsoleFlags,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        // Degenerate sizes would make every cursor/region computation awkward;
        // clamp to at least one cell.
        let cols = cols.max(1);
        let rows = rows.max(1);
        Screen {
            cols,
            rows,
            lines: blank_grid(cols, rows),
            alt: None,
            scrollback: VecDeque::new(),
            offset: 0,
            cx: 0,
            cy: 0,
            wrap_pending: false,
            cursor_visible: true,
            cur_fg: Color::Default,
            cur_bg: Color::Default,
            cur_attr: Attrs(0),
            saved_cursor: (0, 0),
            saved_style: Style::default(),
            top: 0,
            bottom: rows - 1,
            title: String::new(),
            dirty: vec![false; rows as usize],
            replies: Vec::new(),
            parser: vte::Parser::new(),
            flags: ConsoleFlags::default(),
        }
    }

    /// Feed raw bytes read from the PTY master.
    pub fn feed(&mut self, bytes: &[u8]) {
        // The parser must keep its state across calls (escape sequences are
        // routinely split across reads), so take it out while `self` is the
        // `Perform` sink and put it back afterwards.
        let mut parser = std::mem::replace(&mut self.parser, vte::Parser::new());
        for &byte in bytes {
            parser.advance(self, byte);
        }
        self.parser = parser;
    }

    /// Resize the visible screen (reflow is optional; truncate/pad is fine).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }

        self.lines = resize_rows(std::mem::take(&mut self.lines), cols, rows);
        if let Some(saved) = self.alt.as_mut() {
            saved.lines = resize_rows(std::mem::take(&mut saved.lines), cols, rows);
        }

        self.cols = cols;
        self.rows = rows;

        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);

        // xterm resets the scroll margins to the full screen on resize.
        self.top = 0;
        self.bottom = rows - 1;

        self.wrap_pending = false;
        self.saved_cursor = (self.saved_cursor.0.min(cols - 1), self.saved_cursor.1.min(rows - 1));

        // Every row must be repainted after a resize.
        self.dirty = vec![true; rows as usize];
        self.offset = self.offset.min(self.scrollback.len());
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Exactly `rows` visible rows, already offset by the scroll position.
    /// Each row has exactly `cols` cells.
    pub fn visible_rows(&self) -> Vec<Vec<Cell>> {
        let cols = self.cols as usize;
        let rows = self.rows as usize;
        let history = self.scrollback.len();
        let offset = self.scroll_offset().min(history);
        // Virtual index space: [0, history) = scrollback, [history, history+rows) = live.
        let first = history - offset;

        let mut out = Vec::with_capacity(rows);
        for i in 0..rows {
            let idx = first + i;
            if idx < history {
                out.push(fit_row(self.scrollback.get(idx), cols));
            } else {
                out.push(fit_row(self.lines.get(idx - history), cols));
            }
        }
        out
    }

    /// Cursor position inside the pane (0-based, may be outside on odd states).
    pub fn cursor(&self) -> (u16, u16) {
        (self.cx, self.cy)
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Rows whose content changed since the last call (for damage tracking).
    pub fn take_damage(&mut self) -> Vec<u16> {
        let mut out = Vec::new();
        for (y, dirty) in self.dirty.iter_mut().enumerate() {
            if *dirty {
                out.push(y as u16);
                *dirty = false;
            }
        }
        out
    }

    pub fn scroll_offset(&self) -> usize {
        if self.alt.is_some() {
            0
        } else {
            self.offset.min(self.scrollback.len())
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        if self.alt.is_some() || lines == 0 {
            return;
        }
        let wanted = self.offset.saturating_add(lines).min(self.scrollback.len());
        if wanted != self.offset {
            self.offset = wanted;
            self.mark_all_dirty();
        }
    }

    pub fn scroll_down(&mut self, lines: usize) {
        if self.alt.is_some() || lines == 0 {
            return;
        }
        let wanted = self.offset.saturating_sub(lines);
        if wanted != self.offset {
            self.offset = wanted;
            self.mark_all_dirty();
        }
    }

    pub fn scroll_reset(&mut self) {
        if self.offset != 0 {
            self.offset = 0;
            self.mark_all_dirty();
        }
    }

    /// Title reported by OSC 0/2, if any.
    pub fn title(&self) -> String {
        self.title.clone()
    }

    /// Bytes the terminal must send back to the pty (query replies).
    ///
    /// The buffer is drained and cleared by every call. The server writes the
    /// returned bytes to the pty master.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    // ------------------------------------------------------------- internals

    /// Append raw reply bytes, capped so a query flood cannot grow unbounded.
    fn reply(&mut self, bytes: &[u8]) {
        if self.replies.len() + bytes.len() <= MAX_REPLIES {
            self.replies.extend_from_slice(bytes);
        }
    }

    /// CPR / DECXCPR answer: 1-based cursor position clamped into the screen.
    fn reply_cursor_position(&mut self, private: bool) {
        let row = (self.cy as usize + 1).min(self.rows as usize);
        let col = (self.cx as usize + 1).min(self.cols as usize);
        let answer = if private {
            format!("\x1b[?{};{}R", row, col)
        } else {
            format!("\x1b[{};{}R", row, col)
        };
        self.reply(answer.as_bytes());
    }

    fn set_dirty(&mut self, y: u16) {
        if let Some(flag) = self.dirty.get_mut(y as usize) {
            *flag = true;
        }
    }

    fn mark_all_dirty(&mut self) {
        for flag in self.dirty.iter_mut() {
            *flag = true;
        }
    }

    /// Blank cell used for erase/new-line fill (current background, BCE).
    fn blank_cell(&self) -> Cell {
        Cell::blank(Color::Default, self.cur_bg)
    }

    fn put_cell(&mut self, x: u16, y: u16, cell: Cell) {
        let cols = self.cols as usize;
        if x as usize >= cols {
            return;
        }
        if let Some(row) = self.lines.get_mut(y as usize) {
            if let Some(slot) = row.get_mut(x as usize) {
                *slot = cell;
            }
        }
        self.set_dirty(y);
    }

    /// Blank the cell at (x, y) and, if it is part of a wide glyph, its partner.
    fn clear_wide_at(&mut self, x: u16, y: u16) {
        let cols = self.cols as usize;
        let xi = x as usize;
        if xi >= cols {
            return;
        }
        let (continuation, wide) = match self.lines.get(y as usize).and_then(|r| r.get(xi)) {
            Some(cell) => (cell.is_continuation(), cell.wide),
            None => return,
        };
        if continuation {
            if xi > 0 {
                if let Some(row) = self.lines.get_mut(y as usize) {
                    row[xi - 1] = Cell::default();
                }
            }
        } else if wide && xi + 1 < cols {
            if let Some(row) = self.lines.get_mut(y as usize) {
                row[xi + 1] = Cell::default();
            }
        }
        if let Some(row) = self.lines.get_mut(y as usize) {
            row[xi] = Cell::default();
        }
    }

    fn forward_tab(&mut self, n: u16) {
        let cols = self.cols as usize;
        let mut x = self.cx as usize;
        for _ in 0..n.max(1) {
            x = ((x / 8) + 1) * 8;
        }
        self.cx = x.min(cols - 1) as u16;
        self.wrap_pending = false;
    }

    fn backward_tab(&mut self, n: u16) {
        let mut x = self.cx as usize;
        for _ in 0..n.max(1) {
            x = if x == 0 { 0 } else { ((x - 1) / 8) * 8 };
        }
        self.cx = x as u16;
        self.wrap_pending = false;
    }

    /// Line feed: move down, scrolling the scroll region at its bottom margin.
    fn index_down(&mut self) {
        let bottom = (self.bottom as usize).min(self.rows as usize - 1);
        if self.cy as usize >= bottom {
            self.scroll_up_region(1);
            self.cy = bottom as u16;
        } else {
            self.cy += 1;
        }
        self.wrap_pending = false;
    }

    /// Reverse line feed (`ESC M`): move up, scrolling down at the top margin.
    fn reverse_index(&mut self) {
        let top = (self.top as usize).min(self.rows as usize - 1);
        if self.cy as usize <= top {
            self.scroll_down_region(1);
            self.cy = top as u16;
        } else {
            self.cy -= 1;
        }
        self.wrap_pending = false;
    }

    fn push_scrollback(&mut self, line: Vec<Cell>) {
        self.scrollback.push_back(line);
        while self.scrollback.len() > DEFAULT_SCROLLBACK {
            self.scrollback.pop_front();
        }
    }

    /// Scroll the rows inside [top, bottom] up by `n`, filling the bottom.
    fn scroll_up_region(&mut self, n: usize) {
        let rows = self.rows as usize;
        let top = (self.top as usize).min(rows - 1);
        let bottom = (self.bottom as usize).min(rows - 1);
        if top > bottom {
            return;
        }
        let n = n.max(1).min(bottom - top + 1);
        let full = top == 0 && bottom == rows - 1 && self.alt.is_none();
        let blank = vec![self.blank_cell(); self.cols as usize];
        for _ in 0..n {
            let removed = self.lines.remove(top);
            if full {
                self.push_scrollback(removed);
            }
            self.lines.insert(bottom, blank.clone());
        }
        for y in top..=bottom {
            self.set_dirty(y as u16);
        }
        self.wrap_pending = false;
    }

    /// Scroll the rows inside [top, bottom] down by `n`, filling the top.
    fn scroll_down_region(&mut self, n: usize) {
        let rows = self.rows as usize;
        let top = (self.top as usize).min(rows - 1);
        let bottom = (self.bottom as usize).min(rows - 1);
        if top > bottom {
            return;
        }
        let n = n.max(1).min(bottom - top + 1);
        let blank = vec![self.blank_cell(); self.cols as usize];
        for _ in 0..n {
            self.lines.remove(bottom);
            self.lines.insert(top, blank.clone());
        }
        for y in top..=bottom {
            self.set_dirty(y as u16);
        }
        self.wrap_pending = false;
    }

    fn erase_range(&mut self, y: u16, start: usize, end: usize) {
        let cols = self.cols as usize;
        let start = start.min(cols);
        let end = end.min(cols);
        if start >= end {
            return;
        }
        let blank = self.blank_cell();
        if let Some(row) = self.lines.get_mut(y as usize) {
            let end = end.min(row.len());
            if start < end {
                for cell in &mut row[start..end] {
                    *cell = blank;
                }
            }
        }
        self.set_dirty(y);
    }

    fn erase_row(&mut self, y: u16) {
        self.erase_range(y, 0, self.cols as usize);
    }

    fn erase_display(&mut self, mode: u16) {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let cy = (self.cy as usize).min(rows - 1);
        let cx = (self.cx as usize).min(cols - 1);
        match mode {
            0 => {
                self.erase_range(cy as u16, cx, cols);
                for y in (cy + 1)..rows {
                    self.erase_row(y as u16);
                }
            }
            1 => {
                for y in 0..cy {
                    self.erase_row(y as u16);
                }
                self.erase_range(cy as u16, 0, cx + 1);
            }
            2 => {
                for y in 0..rows {
                    self.erase_row(y as u16);
                }
            }
            3 => {
                self.scrollback.clear();
                self.offset = 0;
            }
            _ => {}
        }
        self.wrap_pending = false;
    }

    fn erase_line(&mut self, mode: u16) {
        let cols = self.cols as usize;
        let cy = self.cy;
        let cx = (self.cx as usize).min(cols - 1);
        match mode {
            0 => self.erase_range(cy, cx, cols),
            1 => self.erase_range(cy, 0, cx + 1),
            2 => self.erase_range(cy, 0, cols),
            _ => {}
        }
        self.wrap_pending = false;
    }

    fn insert_chars(&mut self, n: usize) {
        let cols = self.cols as usize;
        let cx = (self.cx as usize).min(cols - 1);
        let n = n.max(1).min(cols - cx);
        let blank = self.blank_cell();
        if let Some(row) = self.lines.get_mut(self.cy as usize) {
            for _ in 0..n {
                row.pop();
                row.insert(cx, blank);
            }
        }
        self.set_dirty(self.cy);
        self.wrap_pending = false;
    }

    fn delete_chars(&mut self, n: usize) {
        let cols = self.cols as usize;
        let cx = (self.cx as usize).min(cols - 1);
        let n = n.max(1).min(cols - cx);
        let blank = self.blank_cell();
        if let Some(row) = self.lines.get_mut(self.cy as usize) {
            for _ in 0..n {
                row.remove(cx);
                row.push(blank);
            }
        }
        self.set_dirty(self.cy);
        self.wrap_pending = false;
    }

    fn erase_chars(&mut self, n: usize) {
        let cx = (self.cx as usize).min(self.cols as usize - 1);
        self.erase_range(self.cy, cx, cx.saturating_add(n.max(1)));
        self.wrap_pending = false;
    }

    fn insert_lines(&mut self, n: usize) {
        let rows = self.rows as usize;
        let top = (self.top as usize).min(rows - 1);
        let bottom = (self.bottom as usize).min(rows - 1);
        let cy = self.cy as usize;
        if cy < top || cy > bottom {
            return;
        }
        let n = n.max(1).min(bottom - cy + 1);
        let blank = vec![self.blank_cell(); self.cols as usize];
        for _ in 0..n {
            self.lines.remove(bottom);
            self.lines.insert(cy, blank.clone());
        }
        for y in top..=bottom {
            self.set_dirty(y as u16);
        }
        self.wrap_pending = false;
    }

    fn delete_lines(&mut self, n: usize) {
        let rows = self.rows as usize;
        let top = (self.top as usize).min(rows - 1);
        let bottom = (self.bottom as usize).min(rows - 1);
        let cy = self.cy as usize;
        if cy < top || cy > bottom {
            return;
        }
        let n = n.max(1).min(bottom - cy + 1);
        let blank = vec![self.blank_cell(); self.cols as usize];
        for _ in 0..n {
            self.lines.remove(cy);
            self.lines.insert(bottom, blank.clone());
        }
        for y in top..=bottom {
            self.set_dirty(y as u16);
        }
        self.wrap_pending = false;
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = (self.cx, self.cy);
        self.saved_style = Style::new(self.cur_fg, self.cur_bg, self.cur_attr);
    }

    fn restore_cursor(&mut self) {
        self.cx = self.saved_cursor.0.min(self.cols - 1);
        self.cy = self.saved_cursor.1.min(self.rows - 1);
        self.cur_fg = self.saved_style.fg;
        self.cur_bg = self.saved_style.bg;
        self.cur_attr = self.saved_style.attr;
        self.wrap_pending = false;
    }

    fn reset_sgr(&mut self) {
        self.cur_fg = Color::Default;
        self.cur_bg = Color::Default;
        self.cur_attr = Attrs(0);
    }

    fn set_color(&mut self, foreground: bool, color: Color) {
        if foreground {
            self.cur_fg = color;
        } else {
            self.cur_bg = color;
        }
    }

    /// SGR works on parameter *groups* so that both the semicolon form
    /// (`38;2;r;g;b`) and the colon form (`38:2:cs:r:g:b`) parse correctly.
    fn sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.reset_sgr();
            return;
        }
        let groups: Vec<&[u16]> = params.iter().collect();
        let mut i = 0;
        while i < groups.len() {
            let group = groups[i];
            let Some(&code) = group.first() else {
                i += 1;
                continue;
            };

            // Colon form: several subparameters inside one group.
            if group.len() > 1 {
                match code {
                    38 | 48 => {
                        let foreground = code == 38;
                        match group.get(1).copied() {
                            Some(5) => {
                                if let Some(&n) = group.get(2) {
                                    self.set_color(foreground, Color::Indexed(n.min(255) as u8));
                                }
                            }
                            Some(2) => {
                                // 38:2:cs:r:g:b, or 38:2:r:g:b without a color space.
                                let len = group.len();
                                if len >= 5 {
                                    let color = Color::Rgb(
                                        group[len - 3].min(255) as u8,
                                        group[len - 2].min(255) as u8,
                                        group[len - 1].min(255) as u8,
                                    );
                                    self.set_color(foreground, color);
                                }
                            }
                            _ => {}
                        }
                    }
                    // Curly/dotted underline styles all map to UNDERLINE.
                    4 => self.cur_attr = self.cur_attr.with(Attrs::UNDERLINE, true),
                    _ => {}
                }
                i += 1;
                continue;
            }

            match code {
                0 => self.reset_sgr(),
                1 => self.cur_attr = self.cur_attr.with(Attrs::BOLD, true),
                2 => self.cur_attr = self.cur_attr.with(Attrs::DIM, true),
                3 => self.cur_attr = self.cur_attr.with(Attrs::ITALIC, true),
                4 => self.cur_attr = self.cur_attr.with(Attrs::UNDERLINE, true),
                7 => self.cur_attr = self.cur_attr.with(Attrs::REVERSE, true),
                22 => {
                    self.cur_attr = self
                        .cur_attr
                        .with(Attrs::BOLD, false)
                        .with(Attrs::DIM, false);
                }
                23 => self.cur_attr = self.cur_attr.with(Attrs::ITALIC, false),
                24 => self.cur_attr = self.cur_attr.with(Attrs::UNDERLINE, false),
                27 => self.cur_attr = self.cur_attr.with(Attrs::REVERSE, false),
                30..=37 => self.cur_fg = Color::Indexed((code - 30) as u8),
                39 => self.cur_fg = Color::Default,
                40..=47 => self.cur_bg = Color::Indexed((code - 40) as u8),
                49 => self.cur_bg = Color::Default,
                90..=97 => self.cur_fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.cur_bg = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 => {
                    let foreground = code == 38;
                    // Semicolon form: the sub-parameters are their own groups.
                    if i + 1 < groups.len() && groups[i + 1].len() == 1 {
                        match groups[i + 1][0] {
                            5 if i + 2 < groups.len() && groups[i + 2].len() == 1 => {
                                let n = groups[i + 2][0].min(255) as u8;
                                self.set_color(foreground, Color::Indexed(n));
                                i += 2;
                            }
                            2
                                if i + 4 < groups.len()
                                    && groups[i + 2..=i + 4].iter().all(|g| g.len() == 1) =>
                            {
                                let color = Color::Rgb(
                                    groups[i + 2][0].min(255) as u8,
                                    groups[i + 3][0].min(255) as u8,
                                    groups[i + 4][0].min(255) as u8,
                                );
                                self.set_color(foreground, color);
                                i += 4;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn set_mode(&mut self, mode: u16, on: bool) {
        match mode {
            25 => self.cursor_visible = on,
            1049 => {
                if on {
                    self.enter_alt();
                } else {
                    self.leave_alt();
                }
            }
            2004 => self.flags.bracketed_paste = on,
            1000..=1006 => {
                if on {
                    self.flags.mouse_mode = mode;
                } else if self.flags.mouse_mode == mode {
                    self.flags.mouse_mode = 0;
                }
            }
            _ => {}
        }
    }

    fn enter_alt(&mut self) {
        if self.alt.is_some() {
            return;
        }
        let saved = SavedScreen {
            lines: std::mem::take(&mut self.lines),
            cursor: (self.cx, self.cy),
            style: Style::new(self.cur_fg, self.cur_bg, self.cur_attr),
            top: self.top,
            bottom: self.bottom,
            wrap_pending: self.wrap_pending,
        };
        self.alt = Some(saved);
        self.lines = blank_grid(self.cols, self.rows);
        self.cx = 0;
        self.cy = 0;
        self.top = 0;
        self.bottom = self.rows - 1;
        self.wrap_pending = false;
        self.offset = 0;
        self.mark_all_dirty();
    }

    fn leave_alt(&mut self) {
        let Some(saved) = self.alt.take() else {
            return;
        };
        self.lines = resize_rows(saved.lines, self.cols, self.rows);
        self.cx = saved.cursor.0.min(self.cols - 1);
        self.cy = saved.cursor.1.min(self.rows - 1);
        self.cur_fg = saved.style.fg;
        self.cur_bg = saved.style.bg;
        self.cur_attr = saved.style.attr;
        self.top = saved.top.min(self.rows - 1);
        self.bottom = saved.bottom.min(self.rows - 1);
        if self.top >= self.bottom {
            self.top = 0;
            self.bottom = self.rows - 1;
        }
        self.wrap_pending = saved.wrap_pending;
        self.mark_all_dirty();
    }

    fn full_reset(&mut self) {
        self.lines = blank_grid(self.cols, self.rows);
        self.alt = None;
        self.scrollback.clear();
        self.offset = 0;
        self.cx = 0;
        self.cy = 0;
        self.top = 0;
        self.bottom = self.rows - 1;
        self.wrap_pending = false;
        self.cursor_visible = true;
        self.reset_sgr();
        self.saved_cursor = (0, 0);
        self.saved_style = Style::default();
        self.title.clear();
        self.flags = ConsoleFlags::default();
        self.replies.clear();
        self.mark_all_dirty();
    }

    fn print_char(&mut self, ch: char) {
        // Combining marks and other zero-width characters are ignored.
        let width = match UnicodeWidthChar::width(ch) {
            Some(0) | None => return,
            Some(width) => width.min(2),
        };
        let cols = self.cols as usize;
        if cols == 0 {
            return;
        }

        // Deferred autowrap: the previous glyph filled the last column.
        if self.wrap_pending {
            self.cx = 0;
            self.index_down();
            self.wrap_pending = false;
        }

        // A double-width glyph never straddles the right margin.
        if width == 2 && self.cx as usize + 2 > cols {
            self.cx = 0;
            self.index_down();
        }

        // Kill any wide glyph this write partially overwrites.
        self.clear_wide_at(self.cx, self.cy);
        if width == 2 {
            self.clear_wide_at(self.cx.saturating_add(1), self.cy);
        }

        let cell = Cell {
            ch,
            fg: self.cur_fg,
            bg: self.cur_bg,
            attr: self.cur_attr,
            wide: width == 2,
        };
        self.put_cell(self.cx, self.cy, cell);
        if width == 2 {
            let continuation = Cell {
                ch: '\0',
                fg: self.cur_fg,
                bg: self.cur_bg,
                attr: self.cur_attr,
                wide: false,
            };
            self.put_cell(self.cx.saturating_add(1), self.cy, continuation);
        }

        self.cx = self.cx.saturating_add(width as u16);
        if self.cx as usize >= cols {
            self.cx = (cols - 1) as u16;
            self.wrap_pending = true;
        }
        self.set_dirty(self.cy);
    }

    fn csi(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        // Flatten primary params and subparams: handles both `38;5;n` and
        // `38:5:n` spellings.
        let args: Vec<u16> = params.iter().flat_map(|p| p.iter().copied()).collect();
        let private = intermediates.first().copied();

        match action {
            // Cursor movement.
            'A' => {
                let n = count(&args, 0);
                let floor = if self.cy >= self.top { self.top } else { 0 };
                self.cy = self.cy.saturating_sub(n).max(floor);
                self.wrap_pending = false;
            }
            'B' | 'e' => {
                let n = count(&args, 0);
                let ceil = if self.cy <= self.bottom {
                    self.bottom
                } else {
                    self.rows - 1
                };
                self.cy = self.cy.saturating_add(n).min(ceil);
                self.wrap_pending = false;
            }
            'C' | 'a' => {
                let n = count(&args, 0);
                self.cx = self.cx.saturating_add(n).min(self.cols - 1);
                self.wrap_pending = false;
            }
            'D' => {
                let n = count(&args, 0);
                self.cx = self.cx.saturating_sub(n);
                self.wrap_pending = false;
            }
            'E' => {
                let n = count(&args, 0);
                self.cx = 0;
                self.cy = self.cy.saturating_add(n).min(self.rows - 1);
                self.wrap_pending = false;
            }
            'F' => {
                let n = count(&args, 0);
                self.cx = 0;
                self.cy = self.cy.saturating_sub(n);
                self.wrap_pending = false;
            }
            'G' | '`' => {
                let col = count(&args, 0);
                self.cx = (col - 1).min(self.cols - 1);
                self.wrap_pending = false;
            }
            'd' => {
                let row = count(&args, 0);
                self.cy = (row - 1).min(self.rows - 1);
                self.wrap_pending = false;
            }
            'H' | 'f' => {
                let row = count(&args, 0);
                let col = count(&args, 1);
                self.cy = (row - 1).min(self.rows - 1);
                self.cx = (col - 1).min(self.cols - 1);
                self.wrap_pending = false;
            }
            'I' => self.forward_tab(count(&args, 0)),
            'Z' => self.backward_tab(count(&args, 0)),

            // Erase.
            'J' => self.erase_display(value(&args, 0)),
            'K' => self.erase_line(value(&args, 0)),
            'X' => self.erase_chars(value(&args, 0) as usize),

            // Insert / delete.
            'L' => self.insert_lines(count(&args, 0) as usize),
            'M' => self.delete_lines(count(&args, 0) as usize),
            '@' => self.insert_chars(count(&args, 0) as usize),
            'P' => self.delete_chars(count(&args, 0) as usize),

            // Scrolling.
            'S' => self.scroll_up_region(count(&args, 0) as usize),
            'T' => self.scroll_down_region(count(&args, 0) as usize),

            // Modes / style.
            'h' | 'l' => {
                if private == Some(b'?') {
                    let on = action == 'h';
                    for mode in args.iter().copied() {
                        self.set_mode(mode, on);
                    }
                }
            }
            'm' => self.sgr(params),
            'r' => self.set_scroll_region(&args),
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),

            // Query replies: DSR, CPR, DECXCPR, DA1, DA2.
            'n' => match (private, value(&args, 0)) {
                (Some(b'?'), 6) => self.reply_cursor_position(true),
                (None, 5) => self.reply(DSR_OK_REPLY),
                (None, 6) => self.reply_cursor_position(false),
                _ => {}
            },
            'c' => {
                if intermediates.is_empty() {
                    if value(&args, 0) == 0 {
                        self.reply(DA1_REPLY);
                    }
                } else if intermediates.first() == Some(&b'>') && value(&args, 0) == 0 {
                    self.reply(DA2_REPLY);
                }
            }

            // No-ops we acknowledge but do not implement.
            't' | 'q' | 'g' | 'b' | 'p' => {}
            _ => {}
        }
    }

    fn set_scroll_region(&mut self, args: &[u16]) {
        let top = args
            .first()
            .copied()
            .filter(|&v| v != 0)
            .unwrap_or(1)
            .saturating_sub(1) as u16;
        let bottom = args
            .get(1)
            .copied()
            .filter(|&v| v != 0)
            .unwrap_or(self.rows)
            .min(self.rows)
            .saturating_sub(1) as u16;
        if top < bottom {
            self.top = top;
            self.bottom = bottom;
        } else {
            self.top = 0;
            self.bottom = self.rows - 1;
        }
        // Without DECOM, home is the absolute top-left.
        self.cx = 0;
        self.cy = 0;
        self.wrap_pending = false;
    }

    fn esc(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.index_down(),
            b'E' => {
                self.cx = 0;
                self.index_down();
            }
            b'M' => self.reverse_index(),
            b'c' => self.full_reset(),
            _ => {}
        }
    }

    fn osc(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(kind) = params.first() else {
            return;
        };
        // OSC 0 (icon + title) and OSC 2 (title only).
        if *kind != b"0" && *kind != b"2" {
            return;
        }
        let mut title = String::new();
        for (i, part) in params[1..].iter().enumerate() {
            if i > 0 {
                title.push(';');
            }
            title.push_str(&String::from_utf8_lossy(part));
        }
        self.title = title;
    }

    fn control(&mut self, byte: u8) {
        match byte {
            0x08 => {
                self.cx = self.cx.saturating_sub(1);
                self.wrap_pending = false;
            }
            0x09 => self.forward_tab(1),
            0x0A | 0x0B | 0x0C => self.index_down(),
            0x0D => {
                self.cx = 0;
                self.wrap_pending = false;
            }
            _ => {}
        }
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        self.control(byte);
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.csi(params, intermediates, ignore, action);
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.esc(intermediates, ignore, byte);
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        self.osc(params, bell_terminated);
    }
}

// ------------------------------------------------------------------- helpers

fn blank_grid(cols: u16, rows: u16) -> Vec<Vec<Cell>> {
    vec![vec![Cell::default(); cols as usize]; rows as usize]
}

fn resize_rows(mut lines: Vec<Vec<Cell>>, cols: u16, rows: u16) -> Vec<Vec<Cell>> {
    let cols = cols as usize;
    let rows = rows as usize;
    for row in lines.iter_mut() {
        row.truncate(cols);
        row.resize(cols, Cell::default());
    }
    lines.truncate(rows);
    while lines.len() < rows {
        lines.push(vec![Cell::default(); cols]);
    }
    lines
}

fn fit_row(row: Option<&Vec<Cell>>, cols: usize) -> Vec<Cell> {
    match row {
        Some(row) => {
            let mut out = Vec::with_capacity(cols);
            for cell in row.iter().take(cols) {
                out.push(*cell);
            }
            out.resize(cols, Cell::default());
            out
        }
        None => vec![Cell::default(); cols],
    }
}

/// CSI numeric parameter with the usual "0 or missing means 1" rule.
fn count(args: &[u16], index: usize) -> u16 {
    match args.get(index).copied() {
        Some(value) if value != 0 => value,
        _ => 1,
    }
}

/// CSI numeric parameter with "missing means 0".
fn value(args: &[u16], index: usize) -> u16 {
    args.get(index).copied().unwrap_or(0)
}

// --------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// Text of one row, skipping the continuation cells of wide glyphs.
    fn text(row: &[Cell]) -> String {
        row.iter()
            .filter(|cell| !cell.is_continuation())
            .map(|cell| cell.ch)
            .collect()
    }

    fn rows(screen: &Screen) -> Vec<String> {
        screen.visible_rows().iter().map(|row| text(row)).collect()
    }

    #[test]
    fn sgr_colors_and_attrs() {
        let mut s = Screen::new(12, 1);
        s.feed(b"\x1b[1;4mAB\x1b[22mC\x1b[24mD");
        let row = s.visible_rows();
        assert!(row[0][0].attr.has(Attrs::BOLD));
        assert!(row[0][0].attr.has(Attrs::UNDERLINE));
        assert!(row[0][1].attr.has(Attrs::BOLD));
        assert!(!row[0][2].attr.has(Attrs::BOLD));
        assert!(row[0][2].attr.has(Attrs::UNDERLINE));
        assert!(!row[0][3].attr.has(Attrs::UNDERLINE));

        let mut c = Screen::new(20, 1);
        c.feed(b"\x1b[31;42mX\x1b[39mY\x1b[49mZ\x1b[91;104mW\x1b[0mV");
        let row = c.visible_rows();
        assert_eq!(row[0][0].fg, Color::Indexed(1));
        assert_eq!(row[0][0].bg, Color::Indexed(2));
        assert_eq!(row[0][1].fg, Color::Default);
        assert_eq!(row[0][1].bg, Color::Indexed(2));
        assert_eq!(row[0][2].fg, Color::Default);
        assert_eq!(row[0][2].bg, Color::Default);
        assert_eq!(row[0][3].fg, Color::Indexed(9));
        assert_eq!(row[0][3].bg, Color::Indexed(12));
        assert_eq!(row[0][4].fg, Color::Default);
        assert_eq!(row[0][4].bg, Color::Default);
    }

    #[test]
    fn sgr_256_and_rgb() {
        let mut s = Screen::new(10, 1);
        s.feed(b"\x1b[38;5;208mX\x1b[48;5;17mY\x1b[38;2;1;2;3mZ\x1b[48;2;10;20;30mW");
        let row = s.visible_rows();
        assert_eq!(row[0][0].fg, Color::Indexed(208));
        assert_eq!(row[0][0].bg, Color::Default);
        assert_eq!(row[0][1].fg, Color::Indexed(208));
        assert_eq!(row[0][1].bg, Color::Indexed(17));
        assert_eq!(row[0][2].fg, Color::Rgb(1, 2, 3));
        assert_eq!(row[0][2].bg, Color::Indexed(17));
        assert_eq!(row[0][3].fg, Color::Rgb(1, 2, 3));
        assert_eq!(row[0][3].bg, Color::Rgb(10, 20, 30));

        // The colon spelling of truecolor is accepted too.
        s.feed(b"\x1b[0m\x1b[38:2:0:4:5:6mQ");
        assert_eq!(s.visible_rows()[0][4].fg, Color::Rgb(4, 5, 6));
    }

    #[test]
    fn erase_line_modes() {
        let mut s = Screen::new(6, 2);
        s.feed(b"hello");
        s.feed(b"\x1b[1;3H\x1b[K");
        assert_eq!(text(&s.visible_rows()[0]), "he    ");
        s.feed(b"\x1b[1;1H\x1b[1K");
        assert_eq!(text(&s.visible_rows()[0]), " e    ");
        s.feed(b"\x1b[2K");
        assert_eq!(text(&s.visible_rows()[0]), "      ");
        s.feed(b"\x1b[2;1Hxyz\x1b[2G\x1b[1K");
        assert_eq!(text(&s.visible_rows()[1]), "  z   ");
    }

    #[test]
    fn erase_display_modes() {
        let mut s = Screen::new(3, 3);
        let fill = |s: &mut Screen| s.feed(b"\x1b[2J\x1b[1;1Habc\r\ndef\r\nghi");

        fill(&mut s);
        s.feed(b"\x1b[2;2H\x1b[J");
        assert_eq!(rows(&s), vec!["abc", "d  ", "   "]);

        fill(&mut s);
        s.feed(b"\x1b[2;2H\x1b[1J");
        assert_eq!(rows(&s), vec!["   ", "  f", "ghi"]);

        fill(&mut s);
        s.feed(b"\x1b[2J");
        assert_eq!(rows(&s), vec!["   ", "   ", "   "]);
    }

    #[test]
    fn scrollback_push_offset_and_damage() {
        let mut s = Screen::new(4, 2);
        s.feed(b"a\r\nb\r\nc");
        assert_eq!(s.scroll_offset(), 0);
        assert_eq!(rows(&s), vec!["b   ", "c   "]);
        assert_eq!(s.scrollback.len(), 1);
        assert_eq!(s.take_damage(), vec![0, 1]);

        s.scroll_up(1);
        assert_eq!(s.scroll_offset(), 1);
        let view = s.visible_rows();
        assert_eq!(text(&view[0]), "a   ");
        assert_eq!(text(&view[1]), "b   ");
        // Scrolling changes every visible row, so all rows are damaged.
        assert_eq!(s.take_damage(), vec![0, 1]);

        // Offset clamps to the available history.
        s.scroll_up(100);
        assert_eq!(s.scroll_offset(), 1);
        assert!(s.take_damage().is_empty());

        s.scroll_down(1);
        assert_eq!(s.scroll_offset(), 0);
        assert_eq!(text(&s.visible_rows()[0]), "b   ");
        assert_eq!(s.take_damage(), vec![0, 1]);

        // ED 3 drops the scrollback.
        s.feed(b"\x1b[3J");
        assert_eq!(s.scrollback.len(), 0);
        assert_eq!(s.scroll_offset(), 0);
    }

    #[test]
    fn wide_chars_layout_and_continuation() {
        let mut s = Screen::new(6, 2);
        s.feed("a界b".as_bytes());
        let view = s.visible_rows();
        assert_eq!(view[0][0].ch, 'a');
        assert_eq!(view[0][1].ch, '界');
        assert!(view[0][1].wide);
        assert_eq!(view[0][2].ch, '\0');
        assert!(view[0][2].is_continuation());
        assert_eq!(view[0][3].ch, 'b');
        assert_eq!(s.cursor(), (4, 0));

        // Overwriting the leading half must also erase the continuation cell.
        s.feed(b"\x1b[1;2Hx");
        let view = s.visible_rows();
        assert_eq!(view[0][1].ch, 'x');
        assert!(!view[0][1].wide);
        assert_eq!(view[0][2].ch, ' ');
        assert!(!view[0][2].is_continuation());
        assert_eq!(text(&view[0]), "ax b  ");
    }

    #[test]
    fn wide_char_wraps_at_right_margin() {
        let mut s = Screen::new(4, 2);
        s.feed("abc界".as_bytes());
        let view = s.visible_rows();
        assert_eq!(text(&view[0]), "abc ");
        assert_eq!(view[1][0].ch, '界');
        assert!(view[1][0].wide);
        assert_eq!(view[1][1].ch, '\0');
        assert_eq!(s.cursor(), (2, 1));
    }

    #[test]
    fn resize_truncates_pads_and_clamps_cursor() {
        let mut s = Screen::new(4, 2);
        s.feed(b"abcd\r\nef");
        let _ = s.take_damage();

        s.resize(6, 3);
        assert_eq!(s.size(), (6, 3));
        assert_eq!(rows(&s), vec!["abcd  ", "ef    ", "      "]);
        // Everything is repainted after a resize.
        assert_eq!(s.take_damage(), vec![0, 1, 2]);

        s.resize(2, 2);
        assert_eq!(rows(&s), vec!["ab", "ef"]);
        let (cx, cy) = s.cursor();
        assert!(cx < 2 && cy < 2);

        s.resize(2, 1);
        assert_eq!(rows(&s), vec!["ab"]);
        assert_eq!(s.cursor().1, 0);
        assert_eq!(s.visible_rows().len(), 1);
        assert_eq!(s.visible_rows()[0].len(), 2);
    }

    #[test]
    fn alt_screen_round_trip() {
        let mut s = Screen::new(8, 2);
        s.feed(b"main");
        let _ = s.take_damage();

        s.feed(b"\x1b[?1049h");
        assert_eq!(text(&s.visible_rows()[0]), "        ");
        assert_eq!(s.cursor(), (0, 0));
        assert_eq!(s.take_damage(), vec![0, 1]);

        s.feed(b"alt");
        assert_eq!(text(&s.visible_rows()[0]), "alt     ");

        s.feed(b"\x1b[?1049l");
        assert_eq!(text(&s.visible_rows()[0]), "main    ");
        assert_eq!(s.cursor(), (4, 0));
        assert_eq!(s.take_damage(), vec![0, 1]);
    }

    #[test]
    fn cursor_addressing_and_motion() {
        let mut s = Screen::new(10, 5);
        s.feed(b"\x1b[2;3H");
        assert_eq!(s.cursor(), (2, 1));

        s.feed(b"\x1b[5A");
        assert_eq!(s.cursor(), (2, 0));

        s.feed(b"\x1b[3B");
        assert_eq!(s.cursor(), (2, 3));

        s.feed(b"\x1b[10C");
        assert_eq!(s.cursor(), (9, 3));

        s.feed(b"\x1b[99D");
        assert_eq!(s.cursor(), (0, 3));

        s.feed(b"\x1b[4G");
        assert_eq!(s.cursor().0, 3);

        s.feed(b"\x1b[2d");
        assert_eq!(s.cursor().1, 1);

        s.feed(b"\x1b[3;7f");
        assert_eq!(s.cursor(), (6, 2));

        s.feed(b"\x1b[1;1H\x1b[2E");
        assert_eq!(s.cursor(), (0, 2));

        s.feed(b"\x1b[2F");
        assert_eq!(s.cursor(), (0, 0));

        // ESC 7 / ESC 8 save and restore the cursor and the SGR state.
        s.feed(b"\x1b[2;2H\x1b[31m\x1b7\x1b[5;5H\x1b[0m\x1b8");
        assert_eq!(s.cursor(), (1, 1));
        s.feed(b"Z");
        assert_eq!(s.visible_rows()[1][1].fg, Color::Indexed(1));
    }

    #[test]
    fn query_replies_cpr_and_da1() {
        let mut s = Screen::new(80, 24);
        s.feed(b"\x1b[6n");
        assert_eq!(s.take_replies(), b"\x1b[1;1R".to_vec());
        // The buffer is drained by every take_replies call.
        assert!(s.take_replies().is_empty());

        s.feed(b"\x1b[3;10H\x1b[6n");
        assert_eq!(s.take_replies(), b"\x1b[3;10R".to_vec());

        s.feed(b"\x1b[?6n");
        assert_eq!(s.take_replies(), b"\x1b[?3;10R".to_vec());

        s.feed(b"\x1b[5n");
        assert_eq!(s.take_replies(), b"\x1b[0n".to_vec());

        s.feed(b"\x1b[c");
        assert_eq!(s.take_replies(), b"\x1b[?62;22c".to_vec());
        s.feed(b"\x1b[0c");
        assert_eq!(s.take_replies(), b"\x1b[?62;22c".to_vec());

        s.feed(b"\x1b[>c");
        assert_eq!(s.take_replies(), b"\x1b[>1;10;0c".to_vec());

        // Other sequences produce no reply; CPR stays 1-based with the cursor.
        s.feed(b"abc");
        assert!(s.take_replies().is_empty());
        s.feed(b"\x1b[6n");
        assert_eq!(s.take_replies(), b"\x1b[3;13R".to_vec());
    }

    #[test]
    fn title_from_osc() {
        let mut s = Screen::new(10, 2);
        assert_eq!(s.title(), "");
        s.feed(b"\x1b]0;hello world\x07");
        assert_eq!(s.title(), "hello world");
        s.feed(b"\x1b]2;second title\x1b\\");
        assert_eq!(s.title(), "second title");
    }

    #[test]
    fn scroll_region_scrolls_only_inside() {
        let mut s = Screen::new(3, 5);
        s.feed(b"1\r\n2\r\n3\r\n4\r\n5");
        assert_eq!(rows(&s), vec!["1  ", "2  ", "3  ", "4  ", "5  "]);

        // Margins rows 2..4 (1-based), which is indices 1..=3.
        s.feed(b"\x1b[2;4r\x1b[4;1H\n");
        let view = rows(&s);
        assert_eq!(view, vec!["1  ", "3  ", "4  ", "   ", "5  "]);
        // No scrollback from a partial region.
        assert_eq!(s.scrollback.len(), 0);

        // Reset margins and scroll the full screen with SU.
        s.feed(b"\x1b[r\x1b[2S");
        assert_eq!(s.scrollback.len(), 2);
        assert_eq!(rows(&s), vec!["4  ", "   ", "5  ", "   ", "   "]);
    }

    #[test]
    fn insert_and_delete_lines() {
        let mut s = Screen::new(3, 3);
        s.feed(b"a\r\nb\r\nc");
        s.feed(b"\x1b[2;1H\x1b[L");
        assert_eq!(rows(&s), vec!["a  ", "   ", "b  "]);
        s.feed(b"\x1b[M");
        assert_eq!(rows(&s), vec!["a  ", "b  ", "   "]);
    }

    #[test]
    fn insert_and_delete_chars() {
        let mut s = Screen::new(6, 1);
        s.feed(b"abcdef");
        s.feed(b"\x1b[1;2H\x1b[2@");
        assert_eq!(text(&s.visible_rows()[0]), "a  bcd");
        s.feed(b"\x1b[2P");
        assert_eq!(text(&s.visible_rows()[0]), "abcd  ");
    }
}