//! Agent sessions: Kuana's UI for coding agents (Claude, Codex, Gemini CLI,
//! GitHub Copilot… over ACP, and Kuana's own agent), see `plans/ai.md`.

pub mod history;
pub mod kuana_agent;
pub mod mcp;
pub mod mentions;
pub mod session;
pub mod sidebar;
pub mod store;
pub mod terminals;
pub mod transcript;
pub mod view;
