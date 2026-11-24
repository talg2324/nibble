//! The web tab: a page of a local app, shown in nib. The page lives in a browser nobody sees
//! (`crate::web`); here its pictures are drawn, the mouse and the keys go to it, and what
//! that browser cannot show (a list to choose from, a dialog) is shown instead.
//!
//! One tab per page. Pages come and go on the engine's word (`web::told`): an agent's
//! `web open`, a window a page opened, a tab closed from the socket; a tab closed here closes
//! its page there.

use super::settings::raise;
use super::line_input::LineInput;
use super::{app_wide, App, Focus, Message};
use crate::web::{self, Input, TabInfo, TabJob, View};
use iced::widget::{button, column, container, image, mouse_area, rich_text, row, scrollable, sensor, stack, text, Space};
use iced::{mouse, Alignment, Border, Color, Element, Length, Padding, Point, Size, Task};
use iced_runtime::core::image::Allocation;
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::{Command, Document};

/// What the window keeps for its web tabs.
pub struct Tabs {
    /// The pages there are, as last told, and those this window has (or had) a tab for.
    pub(super) pages: Vec<TabInfo>,
    mine: Vec<u32>,
    shown: u64,
    /// The picture being shown, the page it is of, and what keeps it in the renderer's memory
    /// until the next one is there too.
    picture: Option<(u32, image::Handle)>,
    held: Option<Allocation>,
    /// The page area, in logical pixels, and the window's pixels per logical pixel.
    size: Size,
    scale: f32,
    /// What the engine was last asked to draw.
    asked: Option<(u32, View)>,
    /// Which option of an open list the keyboard is on; what is typed into a prompt.
    choice_at: usize,
    answer: LineInput,
    /// Pictures shown so far (for `state`).
    pub frames: u64,
}

impl Default for Tabs {
    fn default() -> Self {
        Self { pages: Vec::new(), mine: Vec::new(), shown: 0, picture: None, held: None, size: Size::ZERO, scale: 1.0, asked: None, choice_at: 0, answer: LineInput::default(), frames: 0 }
    }
}

#[derive(Debug, Clone)]
pub enum Msg {
    /// The engine has news: pages, a picture, text to copy.
    Wake,
    /// A picture is in the renderer's memory (None: it cannot say), so showing it will not blink.
    Ready(u32, image::Handle, Option<Allocation>),
    Size(Size),
    Scale(f32),
    Move(Point),
    Down(u8),
    Up(u8),
    Scroll(mouse::ScrollDelta),
    Back,
    Reload,
    /// Open the page, or the address that was not opened, in the system's browser.
    Outside,
    OutsideBlocked,
    /// Put away the note about what was not opened.
    Seen,
    /// Answer the dialog.
    Dialog(bool),
    /// Choose from the list (None: put it away).
    Choose(Option<usize>),
    Pasted(Option<String>),
}

/// Why a request that came from an agent (`nib-web`, through the daemon) is not run, if it is
/// not: an agent gets less than the socket (`nib_proto::web::AGENT`; the daemon's side refuses
/// the same, this is the second lock), and its files are on the daemon's machine.
///
/// It reaches the machine it runs on and no other: a saved connection by name (`gpu:8888`) is
/// the user's to open, with their keys, unless it is this window's own (`own_profile`).
fn agent_refused(req: &serde_json::Value, daemon_is_here: bool, label: &str, own_profile: &dyn Fn(&str) -> bool) -> Option<String> {
    let cmd = req.get("cmd").and_then(serde_json::Value::as_str).unwrap_or("");
    if let Err(why) = nib_proto::web::agent_may(cmd) {
        return Some(why);
    }
    if cmd == "web.open" {
        let url = req.get("url").and_then(serde_json::Value::as_str).unwrap_or("");
        if let Some(name) = crate::forward::names_profile(url).filter(|n| !own_profile(n)) {
            return Some(format!("open: {name} is another of the user's saved connections, and an agent opens pages of the machine it runs on ({label}) only: give the port alone, or ask the user to open {url}"));
        }
    }
    (cmd == "web.upload" && !daemon_is_here).then(|| format!("upload: the files are on {label} and the browser is on the machine nib's window is on; not supported yet"))
}

/// An agent's request as the window runs it: a picture comes back as its bytes (`inline`) and
/// is written where the agent is, by `nibd web`. The window writes no file an agent names: its
/// disk is the user's, and may be another machine's than the agent's.
fn agent_shaped(mut req: serde_json::Value) -> serde_json::Value {
    if req.get("cmd").and_then(serde_json::Value::as_str) == Some("web.shot") {
        if let Some(o) = req.as_object_mut() {
            o.remove("path");
            o.insert("inline".into(), true.into());
        }
    }
    req
}

impl App {
    /// An agent's `nib-web …`, passed on by the daemon: run as the same request from the
    /// control socket would be, less what agents are not given, and answered to the daemon.
    pub(super) fn on_agent_web(&mut self, id: u64, req: &str) -> Task<Message> {
        let Some(conn) = self.conn.as_ref() else { return Task::none() };
        let send = conn.notifier();
        let answer = move |v: serde_json::Value| send(nib_proto::ClientNotify::WebReply { id, reply: v.to_string() });
        let req = match serde_json::from_str::<serde_json::Value>(req) {
            Ok(v) if v.is_object() => v,
            _ => {
                answer(serde_json::json!({ "ok": false, "error": "not a request" }));
                return Task::none();
            }
        };
        let req = agent_shaped(req);
        crate::control::log("agent", &crate::control::loggable(&req));
        let here = matches!(self.launch.target, nib_editor::Target::Local { .. });
        // Its own machine by its saved name is the window's remote, as a port alone would be.
        let via = self.forward_via();
        let own = |name: &str| via.as_ref().is_some_and(|v| crate::forward::profile_host(name).is_ok_and(|h| h == v.host));
        if let Some(why) = agent_refused(&req, here, &self.label, &own) {
            crate::control::log("agent!", &why);
            answer(serde_json::json!({ "ok": false, "error": why }));
            return Task::none();
        }
        self.on_control(crate::control::Call::then(req, answer))
    }

    /// The page of the web tab in front, if that is what is in front.
    pub(super) fn web_front(&self) -> Option<u32> {
        if self.ai.shown() {
            return None;
        }
        self.tab().and_then(|t| t.web)
    }

    pub(super) fn web_info(&self, page: u32) -> Option<&TabInfo> {
        self.web.pages.iter().find(|p| p.id == page)
    }

    fn web_zoom(&self, page: u32) -> f32 {
        self.web_info(page).map_or(1.0, |p| p.zoom).max(0.1)
    }

    /// Zoom the page in front `steps` levels (0: back to 100%), as Cmd/Ctrl + wheel, +, - and 0
    /// do over it. Says the new zoom, as the preview's does.
    pub(super) fn web_zoom_by(&mut self, steps: i32) -> Task<Message> {
        let Some(page) = self.web_front() else { return Task::none() };
        let to = if steps == 0 { 1.0 } else { web::zoom_step(self.web_zoom(page), steps) };
        web::tab(TabJob::Zoom { page, steps });
        self.flash_info(format!("page {:.0}%", to * 100.0))
    }

    /// What a web tab is called in the tab bar.
    pub(super) fn web_label(&self, page: u32) -> String {
        self.web_info(page).map_or_else(|| "localhost".to_string(), |p| p.name.clone())
    }

    /// For `state`: the web tabs as the window has them.
    pub(super) fn web_json(&self) -> serde_json::Value {
        serde_json::json!({
            "showing": self.web_front(), "frames": self.web.frames,
            "zoom": self.web_front().map(|p| self.web_zoom(p)),
            "size": { "width": self.web.size.width, "height": self.web.size.height, "scale": self.web.scale },
            "choice": self.web_front().and_then(|p| self.web_info(p)).and_then(|p| p.choice.as_ref()).map(|c| serde_json::json!({ "options": c.options, "at": self.web.choice_at })),
        })
    }

    /// Bring the tabs in line with the pages there are, and the engine in line with which tab
    /// is in front. Cheap; run after anything that may have changed either.
    pub(super) fn web_follow(&mut self) -> Task<Message> {
        if self.headless || (self.web.mine.is_empty() && self.web.pages.is_empty() && self.web.asked.is_none()) {
            return Task::none();
        }
        let mut task = Task::none();
        // A page with no tab: a new one gets its tab; one whose tab the user closed is closed.
        let pages: Vec<u32> = self.web.pages.iter().map(|p| p.id).collect();
        for id in &pages {
            let has_tab = self.tabs.iter().any(|t| t.web == Some(*id));
            if !has_tab && !self.web.mine.contains(id) {
                let mut doc = Document::new_plain(Default::default(), None);
                doc.read_only = true;
                self.put_doc(doc, true);
                self.tabs[self.active].web = Some(*id);
                self.web.mine.push(*id);
                self.ai.front = false;
                self.focus = Focus::Main;
                task = iced::window::latest().and_then(iced::window::scale_factor).map(|s| Message::Web(Msg::Scale(s)));
            } else if !has_tab {
                web::tab(TabJob::Close(*id));
                self.web.pages.retain(|p| p.id != *id);
            }
        }
        // A tab whose page is gone goes too.
        self.web.mine.retain(|id| pages.contains(id));
        while let Some(i) = self.tabs.iter().position(|t| t.web.is_some_and(|id| !self.web.pages.iter().any(|p| p.id == id))) {
            let back = self.active;
            self.active = i;
            let _ = self.close_tab();
            if i < back {
                self.active = back - 1;
            } else if back < self.tabs.len() && i != back {
                self.active = back;
            }
        }
        // The engine draws the page in front, at the size it has here.
        let want = self.web_front().filter(|_| self.web.size.width >= 1.0 && self.web.size.height >= 1.0).map(|page| {
            (page, View { width: self.web.size.width as u32, height: self.web.size.height as u32, scale: self.web.scale })
        });
        if want != self.web.asked {
            match want {
                Some((page, view)) => web::tab(TabJob::View { page, view }),
                None => web::tab(TabJob::Hide),
            }
            self.web.asked = want;
        }
        task
    }

    pub(super) fn on_web_msg(&mut self, m: Msg) -> Task<Message> {
        let Some(page) = self.web_front() else {
            // Nothing of a page is in front: only the engine's news matters.
            return match m {
                Msg::Wake => self.web_wake(),
                Msg::Scale(s) => {
                    self.web.scale = s;
                    Task::none()
                }
                _ => Task::none(),
            };
        };
        match m {
            Msg::Wake => return self.web_wake(),
            Msg::Ready(of, handle, held) => {
                self.web.picture = Some((of, handle));
                self.web.held = held;
                self.web.frames += 1;
            }
            Msg::Size(s) => self.web.size = s,
            Msg::Scale(s) => self.web.scale = s,
            Msg::Move(p) => {
                // A pixel of the tab is 1 / zoom pixels of the page.
                let z = self.web_zoom(page);
                web::tab(TabJob::Input { page, input: Input::Move { x: p.x / z, y: p.y / z } });
            }
            Msg::Down(button) => {
                self.focus = Focus::Main;
                let mods = self.web_mods();
                web::tab(TabJob::Input { page, input: Input::Down { button, mods } });
            }
            Msg::Up(button) => web::tab(TabJob::Input { page, input: Input::Up { button } }),
            Msg::Scroll(d) => {
                // iced: positive is content moving down / right; the page wants the opposite.
                let (dx, dy) = match d {
                    mouse::ScrollDelta::Lines { x, y } => (-x * 40.0, -y * 40.0),
                    mouse::ScrollDelta::Pixels { x, y } => (-x, -y),
                };
                web::tab(TabJob::Input { page, input: Input::Wheel { dx, dy } });
            }
            Msg::Back => web::tab(TabJob::Back(page)),
            Msg::Reload => web::tab(TabJob::Reload(page)),
            Msg::Outside => {
                if let Some(p) = self.web_info(page) {
                    super::viewers::open_external(&p.url);
                }
            }
            // Only this click, a person's, ever takes an outside address to a browser.
            Msg::OutsideBlocked => {
                if let Some(url) = self.web_info(page).and_then(|p| p.blocked.clone()) {
                    super::viewers::open_external(&url);
                }
                web::tab(TabJob::Seen(page));
            }
            Msg::Seen => web::tab(TabJob::Seen(page)),
            Msg::Dialog(accept) => {
                let text = Some(std::mem::take(&mut self.web.answer).to_string()).filter(|_| self.web_info(page).and_then(|p| p.dialog.as_ref()).is_some_and(|d| d["type"] == "prompt"));
                web::tab(TabJob::Dialog { page, accept, text });
            }
            Msg::Choose(index) => web::tab(TabJob::Choose { page, index }),
            Msg::Pasted(text) => {
                if let Some(text) = text.filter(|t| !t.is_empty()) {
                    web::tab(TabJob::Paste { page, text });
                }
            }
        }
        Task::none()
    }

    /// The engine woke the window: take its news.
    fn web_wake(&mut self) -> Task<Message> {
        let told = web::told();
        let mut tasks = Vec::new();
        // A page that just got a dialog or a list starts with a fresh answer.
        for p in &told.tabs {
            let before = self.web.pages.iter().find(|o| o.id == p.id);
            if p.dialog.is_some() && before.is_none_or(|o| o.dialog.is_none()) {
                self.web.answer.set(p.dialog.as_ref().and_then(|d| d["default"].as_str()).unwrap_or(""));
            }
            if p.choice.is_some() && before.is_none_or(|o| o.choice.is_none()) {
                self.web.choice_at = p.choice.as_ref().and_then(|c| c.selected).unwrap_or(0);
            }
            for file in &p.saved {
                tasks.push(self.flash(format!("{} saved {file} to Downloads", p.name), false));
            }
        }
        // A page that is gone takes with it the forward made for it.
        let port = |p: &TabInfo| crate::forward::port_of(&p.url);
        for gone in self.web.pages.iter().filter(|o| !told.tabs.iter().any(|p| p.id == o.id)).filter_map(port) {
            if !told.tabs.iter().any(|p| port(p) == Some(gone)) {
                crate::forward::tab_closed(gone);
            }
        }
        for (text, warn) in crate::forward::take_notes() {
            tasks.push(self.flash(text, warn));
        }
        self.web.pages = told.tabs;
        // A page that was asked for comes forward.
        if told.shown != self.web.shown {
            self.web.shown = told.shown;
            if let Some(i) = told.front.and_then(|id| self.tabs.iter().position(|t| t.web == Some(id))) {
                self.active = i;
                self.ai.front = false;
                self.focus = Focus::Main;
            }
        }
        if let Some(text) = told.copied {
            tasks.push(iced::clipboard::write(text));
        }
        if let Some(p) = web::take_picture() {
            // Shown only once the renderer has it: a picture it is still loading is drawn as
            // nothing, which would blink whenever the page changed.
            let handle = image::Handle::from_rgba(p.width, p.height, p.rgba);
            let ready = handle.clone();
            tasks.push(iced_runtime::image::allocate(handle).map(move |r| Message::Web(Msg::Ready(p.page, ready.clone(), r.ok()))));
        }
        Task::batch(tasks)
    }

    /// The modifier keys held now, as the page's keys.
    fn web_mods(&self) -> Vec<char> {
        let m = self.mods;
        [(m.control(), web::key::CONTROL), (m.alt(), web::key::ALT), (m.shift(), web::key::SHIFT), (m.logo(), web::key::META)].into_iter().filter(|(held, _)| *held).map(|(_, k)| k).collect()
    }

    /// A key with a web tab in front: nib keeps its app-wide commands, a dialog or list of
    /// nib's own takes what answers it, and the page gets the rest.
    pub(super) fn web_key(&mut self, chord: KeyChord, text: Option<String>) -> Option<Task<Message>> {
        let page = self.web_front()?;
        let command = self.keymap.lookup(&chord);
        if command.is_some_and(app_wide) {
            return None;
        }
        let info = self.web_info(page)?;
        if let Some(d) = &info.dialog {
            let prompt = d["type"] == "prompt";
            match chord.key {
                Key::Enter => return Some(self.on_web_msg(Msg::Dialog(true))),
                Key::Esc => return Some(self.on_web_msg(Msg::Dialog(false))),
                _ if prompt => drop(self.web.answer.key(&chord, text.as_deref())),
                _ => {}
            }
            return Some(Task::none());
        }
        if let Some(c) = &info.choice {
            let last = c.options.len().saturating_sub(1);
            match chord.key {
                Key::Up => self.web.choice_at = self.web.choice_at.saturating_sub(1),
                Key::Down => self.web.choice_at = (self.web.choice_at + 1).min(last),
                Key::Home => self.web.choice_at = 0,
                Key::End => self.web.choice_at = last,
                Key::Enter => return Some(self.on_web_msg(Msg::Choose(Some(self.web.choice_at)))),
                Key::Esc | Key::Tab => return Some(self.on_web_msg(Msg::Choose(None))),
                _ => {}
            }
            return Some(Task::none());
        }
        // The clipboard is the system's, which only nib can reach: the page's own copy and
        // paste keys would use the unseen browser's.
        // Cmd/Ctrl 0: the page back to 100%, as in a browser (+ and - are app-wide commands:
        // `run_command` zooms the page with them when a web tab is in front).
        if command == Some(Command::FontReset) {
            return Some(self.web_zoom_by(0));
        }
        match command {
            Some(Command::Copy) => {
                web::tab(TabJob::Copy { page, cut: false });
                return Some(Task::none());
            }
            Some(Command::Cut) => {
                web::tab(TabJob::Copy { page, cut: true });
                return Some(Task::none());
            }
            Some(Command::Paste) => return Some(iced::clipboard::read().map(|t| Message::Web(Msg::Pasted(t)))),
            _ => {}
        }
        let key = match chord.key {
            Key::Char(c) => text.as_deref().and_then(|t| t.chars().next()).filter(|_| !chord.ctrl && !chord.logo).unwrap_or(c),
            Key::Up => web::key::UP,
            Key::Down => web::key::DOWN,
            Key::Left => web::key::LEFT,
            Key::Right => web::key::RIGHT,
            Key::Home => web::key::HOME,
            Key::End => web::key::END,
            Key::PageUp => web::key::PAGE_UP,
            Key::PageDown => web::key::PAGE_DOWN,
            Key::Tab => web::key::TAB,
            Key::Enter => web::key::ENTER,
            Key::Backspace => web::key::BACKSPACE,
            Key::Delete => web::key::DELETE,
            Key::Insert => web::key::INSERT,
            Key::Esc => web::key::ESCAPE,
            Key::F(n) => char::from_u32(web::key::F1 as u32 + (n as u32).saturating_sub(1))?,
        };
        let mods: Vec<char> = [(chord.ctrl, web::key::CONTROL), (chord.alt, web::key::ALT), (chord.shift, web::key::SHIFT), (chord.logo, web::key::META)]
            .into_iter()
            .filter(|(held, _)| *held)
            .map(|(_, k)| k)
            .collect();
        web::tab(TabJob::Input { page, input: Input::Keys(web::press(&mods, key)) });
        Some(Task::none())
    }

    fn web_button<'a>(&self, label: &'a str, msg: Msg) -> Element<'a, Message> {
        let th = &self.theme;
        let (border, hover) = (raise(th.panel, 0.16), raise(th.panel, 0.08));
        button(text(label).size(11).color(th.foreground))
            .on_press(Message::Web(msg))
            .padding([3, 10])
            .style(move |_, st| button::Style {
                background: matches!(st, button::Status::Hovered).then(|| hover.into()),
                border: Border { color: border, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            })
            .into()
    }

    pub(super) fn web_view(&self, page: u32) -> Element<'_, Message> {
        let th = &self.theme;
        let dim = th.dim;
        let Some(info) = self.web_info(page) else {
            return container(text("This page was closed.").size(12).color(dim)).center(Length::Fill).into();
        };
        let mut bar = row![text(info.name.as_str()).size(12).color(th.foreground), text(info.address.as_str()).size(12).color(dim)].spacing(10).padding([4, 10]).align_y(Alignment::Center);
        if info.loading {
            bar = bar.push(text("loading…").size(11).color(dim));
        }
        bar = bar
            .push(Space::new().width(Length::Fill))
            .push(self.web_button("Back", Msg::Back))
            .push(self.web_button("Reload", Msg::Reload))
            .push(self.web_button("Open in browser", Msg::Outside));
        let mut top = column![bar];
        // What the page tried to show from another machine: never opened, only offered.
        if let Some(url) = &info.blocked {
            let shown: String = url.chars().take(90).collect();
            top = top.push(
                container(
                    row![
                        text(format!("Not opened (not this machine): {shown}")).size(11).color(th.warn),
                        Space::new().width(Length::Fill),
                        self.web_button("Open in browser", Msg::OutsideBlocked),
                        self.web_button("Dismiss", Msg::Seen),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding([3, 10]),
            );
        }
        let picture: Element<'_, Message> = match &self.web.picture {
            Some((of, h)) if *of == page => image(h.clone()).width(Length::Fill).height(Length::Fill).content_fit(iced::ContentFit::Fill).into(),
            _ => container(text("Loading the page…").size(12).color(dim)).center(Length::Fill).into(),
        };
        let shape = match info.cursor.as_str() {
            "pointer" => mouse::Interaction::Pointer,
            "text" | "vertical-text" => mouse::Interaction::Text,
            "grab" => mouse::Interaction::Grab,
            "grabbing" | "move" | "all-scroll" => mouse::Interaction::Grabbing,
            "crosshair" => mouse::Interaction::Crosshair,
            "not-allowed" | "no-drop" => mouse::Interaction::NotAllowed,
            "col-resize" | "ew-resize" | "e-resize" | "w-resize" => mouse::Interaction::ResizingHorizontally,
            "row-resize" | "ns-resize" | "n-resize" | "s-resize" => mouse::Interaction::ResizingVertically,
            _ => mouse::Interaction::default(),
        };
        let area = mouse_area(container(picture).width(Length::Fill).height(Length::Fill))
            .on_move(|p| Message::Web(Msg::Move(p)))
            .on_press(Message::Web(Msg::Down(0)))
            .on_release(Message::Web(Msg::Up(0)))
            .on_middle_press(Message::Web(Msg::Down(1)))
            .on_middle_release(Message::Web(Msg::Up(1)))
            .on_right_press(Message::Web(Msg::Down(2)))
            .on_right_release(Message::Web(Msg::Up(2)))
            .on_scroll(|d| Message::Web(Msg::Scroll(d)))
            .interaction(shape);
        let area = crate::zoom::ZoomZone::new(crate::zoom::Zone::Web, area);
        let mut layers = stack![sensor(area).on_show(|s| Message::Web(Msg::Size(s))).on_resize(|s| Message::Web(Msg::Size(s)))];
        // The page's CSS pixels as the tab's.
        let z = info.zoom.max(0.1);
        let (panel, edge, fg, sel) = (th.panel, raise(th.panel, 0.2), th.foreground, th.browser_sel_bg);
        let boxed = move |_: &iced::Theme| container::Style { background: Some(panel.into()), border: Border { color: edge, width: 1.0, radius: 5.0.into() }, ..Default::default() };
        if let Some(c) = &info.choice {
            // nib's own list for a <select>, under it; a press anywhere else puts it away.
            let at = self.web.choice_at;
            let options = column(c.options.iter().enumerate().map(|(i, o)| {
                let on = i == at;
                button(text(o.as_str()).size(12).color(fg))
                    .on_press(Message::Web(Msg::Choose(Some(i))))
                    .padding([3, 10])
                    .width(Length::Fill)
                    .style(move |_, st| button::Style { background: (on || matches!(st, button::Status::Hovered)).then(|| sel.into()), ..Default::default() })
                    .into()
            }));
            let list = container(scrollable(options).style(crate::theme::scrollbar)).width(Length::Fixed((c.width * z).max(140.0))).max_height(260.0).style(boxed);
            let below = ((c.y + c.height) * z + 2.0).clamp(0.0, (self.web.size.height - 60.0).max(0.0));
            layers = layers.push(
                mouse_area(container(list).padding(Padding { left: (c.x * z).clamp(0.0, (self.web.size.width - 150.0).max(0.0)), top: below, ..Padding::ZERO }).width(Length::Fill).height(Length::Fill))
                    .on_press(Message::Web(Msg::Choose(None))),
            );
        }
        if let Some(d) = &info.dialog {
            // The page's alert / confirm / prompt, asked here; the page waits for the answer.
            let kind = d["type"].as_str().unwrap_or("alert");
            let mut body = column![text(format!("{} says", info.name)).size(11).color(dim), text(d["message"].as_str().unwrap_or("").to_owned()).size(13).color(fg)].spacing(8);
            if kind == "prompt" {
                let typed = rich_text(self.web.answer.spans(fg, fg)).size(13);
                body = body.push(container(typed).padding([4, 8]).width(Length::Fill).style(move |_| container::Style { border: Border { color: edge, width: 1.0, radius: 3.0.into() }, ..Default::default() }));
            }
            let mut answers = row![Space::new().width(Length::Fill)].spacing(8);
            if kind != "alert" {
                answers = answers.push(self.web_button(if kind == "beforeunload" { "Stay" } else { "Cancel  esc" }, Msg::Dialog(false)));
            }
            answers = answers.push(self.web_button(if kind == "beforeunload" { "Leave" } else { "OK  ↵" }, Msg::Dialog(true)));
            let card = container(body.push(answers)).padding(16).width(Length::Fixed(420.0)).style(boxed);
            let veil = Color { a: 0.45, ..Color::BLACK };
            layers = layers.push(mouse_area(container(card).center(Length::Fill).style(move |_| container::Style { background: Some(veil.into()), ..Default::default() })).on_press(Message::Web(Msg::Wake)));
        }
        column![top, layers].into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;
    use serde_json::json;

    #[test]
    fn an_agents_request_is_refused_what_the_socket_alone_is_given() {
        // Here too, whatever the daemon let through.
        for cmd in ["web.eval", "web.pointer", "web.quit"] {
            assert!(agent_refused(&json!({ "cmd": cmd, "js": "1" }), true, "local", &|_| false).unwrap().contains("not for agents"), "{cmd}");
        }
        // Nothing but the web tab's requests comes this way.
        for cmd in ["run", "state", "type", "ssh.connect", "serve.down", "forward.add", "web.dance", ""] {
            assert!(agent_refused(&json!({ "cmd": cmd }), true, "local", &|_| false).is_some(), "{cmd}");
        }
        assert!(agent_refused(&json!({}), true, "local", &|_| false).is_some());
        for cmd in ["web.open", "web.read", "web.click", "web.shot", "web.wait", "web.upload", "web.close"] {
            assert_eq!(agent_refused(&json!({ "cmd": cmd }), true, "local", &|_| false), None, "{cmd}");
        }
        // An agent on another machine has its files there, the browser is here.
        assert!(agent_refused(&json!({ "cmd": "web.upload", "ref": "1", "files": ["/a"] }), false, "@gpu", &|_| false).unwrap().contains("@gpu"));
        assert_eq!(agent_refused(&json!({ "cmd": "web.read" }), false, "@gpu", &|_| false), None);
    }

    #[test]
    fn an_agent_opens_pages_of_its_own_machine_and_writes_no_file_on_the_windows() {
        // A port, or an address of this machine, as ever.
        for url in ["8765", "localhost:8765/x", "http://127.0.0.1:8765/?a=1"] {
            assert_eq!(agent_refused(&json!({ "cmd": "web.open", "url": url }), true, "local", &|_| false), None, "{url}");
        }
        // Another saved connection is the user's to reach: its keys, its loopback services.
        let why = agent_refused(&json!({ "cmd": "web.open", "url": "gpu:8888/lab" }), true, "local", &|_| false).unwrap();
        assert!(why.contains("gpu is another of the user's saved connections"), "{why}");
        assert!(agent_refused(&json!({ "cmd": "web.open", "url": "prod:5432" }), false, "@gpu", &|n| n == "gpu").is_some());
        // The window's own machine by its saved name is where the agent runs.
        assert_eq!(agent_refused(&json!({ "cmd": "web.open", "url": "gpu:8888/lab" }), false, "@gpu", &|n| n == "gpu"), None);
        // A picture comes back as bytes: no path of the agent's choosing is written here.
        let shot = agent_shaped(json!({ "cmd": "web.shot", "path": "/Users/ada/.ssh/authorized_keys", "full": true }));
        assert_eq!(shot, json!({ "cmd": "web.shot", "full": true, "inline": true }));
        assert_eq!(agent_shaped(json!({ "cmd": "web.shot", "inline": false })), json!({ "cmd": "web.shot", "inline": true }));
        let other = json!({ "cmd": "web.upload", "ref": "1", "files": ["/a"], "path": "x" });
        assert_eq!(agent_shaped(other.clone()), other);
    }

    #[test]
    fn over_a_web_tab_the_zoom_keys_and_wheel_zoom_the_page() {
        use crate::zoom::Zone;
        let mut a = app();
        let mut doc = Document::new_plain(Default::default(), None);
        doc.read_only = true;
        a.put_doc(doc, true);
        let active = a.active;
        a.tabs[active].web = Some(7);
        a.web.pages.push(TabInfo {
            id: 7, name: "app".into(), address: "localhost:5173".into(), url: "http://127.0.0.1:5173/".into(), loading: false, blocked: None,
            dialog: None, choice: None, cursor: String::new(), saved: Vec::new(), zoom: 1.0,
        });
        a.focus = Focus::Main;
        let size = a.face.size;
        let flash = |a: &App| a.flash.as_ref().map(|f| f.0.clone());
        let press = |a: &mut App, s: &str| {
            let _ = a.update(Message::Key { chord: KeyChord::parse(s).unwrap(), text: None, repeat: false });
            let _ = a.update(Message::KeyUp(KeyChord::parse(s).unwrap().key));
        };
        press(&mut a, "mod+-");
        assert_eq!((a.face.size, flash(&a)), (size, Some("page 90%".into())));
        let _ = a.update(Message::WheelZoom(Some(Zone::Web), 2));
        assert_eq!((a.face.size, flash(&a)), (size, Some("page 125%".into())));
        press(&mut a, "mod+0");
        assert_eq!(flash(&a), Some("page 100%".into()));
        // Not over the page (a terminal has the keys): the code font, as before.
        a.focus = Focus::Terminal(1);
        let _ = a.update(Message::Control(Command::FontBigger));
        assert_eq!(a.face.size, size + 1.0);
    }

    #[test]
    fn a_refused_request_of_an_agent_runs_nothing() {
        let mut a = app();
        a.headless = true;
        let before = (a.terms.len(), a.pending.len());
        // What would open a terminal, or quit, had it come from the socket.
        for req in [r#"{"cmd":"run","command":"new_terminal"}"#, r#"{"cmd":"web.eval","js":"1"}"#, r#"{"cmd":"web.pointer","x":1,"y":1}"#, "junk", "[]"] {
            let _ = a.on_agent_web(1, req);
        }
        assert_eq!((a.terms.len(), a.pending.len()), before);
    }
}
