//! Another machine's ports in this window (`crate::forward`): which machine the window's
//! `localhost` means, links clicked in a terminal, and `nib ctl forward …`.

use super::{viewers, App, Message};
use crate::control::Call;
use crate::forward::{self, Via, Why};
use crate::web::local;
use iced::Task;
use nib_editor::Target;
use serde_json::{json, Value};

impl App {
    /// The machine the window's daemon is on, when its ports have to be forwarded to be seen
    /// here: one reached by ssh. A local daemon's ports are this machine's. So are those of a
    /// daemon in WSL (from Windows): WSL2 shares localhost with Windows, nothing is forwarded.
    pub(super) fn forward_via(&self) -> Option<Via> {
        match &self.launch.target {
            Target::Ssh { ssh, .. } if self.conn.is_some() && !self.lost() => Some(Via { host: ssh.clone(), label: self.label.clone() }),
            _ => None,
        }
    }

    /// `web.open`, from anywhere (the socket, an agent, a link, a server's page): the port it
    /// names is the daemon's machine's, forwarded first when that is not this one.
    pub(super) fn web_open(&self, call: Call) {
        forward::open(call, self.forward_via(), self.root.to_string());
    }

    /// A link clicked in a terminal. A page of the machine the terminal runs on is shown in a
    /// web tab, when there is a browser nib can drive; anything else, as ever, goes to the
    /// system's browser on the machine at the keyboard (a person's click may open it).
    pub(super) fn open_link(&mut self, url: &str) {
        let spelled = local::spell_out(url);
        if local::is_local(&spelled) && !self.headless && !crate::web::state()["browser"].is_null() {
            self.web_open(Call::internal(json!({ "cmd": "web.open", "url": url })));
        } else if url.contains("://") {
            viewers::open_external(url);
        } else {
            // `localhost:8000`, as a server printed it.
            viewers::open_external(&spelled);
        }
    }

    /// `forward.*` from the control socket.
    pub(super) fn on_forward_ctl(&mut self, call: Call) -> Task<Message> {
        match call.cmd() {
            "forward.list" => call.ok(json!({ "forwards": forward::list() })),
            "forward.rm" => match call.req.get("local").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()) {
                Some(p) if forward::remove(p) => call.ok(json!({ "forwards": forward::list() })),
                Some(p) => call.err(format!("no forward on this machine's port {p} (`forward list` shows them)")),
                None => call.err("forward rm needs \"local\": the port on this machine"),
            },
            "forward.add" => {
                let Some(port) = call.req.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).filter(|p| *p != 0) else {
                    call.err("forward add needs \"port\": 1 to 65535");
                    return Task::none();
                };
                let profile = call.str_arg("profile").map(str::to_string);
                let via = self.forward_via();
                if profile.is_none() && via.is_none() {
                    call.err("the window is on this machine: its ports need no forward (another machine's: forward add PROFILE:PORT)");
                    return Task::none();
                }
                // ssh takes its time: not here.
                std::thread::spawn(move || {
                    let made = match profile {
                        Some(name) => forward::profile_host(&name).and_then(|host| forward::add(&host, &format!("@{name}"), port, Why::Asked, via.as_ref().is_some_and(|v| v.host == host))),
                        None => via.ok_or_else(|| "not connected".to_string()).and_then(|v| forward::add(&v.host, &v.label, port, Why::Asked, true)),
                    };
                    match made {
                        Ok(m) => call.ok(json!({ "forward": m.json(), "forwards": forward::list() })),
                        Err(e) => call.err(e),
                    }
                });
            }
            other => call.err(format!("unknown cmd {other:?} (forward.list, add, rm)")),
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;

    #[test]
    fn a_window_on_this_machine_forwards_nothing_and_says_so() {
        let mut a = app();
        assert!(a.forward_via().is_none());
        let (call, rx) = Call::test(json!({ "cmd": "forward.add", "port": 8000 }));
        let _ = a.on_control(call);
        assert!(rx.try_recv().unwrap()["error"].as_str().unwrap().contains("on this machine"));
        let (call, rx) = Call::test(json!({ "cmd": "forward.list" }));
        let _ = a.on_control(call);
        assert_eq!(rx.try_recv().unwrap(), json!({ "ok": true, "forwards": [] }));
        for wrong in [json!({ "cmd": "forward.rm", "local": 1 }), json!({ "cmd": "forward.rm" }), json!({ "cmd": "forward.add" }), json!({ "cmd": "forward.add", "port": 70000 }), json!({ "cmd": "forward.dance" })] {
            let (call, rx) = Call::test(wrong);
            let _ = a.on_control(call);
            assert_eq!(rx.try_recv().unwrap()["ok"], false);
        }
        // A connection nobody saved is refused by name, off the UI thread.
        let (call, rx) = Call::test(json!({ "cmd": "forward.add", "port": 6006, "profile": "no-such-connection-zz9" }));
        let _ = a.on_control(call);
        let v = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(v["ok"] == false && v["error"].as_str().unwrap().contains("no-such-connection-zz9"), "{v}");
    }

    #[test]
    fn a_window_on_another_machine_means_that_machines_ports() {
        let mut a = app();
        a.launch.target = Target::Ssh { ssh: nib_editor::SshHost { host: "gpu.lab".into(), user: None, port: None, identity: None, jump: None }, nibd: None };
        a.label = "gpu".into();
        let via = a.forward_via().expect("an ssh window has a remote");
        assert_eq!((via.host.host.as_str(), via.label.as_str()), ("gpu.lab", "gpu"));
        a.link = crate::app::Link::Lost;
        assert!(a.forward_via().is_none(), "nothing to forward through once it is gone");
    }
}
