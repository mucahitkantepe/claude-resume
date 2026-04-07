use crate::parser::{parse_session, SessionMeta};
use rusqlite::{params, Connection};
use std::fs;
use std::path::{Path, PathBuf};

pub fn db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".claude")
        .join("recall.db")
}

pub fn claude_projects_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".claude")
        .join("projects")
}

/// An indexed session entry.
#[derive(Clone)]
pub struct IndexEntry {
    pub sid: String,
    pub created: String,
    pub modified: String,
    pub mtime_epoch: u64,
    pub msg_count: u32,
    pub label: String,
    pub branch: String,
    pub project: String,
    pub cwd: String,
    pub search_text: String,
}

impl IndexEntry {
    pub fn from_meta(meta: &SessionMeta) -> Self {
        IndexEntry {
            sid: meta.sid.clone(),
            created: meta.created.clone(),
            modified: meta.modified.clone(),
            mtime_epoch: meta.mtime_epoch,
            msg_count: meta.msg_count,
            label: meta.label.clone(),
            branch: meta.branch.clone(),
            project: meta.project.clone(),
            cwd: meta.cwd.clone(),
            search_text: meta.search_text.clone(),
        }
    }
}

/// Initialize the database schema.
fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sessions (
            sid TEXT PRIMARY KEY,
            created TEXT,
            modified TEXT,
            mtime_epoch INTEGER,
            msg_count INTEGER,
            label TEXT,
            branch TEXT,
            project TEXT,
            cwd TEXT,
            search_text TEXT
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS sessions_fts USING fts5(
            sid UNINDEXED,
            label,
            project,
            branch,
            search_text,
            content='sessions',
            content_rowid='rowid',
            tokenize='unicode61 remove_diacritics 2'
        );
        -- Triggers to keep FTS in sync
        CREATE TRIGGER IF NOT EXISTS sessions_ai AFTER INSERT ON sessions BEGIN
            INSERT INTO sessions_fts(rowid, sid, label, project, branch, search_text)
            VALUES (new.rowid, new.sid, new.label, new.project, new.branch, new.search_text);
        END;
        CREATE TRIGGER IF NOT EXISTS sessions_ad AFTER DELETE ON sessions BEGIN
            INSERT INTO sessions_fts(sessions_fts, rowid, sid, label, project, branch, search_text)
            VALUES ('delete', old.rowid, old.sid, old.label, old.project, old.branch, old.search_text);
        END;
        CREATE TRIGGER IF NOT EXISTS sessions_au AFTER UPDATE ON sessions BEGIN
            INSERT INTO sessions_fts(sessions_fts, rowid, sid, label, project, branch, search_text)
            VALUES ('delete', old.rowid, old.sid, old.label, old.project, old.branch, old.search_text);
            INSERT INTO sessions_fts(rowid, sid, label, project, branch, search_text)
            VALUES (new.rowid, new.sid, new.label, new.project, new.branch, new.search_text);
        END;
        ",
    )?;
    Ok(())
}

/// Discover all .jsonl session files.
fn discover_sessions(projects_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(projects_dir) {
        for entry in entries.flatten() {
            let project_dir = entry.path();
            if !project_dir.is_dir() {
                continue;
            }
            if let Ok(sessions) = fs::read_dir(&project_dir) {
                for session in sessions.flatten() {
                    let path = session.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                        && !path
                            .file_name()
                            .unwrap_or_default()
                            .to_str()
                            .unwrap_or("")
                            .ends_with(".bak")
                    {
                        files.push(path);
                    }
                }
            }
        }
    }
    files
}

/// Build or update the search index.
pub fn sync(force: bool) {
    let db = db_path();
    let conn = Connection::open(&db).expect("Failed to open database");

    // Restrict database file permissions (owner-only read/write)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&db, fs::Permissions::from_mode(0o600));
    }

    init_db(&conn).expect("Failed to initialize database");

    // Enable WAL mode for better concurrent reads
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .ok();

    if force {
        conn.execute("DELETE FROM sessions", []).ok();
    }

    // Load existing mtime_epoch by sid
    let mut existing: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    if !force {
        let mut stmt = conn
            .prepare("SELECT sid, mtime_epoch FROM sessions")
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })
            .unwrap();
        for row in rows.flatten() {
            existing.insert(row.0, row.1);
        }
    }

    let session_files = discover_sessions(&claude_projects_dir());
    // Track which sids still exist on disk
    let mut live_sids: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Wrap all mutations in a transaction for atomicity + performance
    conn.execute("BEGIN", []).ok();

    for path in &session_files {
        let sid = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        live_sids.insert(sid.clone());

        let current_mtime = match fs::metadata(path) {
            Ok(m) => m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
            Err(_) => continue,
        };

        // Skip if unchanged
        if let Some(&cached_mtime) = existing.get(&sid) {
            if cached_mtime >= current_mtime {
                continue;
            }
        }

        // Parse the session
        match parse_session(path) {
            Some(meta) => {
                let entry = IndexEntry::from_meta(&meta);
                conn.execute(
                    "INSERT OR REPLACE INTO sessions (sid, created, modified, mtime_epoch, msg_count, label, branch, project, cwd, search_text)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        entry.sid,
                        entry.created,
                        entry.modified,
                        entry.mtime_epoch as i64,
                        entry.msg_count as i64,
                        entry.label,
                        entry.branch,
                        entry.project,
                        entry.cwd,
                        entry.search_text,
                    ],
                )
                .ok();
            }
            None => {
                // Unparseable or empty session — skip it, never delete source files.
                // A parser bug or format change must not become data loss.
            }
        }
    }

    // Remove entries for sessions that no longer exist on disk
    if let Ok(mut stmt) = conn.prepare("SELECT sid FROM sessions") {
        let db_sids: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap_or_else(|_| panic!("query failed"))
            .flatten()
            .collect();
        for sid in &db_sids {
            if !live_sids.contains(sid) {
                conn.execute("DELETE FROM sessions WHERE sid = ?1", params![sid])
                    .ok();
            }
        }
    }

    conn.execute("COMMIT", []).ok();
}

/// Load all entries from the database, sorted by modified descending.
pub fn load_index() -> Vec<IndexEntry> {
    let db = db_path();
    let conn = match Connection::open(&db) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT sid, created, modified, mtime_epoch, msg_count, label, branch, project, cwd, search_text
         FROM sessions ORDER BY modified DESC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    stmt.query_map([], |row| {
        Ok(IndexEntry {
            sid: row.get(0)?,
            created: row.get(1)?,
            modified: row.get(2)?,
            mtime_epoch: row.get::<_, i64>(3)? as u64,
            msg_count: row.get::<_, i64>(4)? as u32,
            label: row.get(5)?,
            branch: row.get(6)?,
            project: row.get(7)?,
            cwd: row.get(8)?,
            search_text: row.get(9)?,
        })
    })
    .unwrap()
    .flatten()
    .collect()
}

/// Search entries using FTS5 for content + fuzzy on short fields via nucleo.
pub fn search_entries(entries: &[IndexEntry], query: &str) -> Vec<IndexEntry> {
    use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let q_lower = query.to_lowercase();
    let query_chars: Vec<(usize, char)> = query.char_indices().collect();
    let min_fuzzy_score = (query_chars.len() as u32) * 16;

    const BONUS_SHORT: u32 = 1000;
    const BONUS_CONTENT: u32 = 500;
    const BONUS_NEAR: u32 = 400;

    let near_variants: Vec<String> = if query_chars.len() >= 5 {
        query_chars
            .iter()
            .map(|&(byte_idx, ch)| {
                let mut v = query.to_string();
                v.replace_range(byte_idx..byte_idx + ch.len_utf8(), "");
                v.to_lowercase()
            })
            .collect()
    } else {
        Vec::new()
    };

    let mut scored: Vec<(&IndexEntry, u32)> = entries
        .iter()
        .filter_map(|e| {
            let short = format!("{} {} {}", e.label, e.project, e.branch);
            let mut buf = Vec::new();
            let haystack = Utf32Str::new(&short, &mut buf);
            if let Some(score) = pattern.score(haystack, &mut matcher) {
                if score >= min_fuzzy_score {
                    return Some((e, score + BONUS_SHORT));
                }
            }
            let content_lower = e.search_text.to_lowercase();
            if content_lower.contains(&q_lower) {
                return Some((e, BONUS_CONTENT));
            }
            for variant in &near_variants {
                if content_lower.contains(variant.as_str()) {
                    return Some((e, BONUS_NEAR));
                }
            }
            None
        })
        .collect();

    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.into_iter().map(|(e, _)| e.clone()).collect()
}

/// Snap a byte index to the nearest valid char boundary (backward).
fn snap_left(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Snap a byte index to the nearest valid char boundary (forward).
fn snap_right(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    let mut i = idx;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Extract match contexts around a query.
pub fn match_contexts(entry: &IndexEntry, query: &str, max: usize) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }
    let lower = entry.search_text.to_lowercase();
    let q_lower = query.to_lowercase();
    let mut contexts = Vec::new();
    let mut start = 0;

    while contexts.len() < max {
        let idx = match lower[start..].find(&q_lower) {
            Some(i) => start + i,
            None => break,
        };
        let s = snap_left(&entry.search_text, idx.saturating_sub(120));
        let e = snap_right(&entry.search_text, idx + query.len() + 120);
        let snippet: String = entry.search_text[s..e]
            .replace('\n', " ")
            .replace('\t', " ");
        let prefix = if s > 0 { "…" } else { "" };
        let suffix = if e < entry.search_text.len() {
            "…"
        } else {
            ""
        };
        contexts.push(format!("{}{}{}", prefix, snippet.trim(), suffix));
        start = idx + query.len();
    }
    contexts
}

/// Find the .jsonl file path for a session ID.
pub fn find_session_file(sid: &str) -> Option<PathBuf> {
    // Reject path traversal attempts
    if sid.contains('/') || sid.contains('\\') || sid.contains("..") {
        return None;
    }
    let projects_dir = claude_projects_dir();
    if let Ok(entries) = fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let path = entry.path().join(format!("{}.jsonl", sid));
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

/// Get match contexts with near-match + file scan fallback.
pub fn match_contexts_deep(entry: &IndexEntry, query: &str, max: usize) -> Vec<String> {
    // Try exact match on indexed text
    let mut contexts = match_contexts(entry, query, max);

    // If no exact match, try near-match variants on indexed text only (fast)
    let chars: Vec<(usize, char)> = query.char_indices().collect();
    if contexts.is_empty() && chars.len() >= 5 {
        for &(byte_idx, ch) in &chars {
            let mut variant = query.to_string();
            variant.replace_range(byte_idx..byte_idx + ch.len_utf8(), "");
            contexts = match_contexts(entry, &variant, max);
            if !contexts.is_empty() {
                break;
            }
        }
    }

    contexts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str, project: &str, branch: &str, text: &str) -> IndexEntry {
        IndexEntry {
            sid: "test".into(), created: "2025-01-01".into(), modified: "2025-01-02".into(),
            mtime_epoch: 0, msg_count: 5, label: label.into(), branch: branch.into(),
            project: project.into(), cwd: "/tmp".into(), search_text: text.into(),
        }
    }

    #[test]
    fn snap_left_ascii() {
        assert_eq!(snap_left("hello", 3), 3);
        assert_eq!(snap_left("hello", 0), 0);
        assert_eq!(snap_left("hello", 100), 5);
    }

    #[test]
    fn snap_left_multibyte() {
        let s = "héllo"; // é is 2 bytes
        assert_eq!(snap_left(s, 2), 1); // inside é → snaps back
        assert_eq!(snap_left(s, 1), 1); // start of é
    }

    #[test]
    fn snap_right_multibyte() {
        let s = "héllo";
        assert_eq!(snap_right(s, 2), 3); // inside é → snaps forward
    }

    #[test]
    fn contexts_exact() {
        let e = entry("t", "p", "", "the quick brown fox jumps over");
        let c = match_contexts(&e, "brown fox", 3);
        assert_eq!(c.len(), 1);
        assert!(c[0].contains("brown fox"));
    }

    #[test]
    fn contexts_case_insensitive() {
        let e = entry("t", "p", "", "Hello World");
        assert_eq!(match_contexts(&e, "hello", 3).len(), 1);
    }

    #[test]
    fn contexts_empty_query() {
        assert!(match_contexts(&entry("t", "p", "", "text"), "", 3).is_empty());
    }

    #[test]
    fn contexts_respects_max() {
        let e = entry("t", "p", "", "aaa bbb aaa bbb aaa");
        assert_eq!(match_contexts(&e, "aaa", 2).len(), 2);
    }

    #[test]
    fn deep_exact_first() {
        let e = entry("t", "p", "", "karabasan game dev");
        let c = match_contexts_deep(&e, "karabasan", 3);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn deep_near_match() {
        let e = entry("t", "p", "", "karabasan game dev");
        // "karabasn" — dropping 'n' yields "karabas", substring of "karabasan"
        assert!(!match_contexts_deep(&e, "karabasn", 3).is_empty());
    }

    #[test]
    fn unicode_near_match_no_panic() {
        let e = entry("t", "p", "", "über cool stuff");
        let _ = match_contexts_deep(&e, "übërf", 3);
    }

    #[test]
    fn emoji_near_match_no_panic() {
        let e = entry("t", "p", "", "hello 🎮 world");
        let _ = match_contexts_deep(&e, "🎮world", 3);
    }

    #[test]
    fn search_exact_content() {
        let entries = vec![
            entry("unrelated", "other", "", "nothing"),
            entry("session", "proj", "", "implemented karabasan engine"),
        ];
        let r = search_entries(&entries, "karabasan");
        assert_eq!(r.len(), 1);
        assert!(r[0].search_text.contains("karabasan"));
    }

    #[test]
    fn search_fuzzy_label() {
        let entries = vec![entry("karabasan dev", "game", "", "content")];
        assert_eq!(search_entries(&entries, "karabasan").len(), 1);
    }

    #[test]
    fn search_no_match() {
        let entries = vec![entry("hello", "test", "", "nothing")];
        assert!(search_entries(&entries, "karabasan").is_empty());
    }

    #[test]
    fn search_near_match() {
        let entries = vec![entry("s", "p", "", "implemented karabasan engine")];
        assert!(!search_entries(&entries, "karabasn").is_empty());
    }

    #[test]
    fn path_traversal_rejected() {
        assert!(find_session_file("../../../etc/passwd").is_none());
        assert!(find_session_file("foo/bar").is_none());
        assert!(find_session_file("foo\\bar").is_none());
    }
}
