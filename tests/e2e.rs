//! End-to-end test: drive the real daemon over the real IPC with a private
//! endpoint, and prove that a session keeps its terminal state across a
//! detach/attach cycle.
//!
//! Runs headless on Windows (ConPTY) — no console required.

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use muxkit::ipc;
use muxkit::proto::{C2S, S2C};

const TIMEOUT: Duration = Duration::from_secs(20);

/// Kills the daemon on drop so a failing test can never leak a server (or lock
/// `mk.exe` and break the next `cargo build`).
struct ServerProc(std::process::Child);

impl Drop for ServerProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn connect_retry(deadline: Instant) -> ipc::Conn {
    loop {
        match ipc::connect() {
            Ok(c) => return c,
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "server never accepted a connection"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Blocking recv until `f` matches. The server is expected to answer; the test
/// harness itself is the timeout (CI kills a stuck test).
fn recv_until<F>(conn: &mut ipc::Conn, what: &str, f: F) -> S2C
where
    F: Fn(&S2C) -> bool,
{
    loop {
        match conn.recv::<S2C>() {
            Ok(Some(m)) => {
                if f(&m) {
                    return m;
                }
            }
            Ok(None) => panic!("connection closed while waiting for {what}"),
            Err(e) => panic!("ipc error while waiting for {what}: {e}"),
        }
    }
}

/// Move frames to a channel so the test can use `recv_timeout`.
fn reader(conn: ipc::Conn) -> Receiver<S2C> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut conn = conn;
        loop {
            match conn.recv::<S2C>() {
                Ok(Some(m)) => {
                    if tx.send(m).is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    });
    rx
}

fn frame_text(msg: &S2C) -> String {
    match msg {
        S2C::Frame { updates, .. } => {
            let mut s = String::new();
            for u in updates {
                for r in &u.runs {
                    s.push_str(&r.s);
                }
            }
            s
        }
        _ => String::new(),
    }
}

/// Wait until any frame contains `needle`; returns true on success.
fn wait_for_text(rx: &Receiver<S2C>, needle: &str, deadline: Instant) -> bool {
    let mut seen = String::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            eprintln!("text never appeared: {needle}\n--- last frames ---\n{seen}");
            return false;
        }
        match rx.recv_timeout(left) {
            Ok(msg) => {
                let t = frame_text(&msg);
                seen.push_str(&t);
                if t.contains(needle) {
                    return true;
                }
            }
            Err(_) => {
                eprintln!("timeout waiting for {needle}\n--- last frames ---\n{seen}");
                return false;
            }
        }
    }
}

#[test]
fn e2e_echo_and_reattach_keeps_state() {
    // Private endpoint so this never touches a real user session.
    let name = format!("muxkit-e2e-{}", std::process::id());
    std::env::set_var("MUXKIT_NAME", &name);

    let exe = env!("CARGO_BIN_EXE_mk");
    let child = std::process::Command::new(exe)
        .arg("-S")
        .env("MUXKIT_NAME", &name)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn server");
    let mut server = ServerProc(child);

    let deadline = Instant::now() + TIMEOUT;
    let mut conn = connect_retry(deadline);
    conn.send(&C2S::Hello { cols: 80, rows: 24 })
        .expect("hello");
    match recv_until(&mut conn, "greeting", |m| matches!(m, S2C::Hello { .. })) {
        S2C::Hello { version, .. } => assert_eq!(version, 1),
        other => unreachable!("{other:?}"),
    }

    let shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
    conn.send(&C2S::Attach {
        session: None,
        cols: 80,
        rows: 24,
        shell: Some(shell.to_string()),
    })
    .expect("attach");
    match recv_until(&mut conn, "attach reply", |m| {
        matches!(m, S2C::Attached { .. } | S2C::Error { .. })
    }) {
        S2C::Attached { session, .. } => assert_eq!(session, 0),
        S2C::Error { msg } => panic!("attach failed: {msg}"),
        other => unreachable!("{other:?}"),
    }

    let frames = reader(conn.try_clone().expect("clone"));
    // Let the shell print its prompt first.
    let _ = frames.recv_timeout(Duration::from_secs(5));

    let marker = "MUXKIT_E2E_OK_42";
    conn.send(&C2S::Input {
        data: format!("echo {marker}\r").into_bytes(),
    })
    .expect("send input");
    assert!(
        wait_for_text(&frames, marker, Instant::now() + TIMEOUT),
        "the shell output never reached the frame stream"
    );

    // Detach: the client connection goes away, the session must survive.
    conn.send(&C2S::Detach).expect("detach");
    loop {
        match frames.recv_timeout(Duration::from_secs(5)) {
            Ok(S2C::Detached) => break,
            Ok(_) => continue,
            Err(_) => panic!("server did not confirm detach"),
        }
    }
    drop(conn);
    drop(frames);

    // Reattach to session 0: the first frame must repaint the previous screen.
    let mut conn = connect_retry(Instant::now() + TIMEOUT);
    conn.send(&C2S::Hello { cols: 80, rows: 24 })
        .expect("hello 2");
    let _ = recv_until(&mut conn, "greeting 2", |m| matches!(m, S2C::Hello { .. }));
    conn.send(&C2S::Attach {
        session: Some(0),
        cols: 80,
        rows: 24,
        shell: None,
    })
    .expect("reattach");
    match recv_until(&mut conn, "reattach reply", |m| {
        matches!(m, S2C::Attached { .. } | S2C::Error { .. })
    }) {
        S2C::Attached { session, .. } => assert_eq!(session, 0),
        S2C::Error { msg } => panic!("reattach failed: {msg}"),
        other => unreachable!("{other:?}"),
    }
    let frames = reader(conn.try_clone().expect("clone 2"));
    assert!(
        wait_for_text(&frames, marker, Instant::now() + TIMEOUT),
        "the screen state was lost across detach/attach"
    );

    // Clean shutdown.
    conn.send(&C2S::Kill { session: 0 }).expect("kill");
    conn.send(&C2S::Shutdown).expect("shutdown");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match server.0.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "server did not exit after shutdown"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
}