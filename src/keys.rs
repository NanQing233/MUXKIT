//! Prefix-key handling and keybind configuration.
//!
//! OWNER: teammate `render-keys` (same owner as canvas.rs). Only this file.
//!
//! Defaults mirror muxkit: Ctrl+B is the prefix.
//!   prefix d  detach        prefix %  split pane
//!   prefix o  next pane     prefix [  scroll up
//!   prefix ]  scroll down   prefix s  toggle sync input
//!   prefix q  kill session
//!
//! Semantics of [`KeyHandler::feed`]:
//! * Everything that is not part of a prefix sequence is forwarded verbatim, in
//!   order, as one [`KeyEvent::Bytes`] per maximal run of plain bytes.
//! * Prefix + a bound key becomes the mapped [`Action`].
//! * Prefix twice emits a literal Ctrl+B byte (0x02), so the prefix can be sent
//!   to the pane.
//! * Prefix + an *unbound* key is **swallowed** (both bytes dropped): the prefix
//!   is a command key and never leaks half a sequence into the pty.
//! * The state machine spans calls: a lone Ctrl+B at the end of one read is
//!   completed by the next read.

use std::collections::HashMap;
use std::path::PathBuf;

/// The prefix key: Ctrl+B.
pub const PREFIX: u8 = 0x02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Detach,
    SplitVertical,
    NextPane,
    ScrollUp,
    ScrollDown,
    ScrollReset,
    SyncInput,
    KillSession,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    /// Bytes to forward to the focused pane's pty.
    Bytes(Vec<u8>),
    Action(Action),
}

pub struct KeyHandler {
    bindings: HashMap<(u8, u8), Action>,
    /// A prefix byte was consumed and we are waiting for the command key.
    pending_prefix: bool,
}

impl Default for KeyHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyHandler {
    pub fn new() -> Self {
        KeyHandler {
            bindings: default_bindings(),
            pending_prefix: false,
        }
    }

    /// A handler with the defaults plus every valid line of `text` applied on
    /// top (config wins, later lines win).
    pub fn with_config(text: &str) -> Self {
        let mut h = Self::new();
        for (combo, action) in parse_config(text) {
            h.bindings.insert(combo, action);
        }
        h
    }

    /// Add/replace one `(prefix, key)` binding.
    pub fn bind(&mut self, combo: (u8, u8), action: Action) {
        self.bindings.insert(combo, action);
    }

    /// True while a lone prefix byte is buffered and awaiting its command key.
    pub fn is_pending(&self) -> bool {
        self.pending_prefix
    }

    /// Feed raw stdin bytes, get back the ordered list of things to do.
    pub fn feed(&mut self, input: &[u8]) -> Vec<KeyEvent> {
        let mut out: Vec<KeyEvent> = Vec::new();
        let mut plain: Vec<u8> = Vec::new();
        for &b in input {
            if self.pending_prefix {
                self.pending_prefix = false;
                if b == PREFIX {
                    flush(&mut out, &mut plain);
                    out.push(KeyEvent::Bytes(vec![PREFIX]));
                } else if let Some(action) = self.bindings.get(&(PREFIX, b)) {
                    flush(&mut out, &mut plain);
                    out.push(KeyEvent::Action(*action));
                }
                // else: unbound prefix combo -> swallowed (see module docs).
            } else if b == PREFIX {
                self.pending_prefix = true;
            } else {
                plain.push(b);
            }
        }
        flush(&mut out, &mut plain);
        out
    }
}

fn flush(out: &mut Vec<KeyEvent>, plain: &mut Vec<u8>) {
    if !plain.is_empty() {
        out.push(KeyEvent::Bytes(std::mem::take(plain)));
    }
}

/// The built-in key table (Ctrl+B prefix).
pub fn default_bindings() -> HashMap<(u8, u8), Action> {
    let mut m = HashMap::new();
    m.insert((PREFIX, b'd'), Action::Detach);
    m.insert((PREFIX, b'%'), Action::SplitVertical);
    m.insert((PREFIX, b'o'), Action::NextPane);
    m.insert((PREFIX, b'['), Action::ScrollUp);
    m.insert((PREFIX, b']'), Action::ScrollDown);
    m.insert((PREFIX, b's'), Action::SyncInput);
    m.insert((PREFIX, b'q'), Action::KillSession);
    m
}

/// `%LOCALAPPDATA%/muxkit/keybinds.conf` on Windows, `~/.config/muxkit/...`
/// elsewhere.
pub fn config_path() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(base) = non_empty_var("LOCALAPPDATA") {
            return PathBuf::from(base).join("muxkit").join("keybinds.conf");
        }
    }
    if let Some(home) = home_dir() {
        return home.join(".config").join("muxkit").join("keybinds.conf");
    }
    PathBuf::from("keybinds.conf")
}

/// Load the config file if it exists; returns the raw text for the parser.
///
/// Any IO error (missing file, permissions, not utf-8) is tolerated and
/// reported as `None`: a broken config must never stop the multiplexer.
pub fn load_config() -> Option<String> {
    std::fs::read_to_string(config_path()).ok()
}

fn non_empty_var(key: &str) -> Option<std::ffi::OsString> {
    let v = std::env::var_os(key)?;
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        // Native Windows first (USERPROFILE), then whatever shell HOME is set
        // to (MSYS/Git Bash), then the classic HOMEDRIVE + HOMEPATH pair.
        for key in ["USERPROFILE", "HOME"] {
            if let Some(v) = non_empty_var(key) {
                return Some(PathBuf::from(v));
            }
        }
        if let (Some(drive), Some(path)) = (non_empty_var("HOMEDRIVE"), non_empty_var("HOMEPATH")) {
            let mut p = PathBuf::from(drive);
            p.push(path);
            return Some(p);
        }
    }
    #[cfg(not(windows))]
    {
        for key in ["HOME", "USERPROFILE"] {
            if let Some(v) = non_empty_var(key) {
                return Some(PathBuf::from(v));
            }
        }
    }
    None
}

/// Parsed binding table, exposed for tests.
///
/// Accepted line shape: `<prefix> <key> <action>`, e.g.
///   `prefix d detach_session`
/// Blank lines, `#` comments (whole-line or trailing) and anything unparseable
/// are ignored. Later lines override earlier ones.
pub fn parse_config(text: &str) -> HashMap<(u8, u8), Action> {
    let mut map = HashMap::new();
    for raw in text.lines() {
        let line = match raw.find('#') {
            Some(i) => &raw[..i],
            None => raw,
        };
        let mut tok = line.split_whitespace();
        let (Some(prefix_tok), Some(key_tok), Some(action_tok)) =
            (tok.next(), tok.next(), tok.next())
        else {
            continue;
        };
        let Some(prefix) = parse_prefix(prefix_tok) else {
            continue;
        };
        let Some(key) = parse_key(key_tok) else {
            continue;
        };
        let Some(action) = parse_action(action_tok) else {
            continue;
        };
        map.insert((prefix, key), action);
    }
    map
}

fn parse_prefix(tok: &str) -> Option<u8> {
    let t = tok.to_ascii_lowercase();
    match t.as_str() {
        "prefix" | "ctrl-b" | "c-b" | "^b" => Some(PREFIX),
        _ => None,
    }
}

/// One ASCII byte, or a couple of spelled-out keys that terminals send as one
/// byte (`space` = 0x20, `tab` = 0x09).
fn parse_key(tok: &str) -> Option<u8> {
    match tok.to_ascii_lowercase().as_str() {
        "space" => return Some(b' '),
        "tab" => return Some(b'\t'),
        _ => {}
    }
    let mut it = tok.chars();
    let c = it.next()?;
    if it.next().is_some() || !c.is_ascii() || c.is_ascii_control() {
        return None;
    }
    Some(c as u8)
}

fn parse_action(tok: &str) -> Option<Action> {
    let name = tok.to_ascii_lowercase().replace('-', "_");
    let action = match name.as_str() {
        "detach_session" | "detach" => Action::Detach,
        "split_vertical" | "split" | "new_pane" => Action::SplitVertical,
        "next_pane" => Action::NextPane,
        "scroll_up" => Action::ScrollUp,
        "scroll_down" => Action::ScrollDown,
        "scroll_reset" => Action::ScrollReset,
        "sync_input" => Action::SyncInput,
        "kill_session" => Action::KillSession,
        _ => return None,
    };
    Some(action)
}

// ---------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_bytes_pass_through_in_one_batch() {
        let mut h = KeyHandler::new();
        assert_eq!(
            h.feed(b"hello"),
            vec![KeyEvent::Bytes(b"hello".to_vec())]
        );
        assert!(!h.is_pending());
    }

    #[test]
    fn prefix_d_detaches() {
        let mut h = KeyHandler::new();
        assert!(h.feed(&[PREFIX]).is_empty(), "prefix byte alone is buffered");
        assert!(h.is_pending());
        assert_eq!(h.feed(b"d"), vec![KeyEvent::Action(Action::Detach)]);
        assert!(!h.is_pending(), "prefix state is consumed");
    }

    #[test]
    fn prefix_and_command_in_one_read() {
        let mut h = KeyHandler::new();
        assert_eq!(
            h.feed(&[PREFIX, b'q']),
            vec![KeyEvent::Action(Action::KillSession)]
        );
    }

    #[test]
    fn doubled_prefix_emits_a_literal_ctrl_b() {
        let mut h = KeyHandler::new();
        assert_eq!(
            h.feed(&[PREFIX, PREFIX]),
            vec![KeyEvent::Bytes(vec![PREFIX])]
        );
        assert!(!h.is_pending());
        // Still usable afterwards.
        assert_eq!(h.feed(b"x"), vec![KeyEvent::Bytes(b"x".to_vec())]);
    }

    #[test]
    fn unknown_prefix_combo_is_swallowed() {
        let mut h = KeyHandler::new();
        assert!(h.feed(&[PREFIX, b'Z']).is_empty());
        assert!(!h.is_pending());
        // Only the following plain byte survives.
        assert_eq!(
            h.feed(b"\x02Zx"),
            vec![KeyEvent::Bytes(b"x".to_vec())]
        );
    }

    #[test]
    fn bytes_before_and_after_an_action_keep_their_order() {
        let mut h = KeyHandler::new();
        assert_eq!(
            h.feed(b"ab\x02dq"),
            vec![
                KeyEvent::Bytes(b"ab".to_vec()),
                KeyEvent::Action(Action::Detach),
                KeyEvent::Bytes(b"q".to_vec()),
            ]
        );
    }

    #[test]
    fn default_bindings_cover_the_documented_keys() {
        let expected = [
            (b'd', Action::Detach),
            (b'%', Action::SplitVertical),
            (b'o', Action::NextPane),
            (b'[', Action::ScrollUp),
            (b']', Action::ScrollDown),
            (b's', Action::SyncInput),
            (b'q', Action::KillSession),
        ];
        let mut h = KeyHandler::new();
        for (key, action) in expected {
            assert_eq!(
                h.feed(&[PREFIX, key]),
                vec![KeyEvent::Action(action)],
                "key {key:?}"
            );
        }
    }

    #[test]
    fn parse_config_comments_blank_and_garbage() {
        let text = "\
# muxkit keybinds
#
prefix d detach_session
prefix % split_vertical

not a binding line
prefix    o    next_pane   # trailing comment
prefix ?? split_vertical
prefix n nonsense_action
";
        let m = parse_config(text);
        assert_eq!(m.len(), 3, "only the 3 valid lines bind: {m:?}");
        assert_eq!(m.get(&(PREFIX, b'd')), Some(&Action::Detach));
        assert_eq!(m.get(&(PREFIX, b'%')), Some(&Action::SplitVertical));
        assert_eq!(m.get(&(PREFIX, b'o')), Some(&Action::NextPane));
    }

    #[test]
    fn parse_config_action_aliases() {
        let text = "\
prefix a new_pane
prefix b scroll_reset
prefix c scroll_down
prefix e scroll_up
prefix f sync_input
prefix g kill_session
prefix h split-vertical
prefix j detach
";
        let m = parse_config(text);
        assert_eq!(m.get(&(PREFIX, b'a')), Some(&Action::SplitVertical));
        assert_eq!(m.get(&(PREFIX, b'b')), Some(&Action::ScrollReset));
        assert_eq!(m.get(&(PREFIX, b'c')), Some(&Action::ScrollDown));
        assert_eq!(m.get(&(PREFIX, b'e')), Some(&Action::ScrollUp));
        assert_eq!(m.get(&(PREFIX, b'f')), Some(&Action::SyncInput));
        assert_eq!(m.get(&(PREFIX, b'g')), Some(&Action::KillSession));
        assert_eq!(m.get(&(PREFIX, b'h')), Some(&Action::SplitVertical));
        assert_eq!(m.get(&(PREFIX, b'j')), Some(&Action::Detach));
    }

    #[test]
    fn parse_config_later_lines_win() {
        let m = parse_config("prefix d scroll_up\nprefix d detach_session\n");
        assert_eq!(m.get(&(PREFIX, b'd')), Some(&Action::Detach));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn with_config_overrides_defaults_and_keeps_the_rest() {
        let mut h = KeyHandler::with_config("prefix d scroll_reset\nprefix [ scroll_reset\n");
        assert_eq!(h.feed(&[PREFIX, b'd']), vec![KeyEvent::Action(Action::ScrollReset)]);
        assert_eq!(h.feed(&[PREFIX, b'[']), vec![KeyEvent::Action(Action::ScrollReset)]);
        assert_eq!(h.feed(&[PREFIX, b'q']), vec![KeyEvent::Action(Action::KillSession)]);
        // A handler built from a bad config still works.
        let mut h = KeyHandler::with_config("totally garbage\n");
        assert_eq!(h.feed(&[PREFIX, b'd']), vec![KeyEvent::Action(Action::Detach)]);
    }

    #[test]
    fn config_path_points_at_the_per_user_file() {
        let p = config_path();
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("keybinds.conf"));
        let mut parts: Vec<String> = p
            .components()
            .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
            .collect();
        parts.pop(); // keybinds.conf
        assert_eq!(parts.pop().as_deref(), Some("muxkit"));
    }

    #[test]
    fn load_config_tolerates_a_missing_file() {
        // config_path() points at the user's real file; only assert that the
        // call does not panic and returns Option.
        let _: Option<String> = load_config();
    }
}