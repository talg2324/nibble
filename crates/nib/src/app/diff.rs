//! nibdiff: what changed in the project, shown like a pull request. One tab (toolbar button,
//! `Command::ToggleDiff`): the changed files on the left, their hunks on the right, asked from
//! the daemon's `git diff` again every second while the tab is open, so it follows whoever is
//! editing (you, an agent, a `git pull`). Keys: ↑↓ / j k rows, n p hunks, ] [ files, ←→ fold,
//! s scope, Space stages or unstages the file, m commits, Enter opens the line in the editor.
//!
//! An agent's work is reviewed here: comments on lines of its changes and on blocks of its plan
//! (the pane beside them; Tab gives it the keys), all in one review that nibd keeps (D-070).

use super::comment::{Comment, Draft, Reply};
use super::plan::{self, Plan, PlanMsg};
use super::{async_sleep, App, Focus, Message, Pending};
use crate::diff_view::{blend, status_word, DiffEvent, DiffView, Paint, Row, Talk};
use helix_core::Rope;
use iced::advanced::widget::{operate, operation, Id as WidgetId};
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Alignment, Border, Color, Element, Font, Length, Padding, Task};
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::{Command, Document};
use nib_proto::{AiId, AiWork, DiffFile, DiffLineKind, DiffScope, DiffSet, DiffStatus, GitOp, Pos, Request, Response, Review, ReviewComment, ReviewOp, RpcError};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

/// How often the diff is asked again while its tab is in front.
const POLL: Duration = Duration::from_secs(1);
/// An unanswered ask older than this is given up on (the connection was replaced).
const ASK_TIMEOUT: Duration = Duration::from_secs(15);
const LIST_WIDTH: f32 = 280.0;
const PLAN_ID: &str = "diff-plan";
/// Syntax colours are kept until this many diffs in a row have not used them: a walk through
/// the scopes and back finds them still there.
const PAINT_KEPT: u64 = 12;
/// Files with more diff lines than this are shown without syntax colours.
const PAINT_LINES: usize = 4_000;

/// Your own changes, as git has them; after these come everybody's work and each agent's.
const SCOPES: [DiffScope; 3] = [DiffScope::Head, DiffScope::Unstaged, DiffScope::Staged];

/// As `nib ctl` names a scope.
fn scope_name(s: DiffScope) -> String {
    match s {
        DiffScope::Head => "head".into(),
        DiffScope::Unstaged => "unstaged".into(),
        DiffScope::Staged => "staged".into(),
        DiffScope::Everyone => "everyone".into(),
        DiffScope::Agent(id) => format!("agent:{id}"),
        DiffScope::Since(id) => format!("since:{id}"),
    }
}

/// The agent whose work `s` shows.
fn agent_of(s: DiffScope) -> Option<AiId> {
    match s {
        DiffScope::Agent(id) | DiffScope::Since(id) => Some(id),
        _ => None,
    }
}

/// A comment that says `body` on `target`, as `nib ctl diff comment --on` names it: `PATH`,
/// `PATH:LINE` or `PATH:START-END` (lines of the file as it is now), `plan:LINE` or
/// `plan:START-END` (lines of the agent's plan; a file called `plan` is `./plan`); none: on the
/// work as a whole. The daemon takes the lines' text as the anchor.
fn comment_on(target: Option<&str>, body: String) -> Result<ReviewComment, String> {
    let mut c = ReviewComment { body, ..Default::default() };
    let Some(t) = target else { return Ok(c) };
    let (path, lines) = match t.rsplit_once(':') {
        Some((p, l)) if !l.is_empty() && l.chars().all(|ch| ch.is_ascii_digit() || ch == '-') => (p, Some(l)),
        _ => (t, None),
    };
    c.path = Some(path.to_string());
    if let Some(l) = lines {
        let (a, b) = l.split_once('-').unwrap_or((l, l));
        (c.start_line, c.end_line) = (a.parse().map_err(|_| "bad line")?, b.parse().map_err(|_| "bad line")?);
        if c.start_line == 0 || c.end_line < c.start_line {
            return Err("lines are 1-based, START-END".to_string());
        }
        if path == "plan" {
            (c.path, c.plan) = (None, true);
        }
    }
    Ok(c)
}

/// What a comment being written is on.
#[derive(Debug, Clone, PartialEq)]
enum On {
    /// Lines of a file, on the side that is there now or (`old`) on removed ones.
    Lines { path: String, old: bool, anchor: String },
    File { path: String },
    /// The work as a whole.
    Whole,
    /// An answer to comment `to`.
    Reply { to: String },
}

/// A comment (or reply) being typed in the diff.
struct Writing {
    on: On,
    /// Its lines (for `On::Lines`) and its text.
    draft: Draft,
    /// The row the cursor was on: where it goes back to.
    back: usize,
}

/// `text` in rows of at most `width` columns, broken between words.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(12);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let mut row = String::new();
        for word in line.split(' ') {
            let (have, more) = (row.chars().count(), word.chars().count());
            if have > 0 && have + 1 + more > width {
                out.push(std::mem::take(&mut row));
            }
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(word);
            // A word longer than a row is cut.
            while row.chars().count() > width {
                let cut = row.char_indices().nth(width).map_or(row.len(), |(i, _)| i);
                let rest = row.split_off(cut);
                out.push(std::mem::replace(&mut row, rest));
            }
        }
        out.push(row);
    }
    out
}

/// An agent's plan, as the diff tab shows it beside the agent's changes.
struct Planned {
    id: AiId,
    /// `AiWork::plan` when it was read.
    stamp: u64,
    /// Its rows, what is picked and the comment being written. The filed comments it shows are
    /// the review's (`View::plan_sync`): nibd keeps them, so they outlive this.
    plan: Plan,
    /// The comment of the review that the one being written answers.
    answering: Option<String>,
}

#[derive(Default)]
pub struct View {
    scope: DiffScope,
    files: Vec<DiffFile>,
    /// The stamp of `files`, as the daemon gave it.
    stamp: Option<u64>,
    /// An answer arrived, and whether the root is in a repository.
    loaded: bool,
    repo: bool,
    rows: Vec<Row>,
    /// Per row, the syntax colours of a line.
    paint: Vec<Vec<Paint>>,
    top: usize,
    cursor: usize,
    /// Rows that fit in the view.
    height: usize,
    /// Folded files, by path.
    closed: HashSet<String>,
    /// When the diff was last asked for, until the answer comes.
    asking: Option<Instant>,
    /// The poll loop that counts (an older one's ticks are dropped) and whether it runs.
    poll: u64,
    polling: bool,
    /// What each agent's `nib-work` last said: its branch, and whether it has a plan.
    work: HashMap<AiId, AiWork>,
    /// The plan of the agent whose work shows.
    plan: Option<Planned>,
    /// The plan being read: whose, and which version.
    plan_asked: Option<(AiId, u64)>,
    /// The agent whose branch is being landed.
    landing: Option<AiId>,
    /// The review of the work that shows (an agent's scope), and the `AiWork::review` it was
    /// read at.
    review: Review,
    review_stamp: Option<u64>,
    /// The comment being typed.
    writing: Option<Writing>,
    /// Where ⇧↑ / ⇧↓ began: a comment takes the lines from there to the cursor.
    mark: Option<usize>,
    /// Columns that fit in the view.
    cols: usize,
    /// The syntax colours of files' diff lines, kept to be used again: colouring parses the
    /// code, and the rows are rebuilt for much less than a changed file (a fold, a comment, a
    /// wider window, another file's change, going back to a scope seen a moment ago). By a
    /// hash of the file's path and diff lines, with the diff (`painted_at`) that last used them.
    painted: HashMap<u64, (u64, Vec<Vec<Paint>>)>,
    /// Counts the diffs shown, for dropping colours nothing has used for a while.
    painted_at: u64,
    /// The two groups of scopes in the header, YOURS and AGENTS: shown whole or folded to
    /// their name. `None`: as it comes (whole when what shows is one of its scopes).
    folds: [Option<bool>; 2],
    /// The plan pane is put away (`P`).
    plan_hidden: bool,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Event(DiffEvent),
    Scope(DiffScope),
    /// A file in the list was clicked.
    File(usize),
    /// Time to ask again, for poll loop `.0`.
    Poll(u64),
    /// From the plan pane.
    Plan(PlanMsg),
    /// A group of scopes was clicked by its name (0: YOURS, 1: AGENTS): fold or unfold it.
    Fold(usize),
}

impl App {
    /// The diff tab's place among the tabs.
    fn diff_tab(&self) -> Option<usize> {
        self.tabs.iter().position(|t| t.diff)
    }

    /// The diff tab is the one showing.
    pub(super) fn diff_active(&self) -> bool {
        !self.ai.shown() && self.tab().is_some_and(|t| t.diff)
    }

    /// Show the diff tab; with it already in front, close it.
    pub(super) fn diff_toggle(&mut self) -> Task<Message> {
        match self.diff_tab() {
            Some(_) if self.diff_active() => return self.close_tab(),
            Some(i) => {
                self.active = i;
                self.ai.front = false;
                self.focus = Focus::Main;
            }
            None => {
                let mut doc = Document::new_plain(Default::default(), None);
                doc.read_only = true;
                self.put_doc(doc, true);
                self.tabs[self.active].diff = true;
                self.ai.front = false;
            }
        }
        self.diff_wake()
    }

    /// Ask now, and keep asking: called when the tab is shown and, as a safety net, with every
    /// git poll (a loop that died with a connection starts again).
    pub(super) fn diff_wake(&mut self) -> Task<Message> {
        if self.diff_tab().is_none() {
            return Task::none();
        }
        let ask = self.diff_ask();
        if self.diff.polling {
            return ask;
        }
        self.diff.polling = true;
        self.diff.poll += 1;
        let id = self.diff.poll;
        Task::batch([ask, Task::perform(async_sleep(POLL), move |_| Message::Diff(Msg::Poll(id)))])
    }

    /// The scopes on offer and what each is called: yours, then everybody's, then one for each
    /// agent at work.
    fn diff_scopes(&self) -> Vec<(DiffScope, String)> {
        let mut all = vec![
            (DiffScope::Head, "All changes".to_string()),
            (DiffScope::Unstaged, "Unstaged".to_string()),
            (DiffScope::Staged, "Staged".to_string()),
        ];
        let agents = self.ai_agents();
        if !agents.is_empty() {
            all.push((DiffScope::Everyone, "All agents".to_string()));
        }
        all.extend(agents.into_iter().map(|(id, name)| (DiffScope::Agent(id), name)));
        // An agent that has gone, while its work still shows.
        if let Some(id) = agent_of(self.diff.scope) {
            if !all.iter().any(|(s, _)| *s == DiffScope::Agent(id)) {
                all.push((DiffScope::Agent(id), format!("agent {id} (ended)")));
            }
        }
        all
    }

    /// A scope by its `nib ctl` name: `head`, `unstaged`, `staged`, `everyone`, `agent:N`, or an
    /// agent's name.
    pub(super) fn diff_scope_named(&self, name: &str) -> Option<DiffScope> {
        let found = self.diff_scopes().into_iter().find(|(s, label)| scope_name(*s) == name || (matches!(s, DiffScope::Agent(_)) && label == name)).map(|(s, _)| s);
        // `since:N`: what agent N changed since its last review.
        found.or_else(|| {
            let id: AiId = name.strip_prefix("since:")?.parse().ok()?;
            self.diff_scopes().iter().any(|(s, _)| *s == DiffScope::Agent(id)).then_some(DiffScope::Since(id))
        })
    }

    /// `nib ctl diff comment|reply|resolve|send`: the review of the agent whose work shows.
    /// `Err`: why not.
    pub(super) fn diff_review_ctl(&mut self, what: &str, target: Option<&str>, body: Option<&str>) -> Result<Task<Message>, String> {
        if agent_of(self.diff.scope).is_none() || !self.diff_active() {
            return Err("the diff tab is not showing an agent's work (diff open agent:N)".to_string());
        }
        let body = || body.map(str::to_string).filter(|b| !b.trim().is_empty()).ok_or("no text given".to_string());
        let id = || target.map(str::to_string).ok_or("which comment?".to_string());
        let op = match what {
            "send" => ReviewOp::Send,
            "reply" => ReviewOp::Reply { to: id()?, body: body()? },
            "resolve" => ReviewOp::Resolve { comment: id()?, resolved: true },
            "reopen" => ReviewOp::Resolve { comment: id()?, resolved: false },
            "remove" => ReviewOp::Remove { comment: id()? },
            "comment" => ReviewOp::Comment(comment_on(target, body()?)?),
            other => return Err(format!("unknown review command {other:?}")),
        };
        Ok(self.diff_review_edit(op))
    }

    /// Group `g` of the header's scopes (0: YOURS, 1: AGENTS) is shown whole.
    fn diff_group_open(&self, g: usize) -> bool {
        let mine = SCOPES.contains(&self.diff.scope);
        self.diff.folds.get(g).copied().flatten().unwrap_or((g == 0) == mine)
    }

    /// Show the diff tab on `scope` (the finish dialog's "see its work", `nib ctl diff open`).
    pub(super) fn diff_show(&mut self, scope: DiffScope) -> Task<Message> {
        // Asked for by what it shows (an agent's work, from its chat): only that group is open.
        self.diff.folds = [None, None];
        let open = if self.diff_active() { Task::none() } else { self.diff_toggle() };
        Task::batch([open, self.diff_scope(scope)])
    }

    /// Model selection replaces the process, while its work and open review stay put.
    pub(super) fn diff_replace_agent(&mut self, old: AiId, new: AiId) {
        if let Some(work) = self.diff.work.remove(&old) { self.diff.work.insert(new, work); }
        self.diff.scope = match self.diff.scope {
            DiffScope::Agent(id) if id == old => DiffScope::Agent(new),
            DiffScope::Since(id) if id == old => DiffScope::Since(new),
            scope => scope,
        };
        if let Some(plan) = self.diff.plan.as_mut().filter(|p| p.id == old) { plan.id = new; }
        if self.diff.plan_asked.is_some_and(|(id, _)| id == old) { self.diff.plan_asked = None; }
    }

    /// An agent's `nib-work` state changed (`AiEvent::Work`).
    pub(super) fn diff_work(&mut self, id: AiId, work: AiWork) -> Task<Message> {
        // Its review changed (it answered a comment): read it again.
        let review = agent_of(self.diff.scope) == Some(id) && work.review != self.diff.review_stamp;
        self.diff.work.insert(id, work);
        let review = if review { self.diff_review_ask() } else { Task::none() };
        Task::batch([self.diff_plan_ask(), review])
    }

    /// Read the review of the agent whose work shows.
    fn diff_review_ask(&mut self) -> Task<Message> {
        let Some(id) = agent_of(self.diff.scope) else { return Task::none() };
        if self.diff_tab().is_none() || self.disconnected || !self.connected {
            return Task::none();
        }
        self.diff.review_stamp = self.diff.work.get(&id).and_then(|w| w.review);
        self.request(Request::Review { id }, Pending::DiffReview { id })
    }

    fn diff_review_edit(&mut self, op: ReviewOp) -> Task<Message> {
        let Some(id) = agent_of(self.diff.scope) else { return self.flash_info("comments are on an agent's work: pick an agent (a)") };
        self.request(Request::ReviewEdit { id, op }, Pending::DiffReview { id })
    }

    pub(super) fn on_diff_review(&mut self, id: AiId, result: Result<Response, RpcError>) -> Task<Message> {
        if agent_of(self.diff.scope) != Some(id) {
            return Task::none();
        }
        let (review, message) = match result {
            Ok(Response::Review(r)) => (r, None),
            Ok(Response::ReviewSent { review, message }) => (review, Some(message)),
            Ok(other) => return self.flash_warn(format!("unexpected response: {other:?}")),
            Err(e) => return self.flash_warn(e.msg),
        };
        let ui = &mut self.diff;
        let at = ui.place();
        ui.review = review;
        ui.plan_sync();
        ui.rebuild();
        ui.restore(at);
        let Some(message) = message else { return Task::none() };
        // Sent: said to the agent in its chat, which goes on from there.
        let round = self.diff.review.round;
        self.ai_tell(id, message);
        self.flash_info(format!("review sent: round {round}"))
    }

    /// `c`: write a comment on what the cursor is on (the lines from the mark to it, a file, or
    /// an answer to the comment it is in). `whole`: on the work as a whole.
    fn diff_comment(&mut self, whole: bool) -> Task<Message> {
        if agent_of(self.diff.scope).is_none() {
            return self.flash_info("comments are on an agent's work: pick an agent (a)");
        }
        let ui = &mut self.diff;
        let on = if whole { Some((On::Whole, 0, 0)) } else { ui.target() };
        let Some((on, start, end)) = on else { return self.flash_info("nothing to comment on here") };
        ui.writing = Some(Writing { on, draft: Draft::new(start, end), back: ui.cursor });
        ui.mark = None;
        ui.rebuild();
        ui.show_draft();
        Task::none()
    }

    /// A key while a comment is being typed.
    fn diff_writing_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let held = chord.ctrl || chord.logo;
        let Some(w) = &mut self.diff.writing else { return Task::none() };
        match chord.key {
            Key::Esc => {
                let back = w.back;
                self.diff.writing = None;
                self.diff.rebuild();
                self.diff.cursor = back.min(self.diff.rows.len().saturating_sub(1));
                return Task::none();
            }
            Key::Enter if chord.shift => w.draft.insert("\n"),
            Key::Enter => {
                if w.draft.blank() {
                    return self.flash_info("type the comment first (esc: cancel)");
                }
                let w = self.diff.writing.take().expect("checked above");
                let (start_line, end_line, body) = (w.draft.start_line, w.draft.end_line, w.draft.body.clone());
                let op = match w.on.clone() {
                    On::Reply { to } => ReviewOp::Reply { to, body },
                    On::Whole => ReviewOp::Comment(ReviewComment { body, ..Default::default() }),
                    On::File { path } => ReviewOp::Comment(ReviewComment { path: Some(path), body, ..Default::default() }),
                    On::Lines { path, old, anchor } => ReviewOp::Comment(ReviewComment { path: Some(path), old, start_line, end_line, anchor, body, ..Default::default() }),
                };
                self.diff.rebuild();
                self.diff.cursor = w.back.min(self.diff.rows.len().saturating_sub(1));
                return self.diff_review_edit(op);
            }
            Key::Backspace if held => w.draft.backspace_line(),
            Key::Backspace if chord.alt => w.draft.backspace_word(),
            Key::Backspace => w.draft.backspace(),
            _ if !held => {
                let typed: String = text.unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
                w.draft.insert(&typed);
            }
            _ => {}
        }
        let ui = &mut self.diff;
        ui.rebuild();
        ui.show_draft();
        Task::none()
    }

    /// The comment the cursor is in.
    fn diff_comment_here(&self) -> Option<&ReviewComment> {
        let Some(Row::Talk { comment: Some(id), .. }) = self.diff.rows.get(self.diff.cursor) else { return None };
        self.diff.review.comments.iter().find(|c| c.id == *id)
    }

    /// The branch agent `id` works on and the one it lands on, if it started one.
    pub(super) fn diff_branch(&self, id: AiId) -> Option<(String, String)> {
        let w = self.diff.work.get(&id)?;
        Some((w.branch.clone()?, w.base.clone().unwrap_or_default()))
    }

    /// Land agent `id`'s branch on its base as one commit that says what `f` says; the agent is
    /// ended when that worked (`on_diff_landed`).
    pub(super) fn diff_land(&mut self, id: AiId, f: &nib_proto::AiFinished) -> Task<Message> {
        if self.diff.landing.is_some() {
            return self.flash_info("a branch is being landed: one at a time");
        }
        self.diff.landing = Some(id);
        // The subject: what it did, on one line; the summary whole below it.
        let did = f.did.split_whitespace().collect::<Vec<_>>().join(" ");
        let subject: String = match did.char_indices().nth(72) {
            Some((cut, _)) => format!("{}…", did[..cut].trim_end()),
            None => did,
        };
        let branch = self.diff_branch(id).map(|(b, _)| format!("\n\nBranch: {b}")).unwrap_or_default();
        let message = format!("{subject}\n\n{}{branch}", f.text());
        self.request(Request::WorkLand { id, message }, Pending::WorkLand { id })
    }

    pub(super) fn on_diff_landed(&mut self, id: AiId, result: Result<Response, RpcError>) -> Task<Message> {
        self.diff.landing = None;
        let (ok, output) = match result {
            Ok(Response::GitRan { ok, output }) => (ok, output),
            Ok(other) => (false, format!("unexpected response: {other:?}")),
            Err(e) => (false, e.to_string()),
        };
        if !ok {
            // Nothing was changed. The agent is told, in its chat, and goes on from there.
            let base = self.diff_branch(id).map_or("the base branch".to_string(), |(_, b)| b);
            self.ai_tell(
                id,
                format!(
                    "(nib: your branch could not be landed on {base}; nothing was changed. git said:\n{output}\n\
                     If {base} moved on or there is a conflict: run `nib-work sync`, resolve what it reports, check your work again, then run `nib-mail done` again. \
                     If it is something only the user can fix (the project is on another branch, something is staged there), say so and wait.)"
                ),
            );
            self.ai_show_session(id);
            return self.flash_warn(format!("not landed: {}", output.lines().next().unwrap_or("git failed")));
        }
        self.diff.work.remove(&id);
        if agent_of(self.diff.scope) == Some(id) {
            // Its work is the project's now: what shows is what is left to commit.
            let _ = self.diff_scope(DiffScope::Head);
        }
        let flash = self.flash_info(output.lines().last().unwrap_or("landed").to_string());
        self.diff.asking = None;
        Task::batch([flash, self.ai_end_agent(id), self.reload_clean_tabs(), self.tree_refresh(), self.diff_ask()])
    }

    /// ⌘↵ in the diff tab on the work of an agent that said it is done: accept it from here.
    fn diff_accept(&mut self) -> Task<Message> {
        let Some(id) = agent_of(self.diff.scope) else { return Task::none() };
        if !self.ai_said_done(id) {
            return self.flash_info("this agent has not said its job is done");
        }
        if !self.ai_show_session(id) {
            return Task::none();
        }
        self.ai_accept(true)
    }

    /// Read the plan of the agent whose work shows, if it has one that isn't the one loaded.
    fn diff_plan_ask(&mut self) -> Task<Message> {
        let Some(id) = agent_of(self.diff.scope) else {
            self.diff.plan = None;
            return Task::none();
        };
        let stamp = self.diff.work.get(&id).and_then(|w| w.plan);
        if self.diff.plan.as_ref().is_some_and(|p| p.id != id || stamp.is_none()) {
            self.diff.plan = None;
        }
        match stamp {
            Some(stamp) if !self.diff.plan.as_ref().is_some_and(|p| p.stamp == stamp) && self.diff.plan_asked != Some((id, stamp)) && self.diff_tab().is_some() && !self.disconnected => {
                self.diff.plan_asked = Some((id, stamp));
                self.request(Request::WorkPlan { id }, Pending::DiffPlan { id, stamp })
            }
            _ => Task::none(),
        }
    }

    pub(super) fn on_diff_plan(&mut self, id: AiId, stamp: u64, result: Result<Response, RpcError>) -> Task<Message> {
        if self.diff.plan_asked == Some((id, stamp)) {
            self.diff.plan_asked = None;
        }
        if agent_of(self.diff.scope) != Some(id) {
            return Task::none();
        }
        let Ok(Response::WorkPlan(Some(md))) = result else { return Task::none() };
        let ui = &mut self.diff;
        match &mut ui.plan {
            // Recorded again as it was: what is picked and being written there stays.
            Some(p) if p.id == id && p.plan.text == md => p.stamp = stamp,
            _ => ui.plan = Some(Planned { id, stamp, plan: Plan::new(&md), answering: None }),
        }
        ui.plan_sync();
        let at = ui.place();
        ui.rebuild();
        ui.restore(at);
        // Another plan: the comments on it are where their lines went (nibd finds them).
        if self.diff.review.comments.iter().any(|c| c.plan) {
            return self.diff_review_ask();
        }
        Task::none()
    }

    /// From the plan pane: a click on a row, on a draft's buttons, on a comment's ✕.
    fn diff_plan_msg(&mut self, m: PlanMsg) -> Task<Message> {
        // A comment being typed in the changes has the keys.
        if self.diff.writing.is_some() {
            return Task::none();
        }
        let Some(p) = &mut self.diff.plan else { return Task::none() };
        match m {
            PlanMsg::Save => return self.diff_plan_save(),
            PlanMsg::Cancel => self.diff.plan_cancel(),
            // A click picks a row; on the row picked, it opens a comment. One being written is
            // filed first.
            PlanMsg::Row(i) => {
                self.focus = Focus::Main;
                let again = p.plan.comments.draft.is_none() && p.plan.sel == Some((i, i));
                let filed = self.diff_plan_save();
                if let Some(p) = &mut self.diff.plan {
                    if !again || !p.plan.write() {
                        p.plan.sel = None;
                        p.plan.update(PlanMsg::Row(i));
                    }
                }
                return filed;
            }
            PlanMsg::Remove(comment) => {
                // Sent, it is part of what the agent was told: resolve it instead.
                if self.diff.review.comments.iter().any(|c| c.id == comment && c.round > 0) {
                    return self.flash_info("already sent: resolve it (x) instead of taking it back");
                }
                return self.diff_review_edit(ReviewOp::Remove { comment });
            }
            PlanMsg::Fold => {}
        }
        Task::none()
    }

    /// File the comment being written in the plan pane: into the agent's review, with the lines
    /// of the plan it is on (or as the answer it is). One with no words is dropped.
    fn diff_plan_save(&mut self) -> Task<Message> {
        let Some(p) = self.diff.plan.as_mut().filter(|p| p.plan.comments.draft.is_some()) else { return Task::none() };
        let (answering, taken) = (p.answering.take(), p.plan.take());
        // The keys stay in the pane, on the row it was on.
        p.plan.step(0, false);
        let Some((start_line, end_line, anchor, body)) = taken else { return Task::none() };
        self.diff_review_edit(match answering {
            Some(to) => ReviewOp::Reply { to, body },
            None => ReviewOp::Comment(ReviewComment { plan: true, start_line, end_line, anchor, body, ..Default::default() }),
        })
    }

    /// A key for the plan pane, which has the keys while one of its rows is picked or a comment
    /// is being written in it. Tab picks a row, or lets go; then, as where a plan is approved
    /// (D-068): ↑ / ↓ move, ⇧ takes in more rows, ↵ (or c) writes a comment on them; and as on
    /// a comment in the changes: r answers the one under the rows picked, x resolves it or
    /// opens it again, ⌫ takes back one not sent yet. `None`: the key isn't the pane's.
    fn diff_plan_key(&mut self, chord: KeyChord, text: Option<&str>) -> Option<Task<Message>> {
        let held = chord.ctrl || chord.logo;
        let ui = &mut self.diff;
        if agent_of(ui.scope).is_none() || !ui.plan_shown() {
            return None;
        }
        let p = ui.plan.as_mut()?;
        let by = match chord.key {
            Key::Up => Some(-1),
            Key::Down => Some(1),
            _ => None,
        };
        // A comment being typed takes every key.
        if let Some(d) = p.plan.comments.draft.as_mut() {
            match chord.key {
                Key::Esc => ui.plan_cancel(),
                Key::Enter if chord.shift => d.insert("\n"),
                Key::Enter => return Some(self.diff_plan_save()),
                Key::Backspace if held => d.backspace_line(),
                Key::Backspace if chord.alt => d.backspace_word(),
                Key::Backspace => d.backspace(),
                _ if by.is_some() && chord.shift && !held && !chord.alt && p.answering.is_none() => p.plan.step(by.unwrap(), true),
                _ if !held => d.insert(&text.unwrap_or_default().chars().filter(|c| !c.is_control()).collect::<String>()),
                _ => {}
            }
            return Some(self.diff_plan_reveal());
        }
        if held || chord.alt {
            return None;
        }
        if chord.key == Key::Tab {
            if p.plan.sel.take().is_none() {
                p.plan.step(0, false);
            }
            return Some(self.diff_plan_reveal());
        }
        let (start, end) = p.plan.picked()?;
        // The comment under the rows picked: the last one still open, or the last.
        let under = || p.plan.comments.list().iter().rev().filter(|c| start <= c.end_line && c.end_line <= end);
        let here = under().find(|c| !c.resolved).or(under().next()).map(|c| (c.id.clone(), c.start_line, c.end_line, c.resolved));
        match (chord.key, here) {
            (Key::Up | Key::Char('k'), _) => p.plan.step(-1, chord.shift),
            (Key::Down | Key::Char('j'), _) => p.plan.step(1, chord.shift),
            (Key::Esc, _) => p.plan.sel = None,
            (Key::Enter | Key::Char('c'), _) if !chord.shift => {
                p.plan.write();
            }
            (Key::Char('r'), Some((id, start, end, _))) => {
                p.plan.comments.open(start, end);
                (p.answering, p.plan.sel) = (Some(id), None);
            }
            (Key::Char('x'), Some((comment, _, _, resolved))) => return Some(self.diff_review_edit(ReviewOp::Resolve { comment, resolved: !resolved })),
            (Key::Backspace, Some((comment, ..))) => return Some(self.diff_plan_msg(PlanMsg::Remove(comment))),
            _ => return None,
        }
        Some(self.diff_plan_reveal())
    }

    /// Scroll the plan pane to about where the rows picked (or the comment being written) are:
    /// their share of the plan's lines decides how far down (rows are drawn at different
    /// heights, so it's a guess).
    fn diff_plan_reveal(&self) -> Task<Message> {
        let Some(p) = &self.diff.plan else { return Task::none() };
        let line = p.plan.comments.draft.as_ref().map(|d| d.end_line).or(p.plan.picked().map(|(_, end)| end));
        let (Some(line), Some(first), Some(last)) = (line, p.plan.rows.first().map(|r| r.start_line), p.plan.rows.last().map(|r| r.end_line)) else { return Task::none() };
        let y = if last > first { (line.saturating_sub(first) as f32 / (last - first) as f32).clamp(0.0, 1.0) } else { 0.0 };
        operate(operation::scrollable::snap_to(WidgetId::from(PLAN_ID), operation::scrollable::RelativeOffset { x: None, y: Some(y) }))
    }

    fn diff_ask(&mut self) -> Task<Message> {
        if self.disconnected || !self.connected || self.diff.asking.is_some_and(|t| t.elapsed() < ASK_TIMEOUT) {
            return Task::none();
        }
        self.diff.asking = Some(Instant::now());
        let (scope, epoch) = (self.diff.scope, self.epoch);
        self.request(Request::Diff { scope, known: self.diff.stamp }, Pending::Diff { scope, epoch })
    }

    pub(super) fn on_diff(&mut self, scope: DiffScope, epoch: u64, result: Result<Response, RpcError>) -> Task<Message> {
        self.diff.asking = None;
        if scope != self.diff.scope || epoch != self.epoch {
            // About another scope or another root: ask about this one.
            return self.diff_ask();
        }
        let Ok(Response::Diff(DiffSet { stamp, files, repo })) = result else { return Task::none() };
        let ui = &mut self.diff;
        (ui.loaded, ui.repo) = (true, repo);
        if !repo {
            (ui.files, ui.stamp) = (Vec::new(), None);
            ui.rebuild();
            return Task::none();
        }
        if let Some(files) = files {
            let at = ui.place();
            (ui.files, ui.stamp) = (files, Some(stamp));
            ui.mark = None;
            ui.rebuild();
            ui.restore(at);
            // The code moved: so may the lines the comments are on.
            if agent_of(scope).is_some() && !self.diff.review.comments.is_empty() {
                return self.diff_review_ask();
            }
        }
        Task::none()
    }

    pub(super) fn on_diff_msg(&mut self, m: Msg) -> Task<Message> {
        match m {
            Msg::Poll(id) if id != self.diff.poll => {}
            Msg::Poll(_) if self.diff_tab().is_none() => self.diff.polling = false,
            Msg::Poll(id) => {
                // Not in front: nobody is looking, so git is left alone.
                let ask = if self.diff_active() { self.diff_ask() } else { Task::none() };
                return Task::batch([ask, Task::perform(async_sleep(POLL), move |_| Message::Diff(Msg::Poll(id)))]);
            }
            Msg::Scope(s) => return self.diff_scope(s),
            Msg::Fold(g) => {
                let open = self.diff_group_open(g);
                if let Some(f) = self.diff.folds.get_mut(g) {
                    *f = Some(!open);
                }
            }
            Msg::Plan(m) => return self.diff_plan_msg(m),
            Msg::File(i) => {
                self.focus = Focus::Main;
                let ui = &mut self.diff;
                ui.plan_let_go();
                if let Some(r) = ui.rows.iter().position(|r| matches!(r, Row::File { file, .. } if *file == i)) {
                    ui.cursor = r;
                    ui.top = r.min(ui.max_top());
                }
            }
            Msg::Event(DiffEvent::Resized { rows, cols }) => {
                if self.diff.cols != cols {
                    // Comments are wrapped to the width.
                    self.diff.cols = cols;
                    let at = self.diff.place();
                    self.diff.rebuild();
                    self.diff.restore(at);
                }
                self.diff.height = rows;
                self.diff.top = self.diff.top.min(self.diff.max_top());
            }
            Msg::Event(DiffEvent::Scroll { rows }) => {
                let ui = &mut self.diff;
                ui.top = ui.top.saturating_add_signed(rows.round() as isize).min(ui.max_top());
            }
            Msg::Event(DiffEvent::Click { row }) => {
                self.focus = Focus::Main;
                self.diff.plan_let_go();
                self.diff.cursor = row.min(self.diff.rows.len().saturating_sub(1));
                if matches!(self.diff.rows.get(row), Some(Row::File { .. })) {
                    self.diff.fold(None);
                }
            }
        }
        Task::none()
    }

    fn diff_scope(&mut self, s: DiffScope) -> Task<Message> {
        if s == self.diff.scope {
            return Task::none();
        }
        let ui = &mut self.diff;
        let was = ui.scope;
        (ui.scope, ui.stamp, ui.loaded, ui.asking) = (s, None, false, None);
        (ui.top, ui.cursor) = (0, 0);
        ui.files.clear();
        // The same agent's work, seen another way, keeps its review.
        if agent_of(s) != agent_of(was) || agent_of(s).is_none() {
            (ui.review, ui.review_stamp, ui.writing) = (Review::default(), None, None);
        }
        ui.mark = None;
        ui.rebuild();
        let review = if agent_of(s).is_some() && agent_of(s) != agent_of(was) { self.diff_review_ask() } else { Task::none() };
        Task::batch([self.diff_ask(), self.diff_plan_ask(), review])
    }

    /// To the next scope in `among` after the one showing (the first, from anywhere else).
    fn diff_scope_next(&mut self, among: impl Fn(DiffScope) -> bool) -> Task<Message> {
        let all: Vec<DiffScope> = self.diff_scopes().into_iter().map(|(s, _)| s).filter(|s| among(*s)).collect();
        let Some(first) = all.first().copied() else { return self.flash_info("no agents are at work") };
        let next = all.iter().position(|s| *s == self.diff.scope).map_or(first, |i| all[(i + 1) % all.len()]);
        self.diff_scope(next)
    }

    /// A key in the diff tab. `None`: not one of its own; the keymap decides.
    pub(super) fn diff_key(&mut self, chord: KeyChord, text: Option<String>) -> Option<Task<Message>> {
        // A comment being typed takes every key.
        if self.diff.writing.is_some() {
            return Some(self.diff_writing_key(chord, text));
        }
        if let Some(t) = self.diff_plan_key(chord, text.as_deref()) {
            return Some(t);
        }
        let held = if cfg!(target_os = "macos") { chord.logo } else { chord.ctrl || chord.logo };
        if held && chord.key == Key::Enter && agent_of(self.diff.scope).is_some() {
            // ⌘⇧↵ sends the review; ⌘↵ accepts the work.
            return Some(if chord.shift { self.diff_review_edit(ReviewOp::Send) } else { self.diff_accept() });
        }
        if chord.ctrl || chord.logo || chord.alt {
            return None;
        }
        if let Some(id) = agent_of(self.diff.scope) {
            match chord.key {
                // Back to the agent whose work this is (first: drop the mark).
                Key::Esc if self.diff.mark.take().is_some() => return Some(Task::none()),
                Key::Esc => {
                    self.ai_show_session(id);
                    return Some(Task::none());
                }
                Key::Char('c') => return Some(self.diff_comment(chord.shift)),
                // What changed since the review was last sent, and back.
                Key::Char('t') => {
                    let to = match self.diff.scope {
                        DiffScope::Since(_) => DiffScope::Agent(id),
                        _ if self.diff.review.since => DiffScope::Since(id),
                        _ => return Some(self.flash_info("no review was sent to this agent on its own branch yet")),
                    };
                    return Some(self.diff_scope(to));
                }
                Key::Char('r' | 'x') | Key::Backspace => {
                    if let Some(c) = self.diff_comment_here() {
                        let (comment, resolved, sent) = (c.id.clone(), c.resolved, c.round > 0);
                        return Some(match chord.key {
                            Key::Char('r') => self.diff_comment(false),
                            Key::Char('x') => self.diff_review_edit(ReviewOp::Resolve { comment, resolved: !resolved }),
                            // Sent, it is part of what the agent was told: resolve it instead.
                            _ if sent => self.flash_info("already sent: resolve it (x) instead of taking it back"),
                            _ => self.diff_review_edit(ReviewOp::Remove { comment }),
                        });
                    }
                }
                _ => {}
            }
        }
        let ui = &mut self.diff;
        match chord.key {
            Key::Char('j') => ui.step(1),
            Key::Char('k') => ui.step(-1),
            Key::Char('n') => ui.jump(false, |r| matches!(r, Row::Hunk { .. })),
            Key::Char('p') if chord.shift => {
                // Put away, the comments on the plan show above the files instead.
                ui.plan_hidden = !ui.plan_hidden;
                let at = ui.place();
                ui.rebuild();
                ui.restore(at);
            }
            Key::Char('p') => ui.jump(true, |r| matches!(r, Row::Hunk { .. })),
            Key::Char(']') => ui.jump(false, |r| matches!(r, Row::File { .. })),
            Key::Char('[') => ui.jump(true, |r| matches!(r, Row::File { .. })),
            Key::Char('g') if chord.shift => ui.step(isize::MAX / 2),
            Key::Char('g') => ui.step(isize::MIN / 2),
            Key::Char('s') => return Some(self.diff_scope_next(|_| true)),
            Key::Char('a') => return Some(self.diff_scope_next(|s| matches!(s, DiffScope::Everyone | DiffScope::Agent(_)))),
            Key::Char(c @ '1'..='3') => return Some(self.diff_scope(SCOPES[c as usize - '1' as usize])),
            Key::Char('r') => {
                ui.asking = None;
                return Some(self.diff_ask());
            }
            Key::Char(' ') => return Some(self.diff_stage()),
            Key::Char('m') => return Some(self.git_commit_box()),
            Key::Char('o') => return Some(self.diff_open()),
            Key::Enter if matches!(ui.rows.get(ui.cursor), Some(Row::File { .. })) => ui.fold(None),
            Key::Enter => return Some(self.diff_open()),
            _ => return None,
        }
        Some(Task::none())
    }

    /// A command while the diff tab shows (the arrows and paging keys, as the keymap has them).
    pub(super) fn diff_command(&mut self, c: Command) -> Task<Message> {
        use Command::*;
        let ui = &mut self.diff;
        let page = ui.height.max(2) as isize - 1;
        match c {
            MoveUp => ui.step(-1),
            MoveDown => ui.step(1),
            SelectUp => ui.select(-1),
            SelectDown => ui.select(1),
            PageUp => ui.step(-page),
            PageDown => ui.step(page),
            MoveDocStart => ui.step(isize::MIN / 2),
            MoveDocEnd => ui.step(isize::MAX / 2),
            MoveLeft => ui.fold(Some(false)),
            MoveRight => ui.fold(Some(true)),
            _ => {}
        }
        Task::none()
    }

    /// Space: stage the file under the cursor (in the staged scope: take it out of the index).
    fn diff_stage(&mut self) -> Task<Message> {
        let ui = &self.diff;
        if agent_of(ui.scope).is_some() || ui.scope == DiffScope::Everyone {
            return self.flash_info("staging is for your own changes: scopes 1, 2 and 3");
        }
        let Some(f) = ui.rows.get(ui.cursor).and_then(Row::file).and_then(|i| ui.files.get(i)) else { return Task::none() };
        let paths: Vec<String> = f.old_path.iter().cloned().chain([f.path.clone()]).collect();
        let op = if ui.scope == DiffScope::Staged { GitOp::Unstage { paths } } else { GitOp::Stage { paths } };
        self.request(Request::GitRun { op }, Pending::DiffGit)
    }

    pub(super) fn on_diff_git(&mut self, result: Result<Response, RpcError>) -> Task<Message> {
        let failed = match result {
            Ok(Response::GitRan { ok: true, .. }) => None,
            Ok(Response::GitRan { output, .. }) => Some(output.lines().next().unwrap_or("git failed").to_string()),
            Ok(other) => Some(format!("unexpected response: {other:?}")),
            Err(e) => Some(e.to_string()),
        };
        let flash = failed.map_or(Task::none(), |f| self.flash_warn(f));
        self.diff.asking = None;
        Task::batch([flash, self.diff_ask(), self.tree_refresh()])
    }

    /// Open the file under the cursor in the editor, at the line the cursor is on.
    fn diff_open(&mut self) -> Task<Message> {
        let ui = &self.diff;
        let Some(row) = ui.rows.get(ui.cursor) else { return Task::none() };
        let Some(f) = row.file().and_then(|i| ui.files.get(i)) else { return Task::none() };
        if f.status == DiffStatus::Deleted {
            return self.flash_info(format!("{} is deleted", f.path));
        }
        let line = match row {
            Row::Line { hunk, line, new, .. } => new.or_else(|| {
                // A removed line: the first line after it that is still there.
                let h = f.hunks.get(*hunk)?;
                let after = h.lines[..*line].iter().filter(|l| l.kind != DiffLineKind::Del).count() as u32;
                Some(h.new_start + after)
            }),
            Row::Hunk { hunk, .. } => f.hunks.get(*hunk).map(|h| h.new_start),
            _ => None,
        };
        let goto = line.map(|l| Pos { line: l.saturating_sub(1), col: 0 });
        self.focus = Focus::Main;
        // A file of an agent's own work tree is opened there, not in the project.
        let path = match &f.dir {
            Some(dir) => nib_proto::path::join(dir, &f.path),
            None => f.path.clone(),
        };
        self.open_path_at(path, true, goto)
    }

    /// The diff tab, for `nib ctl state`. `null` while there is none.
    pub(super) fn diff_json(&self) -> Value {
        if self.diff_tab().is_none() {
            return Value::Null;
        }
        let ui = &self.diff;
        let at = ui.rows.get(ui.cursor).map(|r| {
            let path = r.file().and_then(|i| ui.files.get(i)).map(|f| f.path.as_str());
            match r {
                Row::File { .. } => json!({ "kind": "file", "path": path }),
                Row::Hunk { hunk, .. } => json!({ "kind": "hunk", "path": path, "hunk": hunk }),
                Row::Note { text, .. } => json!({ "kind": "note", "path": path, "text": text }),
                Row::Gap => json!({ "kind": "gap" }),
                Row::Talk { comment, text, .. } => json!({ "kind": "comment", "path": path, "comment": comment, "text": text }),
                Row::Line { file, hunk, line, old, new } => {
                    let l = ui.files.get(*file).and_then(|f| f.hunks.get(*hunk)).and_then(|h| h.lines.get(*line));
                    let sign = l.map(|l| match l.kind {
                        DiffLineKind::Add => "+",
                        DiffLineKind::Del => "-",
                        DiffLineKind::Context => " ",
                    });
                    json!({ "kind": "line", "path": path, "hunk": hunk, "old": old, "new": new, "sign": sign, "text": l.map(|l| l.text.as_str()) })
                }
            }
        });
        json!({
            "front": self.diff_active(),
            "scope": scope_name(ui.scope),
            "scopes": self.diff_scopes().into_iter().map(|(s, label)| json!({ "scope": scope_name(s), "label": label })).collect::<Vec<_>>(),
            "work": agent_of(ui.scope).and_then(|id| ui.work.get(&id)).map(|w| json!({ "branch": w.branch, "dir": w.dir, "base": w.base, "plan": w.plan.is_some() })),
            "review": agent_of(ui.scope).map(|_| json!({
                "round": ui.review.round,
                "since": ui.review.since,
                "comments": ui.review.comments.iter().map(|c| json!({
                    "id": c.id, "path": c.path, "old": c.old, "plan": c.plan, "start_line": c.start_line, "end_line": c.end_line, "anchor": c.anchor,
                    "body": c.body, "author": c.author, "round": c.round, "resolved": c.resolved, "drifted": c.drifted,
                    "replies": c.replies.iter().map(|r| json!({ "id": r.id, "body": r.body, "author": r.author })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })),
            "writing": ui.writing.as_ref().map(|w| json!({ "on": format!("{:?}", w.on), "draft": w.draft.json() })),
            "mark": ui.mark,
            "landing": ui.landing,
            "groups": { "yours": self.diff_group_open(0), "agents": self.diff_group_open(1) },
            // Its `comments` are the review's that are on the plan, on the rows they show under.
            "plan": ui.plan.as_ref().map(|p| {
                let mut v = p.plan.json();
                (v["text"], v["shown"], v["answering"]) = (json!(p.plan.text), json!(!ui.plan_hidden), json!(p.answering));
                v
            }),
            "loaded": ui.loaded,
            "repo": ui.repo,
            "adds": ui.files.iter().map(|f| f.adds).sum::<u32>(),
            "dels": ui.files.iter().map(|f| f.dels).sum::<u32>(),
            "files": ui.files.iter().map(|f| json!({
                "path": f.path,
                "old_path": f.old_path,
                "status": status_word(f.status),
                "adds": f.adds,
                "dels": f.dels,
                "binary": f.binary,
                "hunks": f.hunks.len(),
                "folded": ui.closed.contains(&f.path),
                "agents": f.agents,
                "dir": f.dir,
            })).collect::<Vec<_>>(),
            "rows": ui.rows.len(),
            "top": ui.top,
            "cursor": ui.cursor,
            "at": at,
        })
    }

    // -----------------------------------------------------------------------------------------
    // view
    // -----------------------------------------------------------------------------------------

    pub(super) fn diff_view(&self) -> Element<'_, Message> {
        let ui = &self.diff;
        let th = &self.theme;
        let (accent, dim, fg, sel_bg, divider) = (th.accent, th.dim, th.foreground, th.selection, th.divider);
        let focused = self.focus == Focus::Main;

        let agents = self.ai_agents();
        let agent_name = |id: &AiId| agents.iter().find(|(a, _)| a == id).map_or(format!("agent {id}"), |(_, n)| n.clone());
        // Two groups: what you changed yourself, and what the agents did.
        let chip = |s: DiffScope, label: String| -> Element<'_, Message> {
            let on = s == ui.scope || (agent_of(s).is_some() && agent_of(s) == agent_of(ui.scope));
            button(text(label).size(12).color(if on { fg } else { dim }))
                .on_press(Message::Diff(Msg::Scope(s)))
                .padding([2, 9])
                .style(move |_, st| button::Style {
                    background: (on || matches!(st, button::Status::Hovered)).then(|| sel_bg.into()),
                    border: Border { color: if on { accent } else { Color::TRANSPARENT }, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                })
                .into()
        };
        // A group: its name (click: fold / unfold), then its scopes; folded, only the one
        // that shows, if it is one of them.
        fn group<'a>(g: usize, name: &'static str, open: bool, chips: Vec<(bool, Element<'a, Message>)>, dim: Color, divider: Color) -> Element<'a, Message> {
            let head = button(text(format!("{} {name}", if open { "▾" } else { "▸" })).size(10).color(dim))
                .on_press(Message::Diff(Msg::Fold(g)))
                .padding([2, 4])
                .style(|_, _| button::Style::default());
            let mut r = row![head].spacing(2).align_y(Alignment::Center).padding([1, 4]);
            for (on, c) in chips {
                if open || on {
                    r = r.push(c);
                }
            }
            container(r).style(move |_| container::Style { border: Border { color: divider, width: 1.0, radius: 5.0.into() }, ..Default::default() }).into()
        }
        let scopes = self.diff_scopes();
        let short = |s: DiffScope, label: String| match s {
            DiffScope::Head | DiffScope::Everyone => "all".to_string(),
            DiffScope::Unstaged => "unstaged".to_string(),
            DiffScope::Staged => "staged".to_string(),
            _ => label,
        };
        let is_on = |s: DiffScope| s == ui.scope || (agent_of(s).is_some() && agent_of(s) == agent_of(ui.scope));
        let (yours, theirs_chips): (Vec<_>, Vec<_>) = scopes.into_iter().partition(|(s, _)| SCOPES.contains(s));
        let mut chips = row![group(0, "YOURS", self.diff_group_open(0), yours.into_iter().map(|(s, l)| (is_on(s), chip(s, short(s, l)))).collect(), dim, divider)]
            .spacing(10)
            .align_y(Alignment::Center);
        if !theirs_chips.is_empty() {
            chips = chips.push(group(1, "AGENTS", self.diff_group_open(1), theirs_chips.into_iter().map(|(s, l)| (is_on(s), chip(s, short(s, l)))).collect(), dim, divider));
        }
        let (adds, dels): (u32, u32) = ui.files.iter().fold((0, 0), |(a, d), f| (a + f.adds, d + f.dels));
        let files = match ui.files.len() {
            1 => "1 file".to_string(),
            n => format!("{n} files"),
        };
        let totals = row![
            text(files).size(12).color(dim),
            text(format!("+{adds}")).size(12).font(Font::MONOSPACE).color(th.diff_add),
            text(format!("−{dels}")).size(12).font(Font::MONOSPACE).color(th.diff_del),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        // What can be done here first (review an agent's work, or commit your own), then how
        // to move about.
        let (land_key, send_key) = if cfg!(target_os = "macos") { ("⌘↵", "⌘⇧↵") } else { ("ctrl+↵", "ctrl+⇧↵") };
        let mut acts = row![].spacing(12);
        if let Some(id) = agent_of(ui.scope) {
            let open = ui.review.comments.iter().filter(|c| !c.resolved).count();
            // The plan pane has the keys: what they do there.
            if ui.plan_shown() && ui.plan.as_ref().is_some_and(|p| p.plan.sel.is_some() || p.plan.comments.draft.is_some()) {
                acts = acts.push(self.hint("↵", "comment on this part of the plan")).push(self.hint("⇧↑↓", "take in more")).push(self.hint("tab", "back to the changes"));
            } else {
                acts = acts.push(self.hint("c", "comment on this line")).push(self.hint("⇧↑↓", "take in more lines")).push(self.hint("⇧C", "comment on it all"));
                if ui.plan_shown() {
                    acts = acts.push(self.hint("tab", "comment on the plan"));
                }
            }
            if !ui.review.comments.is_empty() {
                acts = acts.push(self.hint("r", "reply")).push(self.hint("x", "resolve"));
            }
            let send: Element<'_, Message> = match open {
                0 => self.hint(send_key, "send comments to the agent"),
                1 => self.hint(send_key, "send 1 comment to the agent"),
                _ => row![self.hint(send_key, "send"), text(format!("{open} comments to the agent")).size(11).color(dim)].spacing(4).align_y(Alignment::Center).into(),
            };
            acts = acts.push(send);
            if ui.review.since {
                acts = acts.push(self.hint("t", if matches!(ui.scope, DiffScope::Since(_)) { "all its changes" } else { "only what changed since the review" }));
            }
            // It said it is done: the answer can be given from here.
            if ui.landing == Some(id) {
                acts = acts.push(text("landing…").size(11).color(accent));
            } else if self.ai_said_done(id) {
                acts = acts.push(self.hint(land_key, if self.diff_branch(id).is_some() { "land it" } else { "accept" }));
            }
            acts = acts.push(self.hint("esc", "its chat"));
        } else if ui.scope == DiffScope::Everyone {
            acts = acts.push(text("To comment on an agent's work and send it back, pick the agent above (a).").size(11).color(dim));
        } else {
            let stage = if ui.scope == DiffScope::Staged { "unstage file" } else { "stage file" };
            acts = acts.push(self.hint("space", stage)).push(self.hint("m", "commit"));
            if !agents.is_empty() {
                let see = if cfg!(target_os = "macos") { "⌘D" } else { "ctrl+D" };
                acts = acts.push(text(format!("· to comment on an agent's work and send it back: pick it under AGENTS (a), or {see} in its chat")).size(11).color(dim));
            }
        }
        let mut moves = row![
            self.hint("↑↓", "line"),
            self.hint("n p", "next / previous change"),
            self.hint("] [", "file"),
            self.hint("←→", "fold"),
            self.hint("↵", "open in the editor"),
            self.hint("s", "next view"),
        ]
        .spacing(12);
        if ui.plan.is_some() {
            moves = moves.push(self.hint("⇧P", if ui.plan_hidden { "show plan" } else { "hide plan" }));
        }
        // Whose pull request this is: the agent's branch and where it lands.
        let branch: Element<'_, Message> = match agent_of(ui.scope) {
            Some(id) => {
                let what = match ui.work.get(&id) {
                    Some(AiWork { branch: Some(b), base, .. }) => format!("{b} → {}", base.as_deref().unwrap_or("?")),
                    _ => "in the shared work tree: the files it edited".to_string(),
                };
                let what = if matches!(ui.scope, DiffScope::Since(_)) { format!("{what} · since review round {}", ui.review.round) } else { what };
                text(what).size(12).font(Font::MONOSPACE).color(dim).into()
            }
            _ => space().into(),
        };
        // The keys on a line of their own, folding onto more when the tab is narrow.
        let header = column![
            row![chips, space::horizontal(), branch, totals].spacing(16).align_y(Alignment::Center),
            acts.align_y(Alignment::Center).wrap().vertical_spacing(4.0),
            moves.align_y(Alignment::Center).wrap().vertical_spacing(4.0),
        ]
        .spacing(6)
        .padding(Padding { left: 12.0, right: 12.0, top: 6.0, bottom: 6.0 });

        let message = |s: String, c: Color| -> Element<'_, Message> { container(text(s).size(12).color(c)).center(Length::Fill).into() };
        // The plan beside the work, like a pull request's description.
        let plan_pane = || -> Option<Element<'_, Message>> {
            let p = ui.plan.as_ref().filter(|_| !ui.plan_hidden)?;
            let look = plan::Look::of(th, self.face.font, self.face.size.min(14.0));
            // An agent's plan is commented on like its changes; the comments are the review's.
            let live = agent_of(ui.scope).is_some();
            let pane = column![
                container(text("PLAN").size(10).color(dim)).padding(Padding { left: 14.0, top: 8.0, bottom: 2.0, right: 0.0 }),
                scrollable(container(plan::view(&p.plan, &look, live, |m| Message::Diff(Msg::Plan(m)))).padding(Padding { left: 14.0, right: 16.0, top: 6.0, bottom: 12.0 }))
                    .id(WidgetId::from(PLAN_ID))
                    .style(crate::theme::scrollbar)
                    .height(Length::Fill),
            ];
            Some(container(pane).width(Length::FillPortion(2)).into())
        };
        let body: Element<'_, Message> = if !ui.loaded {
            message("Asking git…".into(), dim)
        } else if !ui.repo {
            message("This folder is not a git repository.".into(), dim)
        } else if ui.files.is_empty() {
            let nothing = message(
                match ui.scope {
                    DiffScope::Head => "Nothing changed since the last commit.",
                    DiffScope::Unstaged => "Nothing left to stage.",
                    DiffScope::Staged => "Nothing is staged.",
                    DiffScope::Agent(_) => "This agent has changed nothing yet.",
                    DiffScope::Since(_) => "Nothing changed since the review was sent.",
                    DiffScope::Everyone => "Nobody has changed anything since the last commit.",
                }
                .into(),
                dim,
            );
            // An agent that has planned but not started: its plan is all there is to read.
            match plan_pane() {
                Some(pane) => row![pane, self.vrule(), container(nothing).width(Length::FillPortion(3))].into(),
                None => nothing,
            }
        } else {
            let here = ui.rows.get(ui.cursor).and_then(Row::file);
            let list = column(ui.files.iter().enumerate().map(|(i, f)| {
                let on = here == Some(i);
                let (letter, color) = match f.status {
                    DiffStatus::Added => ("A", th.diff_add),
                    DiffStatus::Deleted => ("D", th.diff_del),
                    DiffStatus::Renamed => ("R", accent),
                    DiffStatus::Modified => ("M", th.warn),
                };
                let (dir, name) = match f.path.rsplit_once('/') {
                    Some((d, n)) => (format!("{d}/"), n),
                    None => (String::new(), f.path.as_str()),
                };
                let label = row![
                    text(letter).size(11).font(Font::MONOSPACE).color(color),
                    container(
                        row![text(name).size(12).color(fg).wrapping(text::Wrapping::None), text(dir).size(11).color(dim).wrapping(text::Wrapping::None)]
                            .spacing(6)
                            .align_y(Alignment::Center)
                    )
                    .width(Length::Fill)
                    .clip(true),
                    text(format!("+{}", f.adds)).size(11).font(Font::MONOSPACE).color(th.diff_add),
                    text(format!("−{}", f.dels)).size(11).font(Font::MONOSPACE).color(th.diff_del),
                ]
                .spacing(8)
                .align_y(Alignment::Center);
                // With everybody's work together: whose each file is.
                let label: Element<'_, Message> = if ui.scope == DiffScope::Everyone {
                    let who = match f.agents.as_slice() {
                        [] => "you (or nobody nib knows)".to_string(),
                        ids => ids.iter().map(&agent_name).collect::<Vec<_>>().join(", "),
                    };
                    column![label, text(who).size(10).color(accent).wrapping(text::Wrapping::None)].spacing(1).into()
                } else {
                    label.into()
                };
                button(label)
                    .on_press(Message::Diff(Msg::File(i)))
                    .padding([3, 10])
                    .width(Length::Fill)
                    .style(move |_, st| button::Style {
                        background: if on {
                            Some(blend(Color::BLACK, sel_bg, 0.6).into())
                        } else {
                            matches!(st, button::Status::Hovered).then(|| blend(Color::BLACK, sel_bg, 0.3).into())
                        },
                        ..Default::default()
                    })
                    .into()
            }));
            let list = scrollable(list).style(crate::theme::scrollbar).width(LIST_WIDTH).height(Length::Fill);
            let view = DiffView::new(&ui.files, &ui.rows, &ui.paint, th, ui.top, ui.cursor, focused, self.face, |e| Message::Diff(Msg::Event(e))).mark(ui.mark);
            let mut body = row![list, self.vrule()];
            if let Some(pane) = plan_pane() {
                body = body.push(pane).push(self.vrule());
            }
            body.push(container(Element::from(view)).width(Length::FillPortion(3))).into()
        };
        column![header, self.hrule(), body].into()
    }
}

impl View {
    /// Forget everything about the project (the root or the connection changed).
    pub(super) fn reset(&mut self) {
        let poll = self.poll;
        // Agents and their branches belong to the old daemon too.
        let scope = if agent_of(self.scope).is_some() || self.scope == DiffScope::Everyone { DiffScope::Head } else { self.scope };
        *self = View { scope, poll, polling: self.polling, height: self.height, plan_hidden: self.plan_hidden, ..Default::default() };
    }

    /// The plan pane shows.
    fn plan_shown(&self) -> bool {
        self.plan.is_some() && !self.plan_hidden
    }

    /// The plan pane shows the review's comments that are on the plan. Called when either
    /// changed.
    fn plan_sync(&mut self) {
        let Some(p) = &mut self.plan else { return };
        let on_plan = self.review.comments.iter().filter(|c| c.plan && c.start_line > 0).map(|c| Comment {
            id: c.id.clone(),
            start_line: c.start_line,
            end_line: c.end_line,
            anchor: c.anchor.clone(),
            body: c.body.clone(),
            author: c.author.clone(),
            resolved: c.resolved,
            drifted: c.drifted,
            replies: c.replies.iter().map(|r| Reply { id: r.id.clone(), body: r.body.clone(), author: r.author.clone() }).collect(),
        });
        p.plan.show(on_plan.collect());
    }

    /// Drop the comment being written in the plan pane; the row it was on is picked again.
    fn plan_cancel(&mut self) {
        let Some(p) = &mut self.plan else { return };
        p.answering = None;
        p.plan.cancel();
        p.plan.step(0, false);
    }

    /// The keys go back to the changes (a comment being written in the plan pane keeps them).
    fn plan_let_go(&mut self) {
        if let Some(p) = &mut self.plan {
            p.plan.sel = None;
        }
    }

    fn max_top(&self) -> usize {
        // The last rows can scroll up to a third of the view, never out of it.
        self.rows.len().saturating_sub((self.height / 3).max(1))
    }

    /// Move the cursor by `by` rows (skipping the gaps between files) and keep it in view.
    fn step(&mut self, by: isize) {
        // Moving plainly lets go of the lines picked (`select` puts the mark back).
        self.mark = None;
        let last = self.rows.len().saturating_sub(1);
        let mut to = self.cursor.saturating_add_signed(by).min(last);
        while matches!(self.rows.get(to), Some(Row::Gap)) && to > 0 && to < last {
            to = if by < 0 { to - 1 } else { to + 1 };
        }
        self.cursor = to;
        self.reveal();
    }

    /// To the next (or previous) row that `want`s.
    /// ⇧↑ / ⇧↓: move, picking the lines from where this began to the cursor (for a comment).
    fn select(&mut self, by: isize) {
        let from = self.mark.or(matches!(self.rows.get(self.cursor), Some(Row::Line { .. })).then_some(self.cursor));
        self.step(by);
        // Only lines of code, of one file.
        let file = from.and_then(|m| self.rows.get(m)).and_then(Row::file);
        self.mark = from.filter(|_| matches!(self.rows.get(self.cursor), Some(Row::Line { file: f, .. }) if Some(*f) == file));
    }

    fn jump(&mut self, back: bool, want: impl Fn(&Row) -> bool) {
        self.mark = None;
        let found = if back {
            self.rows[..self.cursor.min(self.rows.len())].iter().rposition(&want)
        } else {
            self.rows.iter().enumerate().skip(self.cursor + 1).find(|(_, r)| want(r)).map(|(i, _)| i)
        };
        if let Some(i) = found {
            self.cursor = i;
            self.reveal();
        }
    }

    fn reveal(&mut self) {
        let h = self.height.max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + h {
            self.top = self.cursor + 1 - h;
        }
    }

    /// Fold or unfold the file under the cursor (`None`: the other way); the cursor goes to its
    /// header.
    fn fold(&mut self, open: Option<bool>) {
        let Some(i) = self.rows.get(self.cursor).and_then(Row::file) else { return };
        let Some(path) = self.files.get(i).map(|f| f.path.clone()) else { return };
        let open = open.unwrap_or(self.closed.contains(&path));
        if open == !self.closed.contains(&path) {
            return;
        }
        if open {
            self.closed.remove(&path);
        } else {
            self.closed.insert(path);
        }
        self.rebuild();
        self.cursor = self.rows.iter().position(|r| matches!(r, Row::File { file, .. } if *file == i)).unwrap_or(0);
        self.top = self.top.min(self.max_top());
        self.reveal();
    }

    /// Where the cursor is, in terms that survive a new diff: the file, and how far into it.
    fn place(&self) -> Option<(String, usize)> {
        let i = self.rows.get(self.cursor)?.file()?;
        let head = self.rows.iter().position(|r| matches!(r, Row::File { file, .. } if *file == i))?;
        Some((self.files.get(i)?.path.clone(), self.cursor - head))
    }

    fn restore(&mut self, at: Option<(String, usize)>) {
        let last = self.rows.len().saturating_sub(1);
        let found = at.and_then(|(path, off)| {
            let i = self.files.iter().position(|f| f.path == path)?;
            let head = self.rows.iter().position(|r| matches!(r, Row::File { file, .. } if *file == i))?;
            let len = self.rows[head..].iter().take_while(|r| r.file() == Some(i)).count();
            Some(head + off.min(len.saturating_sub(1)))
        });
        self.cursor = found.unwrap_or(self.cursor).min(last);
        self.top = self.top.min(self.max_top());
    }

    /// What a comment written at the cursor would be on, and its lines: the lines from the mark
    /// to the cursor (on the side that is there now if any of them is, else the removed ones),
    /// the file under a header, or the comment the cursor is in (an answer).
    fn target(&self) -> Option<(On, u32, u32)> {
        let row = self.rows.get(self.cursor)?;
        if let Row::Talk { comment: Some(id), .. } = row {
            return Some((On::Reply { to: id.clone() }, 0, 0));
        }
        let file = row.file()?;
        let path = self.files.get(file)?.path.clone();
        if !matches!(row, Row::Line { .. }) {
            return Some((On::File { path }, 0, 0));
        }
        let (a, b) = (self.mark.unwrap_or(self.cursor).min(self.cursor), self.mark.unwrap_or(self.cursor).max(self.cursor));
        // The lines of this file between the two, with their text.
        let lines: Vec<(Option<u32>, Option<u32>, &str)> = self.rows[a..=b.min(self.rows.len() - 1)]
            .iter()
            .filter_map(|r| match r {
                Row::Line { file: f, hunk, line, old, new } if *f == file => Some((*old, *new, self.files[file].hunks.get(*hunk)?.lines.get(*line)?.text.as_str())),
                _ => None,
            })
            .collect();
        let old = lines.iter().all(|(_, new, _)| new.is_none());
        let side: Vec<(u32, &str)> = lines.iter().filter_map(|(o, n, t)| Some((if old { (*o)? } else { (*n)? }, *t))).collect();
        let (start, end) = (side.first()?.0, side.last()?.0);
        let anchor = side.iter().map(|(_, t)| *t).collect::<Vec<_>>().join("\n");
        Some((On::Lines { path, old, anchor }, start, end))
    }

    /// Put the cursor on the last row of the comment being typed, in view.
    fn show_draft(&mut self) {
        if let Some(i) = self.rows.iter().rposition(|r| matches!(r, Row::Talk { kind: Talk::Draft, .. })) {
            self.cursor = i;
            let h = self.height.max(1);
            // With the hint row under it.
            if i + 1 >= self.top + h {
                self.top = i + 2 - h;
            } else if i < self.top {
                self.top = i;
            }
        }
    }

    /// `paint_file(f)`, or what it gave the last time for the same file with the same diff
    /// lines. Nothing is kept while the grammars are still loading (the colours would be
    /// missing for good).
    fn paint_of(&mut self, f: &DiffFile) -> Vec<Vec<Paint>> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        f.path.hash(&mut h);
        for hunk in &f.hunks {
            hunk.lines.len().hash(&mut h);
            for l in &hunk.lines {
                (l.kind as u8, &l.text).hash(&mut h);
            }
        }
        let (key, now) = (h.finish(), self.painted_at + 1);
        if let Some((used, paint)) = self.painted.get_mut(&key) {
            *used = now;
            return paint.clone();
        }
        let paint = paint_file(f);
        if nib_syntax::registry_if_ready().is_some() {
            self.painted.insert(key, (now, paint.clone()));
        }
        paint
    }

    /// The rows of comment `c`, under row-of-code `file`.
    fn talk(&mut self, file: Option<usize>, c: &ReviewComment, placed: bool) {
        let width = self.cols.saturating_sub(2 * 3 + 12);
        let mut push = |kind: Talk, text: String| {
            self.rows.push(Row::Talk { file, comment: Some(c.id.clone()), kind, text });
            self.paint.push(Vec::new());
        };
        // Where it is, when it is not shown under its own lines.
        let place = match (&c.path, placed, c.start_line) {
            (Some(_), true, _) => String::new(),
            (Some(p), false, 0) => format!(" · {p}"),
            (Some(p), false, _) if c.start_line == c.end_line => format!(" · {p}:{}", c.start_line),
            (Some(p), false, _) => format!(" · {p}:{}–{}", c.start_line, c.end_line),
            (None, ..) if c.plan => format!(" · on the plan, {}", super::comment::lines_label(c.start_line, c.end_line)),
            (None, ..) => " · on the work as a whole".to_string(),
        };
        let lines = if placed && c.start_line > 0 && c.start_line != c.end_line { format!(" · lines {}–{}", c.start_line, c.end_line) } else { String::new() };
        let state = match (c.resolved, c.round, c.drifted) {
            (true, ..) => " · resolved",
            (false, 0, _) => " · not sent yet",
            (false, _, true) => " · its lines changed",
            _ => "",
        };
        let head = format!("{} · {}{place}{lines}{state}", c.id, c.author);
        if c.resolved {
            let first = c.body.lines().next().unwrap_or("");
            push(Talk::Settled, format!("{head} · {}", first.chars().take(width.saturating_sub(head.chars().count() + 3).max(8)).collect::<String>()));
            return;
        }
        push(Talk::Head, head);
        for l in wrap(&c.body, width) {
            push(Talk::Body, l);
        }
        for r in &c.replies {
            for (i, l) in wrap(&format!("{}: {}", r.author, r.body), width.saturating_sub(2)).into_iter().enumerate() {
                push(Talk::Reply, format!("{}{l}", if i == 0 { "↳ " } else { "  " }));
            }
        }
    }

    /// The rows of the comment being typed, if `fits` says it is for here.
    fn draft_rows(&mut self, file: Option<usize>, fits: impl Fn(&Writing) -> bool) {
        let Some(w) = self.writing.as_ref().filter(|w| fits(w)) else { return };
        let width = self.cols.saturating_sub(2 * 3 + 12);
        let what = match &w.on {
            On::Reply { to } => format!("answer to {to}"),
            On::Whole => "comment on the work as a whole".to_string(),
            On::File { .. } => "comment on this file".to_string(),
            On::Lines { .. } => format!("comment on {}", w.draft.lines()),
        };
        let mut rows = vec![(Talk::Head, what)];
        let mut body = wrap(&w.draft.body, width);
        if let Some(last) = body.last_mut() {
            last.push('▏');
        }
        rows.extend(body.into_iter().map(|l| (Talk::Draft, l)));
        rows.push((Talk::Hint, "↵ save · ⇧↵ new line · esc cancel".to_string()));
        for (kind, text) in rows {
            self.rows.push(Row::Talk { file, comment: None, kind, text });
            self.paint.push(Vec::new());
        }
    }

    /// `rows` and `paint` from `files`, `closed`, the review and the comment being typed.
    fn rebuild(&mut self) {
        let live: HashSet<&str> = self.files.iter().map(|f| f.path.as_str()).collect();
        self.closed.retain(|p| live.contains(p.as_str()));
        self.rows.clear();
        self.paint.clear();
        let files = std::mem::take(&mut self.files);
        let comments = std::mem::take(&mut self.review.comments);
        // Above the files: comments on the work as a whole, on files not in this diff, and
        // (while its pane, where they show otherwise, is put away) on the plan.
        let pane = self.plan_shown();
        for c in comments.iter().filter(|c| !(c.plan && pane) && c.path.as_deref().is_none_or(|p| !files.iter().any(|f| f.path == p))) {
            self.talk(None, c, false);
            self.draft_rows(None, |w| matches!(&w.on, On::Reply { to } if *to == c.id));
        }
        self.draft_rows(None, |w| w.on == On::Whole);
        if !self.rows.is_empty() && !files.is_empty() {
            self.rows.push(Row::Gap);
            self.paint.push(Vec::new());
        }
        for (file, f) in files.iter().enumerate() {
            if file > 0 {
                self.rows.push(Row::Gap);
                self.paint.push(Vec::new());
            }
            let open = !self.closed.contains(&f.path);
            self.rows.push(Row::File { file, open });
            self.paint.push(Vec::new());
            if !open {
                continue;
            }
            // Which of this file's comments have a line of this diff to sit under.
            let mine: Vec<&ReviewComment> = comments.iter().filter(|c| c.path.as_deref() == Some(f.path.as_str())).collect();
            let shown = |c: &ReviewComment| {
                c.start_line > 0
                    && f.hunks.iter().any(|h| {
                        let (start, len) = if c.old { (h.old_start, h.old_len) } else { (h.new_start, h.new_len) };
                        c.end_line >= start && c.end_line < start + len
                    })
            };
            // Under the header: on the file itself, or on lines this diff does not show.
            for c in mine.iter().filter(|c| !shown(c)) {
                self.talk(Some(file), c, c.start_line == 0);
                self.draft_rows(Some(file), |w| matches!(&w.on, On::Reply { to } if *to == c.id));
            }
            self.draft_rows(Some(file), |w| matches!(&w.on, On::File { path } if *path == f.path));
            let note = if f.binary {
                Some("binary file")
            } else if f.hunks.is_empty() {
                Some(match f.status {
                    DiffStatus::Renamed => "renamed, nothing else changed",
                    DiffStatus::Added | DiffStatus::Deleted => "empty file",
                    DiffStatus::Modified => "only its mode changed",
                })
            } else {
                None
            };
            if let Some(text) = note {
                self.rows.push(Row::Note { file, text });
                self.paint.push(Vec::new());
            }
            let mut paint = self.paint_of(f).into_iter();
            for (hunk, h) in f.hunks.iter().enumerate() {
                self.rows.push(Row::Hunk { file, hunk });
                self.paint.push(Vec::new());
                let (mut old, mut new) = (h.old_start, h.new_start);
                for (line, l) in h.lines.iter().enumerate() {
                    let (o, n) = match l.kind {
                        DiffLineKind::Context => (Some(old), Some(new)),
                        DiffLineKind::Del => (Some(old), None),
                        DiffLineKind::Add => (None, Some(new)),
                    };
                    old += u32::from(o.is_some());
                    new += u32::from(n.is_some());
                    self.rows.push(Row::Line { file, hunk, line, old: o, new: n });
                    self.paint.push(paint.next().unwrap_or_default());
                    // The comments that end on this line, on its side.
                    let here = |c: &ReviewComment| c.start_line > 0 && if c.old { l.kind == DiffLineKind::Del && o == Some(c.end_line) } else { n == Some(c.end_line) };
                    for c in mine.iter().filter(|c| here(c)) {
                        self.talk(Some(file), c, true);
                        self.draft_rows(Some(file), |w| matches!(&w.on, On::Reply { to } if *to == c.id));
                    }
                    self.draft_rows(Some(file), |w| match &w.on {
                        On::Lines { path, old, .. } => *path == f.path && if *old { l.kind == DiffLineKind::Del && o == Some(w.draft.end_line) } else { n == Some(w.draft.end_line) },
                        _ => false,
                    });
                }
            }
            if f.truncated {
                self.rows.push(Row::Note { file, text: "… the rest of this file's diff is too long to show" });
                self.paint.push(Vec::new());
            }
        }
        // Colours that none of the last few diffs used are dropped (an emptied view, between
        // two scopes, is not a diff).
        if !files.is_empty() {
            self.painted_at += 1;
            let now = self.painted_at;
            self.painted.retain(|_, (used, _)| *used + PAINT_KEPT >= now);
        }
        self.files = files;
        self.review.comments = comments;
        if self.mark.is_some_and(|m| !matches!(self.rows.get(m), Some(Row::Line { .. }))) {
            self.mark = None;
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }
}

/// Syntax colours for every diff line of `f`, in order. The hunks are parsed as if they were
/// the file (each side on its own): fragments, but enough for strings, keywords and comments.
/// Empty when the language is unknown or the grammars are not loaded yet.
fn paint_file(f: &DiffFile) -> Vec<Vec<Paint>> {
    let lines: usize = f.hunks.iter().map(|h| h.lines.len()).sum();
    if lines == 0 || lines > PAINT_LINES {
        return Vec::new();
    }
    let Some(registry) = nib_syntax::registry_if_ready() else { return Vec::new() };
    // One text per side; where each diff line landed in it.
    let side = |del: bool| -> (String, Vec<Option<std::ops::Range<usize>>>) {
        let (mut text, mut at) = (String::new(), Vec::with_capacity(lines));
        for l in f.hunks.iter().flat_map(|h| h.lines.iter()) {
            let here = match l.kind {
                DiffLineKind::Context => true,
                DiffLineKind::Del => del,
                DiffLineKind::Add => !del,
            };
            at.push(here.then(|| {
                let start = text.len();
                text.push_str(&l.text);
                text.push('\n');
                start..text.len() - 1
            }));
        }
        (text, at)
    };
    let spans_of = |text: &str| {
        let rope = Rope::from_str(text);
        let lang = registry.detect(Path::new(&f.path), rope.slice(..))?;
        let syntax = nib_syntax::Syntax::new(&rope, lang).ok()?;
        Some(syntax.spans(&rope, 0..text.len()))
    };
    let ((new_text, new_at), (old_text, old_at)) = (side(false), side(true));
    let (Some(new_spans), old_spans) = (spans_of(&new_text), spans_of(&old_text).unwrap_or_default()) else { return Vec::new() };
    let pick = |spans: &[nib_syntax::Span], r: &std::ops::Range<usize>| -> Vec<Paint> {
        let from = spans.partition_point(|s| s.bytes.end <= r.start);
        spans[from..]
            .iter()
            .take_while(|s| s.bytes.start < r.end)
            .filter_map(|s| Some((s.bytes.start.max(r.start) - r.start..s.bytes.end.min(r.end) - r.start, nib_syntax::scope_name(s.scope?))))
            .collect()
    };
    f.hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .enumerate()
        .map(|(i, l)| match (l.kind, &new_at[i], &old_at[i]) {
            (DiffLineKind::Del, _, Some(r)) => pick(&old_spans, r),
            (_, Some(r), _) => pick(&new_spans, r),
            _ => Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;
    use nib_proto::{DiffHunk, DiffLine};

    fn line(kind: DiffLineKind, text: &str) -> DiffLine {
        DiffLine { kind, text: text.into() }
    }

    fn file(path: &str, lines: Vec<DiffLine>) -> DiffFile {
        let adds = lines.iter().filter(|l| l.kind == DiffLineKind::Add).count() as u32;
        let dels = lines.iter().filter(|l| l.kind == DiffLineKind::Del).count() as u32;
        let hunk = DiffHunk { old_start: 10, old_len: lines.len() as u32 - adds, new_start: 10, new_len: lines.len() as u32 - dels, header: String::new(), lines };
        DiffFile { path: path.into(), adds, dels, hunks: vec![hunk], ..Default::default() }
    }

    fn answer(a: &mut App, files: Vec<DiffFile>, stamp: u64) {
        let (scope, epoch) = (a.diff.scope, a.epoch);
        let _ = a.on_diff(scope, epoch, Ok(Response::Diff(DiffSet { stamp, files: Some(files), repo: true })));
    }

    fn two_files() -> Vec<DiffFile> {
        use DiffLineKind::*;
        vec![
            file("a.rs", vec![line(Context, "one"), line(Del, "two"), line(Add, "2"), line(Context, "three")]),
            file("src/b.rs", vec![line(Add, "new")]),
        ]
    }

    fn key(a: &mut App, s: &str) {
        let chord = KeyChord::parse(s).unwrap();
        let _ = a.update(Message::Key { chord, text: None, repeat: false });
    }

    #[test]
    fn the_tab_opens_lists_rows_and_closes() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleDiff));
        assert!(a.diff_active() && a.tabs.len() == 1 && a.tab().unwrap().passive());
        answer(&mut a, two_files(), 1);
        let ui = &a.diff;
        // a.rs: header, hunk, 4 lines; a gap; src/b.rs: header, hunk, 1 line.
        assert_eq!(ui.rows.len(), 10);
        assert_eq!(ui.rows[3], Row::Line { file: 0, hunk: 0, line: 1, old: Some(11), new: None });
        assert_eq!(ui.rows[4], Row::Line { file: 0, hunk: 0, line: 2, old: None, new: Some(11) });
        assert_eq!(ui.rows[5], Row::Line { file: 0, hunk: 0, line: 3, old: Some(12), new: Some(12) });
        assert_eq!(a.diff_json()["files"][1]["path"], "src/b.rs");
        let _ = a.update(Message::Control(Command::ToggleDiff));
        assert!(a.tabs.is_empty() && a.diff_json().is_null());
    }

    #[test]
    fn keys_walk_rows_hunks_and_files_and_fold() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleDiff));
        answer(&mut a, two_files(), 1);
        key(&mut a, "j");
        key(&mut a, "down");
        assert_eq!(a.diff.cursor, 2);
        key(&mut a, "]");
        assert_eq!(a.diff.rows[a.diff.cursor], Row::File { file: 1, open: true });
        key(&mut a, "n");
        assert_eq!(a.diff.rows[a.diff.cursor], Row::Hunk { file: 1, hunk: 0 });
        key(&mut a, "[");
        key(&mut a, "[");
        assert_eq!(a.diff.cursor, 0);
        // Folded: the file is one row, and stays folded when the diff changes.
        key(&mut a, "left");
        assert_eq!(a.diff.rows.len(), 5);
        answer(&mut a, two_files(), 2);
        assert_eq!((a.diff.rows.len(), &a.diff.rows[0]), (5, &Row::File { file: 0, open: false }));
        key(&mut a, "enter");
        assert_eq!(a.diff.rows.len(), 10);
    }

    #[test]
    fn a_new_diff_keeps_the_cursor_in_its_file() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleDiff));
        answer(&mut a, two_files(), 1);
        key(&mut a, "]");
        key(&mut a, "j");
        key(&mut a, "j");
        assert_eq!(a.diff.rows[a.diff.cursor], Row::Line { file: 1, hunk: 0, line: 0, old: None, new: Some(10) });
        // A file appears before it: the cursor follows src/b.rs.
        let mut files = two_files();
        files.insert(0, file("0.rs", vec![line(DiffLineKind::Add, "x")]));
        answer(&mut a, files, 2);
        assert_eq!(a.diff.rows[a.diff.cursor], Row::Line { file: 2, hunk: 0, line: 0, old: None, new: Some(10) });
        // An answer for a scope that is no longer shown changes nothing.
        let epoch = a.epoch;
        let _ = a.on_diff(DiffScope::Staged, epoch, Ok(Response::Diff(DiffSet { stamp: 9, files: Some(vec![]), repo: true })));
        assert_eq!(a.diff.files.len(), 3);
    }

    #[test]
    fn an_agents_work_shows_its_branch_and_plan_and_is_not_staged() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleDiff));
        let _ = a.diff_scope(DiffScope::Agent(3));
        // Its branch and plan arrive; the plan is read once for each version of it.
        let work = AiWork { branch: Some("nib/feat/sum".into()), dir: Some("/w/sum".into()), base: Some("main".into()), plan: Some(7), review: None };
        let _ = a.diff_work(3, work.clone());
        let _ = a.diff_work(3, work);
        assert!(a.pending.values().any(|p| matches!(p, Pending::DiffPlan { id: 3, stamp: 7 })));
        let _ = a.on_diff_plan(3, 7, Ok(Response::WorkPlan(Some("# Add sum\n\n1. write it\n".into()))));
        let _ = a.diff_work(3, AiWork { plan: Some(7), ..Default::default() });
        assert_eq!(a.pending.values().filter(|p| matches!(p, Pending::DiffPlan { .. })).count(), 1);
        let files = vec![DiffFile { dir: Some("/w/sum".into()), agents: vec![3], ..two_files().remove(1) }];
        answer(&mut a, files, 1);
        let j = a.diff_json();
        assert_eq!((j["scope"].as_str(), j["plan"]["title"].as_str(), j["plan"]["shown"].as_bool()), (Some("agent:3"), Some("Add sum"), Some(true)));
        assert_eq!(j["files"][0]["agents"][0], 3);
        key(&mut a, "shift+p");
        assert_eq!(a.diff_json()["plan"]["shown"], false);
        // Its files are its own to commit: Space does nothing; Enter opens the one in its work tree.
        key(&mut a, "space");
        assert!(!a.pending.values().any(|p| matches!(p, Pending::DiffGit)));
        key(&mut a, "o");
        assert!(a.pending.values().any(|p| matches!(p, Pending::Open { path, .. } if path == "/w/sum/src/b.rs")));
        // Another scope: the plan goes with the agent.
        key(&mut a, "1");
        assert!(a.diff.scope == DiffScope::Head && a.diff.plan.is_none());
    }

    #[test]
    fn comments_are_written_on_lines_shown_under_them_and_sent() {
        let send = if cfg!(target_os = "macos") { "cmd+shift+enter" } else { "ctrl+shift+enter" };
        let reviews = |a: &App| a.pending.values().filter(|p| matches!(p, Pending::DiffReview { id: 3 })).count();
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleDiff));
        let _ = a.diff_scope(DiffScope::Agent(3));
        a.diff.cols = 100;
        answer(&mut a, two_files(), 1);
        // Not on an agent's work there is nothing to comment on; here: mark `one`, down to `2`.
        for k in ["j", "j", "shift+down", "shift+down", "c"] {
            key(&mut a, k);
        }
        let w = a.diff.writing.as_ref().expect("writing");
        assert_eq!(w.on, On::Lines { path: "a.rs".into(), old: false, anchor: "one\n2".into() });
        assert_eq!((w.draft.start_line, w.draft.end_line), (10, 11));
        // The draft sits under the last of its lines, and takes the keys (j is a letter now).
        assert!(matches!(&a.diff.rows[5], Row::Talk { kind: Talk::Head, comment: None, .. }), "{:?}", a.diff.rows[5]);
        for c in "why j".chars() {
            let _ = a.update(Message::Key { chord: KeyChord::new(Key::Char(c)), text: Some(c.to_string()), repeat: false });
        }
        assert_eq!(a.diff.writing.as_ref().unwrap().draft.body, "why j");
        key(&mut a, "enter");
        assert!(a.diff.writing.is_none() && reviews(&a) == 1);

        // The daemon's answer: the comment shows under line 11, and one on a file not in the
        // diff shows above the files.
        let c1 = ReviewComment { id: "c1".into(), path: Some("a.rs".into()), start_line: 10, end_line: 11, anchor: "one\n2".into(), body: "why j".into(), author: "user".into(), ..Default::default() };
        let c2 = ReviewComment { id: "c2".into(), path: Some("gone.rs".into()), start_line: 3, end_line: 3, body: "and this".into(), author: "user".into(), round: 1, ..Default::default() };
        let _ = a.on_diff_review(3, Ok(Response::Review(Review { round: 1, since: true, comments: vec![c1.clone(), c2] })));
        let talk = |a: &App, i: usize| match &a.diff.rows[i] {
            Row::Talk { comment, kind, text, .. } => (comment.clone().unwrap_or_default(), *kind, text.clone()),
            r => panic!("row {i}: {r:?}"),
        };
        assert_eq!(talk(&a, 0), ("c2".to_string(), Talk::Head, "c2 · user · gone.rs:3".to_string()));
        // c2: head, body, a gap; a.rs: header, hunk, one, −two, +2, then c1.
        assert_eq!(talk(&a, 8), ("c1".to_string(), Talk::Head, "c1 · user · lines 10–11 · not sent yet".to_string()));
        assert_eq!(talk(&a, 9).2, "why j");
        assert_eq!(a.diff_json()["review"]["comments"][0]["id"], "c1");
        // On the comment: r answers it, x resolves it, backspace takes back one not sent yet.
        a.diff.cursor = 8;
        key(&mut a, "r");
        assert_eq!(a.diff.writing.as_ref().map(|w| w.on.clone()), Some(On::Reply { to: "c1".into() }));
        key(&mut a, "esc");
        assert!(a.diff.writing.is_none());
        key(&mut a, "x");
        key(&mut a, "backspace");
        assert_eq!(reviews(&a), 3);
        // Resolved, it is one dim row.
        let _ = a.on_diff_review(3, Ok(Response::Review(Review { round: 1, since: true, comments: vec![ReviewComment { resolved: true, round: 1, ..c1 }] })));
        assert_eq!(talk(&a, 5).1, Talk::Settled);
        // Sent: the agent is told; then `t` shows what changed since, the review still there.
        key(&mut a, send);
        assert_eq!(reviews(&a), 4);
        key(&mut a, "t");
        assert!(a.diff.scope == DiffScope::Since(3) && a.diff.review.round == 1);
        key(&mut a, "t");
        assert_eq!(a.diff.scope, DiffScope::Agent(3));
        // Another agent's work: another review.
        let _ = a.diff_scope(DiffScope::Head);
        assert!(a.diff.review.comments.is_empty());
        key(&mut a, "c");
        assert!(a.diff.writing.is_none());
    }

    #[test]
    fn the_plan_is_commented_on_in_the_same_review() {
        let reviews = |a: &App| a.pending.values().filter(|p| matches!(p, Pending::DiffReview { id: 3 })).count();
        let typed = |a: &mut App, s: &str| {
            for c in s.chars() {
                let _ = a.update(Message::Key { chord: KeyChord::new(Key::Char(c)), text: Some(c.to_string()), repeat: false });
            }
        };
        const PLAN: &str = "# Add sum\n\nWhy: asked.\n\n1. write it\n2. test it\n";
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleDiff));
        let _ = a.diff_scope(DiffScope::Agent(3));
        a.diff.cols = 100;
        let _ = a.diff_work(3, AiWork { plan: Some(7), ..Default::default() });
        let _ = a.on_diff_plan(3, 7, Ok(Response::WorkPlan(Some(PLAN.into()))));
        answer(&mut a, two_files(), 1);
        let rows = a.diff.rows.len();

        // Tab gives the pane the keys: ↓ and ⇧↓ pick its rows (the cursor in the changes stays),
        // ↵ writes on them, every key goes to the comment, ↵ files it in the agent's review.
        for k in ["tab", "down", "down", "shift+down", "enter"] {
            key(&mut a, k);
        }
        assert_eq!(a.diff.cursor, 0);
        assert_eq!(a.diff_json()["plan"]["draft"]["start_line"], 5);
        typed(&mut a, "why j");
        assert_eq!(a.diff_json()["plan"]["draft"], json!({ "start_line": 5, "end_line": 6, "body": "why j", "editing": null }));
        // The plan is read again as it was (the agent recorded it twice): nothing is lost.
        let _ = a.on_diff_plan(3, 8, Ok(Response::WorkPlan(Some(PLAN.into()))));
        assert_eq!(a.diff.plan.as_ref().map(|p| (p.stamp, p.plan.comments.draft.is_some())), Some((8, true)));
        key(&mut a, "enter");
        let p = &a.diff.plan.as_ref().unwrap().plan;
        assert!(p.comments.draft.is_none() && p.sel == Some((3, 3)) && reviews(&a) == 1, "filed with nibd; the keys stay in the pane");

        // nibd's answer: the comment shows in the pane, under its rows, not among the changes.
        let c1 = ReviewComment { id: "c1".into(), plan: true, start_line: 5, end_line: 6, anchor: "1. write it\n2. test it".into(), body: "why j".into(), author: "user".into(), ..Default::default() };
        let whole = ReviewComment { id: "c2".into(), body: "and tests".into(), author: "user".into(), ..Default::default() };
        let _ = a.on_diff_review(3, Ok(Response::Review(Review { round: 0, since: false, comments: vec![c1.clone(), whole.clone()] })));
        let j = a.diff_json();
        assert_eq!((j["plan"]["comments"].as_array().map(Vec::len), j["plan"]["comments"][0]["id"].as_str(), j["review"]["comments"][0]["plan"].as_bool()), (Some(1), Some("c1"), Some(true)));
        assert_eq!(a.diff.rows.len(), rows + 3, "c2 above the files: head, body, a gap");
        // On its row: r answers it, x resolves it, ⌫ takes it back (not sent yet).
        key(&mut a, "r");
        typed(&mut a, "ok");
        assert_eq!(a.diff_json()["plan"]["answering"], "c1");
        key(&mut a, "enter");
        key(&mut a, "x");
        key(&mut a, "backspace");
        assert_eq!(reviews(&a), 4);
        // Sent, with the agent's answer: it shows under it, and is no longer taken back.
        let sent = ReviewComment { round: 1, replies: vec![nib_proto::ReviewReply { id: "r3".into(), body: "tests first now".into(), author: "claude-1".into() }], ..c1 };
        let _ = a.on_diff_review(3, Ok(Response::Review(Review { round: 1, since: false, comments: vec![sent.clone()] })));
        assert_eq!(a.diff_json()["plan"]["comments"][0]["replies"][0]["body"], "tests first now");
        let _ = a.update(Message::Diff(Msg::Plan(PlanMsg::Remove("c1".into()))));
        assert_eq!(reviews(&a), 4);
        // Tab lets go: the keys are the changes' again.
        key(&mut a, "tab");
        key(&mut a, "down");
        assert_eq!((a.diff.plan.as_ref().unwrap().plan.sel, a.diff.cursor), (None, 1));

        // The pane put away: the comment shows above the files instead, and back.
        key(&mut a, "shift+p");
        assert!(a.diff.rows.iter().any(|r| matches!(r, Row::Talk { text, .. } if text == "c1 · user · on the plan, lines 5–6")), "{:?}", a.diff.rows);
        key(&mut a, "tab");
        assert_eq!(a.diff.plan.as_ref().unwrap().plan.sel, None, "hidden, it takes no keys");
        key(&mut a, "shift+p");
        assert_eq!(a.diff.rows.len(), rows);
        // A click picks a row, a second one writes on it.
        let _ = a.update(Message::Diff(Msg::Plan(PlanMsg::Row(1))));
        let _ = a.update(Message::Diff(Msg::Plan(PlanMsg::Row(1))));
        assert_eq!(a.diff_json()["plan"]["draft"]["start_line"], 3);
        key(&mut a, "esc");

        // Another plan: it is shown, and the review is read again (nibd finds where the lines
        // of each comment went); one whose lines are gone shows under a row all the same.
        a.connected = true;
        let _ = a.diff_work(3, AiWork { plan: Some(9), ..Default::default() });
        let _ = a.on_diff_plan(3, 9, Ok(Response::WorkPlan(Some("# Add sum\n\n1. test it\n".into()))));
        assert_eq!(reviews(&a), 5);
        let _ = a.on_diff_review(3, Ok(Response::Review(Review { round: 1, since: false, comments: vec![ReviewComment { drifted: true, start_line: 3, end_line: 3, ..sent }] })));
        let c = &a.diff_json()["plan"]["comments"][0];
        assert_eq!((c["start_line"].as_u64(), c["drifted"].as_bool(), c["anchor"].as_str()), (Some(3), Some(true), Some("1. write it\n2. test it")));
    }

    #[test]
    fn the_socket_comments_on_the_plan() {
        assert_eq!(comment_on(Some("plan:3-4"), "x".into()), Ok(ReviewComment { plan: true, start_line: 3, end_line: 4, body: "x".into(), ..Default::default() }));
        assert_eq!(comment_on(Some("./plan:3"), "x".into()).map(|c| (c.plan, c.path)), Ok((false, Some("./plan".to_string()))));
        assert_eq!(comment_on(Some("plan"), "x".into()).map(|c| (c.plan, c.path)), Ok((false, Some("plan".to_string()))), "no lines: a file");
        assert!(comment_on(Some("plan:4-3"), "x".into()).is_err());
        let ask = |a: &mut App, target: &str| {
            let (call, rx) = crate::control::Call::test(json!({ "cmd": "diff.review", "what": "comment", "target": target, "body": "why?" }));
            let _ = a.update(Message::Ctl(call));
            rx.try_recv().unwrap()
        };
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleDiff));
        assert_eq!(ask(&mut a, "plan:3")["ok"], false, "not an agent's work");
        let _ = a.diff_scope(DiffScope::Agent(3));
        assert_eq!(ask(&mut a, "plan:3")["ok"], true);
        assert_eq!(ask(&mut a, "plan:0")["ok"], false);
        assert_eq!(a.pending.values().filter(|p| matches!(p, Pending::DiffReview { id: 3 })).count(), 1);
    }

    #[test]
    fn the_scope_groups_fold_to_what_shows() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleDiff));
        // Your own changes show: YOURS is open, AGENTS folded.
        assert_eq!((a.diff_group_open(0), a.diff_group_open(1)), (true, false));
        // An agent's work, asked for from its chat: the other way round.
        let _ = a.diff_show(DiffScope::Agent(3));
        assert_eq!((a.diff_group_open(0), a.diff_group_open(1)), (false, true));
        // A click on a name folds or unfolds it, and that stays while the scopes change…
        let _ = a.update(Message::Diff(Msg::Fold(0)));
        let _ = a.update(Message::Diff(Msg::Fold(1)));
        assert_eq!((a.diff_group_open(0), a.diff_group_open(1)), (true, false));
        let _ = a.diff_scope(DiffScope::Head);
        assert_eq!((a.diff_group_open(0), a.diff_group_open(1)), (true, false));
        // …until an agent's work is asked for again.
        let _ = a.diff_show(DiffScope::Agent(3));
        assert_eq!((a.diff_group_open(0), a.diff_group_open(1)), (false, true));
        assert_eq!(a.diff_json()["groups"]["yours"], false);
    }

    #[test]
    fn long_comments_wrap_between_words() {
        assert_eq!(wrap("one two three four", 12), ["one two", "three four"]);
        assert_eq!(wrap("a\n\nabcdefghijklmnop", 12), ["a", "", "abcdefghijkl", "mnop"]);
    }

    #[test]
    fn space_stages_and_enter_opens_the_line() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleDiff));
        answer(&mut a, two_files(), 1);
        key(&mut a, "space");
        assert!(a.pending.values().any(|p| matches!(p, Pending::DiffGit)));
        // On the removed line: the editor opens where it was.
        for _ in 0..3 {
            key(&mut a, "j");
        }
        key(&mut a, "enter");
        assert!(a.pending.values().any(|p| matches!(p, Pending::Open { path, goto: Some(Pos { line: 10, col: 0 }), .. } if path == "a.rs")));
        key(&mut a, "s");
        assert_eq!((a.diff.scope, a.diff.loaded), (DiffScope::Unstaged, false));
    }
}
