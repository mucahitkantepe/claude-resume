//! Semantic search with the real embedding model. Ignored by default because it needs the
//! ~133 MB model:
//!
//!     cargo test --test semantic -- --ignored
//!
//! The model is read from `$CLAUDE_RESUME_TEST_MODELS`, or from `~/.claude/models` when
//! `claude-resume embed` has already downloaded it there. These tests never download anything.
#![cfg(feature = "semantic")]

mod common;

use claude_resume::search::{MatchKind, Mode, Searcher};
use claude_resume::semantic::{self, Embedder};
use common::Sandbox;
use predicates::prelude::*;
use std::path::PathBuf;

fn models() -> PathBuf {
    let dir = std::env::var_os("CLAUDE_RESUME_TEST_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap().join(".claude/models"));
    assert!(
        semantic::model_cached(&dir),
        "the embedding model is not in {}; run `claude-resume embed` once or set CLAUDE_RESUME_TEST_MODELS",
        dir.display()
    );
    dir
}

fn corpus(sb: &Sandbox) {
    sb.session("infra")
        .cwd(&sb.workdir("platform"))
        .user("set up terraform for the production cluster on AWS with an S3 state backend")
        .assistant("Created main.tf with the VPC, EKS cluster and backend config.")
        .write();
    sb.session("bread")
        .cwd(&sb.workdir("home"))
        .user("how long should I proof sourdough overnight in the fridge?")
        .assistant("12 to 16 hours at around 4°C works well.")
        .write();
    sb.session("css")
        .cwd(&sb.workdir("site"))
        .user("center a div horizontally and vertically with flexbox")
        .assistant("Use display:flex with justify-content and align-items set to center.")
        .write();
    sb.session("k8s")
        .cwd(&sb.workdir("platform"))
        .user("pods keep crashing with OOMKilled in kubernetes, should I raise the memory limits?")
        .assistant("Check the actual usage first; the limit is 256Mi.")
        .write();
}

#[test]
#[ignore = "needs the embedding model"]
fn finds_sessions_by_meaning_rather_than_keywords() {
    let models = models();
    let sb = Sandbox::new();
    corpus(&sb);
    let store = sb.store();
    let embedder = Embedder::load(&models, false).unwrap();
    for (sid, text, hash) in semantic::stale(&store).unwrap() {
        let v = embedder.embed_session(&text).unwrap();
        store
            .put_embedding(&sid, semantic::MODEL_KEY, &hash, &semantic::to_bytes(&v))
            .unwrap();
    }
    let searcher = Searcher::new(&store, models).unwrap();
    for (query, expected) in [
        ("deploying cloud infrastructure", "infra"),
        ("baking bread at home", "bread"),
        ("web page layout", "css"),
        ("container runs out of memory", "k8s"),
    ] {
        let hits = searcher.search(query, Mode::Semantic).unwrap();
        let ranked: Vec<(&str, f32)> = hits
            .iter()
            .map(|h| (searcher.session(h).sid.as_str(), h.score))
            .collect();
        assert_eq!(
            ranked.first().map(|r| r.0),
            Some(expected),
            "{query:?} ranked {ranked:?}"
        );
        assert!(hits.iter().all(|h| h.kind == MatchKind::Semantic));
        assert!(
            hits.windows(2).all(|w| w[0].score >= w[1].score),
            "best first"
        );
    }
}

#[test]
#[ignore = "needs the embedding model"]
fn embeddings_are_deterministic_and_normalised() {
    let embedder = Embedder::load(&models(), false).unwrap();
    let a = embedder
        .embed_session("set up terraform for production")
        .unwrap();
    let b = embedder
        .embed_session("set up terraform for production")
        .unwrap();
    assert_eq!(a.len(), 384);
    assert_eq!(a, b);
    assert!((semantic::cosine(&a, &a) - 1.0).abs() < 1e-4);
    let long = embedder
        .embed_session(&"terraform plan output line\n".repeat(400))
        .unwrap();
    assert!(
        (semantic::cosine(&long, &long) - 1.0).abs() < 1e-4,
        "multi-chunk sessions are normalised too"
    );
}

#[test]
#[ignore = "needs the embedding model"]
fn cli_embed_then_semantic_search() {
    let models = models();
    let sb = Sandbox::new();
    corpus(&sb);
    sb.cmd()
        .arg("embed")
        .env("CLAUDE_RESUME_MODELS_DIR", &models)
        .assert()
        .success()
        .stderr(predicate::str::contains("Embedded 4 sessions"));
    sb.cmd()
        .arg("embed")
        .env("CLAUDE_RESUME_MODELS_DIR", &models)
        .assert()
        .success()
        .stderr(predicate::str::contains("All sessions are embedded"));
    sb.cmd()
        .args([
            "search",
            "baking bread at home",
            "-m",
            "semantic",
            "-n",
            "1",
        ])
        .env("CLAUDE_RESUME_MODELS_DIR", &models)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("1. how long should I proof sourdough")
                .and(predicate::str::contains("similarity")),
        );
}

#[test]
#[ignore = "needs the embedding model"]
fn new_sessions_are_found_without_running_embed_again() {
    let models = models();
    let sb = Sandbox::new();
    corpus(&sb);
    let run = |args: &[&str]| {
        let mut cmd = sb.cmd();
        cmd.args(args).env("CLAUDE_RESUME_MODELS_DIR", &models);
        cmd
    };
    run(&["embed"]).assert().success();

    // A semantic search embeds what is new before it searches...
    sb.session("garden")
        .cwd(&sb.workdir("home"))
        .user("when should I plant tomato seedlings outside?")
        .assistant("After the last frost, once nights stay above 10°C.")
        .write();
    run(&["search", "-m", "semantic", "growing vegetables", "-n", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "1. when should I plant tomato seedlings",
        ));

    // ...and every other command leaves it to a background embed.
    sb.session("guitar")
        .cwd(&sb.workdir("home"))
        .user("how do I tune a guitar to drop D?")
        .assistant("Lower the low E string a whole step, to D.")
        .write();
    run(&["search", "terraform"]).assert().success();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !sb
        .store()
        .embeddings(semantic::MODEL_KEY)
        .unwrap()
        .iter()
        .any(|(sid, _)| sid == "guitar")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "never embedded in the background"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    run(&[
        "--no-sync",
        "search",
        "-m",
        "semantic",
        "playing a musical instrument",
        "-n",
        "1",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("1. how do I tune a guitar"));
}
