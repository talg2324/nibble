//! The window's connection to a nibd, and switching it in place.
//!
//! Connecting runs on its own thread (ssh and a nibd upload take seconds) while the window stays
//! usable. Every attempt gets a number (`gen`); everything a connection sends carries its number,
//! so whatever an old daemon still sends after a switch is dropped. A switch only happens once the
//! new daemon has answered the handshake: a failed attempt leaves the window as it was.
//!
//! What belongs to a machine is reset on a switch (tabs of its files, terminals, the file tree,
//! git, the AI chat, language-server state); the window, layout, settings and control socket stay.
//! Tabs with unsaved changes and open terminals are given up only when the person (or an agent,
//! with `force`) agreed to first.

use super::{App, Focus, Message, Pending};
use crate::control::Call;
use crate::transport::{self, Askpass, Conn, SshEnv};
use crate::Launch;
use iced::Task;
use nib_proto::{ErrorKind, Request, RpcError, ServerMsg};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// What a connection's thread reports, tagged with the attempt's number in `Message::Conn`.
#[derive(Debug, Clone)]
pub enum ConnEvent {
    /// Progress while connecting ("uploading nibd …"), and how far along (0..=1) when known.
    Status(String, Option<f32>),
    /// The daemon answered: the connection, to be taken once.
    Ready(Slot),
    /// Connecting failed.
    Failed(String),
    Server(ServerMsg),
    /// The daemon went away.
    Closed(String),
}

/// A connection handed from its thread to the app (messages must be `Clone`; this is taken once).
#[derive(Clone)]
pub struct Slot(Arc<Mutex<Option<Conn>>>);

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Slot")
    }
}

impl Slot {
    fn take(&self) -> Option<Conn> {
        self.0.lock().ok()?.take()
    }
}

/// Closing a connection waits a moment for the daemon to exit: never on the UI thread.
fn close_later(c: Conn) {
    std::thread::spawn(move || drop(c));
}

/// An attempt in progress.
pub struct Connecting {
    pub gen: u64,
    pub launch: Launch,
    pub status: String,
    /// How far along the current step is (an upload), when known: shown as a bar.
    pub fraction: Option<f32>,
    pub started: Instant,
    /// Proves an askpass request comes from this attempt's ssh.
    pub token: String,
    /// Unsaved tabs may be dropped (the person or agent agreed to).
    discard: bool,
    /// A control request waiting for the outcome (`ssh.connect`).
    call: Option<Call>,
    /// Started from the SSH panel: close it once connected.
    from_panel: bool,
}

impl App {
    /// Start connecting to `launch` in the background. Replaces an attempt in progress.
    /// `batch`: ssh may not ask anything (agents; headless always is).
    pub(super) fn connect(&mut self, launch: Launch, batch: bool, discard: bool, call: Option<Call>, from_panel: bool) -> Task<Message> {
        self.cancel_connect("replaced by another connect");
        self.next_gen += 1;
        let gen = self.next_gen;
        let token = crate::ssh::new_token();
        let batch = batch || self.headless;
        let askpass = (!batch).then(crate::control::socket).flatten().map(|socket| Askpass { socket, token: token.clone() });
        let env = SshEnv { batch, askpass };
        crate::control::log("conn", &format!("connecting to {} (attempt {gen})", launch.label));
        self.conn_error = None;
        self.connecting = Some(Connecting {
            gen,
            status: format!("connecting to {}…", launch.label),
            launch: launch.clone(),
            fraction: None,
            started: Instant::now(),
            token,
            discard,
            call,
            from_panel,
        });
        let (tx, rx) = iced::futures::channel::mpsc::unbounded::<Message>();
        let target = launch.target;
        std::thread::spawn(move || {
            let send = |e: ConnEvent| tx.unbounded_send(Message::Conn(gen, e)).is_ok();
            let progress = |s: String, f: Option<f32>| {
                send(ConnEvent::Status(s, f));
            };
            match transport::connect(&target, &env, &progress) {
                Err(e) => {
                    send(ConnEvent::Failed(format!("{e:#}")));
                }
                Ok((conn, reader)) => {
                    if !send(ConnEvent::Ready(Slot(Arc::new(Mutex::new(Some(conn)))))) {
                        return;
                    }
                    transport::read_loop(reader, |m| {
                        send(match m {
                            Ok(m) => ConnEvent::Server(m),
                            Err(e) => ConnEvent::Closed(e),
                        })
                    });
                }
            }
        });
        Task::run(rx, |m| m)
    }

    /// Stop waiting for the attempt in progress; its connection is closed if it still arrives.
    pub(super) fn cancel_connect(&mut self, why: &str) {
        if let Some(c) = self.connecting.take() {
            if let Some(call) = c.call {
                call.err(why);
            }
            self.ssh_ask_cancel(why);
        }
    }

    /// What a switch would throw away, if anything: the sentence to show before doing it.
    pub(super) fn switch_cost(&self) -> Option<String> {
        let dirty = self.tabs.iter().filter(|t| !t.local && t.doc.is_modified()).count();
        let terms = self.terms.len();
        let mut parts = Vec::new();
        if dirty > 0 {
            parts.push(if dirty == 1 { "1 unsaved file".to_owned() } else { format!("{dirty} unsaved files") });
        }
        if terms > 0 {
            parts.push(if terms == 1 { "1 terminal".to_owned() } else { format!("{terms} terminals") });
        }
        (!parts.is_empty()).then(|| format!("{} on {} will close", parts.join(" and "), self.label))
    }

    pub(super) fn on_conn(&mut self, gen: u64, ev: ConnEvent) -> Task<Message> {
        match ev {
            ConnEvent::Server(m) if gen == self.gen => {
                // The answer to a Hello: did the daemon take the root?
                let hello = match &m {
                    ServerMsg::Response { id, result } if matches!(self.pending.get(id), Some(Pending::Hello { .. })) => Some(result.is_ok()),
                    _ => None,
                };
                // A new connection's first Hello refused (another protocol, a root that isn't
                // there): the connection is useless. Close it and say so where connections are
                // shown, rather than leave every panel waiting on it.
                if let ServerMsg::Response { id, result: Err(e) } = &m {
                    if self.link == super::Link::Greeting && matches!(self.pending.get(id), Some(Pending::Hello { .. })) {
                        self.pending.remove(id);
                        return self.refused(e);
                    }
                }
                let t = self.on_server(m);
                let after = match hello {
                    Some(true) => {
                        self.greeted();
                        // The project's servers, before anybody asks: `state.servers` and the
                        // terminals of those that run are right from the start.
                        let servers = self.app_ask(nib_proto::AppOp::List, None, false, true);
                        // The AI panel stayed open through the switch: look on this machine.
                        let ai = if self.ai.open { self.ai_check(None) } else { Task::none() };
                        let browse = if std::mem::take(&mut self.ssh.browse_next) {
                            let root = self.root.clone();
                            self.ssh_browse(root)
                        } else {
                            Task::none()
                        };
                        Task::batch([ai, browse, servers])
                    }
                    // A root change the daemon refused: it kept the old root, so show that again.
                    Some(false) if self.live() => {
                        self.launch.root = self.tilde(&self.root);
                        Task::batch([self.tree_refresh(), self.git_refresh()])
                    }
                    _ => Task::none(),
                };
                if let Some(ok) = hello {
                    self.answer_connect_call(ok, None);
                }
                Task::batch([t, after])
            }
            // Already shown (a refused Hello closes its own connection).
            ConnEvent::Closed(e) if gen == self.gen && self.lost() => {
                crate::control::log("disc", &e);
                Task::none()
            }
            ConnEvent::Closed(e) if gen == self.gen => {
                crate::control::log("disc", &e);
                self.link = super::Link::Lost;
                // Forwards through the lost connection's ssh master are dead too; a reconnect
                // forwards again what is asked for.
                crate::forward::clear();
                self.ai_disconnected();
                self.flash_warn(format!(
                    "disconnected: {e} — edits kept in memory; to reconnect, pick {} in the SSH panel ({})",
                    self.label,
                    self.keymap.chords_for(nib_editor::Command::ToggleSsh).first().map_or("toolbar".to_string(), |k| k.to_string())
                ))
            }
            ConnEvent::Status(s, f) => {
                if let Some(c) = self.connecting.as_mut().filter(|c| c.gen == gen) {
                    c.status = s;
                    c.fraction = f;
                }
                Task::none()
            }
            ConnEvent::Failed(e) => {
                let Some(c) = self.connecting.take_if(|c| c.gen == gen) else { return Task::none() };
                crate::control::log("conn!", &e);
                self.ssh_ask_cancel("connecting failed");
                if let Some(call) = c.call {
                    call.err(&e);
                }
                let label = c.launch.label.clone();
                self.conn_error = Some((label.clone(), e.clone()));
                let shown = self.ssh_connect_failed(&c.launch);
                Task::batch([shown, self.flash_warn(format!("couldn't connect to {label}: {e}"))])
            }
            ConnEvent::Ready(slot) => {
                let Some(conn) = slot.take() else { return Task::none() };
                let Some(c) = self.connecting.take_if(|c| c.gen == gen) else {
                    close_later(conn);
                    return Task::none();
                };
                // Something was typed on the old machine while connecting, and nobody agreed to lose it.
                let dirty = self.tabs.iter().any(|t| !t.local && t.doc.is_modified());
                if dirty && !c.discard && !self.reconnecting_to(&c.launch) {
                    close_later(conn);
                    let msg = format!("didn't switch to {}: unsaved changes on {} — save them, then connect again", c.launch.label, self.label);
                    if let Some(call) = c.call {
                        call.err(&msg);
                    }
                    return self.flash_warn(msg);
                }
                self.switch_to(gen, conn, c)
            }
            // From a connection that has been replaced.
            ConnEvent::Server(_) | ConnEvent::Closed(_) => Task::none(),
        }
    }

    /// The daemon refused this window's Hello: drop the connection and report a failed connect.
    fn refused(&mut self, e: &RpcError) -> Task<Message> {
        if let Some(c) = self.conn.take() {
            close_later(c);
        }
        self.link = super::Link::Lost;
        self.ai_disconnected();
        let label = self.label.clone();
        let why = match e.kind {
            ErrorKind::ProtocolMismatch => format!(
                "nibd on {label} doesn't match this nib ({}). From a source checkout: scripts/build.sh daemons, then connect again",
                e.msg
            ),
            _ => e.msg.clone(),
        };
        crate::control::log("conn!", &format!("{label}: {why}"));
        self.answer_connect_call(false, Some(&why));
        self.conn_error = Some((label.clone(), why.clone()));
        let shown = self.ssh_connect_failed(&self.launch.clone());
        Task::batch([shown, self.flash_warn(format!("couldn't connect to {label}: {why}"))])
    }

    /// Make `conn` the window's connection: drop what belonged to the old machine and greet the
    /// new daemon. The `Hello` answer opens the file tree (or `launch.open`), as at startup.
    fn switch_to(&mut self, gen: u64, conn: Conn, c: Connecting) -> Task<Message> {
        // The first connection of a window has nothing to clear (and keeps `launch.open`'s tab).
        let first = self.conn.is_none();
        let again = self.reconnecting_to(&c.launch);
        if let Some(old) = self.conn.replace(conn) {
            close_later(old);
        }
        crate::control::log("conn", &format!("connected to {} (attempt {gen})", c.launch.label));
        self.gen = gen;
        self.epoch += 1;
        self.remote_home = None;
        if again {
            self.reset_daemon_state();
        } else if !first {
            self.reset_machine_state();
        }
        self.root = c.launch.root.clone();
        self.label = c.launch.label.clone();
        self.link = super::Link::Greeting;
        if let Some(old) = std::mem::replace(&mut self.connect_call, c.call) {
            old.err("replaced by another connect");
        }
        self.ssh_ask_cancel("connected");
        if c.from_panel && self.ssh.open {
            // Home is rarely the project: let the person pick the folder next.
            if c.launch.root == "~" {
                self.ssh.browse_next = true;
            } else {
                self.ssh_close();
            }
        }
        let hello = self.hello(&c.launch);
        self.launch = c.launch;
        hello
    }

    /// Greet the daemon; the answer is `Pending::Hello`.
    pub(super) fn hello(&mut self, launch: &Launch) -> Task<Message> {
        let req = Request::Hello {
            protocol: nib_proto::PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
            root: launch.root.clone(),
            lsp: self.lsp.clone(),
        };
        self.request(req, Pending::Hello { open: launch.open.clone() })
    }

    /// Make `root` (absolute, on the connected machine) the project folder: same daemon, so tabs
    /// and terminals stay; the file tree, git and the language server start over there.
    pub(super) fn change_root(&mut self, root: String) -> Task<Message> {
        if !self.live() {
            return self.flash_warn(self.not_connected());
        }
        if root == self.root {
            return Task::none();
        }
        crate::control::log("conn", &format!("root -> {root}"));
        self.epoch += 1;
        // Another project: its servers are others, and so are the ports worth having here.
        crate::forward::clear();
        // Answers about the old root would land in the new tree.
        self.pending.retain(|_, p| !matches!(p, Pending::Tree | Pending::TreeGit | Pending::TreeFs { .. } | Pending::Git { .. } | Pending::GitDetails));
        let open = self.tree.open;
        self.tree = Default::default();
        self.tree.open = open;
        self.git = None;
        let open = self.git_ui.open;
        self.git_ui = Default::default();
        self.git_ui.open = open;
        self.diff.reset();
        // The new language server knows no open documents yet.
        for t in &mut self.tabs {
            t.lsp_synced = None;
        }
        let mut l = self.launch.clone();
        l.root = self.tilde(&root);
        l.open = None;
        self.launch = l.clone();
        self.hello(&l)
    }

    /// `/home/tal/x` -> `~/x` on the connected machine, when its home is known.
    pub(super) fn tilde(&self, abs: &str) -> String {
        match &self.remote_home {
            Some(h) if abs == h => "~".into(),
            Some(h) => abs.strip_prefix(&format!("{h}/")).map(|r| format!("~/{r}")).unwrap_or_else(|| abs.to_owned()),
            None => abs.to_owned(),
        }
    }

    /// The daemon accepted the root. Learn the home folder from how `~` resolved, and remember
    /// the connection and the root in its profile (a host typed in the SSH panel is saved here,
    /// once it has worked).
    fn greeted(&mut self) {
        let asked = self.launch.root.clone();
        if let Some(rest) = asked.strip_prefix('~') {
            if let Some(home) = self.root.strip_suffix(rest).filter(|h| !h.is_empty()) {
                self.remote_home = Some(home.trim_end_matches('/').to_owned()).filter(|h| !h.is_empty());
            }
        }
        let Some(name) = self.launch.profile.clone() else { return };
        let root = self.tilde(&self.root);
        let saved = nib_editor::Config::load().map(|c| c.profiles.get(&name).cloned());
        let (mut p, new) = match saved {
            Ok(Some(p)) => (p, false),
            // New: only ssh connections typed in the panel get here without a profile.
            Ok(None) => match profile_for(&self.launch.target) {
                Some(p) => (p, true),
                None => return,
            },
            Err(e) => {
                crate::control::log("warn", &format!("couldn't remember the root: {e:#}"));
                return;
            }
        };
        if !new && p.root == root && p.roots.first().is_none_or(|r| *r == root) {
            return;
        }
        p.use_root(&root);
        if let Err(e) = nib_editor::Config::save_profile(&name, &p, None) {
            crate::control::log("warn", &format!("couldn't save @{name}: {e:#}"));
        }
        if self.ssh.open {
            self.ssh_reload();
        }
    }

    /// "Not connected", with what to do about it (for when something needs the daemon).
    pub(super) fn not_connected(&self) -> String {
        let panel = self.keymap.chords_for(nib_editor::Command::ToggleSsh).first().map_or_else(|| "the SSH panel".to_string(), |k| format!("the SSH panel ({k})"));
        match &self.connecting {
            Some(c) => format!("not connected yet: still connecting to {}", c.launch.label),
            None if self.lost() => format!("not connected: the connection to {} was lost — reconnect from {panel}", self.label),
            None => format!("not connected — connect from {panel}"),
        }
    }

    /// Connecting again to where the connection was lost: same machine, same folder. The tabs
    /// (and their unsaved edits) are still the files there, so they stay.
    pub(super) fn reconnecting_to(&self, l: &Launch) -> bool {
        self.lost() && self.launch.label == l.label && self.launch.target == l.target && self.launch.root == l.root
    }

    /// Reconnecting: only what lived in the old daemon goes (requests, terminals, servers,
    /// forwards, language servers). Tabs, unsaved edits, the agents panel and the tree stay.
    fn reset_daemon_state(&mut self) {
        self.pending.clear();
        self.sent.clear();
        self.terms.clear();
        self.terms_visible = false;
        self.last_term = None;
        self.serve_reset();
        crate::forward::clear();
        self.code = Default::default();
        for t in &mut self.tabs {
            t.lsp_synced = None;
        }
        self.armed = None;
        if matches!(self.focus, Focus::Terminal(_)) {
            self.focus = Focus::Main;
        }
    }

    /// Everything that belongs to the machine the window was connected to.
    fn reset_machine_state(&mut self) {
        self.pending.clear();
        self.sent.clear();
        // config.toml and other local tabs don't live on the daemon's machine.
        self.tabs.retain(|t| t.local);
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        self.closed.clear();
        self.terms.clear();
        self.serve_reset();
        // What was forwarded from the old machine is not the new one's.
        crate::forward::clear();
        self.terms_visible = false;
        self.last_term = None;
        let open = self.tree.open;
        self.tree = Default::default();
        self.tree.open = open;
        self.git = None;
        let open = self.git_ui.open;
        self.git_ui = Default::default();
        self.git_ui.open = open;
        self.diff.reset();
        self.ai = self.ai.for_another_machine();
        self.code = Default::default();
        self.preview_tab = None;
        self.armed = None;
        if !matches!(self.focus, Focus::Ssh | Focus::Toolbar) {
            self.focus = Focus::Main;
        }
    }

    /// `ssh.connect` / `ssh.root` answer once the daemon has answered `Hello`.
    fn answer_connect_call(&mut self, ok: bool, why: Option<&str>) {
        let Some(call) = self.connect_call.take() else { return };
        if ok {
            call.ok(json!({ "label": self.label, "root": self.root }));
        } else {
            let why = why
                .map(str::to_owned)
                .or_else(|| self.messages.back().map(|(m, _)| m.clone()))
                .unwrap_or_else(|| "the daemon refused the handshake".into());
            call.err(why);
        }
    }

    /// `state.connection` and `ssh.list.current`.
    pub(super) fn connection_json(&self) -> Value {
        json!({
            "label": self.label,
            "profile": self.launch.profile,
            "target": target_text(&self.launch.target),
            "root": self.root,
            "connected": self.live(),
            "connecting": self.connecting.as_ref().map(|c| json!({
                "label": c.launch.label,
                "profile": c.launch.profile,
                "status": c.status,
                "seconds": c.started.elapsed().as_secs(),
            })),
            "error": self.conn_error.as_ref().map(|(label, e)| json!({ "label": label, "error": e })),
        })
    }
}

/// A profile that reaches `t` (for saving a host typed in the panel).
fn profile_for(t: &nib_editor::Target) -> Option<nib_editor::Profile> {
    let nib_editor::Target::Ssh { ssh, nibd } = t else { return None };
    Some(nib_editor::Profile {
        ssh: Some(ssh.host.clone()),
        user: ssh.user.clone(),
        port: ssh.port,
        identity: ssh.identity.clone(),
        jump: ssh.jump.clone(),
        nibd: nibd.clone(),
        root: "~".into(),
        ..Default::default()
    })
}

/// How a target reads to people: `tal@gpu-box:2222`, `wsl Ubuntu`, `this machine`.
pub(super) fn target_text(t: &nib_editor::Target) -> String {
    match t {
        nib_editor::Target::Local { .. } => "this machine".into(),
        nib_editor::Target::Ssh { ssh, .. } => ssh.display(),
        nib_editor::Target::Wsl { distro, .. } => format!("wsl {distro}"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;
    use nib_editor::{Document, Target};
    use std::path::PathBuf;

    fn to(label: &str, root: &str) -> Launch {
        Launch { target: Target::Local { nibd: None }, root: root.into(), open: None, label: label.into(), profile: Some(label.trim_start_matches('@').into()) }
    }
    /// An attempt as `connect` would leave it, without starting a real one.
    fn attempt(a: &mut App, gen: u64, l: Launch, discard: bool) {
        a.next_gen = gen;
        let c = Connecting { gen, launch: l, status: String::new(), fraction: None, started: Instant::now(), token: "t".into(), discard, call: None, from_panel: false };
        a.connecting = Some(c);
    }
    fn ready() -> ConnEvent {
        ConnEvent::Ready(Slot(Arc::new(Mutex::new(Some(crate::transport::sink())))))
    }
    fn open(a: &mut App, path: &str, dirty: bool) {
        let mut d = Document::new(Default::default(), Some(PathBuf::from(path)));
        if dirty {
            d.type_char('x');
        }
        a.put_doc(d, true);
    }

    #[test]
    fn a_switch_clears_the_old_machine_and_ignores_what_it_still_sends() {
        let mut a = app();
        open(&mut a, "/tmp/x.py", false);
        let _ = a.update(Message::Control(nib_editor::Command::NewTerminal));
        assert_eq!((a.tabs.len(), a.terms.len()), (1, 1));
        assert!(a.switch_cost().is_some_and(|c| c.contains("1 terminal")));

        attempt(&mut a, 5, to("@b", "/srv"), true);
        let _ = a.update(Message::Conn(5, ready()));
        assert!(a.connecting.is_none());
        assert_eq!((a.gen, a.label.as_str(), a.root.as_str()), (5, "@b", "/srv"));
        assert!(a.tabs.is_empty() && a.terms.is_empty() && a.git.is_none());
        assert!(a.pending.values().all(|p| matches!(p, Pending::Hello { .. })) && a.pending.len() == 1, "only the new Hello");

        // The old connection hanging up, its git poll, or a stale attempt: nothing happens.
        let _ = a.update(Message::Conn(0, ConnEvent::Closed("gone".into())));
        let _ = a.update(Message::GitPoll(0));
        let _ = a.update(Message::Conn(3, ready()));
        assert!(!a.lost() && a.pending.len() == 1 && a.gen == 5);
        // The current one hanging up does.
        let _ = a.update(Message::Conn(5, ConnEvent::Closed("gone".into())));
        assert!(a.lost());
    }

    #[test]
    fn reconnecting_where_the_connection_was_lost_keeps_the_tabs() {
        let mut a = app();
        open(&mut a, "/tmp/x.py", true);
        let _ = a.update(Message::Control(nib_editor::Command::NewTerminal));
        let _ = a.update(Message::Conn(0, ConnEvent::Closed("gone".into())));
        assert!(a.lost());
        let here = a.launch.clone();
        assert!(a.reconnecting_to(&here));
        // Not asked to discard (a reconnect never is): the edits come along, the dead terminal not.
        attempt(&mut a, 6, here, false);
        let _ = a.update(Message::Conn(6, ready()));
        assert_eq!(a.gen, 6, "connected");
        assert!(!a.lost());
        assert_eq!(a.tabs.len(), 1);
        assert!(a.tabs[0].doc.is_modified(), "unsaved edits kept");
        assert!(a.terms.is_empty());
        // Somewhere else is a switch, as before.
        assert!(!a.reconnecting_to(&to("@b", "/srv")));
    }

    #[test]
    fn unsaved_work_typed_while_connecting_stops_the_switch() {
        let mut a = app();
        attempt(&mut a, 2, to("@b", "/srv"), false);
        open(&mut a, "/tmp/x.py", true);
        let _ = a.update(Message::Conn(2, ready()));
        assert_eq!((a.gen, a.label.as_str(), a.tabs.len()), (0, "local", 1), "stayed, with the file");
        assert!(a.connecting.is_none());

        let _ = a.update(Message::Conn(9, ConnEvent::Failed("nope".into())));
        assert!(a.conn_error.is_none(), "a failure of an attempt nobody waits for is ignored");
        attempt(&mut a, 10, to("@c", "/c"), false);
        let _ = a.update(Message::Conn(10, ConnEvent::Status("uploading nibd".into(), Some(0.5))));
        let c = a.connecting.as_ref().unwrap();
        assert_eq!((c.status.as_str(), c.fraction), ("uploading nibd", Some(0.5)));
        let _ = a.update(Message::Conn(10, ConnEvent::Failed("Permission denied (publickey).".into())));
        assert_eq!(a.conn_error.as_ref().map(|e| e.0.as_str()), Some("@c"));
        assert_eq!(a.label, "local", "a failed attempt leaves the window as it was");
    }
}
