//! Answers control-socket requests (`crate::control`): what the window shows, as JSON, and the
//! same inputs a user has (named commands, key chords, typing, opening files).

use super::{Focus, Message, App};
use crate::control::Call;
use crate::media::Media;
use iced::Task;
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::Command;
use serde_json::{json, Value};

/// Most recent flash messages kept for `state`.
pub(super) const MESSAGE_LOG: usize = 50;

impl App {
    pub(super) fn on_control(&mut self, call: Call) -> Task<Message> {
        match call.cmd() {
            "hello" => {
                call.ok(json!({
                    "pid": std::process::id(),
                    "version": env!("CARGO_PKG_VERSION"),
                    "title": self.title(),
                    "label": self.label,
                    "root": self.root,
                    "headless": self.headless,
                    "session": crate::control::session(),
                }));
                Task::none()
            }
            "state" => {
                call.ok(self.control_state());
                Task::none()
            }
            "pending" => {
                // Connecting counts: `wait` should wait for it.
                // So does a server on its way to ready.
                call.ok(json!({ "pending": self.pending.len() + usize::from(self.connecting.is_some()) + self.serve.coming() }));
                Task::none()
            }
            "askpass" => self.on_askpass(call),
            "search" => self.on_search_ctl(call),
            c if c.starts_with("serve.") => self.on_serve_ctl(call),
            c if c.starts_with("forward.") => self.on_forward_ctl(call),
            c if c.starts_with("ssh.") => self.on_ssh_ctl(call),
            // The mouse in the web tab in front, as the user's own: through the tab, so that
            // what the tab does with a press (its own list for a <select>) happens too.
            "web.pointer" => {
                use super::web::Msg;
                if self.web_front().is_none() {
                    call.err("no web tab is in front (`web open` brings one forward; not in --headless)");
                    return Task::none();
                }
                let n = |k: &str| call.req.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32;
                let button = call.req.get("button").and_then(Value::as_u64).unwrap_or(0).min(2) as u8;
                let mut tasks = vec![self.handle(Message::Web(Msg::Move(iced::Point::new(n("x"), n("y")))))];
                match call.str_arg("do").unwrap_or("click") {
                    "move" => {}
                    "down" => tasks.push(self.handle(Message::Web(Msg::Down(button)))),
                    "up" => tasks.push(self.handle(Message::Web(Msg::Up(button)))),
                    "wheel" => tasks.push(self.handle(Message::Web(Msg::Scroll(iced::mouse::ScrollDelta::Pixels { x: -n("dx"), y: -n("dy") })))),
                    "click" => {
                        tasks.push(self.handle(Message::Web(Msg::Down(button))));
                        tasks.push(self.handle(Message::Web(Msg::Up(button))));
                    }
                    other => {
                        call.err(format!("web.pointer: do is move, click, down, up or wheel, not {other:?}"));
                        return Task::none();
                    }
                }
                call.ok(json!({}));
                Task::batch(tasks)
            }
            // A port or a local address means the daemon's machine: forwarded first when that is
            // another one, then the engine's like the rest.
            "web.open" => {
                self.web_open(call);
                Task::none()
            }
            // The web tab's engine answers these itself, on its own thread (crate::web).
            c if c.starts_with("web.") => {
                crate::web::submit(call, &self.root.to_string());
                Task::none()
            }
            "commands" => {
                let names: Vec<&str> = Command::ALL.iter().map(|c| c.name()).collect();
                call.ok(json!({ "commands": names }));
                Task::none()
            }
            "run" => {
                let Some(c) = call.str_arg("command").and_then(Command::from_name) else {
                    call.err("unknown command; `nib ctl commands` lists them (snake_case, e.g. move_doc_end)");
                    return Task::none();
                };
                let t = self.handle(Message::Control(c));
                call.ok(json!({}));
                t
            }
            "key" => {
                let chord = match call.str_arg("chord").map(KeyChord::parse) {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => {
                        call.err(e);
                        return Task::none();
                    }
                    None => {
                        call.err("key needs \"chord\"");
                        return Task::none();
                    }
                };
                let t = self.press(chord, None);
                call.ok(json!({}));
                t
            }
            "type" => {
                let Some(text) = call.str_arg("text").map(str::to_owned) else {
                    call.err("type needs \"text\"");
                    return Task::none();
                };
                let mut tasks = Vec::new();
                for c in text.chars() {
                    tasks.push(match c {
                        '\n' => self.press(KeyChord::new(Key::Enter), None),
                        '\t' => self.press(KeyChord::new(Key::Tab), None),
                        c => self.press(KeyChord::new(Key::Char(c)), Some(c.to_string())),
                    });
                }
                call.ok(json!({}));
                Task::batch(tasks)
            }
            "ai.pick" => {
                // The AI panel's assistant / model dropdown, open on `agent`'s models if given.
                let want = call.str_arg("agent").filter(|s| !s.is_empty());
                let agent = match want.map(ai_agent) {
                    Some(None) => return self.ai_agent_unknown(call, "ai pick"),
                    Some(a) => a,
                    None => None,
                };
                self.ai.open = true;
                let mut tasks = vec![self.ai_pick()];
                if let Some(a) = agent {
                    tasks.push(self.handle(Message::Ai(super::ai::Msg::Tab(a))));
                }
                call.ok(json!({}));
                Task::batch(tasks)
            }
            "ai.warm" => {
                // A local model's 🔥 / ❄️ switch in the dropdown.
                let Some(model) = call.str_arg("model").filter(|m| !m.is_empty()).map(str::to_string) else {
                    call.err("ai warm / ai cold needs a model, e.g. ollama/qwen3:8b");
                    return Task::none();
                };
                let warm = call.req.get("warm").and_then(Value::as_bool).unwrap_or(true);
                let t = self.ai_warm(model.clone(), warm);
                call.ok(json!({ "model": model, "warm": warm }));
                t
            }
            "ai.update" => {
                // The dropdown's Update button.
                let Some(agent) = ai_agent(call.str_arg("agent").unwrap_or_default()) else {
                    return self.ai_agent_unknown(call, "ai update");
                };
                self.ai.open = true;
                let t = self.handle(Message::Ai(super::ai::Msg::Update(agent)));
                call.ok(json!({ "agent": agent.name() }));
                t
            }
            "ai.install" => {
                // The dropdown's Install / Install Ollama button.
                let Some(agent) = ai_agent(call.str_arg("agent").unwrap_or_default()) else {
                    return self.ai_agent_unknown(call, "ai install");
                };
                self.ai.open = true;
                let t = self.handle(Message::Ai(super::ai::Msg::Setup(agent)));
                call.ok(json!({ "agent": agent.name() }));
                t
            }
            "ai.pull" | "ai.rm" => {
                // OpenCode's tab: the download field, or a model's 🗑 (without asking first).
                let Some(model) = call.str_arg("model").filter(|m| !m.trim().is_empty()).map(str::to_string) else {
                    call.err("ai pull / ai rm needs an Ollama model, e.g. qwen3:8b");
                    return Task::none();
                };
                let t = if call.cmd() == "ai.pull" { self.ai_pull(model.clone()) } else { self.ai_delete_now(model.clone()) };
                call.ok(json!({ "model": model }));
                t
            }
            "ai.past" => {
                // The history tab's list (`state.ai.history.items`): reopen one, replay it in
                // Follow, or delete it (both presses at once).
                use super::ai::Msg;
                let i = call.req.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if i >= self.ai_state_json()["history"]["items"].as_array().map_or(0, Vec::len) {
                    call.err("no such conversation in the list (open the history tab first: run agent_history)");
                    return Task::none();
                }
                let t = match call.str_arg("do") {
                    Some("replay") => self.handle(Message::Ai(Msg::HistoryReplay(i))),
                    Some("forget") => Task::batch([self.handle(Message::Ai(Msg::HistoryDelete(i))), self.handle(Message::Ai(Msg::HistoryDelete(i)))]),
                    _ => self.handle(Message::Ai(Msg::HistoryOpen(i))),
                };
                call.ok(json!({}));
                t
            }
            "ai.follow" => {
                // Follow the shown agent in the editor, or stop following.
                let on = call.req.get("on").and_then(Value::as_bool).unwrap_or(true);
                let t = if on != self.ai_following_shown() { self.handle(Message::Ai(super::ai::Msg::Follow)) } else { Task::none() };
                call.ok(json!({ "following": on }));
                t
            }
            "ai.side" => {
                // The pane beside the steps: `side` (terminal, skills, system; `prompt` too), or the older
                // `skills` switch, or none (`"off": true`).
                use super::ai::Side;
                let off = call.req.get("off").and_then(Value::as_bool).unwrap_or(false);
                let skills = call.req.get("skills").and_then(Value::as_bool).unwrap_or(false);
                let side = match call.str_arg("side") {
                    Some(s) => match [Side::Terminal, Side::Skills, Side::Prompt].into_iter().find(|x| x.name() == s || (s == "prompt" && *x == Side::Prompt)) {
                        Some(x) => x,
                        None => {
                            call.err("ai side needs one of: terminal, skills, system");
                            return Task::none();
                        }
                    },
                    None if skills => Side::Skills,
                    None => Side::Terminal,
                };
                let t = self.handle(Message::Ai(super::ai::Msg::Side((!off).then_some(side))));
                call.ok(json!({}));
                t
            }
            "ai.effort" => {
                // The thinking dropdown, for the shown agent: one of its assistant's levels, or `default`.
                let level = call.str_arg("level").unwrap_or_default().to_string();
                let levels = self.ai_efforts();
                if level != "default" && !levels.iter().any(|l| *l == level) {
                    call.err(format!("ai effort needs one of: default, {}", levels.join(", ")));
                    return Task::none();
                }
                let e = if level == "default" { super::ai::Effort::Default } else { super::ai::Effort::Level(level.clone()) };
                let t = self.handle(Message::Ai(super::ai::Msg::Effort(e)));
                call.ok(json!({ "effort": level }));
                t
            }
            "ai.prompt" => {
                // Replace the shown agent's system prompt.
                let text = call.str_arg("text").unwrap_or_default().to_string();
                self.ai_set_prompt(&text);
                call.ok(json!({}));
                Task::none()
            }
            "ai.mode" => {
                // The agents tab's mode switch, for the shown agent.
                let Some(mode) = call.str_arg("mode").and_then(nib_proto::AiMode::from_name) else {
                    call.err("ai mode needs one of: manual, accept-edits, plan, auto");
                    return Task::none();
                };
                let t = self.handle(Message::Ai(super::ai::Msg::Mode(mode)));
                call.ok(json!({ "mode": mode.name() }));
                t
            }
            "ai.answer" => {
                // Allow or refuse what the shown agent asked leave for.
                if !self.ai_waiting() {
                    call.err("the shown agent isn't waiting for an answer");
                    return Task::none();
                }
                // Answers to its questions, one each.
                if let Some(given) = call.req.get("answers").and_then(Value::as_array) {
                    let given: Vec<String> = given.iter().filter_map(Value::as_str).map(str::to_string).collect();
                    match self.ai_answer_with(&given) {
                        Ok(()) => call.ok(json!({ "answered": given.len() })),
                        Err(e) => call.err(e),
                    }
                    return Task::none();
                }
                let allow = call.req.get("allow").and_then(Value::as_bool).unwrap_or(false);
                let message = call.str_arg("message").filter(|m| !m.is_empty()).map(str::to_string);
                self.ai_answer(allow, message);
                call.ok(json!({ "allowed": allow }));
                Task::none()
            }
            "diff.open" => {
                let scope = match call.str_arg("scope") {
                    None => self.diff_scope_named("head"),
                    Some(name) => self.diff_scope_named(name),
                };
                let Some(scope) = scope else {
                    call.err("no such scope: head, unstaged, staged, everyone, agent:N or an agent's name");
                    return Task::none();
                };
                call.ok(json!({}));
                self.diff_show(scope)
            }
            "diff.review" => {
                let (what, target, body) = (call.str_arg("what").unwrap_or(""), call.str_arg("target"), call.str_arg("body"));
                match self.diff_review_ctl(what, target, body) {
                    Ok(task) => {
                        call.ok(json!({}));
                        task
                    }
                    Err(e) => {
                        call.err(e);
                        Task::none()
                    }
                }
            }
            "ai.diff" => {
                if self.ai_shown_session().is_none() {
                    call.err("the shown agent has no session yet");
                    return Task::none();
                }
                call.ok(json!({}));
                self.ai_accept_diff()
            }
            "ai.accept" => {
                // The shown agent said its job is done: accept (it is ended), or keep it.
                if !self.ai_finished_shown() {
                    call.err("the shown agent hasn't said its job is done");
                    return Task::none();
                }
                let yes = call.req.get("yes").and_then(Value::as_bool).unwrap_or(false);
                call.ok(json!({ "accepted": yes }));
                self.ai_accept(yes)
            }
            "drop" => {
                // As if files were dragged onto the tree's folder `dir` or onto the agents chat.
                let paths: Vec<std::path::PathBuf> =
                    call.req.get("paths").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(Into::into).collect();
                let chat = call.str_arg("to") == Some("chat");
                if paths.is_empty() || !(chat || call.str_arg("to") == Some("tree")) {
                    call.err("drop needs \"to\": \"tree\" or \"chat\", and \"paths\"");
                    return Task::none();
                }
                let dir = call.str_arg("dir").unwrap_or_default().to_string();
                let t = self.drop_to(chat, &dir, paths);
                call.ok(json!({}));
                t
            }
            "ai.image" => {
                // As pasting a picture into the message box: a file on this machine.
                let path = call.str_arg("path").unwrap_or_default().to_string();
                match super::ai::Pic::from_file(std::path::Path::new(&path)) {
                    Ok(pic) => {
                        let t = self.handle(Message::Ai(super::ai::Msg::Clip(super::ai::Clip::Image(pic))));
                        call.ok(json!({}));
                        t
                    }
                    Err(e) => {
                        call.err(e);
                        Task::none()
                    }
                }
            }
            "ai.use" => {
                // The same as picking a model in the AI panel's dropdown; by name, any case.
                let Some(agent) = ai_agent(call.str_arg("agent").unwrap_or_default()) else {
                    return self.ai_agent_unknown(call, "ai use");
                };
                let model = call.str_arg("model").map(str::to_string);
                self.ai.open = true;
                let t = self.handle(Message::Ai(super::ai::Msg::Use(agent, model.clone())));
                call.ok(json!({ "agent": agent.name(), "model": model }));
                t
            }
            "open" => {
                let Some(p) = call.str_arg("path") else {
                    call.err("open needs \"path\"");
                    return Task::none();
                };
                let path = nib_proto::path::join(&self.root, p);
                // `line` (1-based): the cursor goes there, scrolled into view.
                let goto = call.req.get("line").and_then(Value::as_u64).map(|l| nib_proto::Pos { line: (l as u32).saturating_sub(1), col: 0 });
                let t = self.open_path_at(path.clone(), true, goto);
                call.ok(json!({ "path": path }));
                t
            }
            "snapshot" if self.headless => {
                call.err("headless: there is no window to snapshot (use state)");
                Task::none()
            }
            "snapshot" => {
                let path = call.str_arg("path").map(std::path::PathBuf::from);
                iced::window::latest()
                    .and_then(iced::window::screenshot)
                    .map(move |shot| Message::ControlShot(call.clone(), path.clone(), shot))
            }
            "" => {
                call.err("a request needs \"cmd\"");
                Task::none()
            }
            other => {
                call.err(format!("unknown cmd {other:?}"));
                Task::none()
            }
        }
    }

    /// Press and release `chord`, as the keyboard would.
    fn press(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let down = self.handle(Message::Key { chord, text, repeat: false });
        let up = self.handle(Message::KeyUp(chord.key));
        Task::batch([down, up])
    }

    /// Encode the screenshot off the UI thread and answer with where it went.
    pub(super) fn control_shot(call: Call, path: Option<std::path::PathBuf>, shot: iced::window::Screenshot) {
        std::thread::spawn(move || {
            let Some(path) = path.or_else(crate::control::next_snapshot_path) else {
                call.err("no path given and no run directory to save in");
                return;
            };
            let (w, h) = (shot.size.width, shot.size.height);
            match image::RgbaImage::from_raw(w, h, shot.rgba.to_vec()).map(|img| img.save(&path)) {
                Some(Ok(())) => call.ok(json!({ "path": path, "width": w, "height": h, "scale": shot.scale_factor })),
                Some(Err(e)) => call.err(format!("saving {}: {e}", path.display())),
                None => call.err("screenshot had the wrong size"),
            }
        });
    }

    fn control_state(&self) -> Value {
        let focus = match self.focus {
            Focus::Tree => "tree".to_owned(),
            Focus::Tabs => "tabs".to_owned(),
            Focus::Main => "editor".to_owned(),
            Focus::Terminal(k) => format!("terminal:{k}"),
            Focus::Toolbar => "toolbar".to_owned(),
            Focus::Git => "git".to_owned(),
            Focus::Ai => "ai".to_owned(),
            Focus::Ssh => "ssh".to_owned(),
        };
        let tabs: Vec<Value> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                json!({
                    "index": i,
                    "active": i == self.active,
                    "path": t.doc.path,
                    "kind": format!("{:?}", t.kind).to_lowercase(),
                    "language": t.doc.language_name(),
                    "modified": t.doc.is_modified(),
                    "rendered": t.rendered,
                "web": t.web,
                    "pdf": t.media.as_ref().and_then(Media::pdf).map(crate::pdf::PdfView::state_json),
                })
            })
            .collect();
        let editor = self.tab().map(|t| {
            let d = &t.doc;
            // 1-based line/column, like the editor shows.
            let lc = |pos: usize| {
                let (l, c) = d.line_col_of(pos);
                json!([l + 1, c + 1])
            };
            let primary = d.selection.primary_index();
            let selections: Vec<Value> = d
                .selection
                .ranges()
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    json!({
                        "primary": i == primary,
                        "anchor": lc(r.anchor),
                        "head": lc(r.head),
                        "text": d.text.slice(r.from()..r.to()).to_string(),
                    })
                })
                .collect();
            let total = d.text.len_lines();
            let end = (t.top + self.rows).min(total);
            let visible: Vec<String> =
                (t.top..end).map(|l| d.text.line(l).to_string().trim_end_matches(['\n', '\r']).to_owned()).collect();
            json!({
                "path": d.path,
                "cursor": lc(d.selection.primary().head),
                "selections": selections,
                "lines": total,
                "top": t.top + 1,
                "visible": visible,
                "read_only": d.read_only,
            })
        });
        let terminals: Vec<Value> = self
            .terms
            .iter()
            .map(|t| {
                let s = t.parser.screen();
                let (row, col) = s.cursor_position();
                json!({
                    "key": t.key,
                    "focused": self.focus == Focus::Terminal(t.key),
                    "started": t.id.is_some(),
                    "server": t.server,
                    "size": [t.size.0, t.size.1],
                    "cursor": [row + 1, col + 1],
                    "screen": s.contents(),
                })
            })
            .collect();
        // The engine's account of the pages, and the window's of its web tabs.
        let mut web = crate::web::state();
        if let Some(o) = web.as_object_mut() {
            o.insert("window".into(), self.web_json());
        }
        json!({
            "title": self.title(),
            "headless": self.headless,
            "session": crate::control::session(),
            "label": self.label,
            "root": self.root,
            "connected": self.connected && !self.disconnected,
            "connection": self.connection_json(),
            // Scratch and NIBDIR on the daemon's machine, and which one the file tree shows.
            "roots": self.tree.places_json(),
            "focus": focus,
            "grid": { "rows": self.rows, "cols": self.cols },
            "panels": {
                "tree": self.tree.open,
                "terminals": self.terms_visible,
                "git": self.git_ui.open,
                "ai": self.ai.open,
                "ssh": self.ssh.open,
                "settings": self.settings.page.map(|p| format!("{p:?}").to_lowercase()),
            },
            "ai": self.ai_state_json(),
            "diff": self.diff_json(),
            "web": web,
            "search": self.search_json(),
            "pending": self.pending.len(),
            "flash": self.flash.as_ref().map(|(text, warn, _)| json!({ "text": text, "warn": warn })),
            "messages": self.messages.iter().map(|(text, warn)| json!({ "text": text, "warn": warn })).collect::<Vec<_>>(),
            "tabs": tabs,
            "editor": editor,
            "terminals": terminals,
            // The project's servers, as the daemon last said (`serve list` asks it).
            "servers": self.servers_json(),
            // Another machine's ports brought to this one (`forward list`).
            "forwards": crate::forward::list(),
        })
    }

    fn ai_agent_unknown(&self, call: Call, what: &str) -> Task<Message> {
        let names: Vec<&str> = nib_proto::AiAgent::ALL.iter().map(|a| a.name()).collect();
        call.err(format!("{what} needs an assistant: one of {}", names.join(", ")));
        Task::none()
    }
}

/// An assistant by name ("Claude Code" or "claude"), any case.
fn ai_agent(name: &str) -> Option<nib_proto::AiAgent> {
    let want = name.to_lowercase();
    nib_proto::AiAgent::ALL.into_iter().find(|a| a.name().to_lowercase() == want || format!("{a:?}").to_lowercase() == want)
}
