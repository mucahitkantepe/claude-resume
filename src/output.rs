//! Rendering `search` results as text (for people) or JSON (for scripts).

use crate::resume;
use crate::search::{Field, Hit, MatchKind, Searcher};
use crate::store::SessionRow;
use crate::time;
use anyhow::Result;
use serde::Serialize;
use std::io::Write;

/// Snippets shown per result.
pub const SNIPPETS_PER_RESULT: usize = 3;

#[derive(Serialize)]
pub struct JsonResult<'a> {
    #[serde(flatten)]
    pub session: &'a SessionRow,
    #[serde(rename = "match")]
    pub kind: MatchKind,
    pub score: f32,
    pub resume_command: String,
    pub snippets: Vec<JsonSnippet>,
}

#[derive(Serialize)]
pub struct JsonSnippet {
    pub field: Field,
    /// The excerpt with matches wrapped in `**…**`.
    pub text: String,
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    query: &'a str,
    total: usize,
    results: Vec<JsonResult<'a>>,
}

pub fn write_json(
    out: &mut impl Write,
    searcher: &Searcher,
    query: &str,
    hits: &[Hit],
    max: usize,
) -> Result<()> {
    let results = hits
        .iter()
        .take(max)
        .map(|hit| {
            let session = searcher.session(hit);
            Ok(JsonResult {
                session,
                kind: hit.kind,
                score: hit.score,
                resume_command: resume_command(session),
                snippets: searcher
                    .snippets(session, query, SNIPPETS_PER_RESULT)?
                    .into_iter()
                    .map(|(field, s)| JsonSnippet {
                        field,
                        text: s.marked("**", "**"),
                    })
                    .collect(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    serde_json::to_writer_pretty(
        &mut *out,
        &JsonOutput {
            query,
            total: hits.len(),
            results,
        },
    )?;
    writeln!(out)?;
    Ok(())
}

pub fn write_text(
    out: &mut impl Write,
    searcher: &Searcher,
    query: &str,
    hits: &[Hit],
    max: usize,
) -> Result<()> {
    if hits.is_empty() {
        writeln!(out, "No sessions match {query:?}.")?;
        return Ok(());
    }
    let shown = hits.len().min(max);
    if shown < hits.len() {
        writeln!(
            out,
            "{} sessions match {query:?} (showing {shown}):\n",
            hits.len()
        )?;
    } else {
        writeln!(
            out,
            "{} {} {query:?}:\n",
            hits.len(),
            if hits.len() == 1 {
                "session matches"
            } else {
                "sessions match"
            }
        )?;
    }
    let now = time::now();
    for (n, hit) in hits.iter().take(max).enumerate() {
        let s = searcher.session(hit);
        writeln!(out, "{}. {}", n + 1, s.title)?;
        let mut meta = vec![if s.project.is_empty() {
            "?".to_string()
        } else {
            s.project.clone()
        }];
        if !s.branch.is_empty() {
            meta.push(s.branch.clone());
        }
        meta.push(format!(
            "{} prompt{}",
            s.prompt_count,
            if s.prompt_count == 1 { "" } else { "s" }
        ));
        meta.push(time::span(s.created, s.updated, now));
        match hit.kind {
            MatchKind::Near => meta.push("near match".into()),
            MatchKind::Semantic => meta.push(format!("similarity {:.2}", hit.score)),
            _ => {}
        }
        writeln!(out, "   {}", meta.join(" · "))?;
        for (field, snippet) in searcher.snippets(s, query, SNIPPETS_PER_RESULT)? {
            writeln!(
                out,
                "   {:>6} › {}",
                field.label(),
                snippet.marked("**", "**")
            )?;
        }
        writeln!(out, "   id: {}", s.sid)?;
        writeln!(out, "   resume: {}\n", resume_command(s))?;
    }
    Ok(())
}

/// The command that resumes `s` in its original directory.
pub fn resume_command(s: &SessionRow) -> String {
    match resume::plan(&s.sid, &s.cwd) {
        Ok(plan) => plan.command_line(),
        Err(_) => format!("claude --resume {}", resume::shell_quote(&s.sid)),
    }
}
