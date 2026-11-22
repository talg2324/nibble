//! Bare URLs in plain text: found for the terminal's Cmd-click, and turned into Markdown
//! autolinks for chat replies (pulldown-cmark only links `<https://…>`, not bare URLs).

use std::ops::Range;

/// This machine, as servers print it without a scheme: `localhost:8000`, `127.0.0.1:8000/docs`,
/// `0.0.0.0:8000`, `[::1]:8000`. The length of host, colon and port at the start of `rest`, if
/// it starts with one and the port ends there.
fn bare_local(rest: &str) -> Option<usize> {
    let host = ["localhost:", "127.0.0.1:", "0.0.0.0:", "[::1]:"].into_iter().find(|h| rest.starts_with(h))?;
    let after = &rest.as_bytes()[host.len()..];
    let digits = after.iter().take_while(|b| b.is_ascii_digit()).count();
    let ends = after.get(digits).is_none_or(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'@')));
    // `localhost:8000.5` is not an address; `localhost:8000.` ends a sentence.
    let fraction = after.get(digits) == Some(&b'.') && after.get(digits + 1).is_some_and(u8::is_ascii_digit);
    ((1..=5).contains(&digits) && after[..digits].iter().fold(0u32, |n, b| n * 10 + u32::from(b - b'0')) <= 65535 && ends && !fraction).then_some(host.len() + digits)
}

/// Byte ranges of the `http://` / `https://` URLs in `s`, and of this machine's addresses
/// written without a scheme (`localhost:8000/docs`, see `bare_local`). Trailing punctuation that
/// usually ends a sentence (and a closing bracket with no opening one inside the URL) is left out.
pub fn find_urls(s: &str) -> Vec<Range<usize>> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        // `head`: what is there before the part that may be trimmed.
        let (head, bare) = if rest.starts_with("https://") {
            (8, false)
        } else if rest.starts_with("http://") {
            (7, false)
        } else if let Some(n) = bare_local(rest) {
            (n, true)
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        // Part of a longer word or host (`mylocalhost:80`, `x.127.0.0.1:80`, `ftp://localhost:1`).
        let inside = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || (bare && matches!(b, b'.' | b'-' | b'/' | b'@' | b':'));
        if i > 0 && inside(bytes[i - 1]) {
            i += head;
            continue;
        }
        let mut end = i + head;
        // A bare address goes on only with a path, a query or a fragment.
        if !bare || matches!(bytes.get(end), Some(b'/' | b'?' | b'#')) {
            while end < s.len() && is_url_byte(bytes[end]) {
                end += 1;
            }
        }
        // Trim what a sentence or a bracket around the URL put after it.
        while let Some(&last) = bytes[i + head..end].last() {
            let unbalanced = |open: u8, close: u8| {
                last == close && {
                    let u = &bytes[i..end];
                    u.iter().filter(|&&b| b == close).count() > u.iter().filter(|&&b| b == open).count()
                }
            };
            if matches!(last, b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'*' | b'_')
                || unbalanced(b'(', b')')
                || (unbalanced(b'[', b']') && !(bare && end == i + head))
            {
                end -= 1;
            } else {
                break;
            }
        }
        if bare || end > i + head {
            out.push(i..end);
        }
        i = end.max(i + head);
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
            // Only those with a scheme: `<localhost:8000>` would be a link to nowhere.
            for r in find_urls(piece).into_iter().filter(|r| piece[r.clone()].starts_with("http")) {
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
    fn this_machine_without_a_scheme_is_found_too() {
        assert_eq!(urls("Uvicorn running on localhost:8000 (Press CTRL+C to quit)"), ["localhost:8000"]);
        assert_eq!(urls("docs at 127.0.0.1:8765/docs?x=1."), ["127.0.0.1:8765/docs?x=1"]);
        assert_eq!(urls("Listening on 0.0.0.0:6006, [::1]:6006/ and (localhost:5173/app)."), ["0.0.0.0:6006", "[::1]:6006/", "localhost:5173/app"]);
        assert_eq!(urls("http://localhost:8000/x and localhost:1"), ["http://localhost:8000/x", "localhost:1"]);
        // Not inside a longer host or word, not without a port, not a port that is none.
        for no in ["mylocalhost:80", "x.127.0.0.1:80", "a-localhost:80", "ftp://localhost:21", "me@localhost:22", "localhost", "localhost:", "localhost:abc", "localhost:80abc", "localhost:123456", "localhost:70000", "10.0.0.0.0:80", "127.0.0.1:80.5", "::127.0.0.1:80"] {
            assert_eq!(urls(no), Vec::<&str>::new(), "{no}");
        }
        assert_eq!(autolink("see localhost:8000 or http://localhost:8000"), "see localhost:8000 or <http://localhost:8000>");
    }

    #[test]
    fn autolinks_bare_urls_only() {
        assert_eq!(autolink("go to https://a.com now"), "go to <https://a.com> now");
        assert_eq!(autolink("[a](https://a.com) <https://b.com>"), "[a](https://a.com) <https://b.com>");
        assert_eq!(autolink("`https://a.com` x"), "`https://a.com` x");
        assert_eq!(autolink("```\nhttps://a.com\n```\nhttps://b.com"), "```\nhttps://a.com\n```\n<https://b.com>");
    }
}
