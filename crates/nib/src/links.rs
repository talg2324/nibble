//! Bare URLs in plain text: found for the terminal's Cmd-click, and turned into Markdown
//! autolinks for chat replies (pulldown-cmark only links `<https://…>`, not bare URLs).

use std::ops::Range;

/// Byte ranges of the `http://` / `https://` URLs in `s`. Trailing punctuation that usually ends
/// a sentence (and a closing bracket with no opening one inside the URL) is left out.
pub fn find_urls(s: &str) -> Vec<Range<usize>> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let scheme = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
            i += scheme;
            continue;
        }
        let mut end = i + scheme;
        while end < s.len() && is_url_byte(bytes[end]) {
            end += 1;
        }
        // Trim what a sentence or a bracket around the URL put after it.
        while let Some(&last) = bytes[i + scheme..end].last() {
            let unbalanced = |open: u8, close: u8| {
                last == close && {
                    let u = &bytes[i..end];
                    u.iter().filter(|&&b| b == close).count() > u.iter().filter(|&&b| b == open).count()
                }
            };
            if matches!(last, b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'*' | b'_')
                || unbalanced(b'(', b')')
                || unbalanced(b'[', b']')
            {
                end -= 1;
            } else {
                break;
            }
        }
        if end > i + scheme {
            out.push(i..end);
        }
        i = end.max(i + scheme);
    }
    out
}

fn is_url_byte(b: u8) -> bool {
    b.is_ascii_graphic() && !matches!(b, b'<' | b'>' | b'"' | b'`' | b'\\' | b'{' | b'}' | b'|' | b'^')
}

/// `md` with every bare URL wrapped as `<url>`, outside code blocks, code spans and links.
pub fn autolink(md: &str) -> String {
    let mut out = String::with_capacity(md.len() + 16);
    let mut fence: Option<&str> = None;
    for line in md.split_inclusive('\n') {
        let t = line.trim_start();
        if let Some(f) = fence {
            if t.starts_with(f) {
                fence = None;
            }
            out.push_str(line);
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(&t[..3]);
            out.push_str(line);
            continue;
        }
        // Outside code spans: the even-numbered pieces between backtick runs.
        for (n, piece) in split_code_spans(line).into_iter().enumerate() {
            if n % 2 == 1 {
                out.push_str(piece);
                continue;
            }
            let mut last = 0;
            for r in find_urls(piece) {
                let before = &piece[..r.start];
                let linked = before.ends_with('<') || before.ends_with("](") || before.ends_with('[') || before.ends_with("]: ");
                if linked {
                    continue;
                }
                out.push_str(&piece[last..r.start]);
                out.push('<');
                out.push_str(&piece[r.clone()]);
                out.push('>');
                last = r.end;
            }
            out.push_str(&piece[last..]);
        }
    }
    out
}

/// `line` cut at backtick runs: text, code span (with its backticks), text, …
fn split_code_spans(line: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let b = line.as_bytes();
    while i < b.len() {
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        let run = b[i..].iter().take_while(|&&c| c == b'`').count();
        let ticks = &line[i..i + run];
        let Some(close) = line[i + run..].find(ticks) else { break };
        pieces.push(&line[start..i]);
        let end = i + run + close + run;
        pieces.push(&line[i..end]);
        start = end;
        i = end;
    }
    pieces.push(&line[start..]);
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(s: &str) -> Vec<&str> {
        find_urls(s).into_iter().map(|r| &s[r]).collect()
    }

    #[test]
    fn finds_urls_and_trims_sentence_punctuation() {
        assert_eq!(urls("visit: https://a.com/x?y=1&z=%3A."), ["https://a.com/x?y=1&z=%3A"]);
        assert_eq!(urls("(see https://en.wikipedia.org/wiki/Rust_(language))"), ["https://en.wikipedia.org/wiki/Rust_(language)"]);
        assert_eq!(urls("(https://a.com)"), ["https://a.com"]);
        assert_eq!(urls("xhttps://a.com http:// nope"), Vec::<&str>::new());
    }

    #[test]
    fn autolinks_bare_urls_only() {
        assert_eq!(autolink("go to https://a.com now"), "go to <https://a.com> now");
        assert_eq!(autolink("[a](https://a.com) <https://b.com>"), "[a](https://a.com) <https://b.com>");
        assert_eq!(autolink("`https://a.com` x"), "`https://a.com` x");
        assert_eq!(autolink("```\nhttps://a.com\n```\nhttps://b.com"), "```\nhttps://a.com\n```\n<https://b.com>");
    }
}
