//! Text clean-up, truncation and match snippets.

use regex::{Regex, RegexBuilder};
use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;

/// CSI (`ESC [ … final`), OSC (`ESC ] … BEL|ST`) and two-byte escape sequences.
static ESCAPES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]").unwrap()
});

/// Strip terminal escape sequences and control characters, and normalise line endings.
/// Newlines are kept (`\r\n` and lone `\r` become `\n`) so multi-line text survives for previews.
pub fn clean(s: &str) -> String {
    let s: Cow<str> = if s.contains('\x1b') {
        ESCAPES.replace_all(s, "")
    } else {
        Cow::Borrowed(s)
    };
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' | '\n' => out.push('\n'),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Collapse every run of whitespace (newlines included) into one space and trim.
pub fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// The first `max_chars` characters of `s` (never splits a character).
pub fn cap(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// The first `head` and last `tail` characters of `s`, joined by ` … ` when the middle is cut.
pub fn head_tail(s: &str, head: usize, tail: usize) -> Cow<'_, str> {
    let n = s.chars().count();
    if n <= head + tail {
        return Cow::Borrowed(s);
    }
    let tail_start = s.char_indices().nth(n - tail).map_or(s.len(), |(i, _)| i);
    Cow::Owned(format!("{} … {}", cap(s, head), &s[tail_start..]))
}

/// `cap` plus a trailing `…` when something was cut.
pub fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) if max_chars > 0 => format!("{}…", s[..idx].trim_end()),
        Some(_) => String::new(),
        None => s.to_string(),
    }
}

/// The text between `<tag>` and `</tag>` (to the end of `s` if the closing tag is missing).
pub fn tag_content<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close).map_or(s.len(), |i| start + i);
    Some(&s[start..end])
}

/// Remove `<system-reminder>…</system-reminder>` blocks Claude Code appends to user turns.
pub fn strip_system_reminders(s: &str) -> Cow<'_, str> {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    if !s.contains(OPEN) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find(CLOSE) {
            Some(end) => &rest[start + end + CLOSE.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// A case-insensitive regex matching any of `terms` literally, longest alternative first.
pub fn terms_regex<S: AsRef<str>>(terms: &[S]) -> Option<Regex> {
    let mut parts: Vec<&str> = terms
        .iter()
        .map(AsRef::as_ref)
        .filter(|t| !t.trim().is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.sort_by_key(|t| std::cmp::Reverse(t.len()));
    let pattern = parts
        .iter()
        .map(|t| regex::escape(t))
        .collect::<Vec<_>>()
        .join("|");
    RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
        .ok()
}

/// One excerpt of a longer text, flattened to a single line, with the byte ranges of the matched
/// terms inside `text` so callers can highlight them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    pub highlights: Vec<Range<usize>>,
}

impl Snippet {
    /// The snippet with every highlight wrapped in `open`/`close` (e.g. `**` for markdown).
    pub fn marked(&self, open: &str, close: &str) -> String {
        let mut out = String::with_capacity(self.text.len() + 8 * self.highlights.len());
        let mut last = 0;
        for r in &self.highlights {
            out.push_str(&self.text[last..r.start]);
            out.push_str(open);
            out.push_str(&self.text[r.clone()]);
            out.push_str(close);
            last = r.end;
        }
        out.push_str(&self.text[last..]);
        out
    }
}

/// Up to `max` excerpts of `text` around matches of `re`, with about `radius` characters of
/// context on each side. Nearby matches share one excerpt, no text appears in two excerpts, and
/// an excerpt never crosses a blank line (what separates one prompt or reply from the next).
/// Offsets are taken from the original text, so case folding of non-ASCII characters
/// (`İ` → `i̇`) cannot misalign the window.
pub fn snippets(text: &str, re: &Regex, radius: usize, max: usize) -> Vec<Snippet> {
    let mut out = Vec::new();
    if max == 0 {
        return out;
    }
    let mut window: Option<Window> = None;
    for m in re.find_iter(text) {
        if m.as_str().is_empty() {
            continue;
        }
        let entry = entry_around(text, m.range());
        let mut start = back(text, m.start(), radius).max(entry.start);
        let end = forward(text, m.end(), radius).min(entry.end);
        if let Some(w) = &mut window
            && w.entry == entry
            && start <= w.end
        {
            if end - w.start <= radius * 3 {
                // Close enough to share the excerpt while it stays a readable size.
                w.end = w.end.max(end);
                w.ranges.push(m.range());
                continue;
            }
            if m.start() < w.end {
                // Already within the excerpt's trailing context.
                w.end = w.end.max(m.end());
                w.ranges.push(m.range());
                continue;
            }
            start = w.end;
        }
        if let Some(w) = window.take() {
            out.push(w.build(text));
            if out.len() == max {
                return out;
            }
        }
        window = Some(Window {
            entry,
            start,
            end,
            ranges: vec![m.range()],
        });
    }
    out.extend(window.map(|w| w.build(text)));
    out
}

/// The blank-line-separated entry of `text` that contains `range`.
fn entry_around(text: &str, range: Range<usize>) -> Range<usize> {
    let start = text[..range.start].rfind("\n\n").map_or(0, |i| i + 2);
    let end = text[range.end..]
        .find("\n\n")
        .map_or(text.len(), |i| range.end + i);
    start..end
}

struct Window {
    entry: Range<usize>,
    start: usize,
    end: usize,
    ranges: Vec<Range<usize>>,
}

impl Window {
    fn build(self, text: &str) -> Snippet {
        // Cut at word boundaries, so an excerpt doesn't open or close on half a word.
        let (first, last) = (self.ranges[0].start, self.ranges[self.ranges.len() - 1].end);
        let start = if self.start > self.entry.start {
            word_start(text, self.start, first)
        } else {
            self.start
        };
        let end = if self.end < self.entry.end {
            word_end(text, last, self.end)
        } else {
            self.end
        };
        let mut s = Flat::default();
        if start > self.entry.start {
            s.out.push('…');
        }
        let mut highlights = Vec::with_capacity(self.ranges.len());
        let mut pos = start;
        for r in self.ranges {
            s.push(&text[pos..r.start]);
            let h_start = s.out.len();
            s.push(&text[r.clone()]);
            highlights.push(h_start..s.out.len());
            pos = r.end;
        }
        s.push(&text[pos..end]);
        let mut out = s.out.trim_end().to_string();
        // A match that ends in whitespace may reach into what was just trimmed.
        for h in &mut highlights {
            h.end = h.end.min(out.len());
            h.start = h.start.min(h.end);
        }
        highlights.retain(|h| !h.is_empty());
        if end < self.entry.end {
            out.push('…');
        }
        Snippet {
            text: out,
            highlights,
        }
    }
}

/// The start of the first whole word at or after `at`, but never past `limit`.
fn word_start(text: &str, at: usize, limit: usize) -> usize {
    if text[..at].ends_with(char::is_whitespace) {
        return at;
    }
    text[at..limit]
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map_or(at, |(i, c)| at + i + c.len_utf8())
}

/// The end of the last whole word at or before `at`, but never before `limit`.
fn word_end(text: &str, limit: usize, at: usize) -> usize {
    if text[at..].starts_with(char::is_whitespace) {
        return at;
    }
    text[limit..at]
        .rfind(char::is_whitespace)
        .map_or(at, |i| limit + i)
}

/// Appends text while collapsing whitespace runs into single spaces.
#[derive(Default)]
struct Flat {
    out: String,
    space: bool,
}

impl Flat {
    fn push(&mut self, t: &str) {
        for c in t.chars() {
            if c.is_whitespace() {
                if !self.space && !self.out.is_empty() && !self.out.ends_with('…') {
                    self.out.push(' ');
                }
                self.space = true;
            } else {
                self.out.push(c);
                self.space = false;
            }
        }
    }
}

/// Byte index `n` characters before `idx`.
fn back(s: &str, idx: usize, n: usize) -> usize {
    s[..idx]
        .char_indices()
        .rev()
        .nth(n.saturating_sub(1))
        .map_or(0, |(i, _)| i)
}

/// Byte index `n` characters after `idx`.
fn forward(s: &str, idx: usize, n: usize) -> usize {
    s[idx..]
        .char_indices()
        .nth(n)
        .map_or(s.len(), |(i, _)| idx + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_escapes_and_controls() {
        assert_eq!(clean("\x1b[1;31mred\x1b[0m ok"), "red ok");
        assert_eq!(clean("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(clean("tab\there\x07bell\x00nul"), "tab here bell nul");
        assert_eq!(clean("\x1b]0;title\x07after"), "after");
    }

    #[test]
    fn one_line_collapses_whitespace() {
        assert_eq!(one_line("  a \n\n b\t c  "), "a b c");
    }

    #[test]
    fn cap_and_truncate_respect_char_boundaries() {
        assert_eq!(cap("héllo wörld", 4), "héll");
        assert_eq!(cap("abc", 10), "abc");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn head_tail_keeps_both_ends() {
        assert_eq!(head_tail("short", 3, 3), "short");
        assert_eq!(head_tail("abcdefghij", 3, 2), "abc … ij");
        assert_eq!(head_tail("ééééééé", 2, 2), "éé … éé");
    }

    #[test]
    fn tag_content_extracts_inner_text() {
        let s = "<command-name>/review</command-name>\n<command-args>123</command-args>";
        assert_eq!(tag_content(s, "command-name"), Some("/review"));
        assert_eq!(tag_content(s, "command-args"), Some("123"));
        assert_eq!(tag_content(s, "missing"), None);
        assert_eq!(tag_content("<bash-input>ls", "bash-input"), Some("ls"));
    }

    #[test]
    fn strips_system_reminders() {
        let s = "fix it<system-reminder>ignore me</system-reminder> please";
        assert_eq!(strip_system_reminders(s), "fix it please");
        assert_eq!(strip_system_reminders("<system-reminder>only"), "");
        assert!(matches!(strip_system_reminders("plain"), Cow::Borrowed(_)));
    }

    #[test]
    fn snippet_highlights_case_insensitively() {
        let re = terms_regex(&["kafka"]).unwrap();
        let s = snippets("We tuned the Kafka consumer today", &re, 80, 3);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].marked("[", "]"), "We tuned the [Kafka] consumer today");
    }

    #[test]
    fn snippet_windows_and_ellipses() {
        let text = format!("{} needle {}", "a".repeat(200), "b".repeat(200));
        let re = terms_regex(&["needle"]).unwrap();
        let s = &snippets(&text, &re, 10, 3)[0];
        assert!(s.text.starts_with('…') && s.text.ends_with('…'));
        assert_eq!(&s.text[s.highlights[0].clone()], "needle");
        assert!(s.text.chars().count() < 40);
    }

    #[test]
    fn snippet_merges_nearby_matches_and_respects_max() {
        let re = terms_regex(&["x"]).unwrap();
        let near = snippets("x y x", &re, 10, 5);
        assert_eq!(near.len(), 1);
        assert_eq!(near[0].highlights.len(), 2);
        let far = format!("x{}x{}x", " ".repeat(500), " ".repeat(500));
        assert_eq!(snippets(&far, &re, 10, 2).len(), 2);
    }

    #[test]
    fn snippet_offsets_survive_unicode_case_folding() {
        // 'İ' lowercases to two chars; byte offsets must still point at the match.
        let text = "İİİİ İstanbul trip planning";
        let re = terms_regex(&["trip"]).unwrap();
        let s = &snippets(text, &re, 80, 1)[0];
        assert_eq!(&s.text[s.highlights[0].clone()], "trip");
    }

    #[test]
    fn snippet_flattens_newlines() {
        let re = terms_regex(&["b"]).unwrap();
        let s = &snippets("a\n  b\nc", &re, 10, 1)[0];
        assert_eq!(s.text, "a b c");
        assert_eq!(&s.text[s.highlights[0].clone()], "b");
    }

    #[test]
    fn snippets_stay_within_one_prompt() {
        // Prompts are stored separated by blank lines; an excerpt that ran from one into the
        // next read as a sentence nobody wrote.
        let re = terms_regex(&["kafka"]).unwrap();
        let text = "is the kafka consumer lagging?\n\nrestart it\n\nthen check kafka again";
        let s: Vec<String> = snippets(text, &re, 80, 5)
            .iter()
            .map(|s| s.marked("[", "]"))
            .collect();
        assert_eq!(
            s,
            [
                "is the [kafka] consumer lagging?",
                "then check [kafka] again"
            ]
        );
    }

    #[test]
    fn snippets_start_and_end_on_whole_words() {
        let re = terms_regex(&["needle"]).unwrap();
        let s = &snippets("abcdefghij klmnop needle qrstuv wxyzab", &re, 10, 1)[0];
        assert_eq!(s.marked("[", "]"), "…klmnop [needle] qrstuv…");
        // Text without spaces (or a word longer than the window) is still cut.
        let s = &snippets(&format!("{}needle", "x".repeat(40)), &re, 10, 1)[0];
        assert_eq!(s.marked("[", "]"), format!("…{}[needle]", "x".repeat(10)));
    }

    #[test]
    fn highlights_stay_inside_the_snippet_text() {
        // Regression: a match ending in a space (`"import "`, a near variant of `"import x"`) at
        // the end of an excerpt was trimmed away, leaving its highlight inside the trailing '…'.
        let text = "a.py:22:import contextlib a.py:23:import gzip a.py:24:import json\n\nimport ";
        for term in ["import ", " import", "import"] {
            let re = terms_regex(&[term]).unwrap();
            for radius in 1..40 {
                for s in snippets(text, &re, radius, 10) {
                    for h in &s.highlights {
                        assert!(
                            h.start < h.end
                                && h.end <= s.text.len()
                                && s.text.is_char_boundary(h.start)
                                && s.text.is_char_boundary(h.end),
                            "{term:?} radius {radius}: {s:?}"
                        );
                    }
                    s.marked("[", "]");
                }
            }
        }
    }

    #[test]
    fn snippets_never_repeat_text() {
        let re = terms_regex(&["x"]).unwrap();
        // Long words and short ones: cutting at word boundaries hides an overlap in the first.
        for (text, radius) in [
            (format!("x {} x {} x", "a".repeat(15), "b".repeat(15)), 10),
            (format!("x {}x {}x", "a ".repeat(25), "b ".repeat(25)), 30),
        ] {
            let s = snippets(&text, &re, radius, 5);
            let shown: usize = s.iter().map(|s| s.text.trim_matches('…').len()).sum();
            assert!(shown <= text.len(), "{s:?}");
            assert_eq!(s.iter().map(|s| s.highlights.len()).sum::<usize>(), 3);
            for s in &s {
                for h in &s.highlights {
                    assert_eq!(&s.text[h.clone()], "x", "{s:?}");
                }
            }
        }
    }

    #[test]
    fn terms_regex_prefers_longest_and_escapes() {
        let re = terms_regex(&["a", "a.b"]).unwrap();
        assert_eq!(re.find("xa.by").unwrap().as_str(), "a.b");
        assert!(terms_regex::<&str>(&[]).is_none());
        assert!(terms_regex(&["  "]).is_none());
    }
}
