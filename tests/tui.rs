//! The interactive picker: input handling, rendering, and a real end-to-end run inside tmux.

mod common;

use claude_resume::search::{Mode, Searcher};
use claude_resume::store::Store;
use claude_resume::tui::{App, Outcome, render};
use common::Sandbox;
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use std::process::Command;
use std::time::{Duration, Instant};

const LAG: &str = "11111111-aaaa-4000-8000-000000000001";

fn corpus() -> (Sandbox, Store) {
    let sb = Sandbox::new();
    sb.session(LAG)
        .at("2026-09-20T10:00:00Z")
        .cwd(&sb.workdir("shop"))
        .branch("fix/lag")
        .user("why is the kafka consumer lag growing on prod?")
        .assistant("The consumer group keeps rebalancing.")
        .ai_title("Kafka consumer lag on prod")
        .pr_link("https://github.com/o/r/pull/42")
        .write();
    sb.session("22222222-bbbb-4000-8000-000000000002")
        .at("2026-09-10T10:00:00Z")
        .cwd(&sb.workdir("infra"))
        .user("terraform apply fails with a state lock")
        .assistant("Use terraform force-unlock.")
        .write();
    let mut long = sb
        .session("33333333-cccc-4000-8000-000000000003")
        .at("2026-08-01T10:00:00Z")
        .cwd(&sb.workdir("web"));
    for i in 1..=20 {
        long = long
            .user(&format!("step {i} of the long refactor"))
            .assistant("done");
    }
    long.write();
    let store = sb.store();
    (sb, store)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_key(key(KeyCode::Char(c)));
    }
}

fn selected_sid(app: &App) -> String {
    app.selected_session()
        .map(|s| s.sid.clone())
        .unwrap_or_default()
}

fn screen(app: &mut App, width: u16, height: u16) -> (Vec<String>, (u16, u16)) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| render(f, app)).unwrap();
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    let buffer = terminal.backend().buffer().clone();
    let lines = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect::<String>()
        })
        .collect();
    (lines, (cursor.x, cursor.y))
}

#[test]
fn starts_with_every_session_newest_first() {
    let (sb, store) = corpus();
    let app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "",
    );
    assert_eq!(app.hits.len(), 3);
    assert_eq!(selected_sid(&app), LAG);
}

#[test]
fn typing_filters_and_backspace_widens_again() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "",
    );
    type_text(&mut app, "terraform");
    assert_eq!(app.hits.len(), 1);
    assert_eq!(selected_sid(&app), "22222222-bbbb-4000-8000-000000000002");
    for _ in 0.."terraform".len() {
        app.handle_key(key(KeyCode::Backspace));
    }
    assert_eq!(app.query, "");
    assert_eq!(app.hits.len(), 3);
}

#[test]
fn initial_query_is_applied() {
    let (sb, store) = corpus();
    let app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Exact,
        "rebalancing",
    );
    assert_eq!(app.hits.len(), 1);
    assert_eq!(selected_sid(&app), LAG);
}

#[test]
fn navigation_is_clamped_and_resets_the_preview_scroll() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "",
    );
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.selected, 0);
    app.handle_key(ctrl('d'));
    assert_eq!(app.preview_scroll, 5);
    app.handle_key(key(KeyCode::Down));
    assert_eq!((app.selected, app.preview_scroll), (1, 0));
    app.handle_key(ctrl('n'));
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key(KeyCode::PageDown));
    assert_eq!(app.selected, 2, "clamped to the last session");
    app.handle_key(ctrl('k'));
    app.handle_key(ctrl('p'));
    assert_eq!(app.selected, 0);
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.selected, 1);
}

#[test]
fn enter_resumes_the_selected_session_and_escape_cancels() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "",
    );
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    match app.outcome.take() {
        Some(Outcome::Resume(s)) => assert_eq!(s.sid, "22222222-bbbb-4000-8000-000000000002"),
        other => panic!("{other:?}"),
    }
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.outcome, Some(Outcome::Cancel));
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "",
    );
    app.handle_key(ctrl('c'));
    assert_eq!(app.outcome, Some(Outcome::Cancel));
}

#[test]
fn enter_with_no_matches_does_nothing() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "zzzzzzzz",
    );
    assert!(app.hits.is_empty());
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.outcome, None);
}

#[test]
fn word_deletion() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "",
    );
    type_text(&mut app, "kafka consumer-lag");
    app.handle_key(ctrl('w'));
    assert_eq!(app.query, "kafka consumer");
    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(app.query, "kafka");
    app.handle_key(ctrl('w'));
    assert_eq!(app.query, "");
}

#[test]
fn paste_adds_only_the_first_line() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Exact,
        "",
    );
    app.handle_paste("force-unlock\nsecond line ignored");
    assert_eq!(app.query, "force-unlock");
    assert_eq!(app.hits.len(), 1);
}

#[test]
fn shift_tab_cycles_modes_and_semantic_explains_what_is_missing() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "deploy",
    );
    app.handle_key(key(KeyCode::BackTab));
    assert_eq!(app.mode, Mode::Semantic);
    assert!(app.hits.is_empty());
    let hint = if cfg!(feature = "semantic") {
        "claude-resume embed"
    } else {
        "without the `semantic` feature"
    };
    assert!(
        app.status.as_deref().unwrap().contains(hint),
        "{:?}",
        app.status
    );
    let (lines, _) = screen(&mut app, 200, 20);
    assert!(lines.iter().any(|l| l.contains(hint)), "status line shown");
    app.handle_key(key(KeyCode::BackTab));
    assert_eq!(app.mode, Mode::Exact);
    assert_eq!(app.status, None);
    app.handle_key(key(KeyCode::BackTab));
    assert_eq!(app.mode, Mode::Fuzzy);
}

#[test]
fn semantic_search_waits_for_a_pause_in_typing() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Semantic,
        "",
    );
    assert_eq!(app.hits.len(), 3, "empty query lists everything");
    type_text(&mut app, "deploy");
    assert_eq!(app.hits.len(), 3, "not searched yet");
    assert!(app.poll_timeout(Instant::now()) <= Duration::from_millis(250));
    app.tick(Instant::now());
    assert_eq!(app.hits.len(), 3, "still typing");
    app.tick(Instant::now() + Duration::from_secs(1));
    assert!(
        app.hits.is_empty() && app.status.is_some(),
        "searched after the pause"
    );
}

#[test]
fn renders_the_list_and_a_preview_of_the_selected_session() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "consumer",
    );
    let (lines, _) = screen(&mut app, 160, 30);
    let all = lines.join("\n");
    assert!(all.contains(" fuzzy (1/3) "), "{all}");
    assert!(all.contains(" > "), "selection pointer");
    assert!(all.contains("shop") && all.contains("Kafka consumer lag on prod"));
    assert!(all.contains("1 prompt · "), "{all}");
    assert!(all.contains("⎇ fix/lag"));
    assert!(all.contains(&format!("id {LAG}")));
    assert!(all.contains("PR https://github.com/o/r/pull/42"));
    assert!(all.contains("Matches (2):"), "{all}");
    assert!(all.contains("you › why is the kafka consumer lag growing on prod?"));
    assert!(all.contains("claude › The consumer group keeps rebalancing."));
    assert!(all.contains("1. why is the kafka consumer lag growing on prod?"));
    assert!(all.contains("enter resume"));
}

#[test]
fn long_lines_wrap_under_their_text_and_long_names_end_in_an_ellipsis() {
    let sb = Sandbox::new();
    let prompt = "please find out why the nightly export job sometimes writes duplicate rows into \
                  the warehouse table, then fix it and add a regression test";
    sb.session("44444444-dddd-4000-8000-000000000004")
        .cwd(&sb.workdir("a-really-long-project-name"))
        .user(prompt)
        .assistant("ok")
        .write();
    let store = sb.store();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "",
    );
    let (lines, _) = screen(&mut app, 160, 30);
    // The preview is the right half: a border, then the content.
    let preview: Vec<String> = lines
        .iter()
        .map(|l| l.chars().skip(81).take(78).collect::<String>())
        .collect();
    let first = preview
        .iter()
        .position(|l| l.starts_with("   1. please find out"))
        .unwrap_or_else(|| panic!("{}", preview.join("\n")));
    let wrapped: Vec<&str> = preview[first..]
        .iter()
        .take_while(|l| !l.trim().is_empty())
        .map(String::as_str)
        .collect();
    assert!(wrapped.len() > 1, "{wrapped:?}");
    for line in &wrapped[1..] {
        assert!(
            line.starts_with("      ") && !line[6..].starts_with(' '),
            "continuation lines line up under the text: {wrapped:?}"
        );
    }
    let words: Vec<&str> = wrapped
        .iter()
        .flat_map(|l| l.split_whitespace())
        .skip(1)
        .collect();
    assert_eq!(words.join(" "), prompt, "no word lost or split");
    assert!(
        lines.iter().any(|l| l.contains("a-really-long… ")),
        "{}",
        lines.join("\n")
    );
}

#[test]
fn scrolling_the_preview_stops_at_its_end() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "",
    );
    for _ in 0..50 {
        app.handle_key(ctrl('d'));
    }
    screen(&mut app, 160, 30);
    app.handle_key(ctrl('u'));
    let (lines, _) = screen(&mut app, 160, 30);
    // Only the preview (right half): the list shows the title too.
    let preview: Vec<String> = lines.iter().map(|l| l.chars().skip(81).collect()).collect();
    assert!(
        preview
            .iter()
            .any(|l| l.contains("Kafka consumer lag on prod")),
        "one ^u brings the top back: {}",
        preview.join("\n")
    );
}

#[test]
fn long_sessions_preview_their_first_and_last_prompts() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "",
    );
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Down));
    assert_eq!(selected_sid(&app), "33333333-cccc-4000-8000-000000000003");
    let (lines, _) = screen(&mut app, 160, 40);
    let all = lines.join("\n");
    assert!(all.contains("20 prompts"));
    assert!(all.contains("1. step 1 of the long refactor"));
    assert!(all.contains("8. step 8 of the long refactor"));
    assert!(all.contains("… 9 more"), "{all}");
    assert!(all.contains("18. step 18 of the long refactor"));
    assert!(all.contains("20. step 20 of the long refactor"));
    assert!(!all.contains("step 9 of"));
}

#[test]
fn narrow_terminals_hide_the_preview_and_tiny_ones_still_render() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "",
    );
    let (lines, _) = screen(&mut app, 80, 24);
    let all = lines.join("\n");
    assert!(!all.contains(" preview "));
    assert!(all.contains("Kafka consumer lag"));
    for (w, h) in [(20, 5), (1, 1), (200, 8), (81, 11)] {
        screen(&mut app, w, h); // must not panic
    }
}

#[test]
fn cursor_sits_after_the_query_even_with_non_ascii_input() {
    // 0.4 used the byte length, so `ş` or `é` pushed the cursor too far right.
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "",
    );
    type_text(&mut app, "şifre");
    let (_, (x, y)) = screen(&mut app, 160, 30);
    assert_eq!((x, y), (1 + 5, 1));
    type_text(&mut app, "日本");
    let (_, (x, _)) = screen(&mut app, 160, 30);
    assert_eq!(x, 1 + 5 + 4, "wide characters take two cells");
}

#[test]
fn a_long_query_scrolls_so_its_end_and_the_cursor_stay_in_the_box() {
    let (sb, store) = corpus();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Exact,
        "",
    );
    let query = format!(
        "{} kafka consumer lag",
        "speed up the deploy pipeline ".repeat(4)
    );
    type_text(&mut app, &query);
    for (width, height) in [(100, 30), (60, 8)] {
        let (lines, (x, y)) = screen(&mut app, width, height);
        let left = if width > 80 { 45 } else { width };
        let row = &lines[y as usize];
        assert!(row.contains("consumer lag"), "{width}x{height}: {row:?}");
        assert!(
            x < left,
            "{width}x{height}: cursor at {x}, box ends at {left}"
        );
        let before: String = row.chars().take(x as usize).collect();
        assert!(before.trim_end().ends_with("consumer lag"), "{before:?}");
    }
}

#[test]
fn fuzzy_highlights_land_on_the_matched_letters_after_emoji_and_accents() {
    let sb = Sandbox::new();
    sb.session("55555555-eeee-4000-8000-000000000005")
        .ai_title("❤️ cafe\u{301} kafka lag")
        .user("hello")
        .assistant("ok")
        .write();
    let store = sb.store();
    let mut app = App::new(
        Searcher::new(&store, sb.paths().models).unwrap(),
        Mode::Fuzzy,
        "kfk",
    );
    let mut terminal = Terminal::new(TestBackend::new(160, 20)).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    // The list is the left half; the preview repeats the title without highlights.
    let highlighted: String = (0..20)
        .flat_map(|y| (0..80).map(move |x| (x, y)))
        .filter_map(|(x, y)| buffer.cell((x, y)))
        .filter(|c| c.fg == ratatui::style::Color::Yellow)
        .map(|c| c.symbol().to_string())
        .collect();
    assert_eq!(highlighted, "kfk");
}

/// Drive the real binary inside a private tmux server: type a query, press Enter, and check that
/// `claude --resume` was started for the chosen session in its original directory.
#[test]
fn end_to_end_pick_and_resume_in_a_real_terminal() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let (sb, _store) = corpus();
    let tmux = Tmux::start(&sb, "--no-sync");
    tmux.wait_for("fuzzy (3/3)");
    tmux.send_literal("consumer lag");
    tmux.wait_for("fuzzy (1/3)");
    tmux.wait_for("Kafka consumer lag on prod");
    tmux.send_key("Enter");
    let deadline = Instant::now() + Duration::from_secs(10);
    while sb.claude_calls().is_empty() {
        assert!(
            Instant::now() < deadline,
            "claude was never started; screen:\n{}",
            tmux.capture()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let calls = sb.claude_calls();
    assert_eq!(calls[0].args, format!("--resume {LAG}"));
    assert_eq!(calls[0].cwd, sb.workdir("shop"));
}

#[test]
fn end_to_end_escape_leaves_without_resuming() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let (sb, _store) = corpus();
    let tmux = Tmux::start(&sb, "--no-sync");
    tmux.wait_for("fuzzy (3/3)");
    tmux.send_key("Escape");
    tmux.wait_for("picker exited");
    assert!(sb.claude_calls().is_empty());
}

/// Opened on a semantic query, the picker embeds new sessions before it searches; otherwise it
/// leaves that to a background embed.
#[cfg(feature = "semantic")]
#[test]
fn end_to_end_the_picker_keeps_semantic_search_up_to_date() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let (sb, store) = corpus();
    common::tiny_model(&sb.claude_dir.join("models"));
    let embedded = || {
        store
            .embeddings(claude_resume::semantic::MODEL_KEY)
            .unwrap()
            .len()
    };
    let log = sb.claude_dir.join("claude-resume.log");

    let tmux = Tmux::start(&sb, "-m semantic kafka");
    tmux.wait_for("semantic (");
    assert_eq!(embedded(), 3, "embedded before the first search");
    assert!(!log.exists(), "not in the background");
    drop(tmux);

    sb.session("new").user("one more").write();
    let tmux = Tmux::start(&sb, "");
    tmux.wait_for("fuzzy (4/4)");
    let deadline = Instant::now() + Duration::from_secs(10);
    while embedded() < 4 {
        assert!(
            Instant::now() < deadline,
            "never embedded in the background"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(tmux);
}

struct Tmux {
    socket: std::path::PathBuf,
}

impl Tmux {
    /// The picker, run as `claude-resume <args>`.
    fn start(sb: &Sandbox, args: &str) -> Self {
        let socket = sb.root.join("tmux.sock");
        let path = format!(
            "{}:{}",
            sb.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let script = format!(
            "cd '{root}' && env HOME='{home}' PATH='{path}' SHELL=/bin/sh TZ=UTC FAKE_CLAUDE_LOG='{log}' '{bin}' {args}; echo picker exited; sleep 30",
            root = sb.root.display(),
            home = sb.home.display(),
            log = sb.log.display(),
            bin = common::bin().display(),
        );
        let status = Command::new("tmux")
            .arg("-S")
            .arg(&socket)
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-x",
                "160",
                "-y",
                "40",
                "-s",
                "t",
            ])
            .arg(script)
            .env("SHELL", "/bin/sh")
            .env_remove("TMUX")
            .status()
            .unwrap();
        assert!(status.success());
        Self { socket }
    }

    fn tmux(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .unwrap()
    }

    fn capture(&self) -> String {
        String::from_utf8_lossy(&self.tmux(&["capture-pane", "-p", "-t", "t"]).stdout).into_owned()
    }

    fn wait_for(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = self.capture();
            if screen.contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {text:?}; screen:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn send_literal(&self, text: &str) {
        self.tmux(&["send-keys", "-t", "t", "-l", text]);
    }

    fn send_key(&self, key: &str) {
        self.tmux(&["send-keys", "-t", "t", key]);
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
    }
}
