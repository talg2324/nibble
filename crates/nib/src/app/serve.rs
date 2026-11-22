//! The project's servers (nibd's `serve`), in the window: each one that runs is a terminal in
//! the terminal column, headed with its name and state; one with a `view` gets a web tab once
//! its port answers. Started and stopped by the `serve_*` commands and by `nib ctl serve …`.

use super::{App, Focus, Message, Pending, Term};
use crate::control::Call;
use iced::widget::{row, text};
use iced::{Element, Font, Task};
use nib_proto::{ClientNotify, Request, Response, RpcError, ServeOp, ServerDef, ServerInfo, ServerState};
use serde_json::{json, Value};

/// What the window knows of the project's servers.
#[derive(Default)]
pub(super) struct State {
    /// As the daemon last said, by name.
    list: Vec<ServerInfo>,
    /// Servers whose page was shown: ready again, the page is reloaded.
    viewed: Vec<String>,
}

impl State {
    /// How many are on their way to ready (`nib ctl wait` waits for them).
    pub(super) fn coming(&self) -> usize {
        self.list.iter().filter(|s| s.state.coming()).count()
    }
}

impl App {
    /// Ask the daemon to do `op`; `call`, if a control client asked, gets the servers as they
    /// are afterwards.
    pub(super) fn serve(&mut self, op: ServeOp, call: Option<Call>) -> Task<Message> {
        self.serve_ask(op, call, false)
    }

    /// `quiet`: the window's own question (at connect): no message if the daemon refuses.
    pub(super) fn serve_ask(&mut self, op: ServeOp, call: Option<Call>, quiet: bool) -> Task<Message> {
        if quiet && (self.conn.is_none() || self.disconnected) {
            return Task::none();
        }
        if self.conn.is_none() || self.disconnected {
            if let Some(c) = &call {
                c.err("not connected");
            }
            return self.flash_warn("not connected");
        }
        self.request(Request::Serve { op }, Pending::Serve { call, quiet })
    }

    /// `serve_restart`: the server whose terminal has the keyboard.
    pub(super) fn serve_restart_focused(&mut self) -> Task<Message> {
        match self.focused_term().and_then(|t| t.server.clone()) {
            Some(name) => self.serve(ServeOp::Restart { name }, None),
            None => self.flash_info("restart a server from its terminal (or: nib ctl serve restart NAME)"),
        }
    }

    pub(super) fn on_serve(&mut self, call: Option<Call>, quiet: bool, result: Result<Response, RpcError>) -> Task<Message> {
        match result {
            Ok(Response::Servers(list)) => {
                // Those the daemon no longer lists are gone.
                let gone: Vec<String> = self.serve.list.iter().filter(|s| !list.iter().any(|l| l.name == s.name)).map(|s| s.name.clone()).collect();
                let mut tasks: Vec<Task<Message>> = gone.into_iter().map(|name| self.on_server_info(ServerInfo { name, removed: true, ..Default::default() })).collect();
                tasks.extend(list.into_iter().map(|s| self.on_server_info(s)));
                if let Some(c) = call {
                    c.ok(json!({ "servers": self.servers_json() }));
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
                None => self.flash_warn(format!("servers: {}", e.msg)),
            },
        }
    }

    /// The daemon says how a server is now: keep its terminal and its page in line.
    pub(super) fn on_server_info(&mut self, info: ServerInfo) -> Task<Message> {
        let name = info.name.clone();
        let before = self.serve.list.iter().find(|s| s.name == name).map(|s| s.state);
        // On another machine, a server that runs has its port brought here; one that no longer
        // does, no longer.
        let via = self.forward_via();
        let had = self.serve.list.iter().find(|s| s.name == name).and_then(|s| s.def.port);
        if let (Some(via), Some(port), true) = (&via, had.or(info.def.port), info.removed || !(info.state == ServerState::Ready || info.state.coming())) {
            crate::forward::server_gone(&via.host, port);
        }
        self.serve.list.retain(|s| s.name != name);
        let term = self.terms.iter().position(|t| t.server.as_deref() == Some(name.as_str()));
        if info.removed || (info.state == ServerState::Stopped && info.pty.is_none()) {
            // Stopped on purpose: its terminal goes, as a closed one does.
            if let Some(key) = term.map(|i| self.terms[i].key) {
                self.remove_term(key);
            }
            if info.removed {
                self.serve.viewed.retain(|n| *n != name);
                return Task::none();
            }
        } else if let Some(i) = term {
            let t = &mut self.terms[i];
            if let (Some(id), true) = (info.pty, info.pty != t.id) {
                // Started again, in the terminal it had: what it printed before stays above.
                t.parser.process(b"\r\n\x1b[2m--- started again ---\x1b[0m\r\n");
                t.queued.clear();
                let (rows, cols) = t.size;
                self.notify(ClientNotify::PtyResize { id, cols, rows });
            }
            self.terms[i].id = info.pty;
        } else if let Some(id) = info.pty {
            let (rows, cols) = (24, 80);
            let key = self.next_term_key;
            self.next_term_key += 1;
            self.terms.push(Term {
                key,
                id: Some(id),
                parser: vt100::Parser::new(rows.try_into().unwrap(), cols.try_into().unwrap(), 5000),
                size: (rows, cols),
                queued: Vec::new(),
                server: Some(name.clone()),
            });
            // Shown, but the keyboard stays where it is.
            self.terms_visible = true;
        }
        let ready = info.state == ServerState::Ready && before != Some(ServerState::Ready);
        let port = info.def.port.filter(|_| ready);
        let view = info.def.view.clone().filter(|_| !self.headless);
        self.serve.list.push(info);
        self.serve.list.sort_by(|a, b| a.name.cmp(&b.name));
        let Some(port) = port else { return Task::none() };
        // Its page: the same request `nib ctl web open URL` makes; again after a restart, its
        // tab stays where it is and shows the page afresh.
        let again = self.serve.viewed.contains(&name);
        if view.is_some() && !again {
            self.serve.viewed.push(name.clone());
        }
        let root = self.root.to_string();
        let show = move |port: u16| {
            let Some(view) = view else { return };
            crate::web::submit(Call::internal(json!({ "cmd": "web.open", "url": format!("http://127.0.0.1:{port}{view}"), "background": again })), &root);
            if again {
                crate::web::submit(Call::internal(json!({ "cmd": "web.reload", "tab": port.to_string() })), &root);
            }
        };
        match via {
            None => show(port),
            // The browser is on this machine, the server on that one: its port comes here
            // first. Every ready server's, with a page of its own or not: the frontend's page
            // asks the backend's port from the browser.
            Some(via) => drop(std::thread::spawn(move || match crate::forward::add(&via.host, &via.label, port, crate::forward::Why::Server, true) {
                Ok(made) => show(made.local),
                Err(e) => crate::forward::note(format!("server {name}: {e}"), true),
            })),
        }
        Task::none()
    }

    /// The window is on another machine (or another daemon): nothing of the old one's is left.
    pub(super) fn serve_reset(&mut self) {
        self.serve = State::default();
    }

    /// The line above a server's terminal: its name, how it is, where it listens.
    pub(super) fn serve_header(&self, t: &Term) -> Option<Element<'_, Message>> {
        let name = t.server.as_deref()?;
        let th = &self.theme;
        let info = self.serve.list.iter().find(|s| s.name == name);
        let state = info.map_or(ServerState::Stopped, |s| s.state);
        let colour = match state {
            ServerState::Ready => th.accent,
            ServerState::Exited(_) | ServerState::PortTaken => th.warn,
            _ => th.dim,
        };
        let port = info.and_then(|s| s.def.port).map(|p| format!(":{p}")).unwrap_or_default();
        let fg = if self.focus == Focus::Terminal(t.key) { th.foreground } else { th.dim };
        Some(
            row![
                text(name.to_string()).size(12).font(Font::MONOSPACE).color(fg),
                text(state.name()).size(12).font(Font::MONOSPACE).color(colour),
                text(port).size(12).font(Font::MONOSPACE).color(th.dim),
            ]
            .spacing(10)
            .padding([2, 6])
            .into(),
        )
    }

    /// What a web tab is called in the tab bar: the `title` of the server whose page it
    /// shows, if it has one, else the page's own name.
    pub(super) fn web_title(&self, page: u32) -> String {
        let titled = || {
            if !self.serve.list.iter().any(|s| s.def.title.is_some()) {
                return None;
            }
            // The engine's account of its pages has their addresses.
            let state = crate::web::state();
            let url = state["tabs"].as_array()?.iter().find(|t| t["id"] == page)?["url"].as_str()?.to_string();
            // (Where its port is on this machine, when the server is on another.)
            let via = self.forward_via();
            let here = |p: u16| via.as_ref().and_then(|v| crate::forward::local_of(&v.host, p)).unwrap_or(p);
            let s = self.serve.list.iter().find(|s| s.def.port.is_some_and(|p| url.starts_with(&format!("http://127.0.0.1:{}/", here(p)))))?;
            s.def.title.clone()
        };
        titled().unwrap_or_else(|| self.web_label(page))
    }

    /// For `state`: every server the daemon told of.
    pub(super) fn servers_json(&self) -> Value {
        self.serve
            .list
            .iter()
            .map(|s| {
                let term = self.terms.iter().find(|t| t.server.as_deref() == Some(s.name.as_str()));
                json!({
                    "name": s.name, "state": s.state.name(), "port": s.def.port, "view": s.def.view, "title": s.def.title,
                    "terminal": term.map(|t| t.key), "cmd": s.def.cmd, "after": s.def.after, "cwd": s.def.cwd, "env": s.def.env,
                })
            })
            .collect()
    }

    /// `serve.*` from the control socket.
    pub(super) fn on_serve_ctl(&mut self, call: Call) -> Task<Message> {
        let name = call.str_arg("name").map(str::to_string);
        let names: Vec<String> = call.req.get("names").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        let needs_name = |what: &str| {
            call.err(format!("serve {what} needs a server's name"));
            Task::none()
        };
        let op = match (call.cmd(), name) {
            ("serve.list", _) => ServeOp::List,
            ("serve.up", _) => ServeOp::Up { names },
            ("serve.down", _) => ServeOp::Down { names },
            ("serve.set", Some(name)) => match serde_json::from_value::<ServerDef>(call.req.get("def").cloned().unwrap_or_default()) {
                Ok(def) => ServeOp::Set { name, def },
                Err(e) => {
                    call.err(format!("serve set: {e}"));
                    return Task::none();
                }
            },
            ("serve.rm", Some(name)) => ServeOp::Remove { name },
            ("serve.restart", Some(name)) => ServeOp::Restart { name },
            ("serve.log", Some(name)) => {
                // What its terminal shows, as `state.terminals[].screen` has it.
                let Some(t) = self.terms.iter().find(|t| t.server.as_deref() == Some(name.as_str())) else {
                    call.err(format!("{name} has no terminal: it is not running, and has not ended by itself (serve list)"));
                    return Task::none();
                };
                let screen = t.parser.screen().contents();
                let all: Vec<&str> = screen.trim_end().lines().collect();
                let n = call.req.get("lines").and_then(Value::as_u64).map_or(all.len(), |n| n as usize);
                call.ok(json!({ "name": name, "terminal": t.key, "log": all[all.len().saturating_sub(n)..].join("\n") }));
                return Task::none();
            }
            ("serve.set", None) => return needs_name("set"),
            ("serve.rm", None) => return needs_name("rm"),
            ("serve.restart", None) => return needs_name("restart"),
            ("serve.log", None) => return needs_name("log"),
            (other, _) => {
                call.err(format!("unknown cmd {other:?} (serve.list, set, rm, up, down, restart, log)"));
                return Task::none();
            }
        };
        self.serve(op, Some(call))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;
    use nib_proto::{ReqId, ServerMsg, ServerNotify};

    fn info(name: &str, state: ServerState, pty: Option<u32>) -> ServerInfo {
        ServerInfo { name: name.into(), def: ServerDef { cmd: "run".into(), port: Some(8765), ..Default::default() }, state, pty, removed: false }
    }

    fn told(a: &mut App, i: ServerInfo) {
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::Server(i)));
    }

    /// The request to the daemon that is waiting for its answer.
    fn serve_request(a: &App) -> ReqId {
        *a.pending.iter().find(|(_, p)| matches!(p, Pending::Serve { .. })).expect("a Serve request").0
    }

    #[test]
    fn a_running_server_is_a_terminal_that_outlives_its_process_but_not_a_stop() {
        let mut a = app();
        a.headless = true;
        told(&mut a, info("backend", ServerState::Starting, Some(7)));
        assert_eq!(a.terms.len(), 1);
        assert!(a.terms_visible && a.focus != Focus::Terminal(a.terms[0].key), "shown, the keyboard stays");
        assert_eq!((a.terms[0].server.as_deref(), a.terms[0].id), (Some("backend"), Some(7)));
        assert_eq!(a.serve.coming(), 1);
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::PtyOutput { id: 7, data: b"Uvicorn running\r\n".to_vec() }));
        told(&mut a, info("backend", ServerState::Ready, Some(7)));
        let s = a.servers_json();
        assert_eq!((s[0]["name"].as_str(), s[0]["state"].as_str(), s[0]["port"].as_u64(), s[0]["terminal"].as_u64()), (Some("backend"), Some("ready"), Some(8765), Some(a.terms[0].key)));
        assert_eq!(a.serve.coming(), 0);
        // It ended by itself: what it printed can still be read.
        told(&mut a, info("backend", ServerState::Exited(Some(1)), None));
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::PtyExit { id: 7, code: Some(1) }));
        assert_eq!(a.terms.len(), 1);
        assert!(a.terms[0].id.is_none() && a.terms[0].parser.screen().contents().contains("Uvicorn running"));
        assert_eq!(a.servers_json()[0]["state"], "exited(1)");
        // Started again: the same terminal.
        told(&mut a, info("backend", ServerState::Starting, Some(9)));
        assert_eq!((a.terms.len(), a.terms[0].id), (1, Some(9)));
        assert!(a.terms[0].parser.screen().contents().contains("started again"));
        // Stopped: the terminal goes; removed: the server is forgotten.
        told(&mut a, info("backend", ServerState::Stopped, None));
        assert!(a.terms.is_empty() && a.servers_json()[0]["terminal"].is_null());
        told(&mut a, ServerInfo { name: "backend".into(), removed: true, ..Default::default() });
        assert_eq!(a.servers_json(), json!([]));
    }

    #[test]
    fn the_servers_are_asked_for_at_connect_and_a_refusal_then_is_not_a_message() {
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
        let id = *a.pending.iter().find(|(_, p)| matches!(p, Pending::Serve { call: None, quiet: true })).expect("the servers asked for").0;
        // A machine with no NIBDIR refuses: nothing to tell the user at every connect.
        let said = a.messages.len();
        let _ = a.on_server(ServerMsg::Response { id, result: Err(RpcError::new(nib_proto::ErrorKind::BadRequest, "no NIBDIR")) });
        assert_eq!(a.messages.len(), said);
        // Asked for by somebody, the same refusal is said.
        let _ = a.serve(ServeOp::List, None);
        let id = serve_request(&a);
        let _ = a.on_server(ServerMsg::Response { id, result: Err(RpcError::new(nib_proto::ErrorKind::BadRequest, "no NIBDIR")) });
        assert_eq!(a.messages.len(), said + 1);
    }

    #[test]
    fn closing_a_servers_terminal_closes_its_pty_and_restart_is_for_the_focused_one() {
        let mut a = app();
        a.headless = true;
        told(&mut a, info("backend", ServerState::Ready, Some(7)));
        let key = a.terms[0].key;
        let _ = a.exec(nib_editor::Command::ServeRestart);
        assert!(!a.pending.values().any(|p| matches!(p, Pending::Serve { .. })), "no server's terminal has the keyboard");
        a.set_focus(Focus::Terminal(key));
        let _ = a.exec(nib_editor::Command::ServeRestart);
        serve_request(&a);
        let _ = a.exec(nib_editor::Command::CloseTab);
        assert!(a.terms.is_empty(), "closed like any terminal (the daemon stops the server with its pty)");
    }

    #[test]
    fn the_control_socket_asks_the_daemon_and_answers_with_the_servers() {
        let mut a = app();
        a.headless = true;
        let (call, rx) = Call::test(json!({ "cmd": "serve.set", "name": "backend", "def": { "cmd": "run", "port": 8765, "after": [] } }));
        let _ = a.on_control(call);
        let id = serve_request(&a);
        let _ = a.on_server(ServerMsg::Response { id, result: Ok(Response::Servers(vec![info("backend", ServerState::Stopped, None)])) });
        let v = rx.try_recv().unwrap();
        assert_eq!((v["ok"].as_bool(), v["servers"][0]["state"].as_str(), v["servers"][0]["cmd"].as_str()), (Some(true), Some("stopped"), Some("run")));
        // The daemon's refusal is the answer.
        let (call, rx) = Call::test(json!({ "cmd": "serve.up", "names": ["nobody"] }));
        let _ = a.on_control(call);
        let id = serve_request(&a);
        let _ = a.on_server(ServerMsg::Response { id, result: Err(RpcError::new(nib_proto::ErrorKind::BadRequest, "no server named \"nobody\"")) });
        assert_eq!(rx.try_recv().unwrap(), json!({ "ok": false, "error": "no server named \"nobody\"" }));
        // The log is the terminal's screen.
        told(&mut a, info("backend", ServerState::Ready, Some(3)));
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::PtyOutput { id: 3, data: b"one\r\ntwo\r\nthree\r\n".to_vec() }));
        let (call, rx) = Call::test(json!({ "cmd": "serve.log", "name": "backend", "lines": 2 }));
        let _ = a.on_control(call);
        assert_eq!(rx.try_recv().unwrap()["log"], "two\nthree");
        for wrong in [json!({ "cmd": "serve.restart" }), json!({ "cmd": "serve.set", "name": "x", "def": { "port": 1 } }), json!({ "cmd": "serve.dance" })] {
            let (call, rx) = Call::test(wrong);
            let _ = a.on_control(call);
            assert_eq!(rx.try_recv().unwrap()["ok"], false);
        }
        // A list without it: it is gone here too.
        let (call, _rx) = Call::test(json!({ "cmd": "serve.list" }));
        let _ = a.on_control(call);
        let id = serve_request(&a);
        let _ = a.on_server(ServerMsg::Response { id, result: Ok(Response::Servers(vec![])) });
        assert!(a.terms.is_empty() && a.servers_json() == json!([]));
    }
}
