//! Test harness: an isolated HOME with transcripts written in the JSONL shapes Claude Code
//! actually produces, and a fake `claude` executable that records how it was invoked.
#![allow(dead_code)]

use claude_resume::paths::Paths;
use claude_resume::store::Store;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub struct Sandbox {
    _root: TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub claude_dir: PathBuf,
    pub bin: PathBuf,
    pub log: PathBuf,
}

/// One recorded invocation of the fake `claude`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCall {
    pub cwd: PathBuf,
    pub args: String,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox {
    pub fn new() -> Self {
        let root_dir = tempfile::tempdir().unwrap();
        // Canonical path: macOS temp dirs live behind the /var → /private/var symlink, and the
        // fake claude reports the physical $PWD.
        let root = root_dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let claude_dir = home.join(".claude");
        let bin = root.join("bin");
        let log = root.join("claude-calls.log");
        fs::create_dir_all(claude_dir.join("projects")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("claude");
        fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s\\t%s\\n' \"$(pwd -P)\" \"$*\" >> \"$FAKE_CLAUDE_LOG\"\nexit \"${FAKE_CLAUDE_EXIT:-0}\"\n",
        )
        .unwrap();
        make_executable(&fake);
        Self {
            _root: root_dir,
            root,
            home,
            claude_dir,
            bin,
            log,
        }
    }

    pub fn projects(&self) -> PathBuf {
        self.claude_dir.join("projects")
    }

    pub fn paths(&self) -> Paths {
        Paths::under(&self.claude_dir)
    }

    /// A real directory to use as a session's working directory.
    pub fn workdir(&self, name: &str) -> PathBuf {
        let dir = self.root.join("work").join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn session(&self, sid: &str) -> SessionBuilder<'_> {
        SessionBuilder {
            sandbox: self,
            sid: sid.to_string(),
            cwd: self.workdir("proj").to_string_lossy().into_owned(),
            branch: "main".into(),
            lines: Vec::new(),
            clock: chrono::DateTime::parse_from_rfc3339("2026-09-01T10:00:00Z")
                .unwrap()
                .timestamp(),
            message_seq: 0,
        }
    }

    /// Open the sandbox index and sync it.
    pub fn store(&self) -> Store {
        let paths = self.paths();
        let mut store = Store::open(&paths.db).unwrap();
        let stats = store.sync(&paths.projects(), false).unwrap();
        assert!(stats.failed.is_empty(), "sync failures: {:?}", stats.failed);
        store
    }

    /// `claude-resume` with HOME, PATH, SHELL and the fake claude pointed into the sandbox.
    pub fn cmd(&self) -> assert_cmd::Command {
        let mut cmd = std::process::Command::new(bin());
        self.isolate(&mut cmd);
        assert_cmd::Command::from_std(cmd)
    }

    pub fn std_cmd(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(bin());
        self.isolate(&mut cmd);
        cmd
    }

    pub fn isolate(&self, cmd: &mut std::process::Command) {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cmd.env("HOME", &self.home)
            .env("PATH", path)
            .env("SHELL", "/bin/sh")
            .env("TZ", "UTC")
            .env("FAKE_CLAUDE_LOG", &self.log)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_RESUME_DB")
            .env_remove("CLAUDE_RESUME_MODELS_DIR")
            .env_remove("CLAUDE_RESUME_NO_SYNC")
            .current_dir(&self.root);
    }

    pub fn claude_calls(&self) -> Vec<ClaudeCall> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let (cwd, args) = l.split_once('\t').unwrap_or((l, ""));
                ClaudeCall {
                    cwd: PathBuf::from(cwd),
                    args: args.to_string(),
                }
            })
            .collect()
    }

    /// Every file under `projects/` with its bytes and mtime, to prove nothing was modified.
    pub fn snapshot_projects(&self) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
        let mut out = Vec::new();
        walk(&self.projects(), &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>, std::time::SystemTime)>) {
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push((
                p.clone(),
                fs::read(&p).unwrap(),
                fs::metadata(&p).unwrap().modified().unwrap(),
            ));
        }
    }
}

/// A stand-in for the embedding model, in the Hugging Face cache layout under `models`: a BERT
/// with eight dimensions, one layer and random weights, so semantic search runs end to end in
/// milliseconds without the real 133 MB download. Its vectors mean nothing: tests check which
/// sessions get embedded and when, while `tests/semantic.rs` checks ranking with the real model.
#[cfg(feature = "semantic")]
pub fn tiny_model(models: &Path) {
    use candle_core::{Device, Tensor};
    const HIDDEN: usize = 8;
    const INTERMEDIATE: usize = 16;
    const POSITIONS: usize = 512;
    let words = [
        "[PAD]",
        "[UNK]",
        "kafka",
        "consumer",
        "lag",
        "terraform",
        "state",
        "lock",
        "deploy",
    ];
    let vocab: serde_json::Map<String, Value> = words
        .iter()
        .enumerate()
        .map(|(id, word)| (word.to_string(), json!(id)))
        .collect();
    let repo = claude_resume::semantic::model_dir(models);
    let snapshot = repo.join("snapshots/tiny");
    fs::create_dir_all(&snapshot).unwrap();
    fs::create_dir_all(repo.join("refs")).unwrap();
    fs::write(repo.join("refs/main"), "tiny").unwrap();
    let config = json!({
        "vocab_size": words.len(), "hidden_size": HIDDEN, "num_hidden_layers": 1,
        "num_attention_heads": 2, "intermediate_size": INTERMEDIATE, "hidden_act": "gelu",
        "hidden_dropout_prob": 0.0, "max_position_embeddings": POSITIONS, "type_vocab_size": 2,
        "initializer_range": 0.02, "layer_norm_eps": 1e-12, "pad_token_id": 0
    });
    fs::write(snapshot.join("config.json"), config.to_string()).unwrap();
    let tokenizer = json!({
        "version": "1.0", "truncation": null, "padding": null, "added_tokens": [],
        "normalizer": {"type": "Lowercase"}, "pre_tokenizer": {"type": "Whitespace"},
        "post_processor": null, "decoder": null,
        "model": {"type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]"}
    });
    fs::write(snapshot.join("tokenizer.json"), tokenizer.to_string()).unwrap();

    let mut shapes = vec![
        (
            "embeddings.word_embeddings.weight".to_string(),
            vec![words.len(), HIDDEN],
        ),
        (
            "embeddings.position_embeddings.weight".into(),
            vec![POSITIONS, HIDDEN],
        ),
        (
            "embeddings.token_type_embeddings.weight".into(),
            vec![2, HIDDEN],
        ),
    ];
    let layer = "encoder.layer.0";
    for (name, out, input) in [
        ("attention.self.query", HIDDEN, HIDDEN),
        ("attention.self.key", HIDDEN, HIDDEN),
        ("attention.self.value", HIDDEN, HIDDEN),
        ("attention.output.dense", HIDDEN, HIDDEN),
        ("intermediate.dense", INTERMEDIATE, HIDDEN),
        ("output.dense", HIDDEN, INTERMEDIATE),
    ] {
        shapes.push((format!("{layer}.{name}.weight"), vec![out, input]));
        shapes.push((format!("{layer}.{name}.bias"), vec![out]));
    }
    for norm in [
        "embeddings.LayerNorm".to_string(),
        format!("{layer}.attention.output.LayerNorm"),
        format!("{layer}.output.LayerNorm"),
    ] {
        shapes.push((format!("{norm}.weight"), vec![HIDDEN]));
        shapes.push((format!("{norm}.bias"), vec![HIDDEN]));
    }
    let mut seed: u32 = 7;
    let weights: std::collections::HashMap<String, Tensor> = shapes
        .into_iter()
        .map(|(name, shape)| {
            let values: Vec<f32> = (0..shape.iter().product::<usize>())
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
                })
                .collect();
            (name, Tensor::from_vec(values, shape, &Device::Cpu).unwrap())
        })
        .collect();
    candle_core::safetensors::save(&weights, snapshot.join("model.safetensors")).unwrap();
}

/// The `claude-resume` binary cargo built for these tests.
pub fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_claude-resume"))
}

pub fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Builds a transcript line by line, in the shapes recent Claude Code versions write.
pub struct SessionBuilder<'a> {
    sandbox: &'a Sandbox,
    pub sid: String,
    cwd: String,
    branch: String,
    lines: Vec<Value>,
    clock: i64,
    message_seq: u32,
}

impl SessionBuilder<'_> {
    pub fn cwd(mut self, cwd: &Path) -> Self {
        self.cwd = cwd.to_string_lossy().into_owned();
        self
    }

    pub fn branch(mut self, branch: &str) -> Self {
        self.branch = branch.into();
        self
    }

    /// Timestamp of the next line (each line advances the clock by a minute).
    pub fn at(mut self, rfc3339: &str) -> Self {
        self.clock = chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp();
        self
    }

    fn stamp(&mut self) -> String {
        let t = chrono::DateTime::from_timestamp(self.clock, 0).unwrap();
        self.clock += 60;
        t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    }

    fn envelope(&mut self, kind: &str, message: Value) -> Value {
        json!({
            "parentUuid": null, "isSidechain": false, "userType": "external",
            "cwd": self.cwd, "sessionId": self.sid, "version": "2.1.286", "gitBranch": self.branch,
            "type": kind, "message": message, "uuid": format!("u{}", self.lines.len()), "timestamp": self.stamp(),
        })
    }

    pub fn user(mut self, text: &str) -> Self {
        let line = self.envelope("user", json!({"role": "user", "content": text}));
        self.lines.push(line);
        self
    }

    pub fn assistant(mut self, text: &str) -> Self {
        self.message_seq += 1;
        let id = format!("msg_{:03}", self.message_seq);
        let line = self.envelope(
            "assistant",
            json!({"id": id, "type": "message", "role": "assistant", "model": "claude-opus-5-5", "content": [{"type": "text", "text": text}]}),
        );
        self.lines.push(line);
        self
    }

    /// An assistant tool call and the user turn carrying its result (with the large
    /// `toolUseResult` duplicate Claude Code stores alongside).
    pub fn tool(mut self, name: &str, input: Value, result: &str) -> Self {
        self.message_seq += 1;
        let id = format!("toolu_{:03}", self.message_seq);
        let call = self.envelope(
            "assistant",
            json!({"id": format!("msg_{:03}", self.message_seq), "role": "assistant", "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}),
        );
        let mut res = self.envelope(
            "user",
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": result}]}),
        );
        res["toolUseResult"] = json!({"stdout": result, "stderr": "", "interrupted": false});
        self.lines.push(call);
        self.lines.push(res);
        self
    }

    pub fn command(self, name: &str, args: &str) -> Self {
        let text = format!(
            "<command-name>{name}</command-name>\n            <command-message>{}</command-message>\n            <command-args>{args}</command-args>",
            name.trim_start_matches('/')
        );
        self.user(&text)
    }

    pub fn meta(mut self, text: &str) -> Self {
        let mut line = self.envelope("user", json!({"role": "user", "content": text}));
        line["isMeta"] = json!(true);
        self.lines.push(line);
        self
    }

    pub fn queued(mut self, text: &str) -> Self {
        let ts = self.stamp();
        self.lines.push(json!({
            "type": "attachment", "cwd": self.cwd, "sessionId": self.sid, "timestamp": ts,
            "attachment": {"type": "queued_command", "commandMode": "prompt", "prompt": text, "origin": {"kind": "human"}},
        }));
        self
    }

    pub fn custom_title(mut self, title: &str) -> Self {
        self.lines
            .push(json!({"type": "custom-title", "customTitle": title, "sessionId": self.sid}));
        self
    }

    pub fn ai_title(mut self, title: &str) -> Self {
        self.lines
            .push(json!({"type": "ai-title", "aiTitle": title, "sessionId": self.sid}));
        self
    }

    pub fn pr_link(mut self, url: &str) -> Self {
        let ts = self.stamp();
        self.lines.push(json!({"type": "pr-link", "prNumber": 1, "prUrl": url, "prRepository": "o/r", "sessionId": self.sid, "timestamp": ts}));
        self
    }

    /// Metadata-only lines Claude Code writes before the first prompt.
    pub fn preamble(mut self) -> Self {
        self.lines.push(
            json!({"type": "permission-mode", "permissionMode": "default", "sessionId": self.sid}),
        );
        self.lines.push(json!({"type": "file-history-snapshot", "messageId": "m0", "snapshot": {"trackedFileBackups": {}}, "isSnapshotUpdate": false}));
        self
    }

    pub fn raw(mut self, line: &str) -> Self {
        self.lines
            .push(serde_json::from_str(line).unwrap_or(Value::String(line.into())));
        self
    }

    /// Where Claude Code would store this session: `projects/<cwd with / and . as ->/<sid>.jsonl`.
    pub fn path(&self) -> PathBuf {
        let encoded: String = self
            .cwd
            .chars()
            .map(|c| if c == '/' || c == '.' { '-' } else { c })
            .collect();
        self.sandbox
            .projects()
            .join(encoded)
            .join(format!("{}.jsonl", self.sid))
    }

    pub fn write(self) -> PathBuf {
        let path = self.path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = self
            .lines
            .iter()
            .map(|l| match l {
                Value::String(s) => format!("{s}\n"),
                other => format!("{other}\n"),
            })
            .collect();
        fs::write(&path, body).unwrap();
        path
    }
}

/// Append raw JSONL lines to an existing transcript.
pub fn append(path: &Path, lines: &[Value]) {
    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}
