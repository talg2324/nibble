//! The file tree: a sidebar left of the editor, and the first pane in the focus ring. It always
//! shows the whole top level of the project and never goes above it.
//!
//! Git is a lens over the same tree, not a separate page: `f` files, `c` changes, `s` staged,
//! `u` upstream. A git lens unfolds itself down to every matching file and folds the rest of
//! each folder into a "⋯ n clean" row.
//!
//! The tree owns what is per file: seeing its status, staging it, and the file operations (new,
//! rename, delete), by keyboard or mouse. What is per repository (commit, fetch, pull, push,
//! stash, history) is the Git panel's; `m` jumps to its commit box.
//!
//! Roots, at the bottom left: besides the project, the tree can show the daemon session's
//! scratch directory (`$NIB_SCRATCH`) and the project's persistent NIBDIR (`$NIBDIR`), where
//! agents keep what they make. Same keys and file operations; no git. Tab cycles them.

use super::{app_wide, App, Focus, Message, Pending};
use super::line_input::LineInput;
use iced::widget::{button, column, container, mouse_area, rich_text, row, space, text};
use iced::{mouse, Alignment, Border, Color, Element, Length, Padding, Task};
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::Command;
use nib_proto::{FsOp, GitChange, GitInfo, GitOp, Place, Request, Response, RpcError, TreeEntry, TreeSnapshot};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const WIDTH: f32 = 320.0;
/// Height of one row.
const ROW: f32 = 22.0;
const INDENT: f32 = 12.0;
/// Two clicks on a row within this are a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Window height that is not tree rows (tab bar, header, lenses, the bars at the bottom, control
/// strip). An estimate on the generous side: it only decides when the cursor is scrolled to.
const CHROME: f32 = 200.0;

pub(super) const GREEN: Color = Color::from_rgb8(0x23, 0xd1, 0x8b);
const TEAL: Color = Color::from_rgb8(0x29, 0xb8, 0xdb);
const RED: Color = Color::from_rgb8(0xf1, 0x4c, 0x4c);
const MAGENTA: Color = Color::from_rgb8(0xd6, 0x70, 0xd6);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lens {
    #[default]
    Files,
    Changes,
    Staged,
    Upstream,
}

/// The lenses with the key that picks each.
const LENSES: [(Lens, char, &str); 4] =
    [(Lens::Files, 'f', "files"), (Lens::Changes, 'c', "changes"), (Lens::Staged, 's', "staged"), (Lens::Upstream, 'u', "upstream")];

/// The roots in the order Tab cycles through them, with their label at the bottom left.
const PLACES: [(Place, &str); 3] = [(Place::Project, ""), (Place::Scratch, "scratch"), (Place::Nibdir, "nibdir")];

/// One file or folder. `Tree::entries` holds them depth first, folders before files.
struct Entry {
    /// Relative to the project root.
    path: String,
    /// Where the name starts in `path`.
    name_at: usize,
    depth: usize,
    dir: bool,
    ignored: bool,
    /// Not on disk: deleted here, or only known from the upstream branch.
    gone: bool,
    /// git's two status letters (index, work tree); two spaces when clean.
    code: [u8; 2],
    /// Index just past this folder's last descendant.
    end: usize,
    // Counts of files: 0 or 1 for a file, the sum of everything inside for a folder.
    files: u32,
    dirty: u32,
    staged: u32,
    /// Modified or deleted in the work tree (untracked files are `fresh`).
    unstaged: u32,
    fresh: u32,
    upstream: u32,
}

impl Entry {
    fn name(&self) -> &str {
        &self.path[self.name_at..]
    }
}

/// A line of the tree as shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// `dim`: a top-level entry with nothing for the current lens (the top level is always whole).
    Entry { i: usize, dim: bool },
    /// "⋯ n clean": what a git lens folded away inside folder `parent`.
    Stub { parent: usize, count: u32 },
    /// Where the name of a new file is typed.
    Input { depth: usize },
}

/// A line of text being typed.
enum Input {
    Rename { path: String, text: LineInput },
    /// `dir`: the folder it goes in (empty: the root).
    New { dir: String, folder: bool, text: LineInput },
}

impl Input {
    fn text_mut(&mut self) -> &mut LineInput {
        match self {
            Input::Rename { text, .. } | Input::New { text, .. } => text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Menu {
    /// Under the "new" button: file or folder.
    New,
    /// Right click on the row of this path.
    Row(String),
}

/// What a key, a button or a menu item can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Open,
    Rename,
    NewFile,
    NewFolder,
    Stage,
    Delete,
    Commit,
    FoldAll,
    /// Make the folder under the cursor (a file's folder) the project root.
    Root,
    /// Make the root's parent the root.
    RootUp,
}

#[derive(Debug, Clone)]
pub enum Msg {
    /// A row was clicked (paths, not row numbers: the tree may refresh under the mouse).
    Click(String),
    Twisty(String),
    /// The git letters of a row: stage / unstage it.
    Gutter(String),
    Stub(String),
    Context(String),
    Lens(Lens),
    NewMenu,
    Do(Act),
    /// Answer to "delete?".
    Confirm(bool),
    Scroll(f32),
    /// A place at the bottom left was clicked.
    Place(Place),
    /// A click on empty space.
    Focus,
    Pasted(Option<String>),
}

#[derive(Default)]
pub struct Tree {
    /// The sidebar is shown.
    pub open: bool,
    entries: Vec<Entry>,
    /// Path -> index into `entries`.
    index: HashMap<String, usize>,
    /// The last listing the daemon sent (it leaves it out while nothing was added or removed).
    listing: Vec<TreeEntry>,
    stamp: Option<u64>,
    loaded: bool,
    truncated: bool,
    repo: bool,
    ahead: u32,
    behind: u32,
    /// A snapshot was asked for; `stale`: and something changed since, so ask again.
    asking: bool,
    stale: bool,
    lens: Lens,
    /// Folders unfolded in the files lens.
    opened: HashSet<String>,
    /// Folders folded in the git lenses (which unfold everything that matches).
    folded: HashSet<String>,
    /// The ignored folders unfolded when the last snapshot was asked for (the daemon lists
    /// what's in an ignored folder only when asked to).
    asked_open: Vec<String>,
    /// Cursor and selection anchor: indices into `rows()`.
    cur: usize,
    anchor: usize,
    /// First row shown.
    top: usize,
    scroll_acc: f32,
    selected: HashSet<String>,
    input: Option<Input>,
    /// Paths waiting for "delete? y/n".
    ask: Option<Vec<String>>,
    menu: Option<Menu>,
    last_click: Option<(String, Instant)>,
    /// Put the cursor here once it shows up (just created or renamed).
    reveal: Option<String>,
    /// Which directory this tree shows.
    pub(super) place: Place,
    /// The place the snapshot being asked for is of (the place may change before it arrives).
    asked: Place,
    /// Scratch and NIBDIR on the daemon's machine, from Hello (empty: not offered).
    dirs: [String; 2],
    /// The other places' trees, kept (folds, cursor) while this one is shown.
    parked: Vec<Tree>,
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

/// Every folder above `path`, outermost first.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(move |(i, _)| &path[..i])
}

/// `paths` without those inside another of them.
fn top_level(mut paths: Vec<String>) -> Vec<String> {
    let all: HashSet<String> = paths.iter().cloned().collect();
    paths.retain(|p| !ancestors(p).any(|a| all.contains(a)));
    paths
}

fn build(listing: &[TreeEntry], changes: &[GitChange], upstream: &[String]) -> Vec<Entry> {
    struct Item {
        dir: bool,
        ignored: bool,
        gone: bool,
    }
    let mut items: HashMap<&str, Item> =
        listing.iter().map(|e| (e.path.as_str(), Item { dir: e.dir, ignored: e.ignored, gone: false })).collect();
    let codes: HashMap<&str, [u8; 2]> = changes
        .iter()
        .filter_map(|c| match c.code.as_bytes() {
            [x, y] => Some((c.path.as_str(), [*x, *y])),
            _ => None,
        })
        .collect();
    let incoming: HashSet<&str> = upstream.iter().map(String::as_str).collect();
    // git knows files the disk doesn't have: deleted here, or new upstream.
    for path in codes.keys().chain(incoming.iter()).copied() {
        if items.contains_key(path) {
            continue;
        }
        items.insert(path, Item { dir: false, ignored: false, gone: true });
        let mut p = parent_of(path);
        while !p.is_empty() && !items.contains_key(p) {
            items.insert(p, Item { dir: true, ignored: false, gone: true });
            p = parent_of(p);
        }
    }
    let mut kids: HashMap<&str, Vec<&str>> = HashMap::new();
    for path in items.keys().copied() {
        kids.entry(parent_of(path)).or_default().push(path);
    }
    for list in kids.values_mut() {
        list.sort_by_cached_key(|p| (!items[p].dir, p.to_lowercase(), p.to_string()));
    }

    struct Ctx<'a> {
        items: HashMap<&'a str, Item>,
        kids: HashMap<&'a str, Vec<&'a str>>,
        codes: HashMap<&'a str, [u8; 2]>,
        incoming: HashSet<&'a str>,
    }
    fn emit(cx: &Ctx, parent: &str, depth: usize, out: &mut Vec<Entry>) {
        let Some(list) = cx.kids.get(parent) else { return };
        for path in list {
            let it = &cx.items[path];
            let code = cx.codes.get(path).copied().unwrap_or(*b"  ");
            let file = !it.dir;
            let fresh = file && code[0] == b'?';
            let at = out.len();
            out.push(Entry {
                path: path.to_string(),
                name_at: path.rfind('/').map_or(0, |i| i + 1),
                depth,
                dir: it.dir,
                ignored: it.ignored,
                gone: it.gone,
                code,
                end: at + 1,
                files: (file && !it.gone) as u32,
                dirty: (file && code != *b"  ") as u32,
                staged: (file && !matches!(code[0], b' ' | b'?' | b'!')) as u32,
                unstaged: (file && code[1] != b' ' && !fresh) as u32,
                fresh: fresh as u32,
                upstream: (file && cx.incoming.contains(path)) as u32,
            });
            if !it.dir {
                continue;
            }
            emit(cx, path, depth + 1, out);
            let end = out.len();
            let mut sum = [0u32; 6];
            for e in out[at + 1..end].iter().filter(|e| !e.dir) {
                for (s, n) in sum.iter_mut().zip([e.files, e.dirty, e.staged, e.unstaged, e.fresh, e.upstream]) {
                    *s += n;
                }
            }
            let e = &mut out[at];
            e.end = end;
            [e.files, e.dirty, e.staged, e.unstaged, e.fresh, e.upstream] = sum;
        }
    }
    let mut out = Vec::with_capacity(items.len());
    emit(&Ctx { items, kids, codes, incoming }, "", 0, &mut out);
    out
}

impl Tree {
    /// Scratch and NIBDIR, from the daemon's Hello (empty strings: it couldn't make them).
    /// The project's files and folders, for the AI panel's `@`. Empty while the tree shows
    /// another place (its listing is that place's then).
    pub(super) fn project_listing(&self) -> &[TreeEntry] {
        if self.place == Place::Project {
            &self.listing
        } else {
            &[]
        }
    }

    pub(super) fn set_places(&mut self, scratch: String, nibdir: String) {
        self.dirs = [scratch, nibdir];
    }

    /// For `nib ctl state`: the place shown and where scratch / NIBDIR are.
    pub(super) fn places_json(&self) -> serde_json::Value {
        let none_if_empty = |s: &String| (!s.is_empty()).then(|| s.clone());
        serde_json::json!({
            "shown": format!("{:?}", self.place).to_lowercase(),
            "scratch": none_if_empty(&self.dirs[0]),
            "nibdir": none_if_empty(&self.dirs[1]),
        })
    }

    /// Absolute directory of `place` on the daemon's machine (None: not offered).
    fn dir_of(&self, place: Place) -> Option<&str> {
        let d = match place {
            Place::Project => return None,
            Place::Scratch => &self.dirs[0],
            Place::Nibdir => &self.dirs[1],
        };
        (!d.is_empty()).then_some(d.as_str())
    }

    fn apply(&mut self, snap: TreeSnapshot) {
        let keep = self.cursor_path().map(str::to_string);
        if let Some(entries) = snap.entries {
            self.listing = entries;
        }
        self.stamp = Some(snap.stamp);
        self.truncated = snap.truncated;
        self.repo = snap.repo;
        self.ahead = snap.ahead;
        self.behind = snap.behind;
        self.entries = build(&self.listing, &snap.changes, &snap.upstream);
        self.index = self.entries.iter().enumerate().map(|(i, e)| (e.path.clone(), i)).collect();
        self.loaded = true;
        let index = &self.index;
        self.selected.retain(|p| index.contains_key(p));
        match (self.reveal.take(), keep) {
            (Some(p), _) if self.index.contains_key(&p) => self.show(&p),
            (reveal, keep) => {
                // Not listed yet: the next snapshot has it.
                self.reveal = reveal;
                if let Some(p) = keep.filter(|p| self.cursor_path() != Some(p.as_str())) {
                    self.go_to(&p);
                }
            }
        }
        self.clamp();
    }

    /// The entry has something for the current lens.
    fn hit(&self, e: &Entry) -> bool {
        match self.lens {
            Lens::Files => !e.gone,
            Lens::Changes => e.dirty > 0,
            Lens::Staged => e.staged > 0,
            Lens::Upstream => e.upstream > 0,
        }
    }

    fn expanded(&self, e: &Entry) -> bool {
        e.dir
            && match self.lens {
                Lens::Files => self.opened.contains(&e.path),
                _ => self.hit(e) && !self.folded.contains(&e.path),
            }
    }

    fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        self.walk(0, self.entries.len(), None, &mut out);
        out
    }

    fn walk(&self, start: usize, end: usize, parent: Option<usize>, out: &mut Vec<Row>) {
        let parent_path = parent.map_or("", |p| self.entries[p].path.as_str());
        if matches!(&self.input, Some(Input::New { dir, .. }) if dir == parent_path) {
            out.push(Row::Input { depth: parent.map_or(0, |p| self.entries[p].depth + 1) });
        }
        let mut hidden = 0;
        let mut i = start;
        while i < end {
            let e = &self.entries[i];
            if self.hit(e) {
                out.push(Row::Entry { i, dim: false });
                if self.expanded(e) {
                    self.walk(i + 1, e.end, Some(i), out);
                }
            } else if e.gone {
                // Only a git lens shows what isn't on disk.
            } else if parent.is_none() {
                out.push(Row::Entry { i, dim: true });
            } else {
                hidden += 1;
            }
            i = e.end;
        }
        if let (Some(parent), true) = (parent, hidden > 0) {
            out.push(Row::Stub { parent, count: hidden });
        }
    }

    fn row_entry(&self, r: Option<&Row>) -> Option<&Entry> {
        match r {
            Some(Row::Entry { i, .. }) => self.entries.get(*i),
            _ => None,
        }
    }

    fn cursor_entry(&self) -> Option<&Entry> {
        self.row_entry(self.rows().get(self.cur))
    }

    fn cursor_path(&self) -> Option<&str> {
        self.cursor_entry().map(|e| e.path.as_str())
    }

    /// Cursor onto `path`, if a row shows it.
    fn go_to(&mut self, path: &str) -> bool {
        let rows = self.rows();
        let Some(at) = rows.iter().position(|r| self.row_entry(Some(r)).is_some_and(|e| e.path == path)) else { return false };
        self.cur = at;
        self.anchor = at;
        true
    }

    fn clamp(&mut self) {
        let last = self.rows().len().saturating_sub(1);
        self.cur = self.cur.min(last);
        self.anchor = self.anchor.min(last);
        self.top = self.top.min(last);
    }

    /// Scroll just enough to show the cursor in a list `visible` rows high.
    fn follow(&mut self, visible: usize) {
        self.clamp();
        if self.cur < self.top {
            self.top = self.cur;
        } else if self.cur >= self.top + visible {
            self.top = self.cur + 1 - visible;
        }
    }

    fn set_lens(&mut self, lens: Lens) {
        let keep = self.cursor_path().map(str::to_string);
        self.lens = lens;
        if !keep.is_some_and(|p| self.go_to(&p)) {
            self.cur = 0;
            self.anchor = 0;
        }
    }

    fn step(&mut self, by: isize, extend: bool) {
        let rows = self.rows();
        let last = rows.len().saturating_sub(1);
        self.cur = (self.cur as isize + by).clamp(0, last as isize) as usize;
        if extend {
            self.select_range(&rows);
        } else {
            self.anchor = self.cur;
            self.selected.clear();
        }
    }

    /// Select every entry between the anchor and the cursor.
    fn select_range(&mut self, rows: &[Row]) {
        let (a, b) = (self.anchor.min(self.cur), self.anchor.max(self.cur));
        self.selected = rows
            .iter()
            .take(b + 1)
            .skip(a)
            .filter_map(|r| self.row_entry(Some(r)))
            .map(|e| e.path.clone())
            .collect();
    }

    fn set_fold(&mut self, path: &str, folded: bool) {
        match (self.lens, folded) {
            (Lens::Files, true) => drop(self.opened.remove(path)),
            (Lens::Files, false) => drop(self.opened.insert(path.to_string())),
            (_, true) => drop(self.folded.insert(path.to_string())),
            (_, false) => drop(self.folded.remove(path)),
        }
    }

    /// Ignored folders unfolded in the files lens, sorted.
    fn open_ignored(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .opened
            .iter()
            .filter(|p| self.index.get(*p).is_some_and(|i| self.entries[*i].dir && self.entries[*i].ignored))
            .cloned()
            .collect();
        out.sort();
        out
    }

    fn toggle_fold(&mut self, path: &str) {
        let Some(e) = self.index.get(path).map(|i| &self.entries[*i]) else { return };
        if e.dir {
            let folded = self.expanded(e);
            self.set_fold(path, folded);
        }
    }

    /// ←: fold the folder under the cursor; on anything already folded, go to its parent.
    fn fold_or_parent(&mut self) {
        let Some(e) = self.cursor_entry() else { return };
        let path = e.path.clone();
        if self.expanded(e) {
            self.set_fold(&path, true);
        } else if !parent_of(&path).is_empty() {
            self.selected.clear();
            self.go_to(parent_of(&path));
        }
    }

    fn fold_all(&mut self, folded: bool) {
        let keep = self.cursor_path().map(str::to_string);
        let dirs = self.entries.iter().filter(|e| e.dir && !e.ignored).map(|e| e.path.clone());
        match (self.lens, folded) {
            (Lens::Files, true) => self.opened.clear(),
            (Lens::Files, false) => self.opened = dirs.collect(),
            (_, true) => self.folded = dirs.collect(),
            (_, false) => self.folded.clear(),
        }
        self.selected.clear();
        // Folded away: the cursor moves up to what is left of its branch.
        let Some(p) = keep else { return };
        let seen = [p.as_str()].into_iter().chain(ancestors(&p).collect::<Vec<_>>().into_iter().rev()).any(|a| self.go_to(a));
        if !seen {
            self.cur = 0;
            self.anchor = 0;
        }
    }

    /// Unfold down to `path` and put the cursor on it; in the files lens if this one hides it.
    fn show(&mut self, path: &str) {
        for a in ancestors(path) {
            self.opened.insert(a.to_string());
            self.folded.remove(a);
        }
        if !self.go_to(path) {
            self.lens = Lens::Files;
            self.go_to(path);
        }
    }

    /// The selection, or else what the cursor is on. In tree order.
    fn targets(&self) -> Vec<String> {
        if self.selected.is_empty() {
            return self.cursor_path().map(str::to_string).into_iter().collect();
        }
        self.entries.iter().filter(|e| self.selected.contains(&e.path)).map(|e| e.path.clone()).collect()
    }

    /// Open the name box for a new file or folder: in the folder under the cursor, or next to
    /// the file under it.
    fn start_new(&mut self, folder: bool) {
        let dir = match self.cursor_entry() {
            Some(e) if e.dir && !e.gone => e.path.clone(),
            Some(e) => parent_of(&e.path).to_string(),
            None => String::new(),
        };
        // The box is a row inside that folder, which only the files lens is sure to show.
        self.lens = Lens::Files;
        for a in ancestors(&dir).chain([dir.as_str()]).filter(|a| !a.is_empty()) {
            self.opened.insert(a.to_string());
        }
        self.selected.clear();
        self.input = Some(Input::New { dir, folder, text: LineInput::default() });
        if let Some(at) = self.rows().iter().position(|r| matches!(r, Row::Input { .. })) {
            self.cur = at;
            self.anchor = at;
        }
    }

    /// (files, changed, staged, upstream) over the whole project.
    fn counts(&self) -> [u32; 4] {
        let mut n = [0; 4];
        for e in self.entries.iter().filter(|e| !e.dir) {
            for (n, v) in n.iter_mut().zip([e.files, e.dirty, e.staged, e.upstream]) {
                *n += v;
            }
        }
        n
    }
}

impl App {
    // -----------------------------------------------------------------------------------------
    // daemon
    // -----------------------------------------------------------------------------------------

    /// Ask the daemon for the project's files and git state. One question at a time: asked again
    /// while one is out, it is repeated when the answer arrives.
    pub(super) fn tree_refresh(&mut self) -> Task<Message> {
        if !self.tree.open || !self.live() {
            return Task::none();
        }
        if self.tree.asking {
            self.tree.stale = true;
            return Task::none();
        }
        self.tree.asking = true;
        self.tree.asked = self.tree.place;
        let open = self.tree.open_ignored();
        self.tree.asked_open = open.clone();
        self.request(Request::Tree { known: self.tree.stamp, place: self.tree.place, open }, Pending::Tree)
    }

    /// An ignored folder was unfolded or folded: ask for a listing with (or without) its contents.
    fn tree_fetch_unfolded(&mut self) -> Task<Message> {
        if self.tree.open_ignored() == self.tree.asked_open {
            return Task::none();
        }
        self.tree_refresh()
    }

    pub(super) fn on_tree(&mut self, result: Result<Response, RpcError>) -> Task<Message> {
        self.tree.asking = false;
        if self.tree.asked != self.tree.place {
            // Asked about the place shown before a switch: ask about this one.
            return self.tree_refresh();
        }
        // A failed query leaves the last known tree on screen.
        if let Ok(Response::Tree(snap)) = result {
            self.tree.apply(snap);
            let visible = self.tree_visible_rows();
            self.tree.clamp();
            if self.tree.cur < self.tree.top || self.tree.cur >= self.tree.top + visible {
                self.tree.follow(visible);
            }
        }
        if std::mem::take(&mut self.tree.stale) {
            return self.tree_refresh();
        }
        Task::none()
    }

    /// Stage or unstage finished.
    pub(super) fn on_tree_git(&mut self, result: Result<Response, RpcError>) -> Task<Message> {
        let failed = match result {
            Ok(Response::GitRan { ok: true, .. }) => None,
            Ok(Response::GitRan { output, .. }) => Some(output.lines().next().unwrap_or("git failed").to_string()),
            Ok(other) => Some(format!("unexpected response: {other:?}")),
            Err(e) => Some(e.to_string()),
        };
        let flash = failed.map_or(Task::none(), |f| self.flash_warn(f));
        Task::batch([flash, self.tree_refresh()])
    }

    /// A create / rename / delete finished.
    pub(super) fn on_tree_fs(
        &mut self,
        reveal: Option<String>,
        renamed: Option<(String, String)>,
        open: bool,
        result: Result<Response, RpcError>,
    ) -> Task<Message> {
        if let Err(e) = result {
            return Task::batch([self.flash_warn(e.msg), self.tree_refresh()]);
        }
        if let Some((from, to)) = renamed {
            self.tabs_renamed(&from, &to);
        }
        let opened = match (&reveal, open) {
            (Some(p), true) => self.open_path(self.tree_abs(p), true),
            _ => Task::none(),
        };
        self.tree.reveal = reveal;
        Task::batch([self.tree_refresh(), opened])
    }

    /// The folder files dropped at `at` go in ("" is the root): the folder under the mouse, a
    /// file's folder, the root below the rows or outside the list (`list`: where the rows are
    /// drawn). Not known where: the cursor row's.
    pub(super) fn tree_drop_dir(&self, at: Option<iced::Point>, list: Option<iced::Rectangle>) -> String {
        let t = &self.tree;
        let rows = t.rows();
        let row = match (at, list) {
            (None, _) => Some(t.cur),
            // The "new" menu and the loading line sit above the rows.
            (Some(p), Some(l)) if l.contains(p) && t.loaded && t.menu != Some(Menu::New) => {
                let top = t.top.min(rows.len().saturating_sub(1));
                // A row's own menu pushes the rows under it down by its height; few drops land there.
                Some(top + ((p.y - l.y) / ROW) as usize)
            }
            _ => None,
        };
        match row.and_then(|r| t.row_entry(rows.get(r))) {
            Some(e) if e.dir && !e.gone => e.path.clone(),
            Some(e) => parent_of(&e.path).to_string(),
            None => String::new(),
        }
    }

    /// Paths in the tree's place for files dropped into `dir`: their own names, numbered when
    /// taken.
    pub(super) fn tree_drop_names(&self, dir: &str, files: &[PathBuf]) -> Vec<(PathBuf, String)> {
        let mut chosen: Vec<String> = Vec::new();
        for f in files {
            let name = f.file_name().map_or_else(|| "dropped".to_string(), |n| n.to_string_lossy().into_owned());
            let p = super::drop::free_name(dir, &name, |p| self.tree.index.contains_key(p) || chosen.iter().any(|c| c == p));
            chosen.push(p);
        }
        files.iter().cloned().zip(chosen).collect()
    }

    /// Show what's in `dir` (and the folders above it) once it arrives.
    pub(super) fn tree_unfold(&mut self, dir: &str) {
        if dir.is_empty() {
            return;
        }
        for d in std::iter::once(dir).chain(ancestors(dir)) {
            self.tree.set_fold(d, false);
        }
    }

    fn tree_abs(&self, rel: &str) -> String {
        let base = self.tree.dir_of(self.tree.place).unwrap_or(&self.root);
        format!("{}/{rel}", base.trim_end_matches('/'))
    }

    /// Show `place` in the tree; the one shown now is parked with its folds and cursor. Asking
    /// for the place already shown goes back to the project.
    fn tree_place(&mut self, place: Place) -> Task<Message> {
        let place = if place == self.tree.place { Place::Project } else { place };
        if place == self.tree.place {
            return Task::none();
        }
        if place != Place::Project && self.tree.dir_of(place).is_none() {
            return self.flash_warn("the daemon has no scratch / NIBDIR (see its log)");
        }
        let mut cur = std::mem::take(&mut self.tree);
        let mut next = match cur.parked.iter().position(|t| t.place == place) {
            Some(i) => cur.parked.remove(i),
            None => Tree { place, ..Default::default() },
        };
        next.parked = std::mem::take(&mut cur.parked);
        next.dirs = cur.dirs.clone();
        next.open = cur.open;
        // A snapshot still on its way belongs to whichever tree is shown when it lands.
        (next.asking, next.asked) = (cur.asking, cur.asked);
        (cur.input, cur.ask, cur.menu, cur.asking) = (None, None, None, false);
        next.parked.push(cur);
        self.tree = next;
        self.focus = Focus::Tree;
        self.tree_refresh()
    }

    /// Open tabs of a renamed file (or of files in a renamed folder) follow it.
    fn tabs_renamed(&mut self, from: &str, to: &str) {
        // Daemon paths, `/`-separated (nib_proto::path): matched and joined as strings, since
        // `PathBuf` would split and join them the local way (`\` on Windows).
        let (from, to) = (self.tree_abs(from), self.tree_abs(to));
        let under = format!("{}/", from.trim_end_matches('/'));
        for i in 0..self.tabs.len() {
            let Some(p) = self.tabs[i].doc.path.as_ref().filter(|_| !self.tabs[i].local).map(|p| p.to_string_lossy().into_owned()) else {
                continue;
            };
            let new = if p == from {
                to.clone()
            } else if let Some(rest) = p.strip_prefix(&under) {
                format!("{}/{rest}", to.trim_end_matches('/'))
            } else {
                continue;
            };
            let new = PathBuf::from(new);
            // The language server knows the buffer by its old path.
            self.lsp_close(i);
            let t = &mut self.tabs[i];
            t.doc.path = Some(new);
            t.lsp_synced = None;
        }
    }

    // -----------------------------------------------------------------------------------------
    // actions
    // -----------------------------------------------------------------------------------------

    /// Show the sidebar with the keyboard in it; from inside it, hide it.
    pub(super) fn toggle_tree(&mut self) -> Task<Message> {
        if !self.tree.open {
            self.tree.open = true;
            self.focus = Focus::Tree;
            return self.tree_refresh();
        }
        if self.focus == Focus::Tree {
            self.close_tree();
        } else {
            self.focus = Focus::Tree;
        }
        Task::none()
    }

    pub(super) fn close_tree(&mut self) {
        self.tree.open = false;
        self.tree.input = None;
        self.tree.ask = None;
        self.tree.menu = None;
        if self.focus == Focus::Tree {
            self.focus = Focus::Main;
        }
    }

    fn tree_visible_rows(&self) -> usize {
        let h = self.settings.window.height().unwrap_or(800.0);
        (((h - CHROME) / ROW).floor() as usize).max(3)
    }

    fn tree_follow(&mut self) {
        let visible = self.tree_visible_rows();
        self.tree.follow(visible);
    }

    fn tree_act(&mut self, act: Act) -> Task<Message> {
        self.tree.menu = None;
        match act {
            Act::Open => return self.tree_open(false),
            Act::Stage => return self.tree_stage(self.tree.targets()),
            Act::Rename => {
                let Some(e) = self.tree.cursor_entry().filter(|e| !e.gone) else { return Task::none() };
                let (path, text) = (e.path.clone(), LineInput::from(e.name()));
                self.tree.selected.clear();
                self.tree.input = Some(Input::Rename { path, text });
            }
            Act::NewFile | Act::NewFolder => self.tree.start_new(act == Act::NewFolder),
            Act::Delete => {
                let t = &self.tree;
                let on_disk = t.targets().into_iter().filter(|p| t.index.get(p).is_some_and(|i| !t.entries[*i].gone));
                let paths = top_level(on_disk.collect());
                if !paths.is_empty() {
                    self.tree.ask = Some(paths);
                }
            }
            Act::Commit => {
                if !self.tree.repo {
                    return self.flash_info("not a git repository");
                }
                if self.tree.counts()[1] == 0 {
                    return self.flash_info("nothing to commit");
                }
                // Committing is the Git panel's: go to its message box (Esc comes back here).
                return self.git_commit_box();
            }
            Act::FoldAll => self.tree.fold_all(true),
            Act::Root | Act::RootUp if self.tree.place != Place::Project => {
                return self.flash_info("only a project folder can become the root (Tab goes round the places, back to the project)");
            }
            Act::Root => {
                let Some(e) = self.tree.cursor_entry() else { return Task::none() };
                let rel = if e.dir { e.path.clone() } else { e.path.rsplit_once('/').map(|(d, _)| d.to_owned()).unwrap_or_default() };
                if rel.is_empty() {
                    return Task::none();
                }
                let abs = format!("{}/{rel}", self.root.trim_end_matches('/'));
                return self.change_root(abs);
            }
            Act::RootUp => {
                let up = match self.root.trim_end_matches('/').rsplit_once('/') {
                    Some(("", _)) => "/".to_owned(),
                    Some((parent, _)) => parent.to_owned(),
                    None => return self.flash_info("already at the top"),
                };
                return self.change_root(up);
            }
        }
        self.tree_follow();
        Task::none()
    }

    /// Enter: open the file (the keyboard follows it into the editor), fold / unfold the folder.
    fn tree_open(&mut self, new_tab: bool) -> Task<Message> {
        let rows = self.tree.rows();
        match rows.get(self.tree.cur) {
            Some(Row::Stub { parent, .. }) => {
                let path = self.tree.entries[*parent].path.clone();
                self.tree_stub(&path);
            }
            Some(Row::Entry { i, .. }) => {
                let e = &self.tree.entries[*i];
                let path = e.path.clone();
                if e.dir {
                    self.tree.toggle_fold(&path);
                } else if e.gone {
                    return self.flash_info(format!("{} is not on disk", e.name()));
                } else {
                    self.focus = Focus::Main;
                    return self.open_path(self.tree_abs(&path), new_tab);
                }
            }
            _ => {}
        }
        self.tree_follow();
        self.tree_fetch_unfolded()
    }

    /// "⋯ n clean" in folder `parent`: show that folder whole, in the files lens.
    fn tree_stub(&mut self, parent: &str) {
        self.tree.lens = Lens::Files;
        self.tree.opened.insert(parent.to_string());
        self.tree.selected.clear();
        self.tree.show(parent);
    }

    /// Stage `paths` if anything in them is not staged yet; unstage them otherwise.
    fn tree_stage(&mut self, paths: Vec<String>) -> Task<Message> {
        if !self.tree.repo {
            return self.flash_info("not a git repository");
        }
        let t = &self.tree;
        let found = || paths.iter().filter_map(|p| t.index.get(p)).map(|i| &t.entries[*i]);
        let op = if found().any(|e| e.unstaged + e.fresh > 0) {
            GitOp::Stage { paths }
        } else if found().any(|e| e.staged > 0) {
            GitOp::Unstage { paths }
        } else {
            return Task::none();
        };
        self.request(Request::GitRun { op }, Pending::TreeGit)
    }

    fn tree_delete(&mut self) -> Task<Message> {
        let Some(paths) = self.tree.ask.take() else { return Task::none() };
        self.tree.selected.clear();
        let place = self.tree.place;
        self.request(Request::Fs { op: FsOp::Delete { paths }, place }, Pending::TreeFs { reveal: None, renamed: None, open: false })
    }

    /// Enter in a name box.
    fn tree_input_done(&mut self) -> Task<Message> {
        let fs = |reveal: &str, renamed, open| Pending::TreeFs { reveal: Some(reveal.to_string()), renamed, open };
        match self.tree.input.take() {
            None => Task::none(),
            Some(Input::Rename { path, text }) => {
                let name = text.trim();
                if name.is_empty() || Some(name) == path.rsplit('/').next() {
                    return Task::none();
                }
                if name.contains('/') || name == "." || name == ".." {
                    return self.flash_warn("a name cannot contain /");
                }
                let to = join(parent_of(&path), name);
                let pending = fs(&to, Some((path.clone(), to.clone())), false);
                let place = self.tree.place;
                self.request(Request::Fs { op: FsOp::Rename { from: path, to }, place }, pending)
            }
            Some(Input::New { dir, folder, text }) => {
                // `a/b.rs` makes the folder too; a trailing slash makes a folder.
                let folder = folder || text.trim().ends_with('/');
                let parts: Vec<&str> = text.split('/').map(str::trim).filter(|p| !p.is_empty()).collect();
                if parts.is_empty() {
                    return Task::none();
                }
                if parts.iter().any(|p| matches!(*p, "." | "..")) {
                    return self.flash_warn("new files go inside the project");
                }
                let path = join(&dir, &parts.join("/"));
                let pending = fs(&path, None, !folder);
                let op = if folder { FsOp::CreateDir { path } } else { FsOp::CreateFile { path } };
                let place = self.tree.place;
                self.request(Request::Fs { op, place }, pending)
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // keys
    // -----------------------------------------------------------------------------------------

    pub(super) fn tree_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        if self.ssh_browsing() {
            return self.ssh_browse_key(chord, text);
        }
        let held = chord.ctrl || chord.logo;
        let cmd = self.keymap.lookup(&chord);
        if let Some(input) = &mut self.tree.input {
            match chord.key {
                Key::Esc => self.tree.input = None,
                Key::Enter => return self.tree_input_done(),
                _ if cmd == Some(Command::Paste) => return iced::clipboard::read().map(|s| Message::Tree(Msg::Pasted(s))),
                _ => drop(input.text_mut().key(&chord, text.as_deref())),
            }
            return Task::none();
        }
        if self.tree.ask.is_some() {
            match chord.key {
                Key::Char('y') if !held => return self.tree_delete(),
                Key::Char('n') | Key::Esc => self.tree.ask = None,
                _ => {}
            }
            return Task::none();
        }
        self.tree.menu = None;
        if held || chord.alt {
            return match cmd {
                Some(Command::OpenInNewTab) => self.tree_open(true),
                Some(c) if app_wide(c) => self.exec(c),
                _ => Task::none(),
            };
        }
        let page = self.tree_visible_rows() as isize - 1;
        let lens = |c: char| LENSES.iter().find(|l| l.1 == c).map(|l| l.0);
        match chord.key {
            Key::Char(c) if !chord.shift && lens(c).is_some() => return self.tree_lens(lens(c).unwrap()),
            Key::Down | Key::Char('j') => self.tree.step(1, chord.shift),
            Key::Up | Key::Char('k') => self.tree.step(-1, chord.shift),
            Key::PageDown => self.tree.step(page, chord.shift),
            Key::PageUp => self.tree.step(-page, chord.shift),
            Key::Home => self.tree.step(isize::MIN / 2, chord.shift),
            Key::End => self.tree.step(isize::MAX / 2, chord.shift),
            Key::Right | Key::Char('l') => {
                if let Some(p) = self.tree.cursor_entry().filter(|e| e.dir).map(|e| e.path.clone()) {
                    self.tree.set_fold(&p, false);
                }
            }
            Key::Left | Key::Char('h') => self.tree.fold_or_parent(),
            Key::Char('z') => self.tree.fold_all(!chord.shift),
            Key::Tab if !chord.shift => {
                let at = PLACES.iter().position(|p| p.0 == self.tree.place).unwrap_or(0);
                let next = PLACES[(at + 1) % PLACES.len()].0;
                let next = if self.tree.dir_of(next).is_none() && next != Place::Project { Place::Project } else { next };
                return self.tree_place(if next == Place::Project { self.tree.place } else { next });
            }
            Key::Enter => return self.tree_open(false),
            Key::Char(' ') => return self.tree_act(Act::Stage),
            Key::Char('m') => return self.tree_act(Act::Commit),
            Key::Char('o') => return self.tree_act(Act::Root),
            Key::Char('-') => return self.tree_act(Act::RootUp),
            Key::F(2) | Key::Char('r') => return self.tree_act(Act::Rename),
            Key::Char('n') => return self.tree_act(if chord.shift { Act::NewFolder } else { Act::NewFile }),
            Key::Char('d') | Key::Delete => return self.tree_act(Act::Delete),
            Key::Esc if !self.tree.selected.is_empty() => self.tree.selected.clear(),
            Key::Esc if !self.tabs.is_empty() => self.focus = Focus::Main,
            _ => {
                if let Some(c) = cmd.filter(|c| app_wide(*c)) {
                    return self.exec(c);
                }
            }
        }
        self.tree_follow();
        self.tree_fetch_unfolded()
    }

    /// Switch lens. Upstream shows what the last fetch knew (fetching is the Git panel's).
    fn tree_lens(&mut self, lens: Lens) -> Task<Message> {
        self.tree.set_lens(lens);
        self.tree_follow();
        Task::none()
    }

    // -----------------------------------------------------------------------------------------
    // mouse
    // -----------------------------------------------------------------------------------------

    pub(super) fn on_tree_msg(&mut self, m: Msg) -> Task<Message> {
        match m {
            Msg::Pasted(s) => {
                if let (Some(s), Some(input)) = (s, &mut self.tree.input) {
                    input.text_mut().insert(s.trim().split(['\n', '\r']).next().unwrap_or(""));
                }
                return Task::none();
            }
            Msg::Scroll(rows) => {
                let t = &mut self.tree;
                t.scroll_acc += rows;
                let whole = t.scroll_acc.trunc();
                t.scroll_acc -= whole;
                let visible = self.tree_visible_rows();
                let t = &mut self.tree;
                let max = t.rows().len().saturating_sub(visible) as f32;
                t.top = (t.top as f32 + whole).clamp(0.0, max) as usize;
                return Task::none();
            }
            _ => {}
        }
        self.focus = Focus::Tree;
        let menu = self.tree.menu.take();
        // A click elsewhere abandons a name being typed.
        if !matches!(m, Msg::Do(_)) {
            self.tree.input = None;
        }
        match m {
            Msg::Click(path) => {
                let rows = self.tree.rows();
                let t = &mut self.tree;
                let Some(at) = rows.iter().position(|r| t.row_entry(Some(r)).is_some_and(|e| e.path == path)) else {
                    return Task::none();
                };
                if self.mods.shift() {
                    t.cur = at;
                    t.select_range(&rows);
                } else if self.mods.command() {
                    // The row the cursor was on joins the selection it starts.
                    if let (true, Some(p)) = (t.selected.is_empty(), t.cursor_path().map(str::to_string)) {
                        t.selected.insert(p);
                    }
                    if !t.selected.remove(&path) {
                        t.selected.insert(path);
                    }
                    t.cur = at;
                    t.anchor = at;
                } else {
                    // One click is the arrow keys; the second is Enter.
                    let double = t.last_click.take().is_some_and(|(p, when)| p == path && when.elapsed() < DOUBLE_CLICK);
                    t.selected.clear();
                    t.cur = at;
                    t.anchor = at;
                    if double {
                        return self.tree_open(false);
                    }
                    t.last_click = Some((path, Instant::now()));
                }
            }
            Msg::Twisty(path) => {
                self.tree.selected.clear();
                self.tree.go_to(&path);
                self.tree.toggle_fold(&path);
            }
            Msg::Gutter(path) => {
                self.tree.go_to(&path);
                return self.tree_stage(vec![path]);
            }
            Msg::Stub(parent) => self.tree_stub(&parent),
            Msg::Context(path) => {
                if !self.tree.selected.contains(&path) {
                    self.tree.selected.clear();
                    self.tree.go_to(&path);
                }
                self.tree.menu = Some(Menu::Row(path));
            }
            Msg::NewMenu => self.tree.menu = (menu != Some(Menu::New)).then_some(Menu::New),
            Msg::Lens(lens) => return self.tree_lens(lens),
            Msg::Place(place) => return self.tree_place(place),
            Msg::Do(act) => return self.tree_act(act),
            Msg::Confirm(true) => return self.tree_delete(),
            Msg::Confirm(false) => self.tree.ask = None,
            Msg::Focus | Msg::Scroll(_) | Msg::Pasted(_) => {}
        }
        self.tree_follow();
        self.tree_fetch_unfolded()
    }

    // -----------------------------------------------------------------------------------------
    // view
    // -----------------------------------------------------------------------------------------

    pub(super) fn tree_view(&self) -> Element<'_, Message> {
        let t = &self.tree;
        let th = &self.theme;
        let (fg, dim, accent, panel, divider, sel_bg) = (th.foreground, th.dim, th.accent, th.panel, th.divider, th.selection);
        let focused = self.focus == Focus::Tree;
        // Picking another root (open folder): the browser stands where the tree is.
        if let Some(browser) = self.ssh_browse_view() {
            let body = container(browser).clip(true).style(move |_| container::Style::default().background(panel));
            return self.pane(body.into(), focused).id(super::drop::TREE).width(WIDTH).into();
        }
        let mono = self.face.font;
        let small = |s: String, c: Color| text(s).size(11).color(c).wrapping(text::Wrapping::None);
        let chip = |label: &str, msg: Msg| {
            button(text(label.to_string()).size(11).color(dim))
                .on_press(Message::Tree(msg))
                .padding([1, 7])
                .style(move |_, st| button::Style {
                    background: matches!(st, button::Status::Hovered).then(|| sel_bg.into()),
                    border: Border { color: divider, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                })
        };

        // Branch and what a push / pull would move.
        let project = self.root.rsplit('/').next().unwrap_or_default().to_string();
        let title = match (&self.git, t.place) {
            (_, Place::Scratch) => "scratch".to_string(),
            (_, Place::Nibdir) => format!("nibdir · {project}"),
            (Some(GitInfo::Repo(r)), _) => r.branch.clone().unwrap_or_else(|| "(detached)".into()),
            _ => project.clone(),
        };
        let mut head = row![text(title).size(12).color(fg).wrapping(text::Wrapping::None)].spacing(8).align_y(Alignment::Center);
        if t.ahead > 0 {
            head = head.push(small(format!("↑{}", t.ahead), GREEN));
        }
        if t.behind > 0 {
            head = head.push(small(format!("↓{}", t.behind), MAGENTA));
        }
        let head = head
            .push(space::horizontal())
            .push(chip("+ new", Msg::NewMenu))
            .push(chip("fold", Msg::Do(Act::FoldAll)))
            .padding(Padding { top: 7.0, bottom: 6.0, left: 10.0, right: 8.0 });

        let counts = t.counts();
        let lenses = row(LENSES.iter().enumerate().filter(|(_, l)| t.repo || l.0 == Lens::Files).map(|(n, (lens, key, label))| {
            let on = *lens == t.lens;
            let mut r = row![text(key.to_string()).size(11).font(mono).color(if on { accent } else { dim }), small(label.to_string(), if on { fg } else { dim })]
                .spacing(5)
                .align_y(Alignment::Center);
            if n > 0 {
                r = r.push(small(counts[n].to_string(), dim));
            }
            button(r)
                .on_press(Message::Tree(Msg::Lens(*lens)))
                .padding([2, 4])
                .style(move |_, st| button::Style {
                    background: (on || matches!(st, button::Status::Hovered)).then(|| divider.into()),
                    border: Border { radius: 4.0.into(), ..Default::default() },
                    ..Default::default()
                })
                .into()
        }))
        .spacing(2)
        .padding(Padding { top: 0.0, bottom: 6.0, left: 6.0, right: 6.0 });

        let rows = t.rows();
        let top = t.top.min(rows.len().saturating_sub(1));
        let mut list = column![];
        if t.menu == Some(Menu::New) {
            list = list.push(self.tree_menu(&[Act::NewFile, Act::NewFolder], false));
        }
        if !t.loaded {
            list = list.push(container(small("reading the project…".into(), dim)).padding([6, 10]));
        }
        // A few rows more than fit: the container clips them.
        for (n, r) in rows.iter().enumerate().skip(top).take(self.tree_visible_rows() + 12) {
            let cur = n == t.cur;
            match *r {
                Row::Entry { i, dim: faded } => {
                    let e = &t.entries[i];
                    list = list.push(self.tree_row(e, faded, cur, focused));
                    if t.menu.as_ref() == Some(&Menu::Row(e.path.clone())) {
                        let many = t.selected.len() > 1;
                        let acts: &[Act] = if many {
                            &[Act::Stage, Act::Delete]
                        } else {
                            &[Act::Open, Act::Rename, Act::NewFile, Act::NewFolder, Act::Stage, Act::Delete, Act::Root, Act::RootUp]
                        };
                        list = list.push(self.tree_menu(acts, many));
                    }
                }
                Row::Stub { parent, count } => {
                    let p = &t.entries[parent];
                    let what = if t.lens == Lens::Changes { "clean" } else { "more" };
                    let label = small(format!("⋯ {count} {what}"), if cur && focused { fg } else { dim });
                    let pad = Padding { left: 30.0 + (p.depth + 1) as f32 * INDENT, ..Padding::ZERO };
                    let b = button(container(label).padding(pad).height(ROW).align_y(Alignment::Center))
                        .on_press(Message::Tree(Msg::Stub(p.path.clone())))
                        .padding(0)
                        .width(Length::Fill)
                        .style(move |_, _| button::Style { background: (cur).then(|| if focused { sel_bg } else { divider }.into()), ..Default::default() });
                    list = list.push(b);
                }
                Row::Input { depth } => {
                    let Some(Input::New { folder, text: typed, .. }) = &t.input else { continue };
                    let hint = if *folder { "folder name" } else { "name, path/name, or folder/" };
                    let pad = Padding { left: 30.0 + depth as f32 * INDENT, right: 8.0, ..Padding::ZERO };
                    list = list.push(container(self.tree_field(typed, hint)).padding(pad).height(ROW).align_y(Alignment::Center));
                }
            }
        }
        if t.loaded && rows.is_empty() {
            list = list.push(container(small("empty folder".into(), dim)).padding([6, 10]));
        }
        let list = mouse_area(container(list).id(super::drop::TREE_LIST).width(Length::Fill).height(Length::Fill).clip(true))
            .on_press(Message::Tree(Msg::Focus))
            .on_scroll(|d| {
                Message::Tree(Msg::Scroll(match d {
                    mouse::ScrollDelta::Lines { y, .. } => -y * 3.0,
                    mouse::ScrollDelta::Pixels { y, .. } => -y / ROW,
                }))
            });

        // What is being asked or typed, then what the keys do.
        let mut foot = column![].spacing(6).padding(Padding { top: 6.0, bottom: 6.0, left: 10.0, right: 8.0 });
        if t.truncated {
            // Wraps: the pane is narrow.
            foot = foot.push(text("big folder: the deepest levels aren't listed (o opens a subfolder as the root)").size(11).color(th.warn).wrapping(text::Wrapping::Word));
        }
        if let Some(paths) = &t.ask {
            let what = match paths.as_slice() {
                [one] => one.rsplit('/').next().unwrap_or(one).to_string(),
                many => format!("{} items", many.len()),
            };
            foot = foot.push(
                row![small(format!("delete {what}?"), th.warn), chip("y  delete", Msg::Confirm(true)), chip("n  keep", Msg::Confirm(false))]
                    .spacing(8)
                    .align_y(Alignment::Center),
            );
        } else if t.selected.len() > 1 {
            foot = foot.push(small(format!("{} selected", t.selected.len()), fg));
        }
        if focused && t.input.is_none() && t.ask.is_none() {
            // Git only means something in the project.
            let first = if t.place == Place::Project {
                row![self.hint("↵", "open"), self.hint("space", "stage"), self.hint("m", "commit"), self.hint("z", "fold all")]
            } else {
                row![self.hint("↵", "open"), self.hint("z", "fold all"), self.hint("⇥", "next root")]
            };
            foot = foot
                .push(first.spacing(10))
                .push(row![self.hint("n", "new"), self.hint("F2", "rename"), self.hint("d", "delete"), self.hint("⇧↑↓", "select")].spacing(10));
            if t.place == Place::Project {
                foot = foot.push(row![self.hint("o", "open as root"), self.hint("-", "up a folder"), self.hint("⇥", "next root")].spacing(10));
            }
        }

        // Roots, bottom left: the active one marked; click (or Tab) to switch.
        let place_row = |place: Place, label: String| -> Element<'_, Message> {
            let on = place == t.place;
            let offered = place == Place::Project || t.dir_of(place).is_some();
            let r = row![
                text(if on { "▸" } else { " " }).size(11).font(mono).color(accent),
                small(label, if on { fg } else if offered { dim } else { divider })
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            let b = button(r).padding([2, 8]).width(Length::Fill).style(move |_, st| button::Style {
                background: (on || matches!(st, button::Status::Hovered)).then(|| divider.into()),
                border: Border { radius: 4.0.into(), ..Default::default() },
                ..Default::default()
            });
            if offered { b.on_press(Message::Tree(Msg::Place(place))) } else { b }.into()
        };
        let mut places = column![].spacing(1).padding(Padding { top: 4.0, bottom: 4.0, left: 4.0, right: 4.0 });
        for (place, label) in PLACES {
            let label = if place == Place::Project { project.clone() } else { label.to_string() };
            places = places.push(place_row(place, label));
        }

        let body = column![head, lenses, self.hrule(), list, self.hrule(), foot, self.hrule(), places];
        // Clipped: nothing drawn past the pane's edge, whatever its rows hold.
        self.pane(container(body).clip(true).style(move |_| container::Style::default().background(panel)).into(), focused).id(super::drop::TREE).width(WIDTH).into()
    }

    fn tree_row<'a>(&'a self, e: &'a Entry, faded: bool, cur: bool, focused: bool) -> Element<'a, Message> {
        let warn = self.theme.warn;
        // Index / work tree, as `git status -s` prints them. A folder shows what it holds.
        let (x, y) = if e.dir {
            let dot = |on: bool| if on { "•".to_string() } else { String::new() };
            ((dot(e.staged > 0), GREEN), (dot(e.unstaged + e.fresh > 0), if e.unstaged > 0 { warn } else { TEAL }))
        } else {
            let ch = |b: u8| if b == b' ' { String::new() } else { (b as char).to_string() };
            let yc = match e.code[1] {
                b'?' => TEAL,
                b'D' => RED,
                _ => warn,
            };
            ((ch(e.code[0]), if e.code[0] == b'?' { TEAL } else { GREEN }), (ch(e.code[1]), yc))
        };
        self.tree_row_with(e, x, y, faded, cur, focused)
    }

    fn tree_row_with<'a>(
        &'a self,
        e: &'a Entry,
        x: (String, Color),
        y: (String, Color),
        faded: bool,
        cur: bool,
        focused: bool,
    ) -> Element<'a, Message> {
        let t = &self.tree;
        let th = &self.theme;
        let mono = self.face.font;
        let (fg, dim, warn, sel_bg, divider) = (th.foreground, th.dim, th.warn, th.selection, th.divider);
        let fade = |c: Color| if faded || e.ignored { Color { a: 0.4, ..c } } else { c };
        let small = |s: String, c: Color| text(s).size(11).color(fade(c)).wrapping(text::Wrapping::None);
        let flat = |_: &iced::Theme, _: button::Status| button::Style::default();
        let path = || e.path.clone();

        let letters = row![
            text(x.0).size(11).font(mono).color(fade(x.1)).width(9),
            text(y.0).size(11).font(mono).color(fade(y.1)).width(9)
        ]
        .height(ROW)
        .align_y(Alignment::Center);
        let gutter: Element<'_, Message> = if e.dirty > 0 {
            button(letters).on_press(Message::Tree(Msg::Gutter(path()))).padding(0).style(flat).into()
        } else {
            letters.into()
        };
        let twisty: Element<'_, Message> = if e.dir {
            let mark = if t.expanded(e) { "▾" } else { "▸" };
            button(container(text(mark).size(11).color(fade(dim))).center_x(14).height(ROW).align_y(Alignment::Center))
                .on_press(Message::Tree(Msg::Twisty(path())))
                .padding(0)
                .style(flat)
                .into()
        } else {
            space().width(14).into()
        };

        let color = if e.dir {
            fg
        } else if e.gone {
            RED
        } else if e.fresh > 0 {
            TEAL
        } else if e.unstaged > 0 {
            warn
        } else if e.staged > 0 {
            GREEN
        } else {
            fg
        };
        let name: Element<'_, Message> = match &t.input {
            Some(Input::Rename { path, text: typed }) if *path == e.path => self.tree_field(typed, "new name"),
            _ => {
                let label = if e.dir { format!("{}/", e.name()) } else { e.name().to_string() };
                container(text(label).size(12).font(mono).color(fade(color)).wrapping(text::Wrapping::None)).clip(true).width(Length::Fill).into()
            }
        };
        let mut main = row![name].spacing(7).align_y(Alignment::Center).height(ROW);
        if e.upstream > 0 {
            // Changed upstream and here: a pull will have to merge it.
            let clash = e.dirty > 0 && !e.dir;
            let mark = if e.dir { format!("↓{}", e.upstream) } else if clash { "↓ edited here too".into() } else { "↓".into() };
            main = main.push(small(mark, if clash { RED } else { MAGENTA }));
        }
        if e.dir {
            for (n, c) in [(e.staged, GREEN), (e.unstaged, warn), (e.fresh, TEAL)] {
                if n > 0 {
                    main = main.push(small(n.to_string(), c));
                }
            }
        }
        let main = button(main).on_press(Message::Tree(Msg::Click(path()))).padding(Padding { right: 8.0, ..Padding::ZERO }).width(Length::Fill).style(flat);

        let selected = t.selected.contains(&e.path);
        let bg = match (cur, selected) {
            (true, _) if focused => Some(sel_bg),
            (true, _) => Some(divider),
            (false, true) => Some(Color { a: 0.5, ..sel_bg }),
            _ => None,
        };
        let line = row![gutter, space().width(e.depth as f32 * INDENT), twisty, main].align_y(Alignment::Center);
        let line = container(line).padding(Padding { left: 8.0, ..Padding::ZERO }).style(move |_| container::Style { background: bg.map(Into::into), ..Default::default() });
        mouse_area(line).on_right_press(Message::Tree(Msg::Context(path()))).into()
    }

    /// A one-line text box: what was typed, then the caret.
    fn tree_field<'a>(&self, typed: &LineInput, hint: &str) -> Element<'a, Message> {
        let th = &self.theme;
        let accent = th.accent;
        let typed_caret = typed.spans(th.foreground, accent);
        let mut line = row![rich_text(typed_caret).size(12).font(self.face.font).wrapping(text::Wrapping::None)].align_y(Alignment::Center);
        if typed.is_empty() {
            line = line.push(text(format!(" {hint}")).size(11).color(th.dim).wrapping(text::Wrapping::None));
        }
        container(super::tail_line(line))
            .padding([1, 5])
            .width(Length::Fill)
            .clip(true)
            .style(move |_| container::Style { border: Border { color: accent, width: 1.0, radius: 3.0.into() }, ..Default::default() })
            .into()
    }

    /// The actions of a right click (or of the "new" button), listed under the row.
    fn tree_menu<'a>(&self, acts: &[Act], many: bool) -> Element<'a, Message> {
        let th = &self.theme;
        let (fg, dim, sel_bg, divider) = (th.foreground, th.dim, th.selection, th.divider);
        let n = self.tree.selected.len();
        let items = acts.iter().map(|act| {
            let (label, key) = match act {
                Act::Open => ("Open", "↵"),
                Act::Rename => ("Rename", "F2"),
                Act::NewFile => ("New file", "n"),
                Act::NewFolder => ("New folder", "N"),
                Act::Stage => ("Stage / unstage", "space"),
                Act::Delete => ("Delete", "d"),
                Act::Commit => ("Commit", "m"),
                Act::FoldAll => ("Fold all", "z"),
                Act::Root => ("Open as root", "o"),
                Act::RootUp => ("Root: up a folder", "-"),
            };
            let label = if many && matches!(act, Act::Stage | Act::Delete) { format!("{label} {n}") } else { label.to_string() };
            button(row![text(label).size(12).color(fg), space::horizontal(), text(key).size(11).color(dim)].align_y(Alignment::Center))
                .on_press(Message::Tree(Msg::Do(*act)))
                .padding([2, 8])
                .width(Length::Fill)
                .style(move |_, st| button::Style {
                    background: matches!(st, button::Status::Hovered).then(|| sel_bg.into()),
                    border: Border { radius: 3.0.into(), ..Default::default() },
                    ..Default::default()
                })
                .into()
        });
        let bg = th.background;
        container(container(column(items)).padding(3).width(200).style(move |_| container::Style {
            background: Some(bg.into()),
            border: Border { color: divider, width: 1.0, radius: 5.0.into() },
            ..Default::default()
        }))
        .padding(Padding { left: 30.0, top: 2.0, bottom: 4.0, right: 0.0 })
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib_proto::{ReqId, ServerMsg};

    fn entry(path: &str, dir: bool) -> TreeEntry {
        TreeEntry { path: path.into(), dir, ignored: path == "target" }
    }
    fn change(code: &str, path: &str) -> GitChange {
        GitChange { path: path.into(), code: code.into(), staged: !code.starts_with([' ', '?']) }
    }
    fn snapshot() -> TreeSnapshot {
        TreeSnapshot {
            stamp: 1,
            entries: Some(vec![
                entry("README.md", false),
                entry("src", true),
                entry("target", true),
                entry("docs", true),
                entry("docs/guide.md", false),
                entry("src/app", true),
                entry("src/main.rs", false),
                entry("src/lib.rs", false),
                entry("src/app/tree.rs", false),
                entry("src/app/git.rs", false),
            ]),
            truncated: false,
            repo: true,
            changes: vec![change(" M", "src/main.rs"), change("??", "src/app/tree.rs"), change("M ", "README.md"), change(" D", "src/old.rs")],
            upstream: vec!["README.md".into()],
            ahead: 2,
            behind: 1,
        }
    }
    /// App with the tree open, loaded and focused.
    fn app() -> App {
        let mut a = super::super::tests::app();
        a.tree.open = true;
        a.focus = Focus::Tree;
        let _ = a.on_tree(Ok(Response::Tree(snapshot())));
        a
    }
    fn key(a: &mut App, s: &str) {
        let chord = KeyChord::parse(s).unwrap();
        let text = match chord.key {
            Key::Char(c) if !(chord.ctrl || chord.logo) => Some(c.to_string()),
            _ => None,
        };
        let _ = a.update(Message::Key { chord, text, repeat: false });
        let _ = a.update(Message::KeyUp(chord.key));
    }
    fn type_str(a: &mut App, s: &str) {
        for c in s.chars() {
            let _ = a.update(Message::Key { chord: KeyChord::new(Key::Char(c)), text: Some(c.to_string()), repeat: false });
        }
    }
    /// What the rows show: names, indented; stubs as "⋯n"; dimmed rows in parentheses.
    fn shown(a: &App) -> Vec<String> {
        let t = &a.tree;
        t.rows()
            .iter()
            .map(|r| match *r {
                Row::Entry { i, dim } => {
                    let e = &t.entries[i];
                    let s = format!("{}{}", "  ".repeat(e.depth), e.name());
                    if dim { format!("({s})") } else { s }
                }
                Row::Stub { parent, count } => format!("{}⋯{count}", "  ".repeat(t.entries[parent].depth + 1)),
                Row::Input { depth } => format!("{}?", "  ".repeat(depth)),
            })
            .collect()
    }
    fn cursor(a: &App) -> String {
        a.tree.cursor_path().unwrap_or("-").to_string()
    }
    /// The outstanding request of a kind.
    fn sent(a: &App, want: impl Fn(&Pending) -> bool) -> ReqId {
        *a.pending.iter().find(|(_, p)| want(p)).expect("no such request").0
    }

    #[test]
    fn lenses_keep_the_top_level_and_fold_the_rest() {
        let mut a = app();
        // Files: folders first, case-insensitive, all folded.
        assert_eq!(shown(&a), ["docs", "src", "target", "README.md"]);
        assert_eq!(a.tree.counts(), [6, 4, 1, 1]);
        key(&mut a, "c");
        assert_eq!(
            shown(&a),
            ["(docs)", "src", "  app", "    tree.rs", "    ⋯1", "  main.rs", "  old.rs", "  ⋯1", "(target)", "README.md"]
        );
        key(&mut a, "s");
        assert_eq!(shown(&a), ["(docs)", "(src)", "(target)", "README.md"]);
        key(&mut a, "u");
        assert_eq!(shown(&a), ["(docs)", "(src)", "(target)", "README.md"]);
        assert_eq!(cursor(&a), "docs");

        // Fold all, in a git lens: down to the top level. Unfold all brings it back.
        key(&mut a, "c");
        for _ in 0..3 {
            key(&mut a, "j");
        }
        assert_eq!(cursor(&a), "src/app/tree.rs");
        key(&mut a, "z");
        assert_eq!(shown(&a), ["(docs)", "src", "(target)", "README.md"]);
        assert_eq!(cursor(&a), "src", "the cursor stays on its branch");
        key(&mut a, "shift+z");
        assert_eq!(shown(&a).len(), 10);

        // Enter on "⋯ n clean" shows that folder whole.
        a.tree.go_to("src/app/tree.rs");
        key(&mut a, "j");
        key(&mut a, "enter");
        assert_eq!(a.tree.lens, Lens::Files);
        assert_eq!(cursor(&a), "src/app");
        assert_eq!(shown(&a), ["docs", "src", "  app", "    git.rs", "    tree.rs", "  lib.rs", "  main.rs", "target", "README.md"]);
        // ← folds, ← again goes to the parent; nothing goes above the root.
        key(&mut a, "left");
        key(&mut a, "left");
        assert_eq!(cursor(&a), "src");
        key(&mut a, "left");
        key(&mut a, "left");
        assert_eq!((cursor(&a), shown(&a).len()), ("src".into(), 4));
    }

    #[test]
    fn selection_staging_and_commit() {
        let mut a = app();
        key(&mut a, "c");
        a.tree.go_to("src/app/tree.rs");
        key(&mut a, "shift+down");
        key(&mut a, "shift+down");
        // The stub between them is not a file.
        let mut picked: Vec<_> = a.tree.selected.iter().cloned().collect();
        picked.sort();
        assert_eq!(picked, ["src/app/tree.rs", "src/main.rs"]);
        key(&mut a, "space");
        let id = sent(&a, |p| matches!(p, Pending::TreeGit));
        let _ = a.on_server(nib_proto::ServerMsg::Response { id, result: Ok(Response::GitRan { ok: true, output: String::new() }) });
        key(&mut a, "esc");
        assert!(a.tree.selected.is_empty());
        assert_eq!(a.focus, Focus::Tree, "esc cleared the selection first");

        // Cmd-click adds a row to the one the cursor is on, and takes it out again.
        a.tree.go_to("README.md");
        let _ = a.update(Message::Mods(iced::keyboard::Modifiers::COMMAND));
        let _ = a.update(Message::Tree(Msg::Click("src/main.rs".into())));
        assert_eq!(a.tree.selected.len(), 2);
        let _ = a.update(Message::Tree(Msg::Click("src/main.rs".into())));
        assert_eq!(a.tree.selected.len(), 1);
        let _ = a.update(Message::Mods(iced::keyboard::Modifiers::default()));

        // A single click only moves the cursor; the second one opens.
        let _ = a.update(Message::Tree(Msg::Click("src/main.rs".into())));
        assert_eq!((cursor(&a), a.focus), ("src/main.rs".into(), Focus::Tree));
        assert!(!a.pending.values().any(|p| matches!(p, Pending::Open { .. })));
        let _ = a.update(Message::Tree(Msg::Click("src/main.rs".into())));
        assert!(a.pending.values().any(|p| matches!(p, Pending::Open { path, .. } if path == "/tmp/src/main.rs")));
        assert_eq!(a.focus, Focus::Main);

        // m jumps to the Git panel's commit box; what is typed there is the message, not lens keys.
        a.git = Some(GitInfo::Repo(nib_proto::GitRepo { name: "p".into(), branch: None, user_name: None, user_email: None, last_commit: None }));
        a.focus = Focus::Tree;
        key(&mut a, "m");
        assert_eq!(a.focus, Focus::Git);
        type_str(&mut a, "fix sc");
        key(&mut a, "backspace");
        assert_eq!(a.tree.lens, Lens::Changes);
        key(&mut a, "enter");
        let commit = |p: &Pending| matches!(p, Pending::GitRun { .. });
        assert!(a.pending.values().any(commit));
        // Esc leaves the panel for where m was pressed.
        key(&mut a, "esc");
        assert_eq!(a.focus, Focus::Tree);

        // Looking at upstream never fetches by itself.
        let before = a.pending.len();
        key(&mut a, "u");
        assert_eq!((a.tree.lens, a.pending.len()), (Lens::Upstream, before));
    }

    #[test]
    fn new_rename_and_delete() {
        let mut a = app();
        // n on a folder: the name box is its first row, in the files lens.
        key(&mut a, "c");
        a.tree.go_to("src");
        key(&mut a, "n");
        assert_eq!(a.tree.lens, Lens::Files);
        assert_eq!(shown(&a)[..3], ["docs", "src", "  ?"]);
        type_str(&mut a, "ui/panel.rs");
        key(&mut a, "enter");
        let id = sent(&a, |p| matches!(p, Pending::TreeFs { open: true, .. }));
        assert!(matches!(a.pending.get(&id), Some(Pending::TreeFs { reveal: Some(p), .. }) if p == "src/ui/panel.rs"));
        let _ = a.on_server(nib_proto::ServerMsg::Response { id, result: Ok(Response::Ok) });
        // The next snapshot has it: the cursor lands there, its folders unfolded.
        let mut snap = snapshot();
        snap.stamp = 2;
        snap.entries.as_mut().unwrap().extend([entry("src/ui", true), entry("src/ui/panel.rs", false)]);
        let _ = a.on_tree(Ok(Response::Tree(snap.clone())));
        assert_eq!(cursor(&a), "src/ui/panel.rs");

        // Shift+N makes a folder; so does a trailing slash.
        a.focus = Focus::Tree;
        key(&mut a, "shift+n");
        assert!(matches!(&a.tree.input, Some(Input::New { dir, folder: true, .. }) if dir == "src/ui"));
        key(&mut a, "esc");
        assert!(a.tree.input.is_none());

        // F2 renames in place; an open tab follows the file.
        let doc = nib_editor::Document::new(Default::default(), Some(PathBuf::from("/tmp/src/main.rs")));
        a.put_doc(doc, true);
        a.focus = Focus::Tree;
        a.tree.go_to("src/main.rs");
        key(&mut a, "f2");
        assert!(matches!(&a.tree.input, Some(Input::Rename { text, .. }) if text.as_str() == "main.rs"));
        for _ in 0.."main.rs".len() {
            key(&mut a, "backspace");
        }
        type_str(&mut a, "start.rs");
        key(&mut a, "enter");
        let id = sent(&a, |p| matches!(p, Pending::TreeFs { renamed: Some(_), .. }));
        let _ = a.on_server(nib_proto::ServerMsg::Response { id, result: Ok(Response::Ok) });
        assert_eq!(a.tabs[0].doc.path.as_deref(), Some(std::path::Path::new("/tmp/src/start.rs")));

        // d asks first; a folder and a file inside it are one deletion.
        a.tree.go_to("src/app");
        key(&mut a, "right");
        key(&mut a, "shift+down");
        key(&mut a, "d");
        assert_eq!(a.tree.ask.as_deref(), Some(&["src/app".to_string()][..]));
        key(&mut a, "n");
        assert!(a.tree.ask.is_none() && a.tree.input.is_none(), "n answered the question, it did not start a new file");
        // Backspace, then Enter (from muscle memory) deletes nothing: only d / Delete ask, only y answers.
        key(&mut a, "backspace");
        assert!(a.tree.ask.is_none(), "Backspace does not start a delete");
        key(&mut a, "d");
        key(&mut a, "enter");
        assert!(a.tree.ask.is_some(), "Enter does not confirm a delete");
        key(&mut a, "y");
        assert!(a.pending.values().any(|p| matches!(p, Pending::TreeFs { reveal: None, renamed: None, .. })));
    }

    #[test]
    fn the_tree_is_the_first_pane() {
        let mut a = app();
        assert_eq!(a.focus_ring()[..2], [Focus::Tree, Focus::Main]);
        a.focus = Focus::Main;
        a.tree.open = false;
        assert_eq!(a.focus_ring()[0], Focus::Main);
        // The files command shows it and takes the keyboard there; again from inside hides it.
        let _ = a.exec(Command::ToggleExplorer);
        assert!(a.tree.open && a.focus == Focus::Tree);
        a.focus = Focus::Main;
        let _ = a.exec(Command::ToggleExplorer);
        assert!(a.tree.open && a.focus == Focus::Tree);
        let _ = a.exec(Command::ToggleExplorer);
        assert!(!a.tree.open && a.focus == Focus::Main);
    }

    #[test]
    fn scratch_and_nibdir_are_places_with_their_own_trees() {
        let mut a = app();
        a.link = crate::app::Link::Live;
        a.tree.set_places("/s".into(), "/n".into());
        a.tree.opened.insert("docs".into());
        let project = shown(&a);
        assert!(project.contains(&"  guide.md".to_string()), "{project:?}");
        let answer = |a: &mut App, entries: Vec<TreeEntry>| {
            let id = sent(a, |p| matches!(p, Pending::Tree));
            let snap = TreeSnapshot { stamp: 9, entries: Some(entries), ..Default::default() };
            let _ = a.on_server(ServerMsg::Response { id, result: Ok(Response::Tree(snap)) });
        };

        // Tab: scratch. Its tree is its own; paths are relative to it.
        key(&mut a, "tab");
        assert_eq!((a.tree.place, a.tree.asked), (Place::Scratch, Place::Scratch));
        answer(&mut a, vec![entry("try.py", false)]);
        assert_eq!(shown(&a), ["try.py"]);
        assert_eq!(a.tree_abs("try.py"), "/s/try.py");

        // Tab: NIBDIR, switched to before its answer; a late answer about scratch would be ignored.
        key(&mut a, "tab");
        assert_eq!(a.tree.place, Place::Nibdir);
        answer(&mut a, vec![entry("plots", true), entry("plots/loss.png", false)]);
        assert_eq!(shown(&a), ["plots"]);
        assert_eq!(a.tree_abs("plots/loss.png"), "/n/plots/loss.png");

        // Tab: back to the project, folds and all.
        key(&mut a, "tab");
        assert_eq!(a.tree.place, Place::Project);
        assert_eq!(shown(&a), project);
        assert_eq!(a.tree_abs("src"), "/tmp/src");

        // Clicking the place shown goes back to the project; one the daemon didn't make is refused.
        let _ = a.update(Message::Tree(Msg::Place(Place::Scratch)));
        assert_eq!((a.tree.place, shown(&a)), (Place::Scratch, vec!["try.py".to_string()]));
        let _ = a.update(Message::Tree(Msg::Place(Place::Scratch)));
        assert_eq!(a.tree.place, Place::Project);
        a.tree.set_places(String::new(), String::new());
        let _ = a.update(Message::Tree(Msg::Place(Place::Nibdir)));
        assert_eq!(a.tree.place, Place::Project);
    }

    #[test]
    fn an_answer_about_the_previous_place_is_not_shown() {
        let mut a = app();
        a.link = crate::app::Link::Live;
        a.tree.set_places("/s".into(), "/n".into());
        key(&mut a, "tab");
        let asked_scratch = sent(&a, |p| matches!(p, Pending::Tree));
        key(&mut a, "tab");
        assert_eq!(a.tree.place, Place::Nibdir);
        let snap = TreeSnapshot { stamp: 3, entries: Some(vec![entry("try.py", false)]), ..Default::default() };
        let _ = a.on_server(ServerMsg::Response { id: asked_scratch, result: Ok(Response::Tree(snap)) });
        assert!(shown(&a).is_empty(), "scratch's listing must not appear in nibdir");
        assert!(a.pending.values().any(|p| matches!(p, Pending::Tree)), "nibdir is asked about again");
    }
}
