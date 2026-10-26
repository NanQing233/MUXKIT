//! Server daemon (owned by Lead).
//!
//! One engine thread owns *all* state — sessions, panes, terminal screens,
//! attached clients — so there is no lock contention and no torn frames.
//! Everything else talks to it over one `mpsc` channel:
//!
//! * accept loop  -> `Ev::NewClient`
//! * per-client reader thread -> `Ev::Cmd`
//! * per-pane pty reader thread -> `Ev::Output` / `Ev::PaneExit`
//!
//! A session keeps running (and keeps its terminal state) while detached; that
//! is what makes `mk -s <id>` able to reattach to it later.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::Result;

use crate::canvas::Canvas;
use crate::ipc;
use crate::layout;
use crate::proto::*;
use crate::pty::Pty;
use crate::term::Screen;

pub enum Ev {
    NewClient {
        id: u64,
        writer: ipc::Conn,
    },
    Cmd {
        id: u64,
        msg: C2S,
    },
    Output {
        pane: PaneId,
        data: Vec<u8>,
    },
    PaneExit {
        pane: PaneId,
    },
    ClientGone {
        id: u64,
    },
}

struct Pane {
    id: PaneId,
    term: Screen,
    pty: Pty,
    alive: bool,
}

struct Session {
    id: SessionId,
    panes: Vec<Pane>,
    focused: usize,
    cols: u16,
    rows: u16,
    attached: Option<u64>,
    title: String,
    sync_input: bool,
}

struct ClientState {
    writer: ipc::Conn,
    cols: u16,
    rows: u16,
    session: Option<SessionId>,
    last: Option<Canvas>,
    last_cursor: Option<(u16, u16, bool)>,
}

pub struct Server {
    sessions: Vec<Session>,
    clients: HashMap<u64, ClientState>,
    tx: Sender<Ev>,
    next_session: SessionId,
    next_pane: PaneId,
    stop: bool,
}

impl Server {
    fn new(tx: Sender<Ev>) -> Self {
        Server {
            sessions: Vec::new(),
            clients: HashMap::new(),
            tx,
            next_session: 0,
            next_pane: 0,
            stop: false,
        }
    }

    fn run(&mut self, rx: Receiver<Ev>) {
        let mut idle = std::time::Instant::now();
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(ev) => self.handle(ev),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if self.stop {
                break;
            }
            // A daemon with no sessions and no clients lingers briefly so that
            // a client reconnect right after detach does not pay a cold start.
            if self.sessions.is_empty() && self.clients.is_empty() {
                if idle.elapsed() > std::time::Duration::from_millis(3000) {
                    break;
                }
            } else {
                idle = std::time::Instant::now();
            }
        }
    }

    fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::NewClient { id, writer } => {
                // Greeting is sent in reply to C2S::Hello (see on_cmd); sending
                // it here too would put an extra Hello in front of every
                // Attached reply.
                self.clients.insert(
                    id,
                    ClientState {
                        writer,
                        cols: 80,
                        rows: 24,
                        session: None,
                        last: None,
                        last_cursor: None,
                    },
                );
            }
            Ev::Cmd { id, msg } => self.on_cmd(id, msg),
            Ev::Output { pane, data } => {
                if let Some(sess) = self.session_of_pane_mut(pane) {
                    if let Some(p) = sess.panes.iter_mut().find(|p| p.id == pane) {
                        p.term.feed(&data);
                        // Answer terminal queries (DA1 / DSR / CPR) back to the
                        // child; without this, vim/htop/PSReadLine can stall.
                        let replies = p.term.take_replies();
                        if !replies.is_empty() {
                            let _ = p.pty.write(&replies);
                        }
                        sess.title = p.term.title();
                    }
                    let sid = sess.id;
                    self.pump_session(sid);
                }
            }
            Ev::PaneExit { pane } => {
                let Some(sess) = self.session_of_pane_mut(pane) else {
                    return;
                };
                if let Some(p) = sess.panes.iter_mut().find(|p| p.id == pane) {
                    p.alive = false;
                }
                let sid = sess.id;
                if sess.panes.iter().all(|p| !p.alive) {
                    self.close_session(sid);
                } else {
                    sess.panes.retain(|p| p.alive);
                    if sess.focused >= sess.panes.len() {
                        sess.focused = sess.panes.len().saturating_sub(1);
                    }
                    self.resize_session(sid);
                    self.pump_session(sid);
                }
            }
            Ev::ClientGone { id } => {
                if let Some(c) = self.clients.remove(&id) {
                    if let Some(sid) = c.session {
                        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                            if s.attached == Some(id) {
                                s.attached = None;
                            }
                        }
                    }
                }
                self.gc();
            }
        }
    }

    fn on_cmd(&mut self, id: u64, msg: C2S) {
        match msg {
            C2S::Hello { cols, rows } => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.cols = cols.max(1);
                    c.rows = rows.max(2);
                }
                let sessions = self.session_infos();
                if let Some(c) = self.clients.get_mut(&id) {
                    let _ = c.writer.send(&S2C::Hello {
                        version: PROTO_VERSION,
                        sessions,
                    });
                }
            }
            C2S::Attach {
                session,
                cols,
                rows,
                shell,
            } => self.attach(id, session, cols, rows, shell),
            C2S::Input { data } => self.on_input(id, &data),
            C2S::Command { action } => self.on_action(id, action),
            C2S::Resize { cols, rows } => {
                let cols = cols.max(1);
                let rows = rows.max(2);
                let Some(sid) = self.clients.get(&id).and_then(|c| c.session) else {
                    return;
                };
                if let Some(c) = self.clients.get_mut(&id) {
                    c.cols = cols;
                    c.rows = rows;
                    c.last = None;
                }
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.cols = cols;
                    s.rows = rows;
                }
                self.resize_session(sid);
                self.pump_session(sid);
            }
            C2S::Detach => self.detach(id),
            C2S::List => {
                let sessions = self.session_infos();
                if let Some(c) = self.clients.get_mut(&id) {
                    let _ = c.writer.send(&S2C::SessionList { sessions });
                }
            }
            C2S::Kill { session } => {
                self.close_session(session);
                let sessions = self.session_infos();
                if let Some(c) = self.clients.get_mut(&id) {
                    let _ = c.writer.send(&S2C::SessionList { sessions });
                }
                self.gc();
            }
            C2S::Shutdown => {
                self.stop = true;
            }
        }
    }

    fn on_action(&mut self, id: u64, action: PaneAction) {
        let Some(sid) = self.clients.get(&id).and_then(|c| c.session) else {
            return;
        };
        match action {
            PaneAction::SplitVertical => {
                let (cols, rows, n) = match self.sessions.iter().find(|s| s.id == sid) {
                    Some(s) => (s.cols, s.rows, s.panes.len() + 1),
                    None => return,
                };
                let rects = layout::split_vertical(cols, rows, n);
                let r = rects.get(n - 1).copied().unwrap_or(Rect::new(0, 0, cols, rows));
                let shell = crate::pty::default_shell();
                match self.spawn_pane(&shell, r.w.max(1), r.h.max(1)) {
                    Ok(p) => {
                        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                            s.panes.push(p);
                            s.focused = s.panes.len() - 1;
                        }
                        self.resize_session(sid);
                        self.pump_session(sid);
                    }
                    Err(e) => self.send_err(id, &format!("split failed: {e}")),
                }
            }
            PaneAction::NextPane => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    if !s.panes.is_empty() {
                        s.focused = (s.focused + 1) % s.panes.len();
                    }
                }
                self.pump_session(sid);
            }
            PaneAction::ScrollUp | PaneAction::ScrollDown | PaneAction::ScrollReset => {
                let mut changed = false;
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    let fi = s.focused.min(s.panes.len().saturating_sub(1));
                    if let Some(p) = s.panes.get_mut(fi) {
                        match action {
                            PaneAction::ScrollUp => p.term.scroll_up(5),
                            PaneAction::ScrollDown => p.term.scroll_down(5),
                            _ => p.term.scroll_reset(),
                        }
                        changed = true;
                    }
                }
                if changed {
                    self.pump_session(sid);
                }
            }
            PaneAction::SyncInput => {
                let mut now = false;
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.sync_input = !s.sync_input;
                    now = s.sync_input;
                }
                if let Some(c) = self.clients.get_mut(&id) {
                    let _ = c.writer.send(&S2C::Info {
                        msg: format!("sync input: {}", if now { "on" } else { "off" }),
                    });
                }
            }
            PaneAction::KillSession => {
                self.close_session(sid);
            }
        }
    }

    fn attach(
        &mut self,
        id: u64,
        wanted: Option<SessionId>,
        cols: u16,
        rows: u16,
        shell: Option<String>,
    ) {
        let cols = cols.max(1);
        let rows = rows.max(2);
        if let Some(c) = self.clients.get_mut(&id) {
            c.cols = cols;
            c.rows = rows;
            c.last = None;
        }

        let sid = match wanted {
            Some(sid) if self.sessions.iter().any(|s| s.id == sid) => sid,
            Some(_) => {
                self.send_err(id, "no such session");
                return;
            }
            None => match self.new_session(cols, rows, shell) {
                Ok(sid) => sid,
                Err(e) => {
                    self.send_err(id, &format!("cannot start shell: {e}"));
                    return;
                }
            },
        };

        // One client per session, like tmux: a new attach kicks the old one.
        if let Some(s) = self.sessions.iter().find(|s| s.id == sid) {
            if let Some(old) = s.attached {
                if old != id {
                    if let Some(mut oc) = self.clients.remove(&old) {
                        let _ = oc.writer.send(&S2C::Detached);
                    }
                }
            }
        }

        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
            s.attached = Some(id);
            s.cols = cols;
            s.rows = rows;
        }
        if let Some(c) = self.clients.get_mut(&id) {
            c.session = Some(sid);
            c.last = None;
            c.last_cursor = None;
            let _ = c.writer.send(&S2C::Attached {
                session: sid,
                cols,
                rows,
            });
        }
        self.resize_session(sid);
        self.pump_session(sid);
    }

    fn new_session(&mut self, cols: u16, rows: u16, shell: Option<String>) -> Result<SessionId> {
        let id = self.next_session;
        self.next_session += 1;
        let shell = shell.unwrap_or_else(crate::pty::default_shell);
        let rects = layout::split_vertical(cols, rows, 1);
        let rect = rects.first().copied().unwrap_or(Rect::new(0, 0, cols, rows - 1));
        let pane = self.spawn_pane(&shell, rect.w, rect.h)?;
        self.sessions.push(Session {
            id,
            panes: vec![pane],
            focused: 0,
            cols,
            rows,
            attached: None,
            title: String::new(),
            sync_input: false,
        });
        Ok(id)
    }

    fn spawn_pane(&mut self, shell: &str, cols: u16, rows: u16) -> Result<Pane> {
        let pane_id = self.next_pane;
        self.next_pane += 1;
        let tx = self.tx.clone();
        let on_data: Box<dyn Fn(Vec<u8>) + Send> = Box::new(move |data: Vec<u8>| {
            let _ = tx.send(Ev::Output { pane: pane_id, data });
        });
        let tx2 = self.tx.clone();
        let on_exit: Box<dyn FnOnce() + Send> = Box::new(move || {
            let _ = tx2.send(Ev::PaneExit { pane: pane_id });
        });
        let pty = Pty::spawn(shell, cols.max(1), rows.max(1), on_data, on_exit)?;
        Ok(Pane {
            id: pane_id,
            term: Screen::new(cols.max(1), rows.max(1)),
            pty,
            alive: true,
        })
    }

    fn on_input(&mut self, id: u64, data: &[u8]) {
        let Some(sid) = self.clients.get(&id).and_then(|c| c.session) else {
            return;
        };
        let sync = self
            .sessions
            .iter()
            .find(|s| s.id == sid)
            .map(|s| s.sync_input)
            .unwrap_or(false);

        // Sending input to a pane that is scrolled back snaps it to the live
        // screen first (same behaviour as muxkit/tmux).
        let mut was_scrolled = false;
        if let Some(sess) = self.sessions.iter_mut().find(|s| s.id == sid) {
            let fi = sess.focused.min(sess.panes.len().saturating_sub(1));
            if let Some(p) = sess.panes.get_mut(fi) {
                if p.term.scroll_offset() > 0 {
                    p.term.scroll_reset();
                    was_scrolled = true;
                }
            }
        }
        if was_scrolled {
            self.pump_session(sid);
        }

        let Some(sess) = self.sessions.iter_mut().find(|s| s.id == sid) else {
            return;
        };
        let fi = sess.focused.min(sess.panes.len().saturating_sub(1));
        if sync {
            for p in sess.panes.iter_mut() {
                let _ = p.pty.write(data);
            }
        } else if let Some(p) = sess.panes.get_mut(fi) {
            let _ = p.pty.write(data);
        }
    }

    fn detach(&mut self, id: u64) {
        let Some(sid) = self.clients.get(&id).and_then(|c| c.session) else {
            return;
        };
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
            if s.attached == Some(id) {
                s.attached = None;
            }
        }
        if let Some(c) = self.clients.get_mut(&id) {
            c.session = None;
            let _ = c.writer.send(&S2C::Detached);
        }
        self.gc();
    }

    fn close_session(&mut self, sid: SessionId) {
        if let Some(s) = self.sessions.iter().position(|s| s.id == sid) {
            let mut sess = self.sessions.remove(s);
            for p in sess.panes.iter_mut() {
                p.pty.kill();
            }
            if let Some(cid) = sess.attached {
                if let Some(c) = self.clients.get_mut(&cid) {
                    c.session = None;
                    let _ = c.writer.send(&S2C::Exited);
                }
            }
        }
    }

    fn gc(&mut self) {
        // Stop is decided by the idle timer in run(); nothing to do here yet.
    }

    fn send_err(&mut self, id: u64, msg: &str) {
        if let Some(c) = self.clients.get_mut(&id) {
            let _ = c.writer.send(&S2C::Error {
                msg: msg.to_string(),
            });
        }
    }

    fn session_infos(&self) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .map(|s| SessionInfo {
                id: s.id,
                attached: s.attached.is_some(),
                panes: s.panes.len() as u16,
                title: s.title.clone(),
            })
            .collect()
    }

    fn session_of_pane_mut(&mut self, pane: PaneId) -> Option<&mut Session> {
        self.sessions
            .iter_mut()
            .find(|s| s.panes.iter().any(|p| p.id == pane))
    }

    fn resize_session(&mut self, sid: SessionId) {
        let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) else {
            return;
        };
        let (cols, rows) = (s.cols, s.rows);
        let rects = layout::split_vertical(cols, rows, s.panes.len());
        for (i, p) in s.panes.iter_mut().enumerate() {
            let r = rects.get(i).copied().unwrap_or(Rect::new(0, 0, cols, rows));
            p.term.resize(r.w.max(1), r.h.max(1));
            let _ = p.pty.resize(r.w.max(1), r.h.max(1));
        }
    }

    /// Rebuild one session's canvas for every client attached to it and send
    /// the rows that changed (row granularity, one frame per event).
    fn pump_session(&mut self, sid: SessionId) {
        let Some(sess) = self.sessions.iter().find(|s| s.id == sid) else {
            return;
        };
        let clients: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, c)| c.session == Some(sid))
            .map(|(id, _)| *id)
            .collect();
        if clients.is_empty() {
            return;
        }
        let cursor = self.cursor_of(sess);
        for cid in clients {
            let (cols, rows) = match self.clients.get(&cid) {
                Some(c) => (c.cols, c.rows),
                None => continue,
            };
            let frame = self.build_canvas(sess, cols, rows);
            let (updates, send_cursor) = {
                let Some(c) = self.clients.get(&cid) else {
                    continue;
                };
                let updates = match &c.last {
                    Some(prev) => frame.diff(prev),
                    None => full_diff(&frame),
                };
                let send_cursor = cursor != c.last_cursor;
                (updates, send_cursor)
            };
            if updates.is_empty() && !send_cursor {
                continue;
            }
            let msg = S2C::Frame {
                cols,
                rows,
                updates,
                cursor: cursor.map(|(x, y, _)| CursorPos { x, y }),
                cursor_visible: cursor.map(|(_, _, v)| v).unwrap_or(false),
            };
            let Some(c) = self.clients.get_mut(&cid) else {
                continue;
            };
            if c.writer.send(&msg).is_err() {
                c.session = None;
                continue;
            }
            c.last = Some(frame);
            c.last_cursor = cursor;
        }
    }

    fn cursor_of(&self, sess: &Session) -> Option<(u16, u16, bool)> {
        let rects = layout::split_vertical(sess.cols, sess.rows, sess.panes.len());
        let p = sess.panes.get(sess.focused)?;
        if !p.alive || p.term.scroll_offset() > 0 || !p.term.cursor_visible() {
            return None;
        }
        let r = rects.get(sess.focused).copied()?;
        let (cx, cy) = p.term.cursor();
        let x = r.x.saturating_add(cx);
        let y = r.y.saturating_add(cy);
        if x >= sess.cols || y >= sess.rows.saturating_sub(1) {
            return None;
        }
        Some((x, y, true))
    }

    fn build_canvas(&self, sess: &Session, cols: u16, rows: u16) -> Canvas {
        let mut canvas = Canvas::new(cols, rows);
        let status_style = Style::new(
            Color::Default,
            if sess.panes.len() > 1 {
                Color::Indexed(24)
            } else {
                Color::Indexed(238)
            },
            Attrs(0).with(Attrs::REVERSE, true),
        );
        canvas.clear(Style::new(Color::Default, Color::Default, Attrs(0)));

        let border_style = Style::new(Color::Indexed(244), Color::Default, Attrs(0));
        let rects = layout::split_vertical(cols, rows, sess.panes.len());

        for (i, p) in sess.panes.iter().enumerate() {
            let Some(r) = rects.get(i).copied() else {
                continue;
            };
            let visible = p.term.visible_rows();
            canvas.blit(r.x, r.y, r.w, r.h, &visible);
            if sess.panes.len() > 1 {
                // separator column to the right of every pane but the last
                if i + 1 < sess.panes.len() {
                    let x = r.right();
                    let style = if i == sess.focused {
                        Style::new(Color::Indexed(39), Color::Default, Attrs::new(Attrs::BOLD))
                    } else {
                        border_style
                    };
                    for y in 0..r.h {
                        let mut c = Cell::default();
                        c.ch = '│';
                        c.fg = style.fg;
                        c.bg = style.bg;
                        c.attr = style.attr;
                        canvas.put(x, y, c);
                    }
                }
            }
        }

        // status bar on the last row
        if rows >= 2 {
            let y = rows - 1;
            for x in 0..cols {
                let mut c = Cell::default();
                c.ch = ' ';
                c.bg = status_style.bg;
                c.attr = status_style.attr;
                canvas.put(x, y, c);
            }
            let title = if sess.title.is_empty() {
                String::new()
            } else {
                format!(" | {}", sess.title)
            };
            let text = format!(
                " mk | session {} | pane {}/{} | {} panes{} | Ctrl+B ? ",
                sess.id,
                sess.focused + 1,
                sess.panes.len(),
                sess.panes.len(),
                title
            );
            canvas.put_str(0, y, &text, status_style, cols);
        }
        canvas
    }
}

/// Full redraw for a freshly attached client: every row that differs from a
/// blank screen. The receiver of `diff` supplies the runs, so the *frame* must
/// be the receiver — `blank.diff(&frame)` would yield row indexes with blank
/// text (that was a real bug).
fn full_diff(c: &Canvas) -> Vec<RowUpdate> {
    c.diff(&Canvas::new(c.w, c.h))
}

pub fn run() -> Result<()> {
    let listener = ipc::listen()?;
    let (tx, rx) = mpsc::channel::<Ev>();
    let engine_tx = tx.clone();

    std::thread::spawn(move || {
        let mut srv = Server::new(engine_tx);
        srv.run(rx);
        std::process::exit(0);
    });

    let mut next_client: u64 = 0;
    loop {
        match listener.accept() {
            Ok(conn) => {
                let id = next_client;
                next_client += 1;
                match conn.try_clone() {
                    Ok(writer) => {
                        if tx.send(Ev::NewClient { id, writer }).is_err() {
                            break;
                        }
                        let tx = tx.clone();
                        std::thread::spawn(move || {
                            let mut conn = conn;
                            loop {
                                match conn.recv::<C2S>() {
                                    Ok(Some(msg)) => {
                                        if tx.send(Ev::Cmd { id, msg }).is_err() {
                                            break;
                                        }
                                    }
                                    Ok(None) | Err(_) => {
                                        let _ = tx.send(Ev::ClientGone { id });
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(_) => continue,
                }
            }
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    Ok(())
}