//! Watching the files the client has open (`ClientNotify::Watch`), so a tab follows changes made
//! outside nib: a terminal, a formatter, an agent's script, `git checkout`, another editor.
//!
//! The files are polled (`stat` every `EVERY`) rather than watched with inotify / FSEvents: only
//! open tabs are watched, a handful of files, so polling costs next to nothing; it needs no extra
//! dependency in the static daemon; and it also sees changes on network filesystems (NFS, SMB,
//! sshfs), where kernel file events often never arrive.

use crate::fs_ops::mtime_ns;
use nib_proto::{ServerMsg, ServerNotify};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

/// How often the watched files are looked at.
const EVERY: Duration = Duration::from_millis(500);

/// What a file looked like last time: modification time and size, or `None` if it was missing.
type Stamp = Option<(u128, u64)>;

fn stamp(path: &Path) -> Stamp {
    fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| (mtime_ns(&m), m.len()))
}

/// Client path -> (where it is on disk, its stamp).
type Files = HashMap<String, (PathBuf, Stamp)>;

pub struct Watcher {
    files: Arc<Mutex<Files>>,
    tx: Sender<ServerMsg>,
    started: bool,
}

impl Watcher {
    pub fn new(tx: Sender<ServerMsg>) -> Self {
        Self { files: Default::default(), tx, started: false }
    }

    /// Watch exactly these files (`(client path, resolved path)`). Files already watched keep
    /// their stamp, so a change that lands while the set is replaced is still reported.
    pub fn set(&mut self, paths: Vec<(String, PathBuf)>) {
        {
            let mut files = self.files.lock().unwrap();
            let mut next = Files::with_capacity(paths.len());
            for (key, path) in paths {
                let entry = match files.remove(&key) {
                    Some(old) if old.0 == path => old,
                    _ => {
                        let s = stamp(&path);
                        (path, s)
                    }
                };
                next.insert(key, entry);
            }
            *files = next;
        }
        if !self.started {
            self.started = true;
            let (files, tx) = (Arc::downgrade(&self.files), self.tx.clone());
            thread::spawn(move || poll(files, tx));
        }
    }

    /// The daemon itself just wrote `path` (a save): take its new stamp without telling the client,
    /// which already knows from the write's answer.
    pub fn wrote(&self, path: &Path) {
        let mut files = self.files.lock().unwrap();
        for (p, s) in files.values_mut() {
            if p == path {
                *s = stamp(p);
            }
        }
    }
}

/// Runs until the watcher is dropped (the session ended) or the client is gone.
fn poll(files: Weak<Mutex<Files>>, tx: Sender<ServerMsg>) {
    loop {
        thread::sleep(EVERY);
        let Some(files) = files.upgrade() else { return };
        // Stat outside the lock: a slow network filesystem must not hold up `set` / `wrote`.
        let snapshot: Vec<(String, PathBuf, Stamp)> =
            files.lock().unwrap().iter().map(|(k, (p, s))| (k.clone(), p.clone(), *s)).collect();
        let mut changed = Vec::new();
        for (key, path, old) in snapshot {
            let now = stamp(&path);
            if now != old {
                changed.push((key, path, old, now));
            }
        }
        if changed.is_empty() {
            continue;
        }
        let mut map = files.lock().unwrap();
        for (key, path, old, now) in changed {
            // Only if nothing (a save, a new set) touched the entry while we looked.
            match map.get_mut(&key) {
                Some(entry) if entry.0 == path && entry.1 == old => entry.1 = now,
                _ => continue,
            }
            let note = ServerNotify::FileChanged { path: key, mtime_ns: now.map(|(t, _)| t) };
            if tx.send(ServerMsg::Notify(note)).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn next(rx: &mpsc::Receiver<ServerMsg>) -> Option<(String, Option<u128>)> {
        match rx.recv_timeout(EVERY * 3) {
            Ok(ServerMsg::Notify(ServerNotify::FileChanged { path, mtime_ns })) => Some((path, mtime_ns)),
            _ => None,
        }
    }

    #[test]
    fn reports_outside_changes_not_its_own_writes() {
        let dir = std::env::temp_dir().join(format!("nibd-watch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.py");
        fs::write(&file, "x = 1\n").unwrap();
        let (tx, rx) = mpsc::channel();
        let mut w = Watcher::new(tx);
        w.set(vec![("a.py".into(), file.clone())]);
        assert_eq!(next(&rx), None, "nothing changed yet");

        fs::write(&file, "x = 2, longer\n").unwrap();
        let (path, mtime) = next(&rx).expect("an outside write is reported");
        assert_eq!(path, "a.py");
        assert_eq!(mtime, Some(mtime_ns(&fs::metadata(&file).unwrap())));

        thread::sleep(Duration::from_millis(5));
        fs::write(&file, "x = 3, the daemon's own save\n").unwrap();
        w.wrote(&file);
        assert_eq!(next(&rx), None, "the daemon's own write is not echoed");

        fs::remove_file(&file).unwrap();
        assert_eq!(next(&rx), Some(("a.py".into(), None)), "a deleted file is reported as gone");

        w.set(vec![]);
        fs::write(&file, "back\n").unwrap();
        assert_eq!(next(&rx), None, "files no longer open are not watched");
        let _ = fs::remove_dir_all(&dir);
    }
}
