//! A colour picker: saturation/brightness square over a hue bar, drawn with gradient quads (works on
//! both the wgpu and tiny-skia renderers). Click or drag either part; publishes the new HSV.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::gradient::Linear;
use iced::{mouse, Background, Border, Color, Element, Event, Gradient, Length, Point, Radians, Rectangle, Shadow, Size};
use std::f32::consts::{FRAC_PI_2, PI};

/// Hue in degrees [0, 360), saturation and value in percent [0, 100].
pub type Hsv = [f32; 3];

pub fn to_hsv(c: Color) -> Hsv {
    let (r, g, b) = (c.r, c.g, c.b);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let s = if max == 0.0 { 0.0 } else { d / max };
    [h, s * 100.0, max * 100.0]
}

pub fn from_hsv([h, s, v]: Hsv) -> Color {
    let (s, v) = (s / 100.0, v / 100.0);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h.rem_euclid(360.0) / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    // Round through u8 so the hex round-trips exactly.
    let q = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    Color::from_rgb8(q(r), q(g), q(b))
}

const SQUARE_H: f32 = 140.0;
const GAP: f32 = 10.0;
const HUE_H: f32 = 14.0;

pub struct ColorPicker<'a, Message> {
    hsv: Hsv,
    on_change: Box<dyn Fn(Hsv) -> Message + 'a>,
}

impl<'a, Message> ColorPicker<'a, Message> {
    pub fn new(hsv: Hsv, on_change: impl Fn(Hsv) -> Message + 'a) -> Self {
        Self { hsv, on_change: Box::new(on_change) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Square,
    Hue,
}

#[derive(Default)]
struct State {
    dragging: Option<Part>,
}

fn parts(b: Rectangle) -> (Rectangle, Rectangle) {
    let square = Rectangle { height: SQUARE_H, ..b };
    let hue = Rectangle { y: b.y + SQUARE_H + GAP, height: HUE_H, ..b };
    (square, hue)
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for ColorPicker<'_, Message>
where
    Renderer: renderer::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fixed(SQUARE_H + GAP + HUE_H))
    }

    fn layout(&mut self, _tree: &mut Tree, _r: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::Node::new(limits.resolve(Length::Fill, SQUARE_H + GAP + HUE_H, Size::ZERO))
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
        let (square, hue) = parts(layout.bounds());
        let pick = |part: Part, p: Point, hsv: Hsv| -> Hsv {
            match part {
                Part::Square => {
                    let s = ((p.x - square.x) / square.width).clamp(0.0, 1.0) * 100.0;
                    let v = (1.0 - (p.y - square.y) / square.height).clamp(0.0, 1.0) * 100.0;
                    [hsv[0], s, v]
                }
                Part::Hue => [((p.x - hue.x) / hue.width).clamp(0.0, 0.9999) * 360.0, hsv[1], hsv[2]],
            }
        };
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(p) = cursor.position() else { return };
                let part = if square.contains(p) {
                    Part::Square
                } else if hue.contains(p) {
                    Part::Hue
                } else {
                    return;
                };
                st.dragging = Some(part);
                shell.publish((self.on_change)(pick(part, p, self.hsv)));
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                if let Some(part) = st.dragging {
                    shell.publish((self.on_change)(pick(part, *position, self.hsv)));
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => st.dragging = None,
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
        let (square, hue) = parts(layout.bounds());
        let dragging = tree.state.downcast_ref::<State>().dragging.is_some();
        if dragging || cursor.is_over(square) {
            mouse::Interaction::Crosshair
        } else if cursor.is_over(hue) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let (square, hue) = parts(layout.bounds());
        let quad = |bounds: Rectangle, radius: f32, border: Border| Quad {
            bounds,
            border: Border { radius: radius.into(), ..border },
            shadow: Shadow::default(),
            snap: false,
        };
        let linear = |angle: f32, stops: &[(f32, Color)]| {
            let mut l = Linear::new(Radians(angle));
            for (o, c) in stops {
                l = l.add_stop(*o, *c);
            }
            Background::Gradient(Gradient::Linear(l))
        };
        let [h, s, v] = self.hsv;

        // Square: pure hue, white fading in from the left, black from the bottom.
        renderer.fill_quad(quad(square, 4.0, Border::default()), from_hsv([h, 100.0, 100.0]));
        renderer.fill_quad(
            quad(square, 4.0, Border::default()),
            linear(FRAC_PI_2, &[(0.0, Color::WHITE), (1.0, Color::TRANSPARENT)]),
        );
        renderer.fill_quad(
            quad(square, 4.0, Border::default()),
            linear(PI, &[(0.0, Color::TRANSPARENT), (1.0, Color::BLACK)]),
        );

        // Hue bar: 0°..360° left to right (7 stops; the gradient type holds up to 8).
        let stops: Vec<(f32, Color)> =
            (0..=6).map(|i| (i as f32 / 6.0, from_hsv([i as f32 * 60.0 % 360.0, 100.0, 100.0]))).collect();
        renderer.fill_quad(quad(hue, HUE_H / 2.0, Border::default()), linear(FRAC_PI_2, &stops));

        // Markers: a ring at (s, v) and a handle on the hue bar.
        let ring = |center: Point, r: f32| Rectangle { x: center.x - r, y: center.y - r, width: 2.0 * r, height: 2.0 * r };
        let sv = Point::new(square.x + s / 100.0 * square.width, square.y + (1.0 - v / 100.0) * square.height);
        let current = from_hsv(self.hsv);
        renderer.fill_quad(
            quad(ring(sv, 7.0), 7.0, Border { color: Color::WHITE, width: 2.0, ..Default::default() }),
            current,
        );
        renderer.fill_quad(
            quad(ring(sv, 8.0), 8.0, Border { color: Color { a: 0.6, ..Color::BLACK }, width: 1.0, ..Default::default() }),
            Color::TRANSPARENT,
        );
        let hx = hue.x + h / 360.0 * hue.width;
        let handle = Rectangle { x: hx - 4.0, y: hue.y - 2.0, width: 8.0, height: HUE_H + 4.0 };
        renderer.fill_quad(
            quad(handle, 3.0, Border { color: Color::WHITE, width: 2.0, ..Default::default() }),
            from_hsv([h, 100.0, 100.0]),
        );
    }
}

impl<'a, Message: 'a, Theme, Renderer> From<ColorPicker<'a, Message>> for Element<'a, Message, Theme, Renderer>
where
    Renderer: renderer::Renderer + 'a,
{
    fn from(w: ColorPicker<'a, Message>) -> Self {
        Element::new(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_round_trip() {
        for (r, g, b) in [(0, 0, 0), (255, 255, 255), (0x3b, 0x8e, 0xea), (0xce, 0x91, 0x78), (0x12, 0x12, 0x12)] {
            let c = Color::from_rgb8(r, g, b);
            assert_eq!(from_hsv(to_hsv(c)).into_rgba8(), c.into_rgba8());
        }
    }
}
