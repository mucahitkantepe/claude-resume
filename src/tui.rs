use crate::index::{self, IndexEntry};
use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::io::{self, BufRead};

// ── Semantic color palette ──────────────────────────────────────────────────
struct Theme;
impl Theme {
    const PRIMARY: Color = Color::White;
    const SECONDARY: Color = Color::Gray;
    const MUTED: Color = Color::DarkGray;
    const ACCENT: Color = Color::Cyan;
    const HIGHLIGHT: Color = Color::Yellow;
    const SELECTED_BG: Color = Color::DarkGray;
    const BORDER_ACTIVE: Color = Color::Blue;
    const BORDER_INACTIVE: Color = Color::DarkGray;
    const POINTER: Color = Color::Cyan;
}

// ── App state ───────────────────────────────────────────────────────────────
struct App {
    entries: Vec<IndexEntry>,
    filtered: Vec<usize>,
    query: String,
    selected: usize,
    preview_scroll: u16,
    quit: bool,
    chosen: Option<usize>,
    // Cache: (entry_index, query) -> preview lines
    preview_cache: Option<(usize, String, Vec<Line<'static>>)>,
}

impl App {
    fn new(entries: Vec<IndexEntry>) -> Self {
        let filtered: Vec<usize> = (0..entries.len()).collect();
        App {
            entries,
            filtered,
            query: String::new(),
            selected: 0,
            preview_scroll: 0,
            quit: false,
            chosen: None,
            preview_cache: None,
        }
    }

    fn ensure_preview_cached(&mut self) {
        let entry_idx = match self.filtered.get(self.selected) {
            Some(&idx) => idx,
            None => return,
        };
        if let Some((cached_idx, ref cached_query, _)) = self.preview_cache {
            if cached_idx == entry_idx && *cached_query == self.query {
                return; // already cached
            }
        }
        let entry = &self.entries[entry_idx];
        let lines = build_preview_lines(entry, &self.query);
        self.preview_cache = Some((entry_idx, self.query.clone(), lines));
    }

    fn get_preview(&self) -> Option<&[Line<'static>]> {
        self.preview_cache.as_ref().map(|(_, _, l)| l.as_slice())
    }

    fn filter(&mut self) {
        if self.query.is_empty() {
            self.filtered = (0..self.entries.len()).collect();
        } else {
            let mut matcher = Matcher::new(Config::DEFAULT);
            let pattern = Pattern::new(
                &self.query,
                CaseMatching::Ignore,
                Normalization::Smart,
                AtomKind::Fuzzy,
            );
            let q_lower = self.query.to_lowercase();

            const BONUS_SHORT: u32 = 1000;
            const BONUS_CONTENT_EXACT: u32 = 500;
            const BONUS_CONTENT_NEAR: u32 = 400;
            let min_fuzzy_score = (self.query.chars().count() as u32) * 16;

            // Pre-compute near-match variants (drop each char once for 1-edit tolerance)
            let query_chars: Vec<(usize, char)> = self.query.char_indices().collect();
            let near_variants: Vec<String> = if query_chars.len() >= 5 {
                query_chars
                    .iter()
                    .map(|&(byte_idx, ch)| {
                        let mut v = self.query.clone();
                        v.replace_range(byte_idx..byte_idx + ch.len_utf8(), "");
                        v.to_lowercase()
                    })
                    .collect()
            } else {
                Vec::new()
            };

            let mut scored: Vec<(usize, u32)> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(i, e)| {
                    // Tier 1: fuzzy on short visible fields (high threshold)
                    let short = format!("{} {} {}", e.label, e.project, e.branch);
                    let mut buf = Vec::new();
                    let haystack = Utf32Str::new(&short, &mut buf);
                    if let Some(score) = pattern.score(haystack, &mut matcher) {
                        if score >= min_fuzzy_score {
                            return Some((i, score + BONUS_SHORT));
                        }
                    }

                    let content_lower = e.search_text.to_lowercase();

                    // Tier 2: exact substring on content
                    if content_lower.contains(&q_lower) {
                        return Some((i, BONUS_CONTENT_EXACT));
                    }

                    // Tier 3: near-match on content (1-edit distance tolerance)
                    for variant in &near_variants {
                        if content_lower.contains(variant.as_str()) {
                            return Some((i, BONUS_CONTENT_NEAR));
                        }
                    }

                    None
                })
                .collect();

            // Sort by score descending (fuzzy matches on top, content matches below)
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            self.filtered = scored.into_iter().map(|(i, _)| i).collect();
        }
        self.selected = 0;
        self.preview_scroll = 0;
        self.preview_cache = None;
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.preview_scroll = 0;
            self.preview_cache = None;
        }
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
            self.preview_scroll = 0;
            self.preview_cache = None;
        }
    }
}

// ── Date formatting ─────────────────────────────────────────────────────────

fn relative_date(date_str: &str) -> String {
    let parsed = match chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return date_str.to_string(),
    };
    let today = chrono::Local::now().date_naive();
    let days = (today - parsed).num_days();
    match days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        2..=6 => format!("{}d ago", days),
        7..=13 => "1w ago".to_string(),
        14..=29 => format!("{}w ago", days / 7),
        _ => parsed.format("%b %d").to_string(),
    }
}

fn format_date_long(date_str: &str) -> String {
    chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .map(|d| d.format("%a %b %d").to_string())
        .unwrap_or_else(|_| date_str.to_string())
}

// ── Highlight matched text (reusable) ───────────────────────────────────────

fn highlight_spans_in<'a>(text: &str, query: &str, base_style: Style) -> Vec<Span<'a>> {
    if query.is_empty() {
        return vec![Span::styled(text.to_string(), base_style)];
    }
    let match_style = Style::default()
        .fg(Theme::HIGHLIGHT)
        .add_modifier(Modifier::BOLD);

    // Use regex for case-insensitive matching — safe on non-ASCII
    let re = match regex::RegexBuilder::new(&regex::escape(query))
        .case_insensitive(true)
        .build()
    {
        Ok(r) => r,
        Err(_) => return vec![Span::styled(text.to_string(), base_style)],
    };

    let mut spans = Vec::new();
    let mut last = 0;
    for m in re.find_iter(text) {
        if m.start() > last {
            spans.push(Span::styled(text[last..m.start()].to_string(), base_style));
        }
        spans.push(Span::styled(
            text[m.start()..m.end()].to_string(),
            match_style,
        ));
        last = m.end();
    }
    if last < text.len() {
        spans.push(Span::styled(text[last..].to_string(), base_style));
    }
    spans
}

// ── Fuzzy highlight (character-level) ───────────────────────────────────────

fn highlight_fuzzy_spans<'a>(text: &str, query: &str, base_style: Style) -> Vec<Span<'a>> {
    if query.is_empty() {
        return vec![Span::styled(text.to_string(), base_style)];
    }

    let match_style = Style::default()
        .fg(Theme::HIGHLIGHT)
        .add_modifier(Modifier::BOLD);

    // Use nucleo to get matched character indices
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(query, CaseMatching::Ignore, Normalization::Smart, AtomKind::Fuzzy);
    let mut buf = Vec::new();
    let haystack = Utf32Str::new(text, &mut buf);
    let mut indices = Vec::new();

    if pattern.indices(haystack, &mut matcher, &mut indices).is_none() {
        return vec![Span::styled(text.to_string(), base_style)];
    }

    indices.sort_unstable();
    indices.dedup();

    // Convert char indices to a set for O(1) lookup
    let matched_chars: std::collections::HashSet<u32> = indices.into_iter().collect();

    // Build spans: group consecutive matched/unmatched chars
    let mut spans = Vec::new();
    let mut current = String::new();
    let mut current_matched = false;

    for (i, ch) in text.chars().enumerate() {
        let is_match = matched_chars.contains(&(i as u32));
        if is_match != current_matched && !current.is_empty() {
            let style = if current_matched { match_style } else { base_style };
            spans.push(Span::styled(std::mem::take(&mut current), style));
        }
        current_matched = is_match;
        current.push(ch);
    }
    if !current.is_empty() {
        let style = if current_matched { match_style } else { base_style };
        spans.push(Span::styled(current, style));
    }

    spans
}

// ── Build list item with highlights ─────────────────────────────────────────

fn build_list_item<'a>(
    entry: &IndexEntry,
    query: &str,
    is_selected: bool,
    proj_width: usize,
    label_width: usize,
) -> ListItem<'a> {
    let bg = if is_selected {
        Theme::SELECTED_BG
    } else {
        Color::Reset
    };

    // Pointer
    let pointer = if is_selected { " > " } else { "   " };
    let pointer_style = Style::default().fg(Theme::POINTER).bg(bg);

    // Date (relative) — brighter when selected so it's visible on dark bg
    let date = relative_date(&entry.modified);
    let date_padded = format!("{:<9}", date);
    let date_style = if is_selected {
        Style::default().fg(Theme::SECONDARY).bg(bg)
    } else {
        Style::default().fg(Theme::MUTED).bg(bg)
    };

    // Project
    let proj: String = entry.project.chars().take(proj_width).collect();
    let proj_padded = format!("{:<pw$} ", proj, pw = proj_width);
    let proj_style = Style::default().fg(Theme::ACCENT).bg(bg);

    // Label with match highlighting
    let label: String = entry.label.chars().take(label_width).collect();
    let label_base = if is_selected {
        Style::default()
            .fg(Theme::PRIMARY)
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Theme::PRIMARY).bg(bg)
    };

    let mut spans = vec![
        Span::styled(pointer.to_string(), pointer_style),
        Span::styled(date_padded, date_style),
        Span::styled(proj_padded, proj_style),
    ];

    // Try exact highlight first, fall back to fuzzy character highlighting
    let exact_spans = highlight_spans_in(&label, query, label_base);
    let has_exact = !query.is_empty()
        && exact_spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD) && s.style.fg == Some(Theme::HIGHLIGHT));
    if has_exact {
        spans.extend(exact_spans);
    } else if !query.is_empty() {
        spans.extend(highlight_fuzzy_spans(&label, query, label_base));
    } else {
        spans.extend(exact_spans);
    }

    ListItem::new(Line::from(spans))
}

// ── Preview pane ────────────────────────────────────────────────────────────

fn build_preview_lines(entry: &IndexEntry, query: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Compact metadata bar
    let created = format_date_long(&entry.created);
    let modified = format_date_long(&entry.modified);
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {}", entry.project),
            Style::default().fg(Theme::ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" │ {} msgs │ {} → {}", entry.msg_count, created, modified),
            Style::default().fg(Theme::MUTED),
        ),
    ]));
    if !entry.branch.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" ⎇ {}", entry.branch),
            Style::default().fg(Theme::MUTED),
        )));
    }
    lines.push(Line::from(Span::styled(
        format!(" {}", entry.cwd),
        Style::default().fg(Theme::MUTED),
    )));
    lines.push(Line::default());

    // Match contexts
    if !query.is_empty() {
        let contexts = index::match_contexts_deep(entry, query, 6);
        if !contexts.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(" Matches ({}):", contexts.len()),
                Style::default()
                    .fg(Theme::HIGHLIGHT)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::default());
            for ctx in &contexts {
                let spans = highlight_spans_in(ctx, query, Style::default().fg(Theme::SECONDARY));
                let mut prefixed = vec![Span::raw("  ".to_string())];
                prefixed.extend(spans);
                lines.push(Line::from(prefixed));
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                " ──────────────────────────────────────",
                Style::default().fg(Theme::MUTED),
            )));
            lines.push(Line::default());
        }
    }

    // User prompts from session file
    let prompts = load_user_prompts(&entry.sid, 10);
    if !prompts.is_empty() {
        lines.push(Line::from(Span::styled(
            " Conversation:",
            Style::default()
                .fg(Theme::PRIMARY)
                .add_modifier(Modifier::BOLD),
        )));
        for (i, p) in prompts.iter().enumerate() {
            let num = format!(" {}. ", i + 1);
            let mut spans = vec![Span::styled(num, Style::default().fg(Theme::MUTED))];
            spans.extend(highlight_spans_in(
                p,
                query,
                Style::default().fg(Theme::SECONDARY),
            ));
            lines.push(Line::from(spans));
        }
    } else {
        lines.push(Line::from(Span::styled(
            " First prompt:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            format!(" {}", entry.label),
            Style::default().fg(Theme::SECONDARY),
        )));
    }

    lines
}

// ── Load user prompts from .jsonl ───────────────────────────────────────────

fn load_user_prompts(sid: &str, max: usize) -> Vec<String> {
    let path = match index::find_session_file(sid) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = std::io::BufReader::new(file);
    let mut prompts = Vec::new();
    let mut lines_read = 0;

    for line in reader.lines().flatten() {
        lines_read += 1;
        if lines_read > 500 {
            break;
        }
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
            if entry.get("type").and_then(|v| v.as_str()) != Some("user") {
                continue;
            }
            if let Some(content) = entry.get("message").and_then(|m| m.get("content")) {
                let mut text = String::new();
                if let Some(s) = content.as_str() {
                    text = s.to_string();
                } else if let Some(arr) = content.as_array() {
                    for block in arr {
                        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                text = t.to_string();
                                break;
                            }
                        }
                    }
                }
                if !text.is_empty()
                    && !text.starts_with("<local-command")
                    && !text.starts_with("<command-name>/exit")
                {
                    let short: String = text
                        .replace('\n', " ")
                        .replace('\t', " ")
                        .chars()
                        .take(200)
                        .collect();
                    prompts.push(short);
                    if prompts.len() >= max {
                        break;
                    }
                }
            }
        }
    }
    prompts
}

// ── Terminal cleanup guard (RAII) ───────────────────────────────────────────

struct TerminalGuard;

impl TerminalGuard {
    fn init() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        stdout.execute(crossterm::event::EnableMouseCapture)?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = stdout.execute(crossterm::event::DisableMouseCapture);
        let _ = stdout.execute(LeaveAlternateScreen);
    }
}

// ── Main TUI loop ───────────────────────────────────────────────────────────

pub fn run(entries: Vec<IndexEntry>) -> io::Result<Option<IndexEntry>> {
    let _guard = TerminalGuard::init()?;

    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(entries);

    loop {
        // Precompute preview (uses cache to avoid re-reading files on every draw)
        app.ensure_preview_cached();
        let preview_lines = app.get_preview();
        terminal.draw(|f| ui(f, &app, preview_lines))?;

        match event::read()? {
            Event::Key(key) => match key.code {
                KeyCode::Esc => {
                    app.quit = true;
                    break;
                }
                KeyCode::Enter => {
                    if !app.filtered.is_empty() {
                        app.chosen = Some(app.filtered[app.selected]);
                    }
                    break;
                }
                KeyCode::Up | KeyCode::BackTab => app.move_up(),
                KeyCode::Down | KeyCode::Tab => app.move_down(),
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.move_up()
                }
                KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.move_down()
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.move_up()
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.move_down()
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.preview_scroll = app.preview_scroll.saturating_sub(5);
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.preview_scroll += 5;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.quit = true;
                    break;
                }
                KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                    // Option+Delete: delete word
                    let trimmed = app.query.trim_end();
                    if let Some(pos) = trimmed.rfind(|c: char| c == ' ' || c == '-' || c == '_' || c == '/') {
                        app.query.truncate(pos);
                    } else {
                        app.query.clear();
                    }
                    app.filter();
                }
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    // Ctrl+W: delete word (unix style)
                    let trimmed = app.query.trim_end();
                    if let Some(pos) = trimmed.rfind(|c: char| c == ' ' || c == '-' || c == '_' || c == '/') {
                        app.query.truncate(pos);
                    } else {
                        app.query.clear();
                    }
                    app.filter();
                }
                KeyCode::Backspace => {
                    app.query.pop();
                    app.filter();
                }
                KeyCode::Char(c) => {
                    app.query.push(c);
                    app.filter();
                }
                _ => {}
            },
            // Mouse scroll support
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => app.move_down(),
                MouseEventKind::ScrollUp => app.move_up(),
                _ => {}
            },
            _ => {}
        }
    }

    // _guard drops here, restoring terminal even on early returns or panics

    if app.quit {
        return Ok(None);
    }

    Ok(app.chosen.and_then(|i| app.entries.get(i).cloned()))
}

// ── UI renderer ─────────────────────────────────────────────────────────────

fn ui(f: &mut Frame, app: &App, preview_lines: Option<&[Line<'static>]>) {
    let size = f.area();
    let width = size.width as usize;
    let height = size.height;

    // Responsive: collapse preview on very narrow terminals
    let show_preview = width > 80;
    let (left_pct, right_pct) = if !show_preview {
        (100, 0)
    } else if width > 180 {
        (55, 45)
    } else if width > 120 {
        (50, 50)
    } else {
        (45, 55) // Narrow with preview: give preview more room
    };

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(left_pct),
            Constraint::Percentage(right_pct),
        ])
        .split(size);

    // Compact mode: skip borders on very small terminals
    let compact = height < 12;

    // Left: search + header + list + status
    let left_constraints = if compact {
        vec![
            Constraint::Length(1), // search (no border)
            Constraint::Length(1), // header
            Constraint::Min(1),   // list
        ]
    } else {
        vec![
            Constraint::Length(3), // search (with border)
            Constraint::Length(1), // header
            Constraint::Min(1),   // list
            Constraint::Length(1), // status bar
        ]
    };
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(left_constraints)
        .split(main_chunks[0]);

    // ── Search input ──
    let match_info = format!("{}/{}", app.filtered.len(), app.entries.len());
    if compact {
        let text = format!("> {} │ {}", app.query, match_info);
        let input = Paragraph::new(Span::styled(text, Style::default().fg(Theme::PRIMARY)));
        f.set_cursor_position((
            left_chunks[0].x + 2 + app.query.len() as u16,
            left_chunks[0].y,
        ));
        f.render_widget(input, left_chunks[0]);
    } else {
        let input = Paragraph::new(app.query.as_str()).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Theme::BORDER_ACTIVE))
                .title(format!(" claude-resume ({}) ", match_info)),
        );
        f.set_cursor_position((
            left_chunks[0].x + app.query.len() as u16 + 1,
            left_chunks[0].y + 1,
        ));
        f.render_widget(input, left_chunks[0]);
    }

    // ── Column widths (adaptive) ──
    let avail_width = main_chunks[0].width.saturating_sub(if compact { 0 } else { 2 }) as usize;
    let pointer_w = 3; // " > " or "   "
    let date_w = 9; // "yesterday" is the longest
    let remaining = avail_width.saturating_sub(pointer_w + date_w + 2); // 2 spaces
    let proj_width = 14.min(remaining / 3);
    let label_width = remaining.saturating_sub(proj_width + 1);

    // ── Column headers ──
    let header_idx = 1;
    let header_text = format!(
        "   {:<dw$} {:<pw$} {}",
        "date",
        "project",
        "summary",
        dw = date_w,
        pw = proj_width
    );
    let header = Paragraph::new(Span::styled(
        header_text,
        Style::default()
            .fg(Theme::MUTED)
            .add_modifier(Modifier::ITALIC),
    ));
    f.render_widget(header, left_chunks[header_idx]);

    // ── Session list ──
    let list_idx = 2;
    let list_area = left_chunks[list_idx];
    let inner_height = if compact {
        list_area.height as usize
    } else {
        list_area.height.saturating_sub(2) as usize // borders
    };
    let total = app.filtered.len();

    let scroll_offset = if app.selected >= inner_height {
        app.selected - inner_height + 1
    } else {
        0
    };

    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .skip(scroll_offset)
        .take(inner_height)
        .enumerate()
        .map(|(display_idx, &entry_idx)| {
            let e = &app.entries[entry_idx];
            let is_selected = display_idx + scroll_offset == app.selected;
            build_list_item(e, &app.query, is_selected, proj_width, label_width)
        })
        .collect();

    let list_block = if compact {
        Block::default()
    } else {
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::BORDER_ACTIVE))
    };
    let list = List::new(items).block(list_block);
    f.render_widget(list, list_area);

    // ── Scrollbar ──
    if total > inner_height && !compact {
        let scrollbar_area = Rect {
            x: list_area.right() - 1,
            y: list_area.y + 1,
            width: 1,
            height: list_area.height.saturating_sub(2),
        };
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("█");
        let mut scrollbar_state =
            ScrollbarState::new(total).position(app.selected);
        f.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    // ── Status bar (keybinding hints) ──
    if !compact {
        let status_idx = 3;
        let hints = Line::from(vec![
            Span::styled("esc", Style::default().fg(Theme::PRIMARY).add_modifier(Modifier::BOLD)),
            Span::styled(" quit ", Style::default().fg(Theme::MUTED)),
            Span::styled("│", Style::default().fg(Theme::MUTED)),
            Span::styled(" enter", Style::default().fg(Theme::PRIMARY).add_modifier(Modifier::BOLD)),
            Span::styled(" resume ", Style::default().fg(Theme::MUTED)),
            Span::styled("│", Style::default().fg(Theme::MUTED)),
            Span::styled(" ↑↓", Style::default().fg(Theme::PRIMARY).add_modifier(Modifier::BOLD)),
            Span::styled(" navigate ", Style::default().fg(Theme::MUTED)),
            Span::styled("│", Style::default().fg(Theme::MUTED)),
            Span::styled(" ^u/^d", Style::default().fg(Theme::PRIMARY).add_modifier(Modifier::BOLD)),
            Span::styled(" scroll preview", Style::default().fg(Theme::MUTED)),
        ]);
        let status = Paragraph::new(hints);
        f.render_widget(status, left_chunks[status_idx]);
    }

    // ── Right: preview ──
    if show_preview {
        let preview_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::BORDER_INACTIVE))
            .title(" preview ");

        if let Some(lines) = preview_lines {
            let preview = Paragraph::new(lines.to_vec())
                .block(preview_block)
                .wrap(Wrap { trim: false })
                .scroll((app.preview_scroll, 0));
            f.render_widget(preview, main_chunks[1]);
        } else {
            let empty = Paragraph::new(" No sessions found")
                .block(preview_block)
                .style(Style::default().fg(Theme::MUTED));
            f.render_widget(empty, main_chunks[1]);
        }
    }
}
