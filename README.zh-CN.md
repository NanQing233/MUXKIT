# muxkit-rs

**[English](README.md) | 中文**

[![CI](https://github.com/NanQing233/MUXKIT/actions/workflows/ci.yml/badge.svg)](https://github.com/NanQing233/MUXKIT/actions/workflows/ci.yml)

轻量级终端复用器 —— 使用 Rust 重写以原生适配 Windows（ConPTY），在 Unix 上通过 pty
运行。会话在 detach 后继续运行：daemon 持有 pty 与终端状态，所以 `mk -s <id>` 能在你
离开的地方重新渲染会话。

**这是一个 fork。** 本项目是 [muxkit](https://github.com/LatosProject/MUXKIT) 的 Rust
重写（MIT，© 2024 LatosProject；不是上游项目，也未获其背书）。主要变更就是这次重写：
使用 Rust 以原生适配 Windows —— 上游只支持 POSIX（`posix_openpt`、Unix socket、
`termios`、`SCM_RIGHTS`）。

## 本 fork 改了什么

用户可见的设计与上游一致——会话、分屏、回滚、状态栏、`Ctrl+B` 前缀键——差异如下。

**平台**

| | 上游 `muxkit` | 本 fork |
|---|---|---|
| Windows | 不支持（仅 POSIX：`posix_openpt`、Unix socket、`termios`、`SCM_RIGHTS`） | **原生**，`ConPTY` + 命名管道 + 控制台 VT 模式 |
| Unix | 主要目标 | pty / `termios` / Unix domain socket；共享同一套代码，CI 在 Ubuntu 上构建并测试 |
| 语言 / 构建 | C + CMake，内置 `libvterm` | Rust + Cargo；在 Windows 上只需 MSVC 工具链 |

**架构**

- **终端状态由 daemon 持有。** 上游是 client 持有 PTY 和自己的网格，再用 `SCM_RIGHTS`
  把文件描述符传给 server —— 这在 Windows 上无法表达，所以本 fork 反转了模型：server
  解析 VT 字节流、保存每个 pane 的网格与回滚，client 只负责画帧和转发输入字节。余下的
  平台相关代码很少，且以 `cfg` 集中在 `tty.rs`、`pty.rs`、`ipc.rs`、`keys.rs`、
  `client.rs` 中；协议、终端状态、画布与按键处理是共享的。这也是 detach/attach 后会话
  屏幕能保持原样的原因。
- **IPC 采用长度前缀的 JSON 协议**，跑在命名管道（Windows）或 Unix domain socket
  （`interprocess`）上，替代手写消息 + FD 传递。单帧上限 16 MiB；可用 `MUXKIT_NAME`
  覆盖端点名（测试在用）。
- **终端仿真采用 `vte` + 自研格子**，不再内置 `libvterm`。回滚、SGR（含 256 色与
  RGB）、DECSTBM、备用屏（`?1049`）、宽字符（中日韩）与光标状态都在
  [`src/term.rs`](src/term.rs) 中实现。DA1 / DSR / CPR 查询会回包子进程；上游不回，
  因此等待这类回包的程序可能阻塞。
- **渲染改为脏区差分。** 上游每格一次 `write()` 重画整个 pane。在本机 80×24 pane 上
  实测：约每帧 1,900 次 `write()`，显示一个字节的 shell 输出约 7,900 次。本 fork 把各
  pane、边框、状态栏合成一张画布，与上一帧逐行比对，每帧只留下**一次带缓冲的写入**
  （Windows 上实际是一次 `WriteFile`）。见 [`src/canvas.rs`](src/canvas.rs) 与
  [`src/server.rs`](src/server.rs)。

**功能与行为**

- 已实现：会话（新建 / 列表 / attach / kill）、垂直分屏、回滚、同步输入、daemon 模式、
  detach 后会话常驻、`Ctrl+B` 前缀、从配置文件读键位。
- Windows 的输入走 `ENABLE_VIRTUAL_TERMINAL_INPUT`，两个平台喂给按键状态机的都是同一份
  原始字节流；不存在单独的 Windows 按键事件分支。
- 窗口尺寸变化用轮询检测，因为 Windows 没有 `SIGWINCH`。
- 配置路径改为 `%LOCALAPPDATA%\muxkit\keybinds.conf`（Windows）/
  `~/.config/muxkit/keybinds.conf`（Unix）；上游是
  `~/.local/share/muxkit/muxkit-<uid>/keybinds.conf`。上游的动作名仍然可用
  （`new_pane` 是 `split_vertical` 的别名），已有的 `keybinds.conf` 可原样继续使用。
- daemon 不写 `client.log` / `server.log`；没有会话也没有客户端时，空闲 3 秒自动退出。
- **上游功能中未实现的一项：** 中英文界面翻译（`i18n`）—— 状态栏只有英文。上游功能
  列表中的其余项均已实现。
- 两版都没有、不属于回退：鼠标支持、copy-mode、水平 / `tmux` 风格布局、窗口重排。

**二进制与命令行**

- 二进制名为 `mk`（上游是 `muxkit`）。`-l`、`-s <id>`、`-k <id>`、`-h` 与上游一致；
  本 fork 另有 `-S`（前台运行 daemon）与 `--shell <cmd>`。
- 本机实测：release 二进制约 981 KiB，空闲 daemon 约 10 MiB RSS；上游 C 实现用
  `gcc -O2` 在本机编译后为 131 KiB、约 1.3 MiB。差异来自 Rust 运行时、`serde` 与
  ConPTY 管线；换来的是 VT 解析器、格子与画布中不含 `unsafe` 代码，以及把 Windows
  作为一等目标。

## 构建

```powershell
cargo build --release      # -> target\release\mk.exe
cargo test                 # 67 个单测 + 2 个集成测试
```

Windows 上只需要 Rust（MSVC 工具链）。ConPTY 需要 Windows 10 1809+。

在 Windows 上对 Unix 分支做类型检查（不需要链接器）：

```powershell
rustup target add x86_64-unknown-linux-gnu
cargo check --all-targets --target x86_64-unknown-linux-gnu
```

## 使用

```powershell
mk                 # 新建会话（首次会自动拉起 daemon）
mk -l              # 列出会话
mk -s 0            # 接回会话 0
mk -k 0            # 杀掉会话 0
```

按键（前缀 = `Ctrl+B`）：

| 按键 | 动作 |
|---|---|
| `prefix d` | detach（会话继续在后台跑） |
| `prefix %` | 垂直分屏 |
| `prefix o` | 切换到下一个 pane |
| `prefix [` / `]` | 回滚向上 / 向下 |
| `prefix s` | 开关同步输入 |
| `prefix q` | 杀掉会话 |
| `Ctrl+B Ctrl+B` | 发送一个真正的 `Ctrl+B` |

键位可在 `%LOCALAPPDATA%\muxkit\keybinds.conf` 覆盖，参考
[`keybinds.conf.example`](keybinds.conf.example)：

```
prefix d detach_session
prefix % split_vertical
```

## 架构

```
   终端（Windows Terminal / conhost / xterm）
        │  ANSI 帧（只发变化的行）              ▲ 原始输入字节
        ▼                                       │
   ┌───────────────────── 客户端（mk）─────────┴────────────┐
   │  tty.rs: 控制台 raw 模式、备用屏、尺寸轮询              │
   │  keys.rs: Ctrl+B 状态机                                │
   └───────────────┬────────────────────────────────────────┘
                   │ 本地 IPC：命名管道 / Unix domain socket
                   │ 长度前缀 JSON 帧（proto.rs）
                   ▼
   ┌──────────────────── daemon ────────────────────────────┐
   │  server.rs: 所有会话的唯一持有者                        │
   │  term.rs:   VT 解析 -> 格子 + 回滚                      │
   │  canvas.rs: 合成 pane/边框/状态栏并差分                 │
   │  pty.rs:    ConPTY / pty 子进程 + 读线程                │
   └───────────────┬────────────────────────────────────────┘
                   ▼
              shell（pwsh.exe / bash）
```

Windows 相关的实现主要在两个文件：[`src/tty.rs`](src/tty.rs)（控制台模式 +
`ENABLE_VIRTUAL_TERMINAL_*`，让输入输出都是纯 VT 字节流）和
[`src/pty.rs`](src/pty.rs)（portable-pty 的 ConPTY 后端）；其余平台差异是
`client.rs`、`ipc.rs`、`keys.rs` 中少量 `cfg` 分支。

## 目录结构

```
src/main.rs      命令行入口
src/client.rs    attach/detach/list/kill、画帧、转发输入
src/server.rs    daemon 引擎、会话、pane、帧泵
src/proto.rs     共享的格子模型 + 协议消息（契约）
src/term.rs      VT 仿真：格子、SGR、回滚、备用屏、查询回包
src/canvas.rs    画布合成、行差分、ANSI 编码
src/pty.rs       PTY/ConPTY 子进程管理
src/tty.rs       控制台 raw 模式、VT 模式、尺寸、resize 监听
src/keys.rs      前缀键位 + 配置文件
src/ipc.rs       命名管道 / UDS 传输 + 帧封装
src/layout.rs    pane 矩形计算
tests/e2e.rs     daemon 协议、detach/reattach 持久化
tests/client_e2e.rs  真客户端跑在真 ConPTY 里
```

## 验证

```
cargo test          # 67 个单测 + 2 个集成测试，Windows 上全绿
```

CI 每次 push 都会在 `windows-latest`（全套，含真实 ConPTY 的 client 测试）和
`ubuntu-latest`（构建 + 单测 + daemon 端到端测试）上跑同一套用例。

- [`tests/e2e.rs`](tests/e2e.rs) —— 真 daemon + 真 IPC：attach → `echo` → detach →
  reattach，断言标记文本在重新接回后仍在屏幕上（会话持久化），最后干净退出。
- [`tests/client_e2e.rs`](tests/client_e2e.rs) —— 真的 `mk` 客户端跑在真的 **ConPTY**
  里，它输出的 ANSI 再由 `term.rs` 解析回来断言：状态栏已绘制、键入的输入到达 shell、
  输出能够返回、`Ctrl+B %` 开出第二个 pane、`Ctrl+B o` 切换焦点、`Ctrl+B d` detach
  之后 daemon 仍保留会话。
- 单测覆盖 VT 解析（SGR / 擦除 / 回滚 / 宽字符 / 备用屏、CPR + DA1 回包）、画布差分与
  ANSI 编码（每个 run 一个转义，而非每格一个）、前缀状态机、配置解析和 IPC 帧封装。

上面那个 ConPTY 测试跑出来的真实屏幕快照（两个 pane 里的 PowerShell 7）：

```
PowerShell 7.6.8                        │
PS C:\Users\me> echo CLIENT_SIDE_OK     │
CLIENT_SIDE_OK                          │
PS C:\Users\me>                         │
 mk | session 0 | pane 1/2 | 2 panes | C:\Program Files\WindowsApps\Microsoft.Po
```

## 状态与已知限制

- Unix 分支与 Windows 共享全部代码，CI 每次 push 都会在 Ubuntu runner 上构建并测试：
  单测 + daemon 的 attach/echo/detach/reattach 测试。**client 本身**仍只在 Windows 上
  跑过，因为托管 Linux runner 没有 `/dev/tty` 所需的控制终端。
- 只支持垂直分屏（与上游一致）；没有鼠标，没有 copy-mode 翻页。
- resize 时是截断 / 补白，不做按词重排。
- 一个会话同时只能挂一个客户端（新的 attach 会把旧的踢掉），与 tmux 一致。
- OSC 52 剪贴板和超链接会解析但被忽略。

## 许可证

MIT。原 `muxkit` © 2024 [LatosProject](https://github.com/LatosProject)；本 fork © 2026
南清。两段声明都保留在 [LICENSE](LICENSE) 中；仓库不包含上游源代码。

---

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)