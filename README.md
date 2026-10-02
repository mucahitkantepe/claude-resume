# claude-resume

Never lose a Claude Code conversation again.

<p align="center">
  <img src="demo.gif" alt="claude-resume demo" width="800">
</p>

Claude Code lets you resume sessions, but finding the right one gets hard fast — especially once you have hundreds. Sessions older than 30 days are deleted by default, and there's no way to search across session content. `claude-resume` adds full-text search across every conversation you've ever had — your prompts, Claude's replies, the commands it ran, the files it touched and the tool output it saw — with a keyboard-driven TUI that finds and resumes any session in seconds.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/mucahitkantepe/claude-resume/master/install.sh | sh
```

Single binary, no dependencies. The installer verifies the download's checksum, puts the binary in `~/.local/bin` and runs `claude-resume init`, which sets `cleanupPeriodDays` to 99999 in `~/.claude/settings.json` so Claude Code stops deleting old sessions. Your settings are backed up once and keep their key order and indentation; a symlinked settings file stays a symlink.

Set `CLAUDE_RESUME_NO_INIT=1` to skip that, or `CLAUDE_RESUME_VERSION=v0.5.0` to pin a release.

### Build from source

```sh
git clone https://github.com/mucahitkantepe/claude-resume.git
cd claude-resume
cargo build --release                               # CPU
cargo build --release --features metal,accelerate   # macOS Apple Silicon (GPU)
cargo build --release --no-default-features         # without semantic search (smaller, faster build)
cp target/release/claude-resume ~/.local/bin/
claude-resume init
```

## Usage

```sh
claude-resume                         # interactive picker
claude-resume kafka lag               # picker, pre-filtered
claude-resume search "kafka lag"      # print matches (add --json for scripts)
claude-resume search deploy -m semantic --json
claude-resume resume 1b2c3d4e         # resume by id or unique id prefix, in the session's directory
claude-resume embed                   # set up semantic search (asks before downloading the model)
claude-resume sync --force            # rebuild the index
claude-resume init / uninstall
```

### Search syntax

- Words are matched case-insensitively anywhere in a session, in any order: `kafka lag` finds sessions mentioning both.
- Results are ordered: title matches, then sessions where the words appear together as typed, then the rest; most recent first within each group.
- `"consumer lag"` matches the exact phrase; `-staging` excludes sessions containing *staging*. On the command line, put such a query after `--` (`claude-resume search -- kafka -staging`), and quote a word that starts with a dash to search for it (`"-rf"`).
- Words shorter than three characters only match titles, projects and branches.

### Search modes

Press `Shift+Tab` in the picker (or pass `-m`) to switch:

- **fuzzy** (default) — titles match with typos (`kfka` finds *Kafka…*), then exact matches in the conversation, then near matches for long words (one extra or swapped letter: `postgress`).
- **exact** — every word must appear as written (case-insensitive substring).
- **semantic** — finds sessions by meaning, not keywords: "speed up the deploy pipeline" finds Terraform and CI sessions without those words. Needs a one-time `claude-resume embed`, which downloads [bge-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5) (~133 MB) after asking; after that everything runs locally through candle, and new sessions are embedded as they come in (see below).

### Picker keys

| key | action |
| --- | --- |
| type | search |
| `↑` `↓` `Tab` `^p` `^n` `^k` `^j` `PgUp` `PgDn` | move |
| `Enter` | resume the selected session in its original directory |
| `Shift+Tab` | switch search mode |
| `^u` `^d` | scroll the preview |
| `^w` `Alt+Backspace` | delete a word |
| `Esc` `^c` | quit |

Resuming runs `claude --resume <id>` through your interactive shell, so a `claude` alias or function (e.g. one that adds flags) is honoured.

## How it works

- **Index** — `~/.claude/claude-resume.db`, a SQLite database with an FTS5 trigram index (substring search in milliseconds). It is private to your user and rebuilt automatically when the format changes. `CLAUDE_CONFIG_DIR` is honoured.
- **What is indexed** — every prompt (including ones queued while Claude was busy and very long pastes), Claude's replies, compaction summaries, tool calls (commands, file paths, edited code) and the start and end of each tool output. Titles come from `/rename`, then Claude Code's generated title, then the first real prompt. Sessions that contain nothing but slash commands (a lone `/exit`) are not listed.
- **Incremental** — only new or changed transcripts are parsed (compared by size and nanosecond mtime), in parallel. Every command brings the index up to date before it searches, so there is nothing to schedule.
- **Embeddings** — once `claude-resume embed` has downloaded the model, every command also embeds sessions that changed, in a detached background process, so nobody waits for the model (one runs at a time, and what it reports goes to `~/.claude/claude-resume.log`). A semantic search loads the model anyway, so it embeds a few changed sessions itself before it searches; a bigger backlog, or one already being embedded, it leaves to the background and says its results may miss those sessions.
- **Read-only** — claude-resume never modifies or deletes anything under `~/.claude/projects`.

## Development

```sh
cargo test                      # unit + integration tests (the TUI end-to-end tests need tmux)
cargo test -- --ignored         # also the semantic tests that run the real embedding model (the others use a tiny stand-in)
scripts/record-demo.sh          # re-record demo.gif (needs vhs and ttyd) from made-up sessions
```

The integration tests run the real binary against transcripts written in Claude Code's JSONL format inside a temporary `HOME`, with a fake `claude` on `PATH` that records how it was launched. The demo recording never reads your own sessions either: `scripts/demo-sessions.py` writes fictional ones under `target/demo`.

## Uninstall

```sh
claude-resume uninstall
rm ~/.local/bin/claude-resume
```

`uninstall` removes the index, the downloaded embedding model and what older versions added to Claude Code's settings. It leaves `cleanupPeriodDays` alone, so your sessions stay safe.

## License

MIT
