use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use claude_resume::paths::Paths;
use claude_resume::search::{Mode, Searcher};
use claude_resume::settings::{self, Retention, Settings};
use claude_resume::store::{Progress, Store, SyncStats};
use claude_resume::{output, resume, semantic, tui};
use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Write};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "claude-resume",
    version,
    about = "Find, search and resume Claude Code sessions"
)]
struct Cli {
    /// Rebuild the index from scratch first
    #[arg(short, long)]
    force: bool,

    /// Use the index as it is, without syncing it first (or set CLAUDE_RESUME_NO_SYNC=1)
    #[arg(long, global = true)]
    no_sync: bool,

    /// Search mode the picker starts in
    #[arg(short, long, value_enum, default_value_t = Mode::Fuzzy)]
    mode: Mode,

    /// Initial query for the picker
    query: Vec<String>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Search sessions and print the matches
    Search {
        /// Words to find (all must match; "quote phrases", -exclude)
        #[arg(required = true)]
        query: Vec<String>,
        /// Maximum results to show
        #[arg(short = 'n', long = "max", default_value_t = 10)]
        max: usize,
        #[arg(short, long, value_enum, default_value_t = Mode::Fuzzy)]
        mode: Mode,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Resume a session (by id or unique id prefix) in the directory it was started in
    Resume {
        id: String,
        /// Print the command instead of running it
        #[arg(long)]
        print: bool,
    },
    /// Update the index (every other command does this first)
    Sync {
        /// Rebuild the index from scratch
        #[arg(short, long)]
        force: bool,
        /// Print nothing
        #[arg(short, long)]
        quiet: bool,
    },
    /// Set up semantic search: download its model (after asking) and embed every session
    Embed {
        /// Download the model without asking
        #[arg(short, long)]
        yes: bool,
        /// How other commands start it after they sync: it never downloads, gives way to an
        /// embed already running, and reports to claude-resume.log
        #[arg(long, hide = true)]
        background: bool,
    },
    /// Stop Claude Code from deleting old sessions (sets cleanupPeriodDays in settings.json)
    Init,
    /// Remove the index, the downloaded model and what older versions added (sessions are kept)
    Uninstall,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let paths = Paths::from_env();
    match run(cli, &paths) {
        Ok(code) => code,
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("claude-resume: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli, paths: &Paths) -> Result<ExitCode> {
    let no_sync =
        cli.no_sync || std::env::var_os("CLAUDE_RESUME_NO_SYNC").is_some_and(|v| !v.is_empty());
    match cli.command {
        None => picker(paths, cli.force, no_sync, cli.mode, &cli.query.join(" ")),
        Some(Cmd::Search {
            query,
            max,
            mode,
            json,
        }) => {
            let mut store = Store::open(&paths.db)?;
            if !no_sync {
                let stats = sync(&mut store, paths, false, true)?;
                update_embeddings(&store, paths, &stats, mode == Mode::Semantic)?;
            }
            let query = query.join(" ");
            let searcher = Searcher::new(&store, paths.models.clone())?;
            let hits = searcher.search(&query, mode)?;
            let mut out = io::stdout().lock();
            if json {
                output::write_json(&mut out, &searcher, &query, &hits, max)?;
            } else {
                output::write_text(&mut out, &searcher, &query, &hits, max)?;
            }
            out.flush()?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Resume { id, print }) => {
            let mut store = Store::open(&paths.db)?;
            if !no_sync {
                let stats = sync(&mut store, paths, false, true)?;
                update_embeddings(&store, paths, &stats, false)?;
            }
            let session = store
                .find(&id)?
                .with_context(|| format!("no session matches id {id:?}"))?;
            launch(&session.sid, &session.cwd, print)
        }
        Some(Cmd::Sync { force, quiet }) => {
            let mut store = Store::open(&paths.db)?;
            let stats = sync(&mut store, paths, force, quiet)?;
            update_embeddings(&store, paths, &stats, false)?;
            Ok(if stats.failed.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Some(Cmd::Embed { yes, background }) => {
            if !cfg!(feature = "semantic") {
                bail!(
                    "this build of claude-resume has no semantic search (built without the `semantic` feature)"
                );
            }
            let mut store = Store::open(&paths.db)?;
            if background {
                // The command that started it has just synced.
                return Ok(match embed_stale(&store, paths, Embedding::Background) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        log(&format!("claude-resume: {e:#}"));
                        ExitCode::FAILURE
                    }
                });
            }
            if !no_sync {
                sync(&mut store, paths, false, false)?;
            }
            if !semantic::model_cached(&paths.models) && !yes && !confirm_download()? {
                eprintln!("Cancelled.");
                return Ok(ExitCode::SUCCESS);
            }
            embed_stale(&store, paths, Embedding::Command)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Init) => init(paths),
        Some(Cmd::Uninstall) => uninstall(paths),
    }
}

fn picker(paths: &Paths, force: bool, no_sync: bool, mode: Mode, query: &str) -> Result<ExitCode> {
    let mut store = Store::open(&paths.db)?;
    if !no_sync || force {
        let stats = sync(&mut store, paths, force, false)?;
        let searching = mode == Mode::Semantic && !query.trim().is_empty();
        update_embeddings(&store, paths, &stats, searching)?;
    }
    if store.is_empty()? {
        bail!("no sessions found in {}", paths.projects().display());
    }
    let searcher = Searcher::new(&store, paths.models.clone())?;
    match tui::run(searcher, mode, query)? {
        Some(session) => launch(&session.sid, &session.cwd, false),
        None => Ok(ExitCode::SUCCESS),
    }
}

fn launch(sid: &str, cwd: &str, print: bool) -> Result<ExitCode> {
    let plan = resume::plan(sid, cwd)?;
    if print {
        println!("{}", plan.command_line());
        return Ok(ExitCode::SUCCESS);
    }
    match (&plan.dir, &plan.missing_dir) {
        (Some(dir), _) => eprintln!("Resuming {sid} in {}", dir.display()),
        (None, Some(gone)) => eprintln!("{gone} no longer exists; resuming {sid} from here"),
        (None, None) => eprintln!("Resuming {sid}"),
    }
    let status = plan.run().context("starting claude")?;
    Ok(ExitCode::from(
        status.code().unwrap_or(1).clamp(0, 255) as u8
    ))
}

/// Incremental sync. Progress goes to stderr, and only when it is a terminal.
fn sync(store: &mut Store, paths: &Paths, force: bool, quiet: bool) -> Result<SyncStats> {
    let started = Instant::now();
    let interactive = !quiet && io::stderr().is_terminal();
    let mut shown = false;
    let stats = store.sync_with_progress(&paths.projects(), force, |progress| match progress {
        Progress::Waiting if interactive => {
            eprint!("\rWaiting for another claude-resume to finish indexing…");
            shown = true;
        }
        Progress::Parsed { done, total }
            if interactive && total >= 50 && (done % 25 == 0 || done == total) =>
        {
            eprint!("\r\x1b[2KIndexing sessions… {done}/{total}");
            shown = true;
        }
        _ => {}
    })?;
    if shown {
        eprint!("\r\x1b[2K");
    }
    for (path, err) in &stats.failed {
        eprintln!("claude-resume: could not read {}: {err}", path.display());
    }
    if !quiet && (stats.parsed + stats.removed > 0 || force) {
        eprintln!(
            "Indexed {} sessions ({} updated, {} unchanged, {} removed) in {:.1}s",
            stats.sessions,
            stats.parsed,
            stats.unchanged,
            stats.removed,
            started.elapsed().as_secs_f64()
        );
    }
    Ok(stats)
}

fn confirm_download() -> Result<bool> {
    eprintln!(
        "Semantic search needs the {} embedding model ({}).",
        semantic::MODEL_REPO,
        semantic::MODEL_SIZE
    );
    eprintln!("It is downloaded once from huggingface.co and then runs fully offline.");
    if !io::stdin().is_terminal() {
        eprintln!("Run `claude-resume embed --yes` to download it non-interactively.");
        return Ok(false);
    }
    eprint!("Download it now? [y/N] ");
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes"))
}

/// Keep semantic search up to date after a sync, once `embed` has downloaded the model. A
/// semantic search is about to load the model anyway, so it embeds what changed first and sees
/// the newest sessions; everything else leaves that to a detached `embed --background`, so nobody
/// waits for the model.
fn update_embeddings(
    store: &Store,
    paths: &Paths,
    stats: &SyncStats,
    semantic_search: bool,
) -> Result<()> {
    if !semantic::model_cached(&paths.models) {
        return Ok(());
    }
    if semantic_search {
        return embed_stale(store, paths, Embedding::BeforeSearch);
    }
    // Cheaper than `semantic::stale`, which compares every session's text with what its vector
    // was computed from: there is nothing to do if this sync changed no session and every
    // session has a vector.
    if stats.parsed == stats.empty && !store.missing_embeddings(semantic::MODEL_KEY)? {
        return Ok(());
    }
    start_background_embed(paths);
    Ok(())
}

/// `embed_in_background`, reporting rather than failing: the command itself can go on without it.
fn start_background_embed(paths: &Paths) -> bool {
    match embed_in_background(paths) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("claude-resume: could not start embedding new sessions: {e:#}");
            false
        }
    }
}

/// `claude-resume.log` only ever holds what background embeds report; past this size it starts
/// over.
const LOG_LIMIT: u64 = 1 << 20;

/// Start `claude-resume embed --background`, detached: in its own process group, so neither
/// Ctrl-C nor closing the terminal stops it, and holding none of this process's stdio, so a
/// caller reading the output (`$(claude-resume search …)`) is not kept waiting for it.
fn embed_in_background(paths: &Paths) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let log_file = options.open(paths.db.with_extension("log"))?;
    if log_file.metadata()?.len() > LOG_LIMIT {
        log_file.set_len(0)?;
    }
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["embed", "--background"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    // Reaped here if this process outlives it (the picker, or the session it resumes).
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// A line for `claude-resume.log`, which is the stderr of `embed --background`.
fn log(line: &str) {
    eprintln!(
        "{} {line}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
}

/// Who is embedding, which decides how `embed_stale` waits and what it says.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Embedding {
    /// `claude-resume embed`: may download the model (the user agreed) and reports what it did.
    Command,
    /// A semantic search about to run: embeds a few sessions itself (showing progress on a
    /// terminal), but never waits for an embed already running and leaves a bigger backlog to the
    /// background, so the search is never held up for long.
    BeforeSearch,
    /// `embed --background`: gives way to an embed already running, and reports to the log.
    Background,
}

/// The most sessions a semantic search embeds itself, a few seconds' work.
#[cfg(feature = "semantic")]
const EMBED_BEFORE_SEARCH: usize = 10;

#[cfg(feature = "semantic")]
fn embed_stale(store: &Store, paths: &Paths, by: Embedding) -> Result<()> {
    use claude_resume::store::FileLock;
    let interactive = by != Embedding::Background && io::stderr().is_terminal();
    // One embedder at a time: a second run would only compute the same vectors again.
    let lock = paths.db.with_extension("embed.lock");
    let held = match FileLock::try_acquire(&lock)? {
        Some(held) => held,
        None if by == Embedding::Background => return Ok(()),
        None if by == Embedding::BeforeSearch => {
            still_embedding(semantic::stale(store)?.len());
            return Ok(());
        }
        None => {
            eprintln!("Waiting for another claude-resume to finish embedding…");
            FileLock::acquire(&lock)?
        }
    };
    let todo = semantic::stale(store)?;
    if todo.is_empty() {
        if by == Embedding::Command {
            eprintln!("All sessions are embedded.");
        }
        return Ok(());
    }
    if by == Embedding::BeforeSearch && todo.len() > EMBED_BEFORE_SEARCH {
        drop(held);
        if start_background_embed(paths) {
            still_embedding(todo.len());
        }
        return Ok(());
    }
    let embedder = semantic::Embedder::load(&paths.models, by == Embedding::Command)?;
    let started = Instant::now();
    for (n, (sid, text, hash)) in todo.iter().enumerate() {
        match embedder.embed_session(text) {
            Ok(vector) => {
                store.put_embedding(sid, semantic::MODEL_KEY, hash, &semantic::to_bytes(&vector))?
            }
            Err(e) if interactive => eprintln!("\nclaude-resume: could not embed {sid}: {e:#}"),
            Err(e) => eprintln!("claude-resume: could not embed {sid}: {e:#}"),
        }
        if interactive {
            eprint!("\rEmbedding sessions… {}/{}", n + 1, todo.len());
        }
    }
    if interactive {
        eprint!("\r\x1b[2K");
    }
    let done = format!(
        "Embedded {} session{} in {:.1}s.",
        todo.len(),
        if todo.len() == 1 { "" } else { "s" },
        started.elapsed().as_secs_f64()
    );
    match by {
        Embedding::Command => eprintln!("{done}"),
        Embedding::BeforeSearch => {}
        Embedding::Background => log(&done),
    }
    Ok(())
}

/// Say that a semantic search may miss `n` sessions a background embed has yet to reach.
#[cfg(feature = "semantic")]
fn still_embedding(n: usize) {
    match n {
        0 => {}
        1 => eprintln!(
            "1 session is still being embedded in the background, so these results may miss it."
        ),
        n => eprintln!(
            "{n} sessions are still being embedded in the background, so these results may miss them."
        ),
    }
}

#[cfg(not(feature = "semantic"))]
fn embed_stale(_store: &Store, _paths: &Paths, _by: Embedding) -> Result<()> {
    bail!(
        "this build of claude-resume has no semantic search (built without the `semantic` feature)"
    )
}

fn init(paths: &Paths) -> Result<ExitCode> {
    eprintln!("Configuring Claude Code for claude-resume…");
    let mut s = Settings::load(&paths.settings())?;
    match s.ensure_retention() {
        Retention::AlreadySet(days) => eprintln!("  ✓ cleanupPeriodDays is already {days}"),
        Retention::Set { previous } => eprintln!(
            "  ✓ cleanupPeriodDays: {} → {} (Claude Code no longer deletes old sessions)",
            previous.map_or("unset (30)".into(), |d| d.to_string()),
            settings::RETENTION_DAYS
        ),
    }
    let hooks = s.remove_legacy_hooks();
    if hooks > 0 {
        eprintln!(
            "  ✓ removed {hooks} SessionStart hook(s) an older claude-resume added (no longer needed)"
        );
    }
    if s.remove_marketplace_entry() {
        eprintln!("  ✓ removed the plugin marketplace an older claude-resume registered");
    }
    if let Some(backup) = s.save()? {
        eprintln!(
            "  ✓ backed up your previous settings to {}",
            backup.display()
        );
    }
    eprintln!("\nDone. Run `claude-resume` to browse sessions, or `claude-resume search <words>`.");
    Ok(ExitCode::SUCCESS)
}

fn uninstall(paths: &Paths) -> Result<ExitCode> {
    eprintln!("Removing claude-resume…");
    let mut s = Settings::load(&paths.settings())?;
    let hooks = s.remove_legacy_hooks();
    let marketplace = s.remove_marketplace_entry();
    if hooks > 0 || marketplace {
        s.save()?;
        eprintln!(
            "  ✓ removed claude-resume entries from {}",
            paths.settings().display()
        );
    }
    let mut removed = Vec::new();
    for db in [paths.db.clone(), paths.legacy_db()] {
        let db_name = db
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        for path in [
            db.clone(),
            db.with_file_name(format!("{db_name}-wal")),
            db.with_file_name(format!("{db_name}-shm")),
            db.with_extension("lock"),
            db.with_extension("embed.lock"),
            db.with_extension("log"),
        ] {
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                removed.push(path);
            }
        }
    }
    if !removed.is_empty() {
        eprintln!("  ✓ removed the search index ({} files)", removed.len());
    }
    // Only the model claude-resume downloads, and only from its own directory: one set with
    // CLAUDE_RESUME_MODELS_DIR may be a cache that other tools share.
    let model = semantic::model_dir(&paths.models);
    if model.exists() && paths.models == paths.claude_dir.join("models") {
        std::fs::remove_dir_all(&model).with_context(|| format!("removing {}", model.display()))?;
        let _ = std::fs::remove_dir(&paths.models); // fails, as intended, unless it is now empty
        eprintln!(
            "  ✓ removed the embedding model from {}",
            paths.models.display()
        );
    } else if model.exists() {
        eprintln!(
            "  ℹ left the embedding model in {} (CLAUDE_RESUME_MODELS_DIR); delete it if nothing else uses it",
            model.display()
        );
    }
    eprintln!("  ℹ cleanupPeriodDays left as it is, so your sessions stay safe");
    Ok(ExitCode::SUCCESS)
}
