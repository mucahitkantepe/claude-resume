mod embedder;
mod index;
mod install;
mod parser;
mod tui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "claude-resume", about = "Session finder for Claude Code", version)]
struct Cli {
    /// Force full index rebuild
    #[arg(short, long)]
    force: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Search sessions non-interactively (for Claude Code plugin)
    Search {
        /// Search query
        query: String,
        /// Maximum results
        #[arg(short = 'n', long, default_value = "10")]
        max: usize,
        /// Search mode: exact, fuzzy (default), semantic
        #[arg(short = 'm', long, default_value = "fuzzy")]
        mode: String,
    },
    /// Configure Claude Code settings (cleanupPeriodDays, hooks)
    Init,
    /// Remove hooks and index
    Uninstall,
    /// Generate embeddings for semantic search (downloads model on first run)
    Embed,
    /// Sync index (used internally by SessionStart hook)
    #[command(hide = true)]
    Sync,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        None => interactive(cli.force),
        Some(Commands::Search { query, max, mode }) => cmd_search(&query, max, &mode),
        Some(Commands::Init) => cmd_init(),
        Some(Commands::Uninstall) => cmd_uninstall(),
        Some(Commands::Embed) => cmd_embed(),
        Some(Commands::Sync) => index::sync(false),
    }
}

/// Interactive TUI session picker (default command).
fn interactive(force: bool) {
    if std::env::var("CLAUDE_RESUME_NO_SYNC").is_err() {
        index::sync(force);
    }
    let entries = index::load_index();
    if entries.is_empty() {
        eprintln!("No sessions found.");
        std::process::exit(1);
    }

    match tui::run(entries) {
        Ok(Some(entry)) => {
            // cd to session directory and resume
            if !entry.cwd.is_empty() && std::path::Path::new(&entry.cwd).is_dir() {
                eprintln!("Resuming session in {}", entry.cwd);
                let _ = std::env::set_current_dir(&entry.cwd);
            }
            // Spawn via user's login shell so aliases are loaded
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            let status = std::process::Command::new(&shell)
                .arg("-ic")
                .arg(format!("claude --resume {}", &entry.sid))
                .status();
            match status {
                Ok(s) => std::process::exit(s.code().unwrap_or(1)),
                Err(e) => {
                    eprintln!("Failed to launch claude: {}", e);
                    eprintln!("Run: cd {} && claude --resume {}", entry.cwd, entry.sid);
                    std::process::exit(1);
                }
            }
        }
        Ok(None) => {} // User cancelled
        Err(e) => {
            eprintln!("TUI error: {}", e);
            std::process::exit(1);
        }
    }
}

/// Non-interactive search (for plugin skill).
fn cmd_search(query: &str, max: usize, mode: &str) {
    if std::env::var("CLAUDE_RESUME_NO_SYNC").is_err() {
        index::sync(false);
    }
    let entries = index::load_index();

    let match_indices = match mode {
        "exact" => index::search_exact(&entries, query),
        "semantic" => {
            let sids = index::search_semantic(query);
            let sid_to_idx: std::collections::HashMap<&str, usize> = entries
                .iter()
                .enumerate()
                .map(|(i, e)| (e.sid.as_str(), i))
                .collect();
            sids.iter()
                .filter_map(|sid| sid_to_idx.get(sid.as_str()).copied())
                .collect()
        }
        _ => index::search_fuzzy(&entries, query),
    };
    let matches: Vec<&index::IndexEntry> = match_indices.iter().map(|&i| &entries[i]).collect();

    if matches.is_empty() {
        println!("No sessions found matching \"{}\"", query);
        return;
    }

    let count = matches.len().min(max);
    println!("Found {} sessions matching \"{}\":\n", matches.len(), query);

    for entry in matches.iter().take(count) {
        let created = format_date(&entry.created);
        let modified = format_date(&entry.modified);
        println!(
            "  {} → {}  {:>4} msgs  {}",
            created, modified, entry.msg_count, entry.project
        );
        println!("  {}", entry.label);

        // Show match context
        let contexts = index::match_contexts_deep(entry, query, 3);
        for ctx in &contexts {
            let highlighted = ctx.replace(
                &query.to_lowercase(),
                &format!("**{}**", query),
            );
            // Case-insensitive highlight
            let re = regex::RegexBuilder::new(&regex::escape(query))
                .case_insensitive(true)
                .build();
            let display = match re {
                Ok(r) => r.replace_all(ctx, |caps: &regex::Captures| {
                    format!("**{}**", &caps[0])
                }).to_string(),
                Err(_) => highlighted,
            };
            println!("    {}", display);
        }
        println!("  Session ID: {}", entry.sid);
        println!();
    }

    if matches.len() > count {
        println!("  ... and {} more", matches.len() - count);
    }
}

fn cmd_embed() {
    if std::env::var("CLAUDE_RESUME_NO_SYNC").is_err() {
        index::sync(false);
    }

    if !embedder::is_model_downloaded() {
        eprintln!("Semantic search requires the bge-small-en-v1.5 embedding model.");
        eprintln!("  Model: BAAI/bge-small-en-v1.5 (384 dimensions)");
        eprintln!("  Size:  ~133MB download, stored in ~/.claude/models/");
        eprintln!("  Runs:  fully local, no API calls after download");
        eprintln!();
        eprint!("Download and generate embeddings? [y/N] ");

        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err() || !input.trim().eq_ignore_ascii_case("y") {
            eprintln!("Cancelled.");
            return;
        }
        eprintln!();
    }

    index::embed_all();
}

fn cmd_init() {
    eprintln!("Configuring Claude Code for claude-resume...");
    match install::patch_settings() {
        Ok(()) => eprintln!("\nDone! Restart Claude Code to apply changes."),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_uninstall() {
    eprintln!("Removing claude-resume hooks...");
    match install::unpatch_settings() {
        Ok(()) => {
            // Remove database
            let db = index::db_path();
            if db.exists() {
                let _ = std::fs::remove_file(&db);
                // Also remove WAL/SHM files
                let _ = std::fs::remove_file(db.with_extension("db-wal"));
                let _ = std::fs::remove_file(db.with_extension("db-shm"));
                eprintln!("  ✓ Removed search database");
            }
            // Remove old TSV index if it exists
            let old_tsv = dirs::home_dir()
                .unwrap_or_default()
                .join(".claude")
                .join("sessions-search-index.tsv");
            if old_tsv.exists() {
                let _ = std::fs::remove_file(&old_tsv);
                eprintln!("  ✓ Removed old TSV index");
            }
            eprintln!("\nDone.");
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn format_date(date_str: &str) -> String {
    chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .map(|d| d.format("%a %b %d").to_string())
        .unwrap_or_else(|_| date_str.to_string())
}

