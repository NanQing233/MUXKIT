//! Local IPC: named pipes on Windows, Unix domain sockets on Unix.
//!
//! OWNER: teammate `ipc-proto`. Only this file.
//!
//! Backed by the `interprocess` crate (`LocalSocketListener` /
//! `LocalSocketStream`); messages are length-prefixed JSON frames so both ends
//! can be debugged with a hexdump.
//!
//! # Wire format
//!
//! One frame is four bytes of little-endian `u32` length followed by exactly
//! that many bytes of UTF-8 JSON (a serialized [`serde`] value). A frame whose
//! declared length exceeds [`MAX_FRAME`] is rejected before any allocation of
//! that size happens.
//!
//! # Endpoint naming
//!
//! The endpoint name comes from [`pipe_name()`] (overridable through the
//! `MUXKIT_NAME` environment variable for isolated end-to-end tests) and is
//! mapped through `ToNsName::to_ns_name::<GenericNamespaced>()`, which on
//! Windows becomes `\\.\pipe\muxkit-rs-<user>` and on Unix a namespaced
//! (abstract / filesystem-free) socket. No files are created on disk.
//!
//! # "Already running"
//!
//! Binding the endpoint is the daemon's startup step, and a second daemon must
//! not start. [`listen`] therefore reports that specific failure as a typed
//! [`AlreadyRunning`] error, and [`already_running`] is the supported way for
//! callers to test for it:
//!
//! ```ignore
//! if let Err(e) = ipc::listen() {
//!     if ipc::already_running(&e) {
//!         // another `mk -S` owns the endpoint: connect to it instead
//!     } else {
//!         return Err(e);
//!     }
//! }
//! ```

use anyhow::{Context, Result};
use interprocess::local_socket::{
    prelude::{LocalSocketListener, LocalSocketStream},
    traits::{Listener as _, Stream as _},
    GenericNamespaced, ListenerOptions, Name, ToNsName,
};
use interprocess::TryClone;
use serde::{de::DeserializeOwned, Serialize};
use std::io::{self, Read, Write};

/// Maximum accepted frame size; protects against garbage input.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Size of the little-endian `u32` frame length prefix.
const LEN_PREFIX: usize = 4;

/// Stable, per-user pipe/socket name.
///
/// `MUXKIT_NAME` overrides the derived name when it is set to a non-empty
/// value. End-to-end tests use that to run a private server
/// (`MUXKIT_NAME=muxkit-e2e-<pid>`) without touching the user's real sessions.
pub fn pipe_name() -> String {
    if let Ok(name) = std::env::var("MUXKIT_NAME") {
        if !name.is_empty() {
            return name;
        }
    }
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "default".into());
    format!("muxkit-rs-{}", user)
}

/// The endpoint name is already bound by another listener (the daemon is
/// already running).
///
/// [`listen`] returns this — via `anyhow` — instead of a generic I/O error so
/// that the "someone else is already serving" case is distinguishable from a
/// real failure such as a broken/denied endpoint. Use [`already_running`] to
/// test for it.
#[derive(Debug)]
pub struct AlreadyRunning(io::Error);

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "muxkit IPC endpoint is already in use by another server: {}",
            self.0
        )
    }
}

impl std::error::Error for AlreadyRunning {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// True if `e` came from [`listen`] because the endpoint was already taken.
pub fn already_running(e: &anyhow::Error) -> bool {
    // `listen` stores `AlreadyRunning` itself as the root of the anyhow error
    // (no `.context()` wrapper in front of it), so a plain downcast is enough.
    e.downcast_ref::<AlreadyRunning>().is_some()
}

/// Does this bind failure mean "the name is taken"?
///
/// `AddrInUse` is what Unix reports. On Windows the endpoint is a named pipe,
/// and `CreateNamedPipeW` with `FILE_FLAG_FIRST_PIPE_INSTANCE` — which is what
/// `interprocess` uses for the first instance of a listener — reports a name
/// collision as `ERROR_ACCESS_DENIED` (5), which `std` maps to
/// `ErrorKind::PermissionDenied`. `interprocess` does not remap that to
/// `AddrInUse`, so it is recognised explicitly here. The endpoint lives in the
/// current user's pipe namespace, so a "real" access denial on it is not a case
/// worth distinguishing.
fn bind_conflict(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::AddrInUse {
        return true;
    }
    #[cfg(windows)]
    {
        const ERROR_ACCESS_DENIED: i32 = 5;
        if e.kind() == io::ErrorKind::PermissionDenied
            && e.raw_os_error() == Some(ERROR_ACCESS_DENIED)
        {
            return true;
        }
    }
    false
}

/// Bind a listener to an explicit endpoint name (used by the tests).
fn listen_named(name: &str) -> Result<Listener> {
    let ns: Name<'_> = name
        .to_ns_name::<GenericNamespaced>()
        .with_context(|| format!("invalid IPC endpoint name {name:?}"))?;
    match ListenerOptions::new().name(ns).create_sync() {
        Ok(inner) => Ok(Listener { inner }),
        // Deliberately no `.context()` here: the marker must stay the root of
        // the anyhow error so `already_running` can find it.
        Err(e) if bind_conflict(&e) => Err(AlreadyRunning(e).into()),
        Err(e) => Err(anyhow::Error::new(e)
            .context(format!("cannot bind IPC endpoint {name:?}"))),
    }
}

/// Connect to an explicit endpoint name (used by the tests).
fn connect_named(name: &str) -> Result<Conn> {
    let ns: Name<'_> = name
        .to_ns_name::<GenericNamespaced>()
        .with_context(|| format!("invalid IPC endpoint name {name:?}"))?;
    let stream = LocalSocketStream::connect(ns)
        .with_context(|| format!("cannot connect to IPC endpoint {name:?}"))?;
    Ok(Conn { stream })
}

pub struct Listener {
    inner: LocalSocketListener,
}

impl Listener {
    /// Block until a client connects, then return the connection to it.
    pub fn accept(&self) -> Result<Conn> {
        // On Windows named pipes a client that connects and goes away before
        // `accept` runs leaves a dead-on-arrival instance; the server's accept
        // loop is expected to call this continuously (`server::run` does).
        let stream = self.inner.accept().context("accept IPC connection")?;
        Ok(Conn { stream })
    }
}

pub struct Conn {
    stream: LocalSocketStream,
}

impl Conn {
    pub fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let body = serde_json::to_vec(msg).context("serialize IPC frame")?;
        anyhow::ensure!(
            body.len() <= MAX_FRAME,
            "IPC frame too large to send: {} bytes (max {MAX_FRAME})",
            body.len()
        );

        // Prefix and body go out as one write so a frame is never split by two
        // writers sharing a duplicated handle.
        let mut frame = Vec::with_capacity(LEN_PREFIX + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);

        self.stream.write_all(&frame).context("write IPC frame")?;
        self.stream.flush().context("flush IPC frame")?;
        Ok(())
    }

    /// Returns `Ok(None)` on clean EOF.
    pub fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let mut prefix = [0u8; LEN_PREFIX];
        match read_until_eof(&mut self.stream, &mut prefix)? {
            0 => return Ok(None), // peer closed between frames
            LEN_PREFIX => {}
            n => anyhow::bail!(
                "truncated IPC frame: read {n} of {LEN_PREFIX} length bytes before EOF"
            ),
        }
        let len = u32::from_le_bytes(prefix) as usize;
        anyhow::ensure!(
            len <= MAX_FRAME,
            "IPC frame too large: {len} bytes (max {MAX_FRAME})"
        );

        let mut body = vec![0u8; len];
        let n = read_until_eof(&mut self.stream, &mut body)?;
        anyhow::ensure!(
            n == len,
            "truncated IPC frame: read {n} of {len} body bytes before EOF"
        );

        let msg = serde_json::from_slice(&body).context("deserialize IPC frame")?;
        Ok(Some(msg))
    }

    /// Duplicate the handle so one thread can read while another writes.
    pub fn try_clone(&self) -> Result<Conn> {
        let stream = self
            .stream
            .try_clone()
            .context("duplicate IPC connection handle")?;
        Ok(Conn { stream })
    }
}

/// Read until `buf` is full or the peer closes the connection.
///
/// Returns how many bytes were read; a short count can only mean end of
/// stream. A peer closing a Windows named pipe surfaces as
/// [`io::ErrorKind::BrokenPipe`] rather than a zero-length read, so that is
/// folded into "clean EOF" here to keep both platforms behaving alike.
fn read_until_eof(stream: &mut LocalSocketStream, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => break,
            Err(e) => return Err(anyhow::Error::new(e).context("read IPC frame")),
        }
    }
    Ok(filled)
}

/// Bind the listening endpoint.
pub fn listen() -> Result<Listener> {
    listen_named(&pipe_name())
}

/// Connect to an already-running server.
pub fn connect() -> Result<Conn> {
    connect_named(&pipe_name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A payload that is definitely not all-zero, so a mis-framed read shows up.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Ping {
        seq: u32,
        label: String,
        blob: Vec<u8>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Pong {
        seq: u32,
        echo: String,
    }

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    /// A name that no other test, process or real `mk` server will use.
    fn unique_name(tag: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!(
            "{}-test-{tag}-{}-{nanos}-{}",
            pipe_name(),
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn ping(seq: u32) -> Ping {
        Ping {
            seq,
            label: format!("frame-{seq}"),
            blob: (0..=255u8).collect(),
        }
    }

    #[test]
    fn roundtrip_frame() -> Result<()> {
        let name = unique_name("roundtrip");
        // Bind first: `connect` needs the endpoint to exist, and a server
        // thread would race the client here.
        let listener = listen_named(&name)?;

        // Server: accept exactly one client, read one frame, answer it.
        let server = thread::spawn(move || -> Result<()> {
            let mut conn = listener.accept()?;
            let got: Ping = conn.recv()?.expect("client sent a frame");
            assert_eq!(got, ping(7), "server received a different frame");
            conn.send(&Pong {
                seq: got.seq,
                echo: got.label,
            })?;
            Ok(())
        });

        // Client: connect, send, receive the answer.
        let mut conn = connect_named(&name)?;
        conn.send(&ping(7))?;
        let pong: Pong = conn.recv()?.expect("server answered");
        assert_eq!(
            pong,
            Pong {
                seq: 7,
                echo: "frame-7".to_string(),
            }
        );

        server.join().expect("server thread panicked")?;
        Ok(())
    }

    #[test]
    fn large_frame_survives_chunked_pipe_reads() -> Result<()> {
        let name = unique_name("large");
        let listener = listen_named(&name)?;

        // A body far larger than one pipe buffer, so both the write and the
        // read have to loop over partial transfers.
        let big = Ping {
            seq: 99,
            label: "large".to_string(),
            blob: vec![0xAB; 256 * 1024],
        };
        let expect = big.clone();

        let server = thread::spawn(move || -> Result<()> {
            let mut conn = listener.accept()?;
            let got: Ping = conn.recv()?.expect("large frame present");
            assert_eq!(got, expect, "large frame was mangled by chunking");
            Ok(())
        });

        let mut conn = connect_named(&name)?;
        conn.send(&big)?;
        // Hold the connection open until the server has consumed the frame, so
        // this test only exercises chunked transfer, not EOF handling.
        server.join().expect("server thread panicked")?;
        drop(conn);
        Ok(())
    }

    #[test]
    fn try_clone_reads_while_original_writes() -> Result<()> {
        let name = unique_name("clone");
        let listener = listen_named(&name)?;

        let server = thread::spawn(move || -> Result<()> {
            let conn = listener.accept()?;
            // A duplicate handle: one thread blocks reading in it while this
            // thread writes through the original.
            let mut reader = conn.try_clone()?;
            let read = thread::spawn(move || -> Result<Option<Ping>> { reader.recv() });

            let mut writer = conn;
            writer.send(&Pong {
                seq: 11,
                echo: "from clone".to_string(),
            })?;

            let got = read.join().expect("reader thread panicked")?;
            assert_eq!(got.as_ref(), Some(&ping(11)));
            Ok(())
        });

        let mut conn = connect_named(&name)?;
        let pong: Pong = conn.recv()?.expect("server wrote through the original");
        assert_eq!(pong.seq, 11);
        conn.send(&ping(11))?;

        server.join().expect("server thread panicked")?;
        Ok(())
    }

    #[test]
    fn clean_eof_returns_none() -> Result<()> {
        let name = unique_name("eof");
        let listener = listen_named(&name)?;
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        let server = thread::spawn(move || -> Result<()> {
            let mut conn = listener.accept()?;
            let first: Ping = conn.recv()?.expect("first frame present");
            assert_eq!(first, ping(1));
            // The client has closed its side by now: a clean end of stream.
            let after: Option<Ping> = conn.recv()?;
            assert!(after.is_none(), "expected clean EOF, got {after:?}");
            done_tx.send(()).unwrap();
            Ok(())
        });

        let mut conn = connect_named(&name)?;
        conn.send(&ping(1))?;
        // Give the server a moment to pick the frame up, then close.
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(conn);

        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("server did not observe EOF");
        server.join().expect("server thread panicked")?;
        Ok(())
    }

    #[test]
    fn truncated_frame_is_an_error() -> Result<()> {
        let name = unique_name("truncated");
        let listener = listen_named(&name)?;

        let server = thread::spawn(move || -> Result<()> {
            let mut conn = listener.accept()?;
            let result: Result<Option<Ping>> = conn.recv();
            let err = result.expect_err("truncated frame must not decode");
            assert!(
                err.to_string().contains("truncated"),
                "unexpected error: {err:#}"
            );
            Ok(())
        });

        let mut conn = connect_named(&name)?;
        // Claim a 32-byte body, deliver five bytes, then hang up.
        conn.stream.write_all(&32u32.to_le_bytes())?;
        conn.stream.write_all(b"short")?;
        drop(conn);

        server.join().expect("server thread panicked")?;
        Ok(())
    }

    #[test]
    fn oversized_frame_is_rejected() -> Result<()> {
        let name = unique_name("oversized");
        let listener = listen_named(&name)?;

        let server = thread::spawn(move || -> Result<()> {
            let mut conn = listener.accept()?;
            let err = conn
                .recv::<Ping>()
                .expect_err("oversized frame must not be accepted");
            assert!(
                err.to_string().contains("too large"),
                "unexpected error: {err:#}"
            );
            Ok(())
        });

        let mut conn = connect_named(&name)?;
        conn.stream
            .write_all(&((MAX_FRAME as u32) + 1).to_le_bytes())?;
        drop(conn);

        server.join().expect("server thread panicked")?;
        Ok(())
    }

    #[test]
    fn send_rejects_oversized_frame() -> Result<()> {
        let name = unique_name("send-large");
        let listener = listen_named(&name)?;
        let mut conn = connect_named(&name)?;

        // A JSON string just past the limit; nothing is written.
        let too_big = "x".repeat(MAX_FRAME + 1);
        let err = conn
            .send(&too_big)
            .expect_err("oversized send must be refused");
        assert!(
            err.to_string().contains("too large"),
            "unexpected error: {err:#}"
        );

        drop(conn);
        drop(listener);
        Ok(())
    }

    #[test]
    fn handles_are_send() {
        // The server moves `Conn` into a per-client reader thread and keeps a
        // `try_clone()`d writer on the engine thread, and the accept loop moves
        // the `Listener` across threads: all of that needs `Send`.
        fn assert_send<T: Send>() {}
        assert_send::<Conn>();
        assert_send::<Listener>();
    }

    #[test]
    fn second_bind_reports_already_running() -> Result<()> {
        let name = unique_name("conflict");
        let first = listen_named(&name)?;
        let err = match listen_named(&name) {
            Ok(second) => {
                drop(second);
                return Err(anyhow::anyhow!(
                    "binding {name:?} twice must fail while the first listener lives"
                ));
            }
            Err(e) => e,
        };
        assert!(
            already_running(&err),
            "expected an already-running error, got: {err:#}"
        );
        // Not a bogus classification of everything: an unrelated error is not
        // "already running".
        assert!(!already_running(&anyhow::anyhow!("something else")));
        drop(first);
        Ok(())
    }
}