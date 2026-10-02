//! Drawing the picker: search box and session list on the left, preview on the right.

use super::app::App;
use crate::search::{MatchKind, parse_query};
use crate::store::{SessionRow, preview_head};
use crate::text::{self, Snippet};
use crate::time;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Wrap,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    const WARNING: Color = Color::LightRed;
}

const DATE_WIDTH: usize = 9; // "yesterday"
/// Narrowest column the preview wraps text into, however little room the pane has.
const MIN_WRAP: usize = 10;

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let show_preview = area.width > 80;
    let (left, right) = match area.width {
        w if w > 180 => (55, 45),
        w if w > 120 => (50, 50),
        _ => (45, 55),
    };
    let columns = Layout::horizontal(if show_preview {
        [Constraint::Percentage(left), Constraint::Percentage(right)]
    } else {
        [Constraint::Percentage(100), Constraint::Percentage(0)]
    })
    .split(area);
    let compact = area.height < 12;
    render_list_side(f, app, columns[0], compact);
    if show_preview {
        render_preview(f, app, columns[1]);
    }
}

fn render_list_side(f: &mut Frame, app: &App, area: Rect, compact: bool) {
    let rows = Layout::vertical(if compact {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ]
    })
    .split(area);

    let counts = format!("{}/{}", app.hits.len(), app.total());
    if compact {
        let suffix = format!(" │ {} │ {}", counts, app.mode.label());
        let room = (area.width as usize).saturating_sub(2 + 1);
        let suffix = if room > suffix.width() + 8 {
            suffix
        } else {
            String::new()
        };
        // The end of a long query, where the typing happens, stays in view.
        let query = tail(&app.query, room - suffix.width());
        f.render_widget(
            Paragraph::new(format!("> {query}{suffix}")).style(Style::default().fg(Theme::PRIMARY)),
            rows[0],
        );
        f.set_cursor_position((rows[0].x + 2 + query.width() as u16, rows[0].y));
    } else {
        let query = tail(&app.query, (rows[0].width as usize).saturating_sub(2 + 1));
        let input = Paragraph::new(query).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Theme::BORDER_ACTIVE))
                .title(format!(" {} ({}) ", app.mode.label(), counts)),
        );
        f.render_widget(input, rows[0]);
        f.set_cursor_position((rows[0].x + 1 + query.width() as u16, rows[0].y + 1));
    }

    let inner_width = area.width.saturating_sub(if compact { 0 } else { 2 }) as usize;
    let remaining = inner_width.saturating_sub(3 + DATE_WIDTH + 2);
    let project_width = 14.min(remaining / 3);
    let title_width = remaining.saturating_sub(project_width + 1);
    let header = format!(
        "   {:<DATE_WIDTH$} {:<project_width$} {}",
        "date", "project", "title"
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            header,
            Style::default()
                .fg(Theme::MUTED)
                .add_modifier(Modifier::ITALIC),
        )),
        rows[1],
    );

    let list_area = rows[2];
    let visible = if compact {
        list_area.height
    } else {
        list_area.height.saturating_sub(2)
    } as usize;
    let offset = (app.selected + 1).saturating_sub(visible);
    let now = time::now();
    let highlight = HighlightTerms::new(&app.query);
    let items: Vec<ListItem> = app
        .hits
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(i, hit)| {
            let selected = i == app.selected;
            list_item(
                app.searcher.session(hit),
                hit.kind,
                &highlight,
                selected,
                project_width,
                title_width,
                now,
            )
        })
        .collect();
    let block = if compact {
        Block::default()
    } else {
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::BORDER_ACTIVE))
    };
    f.render_widget(List::new(items).block(block), list_area);

    if app.hits.len() > visible && !compact {
        let bar_area = Rect {
            x: list_area.right() - 1,
            y: list_area.y + 1,
            width: 1,
            height: list_area.height.saturating_sub(2),
        };
        let mut state = ScrollbarState::new(app.hits.len()).position(app.selected);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("█"),
            bar_area,
            &mut state,
        );
    }

    if !compact {
        let line = match &app.status {
            Some(msg) => Line::from(Span::styled(
                msg.clone(),
                Style::default().fg(Theme::WARNING),
            )),
            None => hints(app),
        };
        f.render_widget(Paragraph::new(line), rows[3]);
    }
}

fn hints(app: &App) -> Line<'static> {
    let key = |k: &str| {
        Span::styled(
            k.to_string(),
            Style::default()
                .fg(Theme::PRIMARY)
                .add_modifier(Modifier::BOLD),
        )
    };
    let text = |t: &str| Span::styled(t.to_string(), Style::default().fg(Theme::MUTED));
    Line::from(vec![
        key("esc"),
        text(" quit │ "),
        key("enter"),
        text(" resume │ "),
        key("↑↓"),
        text(" move │ "),
        key("^u/^d"),
        text(" scroll │ "),
        key("⇧tab"),
        text(" mode: "),
        Span::styled(
            app.mode.label().to_string(),
            Style::default().fg(Theme::ACCENT),
        ),
    ])
}

fn list_item<'a>(
    s: &SessionRow,
    kind: MatchKind,
    highlight: &HighlightTerms,
    selected: bool,
    project_width: usize,
    title_width: usize,
    now: i64,
) -> ListItem<'a> {
    let bg = if selected {
        Theme::SELECTED_BG
    } else {
        Color::Reset
    };
    let date = format!("{:<DATE_WIDTH$} ", time::relative(s.updated, now));
    let project = format!("{:<project_width$} ", fit(&s.project, project_width));
    let title = text::truncate(&s.title, title_width);
    let mut title_style = Style::default().fg(Theme::PRIMARY).bg(bg);
    if selected {
        title_style = title_style.add_modifier(Modifier::BOLD);
    }
    let mut spans = vec![
        Span::styled(
            if selected { " > " } else { "   " },
            Style::default().fg(Theme::ACCENT).bg(bg),
        ),
        Span::styled(
            date,
            Style::default()
                .fg(if selected {
                    Theme::SECONDARY
                } else {
                    Theme::MUTED
                })
                .bg(bg),
        ),
        Span::styled(project, Style::default().fg(Theme::ACCENT).bg(bg)),
    ];
    spans.extend(match kind {
        MatchKind::Title if highlight.exact(&title).is_none() => {
            highlight.fuzzy(&title, title_style)
        }
        _ => highlight.spans(&title, title_style),
    });
    ListItem::new(Line::from(spans))
}

fn render_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::BORDER_INACTIVE))
        .title(" preview ");
    let Some(session) = app.selected_session().cloned() else {
        let msg = if app.status.is_some() {
            "Search unavailable — see the status line"
        } else {
            "No matching sessions"
        };
        f.render_widget(
            Paragraph::new(format!(" {msg}"))
                .style(Style::default().fg(Theme::MUTED))
                .block(block),
            area,
        );
        return;
    };
    let query = app.query.clone();
    let highlight = HighlightTerms::new(&query);
    let snippets = app.snippets().to_vec();
    let width = area.width.saturating_sub(2) as usize;
    let lines = preview_lines(&session, &snippets, &highlight, time::now(), width);
    // Stop at the last page, so ^u works straight away after scrolling past the end.
    let last_page = lines
        .len()
        .saturating_sub(area.height.saturating_sub(2) as usize);
    app.preview_scroll = app.preview_scroll.min(last_page as u16);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((app.preview_scroll, 0)),
        area,
    );
}

/// The preview pane's content, wrapped to `width` columns (public for tests).
pub fn preview_lines(
    s: &SessionRow,
    snippets: &[(crate::search::Field, Snippet)],
    highlight: &HighlightTerms,
    now: i64,
    width: usize,
) -> Vec<Line<'static>> {
    let muted = Style::default().fg(Theme::MUTED);
    let indent = || vec![Span::raw(" ")];
    let mut lines = hang(
        indent(),
        vec![Span::styled(
            s.title.clone(),
            Style::default()
                .fg(Theme::PRIMARY)
                .add_modifier(Modifier::BOLD),
        )],
        width,
    );
    let mut place = vec![Span::styled(
        if s.project.is_empty() {
            "?".to_string()
        } else {
            s.project.clone()
        },
        Style::default().fg(Theme::ACCENT),
    )];
    if !s.branch.is_empty() {
        place.push(Span::styled(format!(" · ⎇ {}", s.branch), muted));
    }
    lines.extend(hang(indent(), place, width));
    let activity = format!(
        "{} prompt{} · {}",
        s.prompt_count,
        if s.prompt_count == 1 { "" } else { "s" },
        time::span(s.created, s.updated, now)
    );
    lines.extend(hang(indent(), vec![Span::styled(activity, muted)], width));
    lines.extend(hang(
        indent(),
        vec![Span::styled(s.cwd.clone(), muted)],
        width,
    ));
    for pr in &s.pr_links {
        lines.extend(hang(
            vec![Span::styled(" PR ", muted)],
            vec![Span::styled(pr.clone(), muted)],
            width,
        ));
    }
    lines.push(Line::from(Span::styled(format!(" id {}", s.sid), muted)));
    lines.push(Line::default());

    if !snippets.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" Matches ({}):", snippets.len()),
            Style::default()
                .fg(Theme::HIGHLIGHT)
                .add_modifier(Modifier::BOLD),
        )));
        for (field, snippet) in snippets {
            lines.extend(hang(
                vec![Span::styled(format!(" {:>6} › ", field.label()), muted)],
                snippet_spans(snippet, Style::default().fg(Theme::SECONDARY)),
                width,
            ));
        }
        lines.push(Line::from(Span::styled(
            " ──────────────────────────────────────",
            muted,
        )));
    }

    lines.push(Line::from(Span::styled(
        " Prompts:",
        Style::default()
            .fg(Theme::PRIMARY)
            .add_modifier(Modifier::BOLD),
    )));
    if s.preview.is_empty() {
        lines.push(Line::from(Span::styled("  (none)", muted)));
    }
    let total = s.prompt_count as usize;
    let gap = total.saturating_sub(s.preview.len());
    for (i, prompt) in s.preview.iter().enumerate() {
        let number = if gap > 0 && i >= preview_head() {
            total - s.preview.len() + i + 1
        } else {
            i + 1
        };
        if gap > 0 && i == preview_head() {
            lines.push(Line::from(Span::styled(format!("  … {gap} more"), muted)));
        }
        lines.extend(hang(
            vec![Span::styled(format!(" {number:>3}. "), muted)],
            highlight.spans(prompt, Style::default().fg(Theme::SECONDARY)),
            width,
        ));
    }
    lines
}

/// `content` broken into lines of at most `width` columns between words, after `prefix` on the
/// first line and indented by its width on the rest (so wrapped prompts line up under their text).
fn hang(
    prefix: Vec<Span<'static>>,
    content: Vec<Span<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    let indent: usize = prefix.iter().map(|s| s.content.width()).sum();
    let room = width.saturating_sub(indent).max(MIN_WRAP);
    // Words, which may span several styles (a highlighted term inside a word), and the blanks
    // between them.
    let mut pieces: Vec<(bool, Vec<(String, Style)>)> = Vec::new();
    for span in &content {
        for token in tokens(&span.content) {
            let blank = token.starts_with(char::is_whitespace);
            match pieces.last_mut() {
                Some((false, parts)) if !blank => parts.push((token.to_string(), span.style)),
                _ => pieces.push((blank, vec![(token.to_string(), span.style)])),
            }
        }
    }
    let mut lines = Vec::new();
    let mut line = prefix;
    let mut used = 0;
    let mut wrap = |line: &mut Vec<Span<'static>>, used: &mut usize| {
        while line
            .last()
            .is_some_and(|s| s.content.trim().is_empty() && *used > 0)
        {
            line.pop();
        }
        lines.push(Line::from(std::mem::replace(
            line,
            vec![Span::raw(" ".repeat(indent))],
        )));
        *used = 0;
    };
    for (blank, parts) in pieces {
        let w: usize = parts.iter().map(|(t, _)| t.width()).sum();
        if blank {
            // A line never starts with a blank, and one at a break is dropped.
            if used > 0 && used + w < room {
                used += w;
                line.extend(parts.into_iter().map(|(t, style)| Span::styled(t, style)));
            }
            continue;
        }
        if used > 0 && used + w > room {
            wrap(&mut line, &mut used);
        }
        for (part, style) in parts {
            let mut rest = part.as_str();
            // Only a word longer than a whole line gets split.
            while used + rest.width() > room {
                let cut = width_index(rest, room - used).max(if used == 0 {
                    rest.chars().next().map_or(0, char::len_utf8)
                } else {
                    0
                });
                if cut > 0 {
                    line.push(Span::styled(rest[..cut].to_string(), style));
                    used += rest[..cut].width();
                }
                rest = &rest[cut..];
                wrap(&mut line, &mut used);
            }
            if !rest.is_empty() {
                used += rest.width();
                line.push(Span::styled(rest.to_string(), style));
            }
        }
    }
    lines.push(Line::from(line));
    lines
}

/// Runs of whitespace and of everything else.
fn tokens(s: &str) -> impl Iterator<Item = &str> {
    let mut rest = s;
    std::iter::from_fn(move || {
        let first = rest.chars().next()?;
        let blank = first.is_whitespace();
        let end = rest
            .char_indices()
            .find(|(_, c)| c.is_whitespace() != blank)
            .map_or(rest.len(), |(i, _)| i);
        let (token, tail) = rest.split_at(end);
        rest = tail;
        Some(token)
    })
}

/// Byte length of the longest start of `s` that fits in `columns`.
fn width_index(s: &str, columns: usize) -> usize {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        used += c.width().unwrap_or(0);
        if used > columns {
            return i;
        }
    }
    s.len()
}

/// The longest end of `s` that fits in `width` columns.
fn tail(s: &str, width: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices().rev() {
        used += c.width().unwrap_or(0);
        if used > width {
            return &s[i + c.len_utf8()..];
        }
    }
    s
}

/// `s` cut to `width` characters, ending in an ellipsis when something was cut.
fn fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        text::truncate(s, width.saturating_sub(1))
    }
}

fn snippet_spans(snippet: &Snippet, base: Style) -> Vec<Span<'static>> {
    let hl = Style::default()
        .fg(Theme::HIGHLIGHT)
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut last = 0;
    for r in &snippet.highlights {
        if r.start > last {
            spans.push(Span::styled(snippet.text[last..r.start].to_string(), base));
        }
        spans.push(Span::styled(snippet.text[r.clone()].to_string(), hl));
        last = r.end;
    }
    if last < snippet.text.len() {
        spans.push(Span::styled(snippet.text[last..].to_string(), base));
    }
    spans
}

/// Highlights the query's terms in list titles and prompts.
pub struct HighlightTerms {
    regex: Option<regex::Regex>,
    positive: String,
}

impl HighlightTerms {
    pub fn new(query: &str) -> Self {
        let terms: Vec<String> = parse_query(query)
            .into_iter()
            .filter(|t| !t.negated)
            .map(|t| t.text)
            .collect();
        Self {
            regex: text::terms_regex(&terms),
            positive: terms.join(" "),
        }
    }

    fn exact(&self, s: &str) -> Option<()> {
        self.regex.as_ref()?.find(s).map(|_| ())
    }

    pub fn spans(&self, s: &str, base: Style) -> Vec<Span<'static>> {
        let Some(re) = &self.regex else {
            return vec![Span::styled(s.to_string(), base)];
        };
        let hl = base.fg(Theme::HIGHLIGHT).add_modifier(Modifier::BOLD);
        let mut spans = Vec::new();
        let mut last = 0;
        for m in re.find_iter(s) {
            if m.start() > last {
                spans.push(Span::styled(s[last..m.start()].to_string(), base));
            }
            spans.push(Span::styled(m.as_str().to_string(), hl));
            last = m.end();
        }
        if last < s.len() {
            spans.push(Span::styled(s[last..].to_string(), base));
        }
        spans
    }

    /// Character-level highlight of a fuzzy title match.
    fn fuzzy(&self, s: &str, base: Style) -> Vec<Span<'static>> {
        let pattern = Pattern::new(
            &self.positive,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let mut indices = Vec::new();
        let haystack = Utf32Str::new(s, &mut buf);
        if self.positive.is_empty()
            || pattern
                .indices(haystack, &mut matcher, &mut indices)
                .is_none()
        {
            return vec![Span::styled(s.to_string(), base)];
        }
        let matched: std::collections::HashSet<u32> = indices.into_iter().collect();
        // nucleo counts bytes when every grapheme starts with an ASCII character, and
        // graphemes otherwise (`❤️` is two chars but one position).
        let units: Vec<(usize, &str)> = match haystack {
            Utf32Str::Ascii(_) => s
                .char_indices()
                .map(|(i, c)| (i, &s[i..i + c.len_utf8()]))
                .collect(),
            Utf32Str::Unicode(_) => s.graphemes(true).enumerate().collect(),
        };
        let hl = base.fg(Theme::HIGHLIGHT).add_modifier(Modifier::BOLD);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut run_hl = false;
        for (position, unit) in units {
            let is_hl = matched.contains(&(position as u32));
            if is_hl != run_hl && !run.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut run),
                    if run_hl { hl } else { base },
                ));
            }
            run_hl = is_hl;
            run.push_str(unit);
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, if run_hl { hl } else { base }));
        }
        spans
    }
}
