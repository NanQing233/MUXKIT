//! muxkit library crate: everything except the CLI lives here so that the
//! end-to-end test can drive the real protocol against a real server.

pub mod canvas;
pub mod client;
pub mod ipc;
pub mod keys;
pub mod layout;
pub mod proto;
pub mod pty;
pub mod server;
pub mod term;
pub mod tty;