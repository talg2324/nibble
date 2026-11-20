//! LaTeX math in Markdown: `$…$` and `\(…\)` inline; `$$…$$`, `\[…\]` and ```` ```math ```` blocks
//! as display math. Used by the Markdown preview and the AI panel.
//!
//! Before Iced parses the Markdown, each formula is typeset with RaTeX (KaTeX's layout, in Rust;
//! glyphs as paths, so the SVG needs no fonts) and replaced by a reference to its picture: display
//! math by an image block, inline math by a link in its paragraph. The viewers draw those: the
//! image through `display`, a paragraph holding links through `rich` (words and formulas in a
//! wrapping row, since Iced's rich text can't hold pictures). A formula that doesn't parse is left
//! as it was written.

use iced::widget::markdown::Uri;
use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Tree, Widget};
use iced::advanced::renderer;
use iced::widget::{container, rich_text, row, svg, text};
use iced::{mouse, Color, Element, Length, Padding, Rectangle, Size};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, LazyLock, Mutex};

/// Links and images to a typeset formula: `nib-math:<hash>`.
pub const SCHEME: &str = "nib-math:";

/// SVG user units per em: the picture's own size, before the viewer scales it.
const EM: f64 = 40.0;
/// Room around the ink in user units (some glyphs stick out of their boxes a little).
const PAD: f64 = 2.0;
/// Math em per text pixel (KaTeX's default: math a little larger than the text around it).
const SCALE: f32 = 1.15;
/// How far above the baseline the middle of a line of text is, in text sizes; an inline formula
/// is padded so its baseline lines up with the text's.
const MID: f32 = 0.33;

pub struct Formula {
    pub svg: svg::Handle,
    /// In em, padding included.
    pub width: f32,
    /// Above and below the baseline, in em, padding included.
    pub ascent: f32,
    pub descent: f32,
}

/// The formulas a piece of Markdown refers to, by link / image URL.
pub type Formulas = HashMap<String, Arc<Formula>>;

pub struct Prepared {
    /// The Markdown with formulas replaced by references.
    pub md: String,
    pub formulas: Formulas,
    /// `(offset in md, offset in the source)` at the start and end of each replacement.
    marks: Vec<(usize, usize)>,
}

impl Prepared {
    /// Where an offset in `md` is in the source: inside a replacement, the formula's start.
    pub fn source_offset(&self, md: usize) -> usize {
        offset_before(&self.marks, md)
    }
}

/// Where offset `at` of a rewritten text was before the rewrite. `marks` holds `(offset after,
/// offset before)` at the start and end of each replacement; inside one, the answer is its start.
pub fn offset_before(marks: &[(usize, usize)], at: usize) -> usize {
    let i = marks.partition_point(|m| m.0 <= at);
    match i.checked_sub(1) {
        None => at,
        Some(k) if k % 2 == 0 => marks[k].1,
        Some(k) => marks[k].1 + (at - marks[k].0),
    }
}

/// Whether `s` might hold math (a cheap test before `prepare`).
pub fn maybe(s: &str) -> bool {
    s.contains('$') || s.contains("\\(") || s.contains("\\[") || s.contains("```math") || s.contains("~~~math")
}

/// Typesets the formulas in `src` and replaces them with references.
pub fn prepare(src: &str) -> Prepared {
    let mut p = Prepared { md: String::with_capacity(src.len()), formulas: HashMap::new(), marks: Vec::new() };
    if !maybe(src) {
        p.md.push_str(src);
        return p;
    }
    let (skips, inline_only) = regions(src);
    let b = src.as_bytes();
    let (mut i, mut copied, mut k) = (0, 0, 0);
    while i < b.len() {
        while k < skips.len() && skips[k].0.end <= i {
            k += 1;
        }
        if let Some((r, tex)) = skips.get(k).filter(|s| s.0.start <= i) {
            if let Some(tex) = tex {
                p.replace(src, &mut copied, r.clone(), tex, !in_any(&inline_only, r.start));
            }
            i = r.end;
            continue;
        }
        // A formula never runs into the next code span / block.
        let limit = skips.get(k).map_or(b.len(), |s| s.0.start);
        let found = match b[i] {
            b'\\' if matches!(b.get(i + 1), Some(b'[' | b'(')) => {
                let (close, display) = if b[i + 1] == b'[' { ("\\]", true) } else { ("\\)", false) };
                src[i + 2..limit].find(close).map(|j| (i + 2..i + 2 + j, i + 2 + j + 2, display))
            }
            b'\\' => {
                // An escaped character (`\$`, `\\`): text.
                i += 2;
                continue;
            }
            b'$' if b.get(i + 1) == Some(&b'$') => close_display(b, i + 2, limit).map(|j| (i + 2..j, j + 2, true)),
            b'$' => close_inline(b, i, limit).map(|j| (i + 1..j, j + 1, false)),
            _ => None,
        };
        match found {
            Some((tex, end, display)) if !src[tex.clone()].trim().is_empty() => {
                let display = display && !in_any(&inline_only, i);
                p.replace(src, &mut copied, i..end, &src[tex], display);
                i = end;
            }
            _ => i += 1,
        }
    }
    p.md.push_str(&src[copied..]);
    p
}

impl Prepared {
    /// Replaces `src[range]` with a reference to `tex` typeset, if it typesets.
    fn replace(&mut self, src: &str, copied: &mut usize, range: Range<usize>, tex: &str, display: bool) {
        let Some(formula) = typeset(tex.trim(), display) else { return };
        let mut h = DefaultHasher::new();
        (display, tex.trim()).hash(&mut h);
        let url = format!("{SCHEME}{:016x}", h.finish());
        self.md.push_str(&src[*copied..range.start]);
        self.marks.push((self.md.len(), range.start));
        let alt = escape(tex.trim());
        if display {
            let (prefix, at_marker) = line_prefix(src, range.start);
            // An image block of its own (Iced folds a paragraph's text before an image into the
            // image's alt text), in the same list item / quote as the text around it.
            if !at_marker {
                self.md.push_str(&format!("\n{prefix}\n{prefix}"));
            }
            self.md.push_str(&format!("![{alt}]({url})\n{prefix}\n{prefix}"));
        } else {
            self.md.push_str(&format!("[{alt}]({url})"));
        }
        self.marks.push((self.md.len(), range.end));
        *copied = range.end;
        self.formulas.insert(url, formula);
    }
}

/// A range where `$` isn't math, and the formula if it's a ```` ```math ```` block.
type Skip = (Range<usize>, Option<String>);

/// Code (spans, blocks, HTML, front matter) where `$` isn't math, with the text of ```` ```math ````
/// blocks; and headings and tables, where display math stays inline. Both sorted by start.
fn regions(src: &str) -> (Vec<Skip>, Vec<Range<usize>>) {
    let opts = Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS
        | Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let mut skips: Vec<Skip> = Vec::new();
    let mut inline_only = Vec::new();
    // The code block being read: its range, and its text if it is a math block.
    let mut block: Option<(Range<usize>, Option<String>)> = None;
    for (ev, r) in Parser::new_ext(src, opts).into_offset_iter() {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                let math = matches!(&kind, CodeBlockKind::Fenced(lang) if lang.trim() == "math");
                block = Some((r, math.then(String::new)));
            }
            Event::Text(t) => {
                if let Some((_, Some(tex))) = &mut block {
                    tex.push_str(&t);
                }
            }
            Event::End(TagEnd::CodeBlock) => skips.extend(block.take()),
            Event::Start(Tag::HtmlBlock | Tag::MetadataBlock(_)) | Event::Code(_) | Event::Html(_) | Event::InlineHtml(_) => {
                skips.push((r, None))
            }
            Event::Start(Tag::Heading { .. } | Tag::Table(_)) => inline_only.push(r),
            _ => {}
        }
    }
    skips.sort_by_key(|s| s.0.start);
    // HTML events inside an HTML block: keep the outer range only.
    let mut end = 0;
    skips.retain(|s| {
        let keep = s.0.start >= end;
        end = end.max(s.0.end);
        keep
    });
    (skips, inline_only)
}

fn in_any(ranges: &[Range<usize>], at: usize) -> bool {
    ranges.iter().any(|r| r.contains(&at))
}

/// The `$$` closing one opened before `from`.
fn close_display(b: &[u8], from: usize, limit: usize) -> Option<usize> {
    let mut j = from;
    while j + 1 < limit {
        match b[j] {
            b'\\' => j += 2,
            b'$' if b[j + 1] == b'$' => return Some(j),
            _ => j += 1,
        }
    }
    None
}

/// The `$` closing inline math opened at `open` (Pandoc's rule, so prices stay text: the opener
/// isn't followed by a space, the closer isn't preceded by one or followed by a digit; and math
/// doesn't run past the end of the paragraph).
fn close_inline(b: &[u8], open: usize, limit: usize) -> Option<usize> {
    if b.get(open + 1).is_none_or(|c| c.is_ascii_whitespace()) {
        return None;
    }
    let mut j = open + 1;
    while j < limit {
        match b[j] {
            b'\\' => j += 2,
            b'\n' => {
                let rest = &b[j + 1..limit];
                let line = rest.iter().position(|&c| c == b'\n').map_or(rest, |n| &rest[..n]);
                if line.iter().all(|c| c.is_ascii_whitespace()) {
                    return None;
                }
                j += 1;
            }
            b'$' if !b[j - 1].is_ascii_whitespace() && !b.get(j + 1).is_some_and(|c| c.is_ascii_digit()) => {
                return Some(j)
            }
            _ => j += 1,
        }
    }
    None
}

/// What keeps a block on the line at `at` inside its list item / quote (indentation, `>`, a list
/// marker turned into spaces), and whether only that comes before `at` with a list marker in it.
fn line_prefix(src: &str, at: usize) -> (String, bool) {
    let start = src[..at].rfind('\n').map_or(0, |n| n + 1);
    let line = &src.as_bytes()[start..at];
    let (mut prefix, mut marker, mut i) = (String::new(), false, 0);
    while i < line.len() {
        let digits = line[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        match line[i] {
            b' ' | b'\t' | b'>' => {
                prefix.push(line[i] as char);
                i += 1;
            }
            b'-' | b'*' | b'+' if line.get(i + 1) == Some(&b' ') => {
                prefix.push(' ');
                marker = true;
                i += 1;
            }
            _ if digits > 0 && matches!(line.get(i + digits), Some(b'.' | b')')) && line.get(i + digits + 1) == Some(&b' ') => {
                prefix.push_str(&" ".repeat(digits + 1));
                marker = true;
                i += digits + 1;
            }
            _ => return (prefix, false),
        }
    }
    (prefix, marker)
}

/// `tex` as Markdown text (shown only where a viewer doesn't draw the formula).
fn escape(tex: &str) -> String {
    let mut out = String::with_capacity(tex.len() + 8);
    for c in tex.chars() {
        match c {
            '\n' | '\r' => out.push(' '),
            c if c.is_ascii_punctuation() => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// Typeset formulas, so re-parsing (a reply streaming in, a document being edited) doesn't lay
/// them out again. `None`: doesn't parse.
type Cache = HashMap<(bool, String), Option<Arc<Formula>>>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);
const CACHE_MAX: usize = 2048;

fn typeset(tex: &str, display: bool) -> Option<Arc<Formula>> {
    let key = (display, tex.to_string());
    if let Some(f) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return f.clone();
    }
    // A layout bug must not take the editor down; the formula is then shown as written.
    let f = std::panic::catch_unwind(|| render(tex, display)).ok().flatten().map(Arc::new);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= CACHE_MAX {
        cache.clear();
    }
    cache.insert(key, f.clone());
    f
}

fn render(tex: &str, display: bool) -> Option<Formula> {
    use ratex_layout::{layout, to_display_list, validate_layout, LayoutOptions};
    use ratex_types::math_style::MathStyle;
    let ast = ratex_parser::parser::parse(tex).ok()?;
    let style = if display { MathStyle::Display } else { MathStyle::Text };
    let lbox = layout(&ast, &LayoutOptions::default().with_style(style));
    validate_layout(&lbox).ok()?;
    let list = to_display_list(&lbox);
    let opts = ratex_svg::SvgOptions { font_size: EM, padding: PAD, embed_glyphs: true, ..Default::default() };
    let mut svg = ratex_svg::render_to_svg(&list, &opts);
    // The root's width / height are in `pt`, which usvg reads as 4/3 px; plain px keep the
    // picture at `EM` px per em.
    let root = svg.find('>')?;
    svg.replace_range(..root, &svg[..root].replace("pt\"", "\""));
    let pad = PAD / EM;
    Some(Formula {
        svg: svg::Handle::from_memory(svg.into_bytes()),
        width: (list.width + 2.0 * pad) as f32,
        ascent: (list.height + pad) as f32,
        descent: (list.depth + pad) as f32,
    })
}

/// The formula a link / image URL refers to.
pub fn get<'f>(formulas: Option<&'f Formulas>, url: &str) -> Option<&'f Arc<Formula>> {
    url.starts_with(SCHEME).then(|| formulas?.get(url)).flatten()
}

/// A formula's picture, a whole number of pixels in size. Iced draws SVGs snapped to the pixel
/// grid with nearest sampling, so at a fractional size rows of pixels go missing (an `=` loses
/// a bar).
struct Picture {
    svg: svg::Handle,
    color: Color,
    size: Size,
}

impl<M> Widget<M, iced::Theme, iced::Renderer> for Picture {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    /// Shrunk to fit if the room is narrower than the formula.
    fn layout(&mut self, _: &mut Tree, _: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        let scale = (limits.max().width / self.size.width).min(1.0);
        let (w, h) = (self.size.width * scale, self.size.height * scale);
        layout::Node::new(Size::new(w.round().max(1.0), h.round().max(1.0)))
    }

    fn draw(
        &self,
        _: &Tree,
        renderer: &mut iced::Renderer,
        _: &iced::Theme,
        _: &renderer::Style,
        layout: Layout<'_>,
        _: mouse::Cursor,
        _: &Rectangle,
    ) {
        use iced::advanced::svg::Renderer as _;
        let b = layout.bounds();
        renderer.draw_svg(iced::advanced::svg::Svg::new(self.svg.clone()).color(self.color), b, b);
    }
}

fn picture<'a, M: 'a>(f: &Formula, em: f32, color: Color) -> Element<'a, M> {
    let size = Size::new(f.width * em, (f.ascent + f.descent) * em);
    Element::new(Picture { svg: f.svg.clone(), color, size })
}

/// A display formula: centred, shrunk to fit if it's wider than the pane.
pub fn display<'a, M: 'a>(f: &Formula, size: f32, color: Color) -> Element<'a, M> {
    container(picture(f, size * SCALE, color)).center_x(Length::Fill).into()
}

/// Text with inline formulas (a paragraph, a heading, a table cell): its words and formulas in a
/// wrapping row. `None` if it has no formula, for the viewer's usual rendering.
pub fn rich<'a, M: 'a>(
    spans: &[text::Span<'static, Uri>],
    formulas: Option<&Formulas>,
    size: f32,
    color: Color,
    on_link: fn(Uri) -> M,
) -> Option<Element<'a, M>> {
    let is_math = |s: &text::Span<'static, Uri>| s.link.as_deref().and_then(|u| get(formulas, u));
    if !spans.iter().any(|s| is_math(s).is_some()) {
        return None;
    }
    let em = size * SCALE;
    let mut parts: Vec<Element<'a, M>> = Vec::new();
    let mut word: Vec<text::Span<'static, Uri>> = Vec::new();
    let flush = |word: &mut Vec<text::Span<'static, Uri>>, parts: &mut Vec<Element<'a, M>>| {
        if !word.is_empty() {
            parts.push(rich_text(std::mem::take(word)).size(size).on_link_click(on_link).into());
        }
    };
    let mut last_math: Option<&str> = None;
    for s in spans {
        if let Some(f) = is_math(s) {
            // A formula's alt text can come as several spans, all with its link.
            let url = s.link.as_deref();
            if last_math != url {
                flush(&mut word, &mut parts);
                let (a, d) = (f.ascent * em, f.descent * em);
                // Pad so the middle of the box is where a text line's middle is.
                let lift = (2.0 * MID * size - a + d).round();
                let pad = Padding { top: lift.max(0.0), bottom: (-lift).max(0.0), ..Padding::ZERO };
                parts.push(container(picture::<M>(f, em, color)).padding(pad).into());
                last_math = url;
            }
            continue;
        }
        last_math = None;
        // One element per word, so the row can wrap between them.
        let t: &str = &s.text;
        let mut from = 0;
        let mut chars = t.char_indices().peekable();
        while let Some((n, c)) = chars.next() {
            let ends_word = c.is_whitespace() && !chars.peek().is_some_and(|(_, c)| c.is_whitespace());
            if ends_word {
                let end = n + c.len_utf8();
                word.push(text::Span { text: t[from..end].to_string().into(), ..s.clone() });
                flush(&mut word, &mut parts);
                from = end;
            }
        }
        if from < t.len() {
            word.push(text::Span { text: t[from..].to_string().into(), ..s.clone() });
        }
    }
    flush(&mut word, &mut parts);
    Some(row(parts).align_y(iced::Alignment::Center).wrap().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(p: &Prepared) -> usize {
        p.md.matches(SCHEME).count()
    }

    #[test]
    fn finds_formulas() {
        let p = prepare("Euler: $e^{i\\pi}+1=0$ and \\(x^2\\).\n\n$$\n\\int_0^1 x\\,dx\n$$\n\n\\[a=b\\]\n");
        assert_eq!(urls(&p), 4, "{}", p.md);
        assert_eq!(p.formulas.len(), 4);
        assert!(p.md.starts_with("Euler: [e\\^\\{i\\\\pi\\}\\+1\\=0](nib-math:"), "{}", p.md);
        assert_eq!(p.md.matches("![").count(), 2, "{}", p.md);
    }

    #[test]
    fn leaves_prices_code_and_escapes() {
        for s in [
            "costs $5 and $10",
            "between $5-$10",
            "`$x$` and\n\n```\n$$y$$\n```\n",
            "\\$x\\$",
            "$ x $",
            "$a\n\nb$",
        ] {
            let p = prepare(s);
            assert_eq!(p.md, s);
            assert!(p.formulas.is_empty(), "{s}");
        }
        // Doesn't parse: kept as written.
        assert_eq!(prepare("$\\frac{a}{$").md, "$\\frac{a}{$");
    }

    #[test]
    fn math_blocks_and_lists() {
        let p = prepare("```math\nE = mc^2\n```\n");
        assert_eq!(urls(&p), 1, "{}", p.md);
        let p = prepare("- item\n  $$x$$\n- next\n");
        let c = iced::widget::markdown::Content::parse(&p.md);
        let items = c.items();
        assert_eq!(items.len(), 1, "{}", p.md);
        let iced::widget::markdown::Item::List { bullets, .. } = &items[0] else { panic!("{}", p.md) };
        assert_eq!(bullets.len(), 2, "{}", p.md);
        let (iced::widget::markdown::Bullet::Point { items } | iced::widget::markdown::Bullet::Task { items, .. }) = &bullets[0];
        assert!(items.iter().any(|i| matches!(i, iced::widget::markdown::Item::Image { .. })), "{}", p.md);
        // In a table, display math stays in its cell.
        let p = prepare("| a | b |\n|---|---|\n| $$x$$ | 2 |\n");
        assert!(!p.md.contains("!["), "{}", p.md);
    }

    #[test]
    fn maps_offsets_back() {
        let src = "ab $x$ cd";
        let p = prepare(src);
        let cd = p.md.find("cd").unwrap();
        assert_eq!(p.source_offset(cd), src.find("cd").unwrap());
        assert_eq!(p.source_offset(1), 1);
        assert_eq!(p.source_offset(p.md.find('[').unwrap() + 1), 3);
    }
}
