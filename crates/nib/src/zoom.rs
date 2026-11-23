//! Zoom with the mouse wheel: Cmd / Ctrl / Shift + wheel zooms the pane under the pointer.
//!
//! `ZoomRoot` wraps the whole window. With one of those modifiers held it takes the wheel event
//! before anything inside can scroll with it, finds the innermost `ZoomZone` under the pointer
//! (by a widget operation, so zones only mark themselves) and publishes how many steps to zoom.
//! Outside every zone the zoom is the global one (the code font).

use iced::advanced::widget::{tree, Operation, Tree, Widget};
use iced::advanced::{layout, mouse, overlay, renderer, Clipboard, Layout, Shell};
use iced::keyboard::Modifiers;
use iced::{mouse::ScrollDelta, Element, Event, Length, Point, Rectangle, Size, Vector};
use std::any::Any;

/// What zooms on its own, apart from the code font.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// The rendered side of a tab: Markdown / HTML preview, CSV table, SVG picture.
    Preview,
    Pdf,
    /// A web tab's page: zoomed as a browser zooms (laid out anew).
    Web,
}

/// Pixels of a trackpad scroll that make one step (a mouse notch is one step).
const PIXELS_PER_STEP: f32 = 40.0;

/// The operation that finds the innermost zone containing `at`.
struct Find {
    at: Point,
    found: Option<Zone>,
}

impl Operation for Find {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn custom(&mut self, _id: Option<&iced::advanced::widget::Id>, bounds: Rectangle, state: &mut dyn Any) {
        if let Some(z) = state.downcast_ref::<Zone>() {
            if bounds.contains(self.at) {
                self.found = Some(*z);
            }
        }
    }
}

/// Marks its content as a zone; otherwise the content as it is.
pub struct ZoomZone<'a, Message> {
    content: Element<'a, Message>,
    zone: Zone,
}

impl<'a, Message> ZoomZone<'a, Message> {
    pub fn new(zone: Zone, content: impl Into<Element<'a, Message>>) -> Self {
        Self { content: content.into(), zone }
    }
}

/// Wraps the window; turns modified wheel events into `on_zoom(zone, steps)`.
pub struct ZoomRoot<'a, Message> {
    content: Element<'a, Message>,
    on_zoom: Box<dyn Fn(Option<Zone>, i32) -> Message + 'a>,
}

impl<'a, Message> ZoomRoot<'a, Message> {
    pub fn new(content: impl Into<Element<'a, Message>>, on_zoom: impl Fn(Option<Zone>, i32) -> Message + 'a) -> Self {
        Self { content: content.into(), on_zoom: Box::new(on_zoom) }
    }
}

#[derive(Default)]
struct RootState {
    mods: Modifiers,
    /// Scroll not yet worth a whole step.
    pending: f32,
}

/// Whether a wheel event with these modifiers zooms.
pub fn zooms(m: Modifiers) -> bool {
    m.command() || m.control() || m.shift()
}

/// The wheel's travel, in steps: up (or, with Shift turning it sideways, left) is positive.
fn travel(delta: ScrollDelta) -> f32 {
    let (x, y, per) = match delta {
        ScrollDelta::Lines { x, y } => (x, y, 1.0),
        ScrollDelta::Pixels { x, y } => (x, y, PIXELS_PER_STEP),
    };
    (if y != 0.0 { y } else { x }) / per
}

/// Adds `delta` (lines, maybe a fraction of one: a trackpad's few pixels) to `pending` and takes
/// the whole lines out, keeping the rest for the next event, so slow scrolling moves instead of
/// rounding to nothing. Every scrolling grid (editor, table, diff, terminal) goes through it.
pub fn whole_lines(pending: &mut f32, delta: f32) -> f32 {
    *pending += delta;
    let n = pending.trunc();
    *pending -= n;
    n
}

/// Takes whole steps out of `pending`.
fn whole_steps(pending: &mut f32) -> i32 {
    let n = pending.trunc();
    *pending -= n;
    n as i32
}

macro_rules! delegate {
    () => {
        fn size(&self) -> Size<Length> {
            self.content.as_widget().size()
        }
        fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
            self.content.as_widget_mut().layout(tree, renderer, limits)
        }
        fn mouse_interaction(
            &self,
            tree: &Tree,
            layout: Layout<'_>,
            cursor: mouse::Cursor,
            viewport: &Rectangle,
            renderer: &iced::Renderer,
        ) -> mouse::Interaction {
            self.content.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
        }
        fn draw(
            &self,
            tree: &Tree,
            renderer: &mut iced::Renderer,
            theme: &iced::Theme,
            style: &renderer::Style,
            layout: Layout<'_>,
            cursor: mouse::Cursor,
            viewport: &Rectangle,
        ) {
            self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
        }
        fn overlay<'b>(
            &'b mut self,
            tree: &'b mut Tree,
            layout: Layout<'b>,
            renderer: &iced::Renderer,
            viewport: &Rectangle,
            translation: Vector,
        ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
            self.content.as_widget_mut().overlay(tree, layout, renderer, viewport, translation)
        }
    };
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for ZoomZone<'_, Message> {
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }
    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }
    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }
    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &iced::Renderer, operation: &mut dyn Operation) {
        operation.custom(None, layout.bounds(), &mut self.zone);
        self.content.as_widget_mut().operate(tree, layout, renderer, operation);
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
        self.content.as_widget_mut().update(tree, event, layout, cursor, renderer, clipboard, shell, viewport);
    }
    delegate!();
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for ZoomRoot<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<RootState>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(RootState::default())
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
    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        let child = self.content.as_widget_mut().layout(&mut tree.children[0], renderer, limits);
        layout::Node::with_children(child.size(), vec![child])
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &iced::Renderer, operation: &mut dyn Operation) {
        operation.traverse(&mut |op| {
            self.content.as_widget_mut().operate(&mut tree.children[0], child(layout), renderer, op);
        });
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
        let st = tree.state.downcast_mut::<RootState>();
        match event {
            Event::Keyboard(iced::keyboard::Event::ModifiersChanged(m)) => {
                st.mods = *m;
                st.pending = 0.0;
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if zooms(st.mods) => {
                if let Some(at) = cursor.position() {
                    st.pending += travel(*delta);
                    let steps = whole_steps(&mut st.pending);
                    if steps != 0 {
                        let mut find = Find { at, found: None };
                        self.content.as_widget_mut().operate(&mut tree.children[0], child(layout), renderer, &mut find);
                        shell.publish((self.on_zoom)(find.found, steps));
                    }
                    shell.capture_event();
                    return;
                }
            }
            _ => {}
        }
        self.content.as_widget_mut().update(&mut tree.children[0], event, child(layout), cursor, renderer, clipboard, shell, viewport);
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], child(layout), cursor, viewport, renderer)
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, child(layout), cursor, viewport);
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(&mut tree.children[0], child(layout), renderer, viewport, translation)
    }
}

fn child(layout: Layout<'_>) -> Layout<'_> {
    layout.children().next().expect("ZoomRoot lays out one child")
}

impl<'a, Message: 'a> From<ZoomZone<'a, Message>> for Element<'a, Message> {
    fn from(z: ZoomZone<'a, Message>) -> Self {
        Element::new(z)
    }
}

impl<'a, Message: 'a> From<ZoomRoot<'a, Message>> for Element<'a, Message> {
    fn from(z: ZoomRoot<'a, Message>) -> Self {
        Element::new(z)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn slow_trackpad_scrolling_adds_up() {
        let mut acc = 0.0;
        // A third of a line per event: nothing, nothing, then one line.
        let moved: Vec<f32> = (0..6).map(|_| super::whole_lines(&mut acc, 0.34)).collect();
        assert_eq!(moved, [0.0, 0.0, 1.0, 0.0, 0.0, 1.0]);
        assert_eq!(super::whole_lines(&mut acc, -2.5), -2.0);
    }

    use super::*;

    #[test]
    fn wheel_travel_in_steps() {
        assert_eq!(travel(ScrollDelta::Lines { x: 0.0, y: 2.0 }), 2.0);
        // Shift turns a mouse wheel sideways on macOS.
        assert_eq!(travel(ScrollDelta::Lines { x: -1.0, y: 0.0 }), -1.0);
        let mut pending = 0.0;
        for _ in 0..3 {
            pending += travel(ScrollDelta::Pixels { x: 0.0, y: 15.0 });
        }
        assert_eq!(whole_steps(&mut pending), 1);
        assert!((pending - 5.0 / PIXELS_PER_STEP).abs() < 1e-6);
        assert!(zooms(Modifiers::SHIFT) && zooms(Modifiers::CTRL) && !zooms(Modifiers::ALT));
        // Cmd zooms on macOS; elsewhere the Logo key is the system's.
        assert_eq!(zooms(Modifiers::LOGO), cfg!(target_os = "macos"));
    }
}
