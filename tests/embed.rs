//! Keeping semantic search up to date: `embed`, the background embed every other command starts
//! once the model is downloaded, and the embedding a semantic search does before it searches.
//! The model is a tiny stand-in (`common::tiny_model`), so these check which sessions get
//! embedded and when; `tests/semantic.rs` checks what the real model finds.
#![cfg(feature = "semantic")]

mod common;

use claude_resume::semantic::MODEL_KEY;
use claude_resume::store::{FileLock, Store};
use common::Sandbox;
use predicates::prelude::*;
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for something that should take a moment. Generous, so a machine that is
/// busy for a while (or a scanner slowing every process launch) fails nothing.
const SOON: Duration = Duration::from_secs(60);

fn sandbox() -> Sandbox {
    let sb = Sandbox::new();
    sb.session("lag")
        .user("why is the kafka consumer lag growing?")
        .assistant("The consumer group keeps rebalancing.")
        .write();
    sb.session("tf")
        .user("terraform state lock is stuck")
        .assistant("Use terraform force-unlock.")
        .write();
    sb
}

fn models(sb: &Sandbox) -> PathBuf {
    sb.claude_dir.join("models")
}

fn log_path(sb: &Sandbox) -> PathBuf {
    sb.claude_dir.join("claude-resume.log")
}

fn log(sb: &Sandbox) -> String {
    fs::read_to_string(log_path(sb)).unwrap_or_default()
}

/// Sessions that have a vector, read straight from the index.
fn embedded(sb: &Sandbox) -> Vec<String> {
    let store = Store::open(&sb.paths().db).unwrap();
    let mut sids: Vec<String> = store
        .embeddings(MODEL_KEY)
        .unwrap()
        .into_iter()
        .map(|(sid, _)| sid)
        .collect();
    sids.sort();
    sids
}

fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + SOON;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait for a background embed to get `sid` embedded and then to finish.
fn background_embeds(sb: &Sandbox, sid: &str) {
    eventually(&format!("{sid} is embedded"), || {
        embedded(sb).iter().any(|s| s == sid)
    });
    let lock = sb.paths().db.with_extension("embed.lock");
    eventually("the background embed is done", || {
        FileLock::try_acquire(&lock).unwrap().is_some()
    });
}

#[test]
fn embed_embeds_every_session_and_then_has_nothing_left_to_do() {
    let sb = sandbox();
    common::tiny_model(&models(&sb));
    sb.cmd()
        .arg("embed")
        .assert()
        .success()
        .stderr(predicate::str::contains("Embedded 2 sessions in"));
    assert_eq!(embedded(&sb), ["lag", "tf"]);
    sb.cmd()
        .arg("embed")
        .assert()
        .success()
        .stderr(predicate::str::contains("All sessions are embedded."));
    sb.cmd()
        .args(["search", "-m", "semantic", "kafka lag"])
        .assert()
        .success();
}

#[test]
fn every_other_command_embeds_new_sessions_in_the_background() {
    let sb = sandbox();
    common::tiny_model(&models(&sb));
    sb.cmd().arg("embed").assert().success();
    let commands: [&[&str]; 4] = [
        &["search", "kafka"],
        &["search", "-m", "exact", "kafka"],
        &["sync"],
        &["resume", "--print", "lag"],
    ];
    for (n, args) in commands.into_iter().enumerate() {
        let sid = format!("new{n}");
        sb.session(&sid).user("deploy it").write();
        sb.cmd().args(args).assert().success();
        background_embeds(&sb, &sid);
    }
    let log = log(&sb);
    assert_eq!(
        log.lines()
            .filter(|l| l.ends_with("s.") && l.contains(" Embedded 1 session in "))
            .count(),
        4,
        "one timestamped line per run:\n{log}"
    );
}

#[test]
fn a_semantic_search_embeds_a_few_new_sessions_before_it_searches() {
    let sb = sandbox();
    common::tiny_model(&models(&sb));
    sb.cmd().arg("embed").assert().success();
    for n in 0..10 {
        sb.session(&format!("new{n}")).user("deploy it").write();
    }
    sb.cmd()
        .args(["search", "-m", "semantic", "deploy", "--json"])
        .assert()
        .success()
        .stderr("");
    assert_eq!(
        embedded(&sb).len(),
        12,
        "embedded by the time the search ran"
    );
    assert!(!log_path(&sb).exists(), "and not by a background embed");
}

#[test]
fn a_semantic_search_leaves_a_bigger_backlog_to_the_background() {
    let sb = sandbox();
    common::tiny_model(&models(&sb));
    sb.cmd().arg("embed").assert().success();
    for n in 0..11 {
        sb.session(&format!("new{n:02}")).user("deploy it").write();
    }
    sb.cmd()
        .args(["search", "-m", "semantic", "deploy"])
        .timeout(SOON)
        .assert()
        .success()
        .stderr(
            "11 sessions are still being embedded in the background, so these results may miss them.\n",
        );
    background_embeds(&sb, "new10");
    assert_eq!(embedded(&sb).len(), 13);
    assert!(log(&sb).contains("Embedded 11 sessions in"), "{}", log(&sb));
}

#[test]
fn a_semantic_search_never_waits_for_an_embed_already_running() {
    let sb = sandbox();
    common::tiny_model(&models(&sb));
    sb.cmd().arg("embed").assert().success();
    sb.session("new").user("deploy it").write();
    let running = FileLock::acquire(&sb.paths().db.with_extension("embed.lock")).unwrap();
    sb.cmd()
        .args(["search", "-m", "semantic", "deploy"])
        .timeout(SOON)
        .assert()
        .success()
        .stderr(
            "1 session is still being embedded in the background, so these results may miss it.\n",
        );
    assert_eq!(embedded(&sb), ["lag", "tf"], "it searched what was there");
    drop(running);
}

#[test]
fn nothing_is_embedded_until_embed_has_downloaded_the_model() {
    let sb = sandbox();
    for args in [
        &["search", "kafka"][..],
        &["sync"],
        &["resume", "--print", "lag"],
    ] {
        sb.cmd().args(args).assert().success();
    }
    sb.cmd()
        .args(["search", "-m", "semantic", "kafka"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("run `claude-resume embed`"));
    assert!(!models(&sb).exists(), "nothing was downloaded");
    assert!(!log_path(&sb).exists(), "nothing ran in the background");
    assert!(embedded(&sb).is_empty());
}

#[test]
fn no_sync_leaves_the_embeddings_alone_too() {
    let sb = sandbox();
    sb.store();
    common::tiny_model(&models(&sb));
    sb.cmd()
        .args(["--no-sync", "search", "kafka"])
        .assert()
        .success();
    sb.cmd()
        .args(["--no-sync", "search", "-m", "semantic", "kafka"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no sessions are embedded yet"));
    assert!(!log_path(&sb).exists());
    assert!(embedded(&sb).is_empty());
}

/// The stored vector of `sid`.
fn vector(sb: &Sandbox, sid: &str) -> Option<Vec<u8>> {
    let store = Store::open(&sb.paths().db).unwrap();
    let vectors = store.embeddings(MODEL_KEY).unwrap();
    vectors.into_iter().find(|(s, _)| s == sid).map(|(_, v)| v)
}

#[test]
fn a_session_that_changes_is_embedded_again() {
    let sb = sandbox();
    common::tiny_model(&models(&sb));
    sb.cmd().arg("embed").assert().success();
    let before = vector(&sb, "tf").unwrap();
    sb.session("tf")
        .user("terraform state lock is stuck")
        .assistant("Use terraform force-unlock.")
        .user("deploy it again then")
        .write();
    sb.cmd().args(["search", "kafka"]).assert().success();
    eventually("tf is embedded again", || {
        vector(&sb, "tf").is_some_and(|v| v != before)
    });
    background_embeds(&sb, "tf");
}

#[test]
fn sessions_still_without_a_vector_get_one_on_the_next_command() {
    // As after an interrupted background embed, or an upgrade that changed the model: the sync
    // finds nothing new, but some sessions have no vector from the current model.
    let sb = sandbox();
    let store = sb.store();
    store
        .put_embedding("lag", "an-older-model", "hash", &[0; 16])
        .unwrap();
    drop(store);
    common::tiny_model(&models(&sb));
    sb.cmd().args(["search", "kafka"]).assert().success();
    background_embeds(&sb, "lag");
    assert_eq!(embedded(&sb), ["lag", "tf"]);
}

#[test]
fn a_background_embed_holds_up_neither_the_command_nor_its_output() {
    let sb = sandbox();
    common::tiny_model(&models(&sb));
    // Swap the model's config for a pipe that only gets written to when the test says so: the
    // background embed blocks loading the model, as if embedding took forever.
    let config =
        claude_resume::semantic::model_dir(&models(&sb)).join("snapshots/tiny/config.json");
    let contents = fs::read(&config).unwrap();
    fs::remove_file(&config).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(&config)
            .status()
            .unwrap()
            .success()
    );
    let (release, released) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = released.recv(); // or the test ended: let it go either way
        // Each reader gets the config once. That is normally the background embed, but a process
        // that peeks at new files (a virus scanner, say) may open the pipe first, so serve readers
        // in turn, and only once the last one has closed it: one still reading would otherwise
        // get the config twice.
        while let Ok(mut pipe) = fs::OpenOptions::new().write(true).open(&config) {
            let _ = pipe.write_all(&contents);
            drop(pipe);
            loop {
                let probe = fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&config);
                match probe {
                    Err(e) if e.raw_os_error() == Some(libc::ENXIO) => break, // nobody reading
                    Err(_) => return,
                    Ok(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        }
    });

    // Run like a terminal runs a command: as its own process group, the foreground job.
    let mut search = sb.std_cmd();
    search
        .args(["search", "kafka"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let search = search.spawn().unwrap();
    let job = search.id();
    let (done, finished) = std::sync::mpsc::channel();
    // `wait_with_output` reads stdout and stderr to the end, so returning in time also proves
    // the background embed holds neither.
    std::thread::spawn(move || done.send(search.wait_with_output()));
    let output = finished
        .recv_timeout(SOON)
        .expect("the search waited for the background embed")
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("kafka consumer lag"));
    // Closing the terminal hangs up its foreground job; the background embed is not part of it.
    // (Nothing is left in that group, so `kill` may well report that it found nobody.)
    let _ = Command::new("kill")
        .args(["-s", "HUP", "--", &format!("-{job}")])
        .stderr(Stdio::null())
        .status();
    assert!(embedded(&sb).is_empty(), "still loading the model");

    drop(release);
    let deadline = Instant::now() + SOON;
    while embedded(&sb).len() < 2 {
        assert!(
            Instant::now() < deadline,
            "the background embed never finished; its log: {:?}",
            log(&sb)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    background_embeds(&sb, "tf");
    assert_eq!(embedded(&sb), ["lag", "tf"]);
    assert_eq!(log(&sb).matches("Embedded 2 sessions").count(), 1);
}

#[test]
fn a_background_embed_gives_way_to_one_already_running() {
    let sb = sandbox();
    sb.store();
    common::tiny_model(&models(&sb));
    let running = FileLock::acquire(&sb.paths().db.with_extension("embed.lock")).unwrap();
    sb.cmd()
        .args(["embed", "--background"])
        .timeout(SOON)
        .assert()
        .success()
        .stderr("");
    assert!(
        embedded(&sb).is_empty(),
        "it left the work to the one running"
    );
    drop(running);
    sb.cmd().args(["embed", "--background"]).assert().success();
    assert_eq!(embedded(&sb), ["lag", "tf"]);
}

/// `(stat, command)` of each child of `pid`, from `ps`.
fn children(pid: u32) -> Vec<(String, String)> {
    let ps = Command::new("ps")
        .args(["-A", "-o", "ppid=,stat=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let ppid: u32 = fields.next()?.parse().ok()?;
            let stat = fields.next()?.to_string();
            (ppid == pid).then(|| (stat, fields.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

#[test]
fn a_background_embed_that_ends_first_is_not_left_a_zombie() {
    // `resume` waits for the session it resumes, for hours maybe; a background embed that ends
    // meanwhile must not linger as a zombie all that time.
    let sb = sandbox();
    common::tiny_model(&models(&sb));
    sb.cmd().arg("embed").assert().success();
    sb.session("new").user("deploy it").write();
    let release = sb.root.join("release-claude");
    fs::write(
        sb.bin.join("claude"),
        "#!/bin/sh\nwhile [ ! -e \"$RELEASE_CLAUDE\" ]; do sleep 0.05; done\n",
    )
    .unwrap();
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "");
        }
    }
    let release = Release(release);
    let mut resume = sb
        .std_cmd()
        .args(["resume", "lag"])
        .env("RELEASE_CLAUDE", &release.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = resume.id();

    background_embeds(&sb, "new");
    eventually("the background embed has exited", || {
        !children(pid)
            .iter()
            .any(|(stat, command)| !stat.starts_with('Z') && command.contains("embed --background"))
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while children(pid).iter().any(|(stat, _)| stat.starts_with('Z')) {
        assert!(
            Instant::now() < deadline,
            "left a zombie: {:?}",
            children(pid)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(resume.try_wait().unwrap().is_none(), "still resuming");
    drop(release);
    assert!(resume.wait().unwrap().success());
}

#[test]
fn the_log_starts_over_when_it_gets_big() {
    let sb = sandbox();
    common::tiny_model(&models(&sb));
    fs::write(log_path(&sb), "old line\n".repeat(200_000)).unwrap();
    sb.cmd().args(["search", "kafka"]).assert().success();
    background_embeds(&sb, "tf");
    let log = log(&sb);
    assert!(
        !log.contains("old line") && log.contains("Embedded 2 sessions"),
        "{log:.200}"
    );
}

#[test]
fn a_second_embed_waits_for_the_one_already_running() {
    // Two terminals running `claude-resume embed` must not both run the model over every session.
    use std::io::BufRead;
    let sb = sandbox();
    common::tiny_model(&models(&sb));
    let first = FileLock::acquire(&sb.paths().db.with_extension("embed.lock")).unwrap();
    let mut second = sb
        .std_cmd()
        .arg("embed")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (lines, received) = std::sync::mpsc::channel();
    let stderr = std::io::BufReader::new(second.stderr.take().unwrap());
    std::thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            let _ = lines.send(line);
        }
    });
    loop {
        let line = received
            .recv_timeout(SOON)
            .expect("the second embed never said it was waiting");
        if line.contains("Waiting for another claude-resume to finish embedding") {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(second.try_wait().unwrap().is_none(), "still waiting");
    assert!(embedded(&sb).is_empty());
    drop(first);
    assert!(second.wait().unwrap().success());
    let rest: Vec<String> = received.iter().collect();
    assert!(
        rest.iter().any(|l| l.contains("Embedded 2 sessions")),
        "{rest:?}"
    );
    assert_eq!(embedded(&sb), ["lag", "tf"]);
}
