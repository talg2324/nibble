//! SSH helpers around the transport: testing a host before connecting (`probe`), trusting a new
//! host key after its fingerprint was shown (`trust_host_key`), the aliases in ~/.ssh/config, and
//! `nib` running as ssh's askpass program (`askpass_main`).
//!
//! Host keys: nib's ssh refuses unknown and changed keys (`transport::ssh_command`). The probe is
//! the only place an unknown key is fetched, into a temp file, with every login method off, so
//! nothing is sent to an unverified host. Trusting it is a person's click in the SSH panel; the
//! control socket has no way to do it.

use crate::transport::{self, SshEnv};
use nib_editor::SshHost;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Set on the ssh that nib starts: where the askpass helper finds this window, and the secret
/// it shows to be heard.
pub const ASKPASS_SOCKET_ENV: &str = "NIB_ASKPASS_SOCKET";
pub const ASKPASS_TOKEN_ENV: &str = "NIB_ASKPASS_TOKEN";

/// Where a test stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Reached, logged in, platform supported, root exists.
    Ok,
    /// DNS, refused, timed out, or ssh itself failed to start.
    Network,
    /// The host's key isn't in known_hosts: see `fingerprints`, trust it from the panel.
    HostKeyUnknown,
    /// The host's key differs from known_hosts. Never trusted from nib.
    HostKeyChanged,
    /// No key login. `password` says whether the server takes a password or a 2FA code instead.
    Auth,
    /// Logged in, but nib has no daemon for this OS/CPU (and no `nibd` override).
    Platform,
    /// Logged in, but `root` is not a directory there.
    Root,
}

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Stage::Ok => "ok",
            Stage::Network => "network",
            Stage::HostKeyUnknown => "host_key_unknown",
            Stage::HostKeyChanged => "host_key_changed",
            Stage::Auth => "auth",
            Stage::Platform => "platform",
            Stage::Root => "root",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Probe {
    pub stage: Stage,
    /// One sentence for people.
    pub message: String,
    /// Host key fingerprints (`ssh-keygen -l` lines) when the key is unknown.
    pub fingerprints: Vec<String>,
    /// The known_hosts lines to append if a person trusts the key. Never sent over the socket.
    pub key_lines: Vec<String>,
    /// The known_hosts file they go to.
    pub known_hosts: Option<PathBuf>,
    /// Login methods the server offers (`publickey`, `password`, `keyboard-interactive`).
    pub methods: Vec<String>,
    /// `uname -sm` on the host.
    pub platform: Option<String>,
    /// The root as an absolute path on the host.
    pub root: Option<String>,
}

impl Probe {
    fn fail(stage: Stage, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
            fingerprints: vec![],
            key_lines: vec![],
            known_hosts: None,
            methods: vec![],
            platform: None,
            root: None,
        }
    }

    pub fn ok(&self) -> bool {
        self.stage == Stage::Ok
    }

    /// The server takes a password or code: Connect from the window can still log in.
    pub fn password(&self) -> bool {
        self.methods.iter().any(|m| m == "password" || m == "keyboard-interactive")
    }

    /// What the control socket shows. Leaves out `key_lines`: trusting is for people.
    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok(),
            "stage": self.stage.name(),
            "message": self.message,
            "fingerprints": self.fingerprints,
            "methods": self.methods,
            "password": self.password(),
            "platform": self.platform,
            "root": self.root,
        })
    }
}

/// ssh options that make a connection a test: its own TCP connection (not nib's shared master),
/// nothing forwarded, nothing run locally.
const TEST_OPTS: &[&str] = &[
    "-o", "ControlMaster=no", "-o", "ControlPath=none", "-o", "ForwardAgent=no", "-o", "ForwardX11=no",
    "-o", "ClearAllForwardings=yes", "-o", "PermitLocalCommand=no",
];

/// Test `host` the way Connect would use it, without logging in with anything but keys.
/// Blocking (up to ssh's connect timeout, twice); run it off the UI thread.
pub fn probe(host: &SshHost, root: &str, nibd: Option<&str>) -> Probe {
    // 1. Reach the host and learn its key, with every login method off.
    let resolved = resolve(host);
    let tmp = temp_file("probe");
    let mut files = vec![tmp.to_string_lossy().into_owned()];
    files.extend(resolved.known_hosts.iter().cloned());
    let mut c = Command::new("ssh");
    c.arg("-T")
        .args(["-o", "StrictHostKeyChecking=accept-new", "-o"])
        .arg(format!("UserKnownHostsFile={}", files.join(" ")))
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15"])
        .args(TEST_OPTS)
        .args(["-o", "PubkeyAuthentication=no", "-o", "PasswordAuthentication=no", "-o", "KbdInteractiveAuthentication=no"])
        .args(["-o", "GSSAPIAuthentication=no", "-o", "HostbasedAuthentication=no"])
        .args(host.args())
        .arg(&host.host)
        .arg("true");
    let first = run(c);
    let key_lines: Vec<String> = std::fs::read_to_string(&tmp)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    let fingerprints = if key_lines.is_empty() { vec![] } else { fingerprints(&tmp) };
    let _ = std::fs::remove_file(&tmp);
    let (status, err) = match first {
        Ok(v) => v,
        Err(e) => return Probe::fail(Stage::Network, e),
    };
    if err.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") || err.contains("Host key verification failed") {
        return Probe::fail(
            Stage::HostKeyChanged,
            format!(
                "{}'s host key is not the one in known_hosts. Someone may be intercepting the connection, or the server was reinstalled. Check with its admin, then fix known_hosts yourself.",
                host.host
            ),
        );
    }
    let methods = methods(&err);
    if !status && methods.is_empty() {
        return Probe::fail(Stage::Network, last_line(&err).unwrap_or("ssh failed").to_owned());
    }
    if !key_lines.is_empty() {
        return Probe {
            fingerprints,
            key_lines,
            known_hosts: resolved.known_hosts.first().map(PathBuf::from),
            methods,
            ..Probe::fail(Stage::HostKeyUnknown, format!("{} is reachable, but its host key isn't known yet. Check the fingerprint before trusting it.", host.host))
        };
    }

    // 2. The key is known: log in the way Connect does (keys only, no questions) and look around.
    let script = format!(
        "echo \"NIB_UNAME $(uname -sm)\"; r={}; if [ -d \"$r\" ]; then cd \"$r\" && echo \"NIB_ROOT $(pwd)\"; else echo NIB_NOROOT; fi",
        transport::sh_path(root)
    );
    let mut c = transport::ssh_command(host, &SshEnv { batch: true, askpass: None }, TEST_OPTS);
    c.arg(&host.host).arg(transport::remote_sh(&script));
    let (code, out, err) = match run_out(c) {
        Ok(v) => v,
        Err(e) => return Probe::fail(Stage::Network, e),
    };
    let base = Probe { methods: methods.clone(), ..Probe::fail(Stage::Ok, "") };
    // ssh's own failures exit 255; anything else means the login worked and the host refused
    // to run a shell (git hosts, restricted accounts).
    if code != Some(0) && code != Some(255) && !out.contains("NIB_UNAME") {
        let said = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no reason given");
        return Probe { stage: Stage::Platform, message: format!("Logged in, but {} doesn't run commands: {said}", host.host), ..base };
    }
    if code != Some(0) && out.is_empty() {
        let m = if methods.is_empty() { self::methods(&err) } else { methods };
        let password = m.iter().any(|m| m == "password" || m == "keyboard-interactive");
        let message = if password {
            "No key login. The server also takes a password or code: Connect from the window will ask for it.".to_owned()
        } else if err.contains("Permission denied") {
            "No key login: none of your keys (ssh-agent, ~/.ssh, or the identity file) is accepted.".to_owned()
        } else {
            last_line(&err).unwrap_or("ssh failed").to_owned()
        };
        return Probe { methods: m, ..Probe::fail(Stage::Auth, message) };
    }
    let platform = out.lines().find_map(|l| l.strip_prefix("NIB_UNAME ")).map(|s| s.trim().to_owned());
    let found_root = out.lines().find_map(|l| l.strip_prefix("NIB_ROOT ")).map(|s| s.trim().to_owned());
    let base = Probe { platform: platform.clone(), root: found_root.clone(), ..base };
    if nibd.is_none() {
        match platform.as_deref().map(transport::daemon_for) {
            Some(Ok(_)) => {}
            Some(Err(why)) => return Probe { stage: Stage::Platform, message: why, ..base },
            None => return Probe { stage: Stage::Network, message: "the host answered, but not as expected".into(), ..base },
        }
    }
    if found_root.is_none() {
        return Probe { stage: Stage::Root, message: format!("{root} is not a folder on {}.", host.host), ..base };
    }
    let message = format!("Ready: {} · {}", platform.unwrap_or_default(), found_root.unwrap_or_default());
    Probe { message, ..base }
}

/// Append the key lines from a probe to the user's known_hosts. Only ever called from a click.
pub fn trust_host_key(p: &Probe) -> anyhow::Result<PathBuf> {
    use std::io::Write;
    anyhow::ensure!(p.stage == Stage::HostKeyUnknown && !p.key_lines.is_empty(), "no new host key to trust");
    let file = p.known_hosts.clone().unwrap_or_else(|| home().join(".ssh/known_hosts"));
    let file = expand_home(&file.to_string_lossy());
    if let Some(dir) = file.parent() {
        create_private_dir(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&file)?;
    let needs_newline = std::fs::read(&file).ok().and_then(|b| b.last().copied()).is_some_and(|b| b != b'\n');
    if needs_newline {
        writeln!(f)?;
    }
    for l in &p.key_lines {
        writeln!(f, "{l}")?;
    }
    Ok(file)
}

/// Host aliases from ~/.ssh/config (and the files it includes), without wildcard patterns.
pub fn config_hosts() -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    read_config(&home().join(".ssh/config"), &mut out, &mut seen, 0);
    let mut uniq = std::collections::HashSet::new();
    out.retain(|h| uniq.insert(h.clone()));
    out
}

fn read_config(path: &Path, out: &mut Vec<String>, seen: &mut std::collections::HashSet<PathBuf>, depth: u8) {
    if depth > 4 || !seen.insert(path.to_path_buf()) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        let line = line.trim();
        let (key, rest) = line.split_once(|c: char| c.is_whitespace() || c == '=').unwrap_or((line, ""));
        let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
        match key.to_ascii_lowercase().as_str() {
            "host" => out.extend(
                rest.split_whitespace()
                    .filter(|h| !h.contains(['*', '?', '!']) && !h.starts_with('-'))
                    .map(|h| h.trim_matches('"').to_owned()),
            ),
            "include" => {
                for pat in rest.split_whitespace() {
                    for f in glob(&expand_home(pat), &home().join(".ssh")) {
                        read_config(&f, out, seen, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Files matching `pat` (relative to `base`), where only the last component may hold one `*`.
fn glob(pat: &Path, base: &Path) -> Vec<PathBuf> {
    let pat = if pat.is_absolute() { pat.to_path_buf() } else { base.join(pat) };
    let name = pat.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Some((pre, post)) = name.split_once('*') else { return vec![pat] };
    let Some(dir) = pat.parent() else { return vec![] };
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            p.is_file() && n.len() >= pre.len() + post.len() && n.starts_with(pre) && n.ends_with(post)
        })
        .collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------------------------
// askpass
// ---------------------------------------------------------------------------------------------

/// Is this process ssh's askpass helper (nib started ssh with itself as `SSH_ASKPASS`)?
pub fn is_askpass() -> bool {
    std::env::var_os(ASKPASS_TOKEN_ENV).is_some()
}

/// `nib` as `SSH_ASKPASS`: ssh runs it with the question as the argument and reads the answer
/// from stdout. The window that started this ssh asks the person. Exit 1 = cancelled.
pub fn askpass_main(args: &[String]) -> i32 {
    let (Some(sock), Ok(token)) = (std::env::var_os(ASKPASS_SOCKET_ENV), std::env::var(ASKPASS_TOKEN_ENV)) else {
        return 1;
    };
    // `confirm`: a yes/no question; `none`: just a notice (a security key to touch).
    let kind = std::env::var("SSH_ASKPASS_PROMPT").unwrap_or_default();
    let req = json!({ "cmd": "askpass", "token": token, "prompt": args.join(" "), "kind": kind });
    match crate::control::request(Path::new(&sock), &req) {
        Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
            if let Some(a) = v.get("answer").and_then(Value::as_str) {
                println!("{a}");
            }
            0
        }
        _ => 1,
    }
}

/// A secret for one connection's askpass requests.
pub fn new_token() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    let random = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_ok();
    if !random {
        // No /dev/urandom (Windows, where askpass isn't wired anyway): hash what varies.
        use sha2::{Digest, Sha256};
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let h = Sha256::digest(format!("{t}-{}-{:p}", std::process::id(), &b));
        b.copy_from_slice(&h[..16]);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

struct Resolved {
    known_hosts: Vec<String>,
}

/// What ssh's own config says for `host` (`ssh -G`).
fn resolve(host: &SshHost) -> Resolved {
    let out = Command::new("ssh").arg("-G").args(host.args()).arg(&host.host).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let known_hosts = text
        .lines()
        .find_map(|l| l.strip_prefix("userknownhostsfile "))
        .map(|v| v.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_else(|| vec![home().join(".ssh/known_hosts").to_string_lossy().into_owned()]);
    Resolved { known_hosts }
}

fn fingerprints(file: &Path) -> Vec<String> {
    Command::new("ssh-keygen")
        .arg("-lf")
        .arg(file)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

/// Whether it succeeded, and stderr.
fn run(c: Command) -> Result<(bool, String), String> {
    run_out(c).map(|(code, _, err)| (code == Some(0), err))
}

/// Exit code, stdout, stderr.
fn run_out(mut c: Command) -> Result<(Option<i32>, String, String), String> {
    let o = c.stdin(std::process::Stdio::null()).output().map_err(|e| format!("couldn't run ssh: {e}"))?;
    Ok((o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned()))
}

/// `Permission denied (publickey,password).` -> ["publickey", "password"].
fn methods(err: &str) -> Vec<String> {
    err.lines()
        .find_map(|l| l.split_once("Permission denied (").map(|(_, r)| r))
        .and_then(|r| r.split_once(')'))
        .map(|(m, _)| m.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

fn last_line(err: &str) -> Option<&str> {
    err.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("Warning:")).last()
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if p == "~" => home(),
        None => PathBuf::from(p),
    }
}

fn temp_file(what: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("nib-{what}-{}-{n}", std::process::id()))
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_methods_from_ssh_errors() {
        assert_eq!(methods("me@h: Permission denied (publickey,password,keyboard-interactive).\n"), ["publickey", "password", "keyboard-interactive"]);
        assert!(methods("ssh: Could not resolve hostname x: nodename nor servname provided").is_empty());
    }

    #[test]
    fn config_aliases_follow_includes_and_skip_patterns() {
        let dir = temp_file("sshcfg");
        std::fs::create_dir_all(dir.join("conf.d")).unwrap();
        std::fs::write(dir.join("config"), format!("Host gpu lab-*\n  HostName 1.2.3.4\nInclude {}/conf.d/*.conf\nhost=pi\n", dir.display())).unwrap();
        std::fs::write(dir.join("conf.d/a.conf"), "Host bastion !nope\nHost *\n").unwrap();
        std::fs::write(dir.join("conf.d/b.txt"), "Host skipped\n").unwrap();
        let (mut out, mut seen) = (Vec::new(), Default::default());
        read_config(&dir.join("config"), &mut out, &mut seen, 0);
        assert_eq!(out, ["gpu", "bastion", "pi"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
