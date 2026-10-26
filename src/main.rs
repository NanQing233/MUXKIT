//! muxkit — a lightweight terminal multiplexer.
//!
//! Rust rewrite of LatosProject/muxkit, native on Windows (ConPTY) and Unix
//! (ptys). Same model: a per-user daemon owns sessions so they survive detach.

use muxkit::{client, server};

use anyhow::Result;

const HELP: &str = "\
mk — lightweight terminal multiplexer

USAGE:
    mk                  start a new session (spawning the server if needed)
    mk -l               list sessions
    mk -s <id>          attach to session <id>
    mk -k <id>          kill session <id>
    mk -S               start the server in the foreground (internal)
    mk -h               show this help

KEYS (prefix = Ctrl+B):
    prefix d   detach        prefix %   split pane      prefix o   next pane
    prefix [   scroll up     prefix ]   scroll down     prefix s   sync input
    prefix q   kill session  Ctrl+B twice sends a literal Ctrl+B
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut session: Option<u32> = None;
    let mut shell: Option<String> = None;
    let mut mode = "attach";

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            "-l" | "--list" => mode = "list",
            "-S" | "--server" => mode = "server",
            "-s" | "--session" => {
                i += 1;
                anyhow::ensure!(i < args.len(), "-s needs a session id");
                session = Some(args[i].parse()?);
            }
            "-k" | "--kill" => {
                i += 1;
                anyhow::ensure!(i < args.len(), "-k needs a session id");
                session = Some(args[i].parse()?);
                mode = "kill";
            }
            "--shell" => {
                i += 1;
                anyhow::ensure!(i < args.len(), "--shell needs a command");
                shell = Some(args[i].clone());
            }
            other => anyhow::bail!("unknown argument: {other} (try -h)"),
        }
        i += 1;
    }

    match mode {
        "server" => server::run(),
        "list" => client::run_list(),
        "kill" => client::run_kill(session.unwrap_or(0)),
        _ => client::run_attach(session, shell),
    }
}