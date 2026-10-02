//! Parse Claude Code session transcripts (`<claude dir>/projects/<project>/<session-id>.jsonl`).
//!
//! The JSONL format is internal to Claude Code and changes between releases, so parsing is
//! deliberately lenient: unknown entry types and unexpected field types are ignored, invalid
//! UTF-8 is replaced, and one malformed line never aborts the parse.

use crate::text;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

/// Characters kept from the start and the end of one tool result. Output is noisy and bulky
/// (file reads, logs); its head says what ran and its tail holds the outcome or the error.
pub const TOOL_RESULT_HEAD: usize = 250;
pub const TOOL_RESULT_TAIL: usize = 150;
/// Characters kept from one tool input field (a command, a path, an edited snippet)…
pub const TOOL_INPUT_CHARS: usize = 200;
/// …and from the whole one-line summary of a tool call.
pub const TOOL_CALL_CHARS: usize = 400;
/// Characters kept from one prompt or reply: enough for pasted logs and specs, bounded so one
/// pathological paste can't bloat the index.
pub const TEXT_CHARS: usize = 20_000;

/// How Claude Code starts the summary that opens a session continued after compaction.
const COMPACT_SUMMARY_PREFIX: &str = "This session is being continued from a previous conversation";

/// Everything claude-resume needs from one session file.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Transcript {
    /// Working directory the session was started in (first `cwd` seen).
    pub cwd: String,
    /// First git branch seen, ignoring a detached `HEAD`.
    pub branch: String,
    /// Set by `/rename` (the last rename wins).
    pub custom_title: Option<String>,
    /// Title Claude Code generates for the session (the last one wins).
    pub ai_title: Option<String>,
    /// `summary` entries written by older Claude Code versions.
    pub legacy_summary: Option<String>,
    /// What the user typed: prompts, queued prompts, slash commands and `!` shell escapes.
    pub prompts: Vec<Prompt>,
    /// Claude's visible replies, plus compaction summaries.
    pub replies: Vec<String>,
    /// Tool calls (`Bash: cargo test`), tool output excerpts and shell-escape output.
    pub tools: Vec<String>,
    /// Pull requests linked to the session.
    pub pr_links: Vec<String>,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    /// Distinct assistant messages (Claude Code writes one JSONL line per content block).
    pub assistant_turns: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    /// Whether this prompt describes the session well enough to be its title. Bare slash
    /// commands like `/clear` or `/model` don't.
    pub labelable: bool,
}

impl Transcript {
    /// Display title: `/rename` title, else Claude's generated title, else the first real prompt.
    pub fn title(&self) -> String {
        // Claude Code occasionally stores a compaction summary as the session's title.
        let usable = |t: &&String| {
            let t = t.trim();
            !t.is_empty() && !t.starts_with(COMPACT_SUMMARY_PREFIX)
        };
        let raw = self
            .custom_title
            .as_ref()
            .filter(usable)
            .or(self.ai_title.as_ref().filter(usable))
            .or(self.legacy_summary.as_ref().filter(usable))
            .map(String::as_str)
            .or_else(|| {
                self.prompts
                    .iter()
                    .find(|p| p.labelable)
                    .map(|p| p.text.as_str())
            })
            .or_else(|| self.prompts.first().map(|p| p.text.as_str()));
        match raw {
            Some(t) => text::truncate(&text::one_line(t), 120),
            None => "untitled".into(),
        }
    }

    /// Last path component of the working directory.
    pub fn project(&self) -> String {
        self.cwd
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .to_string()
    }

    /// No conversation worth listing: nothing but bare commands, and no reply from Claude.
    pub fn is_empty(&self) -> bool {
        self.replies.is_empty() && !self.prompts.iter().any(|p| p.labelable)
    }
}

pub fn parse_file(path: &Path) -> io::Result<Transcript> {
    parse_reader(BufReader::with_capacity(256 * 1024, File::open(path)?))
}

pub fn parse_reader<R: BufRead>(mut reader: R) -> io::Result<Transcript> {
    let mut t = Transcript::default();
    let mut assistant_ids = HashSet::new();
    let mut buf = Vec::with_capacity(64 * 1024);
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = buf.trim_ascii();
        if line.is_empty() {
            continue;
        }
        let entry: Entry = match serde_json::from_slice(line) {
            Ok(e) => e,
            // Invalid UTF-8 inside a string: retry with replacement characters.
            Err(_) if std::str::from_utf8(line).is_err() => {
                match serde_json::from_str(&String::from_utf8_lossy(line)) {
                    Ok(e) => e,
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        };
        t.absorb(entry, &mut assistant_ids);
    }
    Ok(t)
}

/// The fields we read from one JSONL line. Everything else (notably the large `toolUseResult`
/// and `snapshot` payloads) is skipped by serde without being materialised.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Entry {
    #[serde(rename = "type", deserialize_with = "lenient::string")]
    kind: String,
    #[serde(deserialize_with = "lenient::opt_string")]
    timestamp: Option<String>,
    #[serde(deserialize_with = "lenient::string")]
    cwd: String,
    #[serde(rename = "gitBranch", deserialize_with = "lenient::string")]
    git_branch: String,
    #[serde(rename = "isMeta", deserialize_with = "lenient::bool")]
    is_meta: bool,
    #[serde(rename = "isSidechain", deserialize_with = "lenient::bool")]
    is_sidechain: bool,
    #[serde(rename = "isCompactSummary", deserialize_with = "lenient::bool")]
    is_compact_summary: bool,
    #[serde(rename = "isApiErrorMessage", deserialize_with = "lenient::bool")]
    is_api_error: bool,
    message: Option<Value>,
    attachment: Option<Value>,
    #[serde(rename = "customTitle", deserialize_with = "lenient::opt_string")]
    custom_title: Option<String>,
    #[serde(rename = "aiTitle", deserialize_with = "lenient::opt_string")]
    ai_title: Option<String>,
    #[serde(deserialize_with = "lenient::opt_string")]
    summary: Option<String>,
    #[serde(rename = "prUrl", deserialize_with = "lenient::opt_string")]
    pr_url: Option<String>,
}

/// How one piece of user-turn text should be treated.
#[derive(Debug, PartialEq, Eq)]
enum UserText {
    Prompt(String),
    Command { name: String, args: String },
    Shell(String),
    Output(String),
    Skip,
}

impl Transcript {
    fn absorb(&mut self, e: Entry, assistant_ids: &mut HashSet<String>) {
        if let Some(ts) = e.timestamp.as_deref().and_then(crate::time::parse_rfc3339) {
            self.first_ts = Some(self.first_ts.map_or(ts, |f| f.min(ts)));
            self.last_ts = Some(self.last_ts.map_or(ts, |l| l.max(ts)));
        }
        if self.cwd.is_empty() && !e.cwd.is_empty() {
            self.cwd.clone_from(&e.cwd);
        }
        if self.branch.is_empty() && !e.git_branch.is_empty() && e.git_branch != "HEAD" {
            self.branch.clone_from(&e.git_branch);
        }
        match e.kind.as_str() {
            "user" => self.absorb_user(&e),
            "assistant" => self.absorb_assistant(&e, assistant_ids),
            "attachment" => self.absorb_attachment(&e),
            "custom-title" => set_non_empty(&mut self.custom_title, e.custom_title),
            "ai-title" => set_non_empty(&mut self.ai_title, e.ai_title),
            "summary" => set_non_empty(&mut self.legacy_summary, e.summary),
            "pr-link" => {
                if let Some(url) = e
                    .pr_url
                    .filter(|u| !u.is_empty() && !self.pr_links.contains(u))
                {
                    self.pr_links.push(url);
                }
            }
            _ => {}
        }
    }

    fn absorb_user(&mut self, e: &Entry) {
        // Meta turns are Claude Code talking to itself: skill bodies, command caveats, reminders.
        if e.is_meta {
            return;
        }
        let Some(content) = e.message.as_ref().and_then(|m| m.get("content")) else {
            return;
        };
        if e.is_compact_summary {
            for t in texts(content) {
                self.replies.push(text::clean(text::cap(t, TEXT_CHARS)));
            }
            return;
        }
        let mut user_texts = Vec::new();
        match content {
            Value::String(s) => user_texts.push(s.as_str()),
            Value::Array(blocks) => {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => user_texts.extend(b.get("text").and_then(Value::as_str)),
                        Some("tool_result") => self.absorb_tool_result(b.get("content")),
                        _ => {} // images, documents
                    }
                }
            }
            _ => {}
        }
        self.absorb_user_texts(&user_texts, e.is_sidechain);
    }

    /// Prompts typed while Claude was busy are delivered as `queued_command` attachments, not as
    /// user turns, so without this they would be invisible to search.
    fn absorb_attachment(&mut self, e: &Entry) {
        let Some(a) = e.attachment.as_ref() else {
            return;
        };
        if a.get("type").and_then(Value::as_str) != Some("queued_command") {
            return;
        }
        if a.get("commandMode")
            .and_then(Value::as_str)
            .is_some_and(|m| m != "prompt")
        {
            return; // task notifications and other machine-generated queue entries
        }
        if let Some(prompt) = a.get("prompt") {
            let user_texts = texts(prompt);
            self.absorb_user_texts(&user_texts, e.is_sidechain);
        }
    }

    fn absorb_user_texts(&mut self, user_texts: &[&str], sidechain: bool) {
        let mut parts: Vec<String> = Vec::new();
        for raw in user_texts {
            match classify(raw) {
                UserText::Prompt(p) => parts.push(p),
                UserText::Command { name, args } => {
                    self.flush_prompt(&mut parts, sidechain);
                    let labelable = !args.is_empty();
                    let text = if labelable {
                        format!("{name} {args}")
                    } else {
                        name
                    };
                    self.prompts.push(Prompt { text, labelable });
                }
                UserText::Shell(cmd) => {
                    self.flush_prompt(&mut parts, sidechain);
                    self.prompts.push(Prompt {
                        text: format!("! {cmd}"),
                        labelable: true,
                    });
                }
                UserText::Output(out) => self.tools.push(out),
                UserText::Skip => {}
            }
        }
        self.flush_prompt(&mut parts, sidechain);
    }

    fn flush_prompt(&mut self, parts: &mut Vec<String>, sidechain: bool) {
        if parts.is_empty() {
            return;
        }
        let text = parts.join("\n");
        parts.clear();
        if sidechain {
            self.tools.push(text); // a subagent's instructions, not something the user typed
        } else {
            self.prompts.push(Prompt {
                text,
                labelable: true,
            });
        }
    }

    fn absorb_assistant(&mut self, e: &Entry, assistant_ids: &mut HashSet<String>) {
        if e.is_api_error {
            return;
        }
        let Some(msg) = e.message.as_ref().filter(|m| m.is_object()) else {
            return;
        };
        match msg.get("id").and_then(Value::as_str) {
            Some(id) if !assistant_ids.insert(id.to_string()) => {}
            _ => self.assistant_turns += 1,
        }
        let Some(content) = msg.get("content") else {
            return;
        };
        let push_reply = |this: &mut Self, t: &str| {
            let t = text::clean(text::cap(t, TEXT_CHARS));
            if t.trim().is_empty() {
                return;
            }
            if e.is_sidechain {
                this.tools.push(t)
            } else {
                this.replies.push(t)
            }
        };
        match content {
            Value::String(s) => push_reply(self, s),
            Value::Array(blocks) => {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                push_reply(self, t);
                            }
                        }
                        Some("tool_use") => {
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                            self.tools.push(tool_call(name, b.get("input")));
                        }
                        _ => {} // thinking blocks are not shown to the user; skip them
                    }
                }
            }
            _ => {}
        }
    }

    fn absorb_tool_result(&mut self, content: Option<&Value>) {
        for t in content.map(texts).unwrap_or_default() {
            let t = text::clean(&text::head_tail(t, TOOL_RESULT_HEAD, TOOL_RESULT_TAIL));
            if !t.trim().is_empty() {
                self.tools.push(t);
            }
        }
    }
}

fn set_non_empty(slot: &mut Option<String>, value: Option<String>) {
    if let Some(v) = value.filter(|v| !v.trim().is_empty()) {
        *slot = Some(v);
    }
}

/// Text from a `content` value: a plain string, or the `text` blocks of a block list.
fn texts(content: &Value) -> Vec<&str> {
    match content {
        Value::String(s) => vec![s.as_str()],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// Recognise the wrappers Claude Code puts around non-prompt user turns.
fn classify(raw: &str) -> UserText {
    let s = raw.trim_start();
    if s.starts_with("<command-name>") || s.starts_with("<command-message>") {
        let name = text::tag_content(s, "command-name")
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .or_else(|| text::tag_content(s, "command-message").map(|m| format!("/{}", m.trim())))
            .unwrap_or_default();
        let args = text::tag_content(s, "command-args")
            .map(text::one_line)
            .unwrap_or_default();
        return UserText::Command {
            name,
            args: text::truncate(&args, TEXT_CHARS),
        };
    }
    if s.starts_with("<bash-input>") {
        let cmd = text::tag_content(s, "bash-input").unwrap_or_default();
        return UserText::Shell(text::one_line(&text::clean(cmd)));
    }
    for tag in [
        "bash-stdout",
        "bash-stderr",
        "local-command-stdout",
        "local-command-stderr",
    ] {
        if s.starts_with(&format!("<{tag}>")) {
            let out = text::tag_content(s, tag).unwrap_or_default();
            return match text::clean(&text::head_tail(out, TOOL_RESULT_HEAD, TOOL_RESULT_TAIL)) {
                o if o.trim().is_empty() => UserText::Skip,
                o => UserText::Output(o),
            };
        }
    }
    if s.starts_with("<task-notification>") {
        return UserText::Output(text::clean(&text::head_tail(
            s,
            TOOL_RESULT_HEAD,
            TOOL_RESULT_TAIL,
        )));
    }
    if s.starts_with("<local-command-caveat>")
        || s.starts_with("[Request interrupted")
        || s.starts_with(
            "Caveat: The messages below were generated by the user while running local commands",
        )
    {
        return UserText::Skip;
    }
    let stripped = text::strip_system_reminders(s);
    let cleaned = text::clean(text::cap(stripped.trim(), TEXT_CHARS));
    if cleaned.trim().is_empty() {
        UserText::Skip
    } else {
        UserText::Prompt(cleaned.trim().to_string())
    }
}

/// One line describing a tool call: its name plus the input fields a person would search for.
fn tool_call(name: &str, input: Option<&Value>) -> String {
    const FIELDS: &[&str] = &[
        "command",
        "file_path",
        "notebook_path",
        "path",
        "pattern",
        "glob",
        "url",
        "query",
        "skill",
        "args",
        "description",
        "prompt",
        "subject",
        "old_string",
        "new_string",
        "content",
    ];
    let mut out = format!("{name}:");
    let Some(obj) = input.and_then(Value::as_object) else {
        return out;
    };
    let mut found = false;
    for field in FIELDS {
        if let Some(v) = obj
            .get(*field)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
        {
            out.push(' ');
            out.push_str(text::cap(v, TOOL_INPUT_CHARS));
            found = true;
        }
    }
    if !found && !obj.is_empty() {
        out.push(' ');
        out.push_str(text::cap(
            &Value::Object(obj.clone()).to_string(),
            TOOL_INPUT_CHARS,
        ));
    }
    text::clean(text::cap(&out, TOOL_CALL_CHARS))
}

mod lenient {
    //! Field deserializers that accept any JSON value, so one oddly-typed field never costs us
    //! the whole line.
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    pub fn string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::String(s) => s,
            _ => String::new(),
        })
    }

    pub fn opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::String(s) => Some(s),
            _ => None,
        })
    }

    pub fn bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
        Ok(matches!(Value::deserialize(d)?, Value::Bool(true)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(lines: &[Value]) -> Transcript {
        let data = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        parse_reader(data.as_bytes()).unwrap()
    }

    fn user(content: Value) -> Value {
        json!({"type": "user", "timestamp": "2026-03-28T10:00:00Z", "cwd": "/home/me/proj", "message": {"role": "user", "content": content}})
    }

    fn assistant(id: &str, blocks: Value) -> Value {
        json!({"type": "assistant", "timestamp": "2026-03-28T10:05:00Z", "message": {"id": id, "role": "assistant", "content": blocks}})
    }

    fn prompt_texts(t: &Transcript) -> Vec<&str> {
        t.prompts.iter().map(|p| p.text.as_str()).collect()
    }

    #[test]
    fn title_prefers_rename_then_ai_title_then_first_prompt() {
        let mut lines = vec![
            user(json!("first prompt")),
            assistant("m1", json!([{"type": "text", "text": "ok"}])),
        ];
        assert_eq!(parse(&lines).title(), "first prompt");
        lines.push(json!({"type": "ai-title", "aiTitle": "Generated title", "sessionId": "s"}));
        assert_eq!(parse(&lines).title(), "Generated title");
        lines.push(json!({"type": "custom-title", "customTitle": "my rename", "sessionId": "s"}));
        lines.push(
            json!({"type": "ai-title", "aiTitle": "A later generated title", "sessionId": "s"}),
        );
        assert_eq!(parse(&lines).title(), "my rename");
        lines.push(
            json!({"type": "custom-title", "customTitle": "renamed again", "sessionId": "s"}),
        );
        assert_eq!(parse(&lines).title(), "renamed again");
    }

    #[test]
    fn a_compaction_summary_stored_as_the_title_is_ignored() {
        let t = parse(&[
            user(json!("continue the migration")),
            json!({"type": "custom-title", "customTitle": "This session is being continued from a previous conversation that ran out of context. Summary: …"}),
        ]);
        assert_eq!(t.title(), "continue the migration");
    }

    #[test]
    fn legacy_summary_entries_are_a_title_fallback() {
        let t = parse(&[
            json!({"type": "summary", "summary": "Old style summary", "leafUuid": "x"}),
            user(json!("hi")),
        ]);
        assert_eq!(t.title(), "Old style summary");
    }

    #[test]
    fn untitled_when_there_is_nothing_to_show() {
        assert_eq!(Transcript::default().title(), "untitled");
    }

    #[test]
    fn bare_commands_are_prompts_but_not_titles() {
        let t = parse(&[
            user(json!(
                "<command-name>/clear</command-name>\n            <command-message>clear</command-message>\n            <command-args></command-args>"
            )),
            user(json!("<local-command-stdout></local-command-stdout>")),
            user(json!("fix the flaky login test")),
            user(json!(
                "<command-name>/review</command-name><command-message>review</command-message><command-args>PR 482</command-args>"
            )),
        ]);
        assert_eq!(
            prompt_texts(&t),
            ["/clear", "fix the flaky login test", "/review PR 482"]
        );
        assert_eq!(t.title(), "fix the flaky login test");
        assert!(!t.is_empty());
    }

    #[test]
    fn command_with_args_can_be_the_title() {
        let t = parse(&[user(json!(
            "<command-name>/claude-resume:search</command-name><command-args>kafka lag</command-args>"
        ))]);
        assert_eq!(t.title(), "/claude-resume:search kafka lag");
    }

    #[test]
    fn command_only_session_without_replies_is_empty() {
        let t = parse(&[user(json!(
            "<command-name>/model</command-name><command-args></command-args>"
        ))]);
        assert!(t.is_empty());
        assert_eq!(t.title(), "/model");
    }

    #[test]
    fn skips_meta_interrupts_and_caveats() {
        let mut meta = user(json!("Base directory for this skill: /x\n# Skill body"));
        meta["isMeta"] = json!(true);
        let t = parse(&[
            meta,
            user(json!(
                "<local-command-caveat>Caveat: run locally</local-command-caveat>"
            )),
            user(json!(
                "Caveat: The messages below were generated by the user while running local commands. DO NOT respond."
            )),
            user(json!("[Request interrupted by user for tool use]")),
            user(json!("real question")),
        ]);
        assert_eq!(prompt_texts(&t), ["real question"]);
    }

    #[test]
    fn compact_summaries_are_searchable_but_never_the_title() {
        let mut summary = user(json!(
            "This session is being continued from a previous conversation. Summary: kafka migration"
        ));
        summary["isCompactSummary"] = json!(true);
        let t = parse(&[summary, user(json!("continue with the rollout"))]);
        assert_eq!(t.title(), "continue with the rollout");
        assert!(t.replies[0].contains("kafka migration"));
    }

    #[test]
    fn shell_escapes_and_their_output() {
        let t = parse(&[
            user(json!("<bash-input> git status --short</bash-input>")),
            user(json!(
                "<bash-stdout> M src/main.rs</bash-stdout><bash-stderr></bash-stderr>"
            )),
        ]);
        assert_eq!(prompt_texts(&t), ["! git status --short"]);
        assert_eq!(t.tools, [" M src/main.rs"]);
    }

    #[test]
    fn system_reminders_are_stripped_from_prompts() {
        let t = parse(&[user(json!([
            {"type": "text", "text": "deploy it<system-reminder>internal note</system-reminder>"},
            {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
            {"type": "text", "text": "<system-reminder>only a reminder</system-reminder>"},
        ]))]);
        assert_eq!(prompt_texts(&t), ["deploy it"]);
    }

    #[test]
    fn queued_prompts_are_indexed_but_task_notifications_are_not() {
        let t = parse(&[
            user(json!("start the migration")),
            json!({"type": "attachment", "attachment": {"type": "queued_command", "commandMode": "prompt", "prompt": "also bump the version"}}),
            json!({"type": "attachment", "attachment": {"type": "queued_command", "prompt": [{"type": "text", "text": "and tag it"}]}}),
            json!({"type": "attachment", "attachment": {"type": "queued_command", "commandMode": "task-notification", "prompt": "<task-notification>done</task-notification>"}}),
            json!({"type": "attachment", "attachment": {"type": "total_tokens_reminder", "prompt": "nope"}}),
        ]);
        assert_eq!(
            prompt_texts(&t),
            ["start the migration", "also bump the version", "and tag it"]
        );
    }

    #[test]
    fn replies_tool_calls_and_results() {
        let t = parse(&[
            user(json!("run the tests")),
            assistant(
                "m1",
                json!([{"type": "thinking", "thinking": "secret plan"}]),
            ),
            assistant(
                "m1",
                json!([{"type": "text", "text": "Running cargo test now."}]),
            ),
            assistant(
                "m1",
                json!([{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cargo test --all", "description": "Run tests"}}]),
            ),
            user(
                json!([{"type": "tool_result", "tool_use_id": "t1", "content": "test result: ok. 42 passed"}]),
            ),
            assistant(
                "m2",
                json!([{"type": "tool_use", "id": "t2", "name": "Edit", "input": {"file_path": "/src/lib.rs", "old_string": "foo()", "new_string": "bar()"}}]),
            ),
            user(
                json!([{"type": "tool_result", "tool_use_id": "t2", "content": [{"type": "text", "text": "edited"}, {"type": "image"}]}]),
            ),
            assistant(
                "m3",
                json!([{"type": "tool_use", "id": "t3", "name": "mcp__thing__do", "input": {"weird": 1}}]),
            ),
        ]);
        assert_eq!(t.replies, ["Running cargo test now."]);
        assert_eq!(t.assistant_turns, 3, "one per distinct message id");
        assert!(
            t.tools
                .contains(&"Bash: cargo test --all Run tests".to_string())
        );
        assert!(t.tools.contains(&"test result: ok. 42 passed".to_string()));
        assert!(
            t.tools
                .contains(&"Edit: /src/lib.rs foo() bar()".to_string())
        );
        assert!(t.tools.contains(&"edited".to_string()));
        assert!(
            t.tools
                .contains(&r#"mcp__thing__do: {"weird":1}"#.to_string())
        );
        assert!(!format!("{t:?}").contains("secret plan"));
        assert_eq!(
            prompt_texts(&t),
            ["run the tests"],
            "tool results are not prompts"
        );
    }

    #[test]
    fn long_tool_results_keep_head_and_tail() {
        let big = format!("HEADER {} FAILED: 3 tests", "x".repeat(100_000));
        let t = parse(&[user(json!([{"type": "tool_result", "content": big}]))]);
        assert!(t.tools[0].starts_with("HEADER"));
        assert!(
            t.tools[0].ends_with("FAILED: 3 tests"),
            "the outcome at the end is kept"
        );
        assert!(t.tools[0].chars().count() <= TOOL_RESULT_HEAD + TOOL_RESULT_TAIL + 3);
    }

    #[test]
    fn long_prompts_on_huge_lines_are_kept() {
        // The 0.4 parser skipped every line over 50 KB, losing real prompts with pasted logs.
        let prompt = format!(
            "please analyse this log UNIQUE_MARKER {}",
            "line\n".repeat(20_000)
        );
        let t = parse(&[user(json!(prompt))]);
        assert!(t.prompts[0].text.contains("UNIQUE_MARKER"));
    }

    #[test]
    fn metadata_branch_cwd_project_and_timestamps() {
        let mut first = user(json!("hi"));
        first["gitBranch"] = json!("HEAD");
        let mut second = assistant("m1", json!([{"type": "text", "text": "hello"}]));
        second["gitBranch"] = json!("feat/search");
        second["cwd"] = json!("/other");
        second["timestamp"] = json!("2026-03-29T08:00:00.500Z");
        let t = parse(&[first, second]);
        assert_eq!(t.branch, "feat/search", "detached HEAD is ignored");
        assert_eq!(t.cwd, "/home/me/proj", "first cwd wins");
        assert_eq!(t.project(), "proj");
        assert_eq!(t.first_ts, Some(1_774_692_000));
        assert_eq!(t.last_ts, Some(1_774_771_200));
    }

    #[test]
    fn pr_links_are_collected_once() {
        let pr = json!({"type": "pr-link", "prNumber": 7, "prUrl": "https://github.com/o/r/pull/7", "prRepository": "o/r"});
        let t = parse(&[pr.clone(), pr]);
        assert_eq!(t.pr_links, ["https://github.com/o/r/pull/7"]);
    }

    #[test]
    fn tolerates_garbage_unknown_types_and_odd_fields() {
        let data = [
            "not json at all",
            "",
            r#"{"type":"user","message":{"content":"survives"},"isMeta":"yes","cwd":42}"#,
            r#"{"type":"brand-new-entry-type","payload":{"x":1}}"#,
            r#"[1,2,3]"#,
            r#"{"type":"assistant","message":"not an object"}"#,
        ]
        .join("\n");
        let t = parse_reader(data.as_bytes()).unwrap();
        assert_eq!(prompt_texts(&t), ["survives"]);
        assert_eq!(t.cwd, "");
    }

    #[test]
    fn invalid_utf8_is_replaced_not_dropped() {
        let mut data = br#"{"type":"user","message":{"content":"caf"#.to_vec();
        data.extend_from_slice(&[0xff, 0xfe]);
        data.extend_from_slice(br#" order"}}"#);
        let t = parse_reader(&data[..]).unwrap();
        assert_eq!(t.prompts.len(), 1);
        assert!(t.prompts[0].text.starts_with("caf") && t.prompts[0].text.ends_with("order"));
    }

    #[test]
    fn carriage_returns_and_ansi_are_cleaned() {
        let t = parse(&[user(
            json!([{"type": "tool_result", "content": "\u{1b}[32mPASS\u{1b}[0m\r\nprogress 10%\rprogress 100%"}]),
        )]);
        assert_eq!(t.tools, ["PASS\nprogress 10%\nprogress 100%"]);
    }

    #[test]
    fn sidechain_turns_count_as_tool_activity() {
        let mut agent_prompt = user(json!("search the repo for TODOs"));
        agent_prompt["isSidechain"] = json!(true);
        let t = parse(&[user(json!("main prompt")), agent_prompt]);
        assert_eq!(prompt_texts(&t), ["main prompt"]);
        assert!(t.tools.contains(&"search the repo for TODOs".to_string()));
    }

    #[test]
    fn api_error_messages_are_ignored() {
        let mut err = assistant(
            "e1",
            json!([{"type": "text", "text": "API Error: 529 overloaded"}]),
        );
        err["isApiErrorMessage"] = json!(true);
        let t = parse(&[user(json!("hi")), err]);
        assert!(t.replies.is_empty());
        assert_eq!(t.assistant_turns, 0);
    }
}
