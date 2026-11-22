//! The project's apps (nibd's `serve`), in the window: each server of an app that runs is a
//! terminal, headed with its app, its name and its state, shown beside the app's page only
//! (`term_here`); an app's page gets a web tab once its server's port answers. Started and stopped from the Apps panel
//! (`apps.rs`), by the `app_*` commands, and by `nib ctl app …`.

use super::{App, Focus, Message, Pending, Term};
use crate::control::Call;
use iced::widget::{row, text};
use iced::{Element, Font, Task};
use nib_proto::{AppDef, AppInfo, AppOp, ClientNotify, Request, Response, RpcError, ServerState};
use serde_json::{json, Value};

/// What the window knows of the project's apps.
#[derive(Default)]
pub(super) struct State {
    /// As the daemon last said, by name.
    pub(super) list: Vec<AppInfo>,
    /// Apps whose page was shown: ready again, the page is reloaded.
    viewed: Vec<String>,
    /// Apps the user asked to open: their page comes to the front once it answers (an app an
    /// agent started shows its page behind what the user is looking at).
    pub(super) opening: Vec<String>,
}

impl State {
    /// How many servers are on their way to ready (`nib ctl wait` waits for them).
    pub(super) fn coming(&self) -> usize {
        self.list.iter().flat_map(|a| &a.servers).filter(|s| s.state.coming()).count()
    }

    pub(super) fn get(&self, name: &str) -> Option<&AppInfo> {
        self.list.iter().find(|a| a.name == name)
    }
}

/// A server's terminal is keyed by `app/server`.
pub(super) fn term_key(app: &str, server: &str) -> String {
    format!("{app}/{server}")
}

impl App {
    /// Ask the daemon to do `op` for the user (a click, a key they pressed).
    pub(super) fn app_op(&mut self, op: AppOp) -> Task<Message> {
        self.app_ask(op, None, true, false)
    }

    /// `call`: a control client asked, and gets the apps as they are afterwards. `user`: the
    /// person at the window asked (a control client never is). `quiet`: the window's own
    /// question (at connect): no message if the daemon refuses.
    pub(super) fn app_ask(&mut self, op: AppOp, call: Option<Call>, user: bool, quiet: bool) -> Task<Message> {
        if quiet && (self.conn.is_none() || self.disconnected) {
            return Task::none();
        }
        if self.conn.is_none() || self.disconnected {
            if let Some(c) = &call {
                c.err("not connected");
            }
            return self.flash_warn("not connected");
        }
        self.request(Request::App { op, user }, Pending::Serve { call, quiet })
    }

    /// `app_restart`: the server whose terminal has the keyboard.
    pub(super) fn serve_restart_focused(&mut self) -> Task<Message> {
        let key = self.focused_term().and_then(|t| t.server.clone());
        match key.as_deref().and_then(|k| k.split_once('/')) {
            Some((app, server)) => self.app_op(AppOp::Restart { name: app.into(), server: Some(server.into()) }),
            None => self.flash_info("restart a server from its terminal (or from the Apps panel)"),
        }
    }

    pub(super) fn on_serve(&mut self, call: Option<Call>, quiet: bool, result: Result<Response, RpcError>) -> Task<Message> {
        // The Apps panel's form waits for this answer: a refusal is said there, not flashed.
        let quiet = quiet || (call.is_none() && self.apps_answer(&result));
        match result {
            Ok(Response::Apps(list)) => {
                // Those the daemon no longer lists are gone.
                let gone: Vec<String> = self.serve.list.iter().filter(|a| !list.iter().any(|l| l.name == a.name)).map(|a| a.name.clone()).collect();
                let mut tasks: Vec<Task<Message>> = gone.into_iter().map(|name| self.on_app_info(AppInfo { name, removed: true, ..Default::default() })).collect();
                tasks.extend(list.into_iter().map(|a| self.on_app_info(a)));
                if let Some(c) = call {
                    c.ok(json!({ "apps": self.apps_json() }));
                }
                Task::batch(tasks)
            }
            Ok(_) => Task::none(),
            Err(e) => match call {
                Some(c) => {
                    c.err(&e.msg);
                    Task::none()
                }
                None if quiet => Task::none(),
                None => self.flash_warn(format!("apps: {}", e.msg)),
            },
        }
    }

    /// The daemon says how an app is now: keep its servers' terminals and its page in line.
    pub(super) fn on_app_info(&mut self, info: AppInfo) -> Task<Message> {
        let name = info.name.clone();
        let before = self.serve.list.iter().position(|a| a.name == name).map(|i| self.serve.list.remove(i));
        let state_before = |server: &str| before.as_ref().and_then(|b| b.servers.iter().find(|s| s.name == server)).map(|s| s.state);
        let via = self.forward_via();
        // Servers it had that it no longer has (or all of them: the app is gone).
        for s in before.iter().flat_map(|b| &b.servers).filter(|s| info.removed || !info.servers.iter().any(|n| n.name == s.name)) {
            if let (Some(via), Some(port)) = (&via, s.def.port) {
                crate::forward::server_gone(&via.host, port);
            }
            let key = term_key(&name, &s.name);
            if let Some(k) = self.terms.iter().find(|t| t.server.as_deref() == Some(key.as_str())).map(|t| t.key) {
                self.remove_term(k);
            }
        }
        if info.removed {
            self.serve.viewed.retain(|n| *n != name);
            self.serve.opening.retain(|n| *n != name);
            self.apps_forget(&name);
            return Task::none();
        }
        for s in &info.servers {
            let key = term_key(&name, &s.name);
            // On another machine, a server that runs has its port brought here; one that no
            // longer does, no longer.
            if let (Some(via), Some(port), false) = (&via, s.def.port, s.state == ServerState::Ready || s.state.coming()) {
                crate::forward::server_gone(&via.host, port);
            }
            let term = self.terms.iter().position(|t| t.server.as_deref() == Some(key.as_str()));
            if s.state == ServerState::Stopped && s.pty.is_none() {
                // Stopped on purpose: its terminal goes, as a closed one does.
                if let Some(k) = term.map(|i| self.terms[i].key) {
                    self.remove_term(k);
                }
            } else if let Some(i) = term {
                let t = &mut self.terms[i];
                if let (Some(id), true) = (s.pty, s.pty != t.id) {
                    // Started again, in the terminal it had: what it printed before stays above.
                    t.parser.process(b"\r\n\x1b[2m--- started again ---\x1b[0m\r\n");
                    t.queued.clear();
                    let (rows, cols) = t.size;
                    self.notify(ClientNotify::PtyResize { id, cols, rows });
                }
                self.terms[i].id = s.pty;
            } else if let Some(id) = s.pty {
                let (rows, cols) = (24, 80);
                let k = self.next_term_key;
                self.next_term_key += 1;
                self.terms.push(Term {
                    key: k,
                    id: Some(id),
                    parser: vt100::Parser::new(rows.try_into().unwrap(), cols.try_into().unwrap(), 5000),
                    size: (rows, cols),
                    queued: Vec::new(),
                    server: Some(key),
                });
                // Shown, but the keyboard stays where it is.
                self.terms_visible = true;
            }
        }
        // The servers that just became ready, by port: brought here first when the app runs on
        // another machine (every one: the frontend's page asks the backend's port from the
        // browser), and the page shown when it is the page's.
        let ready: Vec<(String, u16)> = info
            .servers
            .iter()
            .filter(|s| s.state == ServerState::Ready && state_before(&s.name) != Some(ServerState::Ready))
            .filter_map(|s| Some((s.name.clone(), s.def.port?)))
            .collect();
        let page = info.def.page.clone().filter(|_| !self.headless);
        // An app with no page was opened once it runs: nothing is left to bring forward.
        if info.def.page.is_none() && info.servers.iter().any(|s| s.state == ServerState::Ready && state_before(&s.name) != Some(ServerState::Ready)) {
            self.serve.opening.retain(|n| *n != name);
        }
        self.serve.list.push(info);
        self.serve.list.sort_by(|a, b| a.name.cmp(&b.name));
        self.apps_clamp();
        let again = self.serve.viewed.contains(&name);
        let asked = self.serve.opening.contains(&name);
        let root = self.root.to_string();
        for (server, port) in ready {
            let is_page = page.as_ref().is_some_and(|p| p.server == server);
            if is_page {
                if !again {
                    self.serve.viewed.push(name.clone());
                }
                self.serve.opening.retain(|n| *n != name);
            }
            // Its page: the same request `nib ctl web open URL` makes; again after a restart,
            // its tab stays where it is and shows the page afresh.
            let path = page.as_ref().filter(|_| is_page).map(|p| p.path.clone());
            let root = root.clone();
            let show = move |port: u16| {
                let Some(path) = path else { return };
                crate::web::submit(Call::internal(json!({ "cmd": "web.open", "url": format!("http://127.0.0.1:{port}{path}"), "background": !asked })), &root);
                if again {
                    crate::web::submit(Call::internal(json!({ "cmd": "web.reload", "tab": port.to_string() })), &root);
                }
            };
            match &via {
                None => show(port),
                Some(via) => {
                    let via = via.clone();
                    let label = format!("{name}/{server}");
                    drop(std::thread::spawn(move || match crate::forward::add(&via.host, &via.label, port, crate::forward::Why::Server, true) {
                        Ok(made) => show(made.local),
                        Err(e) => crate::forward::note(format!("{label}: {e}"), true),
                    }))
                }
            }
        }
        Task::none()
    }

    /// The window is on another machine (or another daemon): nothing of the old one's is left.
    pub(super) fn serve_reset(&mut self) {
        self.serve = State::default();
    }

    /// The line above a server's terminal: its app, its name, how it is, where it listens.
    pub(super) fn serve_header(&self, t: &Term) -> Option<Element<'_, Message>> {
        let key = t.server.as_deref()?;
        let (app, server) = key.split_once('/')?;
        let th = &self.theme;
        let a = self.serve.get(app);
        let info = a.and_then(|a| a.servers.iter().find(|s| s.name == server));
        let state = info.map_or(ServerState::Stopped, |s| s.state);
        let colour = match state {
            ServerState::Ready => th.accent,
            ServerState::Exited(_) | ServerState::PortTaken => th.warn,
            _ => th.dim,
        };
        let port = info.and_then(|s| s.def.port).map(|p| format!(":{p}")).unwrap_or_default();
        let fg = if self.focus == Focus::Terminal(t.key) { th.foreground } else { th.dim };
        let title = a.map_or(app, AppInfo::title);
        Some(
            row![
                text(format!("{title} › {server}")).size(12).font(Font::MONOSPACE).color(fg),
                text(state.name()).size(12).font(Font::MONOSPACE).color(colour),
                text(port).size(12).font(Font::MONOSPACE).color(th.dim),
            ]
            .spacing(10)
            .padding([2, 6])
            .into(),
        )
    }

    /// The address an app's page is at on this machine (forwarded here when the app runs on
    /// another).
    pub(super) fn app_page_url(&self, a: &AppInfo) -> Option<String> {
        let p = a.def.page.as_ref()?;
        let port = a.def.servers.get(&p.server)?.port?;
        let via = self.forward_via();
        let here = via.as_ref().and_then(|v| crate::forward::local_of(&v.host, port)).unwrap_or(port);
        Some(format!("http://127.0.0.1:{here}{}", p.path))
    }

    /// The app whose page web tab `page` shows (any address on the page's origin), if any.
    fn web_app(&self, page: u32) -> Option<&AppInfo> {
        if self.serve.list.is_empty() {
            return None;
        }
        let url = &self.web_info(page)?.url;
        self.serve.list.iter().find(|a| {
            let Some(base) = self.app_page_url(a) else { return false };
            let origin = base.split('/').take(3).collect::<Vec<_>>().join("/");
            url.starts_with(&format!("{origin}/")) || *url == origin
        })
    }

    /// What a web tab is called in the tab bar: the title of the app whose page it shows, if it
    /// is one, else the page's own name.
    pub(super) fn web_title(&self, page: u32) -> String {
        self.web_app(page).map_or_else(|| self.web_label(page), |a| a.title().to_string())
    }

    /// Whether terminal `t` belongs where the window is now. A server's terminal is part of its
    /// app's page: it shows while that page is the tab in front, and not over code, the agents
    /// or another page. Until then it shows while the user's Open waits for the page (they
    /// watch it start), and always for an app with no page (there is nowhere else to put it).
    pub(super) fn term_here(&self, t: &Term) -> bool {
        let Some(key) = t.server.as_deref() else { return true };
        let app = key.split_once('/').map_or(key, |(a, _)| a);
        let Some(a) = self.serve.get(app) else { return true };
        if a.def.page.is_none() || self.serve.opening.iter().any(|n| n == app) {
            return true;
        }
        if self.ai.shown() {
            return false;
        }
        self.tab().and_then(|t| t.web).and_then(|page| self.web_app(page)).is_some_and(|f| f.name == app)
    }

    /// For `state`: every app the daemon told of, with its servers.
    pub(super) fn apps_json(&self) -> Value {
        self.serve
            .list
            .iter()
            .map(|a| {
                let servers: Vec<Value> = a
                    .servers
                    .iter()
                    .map(|s| {
                        let key = term_key(&a.name, &s.name);
                        let term = self.terms.iter().find(|t| t.server.as_deref() == Some(key.as_str()));
                        json!({
                            "name": s.name, "state": s.state.name(), "port": s.def.port, "terminal": term.map(|t| t.key),
                            "cmd": s.def.cmd, "after": s.def.after, "cwd": s.def.cwd, "env": s.def.env,
                        })
                    })
                    .collect();
                json!({
                    "name": a.name, "title": a.title(), "state": a.state().name(), "page": a.def.page.as_ref().map(|p| json!({ "server": p.server, "path": p.path })),
                    "approved": a.approved, "by": a.origin.as_ref().map(|o| o.by.clone()), "servers": servers,
                })
            })
            .collect()
    }

    /// `app.*` from the control socket: never the user (it cannot approve, and starts only
    /// approved apps).
    pub(super) fn on_app_ctl(&mut self, call: Call) -> Task<Message> {
        let name = call.str_arg("name").map(str::to_string);
        let server = call.str_arg("server").map(str::to_string);
        let names: Vec<String> = call.req.get("names").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        let needs_name = |what: &str| {
            call.err(format!("app {what} needs an app's name"));
            Task::none()
        };
        let op = match (call.cmd(), name) {
            ("app.list", _) => AppOp::List,
            ("app.up", _) => AppOp::Up { names },
            ("app.down", _) => AppOp::Down { names },
            ("app.set", Some(name)) => match serde_json::from_value::<AppDef>(call.req.get("def").cloned().unwrap_or_default()) {
                Ok(def) => AppOp::Set { name, def },
                Err(e) => {
                    call.err(format!("app set: {e}"));
                    return Task::none();
                }
            },
            ("app.rm", Some(name)) => AppOp::Remove { name },
            ("app.restart", Some(name)) => AppOp::Restart { name, server },
            ("app.log", Some(name)) => {
                // What its terminal shows, as `state.terminals[].screen` has it: the named
                // server's, else the page's, else the first.
                let a = self.serve.get(&name);
                let server = server.or_else(|| a.and_then(|a| a.def.page.as_ref().map(|p| p.server.clone()))).or_else(|| a.and_then(|a| a.servers.first().map(|s| s.name.clone())));
                let key = term_key(&name, server.as_deref().unwrap_or(""));
                let Some(t) = self.terms.iter().find(|t| t.server.as_deref() == Some(key.as_str())) else {
                    call.err(format!("{key} has no terminal: it is not running, and has not ended by itself (app list)"));
                    return Task::none();
                };
                let screen = t.parser.screen().contents();
                let all: Vec<&str> = screen.trim_end().lines().collect();
                let n = call.req.get("lines").and_then(Value::as_u64).map_or(all.len(), |n| n as usize);
                call.ok(json!({ "name": name, "server": server, "terminal": t.key, "log": all[all.len().saturating_sub(n)..].join("\n") }));
                return Task::none();
            }
            ("app.approve", _) => {
                call.err("only the user approves an app, from the Apps panel");
                return Task::none();
            }
            ("app.set", None) => return needs_name("set"),
            ("app.rm", None) => return needs_name("rm"),
            ("app.restart", None) => return needs_name("restart"),
            ("app.log", None) => return needs_name("log"),
            (other, _) => {
                call.err(format!("unknown cmd {other:?} (app.list, set, rm, up, down, restart, log)"));
                return Task::none();
            }
        };
        self.app_ask(op, Some(call), false, false)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;
    use nib_proto::{AppPage, ReqId, ServerDef, ServerInfo, ServerMsg, ServerNotify};

    fn server(name: &str, port: u16, state: ServerState, pty: Option<u32>) -> ServerInfo {
        ServerInfo { name: name.into(), def: ServerDef { cmd: "run".into(), port: Some(port), ..Default::default() }, state, pty }
    }

    fn demo(servers: Vec<ServerInfo>) -> AppInfo {
        let def = AppDef {
            title: Some("Demo".into()),
            page: Some(AppPage { server: "web".into(), path: "/".into() }),
            servers: servers.iter().map(|s| (s.name.clone(), s.def.clone())).collect(),
        };
        AppInfo { name: "demo".into(), def, servers, approved: true, hash: "h".into(), ..Default::default() }
    }

    fn told(a: &mut App, i: AppInfo) {
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::App(i)));
    }

    /// The request to the daemon that is waiting for its answer.
    fn app_request(a: &App) -> ReqId {
        *a.pending.iter().find(|(_, p)| matches!(p, Pending::Serve { .. })).expect("an App request").0
    }

    #[test]
    fn a_running_server_is_a_terminal_that_outlives_its_process_but_not_a_stop() {
        let mut a = app();
        a.headless = true;
        told(&mut a, demo(vec![server("api", 8765, ServerState::Starting, Some(7)), server("web", 8766, ServerState::Waiting, None)]));
        assert_eq!(a.terms.len(), 1, "only a server that runs has a terminal");
        assert!(a.terms_visible && a.focus != Focus::Terminal(a.terms[0].key), "shown, the keyboard stays");
        assert_eq!((a.terms[0].server.as_deref(), a.terms[0].id), (Some("demo/api"), Some(7)));
        assert_eq!(a.serve.coming(), 2);
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::PtyOutput { id: 7, data: b"Uvicorn running\r\n".to_vec() }));
        told(&mut a, demo(vec![server("api", 8765, ServerState::Ready, Some(7)), server("web", 8766, ServerState::Stopped, None)]));
        let s = a.apps_json();
        assert_eq!((s[0]["name"].as_str(), s[0]["title"].as_str(), s[0]["state"].as_str()), (Some("demo"), Some("Demo"), Some("ready")));
        assert_eq!((s[0]["servers"][0]["port"].as_u64(), s[0]["servers"][0]["terminal"].as_u64()), (Some(8765), Some(a.terms[0].key)));
        assert_eq!(a.serve.coming(), 0);
        // It ended by itself: what it printed can still be read.
        told(&mut a, demo(vec![server("api", 8765, ServerState::Exited(Some(1)), None), server("web", 8766, ServerState::Stopped, None)]));
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::PtyExit { id: 7, code: Some(1) }));
        assert_eq!(a.terms.len(), 1);
        assert!(a.terms[0].id.is_none() && a.terms[0].parser.screen().contents().contains("Uvicorn running"));
        assert_eq!(a.apps_json()[0]["state"], "exited(1)");
        // Started again: the same terminal.
        told(&mut a, demo(vec![server("api", 8765, ServerState::Starting, Some(9)), server("web", 8766, ServerState::Stopped, None)]));
        assert_eq!((a.terms.len(), a.terms[0].id), (1, Some(9)));
        assert!(a.terms[0].parser.screen().contents().contains("started again"));
        // Stopped: the terminal goes; removed: the app is forgotten.
        told(&mut a, demo(vec![server("api", 8765, ServerState::Stopped, None), server("web", 8766, ServerState::Stopped, None)]));
        assert!(a.terms.is_empty() && a.apps_json()[0]["servers"][0]["terminal"].is_null());
        told(&mut a, AppInfo { name: "demo".into(), removed: true, ..Default::default() });
        assert_eq!(a.apps_json(), json!([]));
    }

    #[test]
    fn a_servers_terminal_shows_on_its_apps_page_only() {
        let mut a = app();
        a.headless = true;
        let _ = a.new_terminal();
        let shell = a.terms[0].key;
        told(&mut a, demo(vec![server("api", 8765, ServerState::Ready, Some(7)), server("web", 8766, ServerState::Ready, Some(8))]));
        let here = |a: &App| a.terms_here().map(|t| t.key).collect::<Vec<_>>();
        let page = |id: u32, url: &str| crate::web::TabInfo {
            id, name: "p".into(), address: String::new(), url: url.into(), loading: false, blocked: None,
            dialog: None, choice: None, cursor: String::new(), saved: Vec::new(), zoom: 1.0,
        };
        // No page in front: the user's shell, not the app's servers.
        assert_eq!(here(&a), vec![shell]);
        // A file, another app's page, then the app's own page (anywhere on its origin).
        a.put_doc(nib_editor::Document::new_plain(Default::default(), None), true);
        let file = a.active;
        for (id, url) in [(1, "http://127.0.0.1:9000/"), (2, "http://127.0.0.1:8766/study/3")] {
            a.put_doc(nib_editor::Document::new_plain(Default::default(), None), true);
            let i = a.active;
            a.tabs[i].web = Some(id);
            a.web.pages.push(page(id, url));
        }
        assert_eq!(here(&a).len(), 3, "on its page: the shell and both servers");
        let api = a.terms[1].key;
        a.set_focus(Focus::Terminal(api));
        assert!(a.focus_ring().contains(&Focus::Terminal(api)));
        let _ = a.update(Message::SelectTab(file));
        assert_eq!(here(&a), vec![shell], "over code they are gone");
        assert!(!matches!(a.focus, Focus::Terminal(_)), "and the keys left them");
        assert!(!a.focus_ring().contains(&Focus::Terminal(api)));
        let _ = a.update(Message::SelectTab(file + 1));
        assert_eq!(here(&a), vec![shell], "another page: gone too");
        // Opened by the user and not up yet: watched starting from wherever they are.
        a.serve.opening.push("demo".into());
        assert_eq!(here(&a).len(), 3);
        assert_eq!(a.control_state()["terminals"][1]["shown"], true);
    }

    #[test]
    fn the_apps_are_asked_for_at_connect_and_a_refusal_then_is_not_a_message() {
        let mut a = app();
        a.headless = true;
        let launch = a.launch.clone();
        let _ = a.hello(&launch);
        let id = *a.pending.iter().find(|(_, p)| matches!(p, Pending::Hello { .. })).unwrap().0;
        let hello = Response::Hello {
            protocol: nib_proto::PROTOCOL_VERSION,
            daemon_version: String::new(),
            root: "/p".into(),
            scratch: String::new(),
            nibdir: String::new(),
            hostname: String::new(),
            os: String::new(),
            arch: String::new(),
            lsp_extensions: Vec::new(),
        };
        let _ = a.on_conn(a.gen, super::super::connection::ConnEvent::Server(ServerMsg::Response { id, result: Ok(hello) }));
        let id = *a.pending.iter().find(|(_, p)| matches!(p, Pending::Serve { call: None, quiet: true })).expect("the apps asked for").0;
        // A machine with no NIBDIR refuses: nothing to tell the user at every connect.
        let said = a.messages.len();
        let _ = a.on_server(ServerMsg::Response { id, result: Err(RpcError::new(nib_proto::ErrorKind::BadRequest, "no NIBDIR")) });
        assert_eq!(a.messages.len(), said);
        // Asked for by somebody, the same refusal is said.
        let _ = a.app_op(AppOp::List);
        let id = app_request(&a);
        let _ = a.on_server(ServerMsg::Response { id, result: Err(RpcError::new(nib_proto::ErrorKind::BadRequest, "no NIBDIR")) });
        assert_eq!(a.messages.len(), said + 1);
    }

    #[test]
    fn closing_a_servers_terminal_closes_its_pty_and_restart_is_for_the_focused_one() {
        let mut a = app();
        a.headless = true;
        told(&mut a, demo(vec![server("api", 8765, ServerState::Ready, Some(7)), server("web", 8766, ServerState::Stopped, None)]));
        let key = a.terms[0].key;
        // Shown while the user's Open waits for its page.
        a.serve.opening.push("demo".into());
        let _ = a.exec(nib_editor::Command::AppRestart);
        assert!(!a.pending.values().any(|p| matches!(p, Pending::Serve { .. })), "no server's terminal has the keyboard");
        a.set_focus(Focus::Terminal(key));
        let _ = a.exec(nib_editor::Command::AppRestart);
        app_request(&a);
        let _ = a.exec(nib_editor::Command::CloseTab);
        assert!(a.terms.is_empty(), "closed like any terminal (the daemon stops the server with its pty)");
    }

    #[test]
    fn the_control_socket_asks_the_daemon_as_not_the_user() {
        let mut a = app();
        a.headless = true;
        let (call, rx) = Call::test(json!({ "cmd": "app.set", "name": "demo", "def": { "servers": { "api": { "cmd": "run", "port": 8765 } } } }));
        let _ = a.on_control(call);
        let id = app_request(&a);
        // Asked as `nib ctl` (not the user): what it defines waits for the user.
        let _ = a.on_server(ServerMsg::Response { id, result: Ok(Response::Apps(vec![AppInfo { approved: false, ..demo(vec![server("api", 8765, ServerState::Stopped, None)]) }])) });
        let v = rx.try_recv().unwrap();
        assert_eq!((v["ok"].as_bool(), v["apps"][0]["state"].as_str(), v["apps"][0]["approved"].as_bool()), (Some(true), Some("stopped"), Some(false)));
        // It cannot approve.
        let (call, rx) = Call::test(json!({ "cmd": "app.approve", "name": "demo", "hash": "h" }));
        let _ = a.on_control(call);
        assert!(rx.try_recv().unwrap()["error"].as_str().unwrap().contains("only the user"));
        // The daemon's refusal is the answer.
        let (call, rx) = Call::test(json!({ "cmd": "app.up", "names": ["nobody"] }));
        let _ = a.on_control(call);
        let id = app_request(&a);
        let _ = a.on_server(ServerMsg::Response { id, result: Err(RpcError::new(nib_proto::ErrorKind::BadRequest, "no app named \"nobody\"")) });
        assert_eq!(rx.try_recv().unwrap(), json!({ "ok": false, "error": "no app named \"nobody\"" }));
        // The log is the terminal's screen.
        told(&mut a, demo(vec![server("api", 8765, ServerState::Ready, Some(3))]));
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::PtyOutput { id: 3, data: b"one\r\ntwo\r\nthree\r\n".to_vec() }));
        let (call, rx) = Call::test(json!({ "cmd": "app.log", "name": "demo", "server": "api", "lines": 2 }));
        let _ = a.on_control(call);
        assert_eq!(rx.try_recv().unwrap()["log"], "two\nthree");
        for wrong in [json!({ "cmd": "app.restart" }), json!({ "cmd": "app.set", "name": "x", "def": { "servers": 1 } }), json!({ "cmd": "app.dance" })] {
            let (call, rx) = Call::test(wrong);
            let _ = a.on_control(call);
            assert_eq!(rx.try_recv().unwrap()["ok"], false);
        }
        // A list without it: it is gone here too.
        let (call, _rx) = Call::test(json!({ "cmd": "app.list" }));
        let _ = a.on_control(call);
        let id = app_request(&a);
        let _ = a.on_server(ServerMsg::Response { id, result: Ok(Response::Apps(vec![])) });
        assert!(a.terms.is_empty() && a.apps_json() == json!([]));
    }
}
