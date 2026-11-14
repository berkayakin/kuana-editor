//! Remote development (`plans/remote-dev.md`): the protocol between Kuana
//! and `kuana-server` (`rpc`), the server's side (`server`) and Kuana's
//! (`client::RemoteHost`, mounted with `client::mount` so `crate::fs` and
//! `crate::exec` reach it).

pub mod client;
pub mod connection;
pub mod container;
#[cfg(unix)]
mod pty;
pub mod rpc;
pub mod server;
pub mod ssh;
pub mod wsl;
#[cfg(test)]
mod tests;

pub use client::{Info, RemoteHost, mount, unmount};
pub use connection::{Connected, Transport};
