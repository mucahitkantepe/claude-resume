//! Indexing: what gets indexed, incremental updates, and that transcripts are never modified.

mod common;

use claude_resume::semantic;
use claude_resume::store::{FileLock, Progress, Store};
use common::{Sandbox, append};
use serde_json::json;
use std::fs;
use std::time::{Duration, SystemTime};

#[test]
fn indexes_conversations_and_hides_empty_sessions_without_touching_any_file() {
    let sb = Sandbox::new();
    sb.session("real")
        .preamble()
        .user("why is the kafka consumer lag growing")
        .assistant("It is rebalancing.")
        .write();
    sb.session("just-exit")
        .preamble()
        .command("/exit", "")
        .write();
    sb.session("metadata-only").preamble().write();
    sb.session("empty-file").write();
    let before = sb.snapshot_projects();

    let paths = sb.paths();
    let mut store = Store::open(&paths.db).unwrap();
    let stats = store.sync(&paths.projects(), false).unwrap();

    assert_eq!((stats.sessions, stats.parsed, stats.empty), (1, 1, 3));
    assert!(stats.failed.is_empty());
    let sessions = store.sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].sid, "real");
    assert_eq!(
        sb.snapshot_projects(),
        before,
        "sync must never modify, delete or touch transcripts"
    );
}

#[test]
fn incremental_sync_parses_only_new_and_changed_files() {
    let sb = Sandbox::new();
    let a = sb.session("a").user("alpha prompt").assistant("ok").write();
    let b = sb.session("b").user("bravo prompt").assistant("ok").write();
    let paths = sb.paths();
    let mut store = Store::open(&paths.db).unwrap();
    let first = store.sync(&paths.projects(), false).unwrap();
    assert_eq!((first.parsed, first.unchanged), (2, 0));

    let again = store.sync(&paths.projects(), false).unwrap();
    assert_eq!(
        (again.parsed, again.unchanged, again.removed),
        (0, 2, 0),
        "nothing changed"
    );

    append(
        &a,
        &[
            json!({"type": "user", "timestamp": "2026-09-02T10:00:00Z", "message": {"content": "a follow-up about zookeeper"}}),
        ],
    );
    sb.session("c")
        .user("charlie prompt")
        .assistant("ok")
        .write();
    fs::remove_file(&b).unwrap();
    let changed = store.sync(&paths.projects(), false).unwrap();
    assert_eq!(
        (
            changed.parsed,
            changed.unchanged,
            changed.removed,
            changed.sessions
        ),
        (2, 0, 1, 2)
    );

    let a_row = store.find("a").unwrap().unwrap();
    assert_eq!(a_row.prompt_count, 2);
    assert!(
        store
            .content(a_row.id)
            .unwrap()
            .unwrap()
            .prompts
            .contains("zookeeper")
    );
    assert!(
        store.find("b").unwrap().is_none(),
        "deleted transcript dropped from the index"
    );
}

#[test]
fn detects_a_rewrite_within_the_same_second() {
    // 0.4 compared whole-second mtimes, so a change in the second it was indexed was missed.
    let sb = Sandbox::new();
    let path = sb
        .session("s")
        .user("first version")
        .assistant("ok")
        .write();
    let paths = sb.paths();
    let mut store = Store::open(&paths.db).unwrap();
    store.sync(&paths.projects(), false).unwrap();

    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let body = fs::read_to_string(&path)
        .unwrap()
        .replace("first version", "other version");
    fs::write(&path, body).unwrap(); // same length
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime + Duration::from_millis(1))
        .unwrap();

    let stats = store.sync(&paths.projects(), false).unwrap();
    assert_eq!(stats.parsed, 1);
    let row = store.find("s").unwrap().unwrap();
    assert!(
        store
            .content(row.id)
            .unwrap()
            .unwrap()
            .prompts
            .contains("other version")
    );
}

#[test]
fn force_rebuilds_everything() {
    let sb = Sandbox::new();
    sb.session("a").user("one").assistant("ok").write();
    sb.session("b").user("two").assistant("ok").write();
    let paths = sb.paths();
    let mut store = Store::open(&paths.db).unwrap();
    store.sync(&paths.projects(), false).unwrap();
    let forced = store.sync(&paths.projects(), true).unwrap();
    assert_eq!(
        (forced.parsed, forced.unchanged, forced.sessions),
        (2, 0, 2)
    );
}

#[test]
fn a_session_that_gains_a_conversation_is_indexed_later() {
    let sb = Sandbox::new();
    let path = sb.session("late").preamble().write();
    let paths = sb.paths();
    let mut store = Store::open(&paths.db).unwrap();
    assert_eq!(store.sync(&paths.projects(), false).unwrap().empty, 1);
    assert_eq!(
        store.sync(&paths.projects(), false).unwrap().parsed,
        0,
        "empty files are remembered, not re-read"
    );
    append(
        &path,
        &[json!({"type": "user", "message": {"content": "now there is a prompt"}})],
    );
    let stats = store.sync(&paths.projects(), false).unwrap();
    assert_eq!((stats.parsed, stats.sessions), (1, 1));
}

#[test]
fn ignores_subagent_transcripts_and_backups() {
    let sb = Sandbox::new();
    let main = sb
        .session("main")
        .user("main session prompt")
        .assistant("ok")
        .write();
    let dir = main.parent().unwrap();
    fs::create_dir_all(dir.join("main/subagents")).unwrap();
    fs::write(
        dir.join("main/subagents/agent-1.jsonl"),
        r#"{"type":"user","message":{"content":"subagent work"}}"#,
    )
    .unwrap();
    fs::copy(&main, dir.join("main.jsonl.bak")).unwrap();
    let store = sb.store();
    let sids: Vec<String> = store
        .sessions()
        .unwrap()
        .into_iter()
        .map(|s| s.sid)
        .collect();
    assert_eq!(sids, ["main"]);
}

#[test]
fn same_session_id_in_two_projects_keeps_the_newest_copy() {
    let sb = Sandbox::new();
    let old = sb
        .session("dup")
        .cwd(&sb.workdir("old"))
        .user("old copy")
        .assistant("ok")
        .write();
    let new = sb
        .session("dup")
        .cwd(&sb.workdir("new"))
        .user("new copy")
        .assistant("ok")
        .write();
    let t = SystemTime::now();
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(t - Duration::from_secs(3600))
        .unwrap();
    fs::File::options()
        .write(true)
        .open(&new)
        .unwrap()
        .set_modified(t)
        .unwrap();
    let store = sb.store();
    let row = store.find("dup").unwrap().unwrap();
    assert!(row.cwd.ends_with("/new"));
    assert_eq!(store.len().unwrap(), 1);
}

#[test]
fn sessions_are_ordered_by_last_activity_not_file_date() {
    // 0.4 sorted by a YYYY-MM-DD string, so sessions from the same day came out in random order.
    let sb = Sandbox::new();
    sb.session("morning")
        .at("2026-09-01T08:00:00Z")
        .user("morning work")
        .assistant("ok")
        .write();
    sb.session("evening")
        .at("2026-09-01T20:00:00Z")
        .user("evening work")
        .assistant("ok")
        .write();
    sb.session("noon")
        .at("2026-09-01T12:00:00Z")
        .user("noon work")
        .assistant("ok")
        .write();
    sb.session("yesterday")
        .at("2026-08-31T23:00:00Z")
        .user("late night")
        .assistant("ok")
        .write();
    let sids: Vec<String> = sb
        .store()
        .sessions()
        .unwrap()
        .into_iter()
        .map(|s| s.sid)
        .collect();
    assert_eq!(sids, ["evening", "noon", "morning", "yesterday"]);
}

#[test]
fn titles_and_metadata_come_from_the_transcript() {
    let sb = Sandbox::new();
    sb.session("renamed")
        .branch("feat/lag")
        .user("first prompt")
        .assistant("ok")
        .ai_title("Generated title")
        .custom_title("My name for it")
        .pr_link("https://github.com/o/r/pull/42")
        .write();
    sb.session("ai")
        .user("<command-name>/clear</command-name>")
        .user("investigate the flaky test")
        .assistant("ok")
        .ai_title("Investigate flaky test")
        .write();
    sb.session("plain")
        .command("/model", "")
        .user("refactor the parser")
        .assistant("ok")
        .write();
    let store = sb.store();
    let renamed = store.find("renamed").unwrap().unwrap();
    assert_eq!(renamed.title, "My name for it");
    assert_eq!(renamed.branch, "feat/lag");
    assert_eq!(renamed.project, "proj");
    assert_eq!(renamed.pr_links, ["https://github.com/o/r/pull/42"]);
    assert_eq!(
        store.find("ai").unwrap().unwrap().title,
        "Investigate flaky test"
    );
    let plain = store.find("plain").unwrap().unwrap();
    assert_eq!(
        plain.title, "refactor the parser",
        "bare slash commands are not titles"
    );
    assert_eq!(plain.preview, ["/model", "refactor the parser"]);
}

#[test]
fn id_prefix_lookup_must_be_unique() {
    let sb = Sandbox::new();
    sb.session("abcd1111").user("one").assistant("ok").write();
    sb.session("abcd2222").user("two").assistant("ok").write();
    let store = sb.store();
    assert_eq!(store.find("abcd1").unwrap().unwrap().sid, "abcd1111");
    assert!(store.find("abcd").unwrap().is_none(), "ambiguous prefix");
    assert!(store.find("abc").unwrap().is_none(), "too short");
    assert!(store.find("zzzz").unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn unreadable_transcript_is_reported_and_its_previous_entry_kept() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    let path = sb
        .session("locked")
        .user("visible before")
        .assistant("ok")
        .write();
    let paths = sb.paths();
    let mut store = Store::open(&paths.db).unwrap();
    store.sync(&paths.projects(), false).unwrap();
    append(
        &path,
        &[json!({"type": "user", "message": {"content": "more"}})],
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let stats = store.sync(&paths.projects(), false).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    if stats.failed.is_empty() {
        return; // running as root: permissions aren't enforced
    }
    assert_eq!(stats.failed.len(), 1);
    assert!(
        store.find("locked").unwrap().is_some(),
        "previous entry kept"
    );
    assert!(path.exists());
}

#[test]
fn concurrent_syncs_from_several_processes_agree() {
    let sb = Sandbox::new();
    for i in 0..40 {
        sb.session(&format!("s{i:02}"))
            .user(&format!("prompt number {i} about kafka"))
            .assistant("ok")
            .write();
    }
    let children: Vec<_> = (0..4)
        .map(|_| sb.std_cmd().args(["sync", "--quiet"]).spawn().unwrap())
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let store = Store::open(&sb.paths().db).unwrap();
    assert_eq!(store.len().unwrap(), 40);
    assert_eq!(
        store.fts_ids("\"kafka\"").unwrap().len(),
        40,
        "full-text index consistent with the table"
    );
}

#[test]
fn embeddings_survive_reindexing_and_go_stale_when_the_text_changes() {
    // 0.4 stored vectors in the sessions row, so any change to a session wiped its embedding.
    let sb = Sandbox::new();
    let path = sb
        .session("e")
        .user("deploy the terraform stack")
        .assistant("ok")
        .write();
    sb.session("f")
        .user("bake sourdough")
        .assistant("ok")
        .write();
    let paths = sb.paths();
    let mut store = Store::open(&paths.db).unwrap();
    store.sync(&paths.projects(), false).unwrap();
    for (sid, text, hash) in semantic::stale(&store).unwrap() {
        assert!(text.contains(if sid == "e" { "terraform" } else { "sourdough" }));
        store
            .put_embedding(
                &sid,
                semantic::MODEL_KEY,
                &hash,
                &semantic::to_bytes(&[1.0, 0.0]),
            )
            .unwrap();
    }
    assert!(semantic::stale(&store).unwrap().is_empty());

    append(
        &path,
        &[
            json!({"type": "assistant", "message": {"id": "m9", "content": [{"type": "text", "text": "a reply does not change the embedded text"}]}}),
        ],
    );
    store.sync(&paths.projects(), false).unwrap();
    assert_eq!(
        store.embeddings(semantic::MODEL_KEY).unwrap().len(),
        2,
        "vectors kept across re-indexing"
    );
    assert!(semantic::stale(&store).unwrap().is_empty());

    append(
        &path,
        &[json!({"type": "user", "message": {"content": "now also set up monitoring"}})],
    );
    store.sync(&paths.projects(), false).unwrap();
    let stale: Vec<String> = semantic::stale(&store)
        .unwrap()
        .into_iter()
        .map(|(sid, ..)| sid)
        .collect();
    assert_eq!(stale, ["e"], "a new prompt changes the embedded text");

    fs::remove_file(&path).unwrap();
    store.sync(&paths.projects(), false).unwrap();
    assert_eq!(
        store.embeddings(semantic::MODEL_KEY).unwrap().len(),
        1,
        "vectors of deleted sessions are not returned"
    );
}

#[test]
fn many_processes_opening_a_fresh_index_at_once_all_succeed() {
    // Regression: concurrent first opens raced on switching the new database to WAL and one of
    // them failed with "database is locked" (seen as a background sync that silently did nothing).
    for _ in 0..5 {
        let sb = Sandbox::new();
        sb.session("s").user("hello").assistant("ok").write();
        let children: Vec<_> = (0..8)
            .map(|i| {
                let args: &[&str] = if i % 2 == 0 {
                    &["sync", "--quiet"]
                } else {
                    &["search", "hello", "--no-sync"]
                };
                sb.std_cmd()
                    .args(args)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in children {
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

#[test]
fn reading_the_index_never_waits_for_a_sync_in_progress() {
    // A sync holds the write lock while it parses (seconds on a first full index). Searches that
    // skip syncing must not queue behind it; 0.5 drafts took the write lock on every open.
    let sb = Sandbox::new();
    sb.session("s").user("hello kafka").assistant("ok").write();
    sb.cmd().args(["sync", "--quiet"]).assert().success();

    let writer = rusqlite::Connection::open(sb.paths().db).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = std::time::Instant::now();
    sb.cmd()
        .args(["search", "kafka", "--no-sync"])
        .timeout(Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicates::str::contains("hello kafka"));
    assert!(started.elapsed() < Duration::from_secs(5));
    writer.execute_batch("COMMIT").unwrap();
}

#[test]
fn an_interrupted_sync_keeps_what_it_had_finished() {
    // A first index of a long history takes a while; Ctrl-C (or a crash) used to throw all of
    // it away, because the whole sync was one transaction.
    let sb = Sandbox::new();
    for i in 0..250 {
        sb.session(&format!("s{i:03}"))
            .user(&format!("prompt {i}"))
            .assistant("ok")
            .write();
    }
    let paths = sb.paths();
    let mut store = Store::open(&paths.db).unwrap();
    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.sync_with_progress(&paths.projects(), false, |p| {
            if p == (Progress::Parsed {
                done: 180,
                total: 250,
            }) {
                panic!("interrupted");
            }
        })
    }));
    assert!(interrupted.is_err());
    drop(store);

    let mut store = Store::open(&paths.db).unwrap();
    let kept = store.len().unwrap();
    assert!((100..180).contains(&kept), "kept {kept}");
    let stats = store.sync(&paths.projects(), false).unwrap();
    assert_eq!((stats.parsed, stats.unchanged), (250 - kept, kept));
    assert_eq!(store.len().unwrap(), 250);
}

#[test]
fn a_sync_waits_for_one_already_running_and_says_so() {
    let sb = Sandbox::new();
    sb.session("s").user("hello").assistant("ok").write();
    let paths = sb.paths();
    Store::open(&paths.db).unwrap();
    let running = FileLock::acquire(&paths.db.with_extension("lock")).unwrap();
    let (waiting, told) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let mut events = Vec::new();
        let mut store = Store::open(&paths.db).unwrap();
        store
            .sync_with_progress(&paths.projects(), false, |p| {
                if p == Progress::Waiting {
                    let _ = waiting.send(());
                }
                events.push(p)
            })
            .unwrap();
        events
    });
    // Released only once it is waiting: on a busy machine it may take a while to get there.
    told.recv_timeout(Duration::from_secs(60))
        .expect("never said it was waiting");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!waiter.is_finished(), "waits for the running sync");
    drop(running);
    let events = waiter.join().unwrap();
    assert_eq!(
        events,
        [Progress::Waiting, Progress::Parsed { done: 1, total: 1 }]
    );
}
