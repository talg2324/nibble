//! Text the mouse can select: like `rich_text`, plus a press, a drag and a release that say
//! where in the text they are (a byte offset), and a highlight over the part that's selected.
//!
//! The widget only reports; what is selected is the app's (a selection can run over many of
//! these, like a chat's paragraphs), and it says which part of this one to highlight. Each
//! line (between `\n`s) is laid out on its own, so a point maps to an offset unambiguously.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Renderer as _};
use iced::advanced::text::{self as core_text, Hit, Paragraph as _, Renderer as _};
use iced::advanced::widget::tree::{self, Tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::alignment;
use iced::advanced::mouse::{self, click::Kind, Click};
use iced::widget::text::{LineHeight, Shaping, Span, Wrapping};
use iced::{Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Size, Vector};

type Paragraph = <iced::Renderer as core_text::Renderer>::Paragraph;

/// What the mouse did, at a byte offset into the whole text.
#[derive(Debug, Clone)]
pub enum Ev<Link> {
    /// Pressed on the text: one click, two (a word) or three (the whole text).
    Press(usize, Kind),
    /// Moved to here with the button held (somewhere a press started a selection).
    Drag(usize),
    /// The button let go, after a press on this text.
    Release,
    /// A link clicked (pressed and let go in the same place, no drag).
    Link(Link),
}

pub struct Selectable<'a, Link, Message> {
    /// The spans of each line, and where each line starts in the whole text.
    lines: Vec<(usize, Vec<Span<'static, Link, Font>>)>,
    len: usize,
    size: Pixels,
    font: Option<Font>,
    color: Option<Color>,
    width: Length,
    /// The part highlighted, start to end.
    selected: Option<(usize, usize)>,
    sel_bg: Color,
    /// A selection is being dragged (here or in another one): moves inside this report `Drag`.
    dragging: bool,
    on_event: Box<dyn Fn(Ev<Link>) -> Message + 'a>,
}

/// The whole text of these spans.
pub fn plain<Link>(spans: &[Span<'_, Link, Font>]) -> String {
    spans.iter().map(|s| s.text.as_ref()).collect()
}

/// The spans cut at each `\n` into lines (the `\n`s dropped), with where each line starts.
fn split_lines<Link: Clone>(spans: &[Span<'_, Link, Font>]) -> Vec<(usize, Vec<Span<'static, Link, Font>>)> {
    let mut lines = vec![(0, Vec::new())];
    let mut at = 0;
    for s in spans {
        let text: &str = s.text.as_ref();
        for (n, piece) in text.split('\n').enumerate() {
            if n > 0 {
                at += 1;
                lines.push((at, Vec::new()));
            }
            if !piece.is_empty() {
                let mut part = s.clone().to_static();
                part.text = piece.to_string().into();
                lines.last_mut().expect("never empty").1.push(part);
            }
            at += piece.len();
        }
    }
    lines
}

/// A line's spans cut again at `lo..hi` (offsets into the line), that part highlighted.
fn highlighted<Link: Clone>(spans: &[Span<'static, Link, Font>], lo: usize, hi: usize, bg: Color) -> Vec<Span<'static, Link, Font>> {
    let mut out = Vec::with_capacity(spans.len() + 2);
    let mut at = 0;
    for s in spans {
        let text: &str = s.text.as_ref();
        let (a, b) = (at, at + text.len());
        at = b;
        // Cut points inside this span, on char boundaries (the offsets come from hit tests).
        let cuts = [lo.clamp(a, b) - a, hi.clamp(a, b) - a];
        for (from, to, on) in [(0, cuts[0], false), (cuts[0], cuts[1], true), (cuts[1], text.len(), false)] {
            if from < to && text.is_char_boundary(from) && text.is_char_boundary(to) {
                let mut part = s.clone();
                part.text = text[from..to].to_string().into();
                if on {
                    part.highlight = Some(core_text::Highlight { background: bg.into(), border: Border::default() });
                }
                out.push(part);
            }
        }
    }
    out
}

impl<'a, Link: Clone + 'static, Message> Selectable<'a, Link, Message> {
    pub fn new(spans: &[Span<'_, Link, Font>], on_event: impl Fn(Ev<Link>) -> Message + 'a) -> Self {
        let len = spans.iter().map(|s| s.text.len()).sum();
        Self {
            lines: split_lines(spans),
            len,
            size: Pixels(14.0),
            font: None,
            color: None,
            width: Length::Shrink,
            selected: None,
            sel_bg: Color::TRANSPARENT,
            dragging: false,
            on_event: Box::new(on_event),
        }
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = Pixels(size);
        self
    }

    pub fn font(mut self, font: Font) -> Self {
        self.font = Some(font);
        self
    }

    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Highlight `range` (byte offsets into the whole text) in `bg`.
    pub fn selected(mut self, range: Option<(usize, usize)>, bg: Color) -> Self {
        self.selected = range.filter(|(a, b)| a < b);
        self.sel_bg = bg;
        self
    }

    pub fn dragging(mut self, on: bool) -> Self {
        self.dragging = on;
        self
    }

    /// The spans line `n` is laid out with: with the selected part of it highlighted.
    fn line_spans(&self, n: usize) -> Vec<Span<'static, Link, Font>> {
        let (start, spans) = &self.lines[n];
        let end = start + spans.iter().map(|s| s.text.len()).sum::<usize>();
        match self.selected {
            Some((a, b)) if a < end && b > *start => {
                let (lo, hi) = (a.saturating_sub(*start), b.min(end) - start);
                highlighted(spans, lo, hi, self.sel_bg)
            }
            _ => spans.clone(),
        }
    }

    fn line_height(&self) -> f32 {
        LineHeight::default().to_absolute(self.size).0
    }
}

/// How every line is laid out: left, top, wrapped at words.
fn shaped<C>(content: C, bounds: Size, size: Pixels, font: Font) -> core_text::Text<C, Font> {
    core_text::Text {
        content,
        bounds,
        size,
        line_height: LineHeight::default(),
        font,
        align_x: core_text::Alignment::Default,
        align_y: alignment::Vertical::Top,
        shaping: Shaping::Advanced,
        wrapping: Wrapping::Word,
    }
}

struct Line<Link> {
    spans: Vec<Span<'static, Link, Font>>,
    paragraph: Paragraph,
    /// Its top, from the widget's.
    top: f32,
    height: f32,
}

struct State<Link> {
    lines: Vec<Line<Link>>,
    /// Where the press that is still held was, and the clicks so far (for double and triple).
    pressed: Option<Point>,
    last_click: Option<Click>,
    /// The link under the mouse: its line and span.
    hovered: Option<(usize, usize)>,
}

impl<Link: Clone + 'static, Message> Selectable<'_, Link, Message> {
    /// The byte offset at `p` (relative to the widget): above the text its start, below its end.
    fn offset_at(&self, state: &State<Link>, p: Point) -> usize {
        let Some(first) = state.lines.first() else { return 0 };
        if p.y < first.top {
            return 0;
        }
        let n = state.lines.iter().rposition(|l| p.y >= l.top).unwrap_or(0);
        let line = &state.lines[n];
        if p.y >= line.top + line.height && n + 1 == state.lines.len() {
            return self.len;
        }
        let (start, spans) = &self.lines[n];
        let len: usize = spans.iter().map(|s| s.text.len()).sum();
        let at = match line.paragraph.hit_test(Point::new(p.x.max(0.0), (p.y - line.top).clamp(0.0, line.height - 0.5))) {
            Some(Hit::CharOffset(i)) => i.min(len),
            None if p.x <= 0.0 => 0,
            None => len,
        };
        start + at
    }

    fn link_at(&self, state: &State<Link>, p: Point) -> Option<(usize, usize)> {
        let n = state.lines.iter().position(|l| p.y >= l.top && p.y < l.top + l.height)?;
        let line = &state.lines[n];
        let i = line.paragraph.hit_span(Point::new(p.x, p.y - line.top))?;
        line.spans.get(i)?.link.as_ref().map(|_| (n, i))
    }
}

impl<Link: Clone + 'static, Message> Widget<Message, iced::Theme, iced::Renderer> for Selectable<'_, Link, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<Link>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<Link> { lines: Vec::new(), pressed: None, last_click: None, hovered: None })
    }

    fn size(&self) -> Size<Length> {
        Size { width: self.width, height: Length::Shrink }
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        let state = tree.state.downcast_mut::<State<Link>>();
        let font = self.font.unwrap_or_else(|| renderer.default_font());
        let lh = self.line_height();
        layout::sized(limits, self.width, Length::Shrink, |limits| {
            let bounds = Size::new(limits.max().width, f32::INFINITY);
            state.lines.truncate(self.lines.len());
            let (mut top, mut width) = (0.0f32, 0.0f32);
            for n in 0..self.lines.len() {
                let spans = self.line_spans(n);
                let size = self.size;
                let fresh = |spans: &[Span<'static, Link, Font>]| Paragraph::with_spans(shaped(spans, bounds, size, font));
                if let Some(line) = state.lines.get_mut(n) {
                    if line.spans != spans {
                        line.paragraph = fresh(&spans);
                        line.spans = spans;
                    } else {
                        let shape = shaped((), bounds, self.size, font);
                        match line.paragraph.compare(shape) {
                            core_text::Difference::None => {}
                            core_text::Difference::Bounds => line.paragraph.resize(bounds),
                            core_text::Difference::Shape => line.paragraph = fresh(&line.spans),
                        }
                    }
                } else {
                    let paragraph = fresh(&spans);
                    state.lines.push(Line { spans, paragraph, top: 0.0, height: 0.0 });
                }
                let line = &mut state.lines[n];
                let size = line.paragraph.min_bounds();
                line.top = top;
                line.height = size.height.max(lh);
                top += line.height;
                width = width.max(size.width);
            }
            Size::new(width, top)
        })
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &iced::Theme,
        defaults: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if !bounds.intersects(viewport) {
            return;
        }
        let state = tree.state.downcast_ref::<State<Link>>();
        let color = self.color.unwrap_or(defaults.text_color);
        for (n, line) in state.lines.iter().enumerate() {
            let at = bounds.position() + Vector::new(0.0, line.top);
            for (i, s) in line.spans.iter().enumerate() {
                let hovered = state.hovered == Some((n, i));
                if s.highlight.is_none() && !s.underline && !hovered {
                    continue;
                }
                let regions = line.paragraph.span_bounds(i);
                if let Some(h) = s.highlight {
                    for r in &regions {
                        renderer.fill_quad(renderer::Quad { bounds: *r + (at - Point::ORIGIN), border: h.border, ..Default::default() }, h.background);
                    }
                }
                if s.underline || hovered {
                    let size = s.size.unwrap_or(self.size).0;
                    let base = size + (self.line_height() - size) / 2.0 - size * 0.08;
                    for r in &regions {
                        let under = Rectangle::new(Point::new(at.x + r.x, at.y + r.y + base), Size::new(r.width, 1.0));
                        renderer.fill_quad(renderer::Quad { bounds: under, ..Default::default() }, s.color.unwrap_or(color));
                    }
                }
            }
            renderer.fill_paragraph(&line.paragraph, at, color, *viewport);
        }
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State<Link>>();
        let bounds = layout.bounds();
        let local = |p: Point| Point::new(p.x - bounds.x, p.y - bounds.y);
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let hovered = cursor.position_in(bounds).and_then(|p| self.link_at(state, p));
                if hovered != state.hovered {
                    state.hovered = hovered;
                    shell.request_redraw();
                }
                // Only the text the pointer is level with moves the selection's end.
                if let Some(p) = cursor.position().filter(|p| self.dragging && p.y >= bounds.y && p.y < bounds.y + bounds.height) {
                    shell.publish((self.on_event)(Ev::Drag(self.offset_at(state, local(p)))));
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(p) = cursor.position_in(bounds) else { return };
                let click = Click::new(cursor.position().unwrap_or(p), mouse::Button::Left, state.last_click);
                state.last_click = Some(click);
                state.pressed = cursor.position();
                shell.publish((self.on_event)(Ev::Press(self.offset_at(state, p), click.kind())));
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                let Some(from) = state.pressed.take() else { return };
                let still = cursor.position().is_some_and(|p| p.distance(from) < 4.0);
                let single = state.last_click.is_some_and(|c| c.kind() == Kind::Single);
                if let (true, true, Some((n, i))) = (still, single, state.hovered) {
                    if let Some(link) = state.lines.get(n).and_then(|l| l.spans.get(i)).and_then(|s| s.link.clone()) {
                        shell.publish((self.on_event)(Ev::Link(link)));
                    }
                }
                shell.publish((self.on_event)(Ev::Release));
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State<Link>>();
        match cursor.position_in(layout.bounds()) {
            Some(_) if state.hovered.is_some() => mouse::Interaction::Pointer,
            Some(_) => mouse::Interaction::Text,
            None => mouse::Interaction::None,
        }
    }
}

impl<'a, Link: Clone + 'static, Message: 'a> From<Selectable<'a, Link, Message>> for Element<'a, Message> {
    fn from(s: Selectable<'a, Link, Message>) -> Self {
        Element::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(parts: &[&str]) -> Vec<Span<'static, (), Font>> {
        parts.iter().map(|p| Span::new(p.to_string())).collect()
    }

    fn texts(spans: &[Span<'static, (), Font>]) -> Vec<(String, bool)> {
        spans.iter().map(|s| (s.text.to_string(), s.highlight.is_some())).collect()
    }

    #[test]
    fn lines_split_at_newlines_and_know_where_they_start() {
        let lines = split_lines(&spans(&["one\ntw", "o\n", "\nthree"]));
        let got: Vec<(usize, String)> = lines.iter().map(|(at, s)| (*at, plain(s))).collect();
        assert_eq!(got, vec![(0, "one".into()), (4, "two".into()), (8, "".into()), (9, "three".into())]);
        assert_eq!(plain(&spans(&["one\ntw", "o\n", "\nthree"])).len(), 14);
    }

    #[test]
    fn the_selected_part_is_cut_out_and_highlighted() {
        let got = texts(&highlighted(&spans(&["hello ", "world"]), 3, 8, Color::WHITE));
        assert_eq!(
            got,
            vec![("hel".into(), false), ("lo ".into(), true), ("wo".into(), true), ("rld".into(), false)]
        );
        let all = texts(&highlighted(&spans(&["héllo"]), 0, 6, Color::WHITE));
        assert_eq!(all, vec![("héllo".into(), true)]);
    }
}
