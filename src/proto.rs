//! Shared wire contract + cell model for muxkit.
//!
//! This file is the single source of truth for the types crossing module
//! boundaries. It is owned by the Lead: do not change it without asking,
//! everyone else codes against it.

use serde::{Deserialize, Serialize};

pub const PROTO_VERSION: u32 = 1;

pub type PaneId = u32;
pub type SessionId = u32;

// ---------------------------------------------------------------- cell model

/// A color in the 256-color / truecolor model used by the emitter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Text attributes, packed as bit flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Attrs(pub u8);

impl Attrs {
    pub const BOLD: u8 = 0x01;
    pub const DIM: u8 = 0x02;
    pub const ITALIC: u8 = 0x04;
    pub const UNDERLINE: u8 = 0x08;
    pub const REVERSE: u8 = 0x10;

    pub fn new(bits: u8) -> Self {
        Attrs(bits)
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
    pub fn with(mut self, flag: u8, on: bool) -> Self {
        if on {
            self.0 |= flag;
        } else {
            self.0 &= !flag;
        }
        self
    }
    pub fn bits(self) -> u8 {
        self.0
    }
}

/// One terminal cell.
///
/// `ch == '\0'` marks the continuation cell of a wide (double-width) glyph;
/// the canvas and the encoder must skip it when emitting text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attr: Attrs,
    pub wide: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attr: Attrs(0),
            wide: false,
        }
    }
}

impl Cell {
    pub fn blank(fg: Color, bg: Color) -> Self {
        Cell {
            ch: ' ',
            fg,
            bg,
            attr: Attrs(0),
            wide: false,
        }
    }

    /// True for a cell whose glyph is the tail of a wide character.
    pub fn is_continuation(&self) -> bool {
        self.ch == '\0'
    }
}

/// A run of text sharing one style (wire + canvas primitive).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attr: Attrs,
}

impl Style {
    pub fn new(fg: Color, bg: Color, attr: Attrs) -> Self {
        Style { fg, bg, attr }
    }
    pub fn from_cell(c: &Cell) -> Self {
        Style {
            fg: c.fg,
            bg: c.bg,
            attr: c.attr,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Run {
    pub s: String,
    pub st: Style,
}

/// One changed canvas row, ready to be emitted as ANSI.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RowUpdate {
    pub y: u16,
    pub runs: Vec<Run>,
}

/// Rectangle on the canvas (cells).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> u16 {
        self.x.saturating_add(self.w)
    }
    pub fn bottom(&self) -> u16 {
        self.y.saturating_add(self.h)
    }
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CursorPos {
    pub x: u16,
    pub y: u16,
}

impl Serialize for CursorPos {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        (self.x, self.y).serialize(s)
    }
}
impl<'de> Deserialize<'de> for CursorPos {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let (x, y) = <(u16, u16)>::deserialize(d)?;
        Ok(CursorPos { x, y })
    }
}

// ------------------------------------------------------------------ protocol

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub attached: bool,
    pub panes: u16,
    pub title: String,
}

/// Session/pane level commands the client can ask the server to perform
/// (prefix-key actions that need server-side state).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum PaneAction {
    SplitVertical,
    NextPane,
    ScrollUp,
    ScrollDown,
    ScrollReset,
    SyncInput,
    KillSession,
}

/// Client -> server.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum C2S {
    Hello { cols: u16, rows: u16 },
    /// Attach to an existing session (by id) or create one if `new` is set.
    Attach {
        session: Option<SessionId>,
        cols: u16,
        rows: u16,
        shell: Option<String>,
    },
    Input { data: Vec<u8> },
    Command { action: PaneAction },
    Resize { cols: u16, rows: u16 },
    Detach,
    List,
    Kill { session: SessionId },
    Shutdown,
}

/// Server -> client.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum S2C {
    Hello {
        version: u32,
        sessions: Vec<SessionInfo>,
    },
    SessionList {
        sessions: Vec<SessionInfo>,
    },
    Attached {
        session: SessionId,
        cols: u16,
        rows: u16,
    },
    /// Changed canvas rows only; the client keeps its own copy of the canvas.
    Frame {
        cols: u16,
        rows: u16,
        updates: Vec<RowUpdate>,
        cursor: Option<CursorPos>,
        cursor_visible: bool,
    },
    Detached,
    Exited,
    Error {
        msg: String,
    },
    Info {
        msg: String,
    },
}