//! Console/TTY handling: raw mode, VT output, size query, resize events.
//!
//! OWNER: teammate `win-pty` (same owner as pty.rs). Only this file.
//!
//! Windows: SetConsoleMode with ENABLE_VIRTUAL_TERMINAL_PROCESSING /
//! ENABLE_VIRTUAL_TERMINAL_INPUT, CONIN$/CONOUT$ handles, alternate screen.
//! Unix: termios raw mode.
//!
//! The client is dumb on purpose: identical byte-level behaviour on both OSes
//! keeps the rest of the program platform-free.
//!
//! Both implementations open the controlling terminal by name rather than
//! trusting stdio (`CONIN$`/`CONOUT$` on Windows, `/dev/tty` on Unix), so the
//! client still gets a raw, VT-capable terminal when its stdio is redirected
//! (pipes, `mk -l`-style subcommands, a detached daemon, ...).

use anyhow::Result;
use std::sync::mpsc::Sender;
use std::time::Duration;

/// Enter the alternate screen, clear it, home the cursor, hide it.
const ALT_ENTER: &[u8] = b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?25l";
/// Show the cursor again and leave the alternate screen (the caller's previous
/// screen contents come back).
const ALT_LEAVE: &[u8] = b"\x1b[?25h\x1b[?1049l";

/// How often the size watcher polls the console.
///
/// Windows has no SIGWINCH, so polling is the portable answer; 200 ms is
/// imperceptible for a resize and costs a couple of microseconds per tick
/// (GetConsoleScreenBufferInfo / TIOCGWINSZ are cheap syscalls).
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Fallback when the console cannot be queried (e.g. it disappeared). The
/// classic 80x24 keeps the protocol happy instead of sending a zero size.
const DEFAULT_SIZE: (u16, u16) = (80, 24);

#[cfg(windows)]
mod imp {
    //! Windows console implementation.
    //!
    //! Notes on the mode bits:
    //! * input: VT input lets ReadFile hand us the same escape sequences a Unix
    //!   pty would. Extended flags must be *on* for QUICK_EDIT_MODE to be
    //!   clearable (without EXTENDED_FLAGS, Windows ignores the QuickEdit bit).
    //!   Line/echo/processed input are off: no line editing, no echo, and
    //!   Ctrl-C arrives as byte 0x03 instead of raising a console event.
    //! * output: VT processing turns our ANSI bytes into screen updates,
    //!   DISABLE_NEWLINE_AUTO_RETURN keeps `\n` from also returning the
    //!   carriage; the client always emits explicit cursor movement.
    //! * both code pages are switched to UTF-8: `WriteFile`/`ReadFile` on a
    //!   console handle transcode with the console code pages, so without this
    //!   the UTF-8 bytes the client writes would be read as CP437/936/... and
    //!   non-ASCII text (CJK, box drawing, emoji) would be mangled.

    use super::{ALT_ENTER, ALT_LEAVE, DEFAULT_SIZE, POLL_INTERVAL, Result};
    use anyhow::Context;
    use std::io;
    use std::sync::mpsc::Sender;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_INVALID_HANDLE,
        ERROR_OPERATION_ABORTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
    };
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
        ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
        ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo, SetConsoleCP,
        SetConsoleMode, SetConsoleOutputCP,
    };

    /// `CP_UTF8` (65001); windows-sys keeps it in the Globalization module,
    /// which this crate does not enable, so the value is spelled out here.
    const CP_UTF8: u32 = 65001;

    /// Input mode bits forced on.
    const INPUT_SET: CONSOLE_MODE = ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_EXTENDED_FLAGS;
    /// Input mode bits forced off.
    const INPUT_CLEAR: CONSOLE_MODE = ENABLE_LINE_INPUT
        | ENABLE_ECHO_INPUT
        | ENABLE_PROCESSED_INPUT
        | ENABLE_QUICK_EDIT_MODE;
    /// Output mode bits forced on.
    const OUTPUT_SET: CONSOLE_MODE = ENABLE_PROCESSED_OUTPUT
        | ENABLE_VIRTUAL_TERMINAL_PROCESSING
        | DISABLE_NEWLINE_AUTO_RETURN;

    /// A raw, VT-capable view of the process console.
    pub struct Tty {
        input: HANDLE,
        output: HANDLE,
        saved_input_mode: CONSOLE_MODE,
        saved_output_mode: CONSOLE_MODE,
        /// Whether the corresponding mode was actually changed; only then must
        /// Drop restore it (and only then is it safe to emit alt-screen
        /// sequences to that handle).
        input_mode_set: bool,
        output_mode_set: bool,
        /// Console code pages to restore (`None` when the query failed, in
        /// which case nothing was changed either).
        saved_input_cp: Option<u32>,
        saved_output_cp: Option<u32>,
    }

    /// `CreateFileW` a console device (`CONIN$` / `CONOUT$`). Opening the
    /// device by name is what makes this work when stdio is redirected; the
    /// share mode lets several clients (and earlier instances) coexist.
    fn open_console(name: &str) -> Result<HANDLE> {
        let mut wide: Vec<u16> = name.encode_utf16().collect();
        wide.push(0); // NUL-terminate the UTF-16 path
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0, // no attributes for an existing device
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("open {name} with CreateFileW (is there a console?)"));
        }
        Ok(handle)
    }

    fn console_mode(handle: HANDLE, what: &str) -> Result<CONSOLE_MODE> {
        let mut mode: CONSOLE_MODE = 0;
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("GetConsoleMode({what}) - handle is not a console"));
        }
        Ok(mode)
    }

    fn set_console_mode(handle: HANDLE, mode: CONSOLE_MODE, what: &str) -> Result<()> {
        if unsafe { SetConsoleMode(handle, mode) } == 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("SetConsoleMode({what})"));
        }
        Ok(())
    }

    /// Raw input mode: VT input on, cooked-input conveniences off, unrelated
    /// bits preserved.
    fn input_mode_for(saved: CONSOLE_MODE) -> CONSOLE_MODE {
        (saved & !INPUT_CLEAR) | INPUT_SET
    }

    /// VT output mode; existing bits (`ENABLE_WRAP_AT_EOL_OUTPUT`, ...) stay.
    fn output_mode_for(saved: CONSOLE_MODE) -> CONSOLE_MODE {
        saved | OUTPUT_SET
    }

    /// Write the whole buffer with `WriteFile`, looping over partial writes
    /// (a full-screen frame can exceed what one console write accepts).
    fn write_bytes(handle: HANDLE, data: &[u8]) -> Result<()> {
        let mut done = 0usize;
        while done < data.len() {
            let rest = &data[done..];
            let len = rest.len().min(u32::MAX as usize) as u32;
            let mut written: u32 = 0;
            let ok =
                unsafe { WriteFile(handle, rest.as_ptr(), len, &mut written, std::ptr::null_mut()) };
            if ok == 0 {
                return Err(io::Error::last_os_error()).with_context(|| {
                    format!("WriteFile(CONOUT$) after {done} of {} bytes", data.len())
                });
            }
            if written == 0 {
                // No forward progress: the console is gone. Don't spin.
                return Err(anyhow::anyhow!(
                    "WriteFile(CONOUT$) accepted 0 of {} bytes (console closed?)",
                    rest.len()
                ));
            }
            done += written as usize;
        }
        Ok(())
    }

    /// Visible window size from a `CONSOLE_SCREEN_BUFFER_INFO`, in cells.
    ///
    /// `srWindow` is inclusive on both sides; the screen buffer itself may be
    /// much taller (scrollback), which is why the window rectangle is used.
    fn size_from_info(info: &CONSOLE_SCREEN_BUFFER_INFO) -> Option<(u16, u16)> {
        let window = info.srWindow;
        let width = i32::from(window.Right) - i32::from(window.Left) + 1;
        let height = i32::from(window.Bottom) - i32::from(window.Top) + 1;
        if width <= 0 || height <= 0 {
            return None;
        }
        Some((width as u16, height as u16))
    }

    fn visible_size(handle: HANDLE) -> Option<(u16, u16)> {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } == 0 {
            return None;
        }
        size_from_info(&info)
    }

    /// Switch both console code pages to UTF-8, remembering the old values.
    ///
    /// Console `ReadFile`/`WriteFile` transcode their buffers using the
    /// console's input/output code pages (that is why plain `WriteFile` on a
    /// CP437/CP936 console mangles UTF-8). muxkit is UTF-8 end to end, so the
    /// Tty owns both code pages for its lifetime and `Drop` puts them back.
    fn use_utf8_code_pages(tty: &mut Tty) {
        let input_cp = unsafe { GetConsoleCP() };
        if input_cp != 0 {
            tty.saved_input_cp = Some(input_cp);
            if input_cp != CP_UTF8 {
                unsafe { SetConsoleCP(CP_UTF8) };
            }
        }
        let output_cp = unsafe { GetConsoleOutputCP() };
        if output_cp != 0 {
            tty.saved_output_cp = Some(output_cp);
            if output_cp != CP_UTF8 {
                unsafe { SetConsoleOutputCP(CP_UTF8) };
            }
        }
    }

    impl Tty {
        pub fn enter() -> Result<Tty> {
            let input = open_console("CONIN$")?;
            let output = match open_console("CONOUT$") {
                Ok(handle) => handle,
                Err(e) => {
                    // Nothing was changed yet; just release the input handle.
                    unsafe { CloseHandle(input) };
                    return Err(e);
                }
            };

            // From here on `tty` owns both handles and restores whatever was
            // changed, even if a later step fails.
            let mut tty = Tty {
                input,
                output,
                saved_input_mode: 0,
                saved_output_mode: 0,
                input_mode_set: false,
                output_mode_set: false,
                saved_input_cp: None,
                saved_output_cp: None,
            };

            tty.saved_input_mode = console_mode(tty.input, "CONIN$")?;
            tty.saved_output_mode = console_mode(tty.output, "CONOUT$")?;

            set_console_mode(tty.input, input_mode_for(tty.saved_input_mode), "CONIN$")?;
            tty.input_mode_set = true;

            set_console_mode(
                tty.output,
                output_mode_for(tty.saved_output_mode),
                "CONOUT$",
            )?;
            tty.output_mode_set = true;

            use_utf8_code_pages(&mut tty);

            // Alternate screen + clear + home + hide cursor, one write.
            tty.write_all(ALT_ENTER)?;
            Ok(tty)
        }

        /// Current visible size in cells. Errors degrade to 80x24 rather than
        /// propagating: a vanished console is handled by `read()` returning 0.
        pub fn size(&self) -> (u16, u16) {
            visible_size(self.output).unwrap_or(DEFAULT_SIZE)
        }

        /// Blocking read of raw input bytes (VT sequences, thanks to
        /// ENABLE_VIRTUAL_TERMINAL_INPUT). Returns 0 at EOF/console teardown.
        pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            loop {
                let len = buf.len().min(u32::MAX as usize) as u32;
                let mut read: u32 = 0;
                let ok = unsafe {
                    ReadFile(
                        self.input,
                        buf.as_mut_ptr(),
                        len,
                        &mut read,
                        std::ptr::null_mut(),
                    )
                };
                if ok != 0 {
                    return Ok(read as usize);
                }
                let err = io::Error::last_os_error();
                // "The console went away" (window closed / process detached):
                // report EOF so the caller's loop ends quietly.
                let console_gone = matches!(
                    err.raw_os_error().map(|code| code as u32),
                    Some(
                        ERROR_HANDLE_EOF
                            | ERROR_BROKEN_PIPE
                            | ERROR_OPERATION_ABORTED
                            | ERROR_INVALID_HANDLE
                    )
                );
                if console_gone {
                    return Ok(0);
                }
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(anyhow::Error::new(err).context("ReadFile(CONIN$)"));
            }
        }

        /// Write already-encoded ANSI straight to the console. Large frames are
        /// allowed: `write_bytes` loops over partial writes.
        pub fn write_all(&mut self, data: &[u8]) -> Result<()> {
            write_bytes(self.output, data)
        }

        /// `WriteFile` is unbuffered; nothing to do.
        pub fn flush(&mut self) {}
    }

    impl Drop for Tty {
        fn drop(&mut self) {
            if self.output_mode_set {
                // Still in VT mode: restore the screen and cursor first.
                let _ = write_bytes(self.output, ALT_LEAVE);
                let _ = unsafe { SetConsoleMode(self.output, self.saved_output_mode) };
            }
            if self.input_mode_set {
                let _ = unsafe { SetConsoleMode(self.input, self.saved_input_mode) };
            }
            // Give the console back its original encodings.
            if let Some(cp) = self.saved_input_cp {
                unsafe { SetConsoleCP(cp) };
            }
            if let Some(cp) = self.saved_output_cp {
                unsafe { SetConsoleOutputCP(cp) };
            }
            unsafe {
                CloseHandle(self.input);
                CloseHandle(self.output);
            }
        }
    }

    /// Poll the visible size and report changes. Exits when the receiver is
    /// dropped (client detached) or the console disappears.
    pub(super) fn spawn_size_watcher(tx: Sender<(u16, u16)>) {
        let _ = std::thread::Builder::new()
            .name("tty-size".into())
            .spawn(move || {
                let output = match open_console("CONOUT$") {
                    Ok(handle) => handle,
                    Err(_) => return, // no console: nothing to watch
                };
                let mut last: Option<(u16, u16)> = None;
                let mut misses = 0u32;
                loop {
                    match visible_size(output) {
                        Some(size) => {
                            misses = 0;
                            if last != Some(size) {
                                last = Some(size);
                                if tx.send(size).is_err() {
                                    break; // receiver gone
                                }
                            }
                        }
                        None => {
                            // Tolerate a couple of transient failures before
                            // declaring the console gone.
                            misses += 1;
                            if misses >= 3 {
                                break;
                            }
                        }
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                unsafe { CloseHandle(output) };
            });
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::System::Console::{ENABLE_WRAP_AT_EOL_OUTPUT, SMALL_RECT};

        fn info(left: i16, top: i16, right: i16, bottom: i16) -> CONSOLE_SCREEN_BUFFER_INFO {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
            info.srWindow = SMALL_RECT {
                Left: left,
                Top: top,
                Right: right,
                Bottom: bottom,
            };
            info
        }

        #[test]
        fn size_comes_from_the_visible_window_not_the_buffer() {
            // 80x24 window at the origin; a tall buffer is irrelevant.
            assert_eq!(size_from_info(&info(0, 0, 79, 23)), Some((80, 24)));
            // A window that is not at the origin (after scrolling).
            assert_eq!(size_from_info(&info(2, 5, 11, 14)), Some((10, 10)));
            // Degenerate rectangles are rejected instead of wrapping.
            assert_eq!(size_from_info(&info(5, 0, 4, 23)), None);
            assert_eq!(size_from_info(&info(0, 9, 79, 8)), None);
        }

        #[test]
        fn input_mode_clears_cooked_bits_and_enables_vt() {
            let cooked = ENABLE_LINE_INPUT
                | ENABLE_ECHO_INPUT
                | ENABLE_PROCESSED_INPUT
                | ENABLE_QUICK_EDIT_MODE;
            let mode = input_mode_for(cooked);
            assert_eq!(mode & INPUT_CLEAR, 0, "cooked input bits must be off");
            assert_ne!(mode & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
            assert_ne!(mode & ENABLE_EXTENDED_FLAGS, 0);
            // Unrelated bits survive the edit.
            assert_eq!(input_mode_for(0x8000) & 0x8000, 0x8000);
        }

        #[test]
        fn output_mode_sets_vt_and_keeps_existing_bits() {
            assert_eq!(output_mode_for(0), OUTPUT_SET);
            assert_eq!(
                OUTPUT_SET,
                ENABLE_PROCESSED_OUTPUT
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                    | DISABLE_NEWLINE_AUTO_RETURN
            );
            let mode = output_mode_for(ENABLE_WRAP_AT_EOL_OUTPUT);
            assert_ne!(mode & ENABLE_WRAP_AT_EOL_OUTPUT, 0);
            assert_ne!(mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING, 0);
        }
    }
}

#[cfg(unix)]
mod imp {
    //! Unix console implementation: termios raw mode on `/dev/tty`, the same
    //! alt-screen bytes as the Windows path, and a polling size watcher (no
    //! SIGWINCH handler: the poll loop is portable and avoids async signal
    //! safety concerns).

    use super::{ALT_ENTER, ALT_LEAVE, DEFAULT_SIZE, POLL_INTERVAL, Result};
    use anyhow::Context;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::sync::mpsc::Sender;

    pub struct Tty {
        tty: File,
        saved: libc::termios,
        /// Whether raw mode is currently applied (and must be reverted).
        raw_active: bool,
    }

    impl Tty {
        pub fn enter() -> Result<Tty> {
            // /dev/tty rather than stdio: raw mode must work even when stdin
            // or stdout is a pipe.
            let tty = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .context("open /dev/tty")?;

            let fd = tty.as_raw_fd();
            let mut saved: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
                return Err(anyhow::Error::new(io::Error::last_os_error())
                    .context("tcgetattr(/dev/tty)"));
            }

            // cfmakeraw equivalent: no line editing, no echo, no signals from
            // keys (Ctrl-C becomes 0x03), no output post-processing, 8-bit
            // chars, and VMIN=1/VTIME=0 so reads block for exactly one byte.
            let mut raw = saved;
            unsafe { libc::cfmakeraw(&mut raw) };
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
                return Err(anyhow::Error::new(io::Error::last_os_error())
                    .context("tcsetattr(/dev/tty, raw)"));
            }

            let mut tty = Tty {
                tty,
                saved,
                raw_active: true,
            };
            tty.write_all(ALT_ENTER)?;
            Ok(tty)
        }

        /// Current visible size in cells; 80x24 if the ioctl fails.
        pub fn size(&self) -> (u16, u16) {
            winsize(self.tty.as_raw_fd()).unwrap_or(DEFAULT_SIZE)
        }

        /// Blocking read of raw input bytes (VT sequences thanks to raw mode).
        /// Returns 0 on EOF / when the terminal goes away.
        pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            loop {
                let n = unsafe {
                    libc::read(
                        self.tty.as_raw_fd(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                    )
                };
                if n >= 0 {
                    return Ok(n as usize);
                }
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // EIO = the controlling terminal was revoked; treat as EOF.
                if err.raw_os_error() == Some(libc::EIO) {
                    return Ok(0);
                }
                return Err(anyhow::Error::new(err).context("read /dev/tty"));
            }
        }

        /// Write already-encoded ANSI to the terminal.
        pub fn write_all(&mut self, data: &[u8]) -> Result<()> {
            self.tty.write_all(data).context("write /dev/tty")
        }

        /// `File` writes are unbuffered; nothing to do.
        pub fn flush(&mut self) {}
    }

    impl Drop for Tty {
        fn drop(&mut self) {
            // Leave the alternate screen while still in raw mode, then give
            // the terminal back its original settings.
            let _ = self.write_all(ALT_LEAVE);
            if self.raw_active {
                let _ = unsafe {
                    libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.saved)
                };
            }
        }
    }

    /// Visible size from TIOCGWINSZ, if the fd is a terminal.
    fn winsize(fd: RawFd) -> Option<(u16, u16)> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 {
            return None;
        }
        if ws.ws_col == 0 || ws.ws_row == 0 {
            return None;
        }
        Some((ws.ws_col, ws.ws_row))
    }

    /// Poll the visible size and report changes; exits when the receiver is
    /// dropped or /dev/tty stops cooperating.
    pub(super) fn spawn_size_watcher(tx: Sender<(u16, u16)>) {
        let _ = std::thread::Builder::new()
            .name("tty-size".into())
            .spawn(move || {
                let file = match OpenOptions::new().read(true).write(true).open("/dev/tty") {
                    Ok(file) => file,
                    Err(_) => return,
                };
                let fd = file.as_raw_fd();
                let mut last: Option<(u16, u16)> = None;
                let mut misses = 0u32;
                loop {
                    match winsize(fd) {
                        Some(size) => {
                            misses = 0;
                            if last != Some(size) {
                                last = Some(size);
                                if tx.send(size).is_err() {
                                    return; // receiver gone
                                }
                            }
                        }
                        None => {
                            // Poll quietly through transient ioctl failures and
                            // only give up once the terminal is really gone.
                            misses += 1;
                            if misses >= 3 {
                                return;
                            }
                        }
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            });
    }
}

pub use imp::Tty;

/// Spawn a thread that polls the console size and sends it on change.
/// Windows has no SIGWINCH, so polling is the portable answer.
pub fn spawn_size_watcher(tx: Sender<(u16, u16)>) {
    imp::spawn_size_watcher(tx);
}