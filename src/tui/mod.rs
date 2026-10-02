//! Interactive session picker.

mod app;
mod view;

pub use app::{App, Outcome, PREVIEW_SNIPPETS};
pub use view::render;

use crate::search::{Mode, Searcher};
use crate::store::SessionRow;
use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind,
};
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use std::io;
use std::time::Instant;

/// Show the picker; returns the session to resume, or `None` if the user backed out.
pub fn run(searcher: Searcher, mode: Mode, query: &str) -> Result<Option<SessionRow>> {
    let mut app = App::new(searcher, mode, query);
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    loop {
        terminal.draw(|f| render(f, &mut app))?;
        if event::poll(app.poll_timeout(Instant::now()))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => app.handle_key(key),
                Event::Paste(text) => app.handle_paste(&text),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                _ => {}
            }
        }
        app.tick(Instant::now());
        match app.outcome.take() {
            Some(Outcome::Resume(session)) => return Ok(Some(*session)),
            Some(Outcome::Cancel) => return Ok(None),
            None => {}
        }
    }
}

/// Puts the terminal into TUI mode and restores it when dropped, including during a panic.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        out.execute(EnterAlternateScreen)?;
        out.execute(EnableMouseCapture)?;
        out.execute(EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.execute(DisableBracketedPaste);
        let _ = out.execute(DisableMouseCapture);
        let _ = out.execute(LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}
