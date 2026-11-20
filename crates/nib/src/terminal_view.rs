//! Terminal pane: renders a `vt100` screen on the same monospace grid as the editor.

use crate::theme::{xterm, Theme};
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::text::{self, Paragraph as _};
use iced::advanced::widget::{tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::alignment;
use iced::{keyboard, mouse, Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Shadow, Size};

#[derive(Debug, Clone)]
pub enum TermEvent {
    Resized { rows: u16, cols: u16 },
    Focus,
    Scroll(i32),
    /// Cmd-click (Ctrl-click off macOS) on a URL: open it on this machine.
    OpenUrl(String),
}

pub struct TerminalView<'a, Message> {
    screen: &'a vt100::Screen,
    theme: &'a Theme,
    font: Font,
    font_size: f32,
    focused: bool,
    on_event: Box<dyn Fn(TermEvent) -> Message + 'a>,
}

impl<'a, Message> TerminalView<'a, Message> {
    pub fn new(
        screen: &'a vt100::Screen,
        theme: &'a Theme,
        font: Font,
        font_size: f32,
        focused: bool,
        on_event: impl Fn(TermEvent) -> Message + 'a,
    ) -> Self {
        Self { screen, theme, font, font_size, focused, on_event: Box::new(on_event) }
    }
}

#[derive(Default)]
struct State {
    cw: f32,
    lh: f32,
    measured_for: Option<(Font, f32)>,
    last: (u16, u16),
    mods: keyboard::Modifiers,
    /// The cell under the mouse, if it is over the grid.
    hover: Option<(u16, u16)>,
}

/// A URL on the screen and the cells it covers: `(row, first col, end col)` per row it spans.
pub struct Link {
    pub url: String,
    pub spans: Vec<(u16, u16, u16)>,
}

/// The URL at `(row, col)`, joining rows the terminal wrapped (or that a program filled to the
/// last column and continued on the next row), so a long link wrapped over lines is one link.
pub fn link_at(screen: &vt100::Screen, row: u16, col: u16) -> Option<Link> {
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows.get(), cols.get());
    if row >= rows || col >= cols {
        return None;
    }
    let filled = |r: u16, c: u16| screen.cell(r, c).is_some_and(|x| !x.contents().trim().is_empty());
    let joined = |r: u16| r + 1 < rows && (screen.row_wrapped(r) || (filled(r, cols - 1) && filled(r + 1, 0)));
    const MAX_ROWS: u16 = 64;
    let mut first = row;
    while first > 0 && row - first < MAX_ROWS && joined(first - 1) {
        first -= 1;
    }
    let mut last = row;
    while last - row < MAX_ROWS && joined(last) {
        last += 1;
    }
    let mut text = String::new();
    // The byte offset in `text` where each cell starts, with its position.
    let mut at: Vec<(usize, u16, u16)> = Vec::new();
    for r in first..=last {
        for c in 0..cols {
            let Some(cell) = screen.cell(r, c) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            at.push((text.len(), r, c));
            match cell.contents() {
                "" => text.push(' '),
                s => text.push_str(s),
            }
        }
        if r < last && !screen.row_wrapped(r) && !filled(r, cols - 1) {
            text.push('\n');
        }
    }
    let i = at.iter().position(|&(_, r, c)| r == row && c == col)?;
    let byte = at[i].0;
    let range = crate::links::find_urls(&text).into_iter().find(|u| u.contains(&byte))?;
    let mut spans: Vec<(u16, u16, u16)> = Vec::new();
    for &(_, r, c) in at.iter().filter(|(b, _, _)| range.contains(b)) {
        match spans.last_mut() {
            Some(s) if s.0 == r => s.2 = c + 1,
            _ => spans.push((r, c, c + 1)),
        }
    }
    Some(Link { url: text[range].to_string(), spans })
}

const PAD: f32 = 6.0;

fn color(c: vt100::Color, default: Color) -> Color {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => xterm(i),
        vt100::Color::Rgb(r, g, b) => Color::from_rgb8(r, g, b),
    }
}

impl<Message, Th, Renderer> Widget<Message, Th, Renderer> for TerminalView<'_, Message>
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

    fn layout(&mut self, tree: &mut Tree, _r: &Renderer, limits: &layout::Limits) -> layout::Node {
        let st = tree.state.downcast_mut::<State>();
        if st.measured_for != Some((self.font, self.font_size)) {
            let probe = "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM";
            let p = Renderer::Paragraph::with_text(text::Text {
                content: probe,
                bounds: Size::INFINITE,
                size: Pixels(self.font_size),
                line_height: text::LineHeight::Relative(1.25),
                font: self.font,
                align_x: text::Alignment::Left,
                align_y: alignment::Vertical::Top,
                shaping: text::Shaping::Basic,
                wrapping: text::Wrapping::None,
            });
            let b = p.min_bounds();
            st.cw = b.width / probe.len() as f32;
            st.lh = b.height;
            st.measured_for = Some((self.font, self.font_size));
        }
        layout::Node::new(limits.max())
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _r: &Renderer,
        _c: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _v: &Rectangle,
    ) {
        let st = tree.state.downcast_mut::<State>();
        if st.cw <= 0.0 {
            return;
        }
        let b = layout.bounds();
        let rows = ((b.height - 2.0 * PAD) / st.lh).floor().max(1.0) as u16;
        let cols = ((b.width - 2.0 * PAD) / st.cw).floor().max(1.0) as u16;
        if st.last != (rows, cols) {
            st.last = (rows, cols);
            shell.publish((self.on_event)(TermEvent::Resized { rows, cols }));
        }
        let hover = cursor.position_over(b).and_then(|p| {
            let (c, r) = ((p.x - b.x - PAD) / st.cw, (p.y - b.y - PAD) / st.lh);
            (c >= 0.0 && r >= 0.0 && r < rows as f32 && c < cols as f32).then_some((r as u16, c as u16))
        });
        if let Event::Keyboard(keyboard::Event::ModifiersChanged(m)) = event {
            if st.mods.command() != m.command() {
                shell.request_redraw();
            }
            st.mods = *m;
        }
        if hover != st.hover {
            st.hover = hover;
            if st.mods.command() {
                shell.request_redraw();
            }
        }
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(button)) if cursor.is_over(b) => {
                let link = (*button == mouse::Button::Left && st.mods.command())
                    .then(|| hover.and_then(|(r, c)| link_at(self.screen, r, c)))
                    .flatten();
                match link {
                    Some(l) => shell.publish((self.on_event)(TermEvent::OpenUrl(l.url))),
                    None => shell.publish((self.on_event)(TermEvent::Focus)),
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(b) => {
                let lines = match *delta {
                    mouse::ScrollDelta::Lines { y, .. } => (y * 3.0) as i32,
                    mouse::ScrollDelta::Pixels { y, .. } => (y / st.lh) as i32,
                };
                if lines != 0 {
                    shell.publish((self.on_event)(TermEvent::Scroll(lines)));
                }
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn mouse_interaction(&self, t: &Tree, l: Layout<'_>, c: mouse::Cursor, _v: &Rectangle, _r: &Renderer) -> mouse::Interaction {
        let st = t.state.downcast_ref::<State>();
        if c.is_over(l.bounds()) && st.mods.command() && st.hover.is_some_and(|(r, c)| link_at(self.screen, r, c).is_some()) {
            mouse::Interaction::Pointer
        } else if c.is_over(l.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(&self, tree: &Tree, renderer: &mut Renderer, _t: &Th, _s: &renderer::Style, layout: Layout<'_>, _c: mouse::Cursor, _v: &Rectangle) {
        let st = tree.state.downcast_ref::<State>();
        let b = layout.bounds();
        let th = self.theme;
        let quad = |r: &mut Renderer, rect: Rectangle, c: Color| {
            r.fill_quad(Quad { bounds: rect, border: Border::default(), shadow: Shadow::default(), snap: true }, c);
        };
        quad(renderer, b, th.terminal_bg);
        if st.cw <= 0.0 {
            return;
        }
        let (cw, lh) = (st.cw, st.lh);
        let (rows, cols) = self.screen.size();
        let (rows, cols) = (rows.get(), cols.get());
        let ox = b.x + PAD;
        let oy = b.y + PAD;
        let mk = |content: String| text::Text {
            content,
            bounds: Size::new(f32::INFINITY, lh),
            size: Pixels(self.font_size),
            line_height: text::LineHeight::Absolute(Pixels(lh)),
            font: self.font,
            align_x: text::Alignment::Left,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Basic,
            wrapping: text::Wrapping::None,
        };
        for r in 0..rows {
            let y = oy + r as f32 * lh;
            if y + lh > b.y + b.height {
                break;
            }
            let mut run = String::new();
            let mut run_start = 0u16;
            let mut run_fg = th.terminal_fg;
            let flush = |renderer: &mut Renderer, run: &mut String, start: u16, fg: Color| {
                if !run.trim().is_empty() {
                    renderer.fill_text(mk(std::mem::take(run)), Point::new(ox + start as f32 * cw, y), fg, b);
                }
                run.clear();
            };
            for c in 0..cols {
                let Some(cell) = self.screen.cell(r, c) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let (mut fg, mut bg) = (color(cell.fgcolor(), th.terminal_fg), color(cell.bgcolor(), th.terminal_bg));
                if cell.inverse() {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if bg != th.terminal_bg {
                    let w = if cell.is_wide() { 2.0 } else { 1.0 };
                    quad(renderer, Rectangle { x: ox + c as f32 * cw, y, width: cw * w, height: lh }, bg);
                }
                if fg != run_fg {
                    flush(renderer, &mut run, run_start, run_fg);
                    run_fg = fg;
                }
                if run.is_empty() {
                    run_start = c;
                }
                let s = cell.contents();
                if s.is_empty() {
                    run.push(' ');
                } else {
                    run.push_str(s);
                }
            }
            flush(renderer, &mut run, run_start, run_fg);
        }
        if let Some(link) = st.hover.filter(|_| st.mods.command()).and_then(|(r, c)| link_at(self.screen, r, c)) {
            for (r, c0, c1) in link.spans {
                let y = oy + (r as f32 + 1.0) * lh - 2.0;
                quad(renderer, Rectangle { x: ox + c0 as f32 * cw, y, width: (c1 - c0) as f32 * cw, height: 1.0 }, th.terminal_fg);
            }
        }
        if !self.screen.hide_cursor() && self.screen.scrollback() == 0 {
            let (r, c) = self.screen.cursor_position();
            let rect = Rectangle { x: ox + c as f32 * cw, y: oy + r as f32 * lh, width: cw, height: lh };
            if self.focused {
                quad(renderer, rect, Color { a: 0.7, ..th.cursor });
            } else {
                renderer.fill_quad(
                    Quad { bounds: rect, border: Border { color: th.cursor, width: 1.0, radius: 0.0.into() }, shadow: Shadow::default(), snap: true },
                    Color::TRANSPARENT,
                );
            }
        }
    }
}

impl<'a, Message: 'a, Th, Renderer> From<TerminalView<'a, Message>> for Element<'a, Message, Th, Renderer>
where
    Renderer: text::Renderer<Font = Font> + 'a,
{
    fn from(w: TerminalView<'a, Message>) -> Self {
        Element::new(w)
    }
}

/// Bytes a key press should send to the PTY. `None` = not for the terminal.
pub fn key_bytes(chord: &nib_editor::KeyChord, text: Option<&str>, app_cursor: bool) -> Option<Vec<u8>> {
    use nib_editor::keymap::Key;
    let csi = |s: &str| -> Vec<u8> {
        let mut v = if app_cursor { b"\x1bO".to_vec() } else { b"\x1b[".to_vec() };
        v.extend_from_slice(s.as_bytes());
        v
    };
    let mut out = match chord.key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => if chord.ctrl || chord.alt { b"\x17".to_vec() } else { b"\x7f".to_vec() },
        Key::Tab => if chord.shift { b"\x1b[Z".to_vec() } else { b"\t".to_vec() },
        Key::Esc => b"\x1b".to_vec(),
        Key::Up => csi("A"),
        Key::Down => csi("B"),
        Key::Right => csi("C"),
        Key::Left => csi("D"),
        Key::Home => csi("H"),
        Key::End => csi("F"),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::Insert => b"\x1b[2~".to_vec(),
        Key::F(n @ 1..=4) => format!("\x1bO{}", (b'P' + n - 1) as char).into_bytes(),
        Key::F(n) => {
            let code = match n { 5 => 15, 6 => 17, 7 => 18, 8 => 19, 9 => 20, 10 => 21, 11 => 23, 12 => 24, _ => return None };
            format!("\x1b[{code}~").into_bytes()
        }
        Key::Char(c) if chord.ctrl && !chord.alt => {
            let b = match c {
                'a'..='z' => c as u8 - b'a' + 1,
                ' ' | '2' | '@' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '6' => 0x1e,
                '/' | '7' | '-' => 0x1f,
                _ => return None,
            };
            vec![b]
        }
        Key::Char(_) => text.filter(|t| !t.is_empty())?.as_bytes().to_vec(),
    };
    // Alt as Meta (ESC prefix) for word movement in shells; not for text already composed by Option.
    if chord.alt && matches!(chord.key, Key::Left | Key::Right | Key::Backspace) {
        out = match chord.key {
            Key::Left => b"\x1bb".to_vec(),
            Key::Right => b"\x1bf".to_vec(),
            _ => b"\x1b\x7f".to_vec(),
        };
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::link_at;

    #[test]
    fn a_url_wrapped_over_rows_is_one_link() {
        let mut p = vt100::Parser::new(6.try_into().unwrap(), 20.try_into().unwrap(), 0);
        let url = "https://claude.com/cai/oauth/authorize?code=true&state=XYZ";
        p.process(format!("visit: {url}\r\nPaste code > ").as_bytes());
        let s = p.screen();
        // Clicked on the third row of the URL.
        let l = link_at(s, 2, 3).expect("a link");
        assert_eq!(l.url, url);
        assert_eq!(l.spans.first(), Some(&(0, 7, 20)));
        assert_eq!(link_at(s, 0, 0).map(|l| l.url), None);
        // A program that printed the URL hard-wrapped to the width, row by row.
        let mut p = vt100::Parser::new(6.try_into().unwrap(), 20.try_into().unwrap(), 0);
        p.process(format!("{}\r\n{}\r\n", &url[..20], &url[20..]).as_bytes());
        assert_eq!(link_at(p.screen(), 1, 0).map(|l| l.url), Some(url.to_string()));
    }
}
