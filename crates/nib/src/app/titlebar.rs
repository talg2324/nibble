//! nib's own title bar and resize edges, where nib draws the window frame itself: on WSL.
//!
//! There winit would draw the frame client-side (sctk-adwaita: Wayland subsurfaces whose shared
//! memory pool it grows as it redraws them on resize and on focus changes). WSLg's compositor
//! (weston 9) keeps pointers into a client's pool across such a resize, and reads freed memory
//! the next time it copies the window to Windows: it crashes, and takes every Linux window on the
//! machine with it. So on WSL the window opens undecorated (main.rs) and this draws the frame:
//! the title (drag it to move the window, double-click to maximize), minimize / maximize / close,
//! a 1 px border, and invisible strips along the edges that resize the window.
//! `NIB_FRAME=system` brings winit's frame back.

use super::{App, Message};
use iced::widget::{button, column, container, mouse_area, row, space, stack, svg, text};
use iced::window::{self, Direction};
use iced::{mouse, Color, Element, Length, Point, Task};
use std::sync::OnceLock;

/// Height of the title bar.
const HEIGHT: f32 = 30.0;
/// Width of a window button (the glyphs in `icons.rs` are drawn for this size).
const BUTTON_W: f32 = 46.0;
/// How far in from the window's edge a press resizes; corners reach further along each side.
const EDGE: f32 = 5.0;
const CORNER: f32 = 14.0;
/// How far the mouse moves with the button down on the title before the window follows it
/// (less is a click, or half of a double-click).
const DRAG_SLOP: f32 = 3.0;

/// nib draws its own window frame (see the module doc).
pub fn own_frame() -> bool {
    static OWN: OnceLock<bool> = OnceLock::new();
    *OWN.get_or_init(|| {
        cfg!(target_os = "linux")
            && std::path::Path::new("/dev/dxg").exists()
            && std::env::var("NIB_FRAME").as_deref() != Ok("system")
    })
}

#[derive(Default)]
pub struct State {
    /// Where the mouse is over the title.
    hover: Point,
    /// Where the left button went down on the title, until it is released or a drag starts.
    pressed: Option<Point>,
    maximized: bool,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Press,
    Move(Point),
    Release,
    DoubleClick,
    Minimize,
    ToggleMaximize,
    Close,
    /// A press on an edge: the compositor resizes the window from there.
    Resize(Direction),
    /// The window changed size: ask whether it is maximized now.
    Resized(window::Id),
    Maximized(bool),
}

fn tb(m: Msg) -> Message {
    Message::TitleBar(m)
}

impl App {
    pub(super) fn on_titlebar(&mut self, m: Msg) -> Task<Message> {
        let id = self.settings.window.id;
        let on_window = |f: fn(window::Id) -> Task<Message>| id.map_or_else(Task::none, f);
        match m {
            Msg::Press => self.titlebar.pressed = Some(self.titlebar.hover),
            Msg::Move(p) => {
                self.titlebar.hover = p;
                if self.titlebar.pressed.is_some_and(|at| at.distance(p) > DRAG_SLOP) {
                    // The compositor moves the window from here on; the release goes to it.
                    self.titlebar.pressed = None;
                    return on_window(window::drag);
                }
            }
            Msg::Release => self.titlebar.pressed = None,
            Msg::DoubleClick | Msg::ToggleMaximize => {
                self.titlebar.pressed = None;
                return on_window(window::toggle_maximize);
            }
            Msg::Minimize => return on_window(|id| window::minimize(id, true)),
            Msg::Close => return self.quit(),
            Msg::Resize(d) => return id.map_or_else(Task::none, |id| window::drag_resize(id, d)),
            Msg::Resized(id) => return window::is_maximized(id).map(|m| tb(Msg::Maximized(m))),
            Msg::Maximized(m) => self.titlebar.maximized = m,
        }
        Task::none()
    }

    /// The frame shows: nib draws its own and the window isn't fullscreen.
    fn framed(&self) -> bool {
        own_frame() && !self.settings.window.fullscreen
    }

    /// The bar along the top: the title, then minimize, maximize / restore and close.
    pub(super) fn titlebar(&self) -> Option<Element<'_, Message>> {
        if !self.framed() {
            return None;
        }
        let th = &self.theme;
        let title = mouse_area(
            container(text(self.title()).size(12).wrapping(text::Wrapping::None).color(th.dim))
                .padding([0, 12])
                .width(Length::Fill)
                .height(Length::Fill)
                .align_y(iced::Alignment::Center),
        )
        .on_press(tb(Msg::Press))
        .on_move(|p| tb(Msg::Move(p)))
        .on_release(tb(Msg::Release))
        .on_double_click(tb(Msg::DoubleClick));

        let (dim, fg, hover_bg) = (th.dim, th.foreground, super::settings::raise(th.background, 0.12));
        let close_bg = Color::from_rgb8(0xc4, 0x2b, 0x1c);
        let ctl = |icon: &svg::Handle, m: Msg, close: bool| {
            let glyph = svg(icon.clone()).width(BUTTON_W).height(HEIGHT).style(move |_, st| svg::Style {
                color: Some(match st {
                    svg::Status::Hovered if close => Color::WHITE,
                    svg::Status::Hovered => fg,
                    svg::Status::Idle => dim,
                }),
            });
            button(glyph).on_press(tb(m)).padding(0).style(move |_, st| button::Style {
                background: matches!(st, button::Status::Hovered | button::Status::Pressed)
                    .then(|| if close { close_bg } else { hover_bg }.into()),
                ..Default::default()
            })
        };
        let icons = &self.icons;
        let max = if self.titlebar.maximized { &icons.win_restore } else { &icons.win_maximize };
        Some(
            row![
                title,
                ctl(&icons.win_minimize, Msg::Minimize, false),
                ctl(max, Msg::ToggleMaximize, false),
                ctl(&icons.win_close, Msg::Close, true),
            ]
            .height(HEIGHT)
            .into(),
        )
    }

    /// `content` with a 1 px border, and strips along the window's edges that resize it (not
    /// while maximized). They lie over the content and take only the presses that land on them.
    pub(super) fn with_frame_edges<'a>(&'a self, content: Element<'a, Message>) -> Element<'a, Message> {
        if !self.framed() || self.titlebar.maximized {
            return content;
        }
        let border = self.theme.divider;
        let content = container(content).padding(1).style(move |_| container::Style {
            border: iced::Border { color: border, width: 1.0, radius: 0.0.into() },
            ..Default::default()
        });
        use mouse::Interaction::{ResizingDiagonallyDown as Nwse, ResizingDiagonallyUp as Nesw, ResizingHorizontally as Ew, ResizingVertically as Ns};
        use Direction::*;
        let grip = |w: Length, h: Length, d: Direction, cursor: mouse::Interaction| -> Element<'a, Message> {
            mouse_area(space().width(w).height(h)).interaction(cursor).on_press(tb(Msg::Resize(d))).into()
        };
        let (e, c, fill) = (Length::Fixed(EDGE), Length::Fixed(CORNER), Length::Fill);
        let edges = column![
            row![grip(c, e, NorthWest, Nwse), grip(fill, e, North, Ns), grip(c, e, NorthEast, Nesw)],
            row![grip(e, fill, West, Ew), space().width(fill).height(fill), grip(e, fill, East, Ew)].height(fill),
            row![grip(c, e, SouthWest, Nesw), grip(fill, e, South, Ns), grip(c, e, SouthEast, Nwse)],
        ];
        stack![content, edges].into()
    }
}
