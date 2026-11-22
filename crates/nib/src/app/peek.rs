//! A picture in the AI chat that the mouse rests on (`[ai] image_zoom_delay`, a second by
//! default) is shown bigger, over everything:
//! four times its size in the chat each way (less if the window is too small, same shape). Any
//! move of the mouse puts it away. Its state lives in the widget tree, so the app holds nothing
//! for it, and the bigger picture is only drawn (from the chat's own handle) while it is up.

use std::time::Duration;

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{self, tree, Tree, Widget};
use iced::advanced::{image as adv_image, overlay, renderer, svg as adv_svg, Clipboard, Shell};
use iced::time::Instant;
use iced::{mouse, window, Border, Color, Element, Event, Length, Rectangle, Shadow, Size, Theme, Vector};

use super::viewers::PreviewImage;

/// How many times bigger, each way.
const SCALE: f32 = 4.0;
/// The space kept clear between the bigger picture and the window's edges.
const MARGIN: f32 = 12.0;

/// `content` (the picture as drawn in the chat) that shows `picture` bigger when rested on for
/// `delay`.
pub(super) fn peek<'a, Message: 'a>(content: impl Into<Element<'a, Message>>, picture: &PreviewImage, delay: Duration) -> Element<'a, Message> {
    Element::new(Peek { content: content.into(), picture: picture.clone(), delay })
}

struct Peek<'a, Message> {
    content: Element<'a, Message>,
    /// A handle: shares the chat's decoded pixels.
    picture: PreviewImage,
    /// How long the mouse rests on the picture before it is shown bigger.
    delay: Duration,
}

#[derive(Clone, Copy, Default)]
enum State {
    #[default]
    Idle,
    /// The mouse is on the picture and hasn't moved since then.
    Resting(Instant),
    Open,
}

impl<'a, Message> Widget<Message, Theme, iced::Renderer> for Peek<'a, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        let over = cursor.is_over(layout.bounds());
        match event {
            Event::Mouse(_) => {
                if matches!(state, State::Open) {
                    shell.invalidate_layout();
                    shell.request_redraw();
                }
                *state = if over {
                    let now = Instant::now();
                    shell.request_redraw_at(now + self.delay);
                    State::Resting(now)
                } else {
                    State::Idle
                };
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                if let State::Resting(since) = *state {
                    if !over {
                        *state = State::Idle;
                    } else if now.duration_since(since) >= self.delay {
                        *state = State::Open;
                        shell.invalidate_layout();
                        shell.request_redraw();
                    } else {
                        shell.request_redraw_at(since + self.delay);
                    }
                }
            }
            _ => {}
        }
        self.content.as_widget_mut().update(&mut tree.children[0], event, layout, cursor, renderer, clipboard, shell, viewport);
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &iced::Renderer, operation: &mut dyn widget::Operation) {
        self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        if matches!(tree.state.downcast_ref::<State>(), State::Open) {
            let shown = layout.bounds() + translation;
            return Some(overlay::Element::new(Box::new(Big { shown, picture: &self.picture })));
        }
        self.content.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}

/// The bigger picture, centred on the one in the chat and kept inside the window.
struct Big<'b> {
    /// Where the picture is drawn in the chat, in window coordinates.
    shown: Rectangle,
    picture: &'b PreviewImage,
}

/// `SCALE` times `shown`, shrunk (same shape) to fit `window` less its margins, centred on
/// `shown` and moved inside the window.
fn place(shown: Rectangle, window: Size) -> Rectangle {
    let room = Size::new((window.width - 2.0 * MARGIN).max(1.0), (window.height - 2.0 * MARGIN).max(1.0));
    let (w, h) = (shown.width.max(1.0) * SCALE, shown.height.max(1.0) * SCALE);
    let fit = (room.width / w).min(room.height / h).min(1.0);
    let (w, h) = (w * fit, h * fit);
    let center = shown.center();
    let x = (center.x - w / 2.0).clamp(MARGIN, (window.width - MARGIN - w).max(MARGIN));
    let y = (center.y - h / 2.0).clamp(MARGIN, (window.height - MARGIN - h).max(MARGIN));
    Rectangle { x, y, width: w, height: h }
}

impl<Message> overlay::Overlay<Message, Theme, iced::Renderer> for Big<'_> {
    fn layout(&mut self, _renderer: &iced::Renderer, bounds: Size) -> layout::Node {
        let r = place(self.shown, bounds);
        layout::Node::new(r.size()).move_to(r.position())
    }

    // `mouse_interaction` stays `None`, so the picture underneath keeps the mouse and sees it move.

    fn draw(&self, renderer: &mut iced::Renderer, theme: &Theme, _style: &renderer::Style, layout: Layout<'_>, _cursor: mouse::Cursor) {
        use iced::advanced::Renderer as _;
        let bounds = layout.bounds();
        let edge = theme.extended_palette().background.strong.color;
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: Border { color: edge, width: 1.0, radius: 4.0.into() },
                shadow: Shadow { color: Color::from_rgba(0.0, 0.0, 0.0, 0.5), offset: Vector::new(0.0, 4.0), blur_radius: 16.0 },
                ..renderer::Quad::default()
            },
            theme.extended_palette().background.base.color,
        );
        match self.picture {
            PreviewImage::Raster(h) => adv_image::Renderer::draw_image(renderer, adv_image::Image::new(h.clone()), bounds, bounds),
            PreviewImage::Svg(h) => adv_svg::Renderer::draw_svg(renderer, adv_svg::Svg::new(h.clone()), bounds, bounds),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::place;
    use iced::{Rectangle, Size};

    #[test]
    fn four_times_or_what_fits() {
        let win = Size::new(2000.0, 1500.0);
        let r = place(Rectangle { x: 500.0, y: 500.0, width: 100.0, height: 50.0 }, win);
        assert_eq!((r.width, r.height), (400.0, 200.0));
        assert_eq!(r.center(), iced::Point::new(550.0, 525.0));
        // Too big for the window: shrunk, same shape, inside it.
        let r = place(Rectangle { x: 0.0, y: 0.0, width: 400.0, height: 300.0 }, Size::new(1000.0, 800.0));
        assert!((r.width / r.height - 4.0 / 3.0).abs() < 1e-4);
        assert!(r.x >= 12.0 && r.y >= 12.0 && r.x + r.width <= 988.0 + 1e-3 && r.y + r.height <= 788.0 + 1e-3);
    }
}
