//! TUI state and input handling, independent of the terminal so it can be tested directly.

use crate::search::{Field, Hit, Mode, Searcher};
use crate::store::SessionRow;
use crate::text::Snippet;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

/// Semantic search embeds the query, so it waits for a pause in typing.
const SEMANTIC_DEBOUNCE: Duration = Duration::from_millis(250);
const IDLE_POLL: Duration = Duration::from_millis(500);
const PAGE: usize = 10;
pub const PREVIEW_SNIPPETS: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Resume(Box<SessionRow>),
    Cancel,
}

pub struct App<'a> {
    pub searcher: Searcher<'a>,
    pub query: String,
    pub mode: Mode,
    pub hits: Vec<Hit>,
    pub selected: usize,
    pub preview_scroll: u16,
    /// A message for the status line (e.g. why semantic search is unavailable).
    pub status: Option<String>,
    pub outcome: Option<Outcome>,
    /// `(session index, query)` the cached snippets belong to.
    snippets_for: Option<(usize, String)>,
    snippets: Vec<(Field, Snippet)>,
    pending_since: Option<Instant>,
}

impl<'a> App<'a> {
    pub fn new(searcher: Searcher<'a>, mode: Mode, query: &str) -> Self {
        let mut app = Self {
            searcher,
            query: query.to_string(),
            mode,
            hits: Vec::new(),
            selected: 0,
            preview_scroll: 0,
            status: None,
            outcome: None,
            snippets_for: None,
            snippets: Vec::new(),
            pending_since: None,
        };
        app.run_search();
        app
    }

    pub fn selected_session(&self) -> Option<&SessionRow> {
        self.hits
            .get(self.selected)
            .map(|h| self.searcher.session(h))
    }

    pub fn total(&self) -> usize {
        self.searcher.sessions().len()
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => self.outcome = Some(Outcome::Cancel),
            KeyCode::Char('c') if ctrl => self.outcome = Some(Outcome::Cancel),
            KeyCode::Enter => {
                if self.pending_since.is_some() {
                    self.run_search();
                }
                if let Some(s) = self.selected_session() {
                    self.outcome = Some(Outcome::Resume(Box::new(s.clone())));
                }
            }
            KeyCode::Up => self.move_by(-1),
            KeyCode::Char('k' | 'p') if ctrl => self.move_by(-1),
            KeyCode::Down | KeyCode::Tab => self.move_by(1),
            KeyCode::Char('j' | 'n') if ctrl => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(PAGE as isize)),
            KeyCode::PageDown => self.move_by(PAGE as isize),
            KeyCode::BackTab => {
                self.mode = self.mode.next();
                self.run_search();
            }
            KeyCode::Char('u') if ctrl => {
                self.preview_scroll = self.preview_scroll.saturating_sub(5)
            }
            KeyCode::Char('d') if ctrl => {
                self.preview_scroll = self.preview_scroll.saturating_add(5)
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Backspace if alt => self.delete_word(),
            KeyCode::Backspace => {
                if self.query.pop().is_some() {
                    self.query_changed();
                }
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.query.push(c);
                self.query_changed();
            }
            _ => {}
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        let line = text.lines().next().unwrap_or("");
        if !line.is_empty() {
            self.query.push_str(line);
            self.query_changed();
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_by(1),
            MouseEventKind::ScrollUp => self.move_by(-1),
            _ => {}
        }
    }

    /// Run a debounced search once typing has paused.
    pub fn tick(&mut self, now: Instant) {
        if self
            .pending_since
            .is_some_and(|since| now.duration_since(since) >= SEMANTIC_DEBOUNCE)
        {
            self.run_search();
        }
    }

    /// How long the event loop may block waiting for input.
    pub fn poll_timeout(&self, now: Instant) -> Duration {
        match self.pending_since {
            Some(since) => SEMANTIC_DEBOUNCE.saturating_sub(now.duration_since(since)),
            None => IDLE_POLL,
        }
    }

    /// Match excerpts for the selected session, computed once per (session, query).
    pub fn snippets(&mut self) -> &[(Field, Snippet)] {
        let Some(hit) = self.hits.get(self.selected) else {
            return &[];
        };
        let key = (hit.index, self.query.clone());
        if self.snippets_for.as_ref() != Some(&key) {
            let session = self.searcher.session(hit);
            self.snippets = self
                .searcher
                .snippets(session, &self.query, PREVIEW_SNIPPETS)
                .unwrap_or_default();
            self.snippets_for = Some(key);
        }
        &self.snippets
    }

    fn query_changed(&mut self) {
        if self.mode == Mode::Semantic && !self.query.trim().is_empty() {
            self.pending_since = Some(Instant::now());
        } else {
            self.run_search();
        }
    }

    pub fn run_search(&mut self) {
        self.pending_since = None;
        match self.searcher.search(&self.query, self.mode) {
            Ok(hits) => {
                self.hits = hits;
                self.status = None;
            }
            Err(e) => {
                self.hits.clear();
                self.status = Some(format!("{e:#}"));
            }
        }
        self.selected = 0;
        self.preview_scroll = 0;
    }

    fn move_by(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let last = self.hits.len() - 1;
        let next = self.selected.saturating_add_signed(delta).min(last);
        if next != self.selected {
            self.selected = next;
            self.preview_scroll = 0;
        }
    }

    fn delete_word(&mut self) {
        let trimmed = self.query.trim_end();
        let cut = trimmed.rfind([' ', '-', '_', '/']).unwrap_or(0);
        self.query.truncate(cut);
        self.query_changed();
    }
}
