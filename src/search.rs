//! Query parsing and the three search modes.
//!
//! * **exact**: every term must occur (case-insensitive substring) somewhere in the session.
//! * **fuzzy** (default): typo-tolerant match on titles, then exact matches in the
//!   conversation, then near matches (one extra or swapped character in longer words).
//! * **semantic**: nearest sessions by meaning, using locally computed embeddings.
//!
//! Terms are separated by spaces and must all match (in any order, anywhere in the session);
//! sessions where they appear together as typed rank first. `"quoted text"` is matched as one
//! phrase and `-term` excludes sessions containing it.

#[cfg(feature = "semantic")]
use crate::semantic;
use crate::store::{SessionRow, Store};
use crate::text::{self, Snippet};
use anyhow::Result;
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Shortest term the trigram index can look up. Shorter terms only match titles, projects
/// and branches (a 1–2 letter substring of every conversation would match nearly everything).
pub const MIN_INDEXED_CHARS: usize = 3;
/// Shortest term that gets near-match (typo) tolerance. Shorter words produce too many
/// accidental hits (`gradle` minus one letter is `grade`, which matches every `upgrade`).
pub const NEAR_MIN_CHARS: usize = 7;
/// Characters of context on each side of a match in snippets.
pub const SNIPPET_RADIUS: usize = 80;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Exact,
    #[default]
    Fuzzy,
    Semantic,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Exact => "exact",
            Mode::Fuzzy => "fuzzy",
            Mode::Semantic => "semantic",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Mode::Exact => Mode::Fuzzy,
            Mode::Fuzzy => Mode::Semantic,
            Mode::Semantic => Mode::Exact,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub text: String,
    pub negated: bool,
}

impl Term {
    fn indexed(&self) -> bool {
        self.text.chars().count() >= MIN_INDEXED_CHARS
    }
}

/// Split a query into terms: whitespace separated, `"…"` for phrases, `-` prefix to exclude.
/// A word starting with `--` (a command-line flag such as `--force`) is searched for as is.
pub fn parse_query(query: &str) -> Vec<Term> {
    let mut terms = Vec::new();
    let mut chars = query.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            break;
        }
        let mut ahead = chars.clone();
        let negated = ahead.next() == Some('-') && ahead.next() != Some('-');
        if negated {
            chars.next();
        }
        let mut text = String::new();
        if chars.next_if_eq(&'"').is_some() {
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                text.push(c);
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                text.push(c);
            }
        }
        let text = text.trim();
        if !text.is_empty() {
            terms.push(Term {
                text: text.to_string(),
                negated,
            });
        }
    }
    terms
}

/// Why a session is in the results. Results are ordered by this, then by score or recency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchKind {
    /// Every term is in the title, project or branch (fuzzy mode: a fuzzy title match).
    Title,
    /// The terms occur side by side, in the order typed, in the conversation.
    Phrase,
    /// Every term occurs in the conversation.
    Content,
    /// Matched only after tolerating one extra or swapped character.
    Near,
    /// Close in meaning (semantic mode).
    Semantic,
    /// Empty query: every session, most recent first.
    Recent,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Index into [`Searcher::sessions`].
    pub index: usize,
    pub kind: MatchKind,
    /// Fuzzy title score or semantic similarity; 0 when ordering is by recency alone.
    pub score: f32,
}

/// Which part of a session a snippet comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Field {
    Prompt,
    Reply,
    Tool,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Prompt => "you",
            Field::Reply => "claude",
            Field::Tool => "tool",
        }
    }
}

pub struct Searcher<'a> {
    store: &'a Store,
    sessions: Vec<SessionRow>,
    /// Lower-cased `title \n project \n branch` per session, for short terms and title matches.
    meta: Vec<String>,
    by_id: HashMap<i64, usize>,
    #[cfg_attr(not(feature = "semantic"), allow(dead_code))]
    models: PathBuf,
    #[cfg(feature = "semantic")]
    engine: std::cell::OnceCell<Result<semantic::Engine, String>>,
}

impl<'a> Searcher<'a> {
    pub fn new(store: &'a Store, models: PathBuf) -> Result<Self> {
        let sessions = store.sessions()?;
        let meta = sessions
            .iter()
            .map(|s| format!("{}\n{}\n{}", s.title, s.project, s.branch).to_lowercase())
            .collect();
        let by_id = sessions
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id, i))
            .collect();
        Ok(Self {
            store,
            sessions,
            meta,
            by_id,
            models,
            #[cfg(feature = "semantic")]
            engine: std::cell::OnceCell::new(),
        })
    }

    /// Every indexed session, most recently active first.
    pub fn sessions(&self) -> &[SessionRow] {
        &self.sessions
    }

    pub fn session(&self, hit: &Hit) -> &SessionRow {
        &self.sessions[hit.index]
    }

    pub fn search(&self, query: &str, mode: Mode) -> Result<Vec<Hit>> {
        let terms = parse_query(query);
        if terms.is_empty() {
            return Ok(self.recent());
        }
        match mode {
            Mode::Exact => self.exact(&terms),
            Mode::Fuzzy => self.fuzzy(&terms),
            Mode::Semantic => self.semantic(query, &terms),
        }
    }

    fn recent(&self) -> Vec<Hit> {
        (0..self.sessions.len())
            .map(|index| Hit {
                index,
                kind: MatchKind::Recent,
                score: 0.0,
            })
            .collect()
    }

    fn exact(&self, terms: &[Term]) -> Result<Vec<Hit>> {
        let candidates = self.content_matches(terms, false)?;
        let phrase = self.phrase_matches(terms)?;
        let mut hits: Vec<Hit> = (0..self.sessions.len())
            .filter(|&i| {
                candidates
                    .as_ref()
                    .is_none_or(|c| c.contains(&self.sessions[i].id))
            })
            .filter(|&i| self.short_terms_match(i, terms))
            .map(|index| {
                let kind = if self.all_in_meta(index, terms) {
                    MatchKind::Title
                } else if phrase.contains(&self.sessions[index].id) {
                    MatchKind::Phrase
                } else {
                    MatchKind::Content
                };
                Hit {
                    index,
                    kind,
                    score: 0.0,
                }
            })
            .collect();
        // `sessions` is already newest-first, and the sort is stable.
        hits.sort_by_key(|h| h.kind);
        Ok(hits)
    }

    fn fuzzy(&self, terms: &[Term]) -> Result<Vec<Hit>> {
        let excluded = self.excluded(terms)?;
        let mut seen: HashSet<usize> = HashSet::new();
        let mut hits = Vec::new();

        let positive: Vec<&str> = terms
            .iter()
            .filter(|t| !t.negated)
            .map(|t| t.text.as_str())
            .collect();
        if !positive.is_empty() {
            let matcher = TitleMatcher::new(&positive);
            let mut titles: Vec<Hit> = Vec::new();
            for (index, s) in self.sessions.iter().enumerate() {
                if excluded.contains(&s.id) {
                    continue;
                }
                if let Some(score) =
                    matcher.score(&format!("{} {} {}", s.title, s.project, s.branch))
                {
                    titles.push(Hit {
                        index,
                        kind: MatchKind::Title,
                        score,
                    });
                }
            }
            titles.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.index.cmp(&b.index)));
            seen.extend(titles.iter().map(|h| h.index));
            hits.extend(titles);
        }

        for hit in self.exact(terms)? {
            if seen.insert(hit.index) {
                hits.push(hit);
            }
        }

        if let Some(ids) = self.content_matches(terms, true)? {
            let near: Vec<usize> = ids
                .iter()
                .filter_map(|id| self.by_id.get(id).copied())
                .collect();
            let mut near: Vec<usize> = near
                .into_iter()
                .filter(|&i| !seen.contains(&i) && !excluded.contains(&self.sessions[i].id))
                .filter(|&i| self.short_terms_match(i, terms))
                .collect();
            near.sort_unstable();
            hits.extend(near.into_iter().map(|index| Hit {
                index,
                kind: MatchKind::Near,
                score: 0.0,
            }));
        }
        Ok(hits)
    }

    #[cfg(feature = "semantic")]
    fn semantic(&self, query: &str, terms: &[Term]) -> Result<Vec<Hit>> {
        let engine = self
            .engine
            .get_or_init(|| {
                semantic::Engine::load(self.store, &self.models).map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let excluded = self.excluded(terms)?;
        let sid_index: HashMap<&str, usize> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| (s.sid.as_str(), i))
            .collect();
        let ranked = engine.rank(query)?;
        Ok(ranked
            .into_iter()
            .filter_map(|(sid, score)| sid_index.get(sid.as_str()).map(|&index| (index, score)))
            .filter(|(index, _)| !excluded.contains(&self.sessions[*index].id))
            .map(|(index, score)| Hit {
                index,
                kind: MatchKind::Semantic,
                score,
            })
            .collect())
    }

    #[cfg(not(feature = "semantic"))]
    fn semantic(&self, _query: &str, _terms: &[Term]) -> Result<Vec<Hit>> {
        anyhow::bail!(
            "this build of claude-resume has no semantic search (built without the `semantic` feature)"
        )
    }

    /// Sessions whose conversation contains every indexed positive term (or, with `near`, a
    /// one-edit variant of it) and no excluded term. `None` when no term can use the index.
    fn content_matches(&self, terms: &[Term], near: bool) -> Result<Option<HashSet<i64>>> {
        let positive: Vec<String> = terms
            .iter()
            .filter(|t| !t.negated && t.indexed())
            .map(|t| {
                if near {
                    near_group(&t.text)
                } else {
                    fts_string(&t.text)
                }
            })
            .collect();
        if near
            && !terms
                .iter()
                .any(|t| !t.negated && t.text.chars().count() >= NEAR_MIN_CHARS)
        {
            return Ok(None);
        }
        let negative: Vec<String> = terms
            .iter()
            .filter(|t| t.negated && t.indexed())
            .map(|t| fts_string(&t.text))
            .collect();
        match (positive.is_empty(), negative.is_empty()) {
            (true, true) => Ok(None),
            (true, false) => {
                let excluded = self.store.fts_ids(&negative.join(" OR "))?;
                Ok(Some(
                    self.sessions
                        .iter()
                        .map(|s| s.id)
                        .filter(|id| !excluded.contains(id))
                        .collect(),
                ))
            }
            (false, true) => Ok(Some(self.store.fts_ids(&positive.join(" AND "))?)),
            (false, false) => Ok(Some(self.store.fts_ids(&format!(
                "({}) NOT ({})",
                positive.join(" AND "),
                negative.join(" OR ")
            ))?)),
        }
    }

    /// Sessions whose conversation has the positive terms side by side, in the order typed, so
    /// that of the hundreds of sessions mentioning kafka, consumer and lag, the ones about
    /// "kafka consumer lag" come first. Only for indexed terms, which makes these a subset of
    /// the sessions containing every term.
    fn phrase_matches(&self, terms: &[Term]) -> Result<HashSet<i64>> {
        let positive: Vec<&Term> = terms.iter().filter(|t| !t.negated).collect();
        if positive.len() < 2 || !positive.iter().all(|t| t.indexed()) {
            return Ok(HashSet::new());
        }
        let phrase: Vec<&str> = positive.iter().map(|t| t.text.as_str()).collect();
        self.store.fts_ids(&fts_string(&phrase.join(" ")))
    }

    /// Ids of sessions any negated term rules out.
    fn excluded(&self, terms: &[Term]) -> Result<HashSet<i64>> {
        let mut out = HashSet::new();
        let indexed: Vec<String> = terms
            .iter()
            .filter(|t| t.negated && t.indexed())
            .map(|t| fts_string(&t.text))
            .collect();
        if !indexed.is_empty() {
            out.extend(self.store.fts_ids(&indexed.join(" OR "))?);
        }
        for t in terms.iter().filter(|t| t.negated && !t.indexed()) {
            let needle = t.text.to_lowercase();
            out.extend(
                self.meta
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| m.contains(&needle))
                    .map(|(i, _)| self.sessions[i].id),
            );
        }
        Ok(out)
    }

    /// Terms too short for the index must appear in the title, project or branch.
    fn short_terms_match(&self, index: usize, terms: &[Term]) -> bool {
        terms
            .iter()
            .filter(|t| !t.indexed())
            .all(|t| self.meta[index].contains(&t.text.to_lowercase()) != t.negated)
    }

    fn all_in_meta(&self, index: usize, terms: &[Term]) -> bool {
        terms
            .iter()
            .filter(|t| !t.negated)
            .all(|t| self.meta[index].contains(&t.text.to_lowercase()))
    }

    /// Excerpts around the query terms, from the user's prompts first, then Claude's replies,
    /// then tool activity. Where the terms appear together as typed, only those places are shown
    /// (they are why the session ranks high); otherwise each term, or else its near variants.
    pub fn snippets(
        &self,
        session: &SessionRow,
        query: &str,
        max: usize,
    ) -> Result<Vec<(Field, Snippet)>> {
        let terms: Vec<String> = parse_query(query)
            .into_iter()
            .filter(|t| !t.negated)
            .map(|t| t.text)
            .collect();
        let Some(content) = self.store.content(session.id)? else {
            return Ok(Vec::new());
        };
        let fields = [
            (Field::Prompt, &content.prompts),
            (Field::Reply, &content.replies),
            (Field::Tool, &content.tools),
        ];
        let collect = |words: &[String]| -> Vec<(Field, Snippet)> {
            let Some(re) = text::terms_regex(words) else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for (field, body) in fields {
                for s in text::snippets(body, &re, SNIPPET_RADIUS, max - out.len()) {
                    out.push((field, s));
                }
                if out.len() >= max {
                    break;
                }
            }
            out
        };
        if terms.len() > 1 {
            let together = collect(&[terms.join(" ")]);
            if !together.is_empty() {
                return Ok(together);
            }
        }
        let found = collect(&terms);
        if !found.is_empty() {
            return Ok(found);
        }
        let variants: Vec<String> = terms.iter().flat_map(|t| near_variants(t)).collect();
        Ok(collect(&variants))
    }
}

/// Typo-tolerant title matching: every term must match as a *compact* subsequence, i.e. its
/// letters appear in order with at most a few others in between (`kfka` → `Kafka`,
/// `rtention` → `retention`). Letters scattered across a long title don't count, which is what
/// lets plain fuzzy scoring match `gradle` against "Grand plan for the deadline".
pub struct TitleMatcher {
    atoms: Vec<(Atom, usize)>,
    matcher: std::cell::RefCell<(Matcher, Vec<char>, Vec<u32>)>,
}

impl TitleMatcher {
    pub fn new(terms: &[&str]) -> Self {
        let atoms = terms
            .iter()
            .map(|t| {
                (
                    Atom::new(
                        t,
                        CaseMatching::Ignore,
                        Normalization::Smart,
                        AtomKind::Fuzzy,
                        false,
                    ),
                    t.chars().count(),
                )
            })
            .collect();
        Self {
            atoms,
            matcher: std::cell::RefCell::new((
                Matcher::new(Config::DEFAULT),
                Vec::new(),
                Vec::new(),
            )),
        }
    }

    /// Letters allowed between the first and last matched one, beyond the term's own length.
    fn slack(len: usize) -> usize {
        (len / 4).max(1)
    }

    /// The summed match score if every term matches compactly.
    pub fn score(&self, haystack: &str) -> Option<f32> {
        let mut guard = self.matcher.borrow_mut();
        let (matcher, buf, indices) = &mut *guard;
        let haystack = Utf32Str::new(haystack, buf);
        let mut total = 0.0;
        for (atom, len) in &self.atoms {
            indices.clear();
            let score = atom.indices(haystack, matcher, indices)?;
            let (first, last) = (*indices.iter().min()?, *indices.iter().max()?);
            if (last - first + 1) as usize > len + Self::slack(*len) {
                return None;
            }
            total += f32::from(score);
        }
        Some(total)
    }
}

/// An FTS5 string literal: matched as a substring by the trigram tokenizer.
fn fts_string(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `("term" OR "variant" …)` for near matching; plain `"term"` for short terms.
fn near_group(term: &str) -> String {
    let variants = near_variants(term);
    if variants.is_empty() {
        return fts_string(term);
    }
    let alternatives: Vec<String> = std::iter::once(term.to_string())
        .chain(variants)
        .map(|v| fts_string(&v))
        .collect();
    format!("({})", alternatives.join(" OR "))
}

/// One-edit variants of `term` that are still specific enough to search for: each single
/// character deleted (an extra keystroke) and each adjacent pair swapped (a transposition).
pub fn near_variants(term: &str) -> Vec<String> {
    let chars: Vec<char> = term.chars().collect();
    if chars.len() < NEAR_MIN_CHARS {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut push = |v: String| {
        // Dropping or moving the last letter of a phrase like "import x" leaves "import ":
        // a different, much broader search rather than a typo of the phrase.
        if v != term && v.trim() == v && !out.contains(&v) {
            out.push(v);
        }
    };
    for i in 0..chars.len() {
        push(
            chars
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, c)| *c)
                .collect(),
        );
    }
    for i in 0..chars.len() - 1 {
        let mut swapped = chars.clone();
        swapped.swap(i, i + 1);
        push(swapped.into_iter().collect());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(text: &str, negated: bool) -> Term {
        Term {
            text: text.into(),
            negated,
        }
    }

    #[test]
    fn parses_terms_phrases_and_exclusions() {
        assert_eq!(
            parse_query("  kafka   lag "),
            [t("kafka", false), t("lag", false)]
        );
        assert_eq!(
            parse_query(r#""consumer lag" -staging"#),
            [t("consumer lag", false), t("staging", true)]
        );
        assert_eq!(
            parse_query(r#"-"two words" x"#),
            [t("two words", true), t("x", false)]
        );
        assert_eq!(
            parse_query(r#""unterminated phrase"#),
            [t("unterminated phrase", false)]
        );
        assert_eq!(parse_query(r#"it's "" -"#), [t("it's", false)]);
        assert_eq!(
            parse_query(r#"--force -rf "-rf""#),
            [t("--force", false), t("rf", true), t("-rf", false)],
            "flags are searchable; a single dash excludes unless quoted"
        );
        assert!(parse_query("   ").is_empty());
    }

    #[test]
    fn fts_strings_escape_quotes() {
        assert_eq!(fts_string(r#"say "hi""#), r#""say ""hi""""#);
        assert_eq!(fts_string("a OR b"), r#""a OR b""#);
    }

    #[test]
    fn near_variants_never_start_or_end_with_a_space() {
        let variants = near_variants("import x");
        assert!(variants.contains(&"importx".to_string()));
        assert!(variants.iter().all(|v| v.trim() == v), "{variants:?}");
    }

    #[test]
    fn near_variants_cover_extra_and_swapped_characters() {
        assert!(
            near_variants("react").is_empty(),
            "short words get no typo tolerance"
        );
        let v = near_variants("karabassan");
        assert!(
            v.contains(&"karabasan".to_string()),
            "extra character removed"
        );
        let v = near_variants("postgers");
        assert!(v.contains(&"postgres".to_string()), "adjacent swap undone");
        assert!(!v.contains(&"postgers".to_string()));
        assert_eq!(
            v.len(),
            v.iter().collect::<HashSet<_>>().len(),
            "no duplicates"
        );
    }

    #[test]
    fn title_matcher_accepts_typos_but_not_scattered_letters() {
        let m = TitleMatcher::new(&["kfka"]);
        assert!(m.score("Kafka consumer lag").is_some(), "missing letter");
        assert!(
            TitleMatcher::new(&["rtention"])
                .score("Kafka retention policy")
                .is_some()
        );
        assert!(
            TitleMatcher::new(&["gradle"])
                .score("Grand plan for the deadline")
                .is_none()
        );
        assert!(
            TitleMatcher::new(&["gradle"])
                .score("great ideas for the release deadline")
                .is_none()
        );
        assert!(
            TitleMatcher::new(&["gradle"])
                .score("Migrate the Gradle build")
                .is_some()
        );
        let both = TitleMatcher::new(&["kafka", "lag"]);
        assert!(
            both.score("lag in kafka consumers").is_some(),
            "terms in any order"
        );
        assert!(
            both.score("kafka consumers").is_none(),
            "every term must match"
        );
        let exact = TitleMatcher::new(&["kafka"]).score("kafka").unwrap();
        let typo = TitleMatcher::new(&["kfka"]).score("kafka").unwrap();
        assert!(exact > typo, "exact matches rank above typos");
    }

    #[test]
    fn mode_cycles() {
        assert_eq!(Mode::Fuzzy.next(), Mode::Semantic);
        assert_eq!(Mode::Semantic.next(), Mode::Exact);
        assert_eq!(Mode::Exact.next(), Mode::Fuzzy);
    }
}
