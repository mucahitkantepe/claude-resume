//! Where Claude Code keeps its data, and where claude-resume keeps its own files.

use std::path::{Path, PathBuf};

/// Locations used by claude-resume. Built once from the environment and passed around, so tests
/// can point everything at a temporary directory without touching process-wide state.
#[derive(Clone, Debug)]
pub struct Paths {
    /// Claude Code's config directory: `$CLAUDE_CONFIG_DIR`, or `~/.claude`.
    pub claude_dir: PathBuf,
    /// The search index (`$CLAUDE_RESUME_DB`, or `<claude_dir>/claude-resume.db`).
    pub db: PathBuf,
    /// Hugging Face cache for the embedding model (`$CLAUDE_RESUME_MODELS_DIR`, or `<claude_dir>/models`).
    pub models: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let claude_dir = env_path("CLAUDE_CONFIG_DIR")
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"));
        Self {
            db: env_path("CLAUDE_RESUME_DB").unwrap_or_else(|| claude_dir.join("claude-resume.db")),
            models: env_path("CLAUDE_RESUME_MODELS_DIR")
                .unwrap_or_else(|| claude_dir.join("models")),
            claude_dir,
        }
    }

    /// Everything relative to one Claude config directory (used by tests).
    pub fn under(claude_dir: &Path) -> Self {
        Self {
            claude_dir: claude_dir.to_path_buf(),
            db: claude_dir.join("claude-resume.db"),
            models: claude_dir.join("models"),
        }
    }

    pub fn projects(&self) -> PathBuf {
        self.claude_dir.join("projects")
    }

    pub fn settings(&self) -> PathBuf {
        self.claude_dir.join("settings.json")
    }

    /// The index file written by claude-resume <= 0.4 (different schema, removed by `uninstall`).
    pub fn legacy_db(&self) -> PathBuf {
        self.claude_dir.join("recall.db")
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}
