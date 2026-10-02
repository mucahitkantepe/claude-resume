//! The `claude-resume` command line, end to end, in an isolated HOME with a fake `claude`.

mod common;

use common::Sandbox;
use predicates::prelude::*;
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::process::Stdio;

fn sandbox_with_sessions() -> Sandbox {
    let sb = Sandbox::new();
    sb.session("11111111-aaaa-4000-8000-000000000001")
        .at("2026-09-20T10:00:00Z")
        .cwd(&sb.workdir("shop"))
        .branch("fix/lag")
        .user("why is the kafka consumer lag growing on prod?")
        .assistant("The consumer group keeps rebalancing.")
        .ai_title("Kafka consumer lag on prod")
        .write();
    sb.session("22222222-bbbb-4000-8000-000000000002")
        .at("2026-09-10T10:00:00Z")
        .cwd(&sb.workdir("infra"))
        .user("terraform apply fails with a state lock")
        .assistant("Use terraform force-unlock; kafka is unrelated.")
        .write();
    sb
}

const LAG: &str = "11111111-aaaa-4000-8000-000000000001";
const TF: &str = "22222222-bbbb-4000-8000-000000000002";

#[test]
fn search_prints_titles_snippets_and_resume_commands() {
    let sb = sandbox_with_sessions();
    let cwd = sb.workdir("shop");
    sb.cmd()
        .args(["search", "consumer", "lag"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "1 session matches \"consumer lag\"",
        ))
        .stdout(predicate::str::contains("1. Kafka consumer lag on prod"))
        .stdout(predicate::str::contains("shop · fix/lag · 1 prompt"))
        .stdout(predicate::str::contains(
            "you › why is the kafka **consumer lag** growing on prod?",
        ))
        .stdout(predicate::str::contains(format!("id: {LAG}")))
        .stdout(predicate::str::contains(format!(
            "resume: cd {} && claude --resume {LAG}",
            cwd.display()
        )));
}

#[test]
fn search_limits_results_and_says_so() {
    let sb = sandbox_with_sessions();
    let out = sb
        .cmd()
        .args(["search", "kafka", "-n", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.starts_with("2 sessions match \"kafka\" (showing 1):"),
        "{out}"
    );
    assert!(out.contains("1. Kafka consumer lag on prod") && !out.contains("2. "));
}

#[test]
fn search_json_is_machine_readable() {
    let sb = sandbox_with_sessions();
    let out = sb
        .cmd()
        .args(["search", "kafka", "--json", "-m", "exact"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["query"], "kafka");
    assert_eq!(v["total"], 2);
    let first = &v["results"][0];
    assert_eq!(first["sid"], LAG);
    assert_eq!(first["title"], "Kafka consumer lag on prod");
    assert_eq!(first["project"], "shop");
    assert_eq!(first["branch"], "fix/lag");
    assert_eq!(first["match"], "title");
    assert_eq!(first["prompt_count"], 1);
    assert!(
        first["resume_command"]
            .as_str()
            .unwrap()
            .ends_with(&format!("claude --resume {LAG}"))
    );
    assert_eq!(first["snippets"][0]["field"], "prompt");
    assert!(
        first["snippets"][0]["text"]
            .as_str()
            .unwrap()
            .contains("**kafka**")
    );
    assert_eq!(v["results"][1]["match"], "content");
    assert_eq!(v["results"][1]["snippets"][0]["field"], "reply");
}

#[test]
fn search_with_no_matches_is_not_an_error() {
    let sb = sandbox_with_sessions();
    sb.cmd()
        .args(["search", "zookeeper"])
        .assert()
        .success()
        .stdout("No sessions match \"zookeeper\".\n");
}

#[test]
fn search_rejects_unknown_modes_instead_of_silently_falling_back() {
    let sb = sandbox_with_sessions();
    sb.cmd()
        .args(["search", "kafka", "-m", "exat"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid value 'exat'"));
}

#[test]
fn search_syncs_first_unless_told_not_to() {
    let sb = sandbox_with_sessions();
    sb.cmd().args(["search", "kafka"]).assert().success();
    sb.session("33333333-cccc-4000-8000-000000000003")
        .user("a brand new zookeeper question")
        .assistant("ok")
        .write();
    sb.cmd()
        .args(["search", "zookeeper", "--no-sync"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No sessions match"));
    sb.cmd()
        .args(["search", "zookeeper"])
        .env("CLAUDE_RESUME_NO_SYNC", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("No sessions match"));
    sb.cmd()
        .args(["search", "zookeeper"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 session matches"));
}

#[test]
fn closing_the_pipe_early_is_not_a_crash() {
    // 0.4 panicked with "failed printing to stdout: Broken pipe" under `| head`.
    let sb = Sandbox::new();
    for i in 0..300 {
        sb.session(&format!("{i:08}-0000-4000-8000-000000000000"))
            .user(&format!(
                "kafka question number {i} {}",
                "padding ".repeat(50)
            ))
            .assistant("ok")
            .write();
    }
    sb.cmd().args(["sync", "--quiet"]).assert().success();
    // ~300 results × ~600 bytes is far more than a pipe buffer holds, so writes hit EPIPE.
    let mut child = sb
        .std_cmd()
        .args(["search", "kafka", "-n", "300"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = [0u8; 16];
    child.stdout.take().unwrap().read_exact(&mut first).unwrap(); // then drop the read end
    let status = child.wait().unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(status.success(), "exit {status:?}, stderr: {err}");
    assert!(!err.contains("panicked"), "{err}");
}

#[test]
fn resume_runs_claude_in_the_sessions_directory() {
    let sb = sandbox_with_sessions();
    sb.cmd()
        .args(["resume", LAG])
        .assert()
        .success()
        .stderr(predicate::str::contains("Resuming"));
    let calls = sb.claude_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].cwd, sb.workdir("shop"));
    assert_eq!(calls[0].args, format!("--resume {LAG}"));
}

#[test]
fn resume_accepts_a_unique_id_prefix_and_passes_claudes_exit_code_through() {
    let sb = sandbox_with_sessions();
    sb.cmd()
        .args(["resume", "2222"])
        .env("FAKE_CLAUDE_EXIT", "3")
        .assert()
        .code(3);
    assert_eq!(sb.claude_calls()[0].args, format!("--resume {TF}"));
    sb.cmd()
        .args(["resume", "nope-nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no session matches"));
}

#[test]
fn resume_falls_back_to_the_current_directory_when_the_original_is_gone() {
    let sb = sandbox_with_sessions();
    fs::remove_dir_all(sb.workdir("infra")).unwrap();
    sb.cmd()
        .args(["resume", TF])
        .assert()
        .success()
        .stderr(predicate::str::contains("no longer exists"));
    assert_eq!(sb.claude_calls()[0].cwd, sb.root);
}

#[test]
fn resume_print_shows_the_command_without_running_it() {
    let sb = sandbox_with_sessions();
    sb.cmd()
        .args(["resume", LAG, "--print"])
        .assert()
        .success()
        .stdout(format!(
            "cd {} && claude --resume {LAG}\n",
            sb.workdir("shop").display()
        ));
    assert!(sb.claude_calls().is_empty());
}

#[test]
fn resume_survives_an_rc_file_that_changes_directory() {
    let sb = sandbox_with_sessions();
    // An interactive shell sources $ENV (sh) / ~/.zshrc; some of those `cd` somewhere.
    let rc = sb.root.join("rc.sh");
    fs::write(&rc, format!("cd {}\n", sb.root.display())).unwrap();
    sb.cmd()
        .args(["resume", LAG])
        .env("ENV", &rc)
        .assert()
        .success();
    assert_eq!(sb.claude_calls()[0].cwd, sb.workdir("shop"));
}

#[test]
fn sync_reports_on_stderr_and_keeps_stdout_clean() {
    // Progress and summaries go to stderr, so scripts can rely on stdout.
    let sb = sandbox_with_sessions();
    sb.cmd()
        .arg("sync")
        .assert()
        .success()
        .stdout("")
        .stderr(predicate::str::contains(
            "Indexed 2 sessions (2 updated, 0 unchanged, 0 removed)",
        ));
    sb.cmd()
        .arg("sync")
        .assert()
        .success()
        .stdout("")
        .stderr("");
    sb.cmd()
        .args(["sync", "--force", "--quiet"])
        .assert()
        .success()
        .stdout("")
        .stderr("");
}

#[test]
fn respects_claude_config_dir() {
    let sb = Sandbox::new();
    let other = sb.root.join("other-config");
    fs::create_dir_all(other.join("projects/-x")).unwrap();
    fs::write(
        other.join("projects/-x/44444444-dddd-4000-8000-000000000004.jsonl"),
        json!({"type": "user", "cwd": "/x", "message": {"content": "relocated config dir session"}}).to_string(),
    )
    .unwrap();
    sb.cmd()
        .args(["search", "relocated"])
        .env("CLAUDE_CONFIG_DIR", &other)
        .assert()
        .success()
        .stdout(predicate::str::contains("1 session matches"));
    assert!(other.join("claude-resume.db").exists());
    assert!(!sb.claude_dir.join("claude-resume.db").exists());
}

#[test]
fn picker_with_no_sessions_says_so() {
    let sb = Sandbox::new();
    sb.cmd()
        .assert()
        .failure()
        .stderr(predicate::str::contains("no sessions found"));
}

#[test]
fn version_matches_the_manifest() {
    let sb = Sandbox::new();
    sb.cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(format!("claude-resume {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn embed_never_downloads_without_consent() {
    let sb = sandbox_with_sessions();
    if !cfg!(feature = "semantic") {
        for args in [&["embed"][..], &["search", "deploy", "-m", "semantic"]] {
            sb.cmd()
                .args(args)
                .assert()
                .failure()
                .stderr(predicate::str::contains("without the `semantic` feature"));
        }
        return;
    }
    // stdin is not a terminal here, so there is nobody to ask: it must refuse, not download.
    sb.cmd()
        .arg("embed")
        .write_stdin("y\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("embed --yes").and(predicate::str::contains("Cancelled")));
    assert!(!sb.claude_dir.join("models").exists());
    sb.cmd()
        .args(["search", "deploy", "-m", "semantic"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("claude-resume embed"));
    assert!(!sb.claude_dir.join("models").exists());
}

mod init_and_uninstall {
    use super::*;

    const LEGACY_HOOK: &str = "/Users/me/.local/bin/claude-resume sync &";

    fn write_settings(sb: &Sandbox, value: Value) {
        fs::write(
            sb.claude_dir.join("settings.json"),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();
    }

    fn read_settings(sb: &Sandbox) -> (String, Value) {
        let raw = fs::read_to_string(sb.claude_dir.join("settings.json")).unwrap();
        let v = serde_json::from_str(&raw).unwrap();
        (raw, v)
    }

    #[test]
    fn init_keeps_sessions_and_removes_what_older_versions_added() {
        let sb = Sandbox::new();
        write_settings(
            &sb,
            json!({
                "$schema": "https://json.schemastore.org/claude-code-settings.json",
                "model": "opus",
                "hooks": {"SessionStart": [
                    {"matcher": "", "hooks": [{"type": "command", "command": LEGACY_HOOK, "timeout": 10}]},
                    {"hooks": [{"type": "command", "command": "bash ~/.claude/hooks/mine.sh"}]}
                ]},
                "enabledPlugins": {"other@market": true},
                "extraKnownMarketplaces": {"claude-resume": {"source": {"source": "github", "repo": "mucahitkantepe/claude-resume"}}}
            }),
        );
        sb.cmd()
            .arg("init")
            .assert()
            .success()
            .stderr(predicate::str::contains(
                "cleanupPeriodDays: unset (30) → 99999",
            ))
            .stderr(predicate::str::contains("removed 1 SessionStart hook"))
            .stderr(predicate::str::contains("removed the plugin marketplace"))
            .stderr(predicate::str::contains("backed up"));

        let (raw, v) = read_settings(&sb);
        assert_eq!(v["cleanupPeriodDays"], 99_999);
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "bash ~/.claude/hooks/mine.sh"
        );
        assert_eq!(v["enabledPlugins"], json!({"other@market": true}));
        assert!(v.get("extraKnownMarketplaces").is_none());
        let pos = |k: &str| raw.find(k).unwrap();
        assert!(
            pos("$schema") < pos("\"model\"") && pos("\"model\"") < pos("\"hooks\""),
            "key order kept:\n{raw}"
        );
        assert!(
            sb.claude_dir
                .join("settings.json.claude-resume.bak")
                .exists()
        );

        assert!(
            sb.claude_calls().is_empty(),
            "init never runs Claude Code's plugin commands"
        );

        // Second run: nothing left to change.
        sb.cmd()
            .arg("init")
            .assert()
            .success()
            .stderr(predicate::str::contains("already 99999"));
        assert_eq!(read_settings(&sb).0, raw);
    }

    #[test]
    fn init_refuses_to_touch_broken_settings() {
        let sb = Sandbox::new();
        fs::write(
            sb.claude_dir.join("settings.json"),
            "{ \"model\": \"opus\", }",
        )
        .unwrap();
        sb.cmd()
            .arg("init")
            .assert()
            .failure()
            .stderr(predicate::str::contains("not valid JSON"));
        assert_eq!(
            fs::read_to_string(sb.claude_dir.join("settings.json")).unwrap(),
            "{ \"model\": \"opus\", }"
        );
    }

    #[test]
    fn uninstall_leaves_a_shared_model_cache_alone() {
        let sb = sandbox_with_sessions();
        let shared = sb.root.join("hf-cache");
        let model = claude_resume::semantic::model_dir(&shared);
        fs::create_dir_all(&model).unwrap();
        sb.cmd()
            .arg("uninstall")
            .env("CLAUDE_RESUME_MODELS_DIR", &shared)
            .assert()
            .success()
            .stderr(predicate::str::contains("left the embedding model"));
        assert!(model.exists());
    }

    #[test]
    fn uninstall_removes_what_claude_resume_added_and_nothing_else() {
        let sb = sandbox_with_sessions();
        sb.cmd().args(["sync", "--quiet"]).assert().success();
        write_settings(
            &sb,
            json!({
                "cleanupPeriodDays": 99_999,
                "hooks": {"SessionStart": [{"matcher": "", "hooks": [{"type": "command", "command": LEGACY_HOOK}]}]},
                "extraKnownMarketplaces": {"claude-resume": {"source": {"source": "github", "repo": "mucahitkantepe/claude-resume"}}, "other": {"source": {"source": "github", "repo": "o/r"}}}
            }),
        );
        fs::write(sb.claude_dir.join("recall.db"), "legacy index").unwrap();
        fs::write(
            sb.claude_dir.join("sessions-search-index.tsv"),
            "another tool's index",
        )
        .unwrap();
        for leftover in ["claude-resume.log", "claude-resume.embed.lock"] {
            fs::write(sb.claude_dir.join(leftover), "").unwrap();
        }
        let models = sb.claude_dir.join("models");
        let model = claude_resume::semantic::model_dir(&models);
        fs::create_dir_all(model.join("snapshots/abc")).unwrap();
        fs::write(model.join("snapshots/abc/model.safetensors"), "weights").unwrap();
        let other_model = models.join("models--someone--else");
        fs::create_dir_all(&other_model).unwrap();
        let transcripts = sb.snapshot_projects();

        sb.cmd()
            .arg("uninstall")
            .assert()
            .success()
            .stderr(predicate::str::contains("removed the search index"));

        let v = read_settings(&sb).1;
        assert_eq!(
            v,
            json!({"cleanupPeriodDays": 99_999, "extraKnownMarketplaces": {"other": {"source": {"source": "github", "repo": "o/r"}}}})
        );
        let left: Vec<String> = fs::read_dir(&sb.claude_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("claude-resume") || name.starts_with("recall"))
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
        assert!(!model.exists(), "the downloaded model is removed");
        assert!(other_model.exists(), "other tools' models are kept");
        assert!(
            sb.claude_dir.join("sessions-search-index.tsv").exists(),
            "0.4 deleted another tool's index"
        );
        assert_eq!(sb.snapshot_projects(), transcripts);
        assert!(sb.claude_calls().is_empty());
    }
}
