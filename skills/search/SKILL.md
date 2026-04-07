---
description: "Search past Claude Code sessions by content. Usage: /claude-resume:search <query>"
user_invocable: true
---

# Session Search

Search previous Claude Code sessions — messages, tool output, code snippets.

**The user MUST provide a search query.** If they didn't, ask: "What should I search for?"

## Steps

1. Run the search:

```bash
claude-resume search "<QUERY>" -n 10
```

Replace `<QUERY>` with the user's search term.

2. If `claude-resume` is not found, tell the user to install it:

```
curl -fsSL https://raw.githubusercontent.com/mucahitkantepe/claude-resume/main/install.sh | sh
```

3. Present the results. When the user picks a session, give the resume command for a **new terminal**:

```
claude --resume <session_id>
```

## Important

- NEVER run `! claude --resume` — it's interactive and won't work inside a session
- If no matches, suggest different keywords
