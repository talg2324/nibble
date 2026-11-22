//! Language features, answered by the language server inside nibd (Python: `ty`): go to
//! definition, find references, rename symbol, change all occurrences, signature and docs on
//! hover, and back / forward through jumps. Also the overlays they use: the results list, the
//! rename box and the hover popup.

use super::{async_sleep, App, Message, Pending, Tab};
use crate::editor_view::ViewEvent;
use super::viewers::PreviewImage;
use crate::math;
use iced::widget::{button, column, container, markdown, mouse_area, rich_text, row, scrollable, space, span, stack, text};
use iced::{mouse, Alignment, Border, Color, Element, Font, Length, Padding, Rectangle, Task};
use nib_editor::document::char_at_visual_col;
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::Command;
use nib_proto::{ClientNotify, FileEdit, HoverInfo, Location, LspOp, Pos, Request, Response, RpcError};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

/// Jumps remembered for Go back.
const NAV_LIMIT: usize = 100;
/// Rows of the results list shown at once.
const PICKER_ROWS: usize = 14;
/// How long the mouse rests on a symbol before its signature and docs are asked for.
const HOVER_DELAY: Duration = Duration::from_millis(500);
/// How long the popup stays after the mouse leaves its symbol (time to reach the popup).
const HOVER_LINGER: Duration = Duration::from_millis(300);
const HOVER_MAX_W: f32 = 640.0;
const HOVER_MAX_H: f32 = 360.0;

#[derive(Default)]
pub struct State {
    pub picker: Option<Picker>,
    pub rename: Option<Rename>,
    hover: Hover,
    back: Vec<Spot>,
    forward: Vec<Spot>,
    /// File extensions the daemon has a language server for (its `Hello`).
    pub lsp_extensions: HashSet<String>,
}

/// Signature and docs of the symbol under the mouse.
#[derive(Default)]
struct Hover {
    /// Bumped on every change; a `HoverDue` with an older number is stale.
    seq: u64,
    /// The symbol character the mouse rests on (`None`: not on one).
    target: Option<HoverTarget>,
    shown: Option<HoverPopup>,
    /// The mouse is on the popup: keep it.
    over_popup: bool,
}

#[derive(Clone, Copy)]
struct HoverTarget {
    tab: u64,
    version: u64,
    pos: Pos,
    /// The character's cell and the text area, in window coordinates.
    cell: Rectangle,
    area: Rectangle,
}

struct HoverPopup {
    tab: u64,
    /// The symbol it describes: the popup stays while the mouse is on it.
    start: Pos,
    end: Pos,
    content: markdown::Content,
    cell: Rectangle,
    area: Rectangle,
}

impl HoverPopup {
    fn covers(&self, t: &HoverTarget) -> bool {
        t.tab == self.tab && self.start <= t.pos && t.pos < self.end
    }
}

impl State {
    /// An overlay has the keyboard.
    pub fn modal(&self) -> bool {
        self.picker.is_some() || self.rename.is_some()
    }
}

/// Definitions / references to choose from.
pub struct Picker {
    title: String,
    items: Vec<Location>,
    selected: usize,
}

/// The rename box: a new name for the symbol at `pos` in tab `tab`.
pub struct Rename {
    tab: u64,
    version: u64,
    pos: Pos,
    old: String,
    text: String,
    /// Still showing the old name selected: the first keystroke replaces it.
    fresh: bool,
}

#[derive(Clone, Debug)]
struct Spot {
    path: String,
    pos: Pos,
}

/// Files the language server handles (by extension: syntax may not be attached yet).
fn has_lsp(t: &Tab, exts: &HashSet<String>) -> bool {
    !t.local
        && t.media.is_none()
        && t.doc.path.as_deref().and_then(Path::extension).and_then(|e| e.to_str()).is_some_and(|e| exts.contains(e))
}

impl App {
    // -----------------------------------------------------------------------------------------
    // buffer sync
    // -----------------------------------------------------------------------------------------

    /// Send tab `i`'s text to the language server if it changed since the last sync.
    fn sync_tab(&mut self, i: usize) {
        let Some(t) = self.tabs.get_mut(i) else { return };
        let version = t.doc.version();
        if !has_lsp(t, &self.code.lsp_extensions) || t.lsp_synced == Some(version) {
            return;
        }
        t.lsp_synced = Some(version);
        let path = t.doc.path.as_ref().unwrap().to_string_lossy().into_owned();
        let text = t.doc.text.to_string();
        self.notify(ClientNotify::DocSync { path, version, text });
    }

    /// Sync the active tab (also on open, so the server starts indexing before the first query).
    pub(super) fn lsp_sync(&mut self) {
        self.sync_tab(self.active);
    }

    /// Tab `i` is going away.
    pub(super) fn lsp_close(&mut self, i: usize) {
        let Some(t) = self.tabs.get(i) else { return };
        if let (Some(_), Some(p)) = (t.lsp_synced, &t.doc.path) {
            let path = p.to_string_lossy().into_owned();
            self.notify(ClientNotify::DocClose { path });
        }
    }

    // -----------------------------------------------------------------------------------------
    // commands
    // -----------------------------------------------------------------------------------------

    pub(super) fn code_command(&mut self, c: Command) -> Task<Message> {
        let Some(tab) = self.tab() else { return Task::none() };
        if !has_lsp(tab, &self.code.lsp_extensions) {
            return match c {
                Command::ChangeAllOccurrences => self.select_text_occurrences(),
                _ => {
                    let lang = tab.doc.language_name();
                    self.flash_info(format!("no language server for {lang} files"))
                }
            };
        }
        self.code.picker = None;
        let op = match c {
            Command::GoToDefinition => LspOp::Definition,
            Command::FindReferences => LspOp::References,
            Command::RenameSymbol => LspOp::PrepareRename,
            _ => LspOp::Highlights,
        };
        let r = self.tabs[self.active].doc.selection.primary();
        let at = if r.is_empty() { r.head } else { r.from() };
        self.lsp_query(at, op)
    }

    /// Ask about the symbol at char index `at` in the active tab.
    fn lsp_query(&mut self, at: usize, op: LspOp) -> Task<Message> {
        self.lsp_sync();
        let t = &self.tabs[self.active];
        let (line, col) = t.doc.line_col_of(at);
        let path = t.doc.path.as_ref().unwrap().to_string_lossy().into_owned();
        let pos = Pos { line: line as u32, col: col as u32 };
        let pending = Pending::Lsp { tab: t.id, version: t.doc.version(), op: op.clone() };
        self.request(Request::Lsp { path, pos, op }, pending)
    }

    pub(super) fn on_lsp(
        &mut self,
        tab: u64,
        version: u64,
        op: LspOp,
        result: Result<Response, RpcError>,
    ) -> Task<Message> {
        let r = match result {
            Ok(r) => r,
            // No server (or it failed): plain text matching still does the job.
            Err(_) if op == LspOp::Highlights => return self.text_occurrences_if(tab, version),
            // Not worth a message: the mouse just passed by.
            Err(_) if op == LspOp::Hover => return Task::none(),
            Err(e) => return self.flash_warn(e.msg),
        };
        match (op, r) {
            (LspOp::Definition, Response::Locations(l)) => match l.len() {
                0 => self.flash_info("no definition found"),
                1 => self.jump(&l[0]),
                n => self.show_picker(format!("{n} definitions"), l),
            },
            (LspOp::References, Response::Locations(l)) => match l.len() {
                0 => self.flash_info("no references found"),
                n => {
                    let name = self.symbol_name(&l[0]);
                    self.show_picker(format!("{n} references to {name}"), l)
                }
            },
            (LspOp::Highlights, Response::Locations(l)) => {
                if l.is_empty() {
                    return self.text_occurrences_if(tab, version);
                }
                let Some(t) = self.tabs.get_mut(self.active).filter(|t| t.id == tab && t.doc.version() == version)
                else {
                    return Task::none(); // edited or switched away meanwhile
                };
                let ranges: Vec<(usize, usize)> = l
                    .iter()
                    .map(|l| {
                        let a = t.doc.char_at(l.start.line as usize, l.start.col as usize);
                        let b = t.doc.char_at(l.end.line as usize, l.end.col as usize);
                        (a, b)
                    })
                    .collect();
                t.doc.select_ranges(&ranges);
                self.follow_cursor();
                self.occurrences_flash(ranges.len())
            }
            (LspOp::PrepareRename, Response::RenameRange(r)) => match r {
                None => self.flash_info("nothing to rename here"),
                Some((pos, _, old)) => {
                    let text = old.clone();
                    self.code.rename = Some(Rename { tab, version, pos, old, text, fresh: true });
                    Task::none()
                }
            },
            (LspOp::Rename { .. }, Response::Edits(files)) => self.apply_rename(files),
            (LspOp::Hover, Response::Hover(info)) => {
                self.show_hover(tab, version, info);
                Task::none()
            }
            (_, other) => self.flash_warn(format!("unexpected answer: {other:?}")),
        }
    }

    /// The identifier a location points at (for titles).
    fn symbol_name(&self, l: &Location) -> String {
        if l.start.line != l.end.line {
            return "symbol".into();
        }
        let n = l.end.col.saturating_sub(l.start.col) as usize;
        l.line_text.chars().skip(l.start.col as usize).take(n).collect()
    }

    // -----------------------------------------------------------------------------------------
    // hover
    // -----------------------------------------------------------------------------------------

    /// Close the popup and forget what the mouse was on (keys, clicks, scrolling).
    pub(super) fn hover_reset(&mut self) {
        let h = &mut self.code.hover;
        h.seq += 1;
        h.target = None;
        h.shown = None;
        h.over_popup = false;
    }

    /// The mouse moved to another text cell, or off the text.
    pub(super) fn on_hover(&mut self, ev: ViewEvent) -> Task<Message> {
        let target = match ev {
            ViewEvent::Hover { line, col, cell, area } => self.hover_target(line, col, cell, area),
            _ => None,
        };
        let h = &mut self.code.hover;
        h.seq += 1;
        h.target = target;
        let delay = match (&h.shown, &h.target) {
            (Some(s), Some(t)) if s.covers(t) => return Task::none(),
            (_, Some(_)) => HOVER_DELAY,
            (Some(_), None) => HOVER_LINGER,
            (None, None) => return Task::none(),
        };
        self.hover_timer(delay)
    }

    fn hover_timer(&self, delay: Duration) -> Task<Message> {
        let seq = self.code.hover.seq;
        Task::perform(async_sleep(delay), move |_| Message::HoverDue(seq))
    }

    /// The symbol character at (line, visual column) of the active tab, if it has a language server.
    fn hover_target(&self, line: usize, col: usize, cell: Rectangle, area: Rectangle) -> Option<HoverTarget> {
        let tab = self.tab().filter(|t| has_lsp(t, &self.code.lsp_extensions) && !self.code.modal() && !t.rendered)?;
        let doc = &tab.doc;
        let t = doc.text.slice(..);
        let p = char_at_visual_col(t, line, col, doc.tab_width);
        let c = t.get_char(p)?;
        if !(c.is_alphanumeric() || c == '_') {
            return None;
        }
        let (line, col) = doc.line_col_of(p);
        let pos = Pos { line: line as u32, col: col as u32 };
        Some(HoverTarget { tab: tab.id, version: doc.version(), pos, cell, area })
    }

    pub(super) fn hover_due(&mut self, seq: u64) -> Task<Message> {
        let h = &mut self.code.hover;
        if seq != h.seq || h.over_popup {
            return Task::none();
        }
        let Some(t) = h.target else {
            h.shown = None;
            return Task::none();
        };
        if h.shown.as_ref().is_some_and(|s| s.covers(&t)) {
            return Task::none();
        }
        h.shown = None;
        match self.tab() {
            Some(tab) if tab.id == t.tab && tab.doc.version() == t.version => {
                let at = tab.doc.char_at(t.pos.line as usize, t.pos.col as usize);
                self.lsp_query(at, LspOp::Hover)
            }
            _ => Task::none(),
        }
    }

    pub(super) fn hover_popup(&mut self, over: bool) -> Task<Message> {
        let h = &mut self.code.hover;
        h.over_popup = over;
        if over {
            return Task::none();
        }
        h.seq += 1;
        self.hover_timer(HOVER_LINGER)
    }

    #[cfg(test)]
    pub(super) fn hover_seq(&self) -> u64 {
        self.code.hover.seq
    }

    /// The symbol the popup is showing, if any.
    #[cfg(test)]
    pub(super) fn hover_shown(&self) -> Option<(Pos, Pos)> {
        self.code.hover.shown.as_ref().map(|s| (s.start, s.end))
    }

    /// The answer arrived: show it if the mouse is still on that symbol.
    fn show_hover(&mut self, tab: u64, version: u64, info: Option<HoverInfo>) {
        let h = &mut self.code.hover;
        let (Some(info), Some(t)) = (info, h.target) else { return };
        if t.tab != tab || t.version != version {
            return;
        }
        let popup = HoverPopup {
            tab,
            start: info.start,
            end: info.end,
            content: markdown::Content::parse(&info.markdown),
            cell: t.cell,
            area: t.area,
        };
        // The mouse moved on meanwhile: its own query is on the way.
        if popup.covers(&t) {
            h.shown = Some(popup);
        }
    }

    // -----------------------------------------------------------------------------------------
    // change all occurrences
    // -----------------------------------------------------------------------------------------

    fn occurrences_flash(&mut self, n: usize) -> Task<Message> {
        if n == 1 {
            return self.flash_info("only one occurrence");
        }
        self.flash_info(format!("{n} occurrences — type to change them all · Esc for one cursor"))
    }

    /// Whole-word text matches (no language server, or it found nothing).
    fn select_text_occurrences(&mut self) -> Task<Message> {
        let Some(t) = self.tabs.get_mut(self.active) else { return Task::none() };
        let occ = t.doc.text_occurrences();
        if occ.is_empty() {
            return self.flash_info("no word under the cursor");
        }
        t.doc.select_ranges(&occ);
        self.follow_cursor();
        self.occurrences_flash(occ.len())
    }

    fn text_occurrences_if(&mut self, tab: u64, version: u64) -> Task<Message> {
        match self.tab() {
            Some(t) if t.id == tab && t.doc.version() == version => self.select_text_occurrences(),
            _ => Task::none(),
        }
    }

    // -----------------------------------------------------------------------------------------
    // rename
    // -----------------------------------------------------------------------------------------

    fn rename_submit(&mut self) -> Task<Message> {
        let Some(r) = self.code.rename.take() else { return Task::none() };
        let name = r.text.trim().to_string();
        if name.is_empty() || name == r.old {
            return Task::none();
        }
        let Some(i) = self.tabs.iter().position(|t| t.id == r.tab) else { return Task::none() };
        if self.tabs[i].doc.version() != r.version {
            return self.flash_warn("the file changed — rename again");
        }
        self.active = i;
        let at = self.tabs[i].doc.char_at(r.pos.line as usize, r.pos.col as usize);
        self.lsp_query(at, LspOp::Rename { new_name: name })
    }

    /// Open buffers are edited in place (left unsaved, one undo step each); other files are
    /// rewritten on disk by nibd.
    fn apply_rename(&mut self, files: Vec<FileEdit>) -> Task<Message> {
        let n_files = files.len();
        let n_edits: usize = files.iter().map(|f| f.edits.len()).sum();
        let mut on_disk = Vec::new();
        let mut in_buffers = 0;
        for f in files {
            let open = self.tabs.iter().position(|t| !t.local && t.doc.path.as_deref() == Some(Path::new(&f.path)));
            let Some(i) = open else {
                on_disk.push(f);
                continue;
            };
            let doc = &mut self.tabs[i].doc;
            let edits = f
                .edits
                .into_iter()
                .map(|e| {
                    let a = doc.char_at(e.start.line as usize, e.start.col as usize);
                    let b = doc.char_at(e.end.line as usize, e.end.col as usize);
                    (a, b, e.new_text)
                })
                .collect();
            doc.apply_edits(edits);
            self.sync_tab(i);
            in_buffers += 1;
        }
        if n_edits == 0 {
            return self.flash_info("nothing to rename");
        }
        let task = if on_disk.is_empty() {
            Task::none()
        } else {
            self.request(Request::ApplyEdits { files: on_disk }, Pending::ApplyEdits)
        };
        let unsaved = if in_buffers > 0 { " (open files changed, not saved yet)" } else { "" };
        let s = |n: usize| if n == 1 { "" } else { "s" };
        let msg = format!("renamed {n_edits} occurrence{} in {n_files} file{}{unsaved}", s(n_edits), s(n_files));
        Task::batch([task, self.flash_info(msg)])
    }

    // -----------------------------------------------------------------------------------------
    // navigation
    // -----------------------------------------------------------------------------------------

    fn here(&self) -> Option<Spot> {
        let t = self.tab().filter(|t| !t.local)?;
        let (line, col) = t.doc.line_col_of(t.doc.selection.primary().head);
        let path = t.doc.path.as_ref()?.to_string_lossy().into_owned();
        Some(Spot { path, pos: Pos { line: line as u32, col: col as u32 } })
    }

    /// Go to `l`, remembering where we were.
    pub(super) fn jump(&mut self, l: &Location) -> Task<Message> {
        if let Some(h) = self.here() {
            self.code.back.push(h);
            if self.code.back.len() > NAV_LIMIT {
                self.code.back.remove(0);
            }
        }
        self.code.forward.clear();
        self.open_path_at(l.path.clone(), true, Some(l.start))
    }

    pub(super) fn navigate(&mut self, forward: bool) -> Task<Message> {
        let here = self.here();
        let code = &mut self.code;
        let (from, to) = if forward { (&mut code.forward, &mut code.back) } else { (&mut code.back, &mut code.forward) };
        let Some(spot) = from.pop() else {
            return self.flash_info(if forward { "nothing to go forward to" } else { "nothing to go back to" });
        };
        if let Some(h) = here {
            to.push(h);
        }
        self.open_path_at(spot.path, true, Some(spot.pos))
    }

    /// Cursor to `pos` in the active tab, scrolled into view (a third from the top if it was off
    /// screen, like VSCode's reveal).
    pub(super) fn goto_in_active(&mut self, pos: Pos) {
        let rows = self.rows.max(1);
        let Some(t) = self.tabs.get_mut(self.active) else { return };
        let p = t.doc.char_at(pos.line as usize, pos.col as usize);
        t.doc.set_cursor(p, false);
        let line = t.doc.line_col_of(p).0;
        if line < t.top || line >= t.top + rows {
            t.top = line.saturating_sub(rows / 3);
        }
        self.follow_cursor();
    }

    // -----------------------------------------------------------------------------------------
    // overlays: keys
    // -----------------------------------------------------------------------------------------

    fn show_picker(&mut self, title: String, items: Vec<Location>) -> Task<Message> {
        self.code.picker = Some(Picker { title, items, selected: 0 });
        Task::none()
    }

    pub(super) fn picker_open(&mut self, i: usize) -> Task<Message> {
        let Some(p) = self.code.picker.take() else { return Task::none() };
        match p.items.get(i) {
            Some(l) => self.jump(l),
            None => Task::none(),
        }
    }

    /// Keys while an overlay is open.
    pub(super) fn code_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        if self.code.rename.is_some() {
            return self.rename_key(chord, text);
        }
        let Some(p) = &mut self.code.picker else { return Task::none() };
        let last = p.items.len().saturating_sub(1);
        let plain = !(chord.ctrl || chord.logo || chord.alt || chord.shift);
        match chord.key {
            Key::Up if plain => p.selected = p.selected.saturating_sub(1),
            Key::Down if plain => p.selected = (p.selected + 1).min(last),
            Key::PageUp if plain => p.selected = p.selected.saturating_sub(PICKER_ROWS),
            Key::PageDown if plain => p.selected = (p.selected + PICKER_ROWS).min(last),
            Key::Home if plain => p.selected = 0,
            Key::End if plain => p.selected = last,
            Key::Enter if plain => {
                let i = p.selected;
                return self.picker_open(i);
            }
            Key::Esc => self.code.picker = None,
            // Any other shortcut closes the list and runs as usual.
            _ => {
                if let Some(c) = self.keymap.lookup(&chord) {
                    self.code.picker = None;
                    return self.exec(c);
                }
            }
        }
        Task::none()
    }

    fn rename_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let Some(r) = &mut self.code.rename else { return Task::none() };
        match chord.key {
            Key::Esc => self.code.rename = None,
            Key::Enter => return self.rename_submit(),
            Key::Backspace => {
                if r.fresh || chord.alt || chord.ctrl || chord.logo {
                    r.text.clear();
                } else {
                    r.text.pop();
                }
                r.fresh = false;
            }
            Key::Left | Key::Right | Key::Home | Key::End => r.fresh = false,
            Key::Char('a') if chord.ctrl || chord.logo => r.fresh = true,
            _ if !(chord.ctrl || chord.logo) => {
                let typed: String = text.unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
                if !typed.is_empty() {
                    if r.fresh {
                        r.text.clear();
                        r.fresh = false;
                    }
                    r.text.push_str(&typed);
                }
            }
            _ => {}
        }
        Task::none()
    }

    // -----------------------------------------------------------------------------------------
    // overlays: view
    // -----------------------------------------------------------------------------------------

    /// Always a stack, so the views below keep their state when an overlay comes and goes.
    pub(super) fn with_code_overlay<'a>(&'a self, base: Element<'a, Message>) -> Element<'a, Message> {
        let mut layers = stack![base];
        if let Some(h) = self.hover_view() {
            layers = layers.push(h);
        }
        let card = match (&self.code.picker, &self.code.rename) {
            _ if self.search.open => self.search_view(),
            (Some(p), _) => self.picker_view(p),
            (None, Some(r)) => self.rename_view(r),
            (None, None) => return layers.into(),
        };
        let (panel, border) = (self.theme.panel, self.theme.divider);
        let card = container(card).padding(6).max_width(860).style(move |_| container::Style {
            background: Some(panel.into()),
            border: Border { color: border, width: 1.0, radius: 6.0.into() },
            shadow: iced::Shadow {
                color: Color { a: 0.5, ..Color::BLACK },
                offset: iced::Vector::new(0.0, 4.0),
                blur_radius: 16.0,
            },
            ..Default::default()
        });
        let placed = container(card)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Start)
            .padding(Padding { top: 40.0, right: 16.0, bottom: 16.0, left: 16.0 });
        layers.push(placed).into()
    }

    /// The popup under the hovered line, or above it when there's more room there.
    fn hover_view(&self) -> Option<Element<'_, Message>> {
        let s = self.code.hover.shown.as_ref()?;
        if self.tab()?.id != s.tab || self.code.modal() {
            return None;
        }
        let th = &self.theme;
        let mut style = markdown::Style::from_palette(iced::Theme::Dark.palette());
        style.link_color = th.accent;
        style.inline_code_color = th.scope("string");
        style.inline_code_font = self.face.font;
        style.code_block_font = self.face.font;
        let settings = markdown::Settings::with_text_size(self.face.size.min(14.0), style);
        let viewer = MarkdownViewer { font: self.face.font, size: self.face.size, color: th.foreground, math: None, images: None };
        let body = markdown::view_with(s.content.items(), settings, &viewer);
        let (panel, border, fg) = (th.panel, th.divider, th.foreground);

        let (cell, area) = (s.cell, s.area);
        let room_below = area.y + area.height - (cell.y + cell.height);
        let room_above = cell.y - area.y;
        let below = room_below >= HOVER_MAX_H || room_below >= room_above;
        let max_h = HOVER_MAX_H.min(if below { room_below } else { room_above } - 4.0).max(60.0);
        let left = cell.x.min(area.x + area.width - HOVER_MAX_W).max(area.x);

        let card = container(scrollable(container(body).padding([8, 12])).style(crate::theme::scrollbar).height(Length::Shrink))
            .max_width(HOVER_MAX_W)
            .max_height(max_h)
            .style(move |_| container::Style {
                background: Some(panel.into()),
                text_color: Some(fg),
                border: Border { color: border, width: 1.0, radius: 4.0.into() },
                shadow: iced::Shadow {
                    color: Color { a: 0.4, ..Color::BLACK },
                    offset: iced::Vector::new(0.0, 2.0),
                    blur_radius: 10.0,
                },
                ..Default::default()
            });
        // A cursor of its own, so the editor below sees the mouse leave its text.
        let card = mouse_area(card)
            .on_enter(Message::HoverPopup(true))
            .on_exit(Message::HoverPopup(false))
            .interaction(mouse::Interaction::Idle);
        let placed: Element<'_, Message> = if below {
            container(card)
                .padding(Padding { top: cell.y + cell.height, left, right: 0.0, bottom: 0.0 })
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            // A box from the window's top to the line, with the popup at its bottom.
            container(card)
                .padding(Padding { left, ..Padding::ZERO })
                .width(Length::Fill)
                .height(cell.y)
                .align_y(Alignment::End)
                .into()
        };
        Some(placed)
    }

    fn picker_view<'a>(&'a self, p: &'a Picker) -> Element<'a, Message> {
        let th = &self.theme;
        let n = p.items.len();
        let start = p.selected.saturating_sub(PICKER_ROWS / 2).min(n.saturating_sub(PICKER_ROWS));
        let root = self.root.trim_end_matches('/');
        let rows = p.items.iter().enumerate().skip(start).take(PICKER_ROWS).map(|(i, l)| {
            let rel = l.path.strip_prefix(root).map_or(l.path.as_str(), |r| r.trim_start_matches('/'));
            let code: String = l.line_text.trim().chars().take(140).collect();
            let sel = i == p.selected;
            let bg = if sel { th.selection } else { Color::TRANSPARENT };
            let line = row![
                text(format!("{rel}:{}", l.start.line + 1)).size(12).font(crate::fonts::mono()).color(th.dim),
                text(code).size(self.face.size.min(13.0)).font(self.face.font).color(th.foreground),
            ]
            .spacing(14)
            .align_y(Alignment::Center);
            button(line)
                .on_press(Message::PickerOpen(i))
                .width(Length::Fill)
                .padding([3, 10])
                .style(move |_, _| button::Style { background: Some(bg.into()), ..Default::default() })
                .into()
        });
        let header = row![
            text(&p.title).size(11).color(th.dim),
            space::horizontal(),
            text(format!("{} / {n}", p.selected + 1)).size(11).color(th.dim),
        ]
        .padding([2, 10]);
        let hints = row![self.hint("↑↓", "choose"), self.hint("↵", "go"), self.hint("esc", "close")]
            .spacing(18)
            .padding([4, 10]);
        column![header, column(rows), hints].spacing(4).into()
    }

    fn rename_view<'a>(&'a self, r: &'a Rename) -> Element<'a, Message> {
        let th = &self.theme;
        let (accent, sel) = (th.accent, th.selection);
        // The caret is a glyph inside the text so it follows the last character when it wraps.
        let name = span(r.text.as_str()).color(th.foreground);
        let name = if r.fresh { name.background(sel) } else { name };
        let typed: [iced::widget::text::Span<'_, (), Font>; 2] = [name, span("▏").color(accent)];
        let field = container(rich_text(typed).font(self.face.font).size(self.face.size).width(Length::Fill))
            .padding([5, 8])
            .width(Length::Fixed(420.0))
            .style(move |_| container::Style {
                border: Border { color: accent, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            });
        let hints = row![self.hint("↵", "rename everywhere"), self.hint("esc", "cancel")].spacing(18);
        column![text(format!("Rename {}", r.old)).size(11).color(th.dim), field, hints]
            .spacing(8)
            .padding([6, 10])
            .into()
    }
}

/// Hover Markdown: code blocks (the signature) in the editor's font, wrapped rather than scrolled.
/// Markdown with code blocks in the code font (hover popups, the AI panel).
/// With `math`, formulas (see `crate::math`) are drawn too.
pub(super) struct MarkdownViewer<'m> {
    pub font: Font,
    pub size: f32,
    pub color: Color,
    pub math: Option<&'m math::Formulas>,
    /// Pictures that have arrived (the AI panel), by URL, and how long the mouse rests on one
    /// before it is shown bigger. A click opens one in a tab.
    pub images: Option<(&'m HashMap<String, Option<PreviewImage>>, Duration)>,
}

/// The tallest a picture in a reply is drawn; a click shows it whole.
const REPLY_IMAGE_MAX_H: f32 = 360.0;

impl<'a> markdown::Viewer<'a, Message> for MarkdownViewer<'a> {
    fn on_link_click(url: markdown::Uri) -> Message {
        Message::PreviewLink(url)
    }

    fn paragraph(&self, settings: markdown::Settings, text: &markdown::Text) -> Element<'a, Message> {
        math::rich(&text.spans(settings.style), self.math, settings.text_size.0, self.color, Self::on_link_click)
            .unwrap_or_else(|| markdown::paragraph(settings, text, Self::on_link_click))
    }

    fn image(
        &self,
        settings: markdown::Settings,
        url: &'a markdown::Uri,
        title: &'a str,
        alt: &markdown::Text,
    ) -> Element<'a, Message> {
        let picture = self.images.and_then(|(m, delay)| Some((m.get(url.as_str())?.as_ref()?, delay)));
        match (math::get(self.math, url), picture) {
            (Some(f), _) => container(math::display(f, settings.text_size.0, self.color)).padding([settings.spacing.0 / 2.0, 0.0]).into(),
            (None, Some((img, delay))) => {
                let pic = container(super::peek::peek(img.view(), img, delay)).max_height(REPLY_IMAGE_MAX_H).padding([settings.spacing.0 / 2.0, 0.0]);
                mouse_area(pic)
                    .on_press(Message::Ai(super::ai::Msg::OpenImage(url.clone())))
                    .interaction(mouse::Interaction::Pointer)
                    .into()
            }
            (None, None) => {
                let _ = title;
                container(iced::widget::rich_text(alt.spans(settings.style)).on_link_click(Self::on_link_click))
                    .padding(settings.spacing.0)
                    .style(container::dark)
                    .into()
            }
        }
    }

    fn code_block(
        &self,
        settings: markdown::Settings,
        _language: Option<&'a str>,
        code: &'a str,
        _lines: &'a [markdown::Text],
    ) -> Element<'a, Message> {
        let body = text(code.trim_end_matches('\n')).font(self.font).size(self.size).color(self.color);
        container(body).padding(settings.spacing.0 / 2.0).width(Length::Fill).into()
    }
}
