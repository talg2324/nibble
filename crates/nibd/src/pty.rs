//! Pseudo-terminals for the client's terminal panes. Output is streamed as
//! `ServerNotify::PtyOutput` through the connection's writer channel.

use nib_proto::{ErrorKind, PtyId, RpcError, ServerMsg, ServerNotify};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

/// `None` once the shell has exited: the pty is closed then (on Windows that is what ends its
/// output pipe and frees the reader thread and the console host).
type Master = Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>;

struct Pty {
    master: Master,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

#[derive(Default)]
pub struct Ptys {
    next: PtyId,
    map: HashMap<PtyId, Pty>,
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 }
}

fn default_shell() -> (String, Vec<String>) {
    if cfg!(windows) {
        // PowerShell 7, else Windows PowerShell (always there), else cmd. `-NoLogo`: no banner.
        let on_path = |exe: &str| {
            std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
        };
        for exe in ["pwsh.exe", "powershell.exe"] {
            if on_path(exe) {
                return (exe.into(), vec!["-NoLogo".into()]);
            }
        }
        (std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()), vec![])
    } else {
        (std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()), vec!["-l".into()])
    }
}

impl Ptys {
    pub fn open(
        &mut self,
        cwd: &Path,
        cmd: Option<Vec<String>>,
        cols: u16,
        rows: u16,
        env: &[(&str, &Path)],
        tx: Sender<ServerMsg>,
    ) -> Result<PtyId, RpcError> {
        let err = |e: anyhow::Error| RpcError::new(ErrorKind::Io, format!("pty: {e}"));
        let pair = native_pty_system().openpty(size(cols, rows)).map_err(err)?;
        let (prog, args) = match cmd {
            Some(mut v) if !v.is_empty() => {
                let p = v.remove(0);
                (p, v)
            }
            _ => default_shell(),
        };
        let mut cb = CommandBuilder::new(prog);
        cb.args(args);
        // Wire-style `C:/proj` works for file APIs, but shells show and reuse the cwd as given.
        if cfg!(windows) {
            cb.cwd(cwd.to_string_lossy().replace('/', "\\"));
        } else {
            cb.cwd(cwd);
        }
        cb.env("TERM", "xterm-256color");
        cb.env("COLORTERM", "truecolor");
        cb.env("TERM_PROGRAM", "nib");
        for (k, v) in env {
            cb.env(k, v);
        }
        let mut child = pair.slave.spawn_command(cb).map_err(err)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(err)?;
        let writer = pair.master.take_writer().map_err(err)?;
        let killer = child.clone_killer();
        let master: Master = Arc::new(Mutex::new(Some(pair.master)));
        let id = self.next;
        self.next += 1;
        // The exit is noticed by waiting on the child, not by the output ending: Windows' ConPTY
        // keeps its pipe open after the shell exits (until the pty is closed, which the client
        // does on PtyExit). Output that is still coming gets a moment to arrive first.
        let (eof_tx, eof_rx) = std::sync::mpsc::channel::<()>();
        let out_tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let msg = ServerNotify::PtyOutput { id, data: buf[..n].to_vec() };
                        if out_tx.send(ServerMsg::Notify(msg)).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = eof_tx.send(());
        });
        let closer = master.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            let _ = eof_rx.recv_timeout(std::time::Duration::from_millis(250));
            closer.lock().unwrap().take();
            let _ = tx.send(ServerMsg::Notify(ServerNotify::PtyExit { id, code }));
        });
        self.map.insert(id, Pty { master, writer, killer });
        Ok(id)
    }

    pub fn input(&mut self, id: PtyId, data: &[u8]) {
        if let Some(p) = self.map.get_mut(&id) {
            let _ = p.writer.write_all(data).and_then(|_| p.writer.flush());
        }
    }

    pub fn resize(&mut self, id: PtyId, cols: u16, rows: u16) {
        if let Some(m) = self.map.get(&id).and_then(|p| p.master.lock().ok()) {
            if let Some(m) = m.as_ref() {
                let _ = m.resize(size(cols, rows));
            }
        }
    }

    pub fn close(&mut self, id: PtyId) {
        if let Some(mut p) = self.map.remove(&id) {
            let _ = p.killer.kill();
        }
    }
}

impl Drop for Ptys {
    fn drop(&mut self) {
        for (_, mut p) in self.map.drain() {
            let _ = p.killer.kill();
        }
    }
}
