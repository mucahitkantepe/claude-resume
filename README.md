# claude-resume

Never lose a Claude Code conversation again.

<p align="center">
  <img src="demo.gif" alt="claude-resume demo" width="800">
</p>

Claude Code lets you resume sessions, but finding the right one gets hard fast — especially once you have hundreds. Sessions older than 30 days are deleted by default, and there's no way to search across session content. `claude-resume` adds full-text search across every conversation you've ever had — messages, tool output, code snippets — with a keyboard-driven TUI that lets you find and resume any session in seconds.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/mucahitkantepe/claude-resume/main/install.sh | sh
```

Single binary, no dependencies. Configures Claude Code automatically.

### Build from source

```sh
git clone https://github.com/mucahitkantepe/claude-resume.git
cd claude-resume
cargo build --release                              # CPU
cargo build --release --features metal,accelerate  # macOS Apple Silicon (GPU)
cp target/release/claude-resume ~/.local/bin/
claude-resume init
```

## Usage

```sh
claude-resume                            # Interactive TUI
claude-resume search "karabasan"         # Non-interactive search
claude-resume search "auth" -m semantic  # Semantic search
claude-resume embed                      # Download model + generate embeddings
claude-resume --force                    # Force full index rebuild
claude-resume init                       # Configure Claude Code
claude-resume uninstall                  # Remove hooks and database
```

### Search modes

Press `Shift+Tab` in the TUI to cycle between modes:

- **Exact** — case-insensitive substring match across all fields
- **Fuzzy** (default) — typo-tolerant matching on labels + near-match on content
- **Semantic** — finds sessions by meaning, not just keywords. "deploy to production" finds Terraform sessions even without those exact words

Semantic search requires a one-time model download (~133MB). Run `claude-resume embed` to set it up. Uses [bge-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5) locally via candle — no API calls, fully offline after download. GPU-accelerated on Apple Silicon.

## How it works

- **Indexes everything** — parses all `.jsonl` session files, extracts user messages, assistant responses, and tool output into a local SQLite index
- **Fuzzy + exact search** — fuzzy matching on session labels, exact substring on full content, with 1-edit near-match tolerance
- **Semantic search** — embeds session content with a local transformer model, finds similar sessions via cosine similarity
- **Preserves sessions** — sets `cleanupPeriodDays: 99999` so Claude Code stops deleting your history
- **Incremental sync** — only re-parses changed sessions, stays fresh via a SessionStart hook
- **Resume from anywhere** — automatically `cd`s to the session's original directory before running `claude --resume`

## Uninstall

```sh
claude-resume uninstall
rm ~/.local/bin/claude-resume
```

## License

MIT
