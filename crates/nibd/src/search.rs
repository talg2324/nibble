//! Project search (`Request::Search`): file names matched fuzzily, and lines containing the
//! query. Reads the files `tree::files` lists: no gitignored files, no folders starting with `.`.

use crate::tree;
use nib_proto::{Location, Pos, SearchResults};
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};
use std::path::Path;

/// Most files looked at (names and contents).
const MAX_FILES: usize = 50_000;
/// Most file-name matches sent.
const MAX_NAMES: usize = 50;
/// Larger files are not read (minified bundles, data dumps).
const MAX_FILE_BYTES: u64 = 2 << 20;
/// A NUL in this many first bytes: a binary file.
const SNIFF: usize = 8 << 10;
/// Longest line text sent; the match is kept inside it.
const MAX_LINE_CHARS: usize = 240;

pub fn search(root: &Path, query: &str, max: usize) -> SearchResults {
    let mut out = SearchResults::default();
    if query.trim().is_empty() {
        return out;
    }
    let (files, truncated) = tree::files(root, MAX_FILES);
    out.truncated = truncated;
    out.files = names(&files, query);

    // Smart case: a capital in the query makes it exact.
    let exact = query.chars().any(char::is_uppercase);
    let needle = if exact { query.to_string() } else { query.to_ascii_lowercase() };
    let root_s = root.to_string_lossy();
    for rel in &files {
        let path = root.join(rel);
        if std::fs::metadata(&path).map_or(true, |m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else { continue };
        if data[..data.len().min(SNIFF)].contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&data);
        // Lowered once for the whole file: ASCII lowering keeps every byte where it was, so its
        // lines and offsets are the text's own. Most files don't have the query at all.
        let lower = if exact { std::borrow::Cow::Borrowed(&*text) } else { std::borrow::Cow::Owned(text.to_ascii_lowercase()) };
        if !lower.contains(&needle) {
            continue;
        }
        for (n, (line, hay)) in text.lines().zip(lower.lines()).enumerate() {
            let Some(at) = hay.find(&needle) else { continue };
            if out.lines.len() >= max {
                out.truncated = true;
                return out;
            }
            let col = line[..at].chars().count();
            let len = line[at..at + needle.len()].chars().count();
            // Long lines are cut around the match; columns stay the line's own.
            let skip = col.saturating_sub(MAX_LINE_CHARS / 3);
            let line_text: String = line.chars().skip(skip).take(MAX_LINE_CHARS).collect();
            let line_text = if skip > 0 { format!("…{line_text}") } else { line_text };
            out.lines.push(Location {
                path: format!("{}/{rel}", root_s.trim_end_matches('/')),
                start: Pos { line: n as u32, col: col as u32 },
                end: Pos { line: n as u32, col: (col + len) as u32 },
                line_text,
            });
        }
    }
    out
}

/// Paths matching `query` fuzzily, best first (shorter paths win ties).
fn names(files: &[String], query: &str) -> Vec<String> {
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, &String)> = files
        .iter()
        .filter_map(|f| pattern.score(Utf32Str::new(f, &mut buf), &mut matcher).map(|s| (s, f)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())).then(a.1.cmp(b.1)));
    scored.into_iter().take(MAX_NAMES).map(|(_, f)| f.clone()).collect()
}
