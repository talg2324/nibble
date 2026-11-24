//! The code view: a custom Iced widget drawing a `nib_editor::Document` on a monospace grid.
//!
//! The widget is stateless w.r.t. the document (the App owns it); it only keeps font metrics and
//! input state (drag, modifiers). It publishes high-level messages (click at line/col, scroll,
//! resized) and the App applies them. Only visible lines are drawn; the CPU renderer's damage
//! tracking then repaints only what changed (the Horizon requirement).

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::text::{self, Paragraph as _};
use iced::advanced::widget::{tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::alignment;
use iced::{
    mouse, Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Shadow, Size,
};
use nib_editor::document::{char_width, line_end};
use nib_editor::Document;

use crate::theme::Theme;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub enum ViewEvent {
    /// Click at a (line, visual column) in document coordinates.
    Click {
        line: usize,
        col: usize,
        extend: bool,
        /// Cmd-click (Ctrl elsewhere): go to the definition.
        jump: bool,
        /// The second press of a double click: select the word there.
        double: bool,
    },
    Drag {
        line: usize,
        col: usize,
    },
    /// Positive = down, in lines.
    Scroll {
        lines: f32,
        cols: f32,
    },
    /// The mouse moved onto the text cell at (line, visual column). `cell`: that cell, and
    /// `area`: the text area, in window coordinates (for placing a popup).
    Hover {
        line: usize,
        col: usize,
        cell: Rectangle,
        area: Rectangle,
    },
    /// The mouse left the text (to the gutter, past the last line, off the view, onto an overlay).
    HoverEnd,
    /// Visible text area in rows/columns.
    Resized {
        rows: usize,
        cols: usize,
    },
}

/// A tinted stretch of text, drawn behind it: what a followed agent is reading or just wrote.
#[derive(Debug, Clone, Copy)]
pub struct Mark {
    /// Char range in the document.
    pub from: usize,
    pub to: usize,
    pub color: Color,
    /// Tint the whole rows it touches, edge to edge (lines being read), not just the characters.
    pub rows: bool,
}

/// Code font: family, size in px, line height as a multiple of size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Typeface {
    pub font: Font,
    pub size: f32,
    pub line_height: f32,
}

/// Cells between a line's end and its blame.
const BLAME_GAP: usize = 4;

pub struct EditorView<'a, Message> {
    doc: &'a Document,
    theme: &'a Theme,
    top: usize,
    left: usize,
    face: Typeface,
    marks: Vec<Mark>,
    /// Someone else drives this editor: their name on a flag at the cursor, and its colour.
    tag: Option<(String, Color)>,
    /// Git blame for the selection: dim text after the end of this 0-based line.
    blame: Option<(usize, String)>,
    on_event: Box<dyn Fn(ViewEvent) -> Message + 'a>,
}

impl<'a, Message> EditorView<'a, Message> {
    pub fn new(
        doc: &'a Document,
        theme: &'a Theme,
        top: usize,
        left: usize,
        face: Typeface,
        on_event: impl Fn(ViewEvent) -> Message + 'a,
    ) -> Self {
        Self {
            doc,
            theme,
            top,
            left,
            face,
            marks: Vec::new(),
            tag: None,
            blame: None,
            on_event: Box::new(on_event),
        }
    }

    pub fn marks(mut self, marks: Vec<Mark>) -> Self {
        self.marks = marks;
        self
    }

    pub fn tag(mut self, tag: Option<(String, Color)>) -> Self {
        self.tag = tag;
        self
    }

    pub fn blame(mut self, blame: Option<(usize, String)>) -> Self {
        self.blame = blame;
        self
    }
}

#[derive(Default)]
struct State {
    /// Wheel movement not yet a whole line / column (`zoom::whole_lines`).
    wheel: (f32, f32),
    char_w: f32,
    line_h: f32,
    measured_for: Option<Typeface>,
    dragging: bool,
    shift: bool,
    /// Cmd on macOS, Ctrl elsewhere.
    command: bool,
    last_size: (usize, usize),
    /// The cell under the mouse last reported with `Hover`.
    hover: Option<(usize, usize)>,
    /// When and where the last left press was (for double clicks).
    last_press: Option<(Instant, (usize, usize))>,
}

/// Two presses on one cell within this are a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

fn gutter_cols(doc: &Document) -> usize {
    doc.text.len_lines().to_string().len() + 3
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for EditorView<'_, Message>
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

    fn layout(
        &mut self,
        tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let st = tree.state.downcast_mut::<State>();
        if st.measured_for != Some(self.face) {
            // Measure the monospace advance once per typeface.
            let probe = "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM";
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
        let gutter_w = gutter_cols(self.doc) as f32 * st.char_w;
        let rows = (bounds.height / st.line_h).floor().max(1.0) as usize;
        let cols = ((bounds.width - gutter_w) / st.char_w).floor().max(1.0) as usize;
        if st.last_size != (rows, cols) {
            st.last_size = (rows, cols);
            shell.publish((self.on_event)(ViewEvent::Resized { rows, cols }));
        }
        let to_doc = |p: Point, st: &State| {
            let line = self.top + ((p.y - bounds.y) / st.line_h).max(0.0) as usize;
            let col =
                self.left + ((p.x - bounds.x - gutter_w) / st.char_w).round().max(0.0) as usize;
            (line, col)
        };
        match event {
            Event::Keyboard(iced::keyboard::Event::ModifiersChanged(m)) => {
                st.shift = m.shift();
                st.command = m.command();
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(p) = cursor.position_over(bounds) {
                    let (line, col) = to_doc(p, st);
                    let now = Instant::now();
                    let double = !st.shift
                        && !st.command
                        && st.last_press.is_some_and(|(t, at)| {
                            at == (line, col) && now.duration_since(t) < DOUBLE_CLICK
                        });
                    // A third press starts over rather than counting as another double click.
                    st.last_press = (!double).then_some((now, (line, col)));
                    st.dragging = !st.command && !double;
                    st.hover = None;
                    shell.publish((self.on_event)(ViewEvent::Click {
                        line,
                        col,
                        extend: st.shift,
                        jump: st.command,
                        double,
                    }));
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if st.dragging => {
                let (line, col) = to_doc(*position, st);
                shell.publish((self.on_event)(ViewEvent::Drag { line, col }));
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => st.dragging = false,
            // `cursor`, not the event's position: it is levitated when an overlay is on top.
            Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) => {
                let area = Rectangle { x: bounds.x + gutter_w, width: bounds.width - gutter_w, ..bounds };
                let hit = cursor.position_over(area).and_then(|p| {
                    let row = ((p.y - bounds.y) / st.line_h).floor() as usize;
                    let line = self.top + row;
                    let col = self.left + ((p.x - area.x) / st.char_w).floor() as usize;
                    (line < self.doc.text.len_lines()).then_some((line, col, row))
                });
                let now = hit.map(|(line, col, _)| (line, col));
                if now != st.hover {
                    st.hover = now;
                    let ev = match hit {
                        Some((line, col, row)) => {
                            let cell = Rectangle {
                                x: area.x + (col - self.left) as f32 * st.char_w,
                                y: bounds.y + row as f32 * st.line_h,
                                width: st.char_w,
                                height: st.line_h,
                            };
                            ViewEvent::Hover { line, col, cell, area }
                        }
                        None => ViewEvent::HoverEnd,
                    };
                    shell.publish((self.on_event)(ev));
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(bounds) => {
                // What's under the mouse changes: the next move reports it again.
                st.hover = None;
                let (lines, cols) = match *delta {
                    mouse::ScrollDelta::Lines { x, y } => (-y * 3.0, -x * 3.0),
                    mouse::ScrollDelta::Pixels { x, y } => (-y / st.line_h, -x / st.char_w),
                };
                let (lines, cols) = (crate::zoom::whole_lines(&mut st.wheel.0, lines), crate::zoom::whole_lines(&mut st.wheel.1, cols));
                if lines == 0.0 && cols == 0.0 {
                    shell.capture_event();
                    return;
                }
                shell.publish((self.on_event)(ViewEvent::Scroll { lines, cols }));
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        _t: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _v: &Rectangle,
        _r: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let t0 = std::time::Instant::now();
        let st = tree.state.downcast_ref::<State>();
        let th = self.theme;
        let bounds = layout.bounds();
        let (cw, lh) = (st.char_w, st.line_h);
        if cw <= 0.0 {
            return;
        }
        let quad = |r: &mut Renderer, rect: Rectangle, c: Color| {
            r.fill_quad(
                Quad {
                    bounds: rect,
                    border: Border::default(),
                    shadow: Shadow::default(),
                    snap: true,
                },
                c,
            );
        };
        quad(renderer, bounds, th.background);

        let doc = self.doc;
        let t = doc.text.slice(..);
        let nlines = t.len_lines();
        let gcols = gutter_cols(doc);
        let gutter_w = gcols as f32 * cw;
        let text_x = bounds.x + gutter_w;
        let text_clip = Rectangle {
            x: text_x,
            width: bounds.width - gutter_w,
            ..bounds
        };
        let rows = (bounds.height / lh).ceil() as usize;
        let first = self.top.min(nlines.saturating_sub(1));
        let last = (self.top + rows).min(nlines);
        let ranges = doc.selection.ranges();
        let (cur_line, _) = doc.cursor_line_col();
        let tab = doc.tab_width;

        let byte_lo = t.line_to_byte(first);
        let byte_hi = if last >= nlines {
            t.len_bytes()
        } else {
            t.line_to_byte(last)
        };
        let spans = doc
            .syntax
            .as_ref()
            .map(|s| s.spans(&doc.text, byte_lo..byte_hi))
            .unwrap_or_default();
        let mut si = 0;

        let mk_text = |content: String, size: f32| text::Text {
            content,
            bounds: Size::new(f32::INFINITY, lh),
            size: Pixels(size),
            line_height: text::LineHeight::Absolute(Pixels(lh)),
            font: self.face.font,
            align_x: text::Alignment::Left,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Basic,
            wrapping: text::Wrapping::None,
        };

        for (row, line) in (first..last).enumerate() {
            let y = bounds.y + row as f32 * lh;
            let lstart = t.line_to_char(line);
            let lend = line_end(t, line);

            if line == cur_line && ranges.iter().all(|r| r.is_empty()) {
                quad(
                    renderer,
                    Rectangle {
                        x: bounds.x,
                        y,
                        width: bounds.width,
                        height: lh,
                    },
                    th.current_line,
                );
            }

            for m in self.marks.iter().filter(|m| m.from <= lend && m.to > lstart || m.from == m.to && m.from == lstart) {
                let rect = if m.rows {
                    Some(Rectangle { x: bounds.x, y, width: bounds.width, height: lh })
                } else {
                    let a = nib_editor::document::visual_col(t, m.from.max(lstart), tab);
                    // Past the end of the line: its newline, one cell.
                    let b = if m.to > lend { nib_editor::document::visual_col(t, lend, tab) + 1 } else { nib_editor::document::visual_col(t, m.to, tab) };
                    let x0 = text_x + (a as f32 - self.left as f32) * cw;
                    let x1 = text_x + (b as f32 - self.left as f32) * cw;
                    Rectangle { x: x0, y, width: x1 - x0, height: lh }.intersection(&text_clip)
                };
                if let Some(r) = rect {
                    quad(renderer, r, m.color);
                }
            }

            // Line number
            let num = format!("{:>w$}", line + 1, w = gcols - 2);
            let nc = if line == cur_line {
                th.gutter_current
            } else {
                th.gutter
            };
            renderer.fill_text(
                mk_text(num, self.face.size),
                Point::new(bounds.x, y),
                nc,
                bounds,
            );

            // Walk the line once: visual columns, selection runs, highlight runs.
            let mut col = 0usize;
            let mut byte = t.char_to_byte(lstart);
            let mut run = String::new();
            let mut run_col = 0usize;
            let mut run_color = th.foreground;
            let mut sel_start: Option<usize> = None;
            let flush =
                |renderer: &mut Renderer, run: &mut String, run_col: usize, color: Color| {
                    if !run.is_empty()
                        && !run.trim().is_empty()
                        && run_col + run.chars().count() > self.left
                    {
                        let x = text_x + (run_col as f32 - self.left as f32) * cw;
                        renderer.fill_text(
                            mk_text(std::mem::take(run), self.face.size),
                            Point::new(x, y),
                            color,
                            text_clip,
                        );
                    }
                    run.clear();
                };
            for (i, ch) in t.slice(lstart..lend).chars().enumerate() {
                let pos = lstart + i;
                while si < spans.len() && spans[si].bytes.end <= byte {
                    si += 1;
                }
                let color = match spans.get(si) {
                    Some(s) if s.bytes.start <= byte => s
                        .scope
                        .map(|h| th.scope(nib_syntax::scope_name(h)))
                        .unwrap_or(th.foreground),
                    _ => th.foreground,
                };
                let selected = ranges.iter().any(|r| r.contains(pos));
                match (selected, sel_start) {
                    (true, None) => sel_start = Some(col),
                    (false, Some(s)) => {
                        self.sel_quad(renderer, &quad, text_x, y, s, col, cw, lh, text_clip);
                        sel_start = None;
                    }
                    _ => {}
                }
                let w = if ch == '\t' {
                    tab - col % tab
                } else {
                    char_width(ch)
                };
                if color != run_color {
                    flush(renderer, &mut run, run_col, run_color);
                    run_col = col;
                    run_color = color;
                }
                if run.is_empty() {
                    run_col = col;
                }
                if ch == '\t' {
                    run.extend(std::iter::repeat_n(' ', w));
                } else if !ch.is_control() {
                    run.push(ch);
                }
                col += w;
                byte += ch.len_utf8();
            }
            // Selected newline shows as one extra cell.
            let nl_selected = lend < t.len_chars() && ranges.iter().any(|r| r.contains(lend));
            match (sel_start, nl_selected) {
                (Some(s), true) => {
                    self.sel_quad(renderer, &quad, text_x, y, s, col + 1, cw, lh, text_clip)
                }
                (Some(s), false) => {
                    self.sel_quad(renderer, &quad, text_x, y, s, col, cw, lh, text_clip)
                }
                (None, true) => {
                    self.sel_quad(renderer, &quad, text_x, y, col, col + 1, cw, lh, text_clip)
                }
                _ => {}
            }
            flush(renderer, &mut run, run_col, run_color);

            // Git blame: past the line's end (and its selected newline), a few cells apart.
            if let Some((_, label)) = self.blame.as_ref().filter(|(l, _)| *l == line) {
                let at = col + BLAME_GAP;
                if at >= self.left {
                    let x = text_x + (at - self.left) as f32 * cw;
                    renderer.fill_text(mk_text(label.clone(), self.face.size), Point::new(x, y), th.gutter, text_clip);
                } else if at + label.chars().count() > self.left {
                    let shown: String = label.chars().skip(self.left - at).collect();
                    renderer.fill_text(mk_text(shown, self.face.size), Point::new(text_x, y), th.gutter, text_clip);
                }
            }
        }

        // Cursors: thin bars, no blinking (blinking forces a repaint twice a second, bad on VDI).
        let lo = t.line_to_char(first);
        let hi = if last >= nlines { t.len_chars() } else { t.line_to_char(last) };
        for r in ranges.iter().filter(|r| (lo..=hi).contains(&r.head)) {
            let line = t.char_to_line(r.head);
            let col = nib_editor::document::visual_col(t, r.head, tab);
            if line < first || line >= last || col < self.left {
                continue;
            }
            let x = text_x + (col - self.left) as f32 * cw;
            let y = bounds.y + (line - first) as f32 * lh;
            let (color, w) = self.tag.as_ref().map_or((th.cursor, 2.0), |(_, c)| (*c, 2.5));
            quad(
                renderer,
                Rectangle {
                    x: x - w / 2.0,
                    y,
                    width: w,
                    height: lh,
                },
                color,
            );
            // The flag with the driver's name: above the cursor, below it on the top row.
            if let Some((name, c)) = &self.tag {
                let size = (self.face.size * 0.8).max(9.0);
                let fh = lh * 0.85;
                let fw = name.chars().count() as f32 * cw * 0.8 + 8.0;
                let fy = if line > first { y - fh } else { y + lh };
                let flag = Rectangle { x: x - 1.25, y: fy, width: fw, height: fh };
                renderer.fill_quad(
                    Quad {
                        bounds: flag,
                        border: Border { radius: 3.0.into(), ..Border::default() },
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    *c,
                );
                renderer.fill_text(
                    text::Text {
                        content: name.clone(),
                        bounds: Size::new(f32::INFINITY, fh),
                        size: Pixels(size),
                        line_height: text::LineHeight::Absolute(Pixels(fh)),
                        font: self.face.font,
                        align_x: text::Alignment::Left,
                        align_y: alignment::Vertical::Top,
                        shaping: text::Shaping::Basic,
                        wrapping: text::Wrapping::None,
                    },
                    Point::new(x + 3.0, fy),
                    th.background,
                    bounds,
                );
            }
        }
        crate::timing::record_draw(t0.elapsed());
    }
}

impl<Message> EditorView<'_, Message> {
    #[allow(clippy::too_many_arguments)]
    fn sel_quad<R: iced::advanced::Renderer>(
        &self,
        renderer: &mut R,
        quad: &impl Fn(&mut R, Rectangle, Color),
        text_x: f32,
        y: f32,
        c0: usize,
        c1: usize,
        cw: f32,
        lh: f32,
        clip: Rectangle,
    ) {
        let x0 = text_x + (c0 as f32 - self.left as f32) * cw;
        let x1 = text_x + (c1 as f32 - self.left as f32) * cw;
        if let Some(r) = (Rectangle {
            x: x0,
            y,
            width: x1 - x0,
            height: lh,
        })
        .intersection(&clip)
        {
            quad(renderer, r, self.theme.selection);
        }
    }
}

impl<'a, Message: 'a, Theme, Renderer> From<EditorView<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Renderer: text::Renderer<Font = Font> + 'a,
{
    fn from(w: EditorView<'a, Message>) -> Self {
        Element::new(w)
    }
}
