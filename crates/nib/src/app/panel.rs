//! What the side panels (SSH, apps) are drawn with: the frame around one, its one-line text
//! boxes and its buttons, so the panels look and behave alike.

use super::line_input::LineInput;
use super::{App, Message};
use iced::widget::{button, column, container, mouse_area, rich_text, row, scrollable, space, text};
use iced::{Alignment, Border, Color, Element, Length, Padding};

/// The command key, as a hint shows it before another key.
pub(super) const MOD_PLUS: &str = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" };

impl App {
    /// The panel around `body`, `width` wide: scrolls, keyboard halo, a click sends `focus`.
    pub(super) fn panel_frame<'a>(&'a self, header: iced::widget::Row<'a, Message>, body: iced::widget::Column<'a, Message>, focused: bool, width: f32, focus: Message) -> Element<'a, Message> {
        let (accent, panel) = (self.theme.accent, self.theme.panel);
        let body = scrollable(body.padding(Padding { left: 14.0, right: 14.0, top: 10.0, bottom: 12.0 })).style(crate::theme::scrollbar).height(Length::Fill);
        let halo = if focused { accent } else { Color::TRANSPARENT };
        let panel = container(column![header, self.hrule(), body]).padding(2).width(width).height(Length::Fill).style(move |_| container::Style {
            background: Some(panel.into()),
            border: Border { color: halo, width: 2.0, radius: 3.0.into() },
            ..Default::default()
        });
        mouse_area(panel).on_press(focus).into()
    }

    /// A one-line text box: `value`, or the dim `placeholder`; a caret when it has the keyboard.
    pub(super) fn panel_box(&self, value: &LineInput, placeholder: &str, selected: bool, focused: bool) -> Element<'_, Message> {
        let th = &self.theme;
        let (accent, dim, fg, divider) = (th.accent, th.dim, th.foreground, th.divider);
        let active = selected && focused;
        let caret = container(space()).width(2).height(15).style(move |_| container::Style::default().background(if active { accent } else { Color::TRANSPARENT }));
        let shown: Element<'_, Message> = if value.is_empty() {
            row![caret, text(placeholder.to_owned()).size(12).color(dim)].align_y(Alignment::Center).into()
        } else {
            let typed = value.spans(fg, if active { accent } else { Color::TRANSPARENT });
            super::tail_line(rich_text(typed).size(12).wrapping(text::Wrapping::None))
        };
        container(shown)
            .padding([6, 8])
            .width(Length::Fill)
            .clip(true)
            .style(move |_| container::Style {
                border: Border { color: if active { accent } else { divider }, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            })
            .into()
    }

    pub(super) fn panel_button(&self, label: impl Into<String>, msg: Option<Message>, selected: bool) -> Element<'_, Message> {
        let th = &self.theme;
        let (accent, fg, sel_bg, divider) = (th.accent, th.foreground, th.selection, th.divider);
        let mut b = button(text(label.into()).size(12).color(fg)).padding([4, 10]).style(move |_, st| button::Style {
            background: (selected || matches!(st, button::Status::Hovered)).then(|| sel_bg.into()),
            border: Border { color: if selected { accent } else { divider }, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        });
        if let Some(m) = msg {
            b = b.on_press(m);
        }
        b.into()
    }
}
