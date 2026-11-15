//! Kuana's GPUI-free core: what both the editor and its remote server
//! (`kuana-server`, see `plans/remote-dev.md`) need. The editor re-exports
//! these modules at its root, so `crate::search` and the like work as before.

pub mod encoding;
pub mod exec;
pub mod fs;
pub mod jsonc;
pub mod paths;
pub mod process;
pub mod remote;
pub mod project;
pub mod project_folders;
pub mod search;
pub mod watcher;
