//! Client (owned by Lead).
//!
//! Deliberately dumb: the server owns every session, so the client only
//! * draws frames sent by the server, * forwards raw input, * handles prefix
//! keys. That is what lets a session outlive its terminal.

use std::io::Read;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::canvas;
use crate::ipc;
use crate::keys::{Action, KeyEvent, KeyHandler};
use crate::proto::*;
use crate::tty::{self, Tty};

enum CEv {
    Msg(S2C),
    Input(Vec<u8>),
    Resize(u16, u16),
    Eof,
}

/// Windows console handle creation flags: never pop a console for the daemon
/// and never inherit our process group.
#[cfg(windows)]
const CREATE_FLAGS: u32 = 0x0800_0000 /* CREATE_NO_WINDOW */ | 0x0000_0200 /* NEW_PROCESS_GROUP */;

fn ensure_server() -> Result<()> {
    if ipc::connect().is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("cannot locate mk.exe")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("-S")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_FLAGS);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    cmd.spawn().context("cannot spawn the muxkit server")?;

    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(100));
        if ipc::connect().is_ok() {
            return Ok(());
        }
    }
    bail!("the muxkit server did not come up");
}

pub fn run_attach(session: Option<u32>, shell: Option<String>) -> Result<()> {
    ensure_server()?;
    let mut tty = Tty::enter()?;
    let (cols, rows) = tty.size();
    let result = attach_loop(&mut tty, session, shell, cols, rows);
    drop(tty);
    result
}

fn attach_loop(
    tty: &mut Tty,
    session: Option<u32>,
    shell: Option<String>,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let mut conn = ipc::connect()?;
    conn.send(&C2S::Hello { cols, rows })?;
    match conn.recv::<S2C>()? {
        Some(S2C::Hello { .. }) => {}
        Some(S2C::Error { msg }) => bail!("{msg}"),
        _ => bail!("unexpected greeting from the server"),
    }
    conn.send(&C2S::Attach {
        session,
        cols,
        rows,
        shell,
    })?;
    match conn.recv::<S2C>()? {
        Some(S2C::Attached { .. }) => {}
        Some(S2C::Error { msg }) => bail!("{msg}"),
        _ => bail!("attach failed"),
    }

    let (tx, rx): (Sender<CEv>, Receiver<CEv>) = mpsc::channel();

    // server -> client frames
    {
        let mut rconn = conn.try_clone()?;
        let tx = tx.clone();
        std::thread::spawn(move || loop {
            match rconn.recv::<S2C>() {
                Ok(Some(msg)) => {
                    if tx.send(CEv::Msg(msg)).is_err() {
                        break;
                    }
                }
                _ => {
                    let _ = tx.send(CEv::Eof);
                    break;
                }
            }
        });
    }

    // console -> client. `Tty::enter` already put the console in raw VT input
    // mode, so reading stdin yields raw escape sequences (same as Unix).
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 8192];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(CEv::Eof);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(CEv::Input(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(CEv::Eof);
                        break;
                    }
                }
            }
        });
    }

    // window resize (Windows has no SIGWINCH: the watcher polls)
    {
        let (stx, srx) = mpsc::channel::<(u16, u16)>();
        tty::spawn_size_watcher(stx);
        let tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok((c, r)) = srx.recv() {
                if tx.send(CEv::Resize(c, r)).is_err() {
                    break;
                }
            }
        });
    }

    tty.write_all(b"\x1b[2J\x1b[H")?;

    let mut keys = KeyHandler::new();
    let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut exit_msg: Option<String> = None;

    loop {
        let ev = match rx.recv() {
            Ok(ev) => ev,
            Err(_) => break,
        };
        match ev {
            CEv::Msg(msg) => match msg {
                S2C::Frame {
                    updates,
                    cursor,
                    cursor_visible,
                    ..
                } => {
                    out.clear();
                    canvas::encode(&updates, &mut out);
                    match cursor {
                        Some(cp) if cursor_visible => {
                            canvas::encode_cursor(cp.x, cp.y, true, &mut out)
                        }
                        _ => canvas::encode_cursor(0, 0, false, &mut out),
                    }
                    if !out.is_empty() {
                        tty.write_all(&out)?;
                    }
                }
                S2C::SessionList { sessions } => {
                    for s in sessions {
                        let mark = if s.attached { "*" } else { " " };
                        eprintln!(
                            "{} {}  {} pane(s)  {}",
                            mark,
                            s.id,
                            s.panes,
                            if s.title.is_empty() { "-" } else { &s.title }
                        );
                    }
                }
                S2C::Detached | S2C::Exited => break,
                S2C::Error { msg } => {
                    exit_msg = Some(msg);
                    break;
                }
                S2C::Info { .. } => {
                    // status-bar level hints are not drawn for now
                }
                _ => {}
            },
            CEv::Input(bytes) => {
                for ev in keys.feed(&bytes) {
                    match ev {
                        KeyEvent::Bytes(b) => {
                            if !b.is_empty() {
                                conn.send(&C2S::Input { data: b })?;
                            }
                        }
                        KeyEvent::Action(a) => match a {
                            Action::Detach => {
                                let _ = conn.send(&C2S::Detach);
                                return Ok(());
                            }
                            Action::KillSession => {
                                let _ = conn.send(&C2S::Command {
                                    action: PaneAction::KillSession,
                                });
                            }
                            other => {
                                let action = match other {
                                    Action::SplitVertical => PaneAction::SplitVertical,
                                    Action::NextPane => PaneAction::NextPane,
                                    Action::ScrollUp => PaneAction::ScrollUp,
                                    Action::ScrollDown => PaneAction::ScrollDown,
                                    Action::ScrollReset => PaneAction::ScrollReset,
                                    Action::SyncInput => PaneAction::SyncInput,
                                    Action::Detach | Action::KillSession => unreachable!(),
                                };
                                conn.send(&C2S::Command { action })?;
                            }
                        },
                    }
                }
            }
            CEv::Resize(cols, rows) => {
                let _ = conn.send(&C2S::Resize { cols, rows });
                tty.write_all(b"\x1b[2J\x1b[H")?;
            }
            CEv::Eof => break,
        }
    }

    if let Some(msg) = exit_msg {
        bail!("{msg}");
    }
    Ok(())
}

pub fn run_list() -> Result<()> {
    ensure_server()?;
    let mut conn = ipc::connect()?;
    conn.send(&C2S::Hello { cols: 80, rows: 24 })?;
    let _ = conn.recv::<S2C>()?;
    conn.send(&C2S::List)?;
    match conn.recv::<S2C>()? {
        Some(S2C::SessionList { sessions }) => {
            if sessions.is_empty() {
                println!("no sessions");
            }
            for s in sessions {
                println!(
                    "{}{}  {} pane(s)  {}",
                    if s.attached { "* " } else { "  " },
                    s.id,
                    s.panes,
                    if s.title.is_empty() { "-" } else { &s.title }
                );
            }
            Ok(())
        }
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub fn run_kill(session: u32) -> Result<()> {
    ensure_server()?;
    let mut conn = ipc::connect()?;
    conn.send(&C2S::Hello { cols: 80, rows: 24 })?;
    let _ = conn.recv::<S2C>()?;
    conn.send(&C2S::Kill { session })?;
    println!("killed session {session}");
    Ok(())
}