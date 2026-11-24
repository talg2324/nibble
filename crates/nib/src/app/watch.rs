//! Open files follow changes made outside nib. nibd polls the files of the open tabs
//! (`ClientNotify::Watch`) and says when one changed (`ServerNotify::FileChanged`); a tab without
//! unsaved edits is read again, a tab with edits keeps them and the user is told (saving then meets
//! the usual conflict instead of overwriting).

use super::{App, Message, Pending};
use crate::media::{Kind, Media};
use nib_proto::{ClientNotify, Request};
use std::path::Path;
use std::sync::Arc;

impl App {
    /// The files nibd should watch: every open tab backed by a file on the daemon's machine that
    /// can be read again (text, tables, previews, images, PDFs).
    fn watch_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .tabs
            .iter()
            .filter(|t| !t.local && matches!(t.media, None | Some(Media::Image { .. } | Media::Pdf(_) | Media::Loading | Media::Failed(_))))
            .filter_map(|t| Some(t.doc.path.as_ref()?.to_string_lossy().into_owned()))
            .chain(self.compile_watch_paths())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// After every update: tell nibd when the set of open files changed (or the connection did).
    pub(super) fn sync_watch(&mut self) {
        if !self.live() {
            return;
        }
        let paths = self.watch_paths();
        if self.watched.as_ref().is_some_and(|(gen, p)| *gen == self.gen && *p == paths) {
            return;
        }
        self.watched = Some((self.gen, paths.clone()));
        self.notify(ClientNotify::Watch { paths });
    }

    pub(super) fn on_file_changed(&mut self, path: String, mtime_ns: Option<u128>) -> Task {
        // Follow is playing an agent's edit in that tab; it reads the file again when done.
        if self.ai_follow_holds(&path) {
            return iced::Task::none();
        }
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        let tabs: Vec<_> = self
            .tabs
            .iter()
            .filter(|t| !t.local && t.doc.path.as_deref() == Some(Path::new(&path)))
            .map(|t| (t.id, t.media.is_some(), t.kind == Kind::Pdf, t.doc.is_modified(), t.doc.disk_mtime_ns))
            .collect();
        let mut tasks = vec![self.compile_file_changed(&path)];
        for (tab, media, pdf, modified, known) in tabs {
            match mtime_ns {
                None => tasks.push(self.flash_warn(format!("{name} was deleted on disk; saving writes it again"))),
                // Already what the tab holds (its own save, or an agent's edit already read).
                Some(m) if known == Some(m) => {}
                // A rebuilt PDF (LaTeX, a report): parsed again, where it was being read.
                Some(_) if pdf => tasks.push(self.request(Request::ReadFile { path: path.clone() }, Pending::ReloadPdf { tab })),
                Some(_) if media => tasks.push(self.request(Request::ReadFile { path: path.clone() }, Pending::ReloadMedia { tab })),
                Some(_) if modified => tasks.push(self.flash_warn(format!(
                    "{name} changed on disk; your unsaved edits are kept, saving will ask before overwriting"
                ))),
                Some(_) => tasks.push(self.request(Request::ReadFile { path: path.clone() }, Pending::Reload { tab })),
            }
        }
        iced::Task::batch(tasks)
    }

    /// An image tab's file changed: decode the new one; the old picture stays until it's ready.
    pub(super) fn on_reload_media(&mut self, tab: u64, data: Vec<u8>) -> Task {
        if !self.tabs.iter().any(|t| t.id == tab) {
            return iced::Task::none();
        }
        iced::Task::perform(async move { crate::media::decode(&data).map(Arc::new) }, move |r| Message::MediaDecoded(tab, r))
    }
}

type Task = iced::Task<Message>;

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;
    use nib_editor::Document;
    use nib_proto::{Response, ServerMsg, ServerNotify};

    fn changed(a: &mut App, path: &str, mtime_ns: Option<u128>) {
        let _ = a.update(Message::Server(ServerMsg::Notify(ServerNotify::FileChanged { path: path.into(), mtime_ns })));
    }

    fn reloads(a: &App) -> Vec<(nib_proto::ReqId, u64)> {
        a.pending
            .iter()
            .filter_map(|(id, p)| match p {
                Pending::Reload { tab } => Some((*id, *tab)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn open_files_follow_changes_made_outside_nib() {
        let mut a = app();
        a.link = crate::app::Link::Live;
        for name in ["clean.py", "dirty.py"] {
            let mut doc = Document::from_bytes(b"one\n", Some(format!("/tmp/{name}").into()));
            doc.disk_mtime_ns = Some(1);
            a.put_doc(doc, true);
        }
        let _ = a.update(Message::Tick);
        assert_eq!(a.watched.as_ref().map(|(_, p)| p.clone()), Some(vec!["/tmp/clean.py".to_string(), "/tmp/dirty.py".to_string()]));

        // The stamp the tab already has (its own save): nothing to do.
        changed(&mut a, "/tmp/clean.py", Some(1));
        assert!(reloads(&a).is_empty());

        // A clean tab is read again.
        changed(&mut a, "/tmp/clean.py", Some(2));
        let r = reloads(&a);
        assert_eq!(r.len(), 1);
        let (id, tab) = r[0];
        assert_eq!(tab, a.tabs[0].id);
        let file = Response::File { path: "/tmp/clean.py".into(), data: b"uno\n".to_vec(), mtime_ns: 2 };
        let _ = a.update(Message::Server(ServerMsg::Response { id, result: Ok(file) }));
        assert_eq!(a.tabs[0].doc.text.to_string(), "uno\n");

        // A tab with unsaved edits keeps them; the user is told.
        a.active = 1;
        let _ = a.update(Message::Key { chord: nib_editor::keymap::KeyChord::new(nib_editor::keymap::Key::Char('x')), text: Some("x".into()), repeat: false });
        changed(&mut a, "/tmp/dirty.py", Some(3));
        assert!(reloads(&a).is_empty());
        assert!(a.tabs[1].doc.is_modified() && a.tabs[1].doc.text.to_string().contains('x'));
        assert!(a.flash.as_ref().is_some_and(|f| f.0.contains("changed on disk")));

        // Gone from disk: said, the tab stays.
        changed(&mut a, "/tmp/clean.py", None);
        assert!(a.flash.as_ref().is_some_and(|f| f.0.contains("deleted")));
        assert_eq!(a.tabs.len(), 2);

        // An image tab asks for the new picture (decoded off the UI thread, the old one shown meanwhile).
        a.put_media("/tmp/plot.png".into(), Media::Loading, true);
        let _ = a.update(Message::Tick);
        changed(&mut a, "/tmp/plot.png", Some(4));
        assert!(a.pending.values().any(|p| matches!(p, Pending::ReloadMedia { .. })));
        // A PDF (still loading, too) is parsed again as a PDF, not handed to the image decoder.
        a.put_media("/tmp/report.pdf".into(), Media::Loading, true);
        let _ = a.update(Message::Tick);
        changed(&mut a, "/tmp/report.pdf", Some(5));
        assert!(a.pending.values().any(|p| matches!(p, Pending::ReloadPdf { .. })));
        a.active = 0;

        // Closing a tab takes it off the watch list.
        a.active = 0;
        let _ = a.update(Message::Control(nib_editor::Command::CloseTab));
        assert_eq!(a.watched.as_ref().map(|(_, p)| p.clone()), Some(vec!["/tmp/dirty.py".to_string(), "/tmp/plot.png".to_string(), "/tmp/report.pdf".to_string()]));
    }
}
