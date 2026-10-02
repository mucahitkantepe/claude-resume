//! claude-resume: find, search and resume Claude Code sessions.
//!
//! The binary is a thin CLI over this library so the integration tests can drive the same
//! code paths (sync, search, TUI state, settings patching) against fixture data.

pub mod output;
pub mod paths;
pub mod resume;
pub mod search;
pub mod semantic;
pub mod settings;
pub mod store;
pub mod text;
pub mod time;
pub mod transcript;
pub mod tui;
