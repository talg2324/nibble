//! The command palette (`command_palette`, Cmd/Ctrl+Shift+P): every command by its title, its
//! keys beside it; typing narrows the list, Enter runs the one chosen. The way to reach the
//! commands with no key, and to learn the keys of the others.

use super::{App, Message};
use iced::widget::{button, column, container, rich_text, row, space, span, text};
use iced::{Alignment, Border, Color, Element, Font, Length, Task};
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::Command;

/// Rows shown at once.
const ROWS: usize = 12;

#[derive(Debug, Default)]
pub struct Palette {
    query: String,
    sel: usize,
}

#[derive(Debug, Clone)]
pub enum Msg {
    /// A row was clicked.
    Run(Command),
}

/// Each word of `query` is in the command's title, category or name (any case).
fn matches(c: Command, query: &str) -> bool {
    let hay = format!("{} {} {}", c.title(), c.category(), c.name().replace('_', " ")).to_lowercase();
    query.to_lowercase().split_whitespace().all(|w| hay.contains(w))
}

impl App {
    /// The commands the palette offers for what is typed: those whose title starts with it first.
    fn palette_items(&self) -> Vec<Command> {
        let q = self.palette.as_ref().map_or("", |p| p.query.as_str());
        let mut v: Vec<Command> = Command::ALL.iter().copied().filter(|c| *c != Command::CommandPalette && matches(*c, q)).collect();
        let q = q.trim().to_lowercase();
        v.sort_by_key(|c| !(!q.is_empty() && c.title().to_lowercase().starts_with(&q)));
        v
    }

    pub(super) fn palette_open(&mut self) -> Task<Message> {
        if self.palette.take().is_none() {
            self.search.open = false;
            self.palette = Some(Palette::default());
        }
        Task::none()
    }

    pub(super) fn on_palette_msg(&mut self, m: Msg) -> Task<Message> {
        match m {
            Msg::Run(c) => {
                self.palette = None;
                self.exec(c)
            }
        }
    }

    pub(super) fn palette_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let held = chord.ctrl || chord.logo;
        let n = self.palette_items().len();
        let Some(p) = self.palette.as_mut() else { return Task::none() };
        match chord.key {
            Key::Esc => self.palette = None,
            Key::Up => p.sel = p.sel.saturating_sub(1),
            Key::Down => p.sel = (p.sel + 1).min(n.saturating_sub(1)),
            Key::PageUp => p.sel = p.sel.saturating_sub(ROWS),
            Key::PageDown => p.sel = (p.sel + ROWS).min(n.saturating_sub(1)),
            Key::Enter => {
                let sel = p.sel;
                if let Some(c) = self.palette_items().get(sel).copied() {
                    return self.on_palette_msg(Msg::Run(c));
                }
            }
            Key::Backspace if held || chord.alt => {
                p.query.clear();
                p.sel = 0;
            }
            Key::Backspace => {
                p.query.pop();
                p.sel = 0;
            }
            // Its own key again closes it.
            _ if self.keymap.lookup(&chord) == Some(Command::CommandPalette) => self.palette = None,
            _ if !held => {
                let typed: String = text.unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
                if !typed.is_empty() {
                    p.query.push_str(&typed);
                    p.sel = 0;
                }
            }
            _ => {}
        }
        Task::none()
    }

    pub(super) fn palette_view(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let (accent, dim, fg) = (th.accent, th.dim, th.foreground);
        let Some(p) = &self.palette else { return space().into() };
        let items = self.palette_items();
        let header = row![text("Commands").size(11).color(dim), space::horizontal(), text(format!("{}", items.len())).size(11).color(dim)]
            .spacing(8)
            .align_y(Alignment::Center)
            .padding([2, 10]);
        let typed: [iced::widget::text::Span<'_, (), Font>; 2] = [span(p.query.as_str()).color(fg), span("▏").color(accent)];
        let field = container(rich_text(typed).font(self.face.font).size(self.face.size).width(Length::Fill))
            .padding([5, 8])
            .width(Length::Fill)
            .style(move |_| container::Style { border: Border { color: accent, width: 1.0, radius: 4.0.into() }, ..Default::default() });
        let field = container(field).padding([0, 10]).width(Length::Fixed(720.0));

        let n = items.len();
        let start = p.sel.saturating_sub(ROWS / 2).min(n.saturating_sub(ROWS));
        let mut rows = column![];
        for (i, c) in items.iter().enumerate().skip(start).take(ROWS) {
            let keys = self.keymap.chords_for(*c).first().map(|k| k.to_string()).unwrap_or_default();
            let line = row![
                text(c.title()).size(13).color(fg),
                space::horizontal(),
                text(c.category()).size(11).color(dim),
                text(keys).size(11).font(crate::fonts::mono()).color(accent).width(Length::Fixed(150.0)),
            ]
            .spacing(14)
            .align_y(Alignment::Center);
            let bg = if i == p.sel { th.selection } else { Color::TRANSPARENT };
            rows = rows.push(
                button(line)
                    .on_press(Message::Palette(Msg::Run(*c)))
                    .width(Length::Fill)
                    .padding([3, 10])
                    .style(move |_, _| button::Style { background: Some(bg.into()), ..Default::default() }),
            );
        }
        if n == 0 {
            rows = rows.push(container(text("no command matches").size(12).color(dim)).padding([3, 10]));
        }
        let hints = row![self.hint("↑↓", "choose"), self.hint("↵", "run"), self.hint("esc", "close")].spacing(18).padding([4, 10]);
        column![header, field, rows, hints].spacing(6).into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;

    #[test]
    fn words_find_commands_and_enter_runs_one() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::CommandPalette));
        assert!(a.palette.is_some());
        for ch in "fold agents".chars() {
            let _ = a.palette_key(KeyChord::new(Key::Char(ch)), Some(ch.to_string()));
        }
        assert_eq!(a.palette_items().first(), Some(&Command::FoldAgents));
        // A command with no key at all is reachable: the beginner cheat sheet.
        a.palette = Some(Palette::default());
        for ch in "beginner".chars() {
            let _ = a.palette_key(KeyChord::new(Key::Char(ch)), Some(ch.to_string()));
        }
        let before = a.beginner;
        let _ = a.palette_key(KeyChord::new(Key::Enter), None);
        assert!(a.palette.is_none() && a.beginner != before);
        assert!(!a.palette_items().contains(&Command::CommandPalette));
    }
}
