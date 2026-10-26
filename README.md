# muxkit-rs

**English | [中文](README.zh-CN.md)**

[![CI](https://github.com/NanQing233/MUXKIT/actions/workflows/ci.yml/badge.svg)](https://github.com/NanQing233/MUXKIT/actions/workflows/ci.yml)

Lightweight terminal multiplexer — a Rust rewrite for native Windows support
(ConPTY); also runs on Unix via ptys. Sessions survive detaching: the daemon keeps
the pty and the terminal state, so `mk -s <id>` re-renders the session where you
left it.

**This is a fork.** It is a Rust rewrite of
[muxkit](https://github.com/LatosProject/MUXKIT) (MIT, © 2024 LatosProject; not
the upstream project, not endorsed by it). The main change is the rewrite: Rust,
for native Windows support — upstream is POSIX-only (`posix_openpt`, Unix
sockets, `termios`, `SCM_RIGHTS`).

## What this fork changes

Same user-facing design as upstream — sessions, panes, scrollback, status bar,
`Ctrl+B` prefix keys — with these differences:

**Platform**

| | upstream `muxkit` | this fork |
|---|---|---|
| Windows | not supported (POSIX-only: `posix_openpt`, Unix sockets, `termios`, `SCM_RIGHTS`) | **native**, `ConPTY` + named pipes + console VT modes |
| Unix | primary target | pty / `termios` / Unix domain socket; shares the same code and is built and tested in CI on Ubuntu |
| Language / build | C + CMake, vendored `libvterm` | Rust + Cargo; on Windows only the MSVC toolchain is needed |

**Architecture**

- **The daemon owns all terminal state.** Upstream's client owned the PTY and
  its own grid, and passed file descriptors to the server with `SCM_RIGHTS` —
  that cannot be expressed on Windows, so the fork inverted the model: the server
  parses the VT stream and keeps the grid and scrollback for each pane, and the
  client only draws frames and forwards input bytes. The remaining
  platform-specific code is small and `cfg`-gated in `tty.rs`, `pty.rs`,
  `ipc.rs`, `keys.rs` and `client.rs`; the protocol, terminal state, canvas and
  key handling are shared. This is also what keeps a session's screen intact
  across detach/attach.
- **IPC is a length-prefixed JSON protocol** over a named pipe (Windows) or a
  Unix domain socket (`interprocess`), instead of hand-written messages plus FD
  passing. A frame is capped at 16 MiB; `MUXKIT_NAME` overrides the endpoint name
  (used by the tests).
- **Terminal emulation is `vte` + our own cell grid** instead of vendored
  `libvterm`. Scrollback, SGR (including 256-color and RGB), DECSTBM, the
  alternate screen (`?1049`), wide characters (CJK) and cursor state live in
  [`src/term.rs`](src/term.rs). DA1 / DSR / CPR queries are answered back to the
  child; upstream does not answer them, so a program that waits for such a reply
  can block.
- **Rendering is damage-based.** Upstream repainted the whole pane with one
  `write()` per cell. Measured on this machine for an 80×24 pane: about 1,900
  `write()` calls per frame, and about 7,900 to display one byte of shell output.
  The fork composes panes, borders and the status bar into a canvas, diffs it row
  by row against the previous frame, and each frame leaves the client as **one
  buffered write** (in practice one `WriteFile` on Windows). See
  [`src/canvas.rs`](src/canvas.rs) and [`src/server.rs`](src/server.rs).

**Features and behaviour**

- Implemented: sessions (`new` / `list` / `attach` / `kill`), vertical pane
  splits, scrollback, synchronized input, daemon mode, session persistence across
  detach, `Ctrl+B` prefix, key bindings from a config file.
- Input on Windows uses `ENABLE_VIRTUAL_TERMINAL_INPUT`, so both platforms feed
  the same raw byte stream to the key state machine; there is no separate Windows
  key-event path.
- Window resizes are detected by polling, because Windows has no `SIGWINCH`.
- The config path moved to `%LOCALAPPDATA%\muxkit\keybinds.conf` (Windows) /
  `~/.config/muxkit/keybinds.conf` (Unix); upstream used
  `~/.local/share/muxkit/muxkit-<uid>/keybinds.conf`. Upstream action names are
  still accepted (`new_pane` is an alias for `split_vertical`), so an existing
  `keybinds.conf` keeps working unchanged.
- The daemon does not write `client.log` / `server.log`; it exits by itself after
  3 s idle with no sessions and no clients.
- **Not implemented from upstream:** the English/Chinese UI translation
  (`i18n`) — status-bar text is English only. The other items in upstream's
  feature list are implemented.
- Absent in both versions, so not a regression: mouse support, copy-mode,
  horizontal / `tmux`-style layouts, console reflow.

**Binary and CLI**

- The binary is `mk` (upstream: `muxkit`). `-l`, `-s <id>`, `-k <id>` and `-h`
  match upstream; this fork adds `-S` (run the daemon in the foreground) and
  `--shell <cmd>`.
- Measured on this machine: release binary ≈ 981 KiB, idle daemon ≈ 10 MiB RSS.
  The upstream C implementation, built here with `gcc -O2`, measured 131 KiB and
  ≈ 1.3 MiB. The difference is the Rust runtime, `serde` and the ConPTY
  plumbing; in exchange the VT parser, the grid and the canvas contain no
  `unsafe` code, and Windows is a first-class target.

## Build

```powershell
cargo build --release      # -> target\release\mk.exe
cargo test                 # 67 unit tests + 2 integration tests
```

Windows needs only Rust (MSVC toolchain). ConPTY requires Windows 10 1809+.

To type-check the Unix code paths from Windows (no linker needed):

```powershell
rustup target add x86_64-unknown-linux-gnu
cargo check --all-targets --target x86_64-unknown-linux-gnu
```

## Use

```powershell
mk                 # new session (starts the daemon on first use)
mk -l              # list sessions
mk -s 0            # attach to session 0
mk -k 0            # kill session 0
```

Keys (prefix = `Ctrl+B`):

| key | action |
|---|---|
| `prefix d` | detach |
| `prefix %` | split pane (vertical) |
| `prefix o` | next pane |
| `prefix [` / `]` | scrollback up / down |
| `prefix s` | toggle synchronized input |
| `prefix q` | kill session |
| `Ctrl+B Ctrl+B` | send a literal `Ctrl+B` |

Bindings can be overridden in `%LOCALAPPDATA%\muxkit\keybinds.conf`; see
[`keybinds.conf.example`](keybinds.conf.example):

```
prefix d detach_session
prefix % split_vertical
```

## Architecture

```
   terminal (Windows Terminal / conhost / xterm)
        │  ANSI frames (row deltas only)        ▲ raw input bytes
        ▼                                       │
   ┌─────────────────────── client (mk) ───────┴────────────┐
   │  tty.rs: raw VT mode, alt screen, size polling         │
   │  keys.rs: Ctrl+B state machine                         │
   └───────────────┬────────────────────────────────────────┘
                   │ local IPC: named pipe / Unix domain socket
                   │ length-prefixed JSON frames (proto.rs)
                   ▼
   ┌─────────────────────── server daemon ──────────────────┐
   │  server.rs: single owner of every session              │
   │  term.rs:   VT parser -> cell grid + scrollback        │
   │  canvas.rs: compose panes/borders/status bar, diff     │
   │  pty.rs:    ConPTY / pty child + reader thread         │
   └───────────────┬────────────────────────────────────────┘
                   ▼
              shell (pwsh.exe / bash)
```

Most Windows specifics live in [`src/tty.rs`](src/tty.rs) (console modes +
`ENABLE_VIRTUAL_TERMINAL_*`, so input and output are plain VT byte streams) and
[`src/pty.rs`](src/pty.rs) (portable-pty's ConPTY backend); the rest of the
platform split is the small `cfg`-gated code in `client.rs`, `ipc.rs` and
`keys.rs`.

## Layout

```
src/main.rs      CLI
src/client.rs    attach/detach/list/kill, frame drawing, input forwarding
src/server.rs    daemon engine, sessions, panes, frame pump
src/proto.rs     shared cell model + wire messages  (the contract)
src/term.rs      VT emulation: grid, SGR, scrollback, alt screen, query replies
src/canvas.rs    canvas composition, row diff, ANSI encoder
src/pty.rs       PTY/ConPTY child management
src/tty.rs       raw console, VT modes, size, resize watcher
src/keys.rs      prefix keybindings + config file
src/ipc.rs       named pipe / UDS transport + framing
src/layout.rs    pane rectangles
tests/e2e.rs     daemon protocol, detach/reattach persistence
tests/client_e2e.rs  the real client inside a real ConPTY
```

## Verification

```
cargo test          # 67 unit tests + 2 integration tests, all green on Windows
```

CI runs the same suite on `windows-latest` (full, including the real-ConPTY
client test) and on `ubuntu-latest` (build, unit tests and the daemon end-to-end
test) on every push.

- [`tests/e2e.rs`](tests/e2e.rs) — real daemon over real IPC: attach → `echo` →
  detach → reattach, asserting the marker text is still on screen after
  reattaching (session persistence), then a clean shutdown.
- [`tests/client_e2e.rs`](tests/client_e2e.rs) — the real `mk` client running
  inside a real **ConPTY**, with its ANSI output parsed back by `term.rs`: asserts
  that the status bar is painted, typed input reaches the shell, output comes
  back, `Ctrl+B %` opens a second pane, `Ctrl+B o` moves focus, and `Ctrl+B d`
  detaches while the daemon keeps the session.
- Unit tests cover the VT parser (SGR / erase / scrollback / wide characters /
  alternate screen, CPR + DA1 replies), the canvas diff and ANSI encoder (one
  escape per run, not per cell), the prefix state machine, config parsing and the
  IPC framing.

A screen dump captured from that ConPTY run (PowerShell 7 inside two panes):

```
PowerShell 7.6.8                        │
PS C:\Users\me> echo CLIENT_SIDE_OK     │
CLIENT_SIDE_OK                          │
PS C:\Users\me>                         │
 mk | session 0 | pane 1/2 | 2 panes | C:\Program Files\WindowsApps\Microsoft.Po
```

## Status / known limits

- Unix support shares the whole codebase and is built and tested in CI on an
  Ubuntu runner on every push: unit tests plus the daemon
  attach/echo/detach/reattach test. The *client* itself has still only been
  exercised on Windows, because a hosted Linux runner has no controlling
  terminal for `/dev/tty`.
- Vertical splits only (same as upstream); no mouse, no copy-mode pager.
- Reflow on resize is truncate/pad, not word reflow.
- One attached client per session (a new attach detaches the old one), like tmux.
- OSC 52 clipboard and hyperlinks are parsed but ignored.

## Licence

MIT. Original `muxkit` © 2024 [LatosProject](https://github.com/LatosProject);
this fork © 2026 南清 (NanQing). Both notices are kept in [LICENSE](LICENSE); no
upstream source code is included.

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)