//! Connect to a nibd over a child process's stdio.
//!
//! - local: spawn `nibd` (sibling of the `nib` executable, else `$PATH`).
//! - ssh:   system OpenSSH. One exec on the fast path: a POSIX-sh snippet picks the
//!   content-addressed daemon for the remote `uname -sm` and `exec`s it, or prints
//!   `NIB_NEED_UPLOAD <os> <arch>`; we then upload over a second ssh exec and retry.
//!   nib's skills go up the same way (`skills-<hash>`, handed to nibd as `NIB_SKILLS`), so
//!   agents on the remote have them too; `NIB_NEED_UPLOAD +skills <os> <arch>`: only those.
//! - wsl:   same snippet through `wsl.exe -d <distro> --`.

use anyhow::{anyhow, bail, Context, Result};
use nib_editor::{SshHost, Target};
use nib_proto::{ClientMsg, ReqId, Request, ServerMsg, MAGIC, NEED_UPLOAD_MARKER};
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, Weak};

/// (uname -sm, rust target triple) for daemons we can upload.
const REMOTE_TARGETS: &[(&str, &str)] = &[
    ("Linux x86_64", "x86_64-unknown-linux-musl"),
    ("Linux aarch64", "aarch64-unknown-linux-musl"),
    ("Linux arm64", "aarch64-unknown-linux-musl"),
];

pub struct Conn {
    child: Child,
    /// Shared with the heartbeat thread, which stops once the connection is dropped.
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: ReqId,
    #[allow(dead_code)] // used for log naming; kept for diagnostics
    pub label: String,
    /// ssh's shared connection to this host, stopped when this connection closes.
    mux: Option<SshHost>,
}

impl Conn {
    pub fn request(&mut self, req: Request) -> Result<ReqId> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&ClientMsg::Request { id, req })?;
        Ok(id)
    }

    pub fn notify(&mut self, n: nib_proto::ClientNotify) -> Result<()> {
        self.send(&ClientMsg::Notify(n))
    }

    /// A way to notify the daemon from another thread (an answer that is ready later, on the
    /// web tab's engine's thread). What it sends after this connection closed goes nowhere.
    pub fn notifier(&self) -> impl Fn(nib_proto::ClientNotify) + Send + 'static {
        let stdin = self.stdin.clone();
        move |n| {
            if let Ok(mut w) = stdin.lock() {
                let _ = nib_proto::send(&mut *w, &ClientMsg::Notify(n));
            }
        }
    }

    fn send(&self, m: &ClientMsg) -> Result<()> {
        let mut w = self.stdin.lock().map_err(|_| anyhow!("connection poisoned"))?;
        nib_proto::send(&mut *w, m)?;
        Ok(())
    }

    fn new(child: Child, stdin: ChildStdin, label: String) -> Conn {
        let stdin = Arc::new(Mutex::new(stdin));
        heartbeat(Arc::downgrade(&stdin));
        Conn { child, stdin, next_id: 1, label, mux: None }
    }
}

/// Ping the daemon every `PING_EVERY` so it knows this window is still there (it exits after a
/// couple of minutes of silence). Runs off the UI thread: a busy or headless window still pings.
fn heartbeat(stdin: Weak<Mutex<ChildStdin>>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(nib_proto::PING_EVERY);
        let Some(w) = stdin.upgrade() else { return };
        let Ok(mut w) = w.lock() else { return };
        let seq = PING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ping = ClientMsg::Notify(nib_proto::ClientNotify::Ping { seq });
        PINGS.lock().unwrap().push_back((seq, std::time::Instant::now()));
        if nib_proto::send(&mut *w, &ping).is_err() {
            return;
        }
    });
}

/// Pings in flight, for the round trip (`seq` is unique across connections, so the window's
/// daemons don't answer each other's).
static PING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PINGS: Mutex<std::collections::VecDeque<(u64, std::time::Instant)>> = Mutex::new(std::collections::VecDeque::new());

/// The round trip of ping `seq`, now that its pong is here.
pub fn pong(seq: u64) -> Option<std::time::Duration> {
    let mut q = PINGS.lock().unwrap();
    q.retain(|(_, t)| t.elapsed() < std::time::Duration::from_secs(600));
    let i = q.iter().position(|(s, _)| *s == seq)?;
    q.remove(i).map(|(_, t)| t.elapsed())
}

impl Drop for Conn {
    fn drop(&mut self) {
        let _ = self.send(&ClientMsg::Request { id: u64::MAX, req: Request::Shutdown });
        // Give nibd a moment to exit cleanly, then make sure it's gone.
        let exited = (0..20).any(|_| {
            let done = matches!(self.child.try_wait(), Ok(Some(_)));
            if !done {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            done
        });
        if !exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(host) = &self.mux {
            stop_mux(host);
        }
    }
}

/// How ssh may ask for things it needs (passwords, 2FA codes).
#[derive(Clone, Default)]
pub struct SshEnv {
    /// Never ask: fail instead (`BatchMode=yes`). For connections an agent starts.
    pub batch: bool,
    /// Ask through this window (`nib` as `SSH_ASKPASS`, see `crate::ssh::askpass_main`).
    pub askpass: Option<Askpass>,
}

/// Where the askpass helper sends ssh's questions, and the secret that proves it was started by
/// this connection (an agent on the control socket must never be able to raise a password prompt).
#[derive(Clone)]
pub struct Askpass {
    pub socket: PathBuf,
    pub token: String,
}

/// Connect and return the connection plus a reader for server messages (to move to a thread).
/// Blocking: ssh, and an upload of nibd, can take a while. `progress` hears what is happening,
/// with how far along it is (0..=1) when that is known.
pub fn connect(target: &Target, env: &SshEnv, progress: &dyn Fn(String, Option<f32>)) -> Result<(Conn, BufReader<ChildStdout>)> {
    match target {
        Target::Local { nibd } => {
            let exe = match nibd {
                Some(p) => PathBuf::from(p),
                None => local_nibd()?,
            };
            let mut cmd = Command::new(&exe);
            cmd.arg("--stdio");
            launch(cmd, "local".into()).map(|r| r.expect_connected())?
        }
        Target::Ssh { ssh, nibd } => remote(&Remote::Ssh(ssh.clone(), env.clone()), nibd.as_deref(), progress),
        Target::Wsl { distro, nibd } => remote(&Remote::Wsl(distro.clone()), nibd.as_deref(), progress),
    }
}

/// ssh joins argv into one string for the remote login shell: run `script` with POSIX sh there,
/// whatever that shell is. `script` may contain anything; it is single-quoted here.
pub fn remote_sh(script: &str) -> String {
    format!("exec sh -c '{}'", script.replace('\'', r"'\''"))
}

/// The bundled daemon for a remote `uname -sm`, or why there is none.
pub fn daemon_for(uname: &str) -> std::result::Result<PathBuf, String> {
    let Some((_, triple)) = REMOTE_TARGETS.iter().find(|(u, _)| *u == uname) else {
        return Err(format!("nib has no daemon for {uname} (Linux x86_64 and arm64 only); set `nibd` to one you built there"));
    };
    bundled_daemon(triple).ok_or_else(|| format!("this nib has no Linux daemon to upload ({triple}). Build them once with scripts/build/build.sh daemons"))
}

/// The socket of nib's shared ssh connection to a host: `transport` opens it, `forward` and
/// `stop_mux` find it by the same option.
pub const CONTROL_PATH: &str = "ControlPath=~/.ssh/nib-%C";

/// Tell ssh's shared connection to `host` to take no new sessions and exit once the ones it
/// carries end: a window leaving a host leaves no logged-in connection behind (another window
/// still connected there keeps its session). Without this the master lingers `ControlPersist`.
fn stop_mux(host: &SshHost) {
    let _ = command("ssh")
        .args(["-o", CONTROL_PATH, "-O", "stop"])
        .args(host.args())
        .arg(&host.host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `p` as one word for POSIX sh, with a leading `~` still meaning the home directory.
pub fn sh_path(p: &str) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    match p.strip_prefix('~') {
        Some("") => "\"$HOME\"".to_owned(),
        Some(rest) if rest.starts_with('/') => format!("\"$HOME\"{}", quote(rest)),
        _ => quote(p),
    }
}

enum Launch {
    Connected(Conn, BufReader<ChildStdout>),
    NeedUpload(String),
}

impl Launch {
    fn expect_connected(self) -> Result<(Conn, BufReader<ChildStdout>)> {
        match self {
            Launch::Connected(c, r) => Ok((c, r)),
            Launch::NeedUpload(_) => bail!("daemon requested upload unexpectedly"),
        }
    }
}

/// How much a remote may print before nibd's handshake (a shell's greeting, a banner).
const HANDSHAKE_NOISE: usize = 64 * 1024;

fn launch(mut cmd: Command, label: String) -> Result<Launch> {
    no_console_window(&mut cmd);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr_log(&label));
    // What ssh says about a failure (host key, auth, network) is the error to show.
    let log_start = std::fs::metadata(log_path(&label)).map(|m| m.len()).unwrap_or(0);
    // Which window the lines below belong to (the log is shared by every window on this target).
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(log_path(&label)) {
        use std::io::Write;
        let _ = writeln!(f, "nib: {} window pid {} starts {:?}", nib_proto::log_clock(), std::process::id(), cmd.get_program());
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {:?}", cmd.get_program()))?;
    let stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // Scan until MAGIC; collect any preceding lines (shell noise or the upload marker).
    let mut line = Vec::new();
    let mut noise = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stdout.read(&mut byte)? == 0 {
            let status = child.wait().ok();
            let said = stderr_since(&label, log_start);
            if !said.is_empty() {
                bail!("{label}: {said}");
            }
            bail!(
                "{label}: connection closed before handshake (exit {status:?}). Output: {:?}. See {}",
                String::from_utf8_lossy(&noise),
                log_path(&label).display()
            );
        }
        line.push(byte[0]);
        if line.ends_with(MAGIC) {
            break;
        }
        // The limit holds within a line too: output with no newline in it is not kept for ever.
        if noise.len() + line.len() > HANDSHAKE_NOISE {
            let _ = child.kill();
            bail!("{label}: no nibd handshake in 64 KiB of output");
        }
        if byte[0] == b'\n' {
            let s = String::from_utf8_lossy(&line).trim().to_string();
            if let Some(rest) = s.strip_prefix(NEED_UPLOAD_MARKER) {
                let _ = child.wait();
                return Ok(Launch::NeedUpload(rest.trim().to_string()));
            }
            noise.extend_from_slice(&line);
            line.clear();
            if noise.len() > HANDSHAKE_NOISE {
                let _ = child.kill();
                bail!("{label}: no nibd handshake in 64 KiB of output");
            }
        }
    }
    Ok(Launch::Connected(
        Conn::new(child, stdin, label),
        stdout,
    ))
}

/// Windows: start `cmd` without a console window of its own. The window may have no console to
/// share (see `detach_own_console` in main.rs), and every background `ssh` or `nibd` would flash
/// one. What it starts in turn (git, shells, language servers) shares its hidden console.
pub fn no_console_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// `Command::new` for something run in the background (see `no_console_window`).
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut c = Command::new(program);
    no_console_window(&mut c);
    c
}

fn local_nibd() -> Result<PathBuf> {
    let name = if cfg!(windows) { "nibd.exe" } else { "nibd" };
    if let Ok(exe) = crate::app_bundle::exe() {
        let sib = exe.with_file_name(name);
        if sib.exists() {
            return Ok(sib);
        }
    }
    Ok(PathBuf::from(name)) // rely on $PATH
}

pub(crate) fn cache_dir() -> PathBuf {
    nib_proto::path::nib_cache_dir()
}

/// The log the daemon's (and ssh's) stderr is appended to for `target`.
pub fn daemon_log(target: &Target) -> PathBuf {
    log_path(&match target {
        Target::Local { .. } => "local".to_owned(),
        Target::Ssh { ssh, .. } => Remote::Ssh(ssh.clone(), SshEnv::default()).label(),
        Target::Wsl { distro, .. } => Remote::Wsl(distro.clone()).label(),
    })
}

fn log_path(label: &str) -> PathBuf {
    let safe: String = label
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cache_dir().join(format!("{safe}.log"))
}

/// The last lines appended to `label`'s log since it was `start` bytes long: why ssh gave up.
fn stderr_since(label: &str, start: u64) -> String {
    let Ok(all) = std::fs::read(log_path(label)) else { return String::new() };
    let new = String::from_utf8_lossy(all.get(start as usize..).unwrap_or_default()).into_owned();
    let lines: Vec<&str> = new.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("nib:")).collect();
    lines[lines.len().saturating_sub(4)..].join(" · ")
}

/// Daemon/ssh stderr must never reach the TUI; append it to a per-target log.
fn stderr_log(label: &str) -> Stdio {
    let p = log_path(label);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

// ---------------------------------------------------------------------------------------------
// remote
// ---------------------------------------------------------------------------------------------

/// `ssh` with everything up to (not including) the host: the profile's options, and nib's rules.
/// - Unknown or changed host keys are refused, never accepted on the fly. A new host is trusted
///   from the SSH panel after its fingerprint is shown (`crate::ssh::trust_host_key`).
/// - Questions (password, 2FA) go to this window through askpass, or fail in batch mode; ssh
///   never waits on a terminal nobody is looking at.
/// `extra` goes first: for ssh the first value given for an option wins.
pub fn ssh_command(host: &SshHost, env: &SshEnv, extra: &[&str]) -> Command {
    let mut c = command("ssh");
    c.arg("-T").args(extra).args(["-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=15"]);
    if let Some(o) = crate::ssh::known_hosts_option(host) {
        c.args(["-o", &o]);
    }
    if env.batch {
        c.args(["-o", "BatchMode=yes"]);
    }
    match (&env.askpass, crate::app_bundle::exe()) {
        (Some(a), Ok(exe)) if !env.batch => {
            c.env("SSH_ASKPASS", exe)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(crate::ssh::ASKPASS_SOCKET_ENV, &a.socket)
                .env(crate::ssh::ASKPASS_TOKEN_ENV, &a.token);
        }
        _ => {}
    }
    c.args(host.args());
    c
}

enum Remote {
    Ssh(SshHost, SshEnv),
    Wsl(String),
}

impl Remote {
    fn label(&self) -> String {
        match self {
            Remote::Ssh(h, _) => h.display(),
            Remote::Wsl(d) => format!("wsl-{d}"),
        }
    }

    /// The host whose shared ssh connection a connection uses (unix: Windows OpenSSH has none).
    fn mux(&self) -> Option<SshHost> {
        match self {
            Remote::Ssh(h, _) if cfg!(unix) => Some(h.clone()),
            _ => None,
        }
    }

    /// Command that runs `script` with POSIX sh on the target.
    fn sh(&self, script: &str) -> Command {
        match self {
            Remote::Ssh(host, env) => {
                let mut c = ssh_command(host, env, &[]);
                if cfg!(unix) {
                    // nib-owned multiplexing: reconnects skip the handshake, and later phases
                    // reuse the master for port forwards (`ssh -O forward`). Windows OpenSSH
                    // has no ControlMaster support.
                    c.args([
                        "-o",
                        "ControlMaster=auto",
                        "-o",
                        CONTROL_PATH,
                        "-o",
                        "ControlPersist=60",
                    ]);
                }
                c.arg(&host.host).arg(remote_sh(script));
                c
            }
            Remote::Wsl(distro) => {
                let mut c = Command::new("wsl.exe");
                c.args(["-d", distro, "--", "sh", "-c", script]);
                c
            }
        }
    }
}

fn daemons_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("NIB_DAEMONS") {
        v.push(PathBuf::from(d));
    }
    if let Some(h) = std::env::var_os("HOME") {
        // Where the installer puts them, and where installers before v0.4 did.
        v.push(PathBuf::from(&h).join(".local/lib/nib/daemons"));
        v.push(PathBuf::from(h).join(".local/share/nib/daemons"));
    }
    if let Ok(exe) = crate::app_bundle::exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join("daemons"));
            v.push(dir.join("../share/nib/daemons"));
        }
    }
    v
}

fn bundled_daemon(triple: &str) -> Option<PathBuf> {
    daemons_dirs()
        .into_iter()
        .map(|d| d.join(triple).join("nibd"))
        .find(|p| p.is_file())
}

fn short_hash(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// What the bootstrap says when nibd is there but nib's skills (`skills-<hash>`) aren't.
const SKILLS_ONLY: &str = "+skills ";

/// Bootstrap snippet. Must not contain single quotes (see `Remote::sh`). `skills`: the hash of
/// the skills to hand nibd (`None`: this nib has none).
///
/// The cached nibd is run only if it still is the file its name says (`same`: its SHA-256
/// starts with the hash in its name, the first 8 bytes); one that was changed where it lies is
/// uploaded again, over it. A remote with neither `sha256sum` nor `shasum` runs it by its name
/// alone, as before.
fn bootstrap_script(hashes: &[(&str, String)], skills: Option<&str>) -> String {
    let mut cases = String::new();
    for (uname, hash) in hashes {
        cases.push_str(&format!("\"{uname}\") h={hash};; "));
    }
    let skills = skills.unwrap_or_default();
    format!(
        "d=\"${{XDG_CACHE_HOME:-$HOME/.cache}}/nib\"; u=\"$(uname -sm)\"; h=; s={skills}; \
         case \"$u\" in {cases}esac; \
         same() {{ if command -v sha256sum >/dev/null 2>&1; then g=$(sha256sum \"$1\"); \
           elif command -v shasum >/dev/null 2>&1; then g=$(shasum -a 256 \"$1\"); else return 0; fi; \
           case \"$g\" in \"$h\"*) return 0;; esac; return 1; }}; \
         if [ -n \"$h\" ] && [ -x \"$d/nibd-$h\" ] && same \"$d/nibd-$h\"; then \
           if [ -z \"$s\" ] || [ -d \"$d/skills-$s\" ]; then \
             [ -z \"$s\" ] || export NIB_SKILLS=\"$d/skills-$s\"; exec \"$d/nibd-$h\" --stdio; \
           fi; \
           echo \"{NEED_UPLOAD_MARKER} {SKILLS_ONLY}$u\"; exit 0; \
         fi; \
         echo \"{NEED_UPLOAD_MARKER} $u\""
    )
}

fn remote(r: &Remote, nibd_override: Option<&str>, progress: &dyn Fn(String, Option<f32>)) -> Result<(Conn, BufReader<ChildStdout>)> {
    let label = r.label();
    if let Some(p) = nibd_override {
        let (mut c, rd) = launch(r.sh(&format!("exec {} --stdio", sh_path(p))), label)?.expect_connected()?;
        c.mux = r.mux();
        return Ok((c, rd));
    }
    let mut hashes = Vec::new();
    for (uname, triple) in REMOTE_TARGETS {
        if let Some(p) = bundled_daemon(triple) {
            hashes.push((*uname, short_hash(&p)?));
        }
    }
    let mut skills = local_skills();
    for attempt in 0..2 {
        match launch(r.sh(&bootstrap_script(&hashes, skills.as_ref().map(|(h, _)| h.as_str()))), label.clone())? {
            Launch::Connected(mut c, rd) => {
                c.mux = r.mux();
                return Ok((c, rd));
            }
            Launch::NeedUpload(what) if attempt == 0 => {
                let (uname, nibd) = match what.strip_prefix(SKILLS_ONLY) {
                    Some(u) => (u.to_string(), false),
                    None => (what, true),
                };
                if nibd {
                    upload(r, &uname, progress)?;
                }
                if let Some((hash, tar)) = &skills {
                    // Without them the remote's agents miss nib's skills; nibd itself still works.
                    if let Err(e) = upload_skills(r, hash, tar) {
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path(&label)) {
                            let _ = writeln!(f, "nib: skills not uploaded: {e:#}");
                        }
                        skills = None;
                    }
                }
            }
            Launch::NeedUpload(what) => {
                bail!("{label}: nibd still missing after upload ({what})")
            }
        }
    }
    unreachable!()
}

/// Store stdin as `nibd-<hash>` in the remote's nib cache (atomically), then prune: keep it and the
/// two most recently uploaded others (another nib version may still connect there; a pruned one is
/// just uploaded again), and drop temp files of uploads that died. Deleting the binary of a running
/// nibd is safe: it keeps running from the unlinked file. Pruning never fails the upload.
/// Must not contain single quotes (see `Remote::sh`).
fn upload_script(hash: &str) -> String {
    format!(
        "set -e; d=\"${{XDG_CACHE_HOME:-$HOME/.cache}}/nib\"; mkdir -p \"$d\"; \
         t=\"$d/.nibd-{hash}.$$\"; cat > \"$t\"; chmod 755 \"$t\"; mv -f \"$t\" \"$d/nibd-{hash}\"; \
         set +e; cd \"$d\" || exit 0; \
         ls -t | grep -x \"nibd-[0-9a-f]*\" | grep -vx \"nibd-{hash}\" | tail -n +3 | while read -r f; do rm -f \"./$f\"; done; \
         find . -maxdepth 1 -name \".nibd-*\" -mmin +10 -exec rm -f {{}} + 2>/dev/null; exit 0"
    )
}

fn upload(r: &Remote, uname: &str, progress: &dyn Fn(String, Option<f32>)) -> Result<()> {
    let triple = REMOTE_TARGETS
        .iter()
        .find(|(u, _)| *u == uname)
        .map(|(_, t)| *t)
        .ok_or_else(|| anyhow!("no nibd build for remote platform {uname:?}"))?;
    let bin = bundled_daemon(triple).ok_or_else(|| {
        // Where it looked goes to the log; the window gets the fix.
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path(&r.label())) {
            let _ = writeln!(f, "nib: no nibd for {triple} in {:?}", daemons_dirs());
        }
        anyhow!("this nib has no Linux daemon to upload ({triple}). Build them once with scripts/build/build.sh daemons, then connect again")
    })?;
    let hash = short_hash(&bin)?;
    let size = std::fs::metadata(&bin).map(|m| m.len()).unwrap_or(0);
    let what = format!("uploading nibd ({:.1} MB, {triple}) to {}", size as f64 / 1e6, r.label());
    progress(format!("{what}…"), Some(0.0));
    let mut cmd = r.sh(&upload_script(&hash));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(stderr_log(&r.label()));
    let mut child = cmd.spawn()?;
    {
        // In chunks, to report progress. This counts what ssh has taken, which runs a little
        // ahead of what has arrived: the end waits on ssh, reported as "finishing".
        let mut stdin = child.stdin.take().unwrap();
        let data = std::fs::read(&bin)?;
        let total = data.len().max(1);
        let mut sent = 0;
        let mut shown = 0;
        for chunk in data.chunks(32 * 1024) {
            stdin.write_all(chunk)?;
            sent += chunk.len();
            let pct = sent * 100 / total;
            if pct != shown {
                shown = pct;
                progress(format!("{what}… {pct}%"), Some(sent as f32 / total as f32));
            }
        }
    }
    progress(format!("{what}: finishing…"), Some(1.0));
    let st = child.wait()?;
    if !st.success() {
        bail!(
            "upload to {} failed ({st}); see {}",
            r.label(),
            log_path(&r.label()).display()
        );
    }
    Ok(())
}

/// nib's skills on this machine (next to this binary; in a checkout, the nearest `.skills` above
/// it), to go up with nibd: a hash of their contents and them as a tar archive.
fn local_skills() -> Option<(String, Vec<u8>)> {
    let dir = crate::app_bundle::exe().ok()?.ancestors().skip(1).map(|d| d.join(".skills")).find(|d| d.is_dir())?;
    let mut files = Vec::new();
    collect_files(&dir, "", &mut files);
    if files.is_empty() {
        return None;
    }
    files.sort();
    let mut h = Sha256::new();
    for (path, data) in &files {
        h.update(path.as_bytes());
        h.update([0]);
        h.update((data.len() as u64).to_le_bytes());
        h.update(data);
    }
    let hash = h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect();
    Some((hash, tar(&files)))
}

/// Every file under `dir`, as (path relative to it with `/`, contents); links followed.
fn collect_files(dir: &Path, rel: &str, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let Ok(meta) = std::fs::metadata(e.path()) else { continue };
        if meta.is_dir() {
            collect_files(&e.path(), &path, out);
        } else if let Ok(data) = std::fs::read(e.path()) {
            out.push((path, data));
        }
    }
}

/// A plain ustar archive of `files`, which `tar -xf -` unpacks anywhere (GNU, BSD, busybox).
/// A path too long for the format is left out.
fn tar(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    fn put(h: &mut [u8; 512], at: usize, s: &str) {
        h[at..at + s.len()].copy_from_slice(s.as_bytes());
    }
    let mut out = Vec::new();
    for (path, data) in files {
        // name: 100 bytes; longer ones split at a `/` into prefix (155) and name
        let (prefix, name) = if path.len() <= 100 {
            ("", path.as_str())
        } else {
            match path.char_indices().filter(|&(i, c)| c == '/' && i <= 155 && path.len() - i - 1 <= 100).map(|(i, _)| i).next() {
                Some(i) => (&path[..i], &path[i + 1..]),
                None => continue,
            }
        };
        let mut h = [0u8; 512];
        put(&mut h, 0, name);
        put(&mut h, 100, "0000644\0");
        put(&mut h, 108, "0000000\0");
        put(&mut h, 116, "0000000\0");
        put(&mut h, 124, &format!("{:011o}\0", data.len()));
        put(&mut h, 136, "00000000000\0");
        put(&mut h, 148, "        ");
        h[156] = b'0';
        put(&mut h, 257, "ustar\0");
        put(&mut h, 263, "00");
        put(&mut h, 345, prefix);
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        put(&mut h, 148, &format!("{sum:06o}\0 "));
        out.extend_from_slice(&h);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.extend_from_slice(&[0; 1024]);
    out
}

/// Unpack stdin (a tar of nib's skills) as `skills-<hash>` in the remote's nib cache
/// (atomically), then prune as `upload_script` does: keep it and the two most recent others.
/// Must not contain single quotes (see `Remote::sh`).
fn skills_upload_script(hash: &str) -> String {
    format!(
        "set -e; d=\"${{XDG_CACHE_HOME:-$HOME/.cache}}/nib\"; mkdir -p \"$d\"; \
         t=\"$d/.skills-{hash}.$$\"; rm -rf \"$t\"; mkdir \"$t\"; tar -xf - -C \"$t\"; \
         rm -rf \"$d/skills-{hash}\"; mv \"$t\" \"$d/skills-{hash}\"; \
         set +e; cd \"$d\" || exit 0; \
         ls -td skills-* 2>/dev/null | grep -x \"skills-[0-9a-f]*\" | grep -vx \"skills-{hash}\" | tail -n +3 | while read -r f; do rm -rf \"./$f\"; done; \
         find . -maxdepth 1 -name \".skills-*\" -mmin +10 -exec rm -rf {{}} + 2>/dev/null; exit 0"
    )
}

fn upload_skills(r: &Remote, hash: &str, tar: &[u8]) -> Result<()> {
    let mut cmd = r.sh(&skills_upload_script(hash));
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(stderr_log(&r.label()));
    let mut child = cmd.spawn()?;
    child.stdin.take().unwrap().write_all(tar)?;
    let st = child.wait()?;
    if !st.success() {
        bail!("skills upload to {} failed ({st})", r.label());
    }
    Ok(())
}

/// Blocking reader loop; run on its own thread.
pub fn read_loop(
    mut rd: BufReader<ChildStdout>,
    mut on_msg: impl FnMut(Result<ServerMsg, String>) -> bool,
) {
    loop {
        let r = match nib_proto::recv::<_, ServerMsg>(&mut rd) {
            Ok(Some(m)) => Ok(m),
            Ok(None) => Err("daemon closed the connection".to_string()),
            Err(e) => Err(e.to_string()),
        };
        let stop = r.is_err();
        if !on_msg(r) || stop {
            return;
        }
    }
}

/// A connection to a process that reads and discards everything (`cat`; `findstr` on Windows,
/// which has no `cat`): lets tests drive the real `App`.
#[cfg(test)]
pub fn sink() -> Conn {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("findstr");
        c.arg("^");
        c
    } else {
        Command::new("cat")
    };
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    Conn::new(child, stdin, "test".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The remote bootstrap is POSIX sh run on the remote; these run it with a local `sh`.
    #[test]
    #[cfg(unix)]
    fn paths_quoted_for_sh_keep_the_home_tilde() {
        assert_eq!(sh_path("~"), "\"$HOME\"");
        assert_eq!(sh_path("~/bin/nibd"), "\"$HOME\"'/bin/nibd'");
        assert_eq!(sh_path("/opt/x; rm -rf /"), "'/opt/x; rm -rf /'");
        assert_eq!(sh_path("it's"), r"'it'\''s'");
        let out = Command::new("sh").arg("-c").arg(format!("printf %s {}", sh_path("~/a b"))).env("HOME", "/h").output().unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "/h/a b");
    }

    #[test]
    fn bootstrap_has_no_single_quotes() {
        let s = bootstrap_script(&[("Linux x86_64", "abcd".into())], Some("5c11"));
        assert!(!s.contains('\''));
        assert!(s.contains("\"Linux x86_64\") h=abcd;;"));
    }

    /// A remote that prints and prints and never says nibd's handshake, with or without ends
    /// of lines, is given up after 64 KiB: what it sends is not kept without limit.
    #[test]
    #[cfg(unix)]
    fn a_remote_that_never_shakes_hands_is_given_up() {
        for flood in ["yes", "yes | tr -d \"\\n\""] {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(flood);
            let started = std::time::Instant::now();
            let Err(e) = launch(cmd, format!("nib-test-flood-{}", std::process::id())) else { panic!("{flood}: connected") };
            assert!(e.to_string().contains("no nibd handshake in 64 KiB"), "{flood}: {e:#}");
            assert!(started.elapsed() < std::time::Duration::from_secs(20), "{flood}");
        }
        let _ = std::fs::remove_file(log_path(&format!("nib-test-flood-{}", std::process::id())));
    }

    /// Runs the bootstrap snippet with a local `sh`, simulating a remote with a fake HOME.
    #[test]
    #[cfg(unix)]
    fn bootstrap_fast_path_and_miss() {
        let home = std::env::temp_dir().join(format!("nib-boot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".cache/nib")).unwrap();
        let uname =
            String::from_utf8(Command::new("uname").arg("-sm").output().unwrap().stdout).unwrap();
        let uname = uname.trim().to_string();
        // Named, as an upload names it, by the start of the SHA-256 of what is in it.
        let body = "#!/bin/sh\necho ran \"$1\"\n";
        let feed: String = Sha256::digest(body.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
        let script = bootstrap_script(&[(uname.as_str(), feed.clone())], None);
        let run = || {
            let out = Command::new("sh")
                .arg("-c")
                .arg(&script)
                .env("HOME", &home)
                .env_remove("XDG_CACHE_HOME")
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(run().trim(), format!("{NEED_UPLOAD_MARKER} {uname}"));
        let fake = home.join(".cache/nib").join(format!("nibd-{feed}"));
        std::fs::write(&fake, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(run().trim(), "ran --stdio");
        // Changed where it lies (its name and its executable bit as they were): not run, and
        // asked for again, which writes the real one over it.
        std::fs::write(&fake, "#!/bin/sh\necho somebody else ran \"$1\"\n").unwrap();
        assert_eq!(run().trim(), format!("{NEED_UPLOAD_MARKER} {uname}"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    /// Skills on the remote: asked for while missing, handed to nibd as `NIB_SKILLS` once there;
    /// the upload script unpacks what `tar` makes.
    #[test]
    #[cfg(unix)]
    fn skills_go_up_and_reach_nibd() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("nib-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let cache = home.join(".cache/nib");
        std::fs::create_dir_all(&cache).unwrap();
        let uname = String::from_utf8(Command::new("uname").arg("-sm").output().unwrap().stdout).unwrap().trim().to_string();
        let sh = |script: &str, input: &[u8]| {
            let mut c = Command::new("sh").arg("-c").arg(script).env("HOME", &home).env_remove("XDG_CACHE_HOME")
                .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
            c.stdin.take().unwrap().write_all(input).unwrap();
            String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap().trim().to_string()
        };
        let body = "#!/bin/sh\necho \"ran $1 $NIB_SKILLS\"\n";
        let feed: String = Sha256::digest(body.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
        let boot = bootstrap_script(&[(uname.as_str(), feed.clone())], Some("5c11"));
        assert_eq!(sh(&boot, b""), format!("{NEED_UPLOAD_MARKER} {uname}"), "no nibd: upload both");
        let fake = cache.join(format!("nibd-{feed}"));
        std::fs::write(&fake, body).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(sh(&boot, b""), format!("{NEED_UPLOAD_MARKER} {SKILLS_ONLY}{uname}"), "nibd there: only the skills");

        let long = format!("nib-work/{}/SKILL.md", "deep/".repeat(25));
        let files = vec![("nib-work/SKILL.md".to_string(), b"---\nname: nib-work\n---\n".to_vec()), (long.clone(), vec![b'x'; 700])];
        sh(&skills_upload_script("5c11"), &tar(&files));
        assert_eq!(std::fs::read(cache.join("skills-5c11/nib-work/SKILL.md")).unwrap(), files[0].1);
        assert_eq!(std::fs::read(cache.join("skills-5c11").join(&long)).unwrap(), files[1].1, "a path over 100 bytes");
        assert_eq!(sh(&boot, b""), format!("ran --stdio {}", cache.join("skills-5c11").display()));

        // Skills from an older nib are pruned past the two most recent others.
        for (i, old) in ["aa01", "aa02", "aa03"].iter().enumerate() {
            std::fs::create_dir_all(cache.join(format!("skills-{old}"))).unwrap();
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(100 * (3 - i as u64));
            std::fs::File::open(cache.join(format!("skills-{old}"))).unwrap().set_modified(t).unwrap();
        }
        sh(&skills_upload_script("5c11"), &tar(&files));
        let mut left: Vec<String> = std::fs::read_dir(&cache).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("skills-")).collect();
        left.sort();
        assert_eq!(left, ["skills-5c11", "skills-aa02", "skills-aa03"]);
        std::fs::remove_dir_all(&home).unwrap();
    }
}
