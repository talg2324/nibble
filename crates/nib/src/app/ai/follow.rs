//! Follow (D-049, D-058): watch one agent the way you would watch a colleague at the next desk.
//!
//! While an agent is followed, what it does drives the editor, at a pace a person can follow:
//! the file it reads comes forward and a cursor walks down the lines it reads, tinting them; an
//! edit is played out where it lands: the old text is selected, removed, and the new text typed
//! in, then glows for a moment. A cursor with the agent's name on a flag does it. The tab
//! following opened closes when the agent moves on (unless it was open before, or has unsaved
//! changes). A shell command that reads or writes a file (`cat`, `sed -n`, a heredoc, `sed -i`)
//! shows that file the same way (D-057); any other command brings the agents tab forward.
//!
//! The agent is usually faster than the show: what it does is queued as beats and played one
//! after the other, faster the longer the queue. The edit is typed into the tab, not the file;
//! once the show is done with a file the tab is read again from disk, so what stays is what the
//! agent wrote. All of it is derived from the steps the harness reports: the agent does nothing
//! for it and spends no tokens on it.
//!
//! Typing in the editor stops following: the keyboard is the user's.

use super::act::Act;
use super::{Session, Step};
use crate::app::{App, Focus, Message, Pending};
use crate::editor_view::Mark;
use iced::{Color, Task};
use nib_proto::{AiStepKind, Pos, Request};
use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

/// Following one agent.
pub(super) struct Follow {
    /// The chat's key.
    pub chat: u64,
    /// The tab following opened: closed when the agent moves on.
    pub tab: Option<String>,
    /// What is still to be shown, oldest first.
    queue: VecDeque<Beat>,
    /// What is being shown.
    play: Option<Play>,
    /// Tabs (id, path) whose text the show changed: read again from disk when it is done.
    touched: Vec<(u64, String)>,
    /// Tinted text: the tab, the marks, how far each has faded (see `Tint`).
    marks: Option<(u64, Vec<(usize, usize, Tint)>)>,
    /// When the last frame was played (the clock runs on frames).
    last: Option<Instant>,
}

#[derive(Clone, Copy, PartialEq)]
enum Tint {
    /// Lines being read.
    Read,
    /// What was just written; `f32`: how much of the glow is left, 1 → 0.
    Added(f32),
}

/// One thing the agent did, to be shown.
#[derive(Debug, Clone)]
enum Beat {
    /// A file comes forward at a line (1-based; 0: where it is).
    Show { path: String, line: u32 },
    /// It reads lines `from` (1-based) to `to`.
    Read { path: String, from: u32, to: Option<u32> },
    /// `old` was replaced by `new`, near `line`.
    Edit { path: String, old: String, new: String, line: Option<u32> },
    /// The file was written whole, or added to (`append`). `body`: what, when known.
    Write { path: String, body: Option<String>, append: bool },
    /// The file changed in a way the step doesn't say (`sed -i`): read again.
    Changed { path: String },
    /// The agents tab, on the followed agent: it is in its terminal, or it has finished.
    Agent,
}

impl Beat {
    fn path(&self) -> Option<&str> {
        match self {
            Beat::Show { path, .. } | Beat::Read { path, .. } | Beat::Edit { path, .. } | Beat::Write { path, .. } | Beat::Changed { path } => Some(path),
            Beat::Agent => None,
        }
    }

    /// Worth dropping when the show falls far behind: it changes nothing.
    fn idle(&self) -> bool {
        matches!(self, Beat::Show { .. } | Beat::Read { .. })
    }
}

struct Play {
    beat: Beat,
    /// Time into the beat (sped up when the queue is long).
    t: Duration,
    run: Run,
}

/// How a beat is being played.
enum Run {
    /// Waiting for its file to open (real time waited so far).
    Opening { asked: bool, waited: Duration },
    /// Standing still until `until`.
    Hold { until: Duration },
    /// A cursor walks lines `from`..=`to` (0-based) of `tab` in `walk`, tinting them; then holds.
    Sweep { tab: u64, from: usize, to: usize, walk: Duration, hold: Duration, tint: Tint },
    /// In `tab` at char `at`: `old` chars are selected over `select`, held for `pause`, removed,
    /// and `new` typed over `typing`; it then glows for `glow`.
    Type { tab: u64, at: usize, old: usize, new: Vec<char>, select: Duration, pause: Duration, typing: Duration, glow: Duration, typed: usize, cut: bool },
}

const OPEN_TIMEOUT: Duration = Duration::from_secs(4);
const SHOW_HOLD: Duration = Duration::from_millis(450);
const AGENT_HOLD: Duration = Duration::from_millis(700);
const READ_HOLD: Duration = Duration::from_millis(350);
const GLOW: Duration = Duration::from_millis(1100);
/// Longer than this, a written file is not typed but painted line by line.
const TYPE_MAX: usize = 3000;
/// The longest frame counted (a stall must not skip a beat).
const FRAME_MAX: Duration = Duration::from_millis(50);

fn clamp(d: f64, lo: u64, hi: u64) -> Duration {
    Duration::from_millis((d as u64).clamp(lo, hi))
}

fn frac(t: Duration, d: Duration) -> f32 {
    if d.is_zero() { 1.0 } else { (t.as_secs_f32() / d.as_secs_f32()).min(1.0) }
}

impl App {
    pub(super) fn ai_following(&self) -> Option<u64> {
        self.ai.follow.as_ref().map(|f| f.chat)
    }

    /// Follow the shown agent, or stop following it.
    pub(super) fn ai_follow_toggle(&mut self) -> Task<Message> {
        let key = self.ai.key;
        if self.ai_following() == Some(key) {
            return self.ai_follow_stop(true);
        }
        let t = self.ai_follow_stop(true);
        self.ai.follow = Some(Follow { chat: key, tab: None, queue: VecDeque::new(), play: None, touched: Vec::new(), marks: None, last: None });
        t
    }

    /// Stop following. `tidy`: also close the tab following opened. Tabs the show typed into are
    /// read again, so they hold what is on disk.
    pub(in crate::app) fn ai_follow_stop(&mut self, tidy: bool) -> Task<Message> {
        let Some(f) = self.ai.follow.take() else { return Task::none() };
        if let (true, Some(path)) = (tidy, &f.tab) {
            self.ai_follow_close(path);
        }
        Task::batch(f.touched.into_iter().map(|(tab, _)| self.ai_follow_reload(tab)).collect::<Vec<_>>())
    }

    /// The user typed in the editor while following: the editor is theirs again. `true`: the
    /// key must not be typed (the tab still shows the show's text, about to be read again).
    pub(in crate::app) fn ai_follow_interrupted(&mut self) -> (Task<Message>, bool) {
        let Some(f) = &self.ai.follow else { return (Task::none(), false) };
        let held = self.tab().is_some_and(|t| f.touched.iter().any(|(id, _)| *id == t.id));
        let t = self.ai_follow_stop(false);
        (Task::batch([t, self.flash_info("stopped following: the editor is yours")]), held)
    }

    /// Queue what the followed agent does (called while its event is handled).
    fn ai_follow_queue(&mut self, beat: Beat) {
        let key = self.ai.key;
        let Some(f) = self.ai.follow.as_mut().filter(|f| f.chat == key) else { return };
        if matches!(beat, Beat::Agent) && matches!(f.queue.back(), Some(Beat::Agent)) {
            return;
        }
        f.queue.push_back(beat);
        // Far behind: what only looks is dropped, what changes files stays.
        while f.queue.len() > 16 {
            match f.queue.iter().position(Beat::idle) {
                Some(i) => drop(f.queue.remove(i)),
                None => break,
            }
        }
    }

    /// The followed agent started `step`: what it reads, and the file it is about to edit.
    pub(super) fn ai_follow_step(&mut self, step: &Step) {
        // A file being written may not exist yet: it shows once written (`ai_follow_done`).
        if step.acts.iter().any(|a| matches!(a, Act::Write { .. } | Act::Patch { .. })) {
            return;
        }
        let beat = match step.acts.first() {
            Some(Act::Read { path, from, to }) => Beat::Read { path: path.clone(), from: *from, to: *to },
            // Brought forward now; the edit is played when it has landed (`ai_follow_done`).
            Some(Act::Edit { path }) => Beat::Show { path: path.clone(), line: 0 },
            _ => Beat::Agent,
        };
        self.ai_follow_queue(beat);
    }

    /// A step of the followed agent ended: what it changed is played out.
    pub(super) fn ai_follow_done(&mut self, step: &Step) {
        let Some(d) = step.done.as_ref().filter(|d| d.ok) else { return };
        let beats: Vec<Beat> = step
            .acts
            .iter()
            .filter(|a| a.writes())
            .map(|a| match (a, &step.kind) {
                (Act::Edit { path }, AiStepKind::Edit { old, new, .. }) => Beat::Edit { path: path.clone(), old: old.clone(), new: new.clone(), line: d.line },
                (Act::Edit { path } | Act::Patch { path }, _) => Beat::Changed { path: path.clone() },
                (Act::Write { path, body, append }, kind) => {
                    let body = match kind {
                        AiStepKind::Write { content, .. } => Some(content.clone()),
                        _ => body.clone(),
                    };
                    Beat::Write { path: path.clone(), body, append: *append }
                }
                (Act::Read { .. }, _) => unreachable!(),
            })
            .collect();
        for b in beats {
            self.ai_follow_queue(b);
        }
    }

    /// The followed agent's turn ended: back to the agents tab, its file closed.
    pub(super) fn ai_follow_turn_done(&mut self) {
        self.ai_follow_queue(Beat::Agent);
    }

    /// Following has something to show: frames drive it.
    pub(in crate::app) fn ai_follow_playing(&self) -> bool {
        self.ai.follow.as_ref().is_some_and(|f| f.play.is_some() || !f.queue.is_empty())
    }

    /// Still showing `path` (or about to): reading it again now would spoil the show, which reads
    /// it again itself when done.
    pub(in crate::app) fn ai_follow_holds(&self, path: &str) -> bool {
        let Some(f) = &self.ai.follow else { return false };
        f.touched.iter().any(|(_, p)| p == path)
            || f.queue.iter().chain(f.play.as_ref().map(|p| &p.beat)).any(|b| b.writes_to(path))
    }

    /// An open, unedited tab on `path` is read again (an agent changed the file).
    pub(super) fn ai_reread(&mut self, path: &str) -> Task<Message> {
        if self.ai_follow_holds(path) {
            return Task::none();
        }
        let tab = self.tabs.iter().find(|t| t.doc.path.as_deref() == Some(Path::new(path)) && !t.doc.is_modified() && t.media.is_none() && !t.local).map(|t| t.id);
        match tab {
            Some(tab) => self.request(Request::ReadFile { path: path.to_string() }, Pending::Reload { tab }),
            None => Task::none(),
        }
    }

    /// Whatever agent changed `step`'s files: a tab showing one (without unsaved edits) is read
    /// again, followed or not. A failed command may still have written some of it.
    pub(super) fn ai_file_changed(&mut self, step: &Step) -> Task<Message> {
        let ok = step.done.as_ref().is_some_and(|d| d.ok) || matches!(step.kind, AiStepKind::Command { .. });
        if !ok {
            return Task::none();
        }
        Task::batch(step.acts.iter().filter(|a| a.writes()).map(|a| self.ai_reread(a.path())).collect::<Vec<_>>())
    }

    /// Read tab `tab` again from disk whatever its stamp says (the show typed into it).
    fn ai_follow_reload(&mut self, tab: u64) -> Task<Message> {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) else { return Task::none() };
        let Some(path) = t.doc.path.as_ref().map(|p| p.to_string_lossy().into_owned()) else { return Task::none() };
        let r = t.doc.save_point();
        // Stamp 0: no file on disk has it, so the reload is taken and a save meets a conflict.
        t.doc.mark_saved(r, 0);
        self.request(Request::ReadFile { path }, Pending::Reload { tab })
    }

    /// Close the tab following opened, unless the user has typed in it.
    fn ai_follow_close(&mut self, path: &str) {
        let Some(i) = self.tabs.iter().position(|t| t.doc.path.as_deref() == Some(Path::new(path))) else { return };
        if self.tabs[i].doc.is_modified() {
            return;
        }
        if let Some(f) = self.ai.follow.as_mut() {
            let id = self.tabs[i].id;
            f.touched.retain(|(t, _)| *t != id);
            f.marks.take_if(|m| m.0 == id);
        }
        self.lsp_close(i);
        self.tabs.remove(i);
        if self.active > i || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
    }

    /// Kick the show after the followed agent's event (frames carry it on).
    pub(super) fn ai_follow_apply(&mut self) -> Task<Message> {
        self.ai_follow_advance(Duration::ZERO)
    }

    /// A frame: the show moves on by the time since the last one.
    pub(in crate::app) fn ai_follow_tick(&mut self) -> Task<Message> {
        let now = Instant::now();
        let Some(f) = self.ai.follow.as_mut() else { return Task::none() };
        let dt = f.last.map_or(Duration::ZERO, |l| now - l).min(FRAME_MAX);
        f.last = Some(now);
        self.ai_follow_advance(dt)
    }

    /// Play `dt` of the show: finish beats, start the next.
    pub(super) fn ai_follow_advance(&mut self, dt: Duration) -> Task<Message> {
        let mut tasks = Vec::new();
        let mut dt = dt;
        // A few beats per frame at most (instant ones: Agent, a file not found).
        for _ in 0..8 {
            let Some(f) = self.ai.follow.as_mut() else { break };
            if f.play.is_none() {
                let Some(beat) = f.queue.pop_front() else {
                    f.last = None;
                    break;
                };
                f.play = Some(Play { beat, t: Duration::ZERO, run: Run::Opening { asked: false, waited: Duration::ZERO } });
            }
            // Faster the further behind.
            let speed = 1.0 + 0.6 * f.queue.len().min(7) as f32;
            let (done, t) = self.ai_follow_play(dt, speed);
            tasks.push(t);
            if !done {
                break;
            }
            dt = Duration::ZERO;
            let ended = self.ai.follow.as_mut().and_then(|f| f.play.take());
            if let Some(p) = ended {
                tasks.push(self.ai_follow_ended(p));
            }
        }
        Task::batch(tasks)
    }

    /// A beat is over: a file it typed into that nothing queued will touch again is read from
    /// disk; the glow it left fades out with the next beat.
    fn ai_follow_ended(&mut self, p: Play) -> Task<Message> {
        let Some(f) = self.ai.follow.as_mut() else { return Task::none() };
        // The lines read, the glow: gone; the flag stays on the tab.
        if let Some(m) = &mut f.marks {
            m.1.clear();
        }
        let (Run::Type { tab, .. } | Run::Sweep { tab, .. }) = p.run else { return Task::none() };
        let Some(path) = p.beat.path() else { return Task::none() };
        if f.queue.iter().any(|b| b.writes_to(path)) {
            return Task::none();
        }
        let was = f.touched.len();
        f.touched.retain(|(t, _)| *t != tab);
        if f.touched.len() == was {
            return Task::none();
        }
        self.ai_follow_reload(tab)
    }

    /// Play the current beat on. `(over, task)`.
    fn ai_follow_play(&mut self, dt: Duration, speed: f32) -> (bool, Task<Message>) {
        let Some(p) = self.ai.follow.as_mut().and_then(|f| f.play.as_mut()) else { return (true, Task::none()) };
        p.t += dt.mul_f32(speed);
        let t = p.t;
        let beat = p.beat.clone();
        match &mut p.run {
            Run::Opening { asked, waited } => {
                *waited += dt;
                let (asked_before, waited) = (*asked, *waited);
                *asked = true;
                let Some(path) = beat.path().map(str::to_string) else {
                    self.ai_follow_show_agent();
                    return self.ai_follow_run(Run::Hold { until: AGENT_HOLD });
                };
                if let Some(i) = self.tabs.iter().position(|t| t.doc.path.as_deref() == Some(Path::new(&path))) {
                    self.ai_follow_front(&path, i);
                    return self.ai_follow_begin(i, beat);
                }
                if waited > OPEN_TIMEOUT {
                    return (true, Task::none());
                }
                if asked_before {
                    return (false, Task::none());
                }
                // Opened by following: it closes when the agent moves on.
                self.ai_follow_leave(&path);
                if let Some(f) = self.ai.follow.as_mut() {
                    f.tab = Some(path.clone());
                }
                self.ai.front = false;
                self.focus = Focus::Main;
                let goto = match beat {
                    Beat::Show { line, .. } | Beat::Read { from: line, .. } if line > 0 => Some(Pos { line: line - 1, col: 0 }),
                    _ => None,
                };
                (false, self.open_path_at(path, true, goto))
            }
            Run::Hold { until } => (t >= *until, Task::none()),
            Run::Sweep { tab, from, to, walk, hold, tint } => {
                let (tab, from, to, walk, hold, tint) = (*tab, *from, *to, *walk, *hold, *tint);
                let line = from + ((to - from) as f32 * frac(t, walk)).round() as usize;
                self.ai_follow_draw(tab, |doc| {
                    let a = doc.text.line_to_char(from);
                    let b = nib_editor::document::line_end(doc.text.slice(..), line);
                    doc.set_cursor(doc.text.line_to_char(line), false);
                    let tint = if let Tint::Added(_) = tint { Tint::Added(1.0 - frac(t.saturating_sub(walk), hold)) } else { tint };
                    vec![(a, b, tint)]
                });
                (t >= walk + hold, Task::none())
            }
            Run::Type { tab, at, old, new, select, pause, typing, glow, typed, cut } => {
                let (tab, at, old, select, pause, typing, glow) = (*tab, *at, *old, *select, *pause, *typing, *glow);
                let cut_now = !*cut && t >= select + pause;
                *cut |= cut_now;
                let want = if t >= select + pause { (new.len() as f32 * frac(t - select - pause, typing)).ceil() as usize } else { 0 };
                let add: String = new[*typed..want.max(*typed)].iter().collect();
                let from = *typed;
                *typed = want.max(*typed);
                let typed = *typed;
                let fade = 1.0 - frac(t.saturating_sub(select + pause + typing), glow);
                self.ai_follow_draw(tab, |doc| {
                    if t < select + pause {
                        // Selecting what goes.
                        let head = at + (old as f32 * frac(t, select)).round() as usize;
                        doc.set_cursor(at, false);
                        doc.set_cursor(head, true);
                        return Vec::new();
                    }
                    if cut_now && old > 0 {
                        doc.apply_edits(vec![(at, at + old, String::new())]);
                    }
                    if !add.is_empty() {
                        doc.apply_edits(vec![(at + from, at + from, add)]);
                    }
                    doc.set_cursor(at + typed, false);
                    vec![(at, at + typed, Tint::Added(fade))]
                });
                (t >= select + pause + typing + glow, Task::none())
            }
        }
    }

    fn ai_follow_run(&mut self, run: Run) -> (bool, Task<Message>) {
        if let Some(p) = self.ai.follow.as_mut().and_then(|f| f.play.as_mut()) {
            p.run = run;
            p.t = Duration::ZERO;
        }
        (false, Task::none())
    }

    /// The followed agent's file `path`, at tab `i`, comes forward; the one following opened
    /// before is closed if it is another.
    fn ai_follow_front(&mut self, path: &str, i: usize) {
        let id = self.tabs[i].id;
        self.ai_follow_leave(path);
        if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
            self.active = i;
        }
        self.ai.front = false;
        self.focus = Focus::Main;
    }

    /// Moving to `path`: the tab following opened for another file closes.
    fn ai_follow_leave(&mut self, path: &str) {
        let owned = self.ai.follow.as_ref().and_then(|f| f.tab.clone());
        if let Some(old) = owned.filter(|o| o != path) {
            self.ai_follow_close(&old);
            if let Some(f) = self.ai.follow.as_mut() {
                f.tab = None;
            }
        }
    }

    fn ai_follow_show_agent(&mut self) {
        let Some(chat) = self.ai_following() else { return };
        if let Some(path) = self.ai.follow.as_mut().and_then(|f| f.tab.take()) {
            self.ai_follow_close(&path);
        }
        if let Some(f) = self.ai.follow.as_mut() {
            f.marks = None;
        }
        if let Some(i) = self.ai.chats.iter().position(|c| c.key == chat) {
            self.ai.cur = i;
        }
        self.ai.open = true;
        self.ai.front = true;
        // Closing its file changed the tabs: that is not a file coming forward.
        self.ai.tabs_seen = (self.active, self.tabs.len(), self.tab().map(|t| t.id));
        self.focus = Focus::Ai;
    }

    /// The file of `beat` is open at tab `i` and in front: work out how to play it.
    fn ai_follow_begin(&mut self, i: usize, beat: Beat) -> (bool, Task<Message>) {
        let tab = self.tabs[i].id;
        if let Some(f) = self.ai.follow.as_mut() {
            f.marks = Some((tab, Vec::new()));
        }
        let doc = &self.tabs[i].doc;
        let lines = doc.text.len_lines().max(1);
        let run = match beat {
            Beat::Agent => unreachable!(),
            Beat::Show { line, .. } => {
                if line > 0 {
                    self.goto_in_active(Pos { line: line - 1, col: 0 });
                }
                Run::Hold { until: SHOW_HOLD }
            }
            Beat::Read { from, to, .. } => {
                let from = (from.max(1) as usize - 1).min(lines - 1);
                // No end said: what fits on the screen.
                let to = to.map_or(from + self.rows.max(2) - 2, |t| t.max(1) as usize - 1).min(lines - 1).max(from);
                let n = (to - from + 1) as f64;
                Run::Sweep { tab, from, to, walk: clamp(n * 35.0, 300, 1800), hold: READ_HOLD, tint: Tint::Read }
            }
            Beat::Changed { .. } | Beat::Write { body: None, .. } => {
                // Nothing to type: read it again and let it glow whole.
                let mut t = Task::none();
                if !self.ai_follow_touched(tab) {
                    t = self.ai_follow_reload(tab);
                }
                return (self.ai_follow_run(Run::Hold { until: SHOW_HOLD }).0, t);
            }
            Beat::Edit { old, new, line, .. } => {
                let text = doc.text.to_string();
                let near = line.map_or(0, |l| doc.text.line_to_byte((l.max(1) as usize - 1).min(lines - 1)));
                let found = |s: &str| (!s.is_empty()).then(|| nearest(&text, s, near)).flatten();
                match (found(&old), found(&new)) {
                    (Some(b), _) => self.ai_follow_typing(i, doc.text.byte_to_char(b), old.chars().count(), &new),
                    // Landed already (opened after it, or a replay): put the old text back first.
                    (None, Some(b)) => {
                        let at = doc.text.byte_to_char(b);
                        let n = new.chars().count();
                        self.ai_follow_draw(tab, |d| {
                            d.apply_edits(vec![(at, at + n, old.clone())]);
                            Vec::new()
                        });
                        self.ai_follow_typing(i, at, old.chars().count(), &new)
                    }
                    // An insertion with nothing to anchor it, or the file moved on: read it again.
                    _ => {
                        let t = self.ai_follow_reload(tab);
                        return (self.ai_follow_run(Run::Hold { until: SHOW_HOLD }).0, t);
                    }
                }
            }
            Beat::Write { body: Some(body), append, .. } => {
                let len = doc.text.len_chars();
                let has = doc.text.to_string();
                let n = body.chars().count();
                if append {
                    // Already at its end: take it off, to add it again.
                    let at = if has.ends_with(&body) { len - n } else { len };
                    if at < len {
                        self.ai_follow_draw(tab, |d| {
                            d.apply_edits(vec![(at, len, String::new())]);
                            Vec::new()
                        });
                    }
                    self.ai_follow_typing(i, at, 0, &body)
                } else if n > TYPE_MAX {
                    // Too long to type: put it in and paint it top to bottom.
                    self.ai_follow_draw(tab, |d| {
                        if has != body {
                            d.apply_edits(vec![(0, len, body.clone())]);
                        }
                        d.set_cursor(0, false);
                        Vec::new()
                    });
                    let to = self.tabs[i].doc.text.len_lines().saturating_sub(1);
                    Run::Sweep { tab, from: 0, to, walk: clamp(to as f64 * 15.0, 400, 2000), hold: GLOW, tint: Tint::Added(1.0) }
                } else if has == body {
                    // A new file, opened once written: typed from empty.
                    self.ai_follow_draw(tab, |d| {
                        d.apply_edits(vec![(0, len, String::new())]);
                        Vec::new()
                    });
                    self.ai_follow_typing(i, 0, 0, &body)
                } else {
                    self.ai_follow_typing(i, 0, len, &body)
                }
            }
        };
        self.ai_follow_run(run)
    }

    fn ai_follow_touched(&self, tab: u64) -> bool {
        self.ai.follow.as_ref().is_some_and(|f| f.touched.iter().any(|(t, _)| *t == tab))
    }

    /// Replace `old` chars at `at` in tab `i` by `new`, played: the cursor goes there first.
    fn ai_follow_typing(&mut self, i: usize, at: usize, old: usize, new: &str) -> Run {
        let t = &mut self.tabs[i];
        t.doc.set_cursor(at, false);
        let id = t.id;
        self.follow_cursor();
        let new: Vec<char> = new.chars().collect();
        Run::Type {
            tab: id,
            at,
            old,
            select: if old == 0 { Duration::ZERO } else { clamp(old as f64 * 6.0, 250, 800) },
            pause: if old == 0 { Duration::from_millis(250) } else { Duration::from_millis(350) },
            typing: clamp(new.len() as f64 * 22.0, 150, 2500),
            glow: GLOW,
            new,
            typed: 0,
            cut: false,
        }
    }

    /// Change tab `tab`'s text as the show goes; `f` returns the marks to tint. The tab stays
    /// unmodified (read again from disk when the show is done with it) and its cursor in view.
    fn ai_follow_draw(&mut self, tab: u64, f: impl FnOnce(&mut nib_editor::Document) -> Vec<(usize, usize, Tint)>) {
        let Some(i) = self.tabs.iter().position(|t| t.id == tab) else { return };
        let t = &mut self.tabs[i];
        let version = t.doc.version();
        let marks = f(&mut t.doc);
        if t.doc.version() != version {
            let r = t.doc.save_point();
            t.doc.mark_saved(r, 0);
            let path = t.doc.path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            if let Some(f) = self.ai.follow.as_mut().filter(|f| !f.touched.iter().any(|(t, _)| *t == tab)) {
                f.touched.push((tab, path));
            }
        }
        if let Some(f) = self.ai.follow.as_mut() {
            f.marks = Some((tab, marks));
        }
        if self.active == i {
            self.follow_cursor();
        }
    }

    /// What tab `tab` shows tinted, and the followed agent's flag at its cursor.
    pub(in crate::app) fn ai_follow_view(&self, tab: u64) -> (Vec<Mark>, Option<(String, Color)>) {
        let Some(f) = &self.ai.follow else { return (Vec::new(), None) };
        let th = &self.theme;
        let marks = f.marks.as_ref().filter(|m| m.0 == tab).map_or_else(Vec::new, |(_, ms)| {
            ms.iter()
                .filter_map(|&(from, to, tint)| {
                    let (color, rows) = match tint {
                        Tint::Read => (Color { a: 0.07, ..th.accent }, true),
                        Tint::Added(a) if a > 0.0 => (Color { a: 0.32 * a, ..ADDED }, false),
                        Tint::Added(_) => return None,
                    };
                    Some(Mark { from, to, color, rows })
                })
                .collect()
        });
        let shows = f.marks.as_ref().is_some_and(|m| m.0 == tab);
        let tag = shows.then(|| self.ai_follow_label()).flatten().map(|(name, _)| (name, th.accent));
        (marks, tag)
    }

    /// Whether `path` is the tab following opened (the tab bar marks it).
    pub(in crate::app) fn ai_follow_tab(&self, path: &Path) -> bool {
        self.ai.follow.as_ref().and_then(|f| f.tab.as_deref()).is_some_and(|p| Path::new(p) == path)
    }

    /// Who is followed, for the control strip's "following fix-calc-sum": the agent by its name
    /// (from its task), its assistant only until it has one; and whether it is still running.
    pub(super) fn ai_follow_label(&self) -> Option<(String, bool)> {
        let key = self.ai_following()?;
        let c = self.ai.chats.iter().find(|c| c.key == key)?;
        let live = matches!(c.session, Session::Ready(_) | Session::Starting);
        let name = super::stage::agent_name(c).unwrap_or_else(|| c.agent.map_or("agent", nib_proto::AiAgent::name).to_string());
        Some((name, live))
    }

    /// Beats still to be shown (state JSON).
    pub(super) fn ai_follow_queued(&self) -> usize {
        self.ai.follow.as_ref().map_or(0, |f| f.queue.len() + usize::from(f.play.is_some()))
    }
}

impl Beat {
    /// It changes `path`.
    fn writes_to(&self, p: &str) -> bool {
        matches!(self, Beat::Edit { path, .. } | Beat::Write { path, .. } | Beat::Changed { path } if path == p)
    }
}

/// The green of what was just written.
const ADDED: Color = Color::from_rgb(0.30, 0.78, 0.42);

/// The byte offset of the occurrence of `s` in `text` nearest to byte `near`.
fn nearest(text: &str, s: &str, near: usize) -> Option<usize> {
    text.match_indices(s).map(|(b, _)| b).min_by_key(|b| b.abs_diff(near))
}
