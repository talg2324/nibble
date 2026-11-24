//! Ports of another machine, brought to this one.
//!
//! The web tab's browser runs on the machine at the keyboard and shows only that machine's
//! pages (`web::local`). When the window's daemon is on another machine (SSH), `localhost:8000`
//! printed in its terminal, a server's `port` in apps.json and `web open 8000` all mean the
//! *remote* machine's port: it is forwarded first, to the same port number here when that is
//! free (pages carry absolute `http://127.0.0.1:8765` addresses), else to the next free one,
//! with a warning naming both. The page is then opened at `127.0.0.1:LOCAL`, which is local by
//! the web tab's rule: nothing about that rule changes here.
//!
//! - The window's own remote, on Unix: `ssh -O forward -L …` on the shared connection nib keeps
//!   to the host (`transport`), `-O cancel` to remove. Nothing new logs in.
//! - Windows (its OpenSSH has no shared connections), a saved SSH profile other than the
//!   window's (`web open gpu:6006`: TensorBoard on a GPU box while editing here), and a shared
//!   connection that is not there: one `ssh -N -L …` child per forward, killed to remove.
//! - A daemon in WSL (from Windows): WSL2 shares localhost with Windows, so there is nothing to
//!   forward (`App::forward_via` gives none). A local daemon: nothing to forward either.
//!
//! ssh never asks anything here (`BatchMode`): a host that needs a password for a new login
//! fails with what ssh said.
//!
//! Forwards are removed when the web tab they were made for closes, when the server they were
//! made for stops, by `nib ctl forward rm`, when the window changes machine or root, and when
//! it quits.

use crate::control::Call;
use crate::transport::{ssh_command, SshEnv};
use crate::web::local;
use nib_editor::SshHost;
use serde_json::{json, Value};
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The machine the window is on, when that is another one reached by ssh.
#[derive(Clone, Debug)]
pub struct Via {
    pub host: SshHost,
    /// What the window calls it.
    pub label: String,
}

/// What a forward is kept for; the later ones outlive the earlier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Why {
    /// A web tab: gone when the tab closes.
    Tab,
    /// One of the project's servers: gone when it stops.
    Server,
    /// `forward add`: gone when removed.
    Asked,
}

enum Kind {
    /// On nib's shared connection to the host.
    Mux,
    /// An `ssh -N -L` of its own.
    Child(Child),
}

struct Forward {
    local: u16,
    remote: u16,
    host: SshHost,
    label: String,
    why: Why,
    kind: Kind,
}

/// A forward as `add` answers it.
#[derive(Clone, Debug, PartialEq)]
pub struct Made {
    pub local: u16,
    pub remote: u16,
    pub host: String,
    /// Made now (not one that was there already).
    pub new: bool,
}

impl Made {
    pub fn json(&self) -> Value {
        json!({ "local": self.local, "remote": self.remote, "host": self.host })
    }
}

static LIST: Mutex<Vec<Forward>> = Mutex::new(Vec::new());
/// One forward is made at a time (the list itself is never held while ssh runs).
static ADDING: Mutex<()> = Mutex::new(());
/// What the window should say in its message line: text, and whether it is a warning.
static NOTES: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());

fn batch() -> SshEnv {
    SshEnv { batch: true, askpass: None }
}

/// `-L`'s argument: this machine's 127.0.0.1 only, to the remote's own localhost (by name, so a
/// server listening only on `[::1]` there is reached too).
fn spec(local: u16, remote: u16) -> String {
    format!("127.0.0.1:{local}:localhost:{remote}")
}

/// `ssh -O forward|cancel -L …` to the shared connection `transport` keeps to `host`: the
/// same options, so that ssh finds the same socket.
fn mux_command(host: &SshHost, what: &str, local: u16, remote: u16) -> Command {
    let mut c = ssh_command(host, &batch(), &["-O", what, "-L", &spec(local, remote)]);
    c.args(["-o", crate::transport::CONTROL_PATH]).arg(&host.host);
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    c
}

/// `ssh -N -L …`: a connection of its own that only forwards, and ends if it cannot.
fn child_command(host: &SshHost, local: u16, remote: u16) -> Command {
    let mut extra = vec!["-N".to_string(), "-L".into(), spec(local, remote), "-o".into(), "ExitOnForwardFailure=yes".into(), "-o".into(), "ServerAliveInterval=30".into()];
    if cfg!(unix) {
        // Never through somebody's shared connection: the forward would outlive this process.
        extra.extend(["-o", "ControlMaster=no", "-o", "ControlPath=none"].map(String::from));
    }
    let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
    let mut c = ssh_command(host, &batch(), &extra);
    c.arg(&host.host);
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    c
}

/// Something listens on this machine's `port` (either localhost).
fn answers(port: u16) -> bool {
    let wait = Duration::from_millis(200);
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), wait).is_ok()
        || TcpStream::connect_timeout(&SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)), wait).is_ok()
}

fn free(port: u16) -> bool {
    !answers(port) && TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The port here for the remote's `want`: the same number if nothing has it, else the next
/// one that is free (never a privileged one it was not asked for).
fn choose(want: u16, taken: &[u16], free: impl Fn(u16) -> bool) -> Option<u16> {
    let ok = |p: &u16| !taken.contains(p) && free(*p);
    Some(want).filter(ok).or_else(|| (want.max(1023).saturating_add(1)..=u16::MAX).take(500).find(ok))
}

/// The last thing ssh said, as the reason.
fn said(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(2)..].join(" · ")
}

/// Start an `ssh -N -L` and wait until it listens here.
fn start_child(host: &SshHost, local: u16, remote: u16) -> Result<Child, String> {
    let mut child = child_command(host, local, remote).spawn().map_err(|e| format!("starting ssh: {e}"))?;
    // What it says is kept (the reason, if it gives up) and never left to fill its pipe.
    let err = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut pipe) = child.stderr.take() {
        let err = err.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = pipe.read(&mut buf) {
                let mut e = err.lock().unwrap();
                e.extend_from_slice(&buf[..n]);
                if e.len() > 16 * 1024 {
                    let cut = e.len() - 8 * 1024;
                    e.drain(..cut);
                }
            }
        });
    }
    let started = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            std::thread::sleep(Duration::from_millis(50));
            let why = said(&err.lock().unwrap());
            return Err(if why.is_empty() { format!("ssh ended ({status})") } else { why });
        }
        if TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], local)), Duration::from_millis(200)).is_ok() {
            return Ok(child);
        }
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("ssh did not connect in 30 s".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Bring `host`'s port `remote` here. `mux`: `host` is the window's own remote, whose shared
/// connection carries it when there is one. Blocking (ssh): never on the UI thread.
pub fn add(host: &SshHost, label: &str, remote: u16, why: Why, mux: bool) -> Result<Made, String> {
    let _one = ADDING.lock().unwrap_or_else(|e| e.into_inner());
    let taken: Vec<u16> = {
        let mut list = LIST.lock().unwrap();
        if let Some(f) = list.iter_mut().find(|f| f.host == *host && f.remote == remote) {
            f.why = f.why.max(why);
            return Ok(Made { local: f.local, remote, host: f.label.clone(), new: false });
        }
        list.iter().map(|f| f.local).collect()
    };
    let local = choose(remote, &taken, free).ok_or_else(|| format!("no free port on this machine near {remote}"))?;
    let mut kind = None;
    if mux && cfg!(unix) {
        match mux_command(host, "forward", local, remote).output() {
            Ok(o) if o.status.success() => kind = Some(Kind::Mux),
            // No shared connection (or it would not): one of its own, below.
            Ok(o) => crate::control::log("fwd", &format!("no forward on the shared connection to {label} ({}): starting an ssh for it", said(&o.stderr))),
            Err(e) => return Err(format!("starting ssh: {e}")),
        }
    }
    let kind = match kind {
        Some(k) => k,
        None => Kind::Child(start_child(host, local, remote).map_err(|e| format!("forwarding {label}'s port {remote}: {e}"))?),
    };
    crate::control::log("fwd", &format!("{label}:{remote} -> 127.0.0.1:{local}"));
    LIST.lock().unwrap().push(Forward { local, remote, host: host.clone(), label: label.to_string(), why, kind });
    if local != remote {
        note(format!("port {remote} is taken on this machine: {label}'s port {remote} is at 127.0.0.1:{local} here (a page that names port {remote} itself will not find it)"), true);
    }
    Ok(Made { local, remote, host: label.to_string(), new: true })
}

/// Take a forward down (its ssh work on a thread of its own unless `now`).
fn end(f: Forward, now: bool) {
    crate::control::log("fwd", &format!("{}:{} -> 127.0.0.1:{} removed", f.label, f.remote, f.local));
    match f.kind {
        Kind::Child(mut c) => {
            let _ = c.kill();
            let _ = c.wait();
        }
        Kind::Mux => {
            let cancel = move || drop(mux_command(&f.host, "cancel", f.local, f.remote).stderr(Stdio::null()).status());
            if now {
                cancel();
            } else {
                std::thread::spawn(cancel);
            }
        }
    }
}

fn remove_where(now: bool, which: impl Fn(&Forward) -> bool) -> usize {
    let gone: Vec<Forward> = {
        let mut list = LIST.lock().unwrap();
        let (gone, kept) = std::mem::take(&mut *list).into_iter().partition(|f| which(f));
        *list = kept;
        gone
    };
    let n = gone.len();
    gone.into_iter().for_each(|f| end(f, now));
    n
}

/// `forward rm LOCALPORT`.
pub fn remove(local: u16) -> bool {
    remove_where(false, |f| f.local == local) > 0
}

/// The web tab of this machine's `local` port closed: a forward made only for it goes.
pub fn tab_closed(local: u16) {
    remove_where(false, |f| f.local == local && f.why == Why::Tab);
}

/// A server of the window's remote no longer runs: the forward made for it goes.
pub fn server_gone(host: &SshHost, remote: u16) {
    remove_where(false, |f| f.host == *host && f.remote == remote && f.why == Why::Server);
}

/// The window leaves its machine or root: every forward goes.
pub fn clear() {
    remove_where(false, |_| true);
}

/// The window quits: every forward goes before it does.
pub fn clear_now() {
    remove_where(true, |_| true);
}

/// For `state.forwards` and `forward list`.
pub fn list() -> Value {
    LIST.lock().unwrap().iter().map(|f| json!({ "local": f.local, "remote": f.remote, "host": f.label })).collect()
}

/// Where `host`'s port `remote` is on this machine, if it is forwarded.
pub fn local_of(host: &SshHost, remote: u16) -> Option<u16> {
    LIST.lock().unwrap().iter().find(|f| f.host == *host && f.remote == remote).map(|f| f.local)
}

/// Say `text` in the window's message line.
pub fn note(text: String, warn: bool) {
    NOTES.lock().unwrap().push((text, warn));
    crate::web::wake();
}

/// What was noted since last asked, for the window's message line.
pub fn take_notes() -> Vec<(String, bool)> {
    std::mem::take(&mut *NOTES.lock().unwrap())
}

/// The port of a local address spelled out (`web::local::spell_out`), and the address with
/// another port in its place.
pub fn port_of(url: &str) -> Option<u16> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !port.contains(']') => port.parse().ok(),
        _ => Some(if scheme == "https" { 443 } else { 80 }),
    }
}

pub fn with_port(url: &str, port: u16) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = match authority.rsplit_once(':') {
        Some((host, p)) if !host.is_empty() && !p.contains(']') => host,
        _ => authority,
    };
    Some(format!("{scheme}://{host}:{port}{}", &rest[end..]))
}

/// `PROFILE:PORT[/path]`: a saved connection's name, a port, and the rest.
pub fn profile_port(asked: &str) -> Option<(&str, u16, &str)> {
    let asked = asked.trim();
    let (name, rest) = asked.split_once(':')?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (port, path) = rest.split_at(digits);
    if !(path.is_empty() || path.starts_with(['/', '?', '#'])) || nib_editor::config::check_profile_name(name).is_err() {
        return None;
    }
    Some((name, port.parse().ok().filter(|p| *p != 0)?, path))
}

/// The host of the saved SSH connection `name`.
pub fn profile_host(name: &str) -> Result<SshHost, String> {
    let config = nib_editor::Config::load().map_err(|e| format!("reading the saved connections: {e:#}"))?;
    let Some(p) = config.profiles.get(name) else {
        return Err(format!("{name} is not this machine, and not a saved SSH connection (`nib ctl ssh list` shows them)"));
    };
    match p.target() {
        Ok(nib_editor::Target::Ssh { ssh, .. }) => Ok(ssh),
        Ok(_) => Err(format!("{name} is not an SSH connection: only another machine's ports are forwarded")),
        Err(e) => Err(format!("{name}: {e:#}")),
    }
}

/// The saved connection `asked` names (`PROFILE:PORT`), when that is how [`open`] would take
/// it: not an address of this machine.
pub fn names_profile(asked: &str) -> Option<&str> {
    let local = local::is_local(&local::spell_out(asked));
    profile_port(asked).filter(|_| !local).map(|(name, _, _)| name)
}

/// What `asked` (a port, an address, `PROFILE:PORT`) needs before the web tab can show it.
enum Route {
    /// Nothing: the engine takes it as it is (and refuses what is not this machine).
    Direct,
    /// A port of the window's remote.
    Remote(u16, String),
    /// A port of a saved connection, and the path asked for there.
    Profile(String, u16, String),
}

fn route(asked: &str, remote: bool) -> Route {
    let url = local::spell_out(asked);
    if local::is_local(&url) {
        return match port_of(&url) {
            Some(port) if remote => Route::Remote(port, url),
            _ => Route::Direct,
        };
    }
    match profile_port(asked) {
        Some((name, port, path)) => Route::Profile(name.to_string(), port, path.to_string()),
        None => Route::Direct,
    }
}

/// `web.open`: hand `call` to the web tab's engine, after forwarding the port it means when
/// that is another machine's (`via`: the window's remote, if it has one). The answer says so
/// (`forwarded: {local, remote, host}`).
pub fn open(call: Call, via: Option<Via>, root: String) {
    let asked = call.str_arg("url").unwrap_or_default().to_string();
    let route = route(&asked, via.is_some());
    if matches!(route, Route::Direct) {
        return crate::web::submit(call, &root);
    }
    std::thread::spawn(move || {
        let made = match route {
            Route::Remote(port, url) => {
                let via = via.expect("a remote route has a remote");
                add(&via.host, &via.label, port, Why::Tab, true).map(|m| (with_port(&url, m.local).unwrap_or(url), m))
            }
            Route::Profile(name, port, path) => profile_host(&name).and_then(|host| {
                // The window's own machine by its saved name: its shared connection.
                let mux = via.as_ref().is_some_and(|v| v.host == host);
                let slash = if path.starts_with('/') { "" } else { "/" };
                add(&host, &format!("@{name}"), port, Why::Tab, mux).map(|m| (format!("http://127.0.0.1:{}{slash}{path}", m.local), m))
            }),
            Route::Direct => unreachable!(),
        };
        let (url, made) = match made {
            Ok(m) => m,
            Err(e) => return call.err(e),
        };
        let mut req = call.req.clone();
        req["url"] = json!(url);
        let asked_by = call.clone();
        let forwarded = Call::then(req, move |mut v| {
            if v["ok"] == true {
                v["forwarded"] = made.json();
            } else {
                if let Some(e) = v["error"].as_str() {
                    v["error"] = json!(format!("{e} (this is {}'s port {}, forwarded to 127.0.0.1:{}: is the server running there?)", made.host, made.remote, made.local));
                }
                // Made for a page that never came: not kept.
                if made.new {
                    tab_closed(made.local);
                }
            }
            asked_by.reply(v);
        });
        crate::web::submit(forwarded, &root);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> SshHost {
        SshHost { host: "gpu.lab".into(), user: Some("ada".into()), port: Some(2222), identity: None, jump: None }
    }

    fn args(c: &Command) -> Vec<String> {
        c.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    fn has(args: &[String], pair: &[&str]) -> bool {
        args.windows(pair.len()).any(|w| w.iter().map(String::as_str).eq(pair.iter().copied()))
    }

    #[test]
    fn the_shared_connection_is_asked_with_the_options_it_was_started_with() {
        let a = args(&mux_command(&host(), "forward", 8765, 8765));
        assert!(has(&a, &["-O", "forward", "-L", "127.0.0.1:8765:localhost:8765"]), "{a:?}");
        assert!(has(&a, &["-o", crate::transport::CONTROL_PATH]) && has(&a, &["-o", "BatchMode=yes"]) && has(&a, &["-o", "StrictHostKeyChecking=yes"]), "{a:?}");
        assert!(has(&a, &["-l", "ada", "-p", "2222"]), "the profile's options: the socket's name is made of them: {a:?}");
        assert_eq!(a.last().map(String::as_str), Some("gpu.lab"));
        let a = args(&mux_command(&host(), "cancel", 8766, 8765));
        assert!(has(&a, &["-O", "cancel", "-L", "127.0.0.1:8766:localhost:8765"]), "{a:?}");
    }

    #[test]
    fn a_forward_of_its_own_only_forwards_never_asks_and_binds_this_machine_only() {
        let a = args(&child_command(&host(), 6007, 6006));
        assert!(has(&a, &["-N", "-L", "127.0.0.1:6007:localhost:6006"]), "{a:?}");
        assert!(has(&a, &["-o", "ExitOnForwardFailure=yes"]) && has(&a, &["-o", "BatchMode=yes"]) && has(&a, &["-o", "StrictHostKeyChecking=yes"]), "{a:?}");
        #[cfg(unix)]
        assert!(has(&a, &["-o", "ControlPath=none"]), "{a:?}");
        assert_eq!(a.last().map(String::as_str), Some("gpu.lab"));
        assert!(!a.iter().any(|x| x.contains("0.0.0.0") || x == "-g"), "{a:?}");
    }

    #[test]
    fn the_same_port_here_when_it_is_free_else_the_next_free_one() {
        let all = |_: u16| true;
        assert_eq!(choose(8765, &[], all), Some(8765));
        // Something listens there, or nib already forwards to it: the next one that is free.
        assert_eq!(choose(8765, &[], |p| p != 8765 && p != 8766), Some(8767));
        assert_eq!(choose(8765, &[8765], all), Some(8766));
        // A privileged port that is taken is not answered with another privileged one.
        assert_eq!(choose(80, &[], |p| p != 80), Some(1024));
        assert_eq!(choose(80, &[], all), Some(80));
        assert_eq!(choose(65535, &[65535], all), None);
        // A port something listens on is not free.
        let held = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        assert!(!free(held.local_addr().unwrap().port()));
    }

    #[test]
    fn what_was_asked_to_open_says_whose_port_it_is() {
        let r = |asked: &str, remote: bool| match route(asked, remote) {
            Route::Direct => "direct".to_string(),
            Route::Remote(p, url) => format!("remote {p} {url}"),
            Route::Profile(n, p, path) => format!("profile {n} {p} {path}"),
        };
        // On this machine nothing is forwarded.
        assert_eq!(r("8000", false), "direct");
        assert_eq!(r("http://localhost:8000/x", false), "direct");
        // On another, this machine's names mean that one.
        assert_eq!(r("8000", true), "remote 8000 http://127.0.0.1:8000/");
        assert_eq!(r("localhost:5173/app?x=1", true), "remote 5173 http://localhost:5173/app?x=1");
        assert_eq!(r("0.0.0.0:6006", true), "remote 6006 http://127.0.0.1:6006");
        assert_eq!(r("http://[::1]:8000/", true), "remote 8000 http://[::1]:8000/");
        assert_eq!(r("http://localhost/", true), "remote 80 http://localhost/");
        // A saved connection's port, from either.
        assert_eq!(r("gpu:6006", false), "profile gpu 6006 ");
        assert_eq!(r("gpu:6006/#scalars", true), "profile gpu 6006 /#scalars");
        // Anything else is the engine's to refuse.
        for other in ["http://example.com/", "https://example.com:8443/x", "gpu", "gpu:", "gpu:x", "gpu:0", "gpu:80x", "-gpu:80", "http://gpu:6006/"] {
            assert_eq!(r(other, true), "direct", "{other}");
        }
    }

    #[test]
    fn an_address_with_another_port() {
        assert_eq!(with_port("http://127.0.0.1:8000/", 8001).as_deref(), Some("http://127.0.0.1:8001/"));
        assert_eq!(with_port("http://localhost:5173/app?x=http://localhost:5173", 5174).as_deref(), Some("http://localhost:5174/app?x=http://localhost:5173"));
        assert_eq!(with_port("http://localhost/", 8080).as_deref(), Some("http://localhost:8080/"));
        assert_eq!(with_port("http://[::1]:8000", 9).as_deref(), Some("http://[::1]:9"));
        assert_eq!(with_port("http://[::1]/x", 9).as_deref(), Some("http://[::1]:9/x"));
        assert_eq!((port_of("https://localhost/"), port_of("http://[::1]/"), port_of("http://127.0.0.1:65535/a:1")), (Some(443), Some(80), Some(65535)));
        // What is rewritten is still this machine's by the web tab's rule.
        assert!(local::is_local(&with_port("http://127.0.0.1:8000/", 8001).unwrap()));
    }

    #[test]
    fn an_unknown_connection_is_refused_by_name() {
        // (The saved connections are whatever this machine has; this name is not one.)
        let e = profile_host("no-such-connection-zz9").unwrap_err();
        assert!(e.contains("no-such-connection-zz9") && (e.contains("not this machine") || e.contains("saved connections")), "{e}");
    }
}
