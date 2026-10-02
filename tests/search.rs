//! Search semantics, ranking and snippets, against a small corpus written in Claude Code's format.

mod common;

use claude_resume::search::{Field, MatchKind, Mode, Searcher};
use claude_resume::store::Store;
use common::Sandbox;
use serde_json::json;

fn corpus() -> (Sandbox, Store) {
    let sb = Sandbox::new();
    sb.session("newest")
        .at("2026-09-25T10:00:00Z")
        .cwd(&sb.workdir("infra"))
        .user("review the infra PR")
        .assistant("The terraform plan looks fine.")
        .write();
    sb.session("kafka-lag").at("2026-09-20T10:00:00Z").cwd(&sb.workdir("shop")).branch("fix/lag")
        .user("why is the kafka consumer lag growing on prod?")
        .assistant("The consumer group keeps rebalancing because max.poll.interval.ms is too low.")
        .tool("Bash", json!({"command": "kafka-consumer-groups --describe --group orders", "description": "Describe consumer group"}), "GROUP orders TOPIC events LAG 120000")
        .write();
    sb.session("terraform")
        .at("2026-09-10T10:00:00Z")
        .cwd(&sb.workdir("infra"))
        .user("terraform apply fails on the prod workspace with a state lock")
        .assistant("Run terraform force-unlock with the lock id.")
        .ai_title("Fix Terraform prod apply")
        .write();
    sb.session("react-ui")
        .at("2026-09-05T10:00:00Z")
        .cwd(&sb.workdir("web"))
        .user("create a react component for the login form")
        .assistant("Here is LoginForm.tsx")
        .ai_title("Login form UI")
        .write();
    sb.session("upgrade")
        .at("2026-09-04T10:00:00Z")
        .cwd(&sb.workdir("web"))
        .user("upgrade the dependencies and create a migration")
        .assistant("Upgraded 12 packages.")
        .write();
    sb.session("turkish")
        .at("2026-09-03T10:00:00Z")
        .cwd(&sb.workdir("ofis"))
        .user("İstanbul ofisi için şifre sıfırlama akışını düzelt")
        .assistant("Şifre sıfırlama e-postası artık gönderiliyor.")
        .write();
    sb.session("postgres")
        .at("2026-09-02T10:00:00Z")
        .cwd(&sb.workdir("shop"))
        .user("optimise the postgres query behind the monthly sales report")
        .assistant("Use a materialized view to build it quickly.")
        .write();
    sb.session("pasted-log")
        .at("2026-08-01T10:00:00Z")
        .cwd(&sb.workdir("ops"))
        .user(&format!(
            "analyse this log UNIQUE_MARKER_XYZ {}",
            "INFO all good\n".repeat(10_000)
        ))
        .assistant("The log shows nothing unusual.")
        .write();
    sb.session("queued")
        .at("2026-07-01T10:00:00Z")
        .cwd(&sb.workdir("ops"))
        .user("start the database migration")
        .queued("also bump the version number")
        .assistant("Started.")
        .write();
    sb.session("kafka-retention")
        .at("2026-03-01T10:00:00Z")
        .cwd(&sb.workdir("platform"))
        .user("set the retention for the audit topic")
        .assistant("Done.")
        .ai_title("Kafka retention policy")
        .write();
    let store = sb.store();
    (sb, store)
}

fn find<'a>(searcher: &'a Searcher, query: &str, mode: Mode) -> Vec<(&'a str, MatchKind)> {
    searcher
        .search(query, mode)
        .unwrap_or_else(|e| panic!("{query:?}: {e:#}"))
        .iter()
        .map(|h| (searcher.session(h).sid.as_str(), h.kind))
        .collect()
}

fn sids<'a>(searcher: &'a Searcher, query: &str, mode: Mode) -> Vec<&'a str> {
    find(searcher, query, mode)
        .into_iter()
        .map(|(sid, _)| sid)
        .collect()
}

#[test]
fn empty_query_lists_everything_newest_first() {
    let (sb, store) = corpus();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    let all = find(&s, "   ", Mode::Fuzzy);
    assert_eq!(all.len(), 10);
    assert_eq!(all[0], ("newest", MatchKind::Recent));
    assert_eq!(all.last().unwrap().0, "kafka-retention");
}

#[test]
fn exact_is_a_case_insensitive_substring_match() {
    let (sb, store) = corpus();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    assert_eq!(
        sids(&s, "KAFKA", Mode::Exact),
        ["kafka-lag", "kafka-retention"]
    );
    assert_eq!(
        sids(&s, "rebalanc", Mode::Exact),
        ["kafka-lag"],
        "mid-word substring"
    );
    assert!(sids(&s, "zookeeper", Mode::Exact).is_empty());
}

#[test]
fn all_terms_must_match_in_any_order_and_any_part_of_the_session() {
    // 0.4 searched the whole query as one literal string, so word order changed the results.
    let (sb, store) = corpus();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    assert_eq!(sids(&s, "kafka lag", Mode::Exact), ["kafka-lag"]);
    assert_eq!(sids(&s, "lag kafka", Mode::Exact), ["kafka-lag"]);
    assert_eq!(
        sids(&s, "rebalancing describe", Mode::Exact),
        ["kafka-lag"],
        "reply + tool call"
    );
    assert!(sids(&s, "kafka terraform", Mode::Exact).is_empty());
}

#[test]
fn quoted_phrases_and_exclusions() {
    let (sb, store) = corpus();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    assert_eq!(sids(&s, r#""consumer lag""#, Mode::Exact), ["kafka-lag"]);
    assert!(sids(&s, r#""lag consumer""#, Mode::Exact).is_empty());
    assert_eq!(sids(&s, "kafka -retention", Mode::Exact), ["kafka-lag"]);
    let without_kafka = sids(&s, "-kafka", Mode::Exact);
    assert_eq!(without_kafka.len(), 8);
    assert!(!without_kafka.contains(&"kafka-lag"));
    assert_eq!(
        sids(&s, "kafka -retention", Mode::Fuzzy),
        ["kafka-lag"],
        "exclusions apply in fuzzy mode too"
    );
}

#[test]
fn tool_calls_tool_output_queued_prompts_and_long_pastes_are_searchable() {
    let (sb, store) = corpus();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    assert_eq!(
        sids(&s, "kafka-consumer-groups", Mode::Exact),
        ["kafka-lag"],
        "Bash command"
    );
    assert_eq!(
        sids(&s, "LAG 120000", Mode::Exact),
        ["kafka-lag"],
        "tool output"
    );
    assert_eq!(
        sids(&s, "version number", Mode::Exact),
        ["queued"],
        "prompt typed while Claude was busy"
    );
    assert_eq!(
        sids(&s, "UNIQUE_MARKER_XYZ", Mode::Exact),
        ["pasted-log"],
        "prompt on a >50 KB line"
    );
    assert_eq!(sids(&s, "LoginForm.tsx", Mode::Exact), ["react-ui"]);
}

#[test]
fn non_ascii_text_matches() {
    let (sb, store) = corpus();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    assert_eq!(sids(&s, "şifre", Mode::Exact), ["turkish"]);
    assert_eq!(
        sids(&s, "ŞIFRE", Mode::Exact),
        ["turkish"],
        "case folding beyond ASCII"
    );
    assert_eq!(sids(&s, "düzelt", Mode::Fuzzy), ["turkish"]);
}

#[test]
fn terms_shorter_than_three_characters_match_titles_projects_and_branches_only() {
    let (sb, store) = corpus();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    assert_eq!(
        sids(&s, "ui", Mode::Exact),
        ["react-ui"],
        "title 'Login form UI'; 'build'/'quickly' in replies don't count"
    );
    assert_eq!(
        sids(&s, "fix", Mode::Exact)[..2],
        ["kafka-lag", "terraform"],
        "branch fix/lag, title 'Fix Terraform…'"
    );
}

#[test]
fn title_matches_rank_above_newer_content_matches() {
    let (sb, store) = corpus();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    assert_eq!(
        find(&s, "terraform", Mode::Exact),
        [
            ("terraform", MatchKind::Title),
            ("newest", MatchKind::Content)
        ]
    );
}

#[test]
fn sessions_with_the_query_as_a_phrase_rank_above_scattered_matches() {
    // On a long history `kafka consumer lag` matches hundreds of sessions ("lag" is in "flag"),
    // and the ones actually about kafka consumer lag were buried under newer, unrelated ones.
    let (sb, mut store) = corpus();
    sb.session("alerts")
        .at("2026-09-15T10:00:00Z")
        .cwd(&sb.workdir("ops"))
        .ai_title("Investigate the prod alerts")
        .user("which feature flags changed for the kafka consumer group?")
        .user("the dashboard shows kafka consumer lag above 10k since the deploy")
        .assistant("ok")
        .write();
    sb.session("scattered")
        .at("2026-09-30T10:00:00Z")
        .cwd(&sb.workdir("platform"))
        .user("add a feature flag for the new kafka producer")
        .assistant("Added the flag; the consumer side needs no change.")
        .write();
    store.sync(&sb.projects(), false).unwrap();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    for mode in [Mode::Exact, Mode::Fuzzy] {
        assert_eq!(
            find(&s, "kafka consumer lag", mode),
            [
                ("kafka-lag", MatchKind::Title),
                ("alerts", MatchKind::Phrase),
                ("scattered", MatchKind::Content)
            ],
            "{mode:?}"
        );
    }
    let alerts = s.sessions().iter().find(|row| row.sid == "alerts").unwrap();
    let snippets: Vec<String> = s
        .snippets(alerts, "kafka consumer lag", 3)
        .unwrap()
        .iter()
        .map(|(_, snippet)| snippet.marked("[", "]"))
        .collect();
    assert_eq!(
        snippets,
        ["the dashboard shows [kafka consumer lag] above 10k since the deploy"],
        "the excerpt shows why the session ranks first, not \"flags\" or a lone \"consumer\""
    );
    assert_eq!(
        find(&s, "consumer kafka lag", Mode::Exact),
        [
            ("kafka-lag", MatchKind::Title),
            ("scattered", MatchKind::Content),
            ("alerts", MatchKind::Content)
        ],
        "in another order it is no phrase, so newest first"
    );
}

#[test]
fn fuzzy_tolerates_typos_in_titles_and_long_words_without_false_positives() {
    let (sb, store) = corpus();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    let kafka = find(&s, "kfka", Mode::Fuzzy);
    assert!(
        kafka.contains(&("kafka-retention", MatchKind::Title))
            && kafka.contains(&("kafka-lag", MatchKind::Title)),
        "{kafka:?}"
    );
    assert_eq!(
        find(&s, "postgress", Mode::Fuzzy),
        [("postgres", MatchKind::Near)],
        "extra letter"
    );
    assert_eq!(
        find(&s, "rebalancnig", Mode::Fuzzy),
        [("kafka-lag", MatchKind::Near)],
        "swapped letters"
    );
    assert!(
        sids(&s, "postgress", Mode::Exact).is_empty(),
        "exact mode has no tolerance"
    );
    // 0.4: "gradle" minus a letter is "grade", which matched every session mentioning "upgrade".
    assert!(sids(&s, "gradle", Mode::Fuzzy).is_empty());
    assert_eq!(
        sids(&s, "react", Mode::Fuzzy),
        ["react-ui"],
        "'create' must not match 'react'"
    );
}

#[test]
fn hostile_queries_are_treated_as_text() {
    let (sb, store) = corpus();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    let queries = [
        "\"",
        "\"\"",
        "a\"b",
        "(",
        ")",
        "*",
        "NOT",
        "AND OR NOT",
        "col:value",
        "'; DROP TABLE sessions; --",
        "\\",
        "^$",
        "🎮",
        "-",
        "--",
        "\"-x\"",
        "kafka OR terraform",
        "NEAR(a b)",
        "{}",
        "%_",
        "a*b?c",
        "-\"\"",
    ];
    for q in queries {
        for mode in [Mode::Exact, Mode::Fuzzy] {
            if let Err(e) = s.search(q, mode) {
                panic!("{q:?} in {mode:?} mode: {e:#}");
            }
        }
    }
    assert!(
        sids(&s, "kafka OR terraform", Mode::Exact).is_empty(),
        "OR is a literal word, not an operator"
    );
    assert_eq!(store.len().unwrap(), 10);
}

#[test]
fn snippets_show_where_the_match_is() {
    let (sb, store) = corpus();
    let s = Searcher::new(&store, sb.paths().models).unwrap();
    let row = store.find("kafka-lag").unwrap().unwrap();

    let snippets = s.snippets(&row, "rebalancing", 3).unwrap();
    assert_eq!(snippets.len(), 1);
    assert_eq!(snippets[0].0, Field::Reply);
    assert!(
        snippets[0]
            .1
            .marked("[", "]")
            .contains("keeps [rebalancing] because")
    );

    let snippets = s.snippets(&row, "kafka", 5).unwrap();
    assert_eq!(
        snippets[0].0,
        Field::Prompt,
        "the user's own words come first"
    );
    assert!(snippets.iter().any(|(f, _)| *f == Field::Tool));

    let near = s
        .snippets(&store.find("postgres").unwrap().unwrap(), "postgress", 3)
        .unwrap();
    assert!(
        near[0].1.marked("[", "]").contains("[postgres]"),
        "near matches are highlighted too"
    );

    assert!(s.snippets(&row, "", 3).unwrap().is_empty());
}

#[test]
fn semantic_mode_without_the_model_explains_itself_and_downloads_nothing() {
    let (sb, store) = corpus();
    let models = sb.paths().models;
    let s = Searcher::new(&store, models.clone()).unwrap();
    let err = s
        .search("deploying infrastructure", Mode::Semantic)
        .unwrap_err();
    let expected = if cfg!(feature = "semantic") {
        "claude-resume embed"
    } else {
        "without the `semantic` feature"
    };
    assert!(format!("{err:#}").contains(expected), "{err:#}");
    assert!(!models.exists(), "nothing was downloaded");
}
