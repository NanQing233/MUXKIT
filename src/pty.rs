//! PTY backend: spawn a shell inside a pseudo terminal (ConPTY on Windows,
//! POSIX pty on Unix), pump its output to a callback, forward input, resize.
//!
//! OWNER: teammate `win-pty`. Only this file.
//!
//! `portable-pty` already abstracts ConPTY vs ptys, so this file stays
//! platform-neutral: the same code path drives ConPTY on Windows and
//! `openpty(3)` on Unix.

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::thread;

pub type DataSink = Box<dyn Fn(Vec<u8>) + Send + 'static>;
pub type ExitSink = Box<dyn FnOnce() + Send + 'static>;

/// Size of the buffer handed to every `read` on the master side. ConPTY emits
/// long repaint bursts (a screen of text at once), so this is deliberately
/// larger than a pipe default.
const READ_CHUNK: usize = 32 * 1024;

/// Terminal type advertised to the child. Nothing in muxkit needs a specific
/// terminfo, but full-screen programs (vim, less, ...) refuse to run without
/// one and a 256-colour modern value is what every other multiplexer uses.
const TERM: &str = "xterm-256color";

/// A shell running inside a pty.
///
/// The struct is owned by exactly one thread (the server session, or the test
/// harness); the reader thread keeps its own clone of the master's read half,
/// so dropping `Pty` never blocks on a reader stuck in `read`.
pub struct Pty {
    /// Keeping the master alive is what keeps ConPTY/the pty pair alive; it is
    /// also the resize handle.
    master: Box<dyn MasterPty + Send>,
    /// The only writer; `take_writer()` may be called once.
    writer: Box<dyn Write + Send>,
    /// Sends SIGKILL / TerminateProcess; separate from `child` so it can be
    /// used without blocking on a wait.
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// Kept so the child is not reaped implicitly and so `pid()` can report it.
    child: Box<dyn Child + Send + Sync>,
}

impl Pty {
    /// Spawn `shell` (a command line) with the given initial size.
    ///
    /// The reader thread calls `on_data` for every chunk and `on_exit` once the
    /// child is gone. Both run on the reader thread.
    ///
    /// `shell` is split on whitespace into program + arguments (this is
    /// intentionally simple: `mk --shell "pwsh.exe -NoLogo"`); an empty value
    /// falls back to [`default_shell`]. `-NoLogo` and friends are the caller's
    /// business, not ours.
    pub fn spawn(
        shell: &str,
        cols: u16,
        rows: u16,
        on_data: DataSink,
        on_exit: ExitSink,
    ) -> Result<Pty> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open a pseudo terminal")?;

        let cmd = build_command(shell);
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn {shell:?} inside a pseudo terminal"))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("clone the pty reader")?;
        let writer = pair.master.take_writer().context("take the pty writer")?;
        // Move the master in last: `pair` is then partially moved, and its
        // slave is dropped at the end of the function. On Unix that close is
        // what lets EOF surface on the master once the child exits.
        let master = pair.master;
        // Clone the killer before the reader thread exists so the failure path
        // below can still terminate the already-running child.
        let mut killer = child.clone_killer();

        let reader_thread = thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = vec![0u8; READ_CHUNK];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break, // clean EOF: the slave side is gone
                        Ok(n) => on_data(buf[..n].to_vec()),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        // EIO on Unix / ERROR_BROKEN_PIPE on Windows: the child
                        // exited. Anything else is equally terminal for us; the
                        // session is dead either way and on_exit must fire.
                        Err(_) => break,
                    }
                }
                on_exit();
            });

        match reader_thread {
            // Dropping the handle detaches the thread; it ends on EOF/error.
            Ok(handle) => drop(handle),
            Err(e) => {
                // The child is already running; don't leak it just because we
                // could not start its reader.
                let _ = killer.kill();
                return Err(anyhow::Error::new(e).context("spawn the pty reader thread"));
            }
        }

        Ok(Pty {
            master,
            writer,
            killer,
            child,
        })
    }

    /// Forward user input to the child's stdin-like pty side.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data).context("write to the pty")?;
        // portable-pty's writers are unbuffered in practice, but flushing is
        // cheap and makes the contract explicit for any future implementation.
        self.writer.flush().context("flush the pty writer")?;
        Ok(())
    }

    /// Tell the kernel/ConPTY the new visible size (SIGWINCH-equivalent).
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize the pty")
    }

    /// Terminate the child. Idempotent-ish: an already-dead child makes the
    /// underlying call fail, which is not an error for us.
    pub fn kill(&mut self) {
        let _ = self.killer.kill();
    }

    /// Process id of the child, for status display/debug.
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }
}

/// Build the child command line.
///
/// `portable-pty` wants argv-style input; the CLI gives us a single shell
/// string, so it is split on whitespace. The environment is inherited and
/// then amended with `TERM` (important for Unix; harmless on Windows, where
/// ConPTY children usually ignore it).
fn build_command(shell: &str) -> CommandBuilder {
    let line = if shell.trim().is_empty() {
        default_shell()
    } else {
        shell.to_string()
    };

    let (program, args) = match split_command_line(&line) {
        Some(parts) => parts,
        // Only reachable if `default_shell()` itself is blank; keep going with
        // whatever we have so the caller gets a spawn error instead of a panic.
        None => (default_shell(), Vec::new()),
    };

    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    cmd.env("TERM", TERM);
    cmd
}

/// Split `"pwsh.exe -NoLogo -NoExit"` into `("pwsh.exe", ["-NoLogo", "-NoExit"])`.
/// Returns `None` for a blank line.
fn split_command_line(line: &str) -> Option<(String, Vec<String>)> {
    let mut words = line.split_whitespace();
    let program = words.next()?.to_string();
    Some((program, words.map(str::to_string).collect()))
}

/// The shell to run when the user does not pass one: pwsh/powershell on
/// Windows, $SHELL on Unix.
///
/// pwsh (PowerShell 7) is preferred when it can be found on `PATH`; plain
/// Windows PowerShell always exists on a Windows install and is the fallback.
pub fn default_shell() -> String {
    if cfg!(windows) {
        if let Ok(path) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path) {
                if dir.join("pwsh.exe").is_file() {
                    return "pwsh.exe".to_string();
                }
            }
        }
        "powershell.exe".to_string()
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};

    #[test]
    fn default_shell_is_a_program_name() {
        let shell = default_shell();
        assert!(!shell.trim().is_empty());
        assert!(!shell.contains(char::is_whitespace));
        #[cfg(windows)]
        assert!(
            shell.eq_ignore_ascii_case("pwsh.exe")
                || shell.eq_ignore_ascii_case("powershell.exe"),
            "unexpected Windows default shell: {shell}"
        );
    }

    #[test]
    fn splits_a_command_line() {
        let (program, args) = split_command_line("pwsh.exe -NoLogo -NoExit").expect("split");
        assert_eq!(program, "pwsh.exe");
        assert_eq!(args, vec!["-NoLogo".to_string(), "-NoExit".to_string()]);

        let (program, args) = split_command_line("  pwsh.exe  ").expect("split");
        assert_eq!(program, "pwsh.exe");
        assert!(args.is_empty());

        assert!(split_command_line("").is_none());
        assert!(split_command_line("   \t\n ").is_none());
    }

    #[test]
    fn command_gets_term_and_split_argv() {
        let cmd = build_command("cmd.exe /Q");
        assert_eq!(
            cmd.get_argv()[0],
            OsString::from("cmd.exe"),
            "first argv entry must be the program"
        );
        assert_eq!(cmd.get_argv()[1], OsString::from("/Q"));
        assert_eq!(cmd.get_env("TERM"), Some(OsStr::new(TERM)));

        // An empty shell falls back to the default shell instead of failing.
        let cmd = build_command("   ");
        assert_eq!(
            cmd.get_argv()[0],
            OsString::from(default_shell()),
            "blank shell must fall back to default_shell()"
        );
    }
}