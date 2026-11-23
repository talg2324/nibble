//! The diff view: a custom Iced widget drawing a diff as rows on a monospace grid (a file's
//! header, a hunk's header, then its lines with both line numbers and a tint for added and
//! removed ones). Like `EditorView` it keeps no state of the diff: the App owns the rows, the
//! scroll position and the cursor row, and applies the events this publishes.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::text::{self, Paragraph as _};
use iced::advanced::widget::{tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::alignment;
use iced::{mouse, Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Shadow, Size};
use nib_editor::document::char_width;
use nib_proto::{DiffFile, DiffLineKind, DiffStatus};
use std::ops::Range;

use crate::editor_view::Typeface;
use crate::theme::Theme;

/// One row of the view. Indexes are into the diff's files, a file's hunks and a hunk's lines.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// A file's header. `open`: its hunks follow.
    File { file: usize, open: bool },
    Hunk { file: usize, hunk: usize },
    /// `old` / `new`: the line's number on each side (1-based), where it exists.
    Line { file: usize, hunk: usize, line: usize, old: Option<u32>, new: Option<u32> },
    /// Something to say instead of lines ("binary file").
    Note { file: usize, text: &'static str },
    /// Empty, between two files.
    Gap,
    /// One row of a review comment, under the line (or file) it is on; `file: None`: above the
    /// files (about the work as a whole, or on a file that is no longer in the diff).
    /// `comment`: its id (`None`: the one being written).
    Talk { file: Option<usize>, comment: Option<String>, kind: Talk, text: String },
}

/// What a row of a comment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Talk {
    /// Who, where, and how it stands.
    Head,
    /// The same, of a resolved comment (which shows nothing else).
    Settled,
    Body,
    Reply,
    /// Being typed.
    Draft,
    /// The keys that act on the draft.
    Hint,
}

impl Row {
    pub fn file(&self) -> Option<usize> {
        match self {
            Row::File { file, .. } | Row::Hunk { file, .. } | Row::Line { file, .. } | Row::Note { file, .. } => Some(*file),
            Row::Talk { file, .. } => *file,
            Row::Gap => None,
        }
    }
}

/// A coloured stretch of a line: bytes of its text, and the syntax scope's name.
pub type Paint = (Range<usize>, &'static str);

#[derive(Debug, Clone, Copy)]
pub enum DiffEvent {
    Click { row: usize },
    /// Positive = down, in rows.
    Scroll { rows: f32 },
    /// Rows and columns that fit.
    Resized { rows: usize, cols: usize },
}

pub struct DiffView<'a, Message> {
    files: &'a [DiffFile],
    rows: &'a [Row],
    /// Per row: how to colour a line's text (empty: plain).
    paint: &'a [Vec<Paint>],
    theme: &'a Theme,
    top: usize,
    cursor: usize,
    focused: bool,
    /// A marked row: the rows from it to the cursor are picked (for a comment).
    mark: Option<usize>,
    face: Typeface,
    on_event: Box<dyn Fn(DiffEvent) -> Message + 'a>,
}

impl<'a, Message> DiffView<'a, Message> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        files: &'a [DiffFile],
        rows: &'a [Row],
        paint: &'a [Vec<Paint>],
        theme: &'a Theme,
        top: usize,
        cursor: usize,
        focused: bool,
        face: Typeface,
        on_event: impl Fn(DiffEvent) -> Message + 'a,
    ) -> Self {
        Self { files, rows, paint, theme, top, cursor, focused, mark: None, face, on_event: Box::new(on_event) }
    }

    pub fn mark(mut self, mark: Option<usize>) -> Self {
        self.mark = mark;
        self
    }

    /// Columns the two line-number columns take, each.
    fn number_cols(&self) -> usize {
        let most = self
            .files
            .iter()
            .flat_map(|f| f.hunks.iter())
            .map(|h| (h.old_start + h.old_len).max(h.new_start + h.new_len))
            .max()
            .unwrap_or(0);
        most.to_string().len().max(3)
    }
}

#[derive(Default)]
struct State {
    /// Wheel movement not yet a whole row (`zoom::whole_lines`).
    wheel: f32,
    char_w: f32,
    line_h: f32,
    measured_for: Option<Typeface>,
    last_size: (usize, usize),
}

/// Tabs in a diff line are shown this wide.
const TAB: usize = 4;

pub fn status_word(s: DiffStatus) -> &'static str {
    match s {
        DiffStatus::Added => "new",
        DiffStatus::Modified => "changed",
        DiffStatus::Deleted => "deleted",
        DiffStatus::Renamed => "renamed",
    }
}

/// `c` laid over `under` at `alpha`: a tint that stays readable on any background.
pub fn blend(under: Color, c: Color, alpha: f32) -> Color {
    let mix = |a: f32, b: f32| a + (b - a) * alpha;
    Color::from_rgb(mix(under.r, c.r), mix(under.g, c.g), mix(under.b, c.b))
}

impl<Message, Th, Renderer> Widget<Message, Th, Renderer> for DiffView<'_, Message>
where
    Renderer: text::Renderer<Font = Font>,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(&mut self, tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        let st = tree.state.downcast_mut::<State>();
        if st.measured_for != Some(self.face) {
            let probe = "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM";
            let p = Renderer::Paragraph::with_text(text::Text {
                content: probe,
                bounds: Size::INFINITE,
                size: Pixels(self.face.size),
                line_height: text::LineHeight::Relative(self.face.line_height),
                font: self.face.font,
                align_x: text::Alignment::Left,
                align_y: alignment::Vertical::Top,
                shaping: text::Shaping::Basic,
                wrapping: text::Wrapping::None,
            });
            let b = p.min_bounds();
            st.char_w = b.width / probe.len() as f32;
            st.line_h = b.height;
            st.measured_for = Some(self.face);
        }
        layout::Node::new(limits.max())
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let st = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();
        if st.char_w <= 0.0 {
            return;
        }
        let rows = (bounds.height / st.line_h).floor().max(1.0) as usize;
        let cols = (bounds.width / st.char_w).floor().max(1.0) as usize;
        if st.last_size != (rows, cols) {
            st.last_size = (rows, cols);
            shell.publish((self.on_event)(DiffEvent::Resized { rows, cols }));
        }
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(p) = cursor.position_over(bounds) {
                    let row = self.top + ((p.y - bounds.y) / st.line_h).max(0.0) as usize;
                    if row < self.rows.len() {
                        shell.publish((self.on_event)(DiffEvent::Click { row }));
                    }
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(bounds) => {
                let rows = match *delta {
                    mouse::ScrollDelta::Lines { y, .. } => -y * 3.0,
                    mouse::ScrollDelta::Pixels { y, .. } => -y / st.line_h,
                };
                let rows = crate::zoom::whole_lines(&mut st.wheel, rows);
                if rows == 0.0 {
                    shell.capture_event();
                    return;
                }
                shell.publish((self.on_event)(DiffEvent::Scroll { rows }));
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Th,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let st = tree.state.downcast_ref::<State>();
        let th = self.theme;
        let bounds = layout.bounds();
        let (cw, lh) = (st.char_w, st.line_h);
        if cw <= 0.0 {
            return;
        }
        let quad = |r: &mut Renderer, rect: Rectangle, c: Color| {
            r.fill_quad(Quad { bounds: rect, border: Border::default(), shadow: Shadow::default(), snap: true }, c);
        };
        quad(renderer, bounds, th.background);
        let mk_text = |content: String| text::Text {
            content,
            bounds: Size::new(f32::INFINITY, lh),
            size: Pixels(self.face.size),
            line_height: text::LineHeight::Absolute(Pixels(lh)),
            font: self.face.font,
            align_x: text::Alignment::Left,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Basic,
            wrapping: text::Wrapping::None,
        };
        let ncols = self.number_cols();
        // space, old number, space, new number, space, sign, space.
        let gutter_w = (2 * ncols + 5) as f32 * cw;
        let text_x = bounds.x + gutter_w;
        let text_clip = Rectangle { x: text_x, width: (bounds.width - gutter_w).max(0.0), ..bounds };
        let (add_bg, del_bg) = (blend(th.background, th.diff_add, 0.16), blend(th.background, th.diff_del, 0.16));
        let head_bg = blend(th.background, th.foreground, 0.07);
        let visible = (bounds.height / lh).ceil() as usize;

        for (i, row) in self.rows.iter().enumerate().skip(self.top).take(visible) {
            let y = bounds.y + (i - self.top) as f32 * lh;
            let band = Rectangle { x: bounds.x, y, width: bounds.width, height: lh };
            let Some(band) = band.intersection(&bounds) else { continue };
            let put = |r: &mut Renderer, s: String, col: usize, c: Color| {
                r.fill_text(mk_text(s), Point::new(bounds.x + col as f32 * cw, y), c, band);
            };
            match row {
                Row::Gap => {}
                Row::File { file, open } => {
                    let Some(f) = self.files.get(*file) else { continue };
                    quad(renderer, band, head_bg);
                    let name = match &f.old_path {
                        Some(old) => format!("{old} → {}", f.path),
                        None => f.path.clone(),
                    };
                    let mut col = 1;
                    put(renderer, if *open { "▾".into() } else { "▸".into() }, col, th.dim);
                    col += 2;
                    let n = name.chars().count();
                    put(renderer, name, col, th.foreground);
                    col += n + 2;
                    let word = status_word(f.status).to_string();
                    let n = word.len();
                    put(renderer, word, col, th.dim);
                    col += n + 2;
                    if f.adds > 0 {
                        let s = format!("+{}", f.adds);
                        let n = s.len();
                        put(renderer, s, col, th.diff_add);
                        col += n + 1;
                    }
                    if f.dels > 0 {
                        put(renderer, format!("−{}", f.dels), col, th.diff_del);
                    }
                }
                Row::Hunk { file, hunk } => {
                    let Some(h) = self.files.get(*file).and_then(|f| f.hunks.get(*hunk)) else { continue };
                    let s = format!("@@ −{},{} +{},{} @@ {}", h.old_start, h.old_len, h.new_start, h.new_len, h.header);
                    put(renderer, s, 2 * ncols + 5, th.dim);
                }
                Row::Note { text, .. } => put(renderer, (*text).to_string(), 2 * ncols + 5, th.dim),
                Row::Talk { kind, text, .. } => {
                    // A card under the code: tinted, with a bar down its left side.
                    let x = bounds.x + (2 * ncols + 5) as f32 * cw;
                    let card = Rectangle { x, width: (bounds.x + bounds.width - x - cw).max(0.0), ..band };
                    let settled = *kind == Talk::Settled;
                    quad(renderer, card, blend(th.background, th.accent, if settled { 0.05 } else { 0.10 }));
                    quad(renderer, Rectangle { width: 2.0, ..card }, if settled { th.dim } else { th.accent });
                    let color = match kind {
                        Talk::Head => th.accent,
                        Talk::Settled | Talk::Hint => th.dim,
                        Talk::Body | Talk::Reply | Talk::Draft => th.foreground,
                    };
                    put(renderer, text.clone(), 2 * ncols + 7, color);
                }
                Row::Line { file, hunk, line, old, new } => {
                    let Some(l) = self.files.get(*file).and_then(|f| f.hunks.get(*hunk)).and_then(|h| h.lines.get(*line)) else {
                        continue;
                    };
                    let (bg, sign, sign_color) = match l.kind {
                        DiffLineKind::Add => (Some(add_bg), "+", th.diff_add),
                        DiffLineKind::Del => (Some(del_bg), "−", th.diff_del),
                        DiffLineKind::Context => (None, " ", th.dim),
                    };
                    if let Some(bg) = bg {
                        quad(renderer, band, bg);
                    }
                    let num = |n: &Option<u32>| n.map_or(" ".repeat(ncols), |n| format!("{n:>ncols$}"));
                    put(renderer, format!(" {} {}", num(old), num(new)), 0, th.gutter);
                    put(renderer, sign.to_string(), 2 * ncols + 3, sign_color);

                    // The text, in runs of one colour; a tab goes to the next multiple of TAB.
                    let paint = self.paint.get(i).map(Vec::as_slice).unwrap_or(&[]);
                    let mut pi = 0;
                    let (mut col, mut run, mut run_col, mut run_color) = (0usize, String::new(), 0usize, th.foreground);
                    let flush = |r: &mut Renderer, run: &mut String, run_col: usize, color: Color| {
                        if !run.trim().is_empty() {
                            r.fill_text(mk_text(std::mem::take(run)), Point::new(text_x + run_col as f32 * cw, y), color, text_clip.intersection(&band).unwrap_or(band));
                        }
                        run.clear();
                    };
                    for (byte, ch) in l.text.char_indices() {
                        while pi < paint.len() && paint[pi].0.end <= byte {
                            pi += 1;
                        }
                        let color = match paint.get(pi) {
                            Some((r, scope)) if r.start <= byte => th.scope(scope),
                            _ => th.foreground,
                        };
                        if color != run_color {
                            flush(renderer, &mut run, run_col, run_color);
                            run_color = color;
                        }
                        if run.is_empty() {
                            run_col = col;
                        }
                        if ch == '\t' {
                            let w = TAB - col % TAB;
                            run.extend(std::iter::repeat_n(' ', w));
                            col += w;
                        } else {
                            run.push(ch);
                            col += char_width(ch);
                        }
                    }
                    flush(renderer, &mut run, run_col, run_color);
                }
            }
            if self.mark.is_some_and(|m| (m.min(self.cursor)..=m.max(self.cursor)).contains(&i)) && matches!(row, Row::Line { .. }) {
                quad(renderer, band, Color { a: 0.22, ..th.selection });
            }
            if i == self.cursor {
                // The row the keys act on: a bar at the left edge, brighter with the keyboard here.
                let c = if self.focused { th.accent } else { th.dim };
                quad(renderer, Rectangle { width: 3.0, ..band }, c);
                quad(renderer, band, Color { a: 0.06, ..th.foreground });
            }
        }
    }
}

impl<'a, Message: 'a, Th, Renderer> From<DiffView<'a, Message>> for Element<'a, Message, Th, Renderer>
where
    Renderer: text::Renderer<Font = Font> + 'a,
{
    fn from(v: DiffView<'a, Message>) -> Self {
        Element::new(v)
    }
}
