//! Rendered preview of Markdown and HTML. Both end up as Iced Markdown items: HTML is first
//! reduced to Markdown (a reader view: structure, text styles, links, tables and images, but no
//! CSS layout and no scripts).

use crate::math;
use crate::media::Kind;
use iced::widget::markdown::{self, Bullet, Item};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;

/// HTML -> Markdown. Never fails: unparseable input falls back to the raw text.
pub fn html_to_markdown(html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "head", "noscript", "template", "svg", "iframe"])
        .build();
    converter.convert(html).unwrap_or_else(|_| html.to_string())
}

/// Where the rendered blocks come from in the source text.
#[derive(Debug, Default)]
pub struct SourceMap {
    /// Source line of each top-level item, in order (for scroll sync). Empty if the mapping failed.
    pub top: Vec<usize>,
    /// Char offset in the source of the leaf blocks (paragraphs, headings, code blocks, images,
    /// tables), keyed by the address of the item's data inside the `Content` (see `block_key`).
    pub targets: HashMap<usize, usize>,
    /// Where each top-level item goes on the page, in order. Empty when everything just flows.
    pub places: Vec<Place>,
}

/// Where a top-level block sits, for the layout HTML asks for and Markdown has no way to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Flow,
    /// Inside `align="center"` / `<center>`.
    Center,
    /// A cell of a table that only lays out images and captions: the cells of one `row` stand
    /// side by side.
    Cell { row: usize, col: usize },
}

/// The title an `<img width="N">` gets, which is how its width reaches the viewer.
pub const WIDTH_TITLE: &str = "nib-w=";

/// The value of attribute `name` in an open tag (lower case).
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let at = tag.find(&format!(" {name}="))? + name.len() + 2;
    let rest = &tag[at..];
    match rest.chars().next()? {
        q @ ('"' | '\'') => rest[1..].split(q).next(),
        _ => rest.split(|c: char| c.is_whitespace() || c == '>' || c == '/').next(),
    }
}

/// `<img width="440">` -> `<img title="nib-w=440">`: Markdown has no image size, but it has a title.
fn width_to_title(html: &str) -> String {
    let low = html.to_ascii_lowercase();
    let (mut out, mut at) = (String::with_capacity(html.len()), 0);
    while let Some(p) = low[at..].find("<img") {
        let s = at + p;
        let tag_ok = low[s + 4..].starts_with(|c: char| c.is_whitespace() || c == '/' || c == '>');
        let Some(e) = low[s..].find('>').map(|e| s + e).filter(|_| tag_ok) else {
            out.push_str(&html[at..s + 4]);
            at = s + 4;
            continue;
        };
        out.push_str(&html[at..s]);
        let width = attr(&low[s..e], "width").and_then(|w| w.trim_end_matches("px").parse::<u32>().ok());
        match width {
            Some(w) if !low[s..e].contains(" title=") => {
                out.push_str(&format!("<img title=\"{WIDTH_TITLE}{w}\""));
                out.push_str(&html[s + 4..e]);
            }
            _ => out.push_str(&html[s..e]),
        }
        at = e;
    }
    out.push_str(&html[at..]);
    out
}

/// The next open tag at or after `from`: where it starts, where it ends, its name (lower case).
fn next_open(low: &str, from: usize) -> Option<(usize, usize, &str)> {
    let mut at = from;
    while let Some(p) = low[at..].find('<') {
        let s = at + p;
        let n = low[s + 1..].find(|c: char| !c.is_ascii_alphanumeric()).map_or(low.len(), |e| s + 1 + e);
        if n > s + 1 {
            let e = s + low[s..].find('>')? + 1;
            return Some((s, e, &low[s + 1..n]));
        }
        at = s + 1;
    }
    None
}

/// Where the element opened just before `from` ends (after its closing tag), counting nested
/// elements of the same name.
fn close_of(low: &str, name: &str, from: usize) -> usize {
    let (open, close) = (format!("<{name}"), format!("</{name}>"));
    let (mut depth, mut at) = (1, from);
    while let Some(c) = low[at..].find(&close).map(|c| at + c) {
        depth += low[at..c]
            .match_indices(&open)
            .filter(|(i, _)| !low[at + i + open.len()..].starts_with(|ch: char| ch.is_ascii_alphanumeric()))
            .count();
        depth -= 1;
        at = c + close.len();
        if depth == 0 {
            return at;
        }
    }
    low.len()
}

/// The pieces of an HTML block that sit differently on the page, as HTML: centered elements, and
/// the cells of tables that only hold images. The rest flows.
fn split_layout(html: &str, rows: &mut usize) -> Vec<(String, Place)> {
    let html = width_to_title(html);
    let low = html.to_ascii_lowercase();
    let mut out: Vec<(String, Place)> = Vec::new();
    let (mut flowed, mut scan) = (0, 0);
    while let Some((s, e, name)) = next_open(&low, scan) {
        let heading = name.len() == 2 && name.starts_with('h') && name.ends_with(|c: char| c.is_ascii_digit());
        let centered = name == "center" || ((name == "p" || name == "div" || heading) && attr(&low[s..e], "align") == Some("center"));
        if name != "table" && !centered {
            scan = e;
            continue;
        }
        let end = close_of(&low, name, e);
        out.push((html[flowed..s].to_string(), Place::Flow));
        if centered {
            out.push((html[s..end].to_string(), Place::Center));
        } else {
            out.extend(table_cells(&html[s..end], rows));
        }
        (flowed, scan) = (end, end);
    }
    out.push((html[flowed..].to_string(), Place::Flow));
    out
}

/// A table of images and captions, one `Place::Cell` per cell of each row with several; any
/// other table as it is.
fn table_cells(table: &str, rows: &mut usize) -> Vec<(String, Place)> {
    let low = table.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut scan = 0;
    while let Some((_, e, name)) = next_open(&low, scan) {
        if name != "tr" {
            scan = e;
            continue;
        }
        let end = close_of(&low, "tr", e);
        let mut cells = Vec::new();
        let mut c = e;
        while let Some((cs, ce, cname)) = next_open(&low[..end], c) {
            if cname != "td" && cname != "th" {
                c = ce;
                continue;
            }
            let cend = close_of(&low, cname, ce);
            cells.push(&table[ce..cend.saturating_sub(cname.len() + 3).max(ce)]);
            c = cend.max(cs + 1);
        }
        if cells.len() < 2 || !cells.iter().all(|c| c.to_ascii_lowercase().contains("<img")) {
            return vec![(table.to_string(), Place::Flow)];
        }
        out.extend(cells.into_iter().enumerate().map(|(col, c)| (c.to_string(), Place::Cell { row: *rows, col })));
        *rows += 1;
        scan = end;
    }
    if out.is_empty() { vec![(table.to_string(), Place::Flow)] } else { out }
}

/// The identity of a rendered block: the address of what the `markdown::Viewer` is handed for it.
pub fn block_key<T: ?Sized>(r: &T) -> usize {
    r as *const T as *const u8 as usize
}

/// Markdown with the HTML in it (blocks like `<p align="center">` or `<table>`, and `<img>` / `<br>`
/// inside a paragraph) turned into Markdown, since Iced's Markdown drops raw HTML. With the marks
/// for `math::offset_before`. HTML nested in a list, quote, heading or table is left alone.
fn html_in_markdown(src: &str) -> (String, Vec<(usize, usize)>, Vec<(usize, usize, Place)>) {
    if !src.contains('<') {
        return (src.to_string(), Vec::new(), Vec::new());
    }
    let opts = Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS
        | Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    // What to put in place of which range, in order.
    let mut swaps: Vec<(Range<usize>, String, Vec<(usize, usize, Place)>)> = Vec::new();
    let (mut depth, mut paragraph, mut rows) = (0usize, false, 0usize);
    for (ev, r) in Parser::new_ext(src, opts).into_offset_iter() {
        match ev {
            Event::Start(Tag::List(_) | Tag::BlockQuote(_)) => depth += 1,
            Event::End(TagEnd::List(_) | TagEnd::BlockQuote(_)) => depth = depth.saturating_sub(1),
            Event::Start(Tag::HtmlBlock) if depth == 0 => {
                let (mut md, mut places) = (String::new(), Vec::new());
                for (html, place) in split_layout(&src[r.clone()], &mut rows) {
                    // An image is a block of its own to Iced: text after it in the same paragraph
                    // (a caption after `<br>`) would be lost.
                    let piece = html_to_markdown(&html).trim().replace("  \n", "\n\n");
                    if piece.is_empty() {
                        continue;
                    }
                    if !md.is_empty() {
                        md.push_str("\n\n");
                    }
                    places.push((md.len(), md.len() + piece.len(), place));
                    md.push_str(&piece);
                }
                swaps.push((r, format!("{md}\n"), places));
            }
            Event::Start(Tag::Paragraph) => paragraph = depth == 0,
            Event::End(TagEnd::Paragraph) => paragraph = false,
            Event::InlineHtml(tag) if paragraph => {
                let t = tag.trim_start_matches('<').trim_start().to_ascii_lowercase();
                let name: String = t.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                match name.as_str() {
                    // A block of its own: Iced folds the paragraph's text before an image into
                    // the image's alt text.
                    "img" => swaps.push((r, format!("\n\n{}\n\n", html_to_markdown(&width_to_title(&tag)).trim()), Vec::new())),
                    "br" => swaps.push((r, "  \n".to_string(), Vec::new())),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let (mut out, mut marks, mut regions, mut copied) = (String::with_capacity(src.len()), Vec::new(), Vec::new(), 0);
    for (r, md, places) in swaps {
        // HTML events inside an HTML block: the block was already replaced.
        if r.start < copied {
            continue;
        }
        out.push_str(&src[copied..r.start]);
        marks.push((out.len(), r.start));
        regions.extend(places.into_iter().map(|(a, b, p)| (out.len() + a, out.len() + b, p)));
        out.push_str(&md);
        marks.push((out.len(), r.end));
        copied = r.end;
    }
    out.push_str(&src[copied..]);
    (out, marks, regions)
}

/// The parsed preview, where its blocks come from, and the math in it (Markdown only: `$` in an
/// HTML page is text).
pub fn render(kind: Kind, source: &str) -> (markdown::Content, SourceMap, math::Formulas) {
    if kind == Kind::Html {
        let md = html_to_markdown(source);
        let content = markdown::Content::parse(&md);
        let ranges = block_ranges(&md);
        let offsets = html_offsets(source, &md, &ranges);
        let map = source_map(source, ranges, offsets, &content);
        return (content, map, math::Formulas::default());
    }
    // Math first: it leaves HTML alone, so a `$` in HTML stays text.
    let p = math::prepare(source);
    let (md, marks, regions) = html_in_markdown(&p.md);
    let content = markdown::Content::parse(&md);
    let ranges = block_ranges(&md);
    let offsets = ranges.iter().map(|r| p.source_offset(math::offset_before(&marks, r.1.start))).collect();
    let places: Vec<Place> = ranges
        .iter()
        .filter(|r| r.2)
        .map(|r| regions.iter().find(|g| g.0 <= r.1.start && r.1.start < g.1).map_or(Place::Flow, |g| g.2))
        .collect();
    let mut map = source_map(source, ranges, offsets, &content);
    if places.len() == map.top.len() && places.iter().any(|p| *p != Place::Flow) {
        map.places = places;
    }
    (content, map, p.formulas)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Para,
    Heading,
    Code,
    List,
    Quote,
    Image,
    Rule,
    Table,
}

/// The items `markdown::Content::parse(md)` makes, in the order it makes them (children before
/// their list / quote), with their source ranges and whether they are top-level. Mirrors
/// `iced_widget::markdown::parse_with`; items inside tables are left out (their cells aren't
/// reachable from outside iced).
fn block_ranges(md: &str) -> Vec<(Block, Range<usize>, bool)> {
    #[derive(PartialEq)]
    enum Scope {
        List,
        Quote,
        Table,
    }
    // `Content::push_str` drops a trailing `|` (an unfinished table row) before parsing.
    let md = if md.trim_end().ends_with('|') { md.trim_end().trim_end_matches('|') } else { md };
    let opts = Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS
        | Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let mut out = Vec::new();
    let mut stack: Vec<Scope> = Vec::new();
    let (mut spans, mut metadata, mut code_block, mut image) = (false, false, false, false);
    let produce = |out: &mut Vec<_>, stack: &[Scope], b: Block, r: Range<usize>| {
        if !stack.contains(&Scope::Table) {
            out.push((b, r, stack.is_empty()));
        }
    };
    for (ev, r) in Parser::new_ext(md, opts).into_offset_iter() {
        match ev {
            Event::Start(tag) => match tag {
                Tag::List(_) | Tag::BlockQuote(_) if !metadata => {
                    if spans {
                        produce(&mut out, &stack, Block::Para, r.clone());
                        spans = false;
                    }
                    stack.push(if matches!(tag, Tag::List(_)) { Scope::List } else { Scope::Quote });
                }
                Tag::CodeBlock(CodeBlockKind::Fenced(_)) if !metadata => {
                    code_block = true;
                    if spans {
                        produce(&mut out, &stack, Block::Para, r);
                        spans = false;
                    }
                }
                Tag::Image { .. } if !metadata => image = true,
                Tag::MetadataBlock(_) => metadata = true,
                Tag::Table(_) => stack.push(Scope::Table),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) if !metadata => {
                    produce(&mut out, &stack, Block::Heading, r);
                    spans = false;
                }
                TagEnd::Paragraph | TagEnd::Item | TagEnd::TableCell if tag == TagEnd::TableCell || !metadata => {
                    if spans {
                        produce(&mut out, &stack, Block::Para, r);
                        spans = false;
                    }
                }
                TagEnd::List(_) if !metadata => {
                    if stack.pop() == Some(Scope::List) {
                        produce(&mut out, &stack, Block::List, r);
                    }
                }
                TagEnd::BlockQuote(_) if !metadata => {
                    if stack.pop() == Some(Scope::Quote) {
                        produce(&mut out, &stack, Block::Quote, r);
                    }
                }
                TagEnd::Image if !metadata && image => {
                    image = false;
                    spans = false;
                    produce(&mut out, &stack, Block::Image, r);
                }
                TagEnd::CodeBlock if !metadata => {
                    code_block = false;
                    produce(&mut out, &stack, Block::Code, r);
                }
                TagEnd::MetadataBlock(_) => metadata = false,
                TagEnd::Table => {
                    if stack.pop() == Some(Scope::Table) {
                        produce(&mut out, &stack, Block::Table, r);
                    }
                }
                _ => {}
            },
            Event::Text(_) if !metadata && !code_block => spans = true,
            Event::Code(_) | Event::SoftBreak | Event::HardBreak if !metadata => spans = true,
            Event::Rule => produce(&mut out, &stack, Block::Rule, r),
            _ => {}
        }
    }
    out
}

/// The parsed items in the order `block_ranges` lists them, with their `block_key`.
fn walk(items: &[Item], top: bool, out: &mut Vec<(Block, usize, bool)>) {
    for item in items {
        let (b, key) = match item {
            Item::Paragraph(t) => (Block::Para, block_key(t)),
            Item::Heading(_, t) => (Block::Heading, block_key(t)),
            Item::CodeBlock { code, .. } => (Block::Code, block_key(code.as_str())),
            Item::Image { url, .. } => (Block::Image, block_key(url)),
            Item::Rule => (Block::Rule, 0),
            Item::Table { columns, .. } => (Block::Table, block_key(columns.as_slice())),
            Item::List { bullets, .. } => {
                for b in bullets {
                    let (Bullet::Point { items } | Bullet::Task { items, .. }) = b;
                    walk(items, false, out);
                }
                (Block::List, 0)
            }
            Item::Quote(items) => {
                walk(items, false, out);
                (Block::Quote, 0)
            }
        };
        out.push((b, key, top));
    }
}

/// `offsets`: the byte offset in `source` of each of `ranges`.
fn source_map(source: &str, ranges: Vec<(Block, Range<usize>, bool)>, offsets: Vec<usize>, content: &markdown::Content) -> SourceMap {
    let mut items = Vec::new();
    walk(content.items(), true, &mut items);
    let matches = ranges.len() == items.len() && ranges.iter().zip(&items).all(|(r, i)| r.0 == i.0 && r.2 == i.2);
    if !matches {
        return SourceMap::default();
    }
    // Byte offset -> (char offset, line), in one pass over the text.
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    sorted.dedup();
    let mut at = HashMap::with_capacity(sorted.len());
    let (mut chars, mut line, mut pos) = (0, 0, 0);
    for &o in &sorted {
        let o = o.min(source.len());
        let chunk = &source[pos..o];
        chars += chunk.chars().count();
        line += chunk.bytes().filter(|&b| b == b'\n').count();
        pos = o;
        at.insert(o, (chars, line));
    }
    let mut map = SourceMap::default();
    for ((b, key, top), o) in items.into_iter().zip(offsets) {
        let (ch, line) = at[&o.min(source.len())];
        if top {
            map.top.push(line);
        }
        if key != 0 && !matches!(b, Block::List | Block::Quote | Block::Rule) {
            map.targets.insert(key, ch);
        }
    }
    map
}

/// Where each block of the Markdown made from an HTML page is in the page: the most distinctive
/// word near its start, searched for from where the previous block was found.
fn html_offsets(html: &str, md: &str, ranges: &[(Block, Range<usize>, bool)]) -> Vec<usize> {
    let lower = html.to_lowercase();
    // `to_lowercase` can change byte lengths; fall back to the case-sensitive text if it did.
    let hay = if lower.len() == html.len() { lower.as_str() } else { html };
    let fold = lower.len() == html.len();
    // Search in order of the text, not the order the blocks were made (children first).
    let mut order: Vec<usize> = (0..ranges.len()).collect();
    order.sort_by_key(|&i| ranges[i].1.start);
    let mut out = vec![0; ranges.len()];
    let mut cursor = 0;
    for i in order {
        let r = &ranges[i].1;
        let slice = &md[r.start.min(md.len())..r.end.min(md.len())];
        let word = slice
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.chars().count() >= 3)
            .take(6)
            .max_by_key(|w| w.len());
        if let Some(w) = word {
            let w = if fold { w.to_lowercase() } else { w.to_string() };
            if let Some(found) = hay[cursor..].find(&w) {
                cursor += found;
            }
        }
        out[i] = cursor;
    }
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%').then(|| s.get(i + 1..i + 3)).flatten().and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where a link or image in the document at `doc` points.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// A file on the daemon's side, resolved against the document's folder.
    File(String),
    /// Something for the system browser / mail client.
    External(String),
    /// An in-page anchor, a `data:` URI, or a scheme nib doesn't handle.
    Other,
}

pub fn resolve(doc: &Path, url: &str) -> Target {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if ["http://", "https://", "mailto:"].iter().any(|p| lower.starts_with(p)) {
        return Target::External(url.to_string());
    }
    let path = match lower.strip_prefix("file://") {
        Some(_) => &url["file://".len()..],
        None if url.starts_with('#') || url.is_empty() => return Target::Other,
        // Any other `scheme:` (but not a Windows drive letter or a path with a colon later on).
        None if url.split(['/', '?', '#']).next().is_some_and(|head| head.len() > 1 && head.contains(':')) => {
            return Target::Other
        }
        None => url,
    };
    let path = percent_decode(path.split(['?', '#']).next().unwrap_or(""));
    if path.is_empty() {
        return Target::Other;
    }
    // `file:///C:/x` → `C:/x`.
    let path = match path.as_bytes() {
        [b'/', d, b':', b'/', ..] if d.is_ascii_alphabetic() => path[1..].to_string(),
        _ => path,
    };
    // Paths are the daemon's (`/`-separated, see nib_proto::path), not this machine's: joined as
    // strings, since `Path::join` would put `\` in them on Windows.
    let doc = doc.to_string_lossy();
    let dir = doc.rfind('/').map_or("", |i| &doc[..i]);
    let joined = nib_proto::path::join(dir, &path);
    // Fold `.` and `..` so the same file always has the same name.
    let abs = joined.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for c in joined.split('/') {
        match c {
            "" | "." => {}
            ".." if out.last().is_some_and(|l| *l != "..") => {
                out.pop();
            }
            ".." if abs => {}
            c => out.push(c),
        }
    }
    let s = out.join("/");
    Target::File(if abs { format!("/{s}") } else { s })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_structured_markdown() {
        let html = r#"<!DOCTYPE html><html><head><title>T</title><style>p{color:red}</style></head>
<body><h1>Hello</h1><script>alert(1)</script>
<p>Some <b>bold</b> and <a href="https://example.com">a link</a>.</p>
<ul><li>one</li><li>two</li></ul>
<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>
<img src="pic.png" alt="pic">
</body></html>"#;
        let md = html_to_markdown(html);
        assert!(md.contains("# Hello"), "{md}");
        assert!(md.contains("**bold**"), "{md}");
        assert!(md.contains("[a link](https://example.com)"), "{md}");
        assert!(md.contains("![pic](pic.png)"), "{md}");
        assert!(!md.contains("alert") && !md.contains("color:red"), "{md}");

        let (content, map, _) = render(Kind::Html, html);
        // Blocks point back into the HTML: the heading at its line, the list after it.
        assert_eq!(map.top.first(), Some(&1), "{md}");
        assert!(map.top.windows(2).all(|w| w[0] <= w[1]), "{:?}", map.top);
        let items = content.items();
        assert!(items.iter().any(|i| matches!(i, Item::Heading(..))), "{md}");
        assert!(items.iter().any(|i| matches!(i, Item::List { .. })), "{md}");
        assert!(items.iter().any(|i| matches!(i, Item::Table { .. })), "{md}");
        assert!(content.images().contains("pic.png"), "{md}");
    }

    #[test]
    fn markdown_renders_directly() {
        let src = "# T\n\n- a\n- b\n\n| x | y |\n|---|---|\n| 1 | 2 |\n";
        let (c, map, _) = render(Kind::Markdown, src);
        assert_eq!(map.top, vec![0, 2, 5]);
        // Each bullet's paragraph, the heading and the table are clickable, at their own offsets.
        let mut offs: Vec<usize> = map.targets.values().copied().collect();
        offs.sort();
        assert_eq!(offs, vec![0, 5, 9, 14]);
        assert!(matches!(c.items()[0], Item::Heading(..)));
        assert!(c.items().iter().any(|i| matches!(i, Item::Table { .. })));
    }

    #[test]
    fn html_inside_markdown_is_rendered() {
        let src = r#"# T

<p align="center">
  <img src="logo.svg" alt="logo" width="140">
</p>

Text with <img src="a.png" alt="a"> inline.

<table>
  <tr>
    <td><img src="x.gif" alt="x"><br><sub><b>X</b>: first</sub></td>
    <td><img src="y.gif" alt="y"><br><sub><b>Y</b>: second</sub></td>
  </tr>
</table>

End `<img src="code.png">`.
"#;
        let (md, ..) = html_in_markdown(src);
        let (c, map, _) = render(Kind::Markdown, src);
        for url in ["logo.svg", "a.png", "x.gif", "y.gif"] {
            assert!(c.images().contains(url), "{url}\n{md}");
        }
        assert!(!c.images().contains("code.png"), "{md}");
        assert!(md.contains("first") && md.contains("second"), "{md}");
        // The text around an inline image stays text.
        let paragraphs = c.items().iter().filter(|i| matches!(i, Item::Paragraph(_))).count();
        assert_eq!(paragraphs, 5, "{md}");
        // Blocks still point at their own lines: heading, logo, ..., the last paragraph.
        assert_eq!(map.top.first(), Some(&0), "{md}");
        assert_eq!(map.top.get(1), Some(&2), "{md}");
        assert_eq!(map.top.last(), Some(&15), "{md}");
        assert!(map.top.windows(2).all(|w| w[0] <= w[1]), "{:?}", map.top);
        // Without HTML nothing changes.
        assert_eq!(html_in_markdown("a < b\n").0, "a < b\n");
    }

    #[test]
    fn html_layout_is_kept() {
        let src = "<p align=\"center\">\n  <img src=\"l.svg\" alt=\"l\" width=\"140\">\n</p>\n\n<h1 align=\"center\">T</h1>\n\nplain\n\n<table align=\"center\">\n  <tr>\n    <td><img src=\"x.gif\" alt=\"x\" width=\"440\"><br><sub>X</sub></td>\n    <td><img src=\"y.gif\" alt=\"y\"><br><sub>Y</sub></td>\n  </tr>\n</table>\n";
        let (md, ..) = html_in_markdown(src);
        assert!(md.contains("nib-w=140") && md.contains("nib-w=440"), "{md}");
        let (_, map, _) = render(Kind::Markdown, src);
        let c = |row, col| Place::Cell { row, col };
        assert_eq!(
            map.places,
            vec![Place::Center, Place::Center, Place::Flow, c(0, 0), c(0, 0), c(0, 1), c(0, 1)],
            "{md}"
        );
        assert_eq!(map.top.len(), map.places.len());
        // A data table is not layout.
        let (_, map, _) = render(Kind::Markdown, "<table><tr><td>a</td><td>b</td></tr></table>\n");
        assert!(map.places.is_empty());
    }

    #[test]
    fn resolves_links() {
        let doc = Path::new("/proj/docs/readme.md");
        let file = |s: &str| Target::File(s.into());
        assert_eq!(resolve(doc, "img/a%20b.png"), file("/proj/docs/img/a b.png"));
        assert_eq!(resolve(doc, "../logo.png?x=1#y"), file("/proj/logo.png"));
        assert_eq!(resolve(doc, "./other.md"), file("/proj/docs/other.md"));
        assert_eq!(resolve(doc, "/abs/x.png"), file("/abs/x.png"));
        assert_eq!(resolve(doc, "file:///abs/x.png"), file("/abs/x.png"));
        // A Windows daemon's paths, whatever this machine is.
        let win = Path::new("C:/proj/docs/readme.md");
        assert_eq!(resolve(win, "../img/a.png"), file("C:/proj/img/a.png"));
        assert_eq!(resolve(win, "file:///D:/x/y.png"), file("D:/x/y.png"));
        assert_eq!(resolve(doc, "HTTPS://example.com/a"), Target::External("HTTPS://example.com/a".into()));
        assert_eq!(resolve(doc, "mailto:a@b.c"), Target::External("mailto:a@b.c".into()));
        assert_eq!(resolve(doc, "#section"), Target::Other);
        assert_eq!(resolve(doc, "data:image/png;base64,AAAA"), Target::Other);
        assert_eq!(resolve(doc, "javascript:void(0)"), Target::Other);
    }
}
