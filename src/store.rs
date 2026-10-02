//! The search index: one SQLite row per session plus an FTS5 trigram index over its text.
//!
//! Syncing only reads transcripts; it never modifies or deletes anything under `projects/`.

use crate::text;
use crate::transcript::{self, Transcript};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Bump whenever the schema or the parser's output changes; existing indexes are then rebuilt.
pub const INDEX_VERSION: i32 = 1;

/// Prompts kept for previews: the first few, then the last few.
const PREVIEW_HEAD: usize = 8;
const PREVIEW_TAIL: usize = 3;
const PREVIEW_CHARS: usize = 200;

const SCHEMA: &str = "
CREATE TABLE sessions (
    id            INTEGER PRIMARY KEY,
    sid           TEXT NOT NULL UNIQUE,
    path          TEXT NOT NULL,
    file_size     INTEGER NOT NULL,
    file_mtime_ns INTEGER NOT NULL,
    created       INTEGER NOT NULL,
    updated       INTEGER NOT NULL,
    cwd           TEXT NOT NULL,
    project       TEXT NOT NULL,
    branch        TEXT NOT NULL,
    title         TEXT NOT NULL,
    prompt_count  INTEGER NOT NULL,
    turn_count    INTEGER NOT NULL,
    preview       TEXT NOT NULL,
    pr_links      TEXT NOT NULL
);
CREATE INDEX sessions_updated ON sessions(updated DESC);

-- Files with no conversation in them (a lone /clear, an aborted start). Remembered so they are
-- not re-parsed on every sync; never deleted.
CREATE TABLE skipped (
    sid           TEXT PRIMARY KEY,
    path          TEXT NOT NULL,
    file_size     INTEGER NOT NULL,
    file_mtime_ns INTEGER NOT NULL
);

-- Searchable text, kept out of `sessions` so listing sessions never reads it.
CREATE TABLE content (
    id      INTEGER PRIMARY KEY,
    meta    TEXT NOT NULL,
    prompts TEXT NOT NULL,
    replies TEXT NOT NULL,
    tools   TEXT NOT NULL
);
CREATE VIRTUAL TABLE content_fts USING fts5(
    meta, prompts, replies, tools,
    content = 'content', content_rowid = 'id',
    tokenize = 'trigram'
);
CREATE TRIGGER content_ai AFTER INSERT ON content BEGIN
    INSERT INTO content_fts(rowid, meta, prompts, replies, tools)
    VALUES (new.id, new.meta, new.prompts, new.replies, new.tools);
END;
CREATE TRIGGER content_ad AFTER DELETE ON content BEGIN
    INSERT INTO content_fts(content_fts, rowid, meta, prompts, replies, tools)
    VALUES ('delete', old.id, old.meta, old.prompts, old.replies, old.tools);
END;
";

/// Embeddings outlive re-indexing: they are keyed by session id and invalidated by a hash of the
/// text they were computed from, not dropped whenever a session changes.
const EMBEDDINGS: &str = "
CREATE TABLE IF NOT EXISTS embeddings (
    sid       TEXT PRIMARY KEY,
    model     TEXT NOT NULL,
    text_hash TEXT NOT NULL,
    vector    BLOB NOT NULL
);
";

/// A session as listed and displayed (no searchable text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRow {
    #[serde(skip)]
    pub id: i64,
    pub sid: String,
    pub path: String,
    pub created: i64,
    pub updated: i64,
    pub cwd: String,
    pub project: String,
    pub branch: String,
    pub title: String,
    pub prompt_count: u32,
    pub turn_count: u32,
    /// One-line prompts: up to `PREVIEW_HEAD` from the start, then up to `PREVIEW_TAIL` from the end.
    #[serde(skip)]
    pub preview: Vec<String>,
    pub pr_links: Vec<String>,
}

/// The searchable text of one session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Content {
    pub meta: String,
    pub prompts: String,
    pub replies: String,
    pub tools: String,
}

/// Progress of a sync, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Another process is syncing the index; this one waits for it to finish.
    Waiting,
    /// `done` of the `total` new or changed files are parsed.
    Parsed { done: usize, total: usize },
}

/// A sync commits this often, so an interrupted first index keeps what it finished, the WAL
/// stays small, and other writers (embedding) only ever wait briefly.
const COMMIT_FILES: usize = 100;
const COMMIT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncStats {
    /// Sessions in the index after the sync.
    pub sessions: usize,
    /// Files parsed because they were new or changed.
    pub parsed: usize,
    /// Files skipped because they had not changed.
    pub unchanged: usize,
    /// Index entries dropped because their file is gone.
    pub removed: usize,
    /// Parsed files that hold no conversation.
    pub empty: usize,
    /// Files that could not be read (the previous index entry, if any, is kept).
    pub failed: Vec<(PathBuf, String)>,
}

/// A transcript file found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub sid: String,
    pub path: PathBuf,
    pub size: i64,
    pub mtime_ns: i64,
}

impl FileInfo {
    fn mtime_secs(&self) -> i64 {
        self.mtime_ns / 1_000_000_000
    }

    fn key(&self) -> (String, i64, i64) {
        (
            self.path.to_string_lossy().into_owned(),
            self.size,
            self.mtime_ns,
        )
    }
}

/// Session transcripts directly inside each project directory. Subagent transcripts
/// (`<session>/subagents/*.jsonl`) and backups (`*.jsonl.bak`) are not sessions.
pub fn discover(projects: &Path) -> io::Result<Vec<FileInfo>> {
    let entries = match fs::read_dir(projects) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut by_sid: HashMap<String, FileInfo> = HashMap::new();
    for project in entries.flatten() {
        if !project.path().is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(sid) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
            let info = FileInfo {
                sid: sid.clone(),
                path,
                size: meta.len() as i64,
                mtime_ns,
            };
            // The same id in two project directories: keep the most recently written copy.
            match by_sid.get(&sid) {
                Some(existing) if existing.mtime_ns >= info.mtime_ns => {}
                _ => {
                    by_sid.insert(sid, info);
                }
            }
        }
    }
    let mut files: Vec<FileInfo> = by_sid.into_values().collect();
    files.sort_by(|a, b| a.sid.cmp(&b.sid));
    Ok(files)
}

pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        create_private(path).with_context(|| format!("creating {}", path.display()))?;
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Two processes opening a brand-new index at once (say, the picker in one terminal and a
        // search in another) both try to switch it to WAL; SQLite fails one of them immediately
        // with SQLITE_BUSY instead of waiting, to avoid a lock-upgrade deadlock. WAL is
        // persistent, so check first and retry.
        retry_busy(|| {
            let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))?;
            }
            Ok(())
        })?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let mut store = Self {
            conn,
            path: path.to_path_buf(),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create or upgrade the schema. The write lock is only taken when there is work to do, so
    /// opening the index never queues behind a sync running in another process.
    fn migrate(&mut self) -> rusqlite::Result<()> {
        if self.schema_is_current()? {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != INDEX_VERSION {
            tx.execute_batch(
                "DROP TABLE IF EXISTS content_fts; DROP TABLE IF EXISTS content;
                 DROP TABLE IF EXISTS skipped; DROP TABLE IF EXISTS sessions;",
            )?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", INDEX_VERSION)?;
        }
        tx.execute_batch(EMBEDDINGS)?;
        tx.commit()
    }

    fn schema_is_current(&self) -> rusqlite::Result<bool> {
        let version: i32 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        let embeddings: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'embeddings')",
            [],
            |r| r.get(0),
        )?;
        Ok(version == INDEX_VERSION && embeddings)
    }

    /// Bring the index up to date with the transcripts under `projects`.
    pub fn sync(&mut self, projects: &Path, force: bool) -> Result<SyncStats> {
        self.sync_with_progress(projects, force, |_| {})
    }

    /// `sync`, reporting progress as it goes.
    pub fn sync_with_progress(
        &mut self,
        projects: &Path,
        force: bool,
        mut progress: impl FnMut(Progress),
    ) -> Result<SyncStats> {
        let lock = self.path.with_extension("lock");
        let _lock = match FileLock::try_acquire(&lock)? {
            Some(held) => held,
            None => {
                progress(Progress::Waiting);
                FileLock::acquire(&lock)?
            }
        };
        let files =
            discover(projects).with_context(|| format!("reading {}", projects.display()))?;
        let known = self.known_files()?;
        let todo: Vec<&FileInfo> = files
            .iter()
            .filter(|f| force || known.get(&f.sid) != Some(&f.key()))
            .collect();
        let live: HashSet<&str> = files.iter().map(|f| f.sid.as_str()).collect();

        let mut stats = SyncStats {
            unchanged: files.len() - todo.len(),
            ..SyncStats::default()
        };
        let conn = &self.conn;
        let begin = || Transaction::new_unchecked(conn, TransactionBehavior::Immediate);
        let mut tx = Some(begin()?);
        for sid in known.keys().filter(|sid| !live.contains(sid.as_str())) {
            remove(tx.as_ref().expect("open"), sid)?;
            stats.removed += 1;
        }
        let total = todo.len();
        let (mut done, mut pending, mut since) = (0, 0, Instant::now());
        parse_parallel(&todo, |file, parsed| {
            done += 1;
            let open = tx.as_ref().expect("open");
            match parsed {
                Ok(t) if t.is_empty() => {
                    remove(open, &file.sid)?;
                    let (path, size, mtime) = file.key();
                    open.execute(
                        "INSERT INTO skipped (sid, path, file_size, file_mtime_ns) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(sid) DO UPDATE SET path = excluded.path,
                             file_size = excluded.file_size, file_mtime_ns = excluded.file_mtime_ns",
                        params![file.sid, path, size, mtime],
                    )?;
                    stats.empty += 1;
                }
                Ok(t) => {
                    write_session(open, file, &t)?;
                    stats.parsed += 1;
                }
                Err(e) => stats.failed.push((file.path.clone(), e.to_string())),
            }
            pending += 1;
            if pending >= COMMIT_FILES || since.elapsed() >= COMMIT_INTERVAL {
                tx.take().expect("open").commit()?;
                tx = Some(begin()?);
                (pending, since) = (0, Instant::now());
            }
            progress(Progress::Parsed { done, total });
            Ok(())
        })?;
        tx.take().expect("open").commit()?;
        stats.sessions = self.len()?;
        Ok(stats)
    }

    /// `(path, size, mtime)` of every file the index knows about, indexed or skipped.
    fn known_files(&self) -> Result<HashMap<String, (String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT sid, path, file_size, file_mtime_ns FROM sessions
             UNION ALL SELECT sid, path, file_size, file_mtime_ns FROM skipped",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn len(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0))?
            as usize)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// All sessions, most recently active first.
    pub fn sessions(&self) -> Result<Vec<SessionRow>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{SESSION_COLUMNS} ORDER BY updated DESC, id DESC"))?;
        let rows = stmt.query_map([], session_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Look a session up by its full id, or by a unique prefix of it (at least 4 characters).
    pub fn find(&self, sid: &str) -> Result<Option<SessionRow>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{SESSION_COLUMNS} WHERE sid = ?1"))?;
        if let Some(row) = stmt.query_row([sid], session_row).optional()? {
            return Ok(Some(row));
        }
        if sid.len() < 4 {
            return Ok(None);
        }
        let mut stmt = self.conn.prepare(&format!(
            "{SESSION_COLUMNS} WHERE substr(sid, 1, ?2) = ?1 LIMIT 2"
        ))?;
        let rows: Vec<SessionRow> = stmt
            .query_map(params![sid, sid.len() as i64], session_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(if rows.len() == 1 {
            rows.into_iter().next()
        } else {
            None
        })
    }

    pub fn content(&self, id: i64) -> Result<Option<Content>> {
        Ok(self
            .conn
            .query_row(
                "SELECT meta, prompts, replies, tools FROM content WHERE id = ?1",
                [id],
                |r| {
                    Ok(Content {
                        meta: r.get(0)?,
                        prompts: r.get(1)?,
                        replies: r.get(2)?,
                        tools: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Ids of sessions whose text matches an FTS5 query expression.
    pub fn fts_ids(&self, expr: &str) -> Result<HashSet<i64>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT rowid FROM content_fts WHERE content_fts MATCH ?1")?;
        let rows = stmt.query_map([expr], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `(sid, title, start of the prompts, stored model, stored text hash)` for every session.
    #[allow(clippy::type_complexity)]
    pub fn embedding_sources(
        &self,
        prompt_chars: usize,
    ) -> Result<Vec<(String, String, String, Option<String>, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.sid, s.title, substr(c.prompts, 1, ?1), e.model, e.text_hash
             FROM sessions s JOIN content c ON c.id = s.id LEFT JOIN embeddings e ON e.sid = s.sid
             ORDER BY s.updated DESC",
        )?;
        let rows = stmt.query_map([prompt_chars as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn put_embedding(
        &self,
        sid: &str,
        model: &str,
        text_hash: &str,
        vector: &[u8],
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO embeddings (sid, model, text_hash, vector) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(sid) DO UPDATE SET model = excluded.model, text_hash = excluded.text_hash,
                 vector = excluded.vector",
            params![sid, model, text_hash, vector],
        )?;
        Ok(())
    }

    /// Whether some indexed session has no vector computed by `model`: it is new, or was embedded
    /// by another model.
    pub fn missing_embeddings(&self, model: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sessions s LEFT JOIN embeddings e ON e.sid = s.sid
                            WHERE e.model IS NOT ?1)",
            [model],
            |r| r.get(0),
        )?)
    }

    /// Stored vectors for `model`, for sessions that are still indexed.
    pub fn embeddings(&self, model: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.sid, e.vector FROM embeddings e JOIN sessions s ON s.sid = e.sid WHERE e.model = ?1",
        )?;
        let rows = stmt.query_map([model], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

const SESSION_COLUMNS: &str = "SELECT id, sid, path, created, updated, cwd, project, branch, title,
    prompt_count, turn_count, preview, pr_links FROM sessions";

fn session_row(r: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    let preview: String = r.get(11)?;
    let pr_links: String = r.get(12)?;
    Ok(SessionRow {
        id: r.get(0)?,
        sid: r.get(1)?,
        path: r.get(2)?,
        created: r.get(3)?,
        updated: r.get(4)?,
        cwd: r.get(5)?,
        project: r.get(6)?,
        branch: r.get(7)?,
        title: r.get(8)?,
        prompt_count: r.get(9)?,
        turn_count: r.get(10)?,
        preview: serde_json::from_str(&preview).unwrap_or_default(),
        pr_links: pr_links.lines().map(str::to_string).collect(),
    })
}

fn write_session(tx: &Transaction, file: &FileInfo, t: &Transcript) -> Result<()> {
    let created = t.first_ts.unwrap_or_else(|| file.mtime_secs());
    let updated = t.last_ts.unwrap_or_else(|| file.mtime_secs()).max(created);
    let title = t.title();
    let project = t.project();
    let preview = serde_json::to_string(&preview(t))?;
    let (path, size, mtime) = file.key();
    tx.execute(
        "INSERT INTO sessions (sid, path, file_size, file_mtime_ns, created, updated, cwd, project,
             branch, title, prompt_count, turn_count, preview, pr_links)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(sid) DO UPDATE SET path = excluded.path, file_size = excluded.file_size,
             file_mtime_ns = excluded.file_mtime_ns, created = excluded.created,
             updated = excluded.updated, cwd = excluded.cwd, project = excluded.project,
             branch = excluded.branch, title = excluded.title, prompt_count = excluded.prompt_count,
             turn_count = excluded.turn_count, preview = excluded.preview, pr_links = excluded.pr_links",
        params![
            file.sid,
            path,
            size,
            mtime,
            created,
            updated,
            t.cwd,
            project,
            t.branch,
            title,
            t.prompts.len() as i64,
            t.assistant_turns as i64,
            preview,
            t.pr_links.join("\n"),
        ],
    )?;
    let id: i64 = tx.query_row("SELECT id FROM sessions WHERE sid = ?1", [&file.sid], |r| {
        r.get(0)
    })?;
    let meta = [
        Some(title.as_str()),
        t.custom_title.as_deref(),
        t.ai_title.as_deref(),
        Some(project.as_str()),
        Some(t.branch.as_str()),
        Some(t.cwd.as_str()),
    ]
    .into_iter()
    .flatten()
    .chain(t.pr_links.iter().map(String::as_str))
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join("\n");
    let prompts = t
        .prompts
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    tx.execute("DELETE FROM content WHERE id = ?1", [id])?;
    tx.execute(
        "INSERT INTO content (id, meta, prompts, replies, tools) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id,
            meta,
            prompts,
            t.replies.join("\n\n"),
            t.tools.join("\n")
        ],
    )?;
    tx.execute("DELETE FROM skipped WHERE sid = ?1", [&file.sid])?;
    Ok(())
}

fn preview(t: &Transcript) -> Vec<String> {
    let line = |p: &transcript::Prompt| text::truncate(&text::one_line(&p.text), PREVIEW_CHARS);
    let n = t.prompts.len();
    if n <= PREVIEW_HEAD + PREVIEW_TAIL {
        return t.prompts.iter().map(line).collect();
    }
    t.prompts[..PREVIEW_HEAD]
        .iter()
        .chain(&t.prompts[n - PREVIEW_TAIL..])
        .map(line)
        .collect()
}

/// Number of preview lines taken from the start of a session (the rest come from its end).
pub fn preview_head() -> usize {
    PREVIEW_HEAD
}

fn remove(tx: &Transaction, sid: &str) -> Result<()> {
    if let Some(id) = tx
        .query_row("SELECT id FROM sessions WHERE sid = ?1", [sid], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?
    {
        tx.execute("DELETE FROM content WHERE id = ?1", [id])?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
    }
    tx.execute("DELETE FROM skipped WHERE sid = ?1", [sid])?;
    tx.execute("DELETE FROM embeddings WHERE sid = ?1", [sid])?;
    Ok(())
}

/// Parse files on worker threads; `on_parsed` runs on the calling thread, in completion order.
fn parse_parallel(
    files: &[&FileInfo],
    mut on_parsed: impl FnMut(&FileInfo, io::Result<Transcript>) -> Result<()>,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(files.len());
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::sync_channel(workers * 2);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let sender = sender.clone();
            let next = &next;
            scope.spawn(move || {
                while let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                    if sender
                        .send((*file, transcript::parse_file(&file.path)))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(sender);
        for (file, parsed) in receiver {
            on_parsed(file, parsed)?;
        }
        Ok(())
    })
}

const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `f` again while SQLite reports the database busy without having waited for it.
fn retry_busy<T>(mut f: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let deadline = std::time::Instant::now() + BUSY_TIMEOUT;
    loop {
        match f() {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => return other,
        }
    }
}

/// An exclusive lock across processes, held until dropped. Syncs take one so that two Claude
/// sessions starting at once don't index the same files twice. The OS releases it if the
/// process dies, so a crash never leaves the index locked.
pub struct FileLock(#[allow(dead_code)] File);

impl FileLock {
    /// Wait until the lock is free, then take it.
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = Self::open(path)?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(Self(file))
    }

    /// Take the lock if nobody holds it.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let file = Self::open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self(file))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("locking {}", path.display()))
            }
        }
    }

    fn open(path: &Path) -> Result<File> {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))
    }
}

/// Create `path` readable only by the owner (the index holds conversation text).
fn create_private(path: &Path) -> io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        opts.mode(0o600);
        opts.open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        opts.open(path).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Prompt;

    fn prompts(n: usize) -> Transcript {
        Transcript {
            prompts: (0..n)
                .map(|i| Prompt {
                    text: format!("prompt {i}"),
                    labelable: true,
                })
                .collect(),
            ..Transcript::default()
        }
    }

    #[test]
    fn preview_keeps_head_and_tail() {
        assert_eq!(preview(&prompts(3)), ["prompt 0", "prompt 1", "prompt 2"]);
        let p = preview(&prompts(30));
        assert_eq!(p.len(), PREVIEW_HEAD + PREVIEW_TAIL);
        assert_eq!(p[PREVIEW_HEAD - 1], "prompt 7");
        assert_eq!(p[PREVIEW_HEAD], "prompt 27");
        assert_eq!(p.last().unwrap(), "prompt 29");
    }

    #[test]
    fn discover_ignores_subagents_backups_and_stray_files() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("-home-me-proj");
        fs::create_dir_all(proj.join("abc/subagents")).unwrap();
        fs::write(proj.join("abc.jsonl"), "{}").unwrap();
        fs::write(proj.join("abc/subagents/agent-1.jsonl"), "{}").unwrap();
        fs::write(proj.join("old.jsonl.bak"), "{}").unwrap();
        fs::write(proj.join("notes.txt"), "x").unwrap();
        fs::write(dir.path().join("top-level.jsonl"), "{}").unwrap();
        let found: Vec<String> = discover(dir.path())
            .unwrap()
            .into_iter()
            .map(|f| f.sid)
            .collect();
        assert_eq!(found, ["abc"]);
    }

    #[test]
    fn discover_missing_dir_is_empty_not_an_error() {
        assert!(
            discover(Path::new("/definitely/not/here"))
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn index_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("i.db");
        Store::open(&db).unwrap();
        assert_eq!(
            fs::metadata(&db).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn schema_version_mismatch_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("i.db");
        {
            let conn = Connection::open(&db).unwrap();
            // An index written by claude-resume 0.4 (different schema, user_version 0).
            conn.execute_batch("CREATE TABLE sessions (sid TEXT PRIMARY KEY, label TEXT); INSERT INTO sessions VALUES ('x', 'old');").unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(store.len().unwrap(), 0);
        let version: i32 = store
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, INDEX_VERSION);
    }
}
