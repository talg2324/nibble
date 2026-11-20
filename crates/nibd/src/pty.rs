//! Pseudo-terminals for the client's terminal panes. Output is streamed as
//! `ServerNotify::PtyOutput` through the connection's writer channel.

use nib_proto::{ErrorKind, PtyId, RpcError, ServerMsg, ServerNotify};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::Sender;

struct Pty {
    master: Box<dyn MasterPty + Send>,
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
        cb.cwd(cwd);
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
        let id = self.next;
        self.next += 1;
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let msg = ServerNotify::PtyOutput { id, data: buf[..n].to_vec() };
                        if tx.send(ServerMsg::Notify(msg)).is_err() {
                            break;
                        }
                    }
                }
            }
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            let _ = tx.send(ServerMsg::Notify(ServerNotify::PtyExit { id, code }));
        });
        self.map.insert(id, Pty { master: pair.master, writer, killer });
        Ok(id)
    }

    pub fn input(&mut self, id: PtyId, data: &[u8]) {
        if let Some(p) = self.map.get_mut(&id) {
            let _ = p.writer.write_all(data).and_then(|_| p.writer.flush());
        }
    }

    pub fn resize(&mut self, id: PtyId, cols: u16, rows: u16) {
        if let Some(p) = self.map.get(&id) {
            let _ = p.master.resize(size(cols, rows));
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
