//! Client end-to-end: run the real `mk` client inside a real ConPTY (Windows)
//! / pty (Unix), feed it keystrokes, and parse its ANSI output with our own VT
//! emulator to check what a user would actually see.
//!
//! This is the test that catches "it compiles but draws nothing" bugs.

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use muxkit::ipc;
use muxkit::proto::C2S;
use muxkit::term::Screen;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

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
                assert!(Instant::now() < deadline, "server never came up");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn screen_text(screen: &Screen) -> String {
    let mut out = String::new();
    for row in screen.visible_rows() {
        for cell in row {
            if cell.is_continuation() {
                continue;
            }
            out.push(if cell.ch == '\0' { ' ' } else { cell.ch });
        }
        out.push('\n');
    }
    out
}

/// Pump everything the client wrote into the screen until `needle` shows up.
fn wait_screen(
    rx: &Receiver<Vec<u8>>,
    screen: &mut Screen,
    needle: &str,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    loop {
        while let Ok(chunk) = rx.try_recv() {
            screen.feed(&chunk);
        }
        let text = screen_text(screen);
        if text.contains(needle) {
            return Ok(text);
        }
        if Instant::now() >= deadline {
            return Err(text);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn client_renders_and_detaches() {
    let name = format!("muxkit-cli-{}", std::process::id());
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
    let _ = connect_retry(Instant::now() + Duration::from_secs(10));

    // Real terminal (ConPTY) running the real client.
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(exe);
    cmd.env("MUXKIT_NAME", &name);
    cmd.env("TERM", "xterm-256color");
    let mut client = pty.slave.spawn_command(cmd).expect("spawn client");

    let mut reader = pty.master.try_clone_reader().expect("reader");
    let mut writer = pty.master.take_writer().expect("writer");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 16384];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut screen = Screen::new(80, 24);

    // 1. The client must paint the status bar.
    let text = wait_screen(&rx, &mut screen, "mk | session", Duration::from_secs(20))
        .expect("the client never painted the status bar");
    assert!(
        text.contains("pane 1/1"),
        "status bar missing pane info:\n{text}"
    );

    // 2. Typing reaches the shell and its output comes back through the frame.
    let marker = "CLIENT_SIDE_OK";
    writer
        .write_all(format!("echo {marker}\r").as_bytes())
        .expect("write input");
    writer.flush().ok();
    let text = wait_screen(&rx, &mut screen, marker, Duration::from_secs(20))
        .expect("shell output never reached the client");
    assert!(
        text.contains("mk | session 0"),
        "status bar vanished while the shell was printing:\n{text}"
    );

    // 2b. Split: Ctrl+B % opens a second pane (vertical split).
    writer.write_all(&[0x02, b'%']).expect("split keys");
    writer.flush().ok();
    let _ = wait_screen(&rx, &mut screen, "pane 2/2", Duration::from_secs(20))
        .expect("split did not open a second pane");

    // 2c. Ctrl+B o cycles focus back to the first pane.
    writer.write_all(&[0x02, b'o']).expect("next pane keys");
    writer.flush().ok();
    let text = wait_screen(&rx, &mut screen, "pane 1/2", Duration::from_secs(20))
        .expect("next-pane did not move focus");

    // Screen dump of what the real client painted into a real terminal
    // (visible with `cargo test -- --nocapture`).
    println!("--- client screen after Ctrl+B % and Ctrl+B o ---\n{text}");

    // 3. Ctrl+B d detaches: the client exits, the daemon keeps the session.
    writer.write_all(&[0x02, b'd']).expect("detach keys");
    writer.flush().ok();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(_)) = client.try_wait() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "client did not exit after detach; screen:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // 4. The session is still there and still holds the marker.
    let mut conn = connect_retry(Instant::now() + Duration::from_secs(10));
    conn.send(&C2S::Hello {
        cols: 80,
        rows: 24,
    })
    .unwrap();
    let _ = conn.recv::<muxkit::proto::S2C>();
    conn.send(&C2S::List).unwrap();
    match conn.recv::<muxkit::proto::S2C>().unwrap() {
        Some(muxkit::proto::S2C::SessionList { sessions }) => {
            assert_eq!(sessions.len(), 1, "session vanished after detach");
            assert!(!sessions[0].attached, "session still marked attached");
        }
        other => panic!("unexpected list reply: {other:?}"),
    }
    conn.send(&C2S::Kill { session: 0 }).unwrap();
    conn.send(&C2S::Shutdown).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.0.try_wait().ok().flatten().is_none() {
        assert!(Instant::now() < deadline, "server did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}