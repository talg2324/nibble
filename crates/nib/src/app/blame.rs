//! Git blame for the selection: who last committed the selected lines, how long ago, and the
//! commit, drawn dim after the selection's last line (`EditorView::blame`). Asked of the daemon's
//! git a moment after the selection stops changing, so a drag doesn't start a git per step; an
//! answer for a selection that has changed since is dropped. `[editor] blame = false` turns it off.

use super::{async_sleep, App, Message, Pending};
use nib_proto::{GitBlameCommit, Request, Response, RpcError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long the selection must stay put before git is asked.
const SETTLE: Duration = Duration::from_millis(150);

/// What is blamed: the tab, the document's version, and the 0-based lines `first..=last`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    tab: u64,
    version: u64,
    first: usize,
    last: usize,
}

pub struct State {
    /// `[editor] blame`.
    pub on: bool,
    key: Option<Key>,
    /// Bumped whenever `key` changes: a wait or an answer from before is stale.
    gen: u64,
    /// The text to show, for `key`.
    label: Option<String>,
}

impl State {
    pub fn new(on: bool) -> Self {
        Self { on, key: None, gen: 0, label: None }
    }
}

impl App {
    /// After every update: when the selected lines changed, forget the old blame and ask again
    /// once they settle.
    pub(super) fn blame_follow(&mut self) -> Task {
        let want = self.blame_want();
        if want == self.blame.key {
            return iced::Task::none();
        }
        self.blame.key = want;
        self.blame.gen += 1;
        self.blame.label = None;
        if want.is_none() {
            return iced::Task::none();
        }
        let gen = self.blame.gen;
        iced::Task::perform(async_sleep(SETTLE), move |_| Message::BlameAsk(gen))
    }

    fn blame_want(&self) -> Option<Key> {
        if !self.blame.on {
            return None;
        }
        let t = self.tab()?;
        if t.local || t.diff || t.web.is_some() || t.media.is_some() || t.doc.path.is_none() {
            return None;
        }
        let r = t.doc.selection.primary();
        if r.is_empty() {
            return None;
        }
        let text = t.doc.text.slice(..);
        let first = text.char_to_line(r.from());
        let mut last = text.char_to_line(r.to());
        // A selection that ends at the start of a line (whole lines selected) doesn't take it.
        if last > first && text.line_to_char(last) == r.to() {
            last -= 1;
        }
        Some(Key { tab: t.id, version: t.doc.version(), first, last })
    }

    pub(super) fn blame_ask(&mut self, gen: u64) -> Task {
        if gen != self.blame.gen || self.lost() {
            return iced::Task::none();
        }
        let Some(key) = self.blame.key else { return iced::Task::none() };
        let Some(t) = self.tabs.iter().find(|t| t.id == key.tab) else { return iced::Task::none() };
        let Some(path) = &t.doc.path else { return iced::Task::none() };
        let req = Request::GitBlame {
            path: path.to_string_lossy().into_owned(),
            first: key.first as u32 + 1,
            last: key.last as u32 + 1,
            // Unsaved edits: blame what the editor shows, so the lines match.
            contents: t.doc.is_modified().then(|| t.doc.text.to_string()),
        };
        self.request(req, Pending::Blame { gen })
    }

    pub(super) fn on_blame(&mut self, gen: u64, result: Result<Response, RpcError>) -> Task {
        if gen == self.blame.gen {
            if let Ok(Response::GitBlame(commits)) = result {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                self.blame.label = label(&commits, now);
            }
        }
        iced::Task::none()
    }

    /// For the editor of tab `tab`: the 0-based line to draw the blame after, and its text.
    pub(super) fn blame_shown(&self, tab: u64) -> Option<(usize, String)> {
        let key = self.blame.key.filter(|k| k.tab == tab)?;
        Some((key.last, self.blame.label.clone()?))
    }

    /// The blame shown now (`nib ctl state`).
    pub(super) fn blame_label(&self) -> Option<&str> {
        self.blame.label.as_deref()
    }
}

type Task = iced::Task<Message>;

/// `ada · 3d ago · dfb39ef  +1 more`: the newest commit of the lines, and how many others.
/// Lines not committed yet count as one more; alone, they say so.
fn label(commits: &[GitBlameCommit], now: i64) -> Option<String> {
    let newest = commits.iter().filter(|c| c.committed).max_by_key(|c| c.time);
    let Some(c) = newest else {
        return (!commits.is_empty()).then(|| "not committed yet".to_string());
    };
    let mut s = format!("{} · {} · {}", c.author, super::git::ago(now - c.time), c.hash);
    let more = commits.len() - 1;
    if more > 0 {
        s.push_str(&format!("  +{more} more"));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(author: &str, time: i64, committed: bool) -> GitBlameCommit {
        GitBlameCommit { hash: format!("{author:0<7}")[..7].to_string(), author: author.into(), time, committed, lines: 1 }
    }

    #[test]
    fn names_the_newest_commit_and_counts_the_rest() {
        let now = 1_700_000_000;
        assert_eq!(label(&[], now), None);
        assert_eq!(label(&[c("ada", now - 3 * 86_400, true)], now).unwrap(), "ada · 3d ago · ada0000");
        let mixed = [c("ada", now - 3 * 86_400, true), c("tidepool", now - 7200, true), c("Not Committed Yet", now, false)];
        assert_eq!(label(&mixed, now).unwrap(), "tidepool · 2h ago · tidepoo  +2 more");
        assert_eq!(label(&[c("Not Committed Yet", now, false)], now).unwrap(), "not committed yet");
    }
}
