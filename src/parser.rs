use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Parsed session metadata + search text
pub struct SessionMeta {
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

/// Parse a .jsonl session file into metadata and search text.
/// Skips individual lines >50KB (e.g. base64 blobs) but indexes everything else.
pub fn parse_session(path: &Path) -> Option<SessionMeta> {
    let sid = path.file_stem()?.to_str()?.to_string();

    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let mtime_epoch = mtime.as_secs();
    let modified = chrono::DateTime::from_timestamp(mtime_epoch as i64, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default();

    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut first_prompt = String::new();
    let mut custom_title = String::new();
    let mut search_parts: Vec<String> = Vec::new();
    let mut msg_count: u32 = 0;
    let mut branch = String::new();
    let mut cwd = String::new();
    let mut created = String::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        // Skip lines > 50KB (base64 blobs, huge tool outputs)
        if line.len() > 50_000 {
            if line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"") {
                msg_count += 1;
            }
            continue;
        }

        let entry: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let etype = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if created.is_empty() {
            if let Some(ts) = entry.get("timestamp").and_then(|v| v.as_str()) {
                created = ts.chars().take(10).collect();
            }
        }

        if branch.is_empty() {
            if let Some(b) = entry.get("gitBranch").and_then(|v| v.as_str()) {
                if b != "HEAD" {
                    branch = b.to_string();
                }
            }
        }

        if cwd.is_empty() {
            if let Some(c) = entry.get("cwd").and_then(|v| v.as_str()) {
                cwd = c.to_string();
            }
        }

        match etype {
            "user" | "assistant" => {
                msg_count += 1;
                let content = entry.get("message").and_then(|m| m.get("content"));

                if let Some(content) = content {
                    let texts = extract_texts(content);

                    for text in &texts {
                        // Cap individual blocks at 2KB to skip base64/binary noise
                        let truncated: String = text.chars().take(2000).collect();
                        search_parts.push(truncated);
                    }

                    // Index short tool results
                    if let Some(arr) = content.as_array() {
                        for block in arr {
                            if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                                if let Some(tr) = block.get("content").and_then(|v| v.as_str()) {
                                    if tr.len() < 5000 {
                                        let truncated: String = tr.chars().take(1500).collect();
                                        search_parts.push(truncated);
                                    }
                                }
                            }
                        }
                    }

                    if etype == "user" {
                        if let Some(first_text) = texts.first() {
                            if !first_text.starts_with("<local-command")
                                && !first_text.starts_with("<command-name>/exit")
                            {
                                if first_prompt.is_empty() {
                                    first_prompt = first_text.clone();
                                }
                            }
                        }
                    }
                }
            }
            "custom-title" => {
                // /rename stores the custom session name here
                if let Some(title) = entry.get("customTitle").and_then(|v| v.as_str()) {
                    custom_title = title.to_string();
                }
            }
            _ => {}
        }
    }

    if msg_count == 0 {
        return None;
    }

    // Prefer custom title (/rename) over first prompt
    let mut label = if !custom_title.is_empty() {
        custom_title
    } else if !first_prompt.is_empty() {
        first_prompt
    } else {
        "untitled".to_string()
    };
    label = label.replace('\n', " ").replace('\t', " ");
    if label.chars().count() > 60 {
        label = label.chars().take(60).collect::<String>() + "…";
    }

    let project = if cwd.is_empty() {
        String::new()
    } else {
        cwd.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string()
    };

    let search_raw = search_parts.join(" ").replace('\n', " ").replace('\t', " ");
    let ansi_re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    let search_text = ansi_re.replace_all(&search_raw, "").to_string();

    if created.is_empty() {
        created = modified.clone();
    }

    Some(SessionMeta {
        sid,
        created,
        modified,
        mtime_epoch,
        msg_count,
        label,
        branch,
        project,
        cwd,
        search_text,
    })
}

fn extract_texts(content: &Value) -> Vec<String> {
    let mut texts = Vec::new();
    if let Some(s) = content.as_str() {
        texts.push(s.to_string());
    } else if let Some(arr) = content.as_array() {
        for block in arr {
            if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    texts.push(t.to_string());
                }
            }
        }
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_session(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        f
    }

    #[test]
    fn parse_basic_session() {
        let f = write_session(&[
            r#"{"type":"user","timestamp":"2025-03-15T10:00:00Z","message":{"content":"hello world"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi there"}]}}"#,
        ]);
        let meta = parse_session(f.path()).unwrap();
        assert_eq!(meta.msg_count, 2);
        assert_eq!(meta.created, "2025-03-15");
        assert_eq!(meta.label, "hello world");
        assert!(meta.search_text.contains("hello world"));
        assert!(meta.search_text.contains("hi there"));
    }

    #[test]
    fn parse_empty_session_returns_none() {
        let f = write_session(&[
            r#"{"type":"system","timestamp":"2025-01-01T00:00:00Z"}"#,
        ]);
        assert!(parse_session(f.path()).is_none());
    }

    #[test]
    fn parse_extracts_branch_and_cwd() {
        let f = write_session(&[
            r#"{"type":"user","timestamp":"2025-01-01T00:00:00Z","gitBranch":"feat/search","cwd":"/home/user/proj","message":{"content":"test"}}"#,
            r#"{"type":"assistant","message":{"content":"reply"}}"#,
        ]);
        let meta = parse_session(f.path()).unwrap();
        assert_eq!(meta.branch, "feat/search");
        assert_eq!(meta.cwd, "/home/user/proj");
        assert_eq!(meta.project, "proj");
    }

    #[test]
    fn parse_label_truncated() {
        let long_msg = "a".repeat(100);
        let line = format!(
            r#"{{"type":"user","timestamp":"2025-01-01T00:00:00Z","message":{{"content":"{}"}}}}"#,
            long_msg
        );
        let f = write_session(&[
            &line,
            r#"{"type":"assistant","message":{"content":"ok"}}"#,
        ]);
        let meta = parse_session(f.path()).unwrap();
        assert!(meta.label.chars().count() <= 61); // 60 + "…"
    }

    #[test]
    fn parse_skips_local_commands() {
        let f = write_session(&[
            r#"{"type":"user","timestamp":"2025-01-01T00:00:00Z","message":{"content":"<local-command>exit</local-command>"}}"#,
            r#"{"type":"user","message":{"content":"real prompt here"}}"#,
            r#"{"type":"assistant","message":{"content":"response"}}"#,
        ]);
        let meta = parse_session(f.path()).unwrap();
        assert_eq!(meta.label, "real prompt here");
    }

    #[test]
    fn parse_custom_title_overrides_label() {
        let f = write_session(&[
            r#"{"type":"user","timestamp":"2025-01-01T00:00:00Z","message":{"content":"original prompt"}}"#,
            r#"{"type":"assistant","message":{"content":"response"}}"#,
            r#"{"type":"custom-title","customTitle":"my-cool-session","sessionId":"abc123"}"#,
        ]);
        let meta = parse_session(f.path()).unwrap();
        assert_eq!(meta.label, "my-cool-session");
    }

    #[test]
    fn parse_indexes_full_content() {
        // Ensure no artificial text budget cap
        let long_text = "x".repeat(3000);
        let mut lines = Vec::new();
        for i in 0..50 {
            lines.push(format!(
                r#"{{"type":"user","timestamp":"2025-01-01T00:00:00Z","message":{{"content":"msg{} {}"}}}}"#,
                i, long_text
            ));
            lines.push(format!(
                r#"{{"type":"assistant","message":{{"content":"reply{} {}"}}}}"#,
                i, long_text
            ));
        }
        // Add a unique marker at the end
        lines.push(r#"{"type":"user","message":{"content":"UNIQUE_MARKER_XYZ"}}"#.to_string());
        lines.push(r#"{"type":"assistant","message":{"content":"done"}}"#.to_string());

        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in &lines {
            writeln!(f, "{}", line).unwrap();
        }
        let meta = parse_session(f.path()).unwrap();
        assert!(meta.search_text.contains("UNIQUE_MARKER_XYZ"), "Content at end of session must be indexed");
    }

    #[test]
    fn extract_texts_string() {
        let v: Value = serde_json::json!("plain text");
        assert_eq!(extract_texts(&v), vec!["plain text"]);
    }

    #[test]
    fn extract_texts_array() {
        let v: Value = serde_json::json!([
            {"type": "text", "text": "first"},
            {"type": "image", "url": "..."},
            {"type": "text", "text": "second"},
        ]);
        assert_eq!(extract_texts(&v), vec!["first", "second"]);
    }
}
