//! The CSV table: a custom widget drawing a `Table` on the code font's monospace grid, with a
//! fixed header row and row-number gutter. Like `EditorView` it is stateless about the data and
//! only draws the visible rows; the App owns scroll position and sort order.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::text::{self, Paragraph as _};
use iced::advanced::widget::{tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::alignment;
use iced::{mouse, Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Shadow, Size};

use crate::editor_view::Typeface;
use crate::table::Table;
use crate::theme::Theme;

#[derive(Debug, Clone, Copy)]
pub enum TableEvent {
    /// Positive = down / right, in rows and cells.
    Scroll { lines: f32, cols: f32 },
    /// A header was clicked.
    Sort(usize),
    /// Visible body rows.
    Resized { rows: usize },
}

/// Cells of padding around a value (one each side) plus the separator.
const GAP: usize = 3;

pub struct TableView<'a, Message> {
    table: &'a Table,
    theme: &'a Theme,
    top: usize,
    left: usize,
    face: Typeface,
    on_event: Box<dyn Fn(TableEvent) -> Message + 'a>,
}

impl<'a, Message> TableView<'a, Message> {
    pub fn new(
        table: &'a Table,
        theme: &'a Theme,
        top: usize,
        left: usize,
        face: Typeface,
        on_event: impl Fn(TableEvent) -> Message + 'a,
    ) -> Self {
        Self { table, theme, top, left, face, on_event: Box::new(on_event) }
    }

    fn gutter_cols(&self) -> usize {
        self.table.len().to_string().len() + 2
    }

    /// Column whose cells cover horizontal cell offset `x` (in table coordinates).
    fn column_at(&self, x: usize) -> Option<usize> {
        let mut start = 0;
        for (c, w) in self.table.widths.iter().enumerate() {
            start += w + GAP;
            if x < start {
                return Some(c);
            }
        }
        None
    }
}

/// Total width of the table in cells.
pub fn total_cols(table: &Table) -> usize {
    table.widths.iter().map(|w| w + GAP).sum()
}

#[derive(Default)]
struct State {
    /// Wheel movement not yet a whole line / column (`zoom::whole_lines`).
    wheel: (f32, f32),
    char_w: f32,
    line_h: f32,
    measured_for: Option<Typeface>,
    last_rows: usize,
}

/// `s` cut to `width` cells, with an ellipsis if anything was dropped.
fn fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for TableView<'_, Message>
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
            let probe = "M".repeat(100);
            let p = Renderer::Paragraph::with_text(text::Text {
                content: probe.as_str(),
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
        // One line is the header.
        let rows = ((bounds.height / st.line_h).floor() as usize).saturating_sub(1).max(1);
        if st.last_rows != rows {
            st.last_rows = rows;
            shell.publish((self.on_event)(TableEvent::Resized { rows }));
        }
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(p) = cursor.position_over(bounds) else { return };
                let body_x = bounds.x + self.gutter_cols() as f32 * st.char_w;
                if p.y - bounds.y < st.line_h && p.x >= body_x {
                    let cell = self.left + ((p.x - body_x) / st.char_w) as usize;
                    if let Some(c) = self.column_at(cell) {
                        shell.publish((self.on_event)(TableEvent::Sort(c)));
                    }
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(bounds) => {
                let (lines, cols) = match *delta {
                    mouse::ScrollDelta::Lines { x, y } => (-y * 3.0, -x * 3.0),
                    mouse::ScrollDelta::Pixels { x, y } => (-y / st.line_h, -x / st.char_w),
                };
                let (lines, cols) = (crate::zoom::whole_lines(&mut st.wheel.0, lines), crate::zoom::whole_lines(&mut st.wheel.1, cols));
                if lines == 0.0 && cols == 0.0 {
                    shell.capture_event();
                    return;
                }
                shell.publish((self.on_event)(TableEvent::Scroll { lines, cols }));
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _v: &Rectangle,
        _r: &Renderer,
    ) -> mouse::Interaction {
        let st = tree.state.downcast_ref::<State>();
        let b = layout.bounds();
        match cursor.position_over(b) {
            Some(p) if p.y - b.y < st.line_h => mouse::Interaction::Pointer,
            _ => mouse::Interaction::default(),
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

        let table = self.table;
        let size = self.face.size;
        let mk_text = |content: String| text::Text {
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

        let gcols = self.gutter_cols();
        let body_x = bounds.x + gcols as f32 * cw;
        let body = Rectangle { x: body_x, width: (bounds.width - gcols as f32 * cw).max(0.0), ..bounds };
        let rows = ((bounds.height / lh).ceil() as usize).saturating_sub(1);
        let first = self.top.min(table.len().saturating_sub(1));
        let last = (first + rows).min(table.len());

        // Header band.
        quad(renderer, Rectangle { height: lh, ..bounds }, th.panel);

        // Columns: x in cells relative to the body's left edge, skipping those scrolled out.
        let mut x_cells = 0isize - self.left as isize;
        let visible_cells = (body.width / cw).ceil() as isize;
        for (c, &w) in table.widths.iter().enumerate() {
            let col_cells = (w + GAP) as isize;
            if x_cells + col_cells > 0 && x_cells < visible_cells {
                let x = body_x + (x_cells as f32 + 1.0) * cw;
                let right_align = table.numeric[c];
                let place = |s: &str| {
                    let n = s.chars().count();
                    if right_align {
                        x + (w.saturating_sub(n)) as f32 * cw
                    } else {
                        x
                    }
                };

                let arrow = match table.sort {
                    Some(s) if s.col == c => {
                        if s.descending {
                            " ▼"
                        } else {
                            " ▲"
                        }
                    }
                    _ => "",
                };
                let sorted = !arrow.is_empty();
                let head = format!("{}{arrow}", fit(&table.header[c], w.saturating_sub(arrow.chars().count())));
                let hc = if sorted { th.accent } else { th.foreground };
                renderer.fill_text(mk_text(head.clone()), Point::new(place(&head), bounds.y), hc, body);

                for (row, i) in (first..last).enumerate() {
                    let Some((_, cells)) = table.row(i) else { break };
                    let Some(cell) = cells.get(c).filter(|s| !s.is_empty()) else { continue };
                    let s = fit(cell, w);
                    let y = bounds.y + (row + 1) as f32 * lh;
                    let color = if right_align { th.scope("constant.numeric") } else { th.foreground };
                    renderer.fill_text(mk_text(s.clone()), Point::new(place(&s), y), color, body);
                }

                // Separator after the column.
                let sx = body_x + (x_cells + col_cells) as f32 * cw - cw / 2.0;
                if let Some(r) = (Rectangle { x: sx, y: bounds.y, width: 1.0, height: bounds.height }).intersection(&body) {
                    quad(renderer, r, th.divider);
                }
            }
            x_cells += col_cells;
        }

        // Row numbers (the row's position in the file, so a sort doesn't hide where it came from).
        for (row, i) in (first..last).enumerate() {
            let Some((n, _)) = table.row(i) else { break };
            let y = bounds.y + (row + 1) as f32 * lh;
            let num = format!("{:>w$}", n, w = gcols - 1);
            renderer.fill_text(mk_text(num), Point::new(bounds.x, y), th.gutter, bounds);
        }
        quad(renderer, Rectangle { y: bounds.y + lh - 1.0, height: 1.0, ..bounds }, th.divider);
    }
}

impl<'a, Message: 'a, Theme, Renderer> From<TableView<'a, Message>> for Element<'a, Message, Theme, Renderer>
where
    Renderer: text::Renderer<Font = Font> + 'a,
{
    fn from(w: TableView<'a, Message>) -> Self {
        Element::new(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_with_ellipsis() {
        assert_eq!(fit("abc", 3), "abc");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("añb", 3), "añb");
    }
}
