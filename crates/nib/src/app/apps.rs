//! The Apps panel (toolbar button, `toggle_apps`): the project's apps, as registered in
//! `$NIBDIR/apps.json` (`serve.rs` keeps them in line with the daemon).
//!
//! One row an app: Open starts its servers and shows its page (or brings the page forward when
//! it runs); Stop stops them; Restart, Edit and Remove are under the row. An app somebody else
//! defined (an agent, `nib ctl`) or changed waits for the user: Open first shows its exact
//! commands, and only the user's own click or key approves them (never a control client's, see
//! `App::ctl_input`). New apps come from a small form, or from an agent: "Register via AI chat"
//! opens a new chat with the request already written (and the nib-app skill named in it), for
//! the user to finish. "Open address" shows a page of this machine nobody registered.

use super::{App, Focus, Message};
use crate::control::Call;
use iced::widget::{button, column, container, mouse_area, rich_text, row, scrollable, space, span, text};
use iced::{Alignment, Border, Color, Element, Font, Length, Padding, Task};
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::Command;
use nib_proto::{AppDef, AppInfo, AppOp, AppPage, Response, RpcError, ServerDef, ServerState};
use serde_json::json;
use std::collections::BTreeSet;

const WIDTH: f32 = 460.0;
/// The command key, as a hint shows it before another key.
const MOD_PLUS: &str = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" };

/// What the user writes to an agent to have an app registered: the skill first, so its
/// instructions go in front of the message whatever the assistant.
pub(super) const ASK_AI: &str = "/nib-app I want to register a new app for this repo with nib-app. Here are my instructions: ";

/// What the keyboard walks, in the order the panel shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Add,
    AskAi,
    App(usize),
    Address,
}

#[derive(Default)]
pub(super) struct Panel {
    pub(super) open: bool,
    sel: usize,
    return_to: Option<Focus>,
    /// Apps whose servers are shown under them.
    expanded: BTreeSet<String>,
    /// Remove was pressed once on this app: once more removes it.
    removing: Option<String>,
    /// Open on an app that waits for approval: its commands are shown; Run approves them.
    approving: Option<String>,
    /// "Open address": a port or a local address.
    address: String,
    form: Option<Form>,
    /// The form was sent to the daemon: its answer closes the form, or says what is wrong.
    saving: bool,
}

/// The add / edit form: the app, then each server's fields.
#[derive(Debug, Clone, Default)]
struct Form {
    /// Editing this app (`None`: a new one).
    orig: Option<String>,
    name: String,
    title: String,
    servers: Vec<FormServer>,
    /// The field with the keyboard (see `Form::fields`).
    sel: usize,
    error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct FormServer {
    name: String,
    cmd: String,
    port: String,
    /// Non-empty on the one server that shows the app's page.
    page: String,
    after: String,
    cwd: String,
    /// `K=V; K=V`
    env: String,
}

/// A server's fields: label, placeholder.
const SERVER_FIELDS: [(&str, &str); 7] = [
    ("server", "api, web… (a-z 0-9 _ -)"),
    ("command", "uv run app serve --port 8765"),
    ("port", "8765 (how nib knows it is ready)"),
    ("page", "/ — show this server's page (one per app)"),
    ("after", "servers that answer first: api,db"),
    ("folder", "inside the project (empty: its root)"),
    ("env", "NAME=value; OTHER=value"),
];

impl Form {
    fn fields(&self) -> usize {
        2 + self.servers.len() * SERVER_FIELDS.len()
    }

    fn field_mut(&mut self, i: usize) -> Option<&mut String> {
        match i {
            0 => Some(&mut self.name),
            1 => Some(&mut self.title),
            i => {
                let (s, f) = ((i - 2) / SERVER_FIELDS.len(), (i - 2) % SERVER_FIELDS.len());
                let s = self.servers.get_mut(s)?;
                Some(match f {
                    0 => &mut s.name,
                    1 => &mut s.cmd,
                    2 => &mut s.port,
                    3 => &mut s.page,
                    4 => &mut s.after,
                    5 => &mut s.cwd,
                    _ => &mut s.env,
                })
            }
        }
    }

    fn from_app(a: &AppInfo) -> Self {
        let servers = a
            .def
            .servers
            .iter()
            .map(|(n, d)| FormServer {
                name: n.clone(),
                cmd: d.cmd.clone(),
                port: d.port.map(|p| p.to_string()).unwrap_or_default(),
                page: a.def.page.as_ref().filter(|p| p.server == *n).map(|p| p.path.clone()).unwrap_or_default(),
                after: d.after.join(","),
                cwd: d.cwd.clone().unwrap_or_default(),
                env: d.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "),
            })
            .collect();
        Form { orig: Some(a.name.clone()), name: a.name.clone(), title: a.def.title.clone().unwrap_or_default(), servers, sel: 0, error: None }
    }

    /// The app it describes. What only the daemon can check (names, ports, `after`) is left to
    /// it; this says what the form itself cannot express.
    fn to_def(&self) -> Result<(String, AppDef), String> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err("the app needs a name".into());
        }
        let mut def = AppDef { title: Some(self.title.trim().to_string()).filter(|t| !t.is_empty()), ..Default::default() };
        let pages: Vec<&FormServer> = self.servers.iter().filter(|s| !s.page.trim().is_empty()).collect();
        if pages.len() > 1 {
            return Err("only one server shows the app's page: leave the others' page empty".into());
        }
        for (i, s) in self.servers.iter().enumerate() {
            let sname = s.name.trim().to_string();
            if sname.is_empty() {
                return Err(format!("server {} needs a name", i + 1));
            }
            let port = match s.port.trim() {
                "" => None,
                p => Some(p.parse::<u16>().map_err(|_| format!("{sname}: port {p:?} is not a number from 1 to 65535"))?),
            };
            let mut env = std::collections::BTreeMap::new();
            for pair in s.env.split(';').map(str::trim).filter(|p| !p.is_empty()) {
                let (k, v) = pair.split_once('=').ok_or_else(|| format!("{sname}: env {pair:?}: write NAME=value"))?;
                env.insert(k.trim().to_string(), v.trim().to_string());
            }
            let d = ServerDef {
                cmd: s.cmd.trim().to_string(),
                port,
                after: s.after.split(',').map(str::trim).filter(|a| !a.is_empty()).map(str::to_string).collect(),
                cwd: Some(s.cwd.trim().to_string()).filter(|c| !c.is_empty()),
                env,
            };
            if def.servers.insert(sname.clone(), d).is_some() {
                return Err(format!("two servers are called {sname}"));
            }
        }
        if let Some(p) = pages.first() {
            def.page = Some(AppPage { server: p.name.trim().to_string(), path: p.page.trim().to_string() });
        }
        Ok((name, def))
    }

    /// What is filled in so far, for an agent to finish.
    fn so_far(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.name.trim().is_empty() {
            let title = if self.title.trim().is_empty() { String::new() } else { format!(" ({})", self.title.trim()) };
            parts.push(format!("app `{}`{title}", self.name.trim()));
        }
        for s in self.servers.iter().filter(|s| !s.cmd.trim().is_empty() || !s.name.trim().is_empty()) {
            let mut d = format!("server {}", if s.name.trim().is_empty() { "?" } else { s.name.trim() });
            if !s.cmd.trim().is_empty() {
                d.push_str(&format!(": `{}`", s.cmd.trim()));
            }
            if !s.port.trim().is_empty() {
                d.push_str(&format!(" on port {}", s.port.trim()));
            }
            if !s.page.trim().is_empty() {
                d.push_str(&format!(", page {}", s.page.trim()));
            }
            parts.push(d);
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

#[derive(Debug, Clone)]
pub enum Msg {
    Toggle,
    Close,
    Focus,
    /// A row of the list was clicked (its index in `apps_rows`).
    Click(usize),
    Open(String),
    Stop(String),
    Restart(String),
    Edit(String),
    Remove(String),
    /// Run what was shown: the app's name and the hash of the definition shown.
    Approve(String, String),
    CancelApprove,
    Add,
    AskAi,
    AddressGo,
    Field(usize),
    AddServer,
    DropServer(usize),
    Save,
    CancelForm,
    Pasted(Option<String>),
}

impl App {
    pub(super) fn on_apps_msg(&mut self, m: Msg) -> Task<Message> {
        match m {
            Msg::Toggle if self.apps.open => self.apps_close(),
            Msg::Toggle => return self.apps_open(),
            Msg::Close => self.apps_close(),
            Msg::Focus => self.apps_focus(),
            Msg::Click(i) => {
                self.apps_focus();
                self.apps.sel = i;
                if let Some(Row::App(j)) = self.apps_rows().get(i).copied() {
                    if let Some(name) = self.serve.list.get(j).map(|a| a.name.clone()) {
                        if !self.apps.expanded.remove(&name) {
                            self.apps.expanded.insert(name);
                        }
                    }
                }
            }
            Msg::Open(name) => return self.apps_open_app(&name),
            Msg::Stop(name) => return self.app_op(AppOp::Down { names: vec![name] }),
            Msg::Restart(name) => return self.app_op(AppOp::Restart { name, server: None }),
            Msg::Edit(name) => {
                if let Some(a) = self.serve.get(&name) {
                    self.apps.form = Some(Form::from_app(a));
                    self.apps_focus();
                }
            }
            Msg::Remove(name) => return self.apps_remove(name),
            Msg::Approve(name, hash) => return self.apps_approve(name, hash),
            Msg::CancelApprove => self.apps.approving = None,
            Msg::Add => {
                self.apps.form = Some(Form { servers: vec![FormServer::default()], ..Default::default() });
                self.apps_focus();
            }
            Msg::AskAi => return self.apps_ask_ai(),
            Msg::AddressGo => self.apps_address_go(),
            Msg::Field(i) => {
                self.apps_focus();
                if let Some(f) = &mut self.apps.form {
                    f.sel = i.min(f.fields() - 1);
                }
            }
            Msg::AddServer => {
                if let Some(f) = &mut self.apps.form {
                    f.servers.push(FormServer::default());
                    f.sel = 2 + (f.servers.len() - 1) * SERVER_FIELDS.len();
                }
            }
            Msg::DropServer(i) => {
                if let Some(f) = &mut self.apps.form {
                    if f.servers.len() > 1 && i < f.servers.len() {
                        f.servers.remove(i);
                        f.sel = f.sel.min(f.fields() - 1);
                    }
                }
            }
            Msg::Save => return self.apps_save(),
            Msg::CancelForm => {
                self.apps.form = None;
                self.apps.saving = false;
            }
            Msg::Pasted(s) => {
                let line = s.unwrap_or_default().trim().split(['\n', '\r']).next().unwrap_or("").to_owned();
                match &mut self.apps.form {
                    Some(f) => {
                        let sel = f.sel;
                        if let Some(v) = f.field_mut(sel) {
                            v.push_str(&line);
                        }
                    }
                    None => self.apps.address.push_str(&line),
                }
            }
        }
        Task::none()
    }

    /// Show the panel (in the Git / SSH / Settings spot), with the list as the daemon has it now.
    fn apps_open(&mut self) -> Task<Message> {
        self.git_close();
        self.ssh_close();
        self.settings.menu = false;
        self.settings.page = None;
        self.settings.recording = None;
        self.apps.open = true;
        self.apps.sel = 0;
        self.apps_focus();
        self.app_ask(AppOp::List, None, true, true)
    }

    fn apps_focus(&mut self) {
        if self.focus != Focus::Apps {
            self.apps.return_to = Some(self.focus).filter(|f| !matches!(f, Focus::Apps | Focus::Toolbar));
            self.focus = Focus::Apps;
        }
    }

    /// Hide the panel; the keyboard goes back to where it was.
    pub(super) fn apps_close(&mut self) {
        self.apps.open = false;
        self.apps.removing = None;
        self.apps.approving = None;
        self.apps.form = None;
        self.apps.saving = false;
        if self.focus != Focus::Apps {
            return;
        }
        self.focus = match self.apps.return_to.take() {
            Some(Focus::Terminal(k)) if self.term_shown(k) => Focus::Terminal(k),
            Some(Focus::Tree) if self.tree.open => Focus::Tree,
            Some(Focus::Ai) if self.ai.open => Focus::Ai,
            _ => Focus::Main,
        };
    }

    /// App `name` is gone: nothing of the panel's points at it.
    pub(super) fn apps_forget(&mut self, name: &str) {
        self.apps.expanded.remove(name);
        if self.apps.approving.as_deref() == Some(name) {
            self.apps.approving = None;
        }
        if self.apps.removing.as_deref() == Some(name) {
            self.apps.removing = None;
        }
        self.apps_clamp();
    }

    /// The list changed: the keyboard stays on a row that is there.
    pub(super) fn apps_clamp(&mut self) {
        self.apps.sel = self.apps.sel.min(self.apps_rows().len() - 1);
    }

    fn apps_rows(&self) -> Vec<Row> {
        let mut r = vec![Row::Add, Row::AskAi];
        r.extend((0..self.serve.list.len()).map(Row::App));
        r.push(Row::Address);
        r
    }

    /// The daemon answered something the panel asked: the form closes, or says what is wrong.
    pub(super) fn apps_answer(&mut self, result: &Result<Response, RpcError>) -> bool {
        if !std::mem::take(&mut self.apps.saving) {
            return false;
        }
        match (result, &mut self.apps.form) {
            (Err(e), Some(f)) => f.error = Some(e.msg.clone()),
            (Ok(_), _) => self.apps.form = None,
            _ => {}
        }
        true
    }

    /// Open: an approved app starts (its page comes forward when it answers), or comes forward
    /// if it runs; one that waits for approval shows what it would run first.
    fn apps_open_app(&mut self, name: &str) -> Task<Message> {
        let Some(a) = self.serve.get(name) else { return Task::none() };
        if !a.approved {
            self.apps.approving = Some(name.to_string());
            self.apps.removing = None;
            return Task::none();
        }
        let page_ready = a.def.page.as_ref().is_some_and(|p| a.servers.iter().any(|s| s.name == p.server && s.state == ServerState::Ready));
        let everything_runs = a.servers.iter().all(|s| s.state == ServerState::Ready || s.state.coming());
        if page_ready && everything_runs {
            let (Some(p), Some(port)) = (a.def.page.clone(), a.def.page.as_ref().and_then(|p| a.def.servers.get(&p.server)).and_then(|d| d.port)) else { return Task::none() };
            self.web_open(Call::internal(json!({ "cmd": "web.open", "url": format!("http://127.0.0.1:{port}{}", p.path) })));
            return Task::none();
        }
        if !self.serve.opening.iter().any(|n| n == name) {
            self.serve.opening.push(name.to_string());
        }
        self.app_op(AppOp::Up { names: vec![name.to_string()] })
    }

    /// Run what the user was shown. Only the user's own click or key: a control client driving
    /// the window cannot approve for them.
    fn apps_approve(&mut self, name: String, hash: String) -> Task<Message> {
        if self.ctl_input {
            return self.flash_warn("only the user approves an app: not through nib ctl");
        }
        self.apps.approving = None;
        if !self.serve.opening.contains(&name) {
            self.serve.opening.push(name.clone());
        }
        // The daemon takes them in order: approved, then started.
        let approve = self.app_op(AppOp::Approve { name: name.clone(), hash });
        let up = self.app_op(AppOp::Up { names: vec![name] });
        Task::batch([approve, up])
    }

    /// Remove asks once (the row says so), and removes on the second press.
    fn apps_remove(&mut self, name: String) -> Task<Message> {
        if self.apps.removing.as_deref() == Some(name.as_str()) {
            self.apps.removing = None;
            return self.app_op(AppOp::Remove { name });
        }
        self.apps.removing = Some(name);
        Task::none()
    }

    fn apps_save(&mut self) -> Task<Message> {
        let Some(f) = &mut self.apps.form else { return Task::none() };
        match f.to_def() {
            Err(e) => {
                f.error = Some(e);
                Task::none()
            }
            Ok((name, def)) => {
                f.error = None;
                let renamed = f.orig.clone().filter(|o| *o != name);
                self.apps.saving = true;
                let set = self.app_op(AppOp::Set { name, def });
                // Renamed: the new one is saved first, then the old one goes.
                let rm = match renamed {
                    Some(old) => self.app_op(AppOp::Remove { name: old }),
                    None => Task::none(),
                };
                Task::batch([set, rm])
            }
        }
    }

    /// A new agent, with the request written and what the form has so far.
    fn apps_ask_ai(&mut self) -> Task<Message> {
        let mut draft = ASK_AI.to_string();
        if let Some(so_far) = self.apps.form.as_ref().and_then(Form::so_far) {
            draft = format!("{} So far: {so_far}. Here are my instructions: ", ASK_AI.trim_end_matches("Here are my instructions: ").trim_end());
        }
        self.apps.form = None;
        self.ai_new_agent_with_draft(draft)
    }

    /// "Open address": a port, or a page of this machine.
    fn apps_address_go(&mut self) {
        let typed = self.apps.address.trim().to_string();
        if typed.is_empty() {
            return;
        }
        let url = match typed.parse::<u16>() {
            Ok(port) => format!("http://127.0.0.1:{port}/"),
            Err(_) if typed.contains("://") => typed.clone(),
            Err(_) => format!("http://{typed}"),
        };
        let call = Call::then(json!({ "cmd": "web.open", "url": url }), move |v| {
            if v["ok"] != true {
                crate::forward::note(format!("{typed}: {}", v["error"].as_str().unwrap_or("could not open it")), true);
            }
        });
        self.web_open(call);
    }

    // -----------------------------------------------------------------------------------------
    // keys
    // -----------------------------------------------------------------------------------------

    pub(super) fn apps_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let held = chord.ctrl || chord.logo;
        let plain = !(held || chord.alt);
        let paste = self.keymap.lookup(&chord) == Some(Command::Paste);
        if let Some(name) = self.apps.approving.clone() {
            return match chord.key {
                Key::Enter if plain => match self.serve.get(&name).map(|a| a.hash.clone()) {
                    Some(hash) => self.apps_approve(name, hash),
                    None => Task::none(),
                },
                Key::Esc => {
                    self.apps.approving = None;
                    Task::none()
                }
                _ => Task::none(),
            };
        }
        if self.apps.form.is_some() {
            return self.apps_form_key(chord, text, paste);
        }
        let rows = self.apps_rows();
        let n = rows.len();
        let cur = rows[self.apps.sel.min(n - 1)];
        let app = match cur {
            Row::App(i) => self.serve.list.get(i).map(|a| a.name.clone()),
            _ => None,
        };
        if !matches!(chord.key, Key::Char('d') | Key::Delete) {
            self.apps.removing = None;
        }
        let down = |s: usize| (s + 1).min(n - 1);
        match chord.key {
            Key::Up if plain => self.apps.sel = self.apps.sel.saturating_sub(1),
            Key::Down if plain => self.apps.sel = down(self.apps.sel),
            Key::Tab if chord.shift && !held => self.apps.sel = self.apps.sel.saturating_sub(1),
            Key::Tab if !held => self.apps.sel = down(self.apps.sel),
            Key::Enter if !held => {
                return match cur {
                    Row::Add => self.on_apps_msg(Msg::Add),
                    Row::AskAi => self.apps_ask_ai(),
                    Row::App(_) => app.map_or_else(Task::none, |a| self.apps_open_app(&a)),
                    Row::Address => {
                        self.apps_address_go();
                        Task::none()
                    }
                }
            }
            Key::Esc if cur == Row::Address && !self.apps.address.is_empty() => self.apps.address.clear(),
            Key::Esc => self.apps_close(),
            _ if cur == Row::Address => match chord.key {
                Key::Backspace if held || chord.alt => self.apps.address.clear(),
                Key::Backspace => {
                    self.apps.address.pop();
                }
                _ if paste => return iced::clipboard::read().map(|s| Message::Apps(Msg::Pasted(s))),
                _ if !held => self.apps.address.extend(text.unwrap_or_default().chars().filter(|c| !c.is_control())),
                _ => {}
            },
            Key::Right | Key::Left | Key::Char(' ') if plain && app.is_some() => {
                let name = app.unwrap_or_default();
                if chord.key == Key::Left {
                    self.apps.expanded.remove(&name);
                } else if chord.key == Key::Right || !self.apps.expanded.remove(&name) {
                    self.apps.expanded.insert(name);
                }
            }
            Key::Char('s') if plain && app.is_some() => return self.on_apps_msg(Msg::Stop(app.unwrap_or_default())),
            Key::Char('r') if plain && app.is_some() => return self.on_apps_msg(Msg::Restart(app.unwrap_or_default())),
            Key::Char('e') if plain && app.is_some() => return self.on_apps_msg(Msg::Edit(app.unwrap_or_default())),
            Key::Char('d') | Key::Delete if plain && app.is_some() => return self.apps_remove(app.unwrap_or_default()),
            Key::Char('a') if plain => return self.on_apps_msg(Msg::Add),
            _ => {
                if let Some(c) = self.keymap.lookup(&chord).filter(|c| super::app_wide(*c)) {
                    return self.exec(c);
                }
            }
        }
        Task::none()
    }

    fn apps_form_key(&mut self, chord: KeyChord, text: Option<String>, paste: bool) -> Task<Message> {
        let held = chord.ctrl || chord.logo;
        let save = held && matches!(chord.key, Key::Char('s') | Key::Enter);
        if save {
            return self.apps_save();
        }
        let Some(f) = &mut self.apps.form else { return Task::none() };
        let n = f.fields();
        match chord.key {
            Key::Esc => {
                self.apps.form = None;
                self.apps.saving = false;
            }
            Key::Up => f.sel = f.sel.saturating_sub(1),
            Key::Tab if chord.shift => f.sel = f.sel.saturating_sub(1),
            Key::Down | Key::Tab => f.sel = (f.sel + 1).min(n - 1),
            Key::Enter if f.sel + 1 == n => return self.apps_save(),
            Key::Enter => f.sel += 1,
            _ if paste => return iced::clipboard::read().map(|s| Message::Apps(Msg::Pasted(s))),
            Key::Backspace => {
                let sel = f.sel;
                if let Some(v) = f.field_mut(sel) {
                    if held || chord.alt {
                        v.clear();
                    } else {
                        v.pop();
                    }
                }
            }
            _ if !held => {
                let sel = f.sel;
                if let Some(v) = f.field_mut(sel) {
                    v.extend(text.unwrap_or_default().chars().filter(|c| !c.is_control()));
                }
            }
            _ => {}
        }
        Task::none()
    }

    // -----------------------------------------------------------------------------------------
    // view
    // -----------------------------------------------------------------------------------------

    pub(super) fn apps_panel(&self) -> Option<Element<'_, Message>> {
        let ui = &self.apps;
        if !ui.open {
            return None;
        }
        let th = &self.theme;
        let (dim, fg) = (th.dim, th.foreground);
        let focused = self.focus == Focus::Apps;
        let rows = self.apps_rows();
        let sel = ui.sel.min(rows.len() - 1);
        let at = |r: Row| rows.iter().position(|x| *x == r).unwrap_or(0);
        let is_sel = |r: Row| focused && ui.form.is_none() && ui.approving.is_none() && at(r) == sel;

        let close = button(text("✕").size(12).color(dim)).on_press(Message::Apps(Msg::Close)).padding([1, 6]).style(|_, _| button::Style::default());
        let header = row![
            text("Apps").size(12).color(fg),
            container(text(self.label.clone()).size(12).color(dim).wrapping(text::Wrapping::None)).width(Length::Fill).clip(true),
            close
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .padding(Padding { left: 14.0, right: 8.0, top: 8.0, bottom: 6.0 });

        let mut body = column![].spacing(6);
        if let Some(f) = &ui.form {
            body = body.push(self.apps_form_view(f, focused));
            return Some(self.apps_frame(header, body, focused));
        }
        if let Some(a) = ui.approving.as_deref().and_then(|n| self.serve.get(n)) {
            body = body.push(self.apps_approval(a, focused));
        }

        body = body.push(
            row![
                self.apps_button("+ Add", Some(Message::Apps(Msg::Add)), is_sel(Row::Add)),
                self.apps_button("✦ Register via AI chat", Some(Message::Apps(Msg::AskAi)), is_sel(Row::AskAi)),
            ]
            .spacing(8),
        );
        body = body.push(space().height(4));
        if self.serve.list.is_empty() {
            body = body.push(text("No apps registered for this project.").size(12).color(fg));
            body = body.push(text("Add one, or ask an agent to register it: tell it how you run the app.").size(11).color(dim));
        }
        for (i, a) in self.serve.list.iter().enumerate() {
            body = body.push(self.apps_row(a, at(Row::App(i)), is_sel(Row::App(i))));
        }

        body = body.push(space().height(6));
        body = body.push(text("Open address").size(11).color(dim));
        body = body.push(
            row![
                mouse_area(self.apps_box(ui.address.clone(), "a port (8000) or localhost:8000/path", is_sel(Row::Address), focused)).on_press(Message::Apps(Msg::Click(at(Row::Address)))),
                self.apps_button("Go", Some(Message::Apps(Msg::AddressGo)), false),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        if focused && ui.approving.is_none() {
            let mut hints = row![self.hint("↵", "open"), self.hint("↑↓", "choose")].spacing(12);
            if matches!(rows.get(sel), Some(Row::App(_))) {
                hints = hints.push(self.hint("→", "servers")).push(self.hint("s", "stop")).push(self.hint("r", "restart")).push(self.hint("e", "edit")).push(self.hint("d", "remove"));
            }
            hints = hints.push(self.hint("esc", "close"));
            body = body.push(space().height(4)).push(hints);
        }
        Some(self.apps_frame(header, body, focused))
    }

    /// One app: a dot in its state's colour, its title, its state and page; its buttons; then,
    /// opened, each server with its command.
    fn apps_row<'a>(&'a self, a: &'a AppInfo, row_ix: usize, selected: bool) -> Element<'a, Message> {
        let th = &self.theme;
        let (accent, dim, fg, warn, sel_bg) = (th.accent, th.dim, th.foreground, th.warn, th.selection);
        let state = a.state();
        let colour = |s: ServerState| match s {
            ServerState::Ready => accent,
            ServerState::Exited(_) | ServerState::PortTaken => warn,
            _ => dim,
        };
        let name = a.name.clone();
        let running = a.running();
        let open_label = if running { "Show" } else { "Open" };
        let mut buttons = row![self.apps_button(open_label, Some(Message::Apps(Msg::Open(name.clone()))), false)].spacing(6);
        if running {
            buttons = buttons.push(self.apps_button("Stop", Some(Message::Apps(Msg::Stop(name.clone()))), false));
        }
        let head = row![
            text(if running || state != ServerState::Stopped { "●" } else { "○" }).size(11).color(colour(state)).width(12),
            container(text(a.title().to_string()).size(12).color(fg).wrapping(text::Wrapping::None)).width(Length::Fill).clip(true),
            text(state.name()).size(11).color(colour(state)),
            buttons,
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let mut c = column![head].spacing(3);
        // What is wrong, in a line.
        if let Some(s) = a.servers.iter().find(|s| matches!(s.state, ServerState::PortTaken)) {
            c = c.push(text(format!("port {} is used by another process: {} was not started", s.def.port.unwrap_or(0), s.name)).size(11).color(warn));
        } else if let Some(s) = a.servers.iter().find(|s| matches!(s.state, ServerState::Exited(_))) {
            c = c.push(text(format!("{} {}: see its terminal", s.name, s.state.name())).size(11).color(warn));
        }
        if !a.approved {
            let by = a.origin.as_ref().map_or("somebody else".to_string(), |o| o.by.clone());
            c = c.push(text(format!("new or changed by {by}: you approve its commands when you open it")).size(11).color(warn));
        }
        if self.apps.removing.as_deref() == Some(a.name.as_str()) {
            c = c.push(
                row![
                    text("Remove this app?").size(11).color(warn),
                    self.apps_button("Remove", Some(Message::Apps(Msg::Remove(name.clone()))), true),
                    self.hint("d", "again to remove"),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        if self.apps.expanded.contains(&a.name) {
            for s in &a.servers {
                let port = s.def.port.map(|p| format!(":{p}")).unwrap_or_default();
                let page = a.def.page.as_ref().filter(|p| p.server == s.name).map(|p| format!("  page {}", p.path)).unwrap_or_default();
                c = c.push(
                    row![
                        space().width(12),
                        text(s.name.clone()).size(11).color(fg).width(80),
                        text(s.state.name()).size(11).color(colour(s.state)).width(80),
                        text(format!("{port}{page}")).size(11).color(dim),
                    ]
                    .spacing(6),
                );
                c = c.push(row![space().width(12), text(s.def.cmd.clone()).size(11).font(Font::MONOSPACE).color(dim)].spacing(6));
            }
            c = c.push(
                row![
                    space().width(12),
                    self.apps_button("Restart", Some(Message::Apps(Msg::Restart(name.clone()))), false),
                    self.apps_button("Edit", Some(Message::Apps(Msg::Edit(name.clone()))), false),
                    self.apps_button("Remove", Some(Message::Apps(Msg::Remove(name.clone()))), false),
                ]
                .spacing(6),
            );
        }
        let r = container(c).padding([4, 6]).width(Length::Fill).style(move |_| container::Style {
            background: selected.then(|| sel_bg.into()),
            border: Border { radius: 3.0.into(), ..Default::default() },
            ..Default::default()
        });
        mouse_area(r).on_press(Message::Apps(Msg::Click(row_ix))).into()
    }

    /// What an app that waits for the user would run, exactly, and who defined it: Run approves.
    fn apps_approval<'a>(&'a self, a: &'a AppInfo, focused: bool) -> Element<'a, Message> {
        let th = &self.theme;
        let (dim, fg, warn) = (th.dim, th.foreground, th.warn);
        let by = a.origin.as_ref().map_or("somebody else".to_string(), |o| o.by.clone());
        let mut c = column![
            text(format!("{} was defined or changed by {by}.", a.title())).size(12).color(fg),
            text("Opening it runs these commands on this machine, as you. Run them only if you expect them.").size(11).color(warn),
        ]
        .spacing(6);
        for s in &a.servers {
            let mut where_ = format!("{} — in {}", s.name, s.def.cwd.as_deref().unwrap_or("the project root"));
            if let Some(p) = s.def.port {
                where_.push_str(&format!(", port {p}"));
            }
            c = c.push(text(where_).size(11).color(dim));
            c = c.push(container(text(s.def.cmd.clone()).size(12).font(Font::MONOSPACE).color(fg)).padding([4, 6]).width(Length::Fill).style(move |_| container::Style {
                border: Border { color: dim, width: 1.0, radius: 3.0.into() },
                ..Default::default()
            }));
            for (k, v) in &s.def.env {
                c = c.push(text(format!("  {k}={v}")).size(11).font(Font::MONOSPACE).color(dim));
            }
        }
        c = c.push(
            row![
                self.apps_button("Run", Some(Message::Apps(Msg::Approve(a.name.clone(), a.hash.clone()))), focused),
                self.apps_button("Cancel", Some(Message::Apps(Msg::CancelApprove)), false),
                self.hint("↵", "run"),
                self.hint("esc", "cancel"),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        container(c)
            .padding(10)
            .width(Length::Fill)
            .style(move |_| container::Style { border: Border { color: warn, width: 1.0, radius: 4.0.into() }, ..Default::default() })
            .into()
    }

    fn apps_form_view<'a>(&'a self, f: &'a Form, focused: bool) -> Element<'a, Message> {
        let th = &self.theme;
        let (dim, fg) = (th.dim, th.foreground);
        let title = match &f.orig {
            Some(n) => format!("Edit {n}"),
            None => "Add an app".to_string(),
        };
        let mut c = column![text(title).size(12).color(fg)].spacing(6);
        let field = |i: usize, label: &'static str, value: &str, hint: &'static str| -> Element<'a, Message> {
            let selected = f.sel == i;
            let r = row![text(label).size(11).color(if selected { fg } else { dim }).width(64), self.apps_box(value.to_string(), hint, selected, focused)]
                .spacing(8)
                .align_y(Alignment::Center);
            mouse_area(r).on_press(Message::Apps(Msg::Field(i))).into()
        };
        c = c.push(field(0, "name", &f.name, "my-app (a-z 0-9 _ -)"));
        c = c.push(field(1, "title", &f.title, "what you call it"));
        for (j, s) in f.servers.iter().enumerate() {
            let base = 2 + j * SERVER_FIELDS.len();
            let mut head = row![text(format!("Server {}", j + 1)).size(11).color(fg), space::horizontal()].align_y(Alignment::Center);
            if f.servers.len() > 1 {
                head = head.push(button(text("✕ remove").size(11).color(dim)).on_press(Message::Apps(Msg::DropServer(j))).padding([0, 4]).style(|_, _| button::Style::default()));
            }
            c = c.push(space().height(2)).push(head);
            let values = [&s.name, &s.cmd, &s.port, &s.page, &s.after, &s.cwd, &s.env];
            for (k, ((label, hint), v)) in SERVER_FIELDS.iter().zip(values).enumerate() {
                c = c.push(field(base + k, label, v, hint));
            }
        }
        c = c.push(text("A command runs in your shell from the project root (or its folder), stays in the foreground, and listens on 127.0.0.1 at its port.").size(11).color(dim));
        if let Some(e) = &f.error {
            c = c.push(text(e.clone()).size(11).color(th.warn));
        }
        c = c.push(
            row![
                self.apps_button("+ server", Some(Message::Apps(Msg::AddServer)), false),
                self.apps_button(if self.apps.saving { "Saving…" } else { "Save" }, Some(Message::Apps(Msg::Save)), false),
                self.apps_button("Cancel", Some(Message::Apps(Msg::CancelForm)), false),
            ]
            .spacing(8),
        );
        c = c.push(self.apps_button("✦ Register via AI chat instead", Some(Message::Apps(Msg::AskAi)), false));
        if focused {
            c = c.push(row![self.hint("↑↓", "field"), self.hint(&format!("{MOD_PLUS}S"), "save"), self.hint("esc", "cancel")].spacing(12));
        }
        c.into()
    }

    /// The panel around `body`: scrolls, keyboard halo, click to focus.
    fn apps_frame<'a>(&'a self, header: iced::widget::Row<'a, Message>, body: iced::widget::Column<'a, Message>, focused: bool) -> Element<'a, Message> {
        let (accent, panel) = (self.theme.accent, self.theme.panel);
        let body = scrollable(body.padding(Padding { left: 14.0, right: 14.0, top: 10.0, bottom: 12.0 })).style(crate::theme::scrollbar).height(Length::Fill);
        let halo = if focused { accent } else { Color::TRANSPARENT };
        let panel = container(column![header, self.hrule(), body]).padding(2).width(WIDTH).height(Length::Fill).style(move |_| container::Style {
            background: Some(panel.into()),
            border: Border { color: halo, width: 2.0, radius: 3.0.into() },
            ..Default::default()
        });
        mouse_area(panel).on_press(Message::Apps(Msg::Focus)).into()
    }

    /// A one-line text box: `value`, or the dim `placeholder`; a caret when it has the keyboard.
    fn apps_box(&self, value: String, placeholder: &str, selected: bool, focused: bool) -> Element<'_, Message> {
        let th = &self.theme;
        let (accent, dim, fg, divider) = (th.accent, th.dim, th.foreground, th.divider);
        let active = selected && focused;
        let caret = container(space()).width(2).height(15).style(move |_| container::Style::default().background(if active { accent } else { Color::TRANSPARENT }));
        let shown: Element<'_, Message> = if value.is_empty() {
            row![caret, text(placeholder.to_owned()).size(12).color(dim)].align_y(Alignment::Center).into()
        } else {
            let typed: [iced::widget::text::Span<'_, (), iced::Font>; 2] = [span(value).color(fg), span("▏").color(if active { accent } else { Color::TRANSPARENT })];
            super::tail_line(rich_text(typed).size(12).wrapping(text::Wrapping::None))
        };
        container(shown)
            .padding([5, 8])
            .width(Length::Fill)
            .clip(true)
            .style(move |_| container::Style {
                border: Border { color: if active { accent } else { divider }, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            })
            .into()
    }

    fn apps_button(&self, label: impl Into<String>, msg: Option<Message>, selected: bool) -> Element<'_, Message> {
        let th = &self.theme;
        let (accent, fg, sel_bg, divider) = (th.accent, th.foreground, th.selection, th.divider);
        let mut b = button(text(label.into()).size(12).color(fg)).padding([3, 9]).style(move |_, st| button::Style {
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

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;
    use nib_proto::{AppOrigin, ServerInfo};

    fn key(a: &mut App, s: &str) {
        let _ = a.update(Message::Key { chord: KeyChord::parse(s).unwrap(), text: None, repeat: false });
        let _ = a.update(Message::KeyUp(KeyChord::parse(s).unwrap().key));
    }

    fn type_str(a: &mut App, s: &str) {
        for c in s.chars() {
            let _ = a.update(Message::Key { chord: KeyChord::new(Key::Char(c)), text: Some(c.to_string()), repeat: false });
            let _ = a.update(Message::KeyUp(Key::Char(c)));
        }
    }

    fn waiting_app() -> AppInfo {
        let def = AppDef {
            title: Some("Demo".into()),
            page: Some(AppPage { server: "web".into(), path: "/".into() }),
            servers: [("web".to_string(), ServerDef { cmd: "python -m http.server 8799".into(), port: Some(8799), ..Default::default() })].into(),
        };
        AppInfo {
            name: "demo".into(),
            servers: vec![ServerInfo { name: "web".into(), def: def.servers["web"].clone(), ..Default::default() }],
            def,
            origin: Some(AppOrigin { by: "agent:claude-3".into(), at: 0 }),
            hash: "abc".into(),
            approved: false,
            removed: false,
        }
    }

    /// Requests the window sent the daemon for the apps, oldest first.
    fn asked(a: &App) -> usize {
        a.pending.values().filter(|p| matches!(p, super::super::Pending::Serve { .. })).count()
    }

    #[test]
    fn the_toolbar_button_opens_the_panel_and_esc_closes_it() {
        let mut a = app();
        let _ = a.exec(Command::ToggleApps);
        assert!(a.apps.open && a.focus == Focus::Apps);
        assert!(a.apps_panel().is_some());
        key(&mut a, "esc");
        assert!(!a.apps.open && a.focus != Focus::Apps);
        // One side panel at a time.
        let _ = a.exec(Command::ToggleApps);
        let _ = a.exec(Command::ToggleSsh);
        assert!(!a.apps.open && a.ssh.open);
    }

    #[test]
    fn an_app_that_waits_shows_its_commands_and_only_the_users_own_key_runs_them() {
        let mut a = app();
        a.headless = true;
        let _ = a.on_app_info(waiting_app());
        let _ = a.exec(Command::ToggleApps);
        let before = asked(&a);
        // Down to the app (past Add and Register), Enter: shown, not run.
        key(&mut a, "down");
        key(&mut a, "down");
        key(&mut a, "enter");
        assert_eq!(a.apps.approving.as_deref(), Some("demo"));
        assert_eq!(asked(&a), before, "nothing is started before the user says so");
        // A control client pressing Enter is refused.
        let (call, rx) = Call::test(json!({ "cmd": "key", "chord": "enter" }));
        let _ = a.on_control(call);
        assert_eq!(rx.try_recv().unwrap()["ok"], true);
        assert_eq!(a.apps.approving.as_deref(), Some("demo"), "still waiting");
        assert_eq!(asked(&a), before);
        assert!(!a.ctl_input, "the mark is gone with the request");
        // The user's Enter approves what was shown, then starts it.
        key(&mut a, "enter");
        assert_eq!(a.apps.approving, None);
        assert_eq!(asked(&a), before + 2, "approve, then up");
        assert!(a.serve.opening.contains(&"demo".to_string()), "its page comes forward when it answers");
    }

    #[test]
    fn remove_asks_once_then_removes() {
        let mut a = app();
        a.headless = true;
        let _ = a.on_app_info(AppInfo { approved: true, ..waiting_app() });
        let _ = a.exec(Command::ToggleApps);
        key(&mut a, "down");
        key(&mut a, "down");
        let before = asked(&a);
        key(&mut a, "d");
        assert_eq!((a.apps.removing.as_deref(), asked(&a)), (Some("demo"), before));
        key(&mut a, "d");
        assert_eq!((a.apps.removing.as_deref(), asked(&a)), (None, before + 1));
        // Anything else in between keeps it.
        key(&mut a, "d");
        key(&mut a, "up");
        assert_eq!(a.apps.removing, None);
    }

    #[test]
    fn the_form_makes_an_app_and_says_what_it_cannot() {
        let mut a = app();
        a.headless = true;
        let _ = a.exec(Command::ToggleApps);
        key(&mut a, "enter"); // + Add
        assert!(a.apps.form.is_some());
        type_str(&mut a, "demo");
        key(&mut a, "down");
        type_str(&mut a, "Demo");
        key(&mut a, "down");
        type_str(&mut a, "web");
        key(&mut a, "down");
        type_str(&mut a, "python -m http.server 8799");
        key(&mut a, "down");
        type_str(&mut a, "87x9");
        key(&mut a, "down");
        type_str(&mut a, "/");
        let before = asked(&a);
        key(&mut a, "ctrl+s");
        assert!(a.apps.form.as_ref().unwrap().error.as_deref().unwrap().contains("port"), "checked before it is sent");
        assert_eq!(asked(&a), before);
        let f = a.apps.form.as_mut().unwrap();
        f.servers[0].port = "8799".into();
        let (name, def) = f.to_def().unwrap();
        assert_eq!((name.as_str(), def.title.as_deref(), def.servers["web"].port), ("demo", Some("Demo"), Some(8799)));
        assert_eq!(def.page, Some(AppPage { server: "web".into(), path: "/".into() }));
        key(&mut a, "ctrl+s");
        assert_eq!(asked(&a), before + 1);
        // The daemon's refusal is said in the form; its yes closes it.
        assert!(a.apps_answer(&Err(RpcError::new(nib_proto::ErrorKind::BadRequest, "demo/web: env PATH is not allowed"))));
        assert_eq!(a.apps.form.as_ref().unwrap().error.as_deref(), Some("demo/web: env PATH is not allowed"));
        key(&mut a, "ctrl+s");
        assert!(a.apps_answer(&Ok(Response::Apps(vec![]))));
        assert!(a.apps.form.is_none());
    }

    #[test]
    fn two_pages_or_a_bad_env_are_said_in_the_form() {
        let f = Form {
            name: "demo".into(),
            servers: vec![FormServer { name: "a".into(), cmd: "x".into(), page: "/".into(), ..Default::default() }, FormServer { name: "b".into(), cmd: "y".into(), page: "/b".into(), ..Default::default() }],
            ..Default::default()
        };
        assert!(f.to_def().unwrap_err().contains("only one server"));
        let f = Form { name: "demo".into(), servers: vec![FormServer { name: "a".into(), cmd: "x".into(), env: "A=1; B".into(), ..Default::default() }], ..Default::default() };
        assert!(f.to_def().unwrap_err().contains("NAME=value"));
        let f = Form { name: "demo".into(), servers: vec![FormServer { name: "a".into(), cmd: "x".into(), env: "A=1; B=x=y".into(), after: " c , d ".into(), ..Default::default() }], ..Default::default() };
        let (_, def) = f.to_def().unwrap();
        assert_eq!((def.servers["a"].env["B"].as_str(), def.servers["a"].after.join(" ")), ("x=y", "c d".into()));
        // And back: an app as the form shows it.
        let back = Form::from_app(&AppInfo { name: "demo".into(), def, ..Default::default() });
        assert_eq!((back.servers[0].env.as_str(), back.servers[0].after.as_str()), ("A=1; B=x=y", "c,d"));
    }

    #[test]
    fn register_via_ai_chat_opens_a_chat_with_the_request_written() {
        let mut a = app();
        let _ = a.exec(Command::ToggleApps);
        key(&mut a, "down");
        key(&mut a, "enter");
        assert_eq!(a.ai_draft(), ASK_AI);
        assert!(ASK_AI.starts_with("/nib-app "), "the skill goes in front of the message");
        // From a form half filled: what it has goes along.
        let _ = a.exec(Command::ToggleApps);
        let _ = a.on_apps_msg(Msg::Add);
        if let Some(f) = &mut a.apps.form {
            f.name = "demo".into();
            f.servers[0] = FormServer { name: "api".into(), cmd: "uv run api".into(), port: "8765".into(), ..Default::default() };
        }
        let _ = a.on_apps_msg(Msg::AskAi);
        let draft = a.ai_draft();
        assert!(draft.starts_with("/nib-app ") && draft.contains("So far: app `demo`; server api: `uv run api` on port 8765.") && draft.ends_with("Here are my instructions: "), "{draft}");
    }
}
