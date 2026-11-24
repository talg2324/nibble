//! A request from an agent's command to the daemon, through files: the command leaves it in
//! `DIR/req/` and waits for the answer of the same name in `DIR/res/`. Used by `nib-app` and
//! `nib-web`, which run in an agent's process tree and cannot reach the daemon otherwise.
//!
//! `DIR` is in the asking agent's own part of the mailbox (`mail::own`: `…/app`, `…/web`), so
//! the daemon knows whose request it is by where it was left ([`take_all`]), and takes it only
//! with that agent's token.
//!
//! Only `*.json` files are requests or answers: `fs_ops::write_whole` writes each beside, under
//! another name, and renames it, so a reader never sees half of one.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// An answer nobody came for (a command that gave up, or was stopped) is deleted after this long.
pub const KEEP: Duration = Duration::from_secs(600);

/// Leave `body` in `dir/req/` and wait at most `limit` for its answer. `Err`: the request could
/// not be written. `Ok(None)`: no answer in time (the request is taken back).
pub fn ask(dir: &Path, body: &str, limit: Duration) -> std::io::Result<Option<Value>> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Names sort in the order they were asked, which is the order the daemon answers in.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let name = format!("{nanos:032}-{}-{}.json", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    crate::fs_ops::write_whole(&dir.join("req").join(&name), body)?;
    let res = dir.join("res").join(&name);
    let started = Instant::now();
    while started.elapsed() < limit {
        if let Some(v) = fs::read(&res).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            let _ = fs::remove_file(&res);
            return Ok(Some(v));
        }
        // Most answers come at once: look often at first, then less.
        thread::sleep(Duration::from_millis(if started.elapsed() < Duration::from_secs(2) { 4 } else { 25 }));
    }
    let _ = fs::remove_file(dir.join("req").join(&name));
    Ok(None)
}

/// The requests left in `dir/req/`, oldest first, each taken (deleted) as it is read: its file
/// name (what the answer is written as) and what it says, `None` if it is not JSON.
pub fn take(dir: &Path) -> Vec<(String, Option<Value>)> {
    let Ok(read) = fs::read_dir(dir.join("req")) else { return Vec::new() };
    let mut files: Vec<_> = read.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|file| {
            let req = fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok());
            let _ = fs::remove_file(&file);
            Some((file.file_name()?.to_string_lossy().into_owned(), req))
        })
        .collect()
}

/// One request an agent left: the folder it is in (where its answer goes), its file's name, the
/// agent whose folder that is, and what it says (`None` if it is not JSON).
pub struct Asked {
    pub dir: PathBuf,
    pub name: String,
    pub agent: String,
    pub req: Option<Value>,
}

impl Asked {
    /// The agent this request is from: the one whose folder it was left in, when `token` is
    /// the one nib gave that agent. `None`: no token, a made-up one, or another agent's.
    pub fn asker(&self, token: &str) -> Option<String> {
        crate::tokens::who(token).filter(|a| *a == self.agent)
    }
}

/// The requests of kind `sub` (`app`, `web`) that the agents of `mailbox` left, each in its
/// own folder, oldest first (file names start with the time), each taken as it is read.
pub fn take_all(mailbox: &Path, sub: &str) -> Vec<Asked> {
    let mut all: Vec<Asked> = Vec::new();
    for agent in crate::mail::agents(mailbox) {
        let dir = crate::mail::own(mailbox, &agent).join(sub);
        all.extend(take(&dir).into_iter().map(|(name, req)| Asked { dir: dir.clone(), name, agent: agent.clone(), req }));
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}

/// [`sweep`] every agent's folder of kind `sub`.
pub fn sweep_all(mailbox: &Path, sub: &str) {
    for agent in crate::mail::agents(mailbox) {
        sweep(&crate::mail::own(mailbox, &agent).join(sub));
    }
}

/// Answer the request named `name` (from [`take`]).
pub fn answer(dir: &Path, name: &str, body: &str) {
    let _ = crate::fs_ops::write_whole(&dir.join("res").join(name), body);
}

/// Delete the answers in `dir/res/` older than [`KEEP`].
pub fn sweep(dir: &Path) {
    for old in fs::read_dir(dir.join("res")).into_iter().flatten().flatten() {
        if old.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > KEEP)) {
            let _ = fs::remove_file(old.path());
        }
    }
}
