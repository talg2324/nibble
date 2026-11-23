//! Files dragged onto the window from the desktop (D-052). Dropped on the file tree they are
//! copied into the folder under the mouse, on the daemon's machine (so over SSH too). Dropped on
//! the agents chat, pictures go with the next message like pasted ones, and text files are
//! attached as `@` mentions, read here.
//!
//! winit says what was dropped but not where. On macOS nib asks AppKit where the mouse is and
//! looks up where the tree and the main pane are drawn; elsewhere it goes by which pane has the
//! keyboard.

use super::{ai, App, Focus, Message, Pending};
use iced::advanced::widget::operation::{Operation, Outcome};
use iced::advanced::widget::Id;
use iced::{Point, Rectangle, Task};
use nib_proto::{FsOp, Place, PutFile, Request};
use std::path::{Path, PathBuf};

/// Widget ids of what a drop can land on.
pub(super) const TREE: &str = "nib-drop-tree";
pub(super) const TREE_LIST: &str = "nib-drop-tree-list";
pub(super) const MAIN: &str = "nib-drop-main";

/// Pictures the chat takes (anything `image` decodes; a GIF sends its first frame).
const PICTURES: [&str; 9] = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico"];
/// Most of a text file attached to the chat (the daemon cuts each to 100 KiB anyway).
const TEXT_MAX: u64 = 400 * 1024;
/// Most one drop on the tree copies.
const COPY_MAX_BYTES: u64 = 1 << 30;
const COPY_MAX_FILES: usize = 20_000;
/// One `FsOp::Put` carries about this much (frames stop at 512 MiB).
const BATCH_BYTES: usize = 64 << 20;

#[derive(Debug, Clone)]
pub enum Msg {
    /// A file is being dragged over the window.
    Hovered,
    /// The drag left without dropping.
    Left,
    /// One dropped file (several come one after another).
    File(PathBuf),
    /// Where the mouse was when they dropped and where the panes are.
    Located(Option<Point>, Panes),
    /// Files read for the tree.
    Read(Result<Copy, String>),
}

/// Bounds of the panes a drop can land on, as drawn.
#[derive(Debug, Clone, Default)]
pub struct Panes {
    tree: Option<Rectangle>,
    list: Option<Rectangle>,
    main: Option<Rectangle>,
}

/// What a drop on the tree writes, read from this machine.
#[derive(Debug, Clone)]
pub struct Copy {
    place: Place,
    dirs: Vec<String>,
    files: Vec<PutFile>,
    /// The first dropped item, to put the cursor on.
    reveal: String,
}

#[derive(Default)]
pub struct Drops {
    /// Dropped, waiting for where.
    files: Vec<PathBuf>,
    locating: bool,
    hovering: bool,
}

/// Where dropped files go.
enum Target {
    /// Into this folder of the tree's place ("" is its root).
    Tree(String),
    Chat,
    Nowhere,
}

impl App {
    pub(super) fn on_drop(&mut self, m: Msg) -> Task<Message> {
        match m {
            Msg::Hovered if !self.drops.hovering => {
                self.drops.hovering = true;
                return self.flash_info("drop on the file tree to copy it there, or on the agents chat to attach it");
            }
            Msg::Hovered => {}
            Msg::Left => self.drops.hovering = false,
            Msg::File(path) => {
                self.drops.hovering = false;
                self.drops.files.push(path);
                if !self.drops.locating {
                    self.drops.locating = true;
                    return locate();
                }
            }
            Msg::Located(at, panes) => {
                self.drops.locating = false;
                let files = std::mem::take(&mut self.drops.files);
                let target = self.drop_target(at, &panes);
                return self.drop_on(target, files);
            }
            Msg::Read(Err(e)) => return self.flash_warn(e),
            Msg::Read(Ok(copy)) => return self.tree_put(copy),
        }
        Task::none()
    }

    /// `nib ctl drop`: as if `files` were dropped on the tree's folder `dir` or on the chat.
    pub(super) fn drop_to(&mut self, chat: bool, dir: &str, files: Vec<PathBuf>) -> Task<Message> {
        let target = if chat { Target::Chat } else { Target::Tree(dir.trim_matches('/').to_string()) };
        self.drop_on(target, files)
    }

    fn drop_target(&self, at: Option<Point>, panes: &Panes) -> Target {
        let over = |r: Option<Rectangle>| at.zip(r).is_some_and(|(p, r)| r.contains(p));
        let chat = self.ai.shown() && !self.ai_history_front();
        match at {
            Some(p) if self.tree.open && over(panes.tree) => Target::Tree(self.tree_drop_dir(Some(p), panes.list)),
            Some(_) if chat && over(panes.main) => Target::Chat,
            Some(_) => Target::Nowhere,
            // Not known where: by focus.
            None if self.tree.open && self.focus == Focus::Tree => Target::Tree(self.tree_drop_dir(None, None)),
            None if chat => Target::Chat,
            None if self.tree.open => Target::Tree(self.tree_drop_dir(None, None)),
            None => Target::Nowhere,
        }
    }

    fn drop_on(&mut self, target: Target, files: Vec<PathBuf>) -> Task<Message> {
        if files.is_empty() {
            return Task::none();
        }
        match target {
            Target::Nowhere => self.flash_info("drop files on the file tree (copies them) or on the agents chat (attaches them)"),
            Target::Tree(_) if !self.live() => self.flash_warn(self.not_connected()),
            Target::Tree(dir) => {
                let items = self.tree_drop_names(&dir, &files);
                self.tree_unfold(&dir);
                let place = self.tree.place;
                Task::perform(async move { read_copy(place, items) }, |r| Message::Drop(Msg::Read(r)))
            }
            Target::Chat => {
                let local = self.label == "local";
                Task::batch(files.into_iter().map(|path| chat_file(path, local)))
            }
        }
    }

    /// Send a read drop to the daemon, in batches small enough for one frame each.
    fn tree_put(&mut self, copy: Copy) -> Task<Message> {
        let Copy { place, dirs, files, reveal } = copy;
        let mut batches: Vec<Vec<PutFile>> = vec![Vec::new()];
        let mut size = 0;
        for f in files {
            if size + f.data.len() > BATCH_BYTES && !batches.last().is_some_and(Vec::is_empty) {
                batches.push(Vec::new());
                size = 0;
            }
            size += f.data.len();
            batches.last_mut().expect("never empty").push(f);
        }
        let n = batches.len();
        let mut dirs = Some(dirs);
        let tasks: Vec<_> = batches
            .into_iter()
            .enumerate()
            .map(|(i, files)| {
                let op = FsOp::Put { dirs: dirs.take().unwrap_or_default(), files };
                let reveal = (i + 1 == n).then(|| reveal.clone());
                self.request(Request::Fs { op, place }, Pending::TreeFs { reveal, renamed: None, open: false })
            })
            .collect();
        Task::batch(tasks)
    }
}

/// One file dropped on the chat: a picture is decoded for the message, a text file read and
/// mentioned, a folder mentioned when the daemon can see it (this machine).
fn chat_file(path: PathBuf, local: bool) -> Task<Message> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if PICTURES.contains(&ext.as_str()) {
        return Task::perform(async move { ai::Pic::from_file(&path) }, |r| Message::Ai(ai::Msg::DroppedPic(r)));
    }
    let shown = path.display().to_string();
    if path.is_dir() {
        // Mentioned by its path: the daemon lists it, so it has to be on this machine.
        let r = if local && !shown.contains(char::is_whitespace) {
            Ok((format!("{}/", shown.trim_end_matches('/')), None))
        } else {
            Err(format!("{shown}: folders can be attached only on a local connection, and without spaces in the path"))
        };
        return Task::done(Message::Ai(ai::Msg::DroppedText(r)));
    }
    Task::perform(async move { read_text(&path) }, |r| Message::Ai(ai::Msg::DroppedText(r)))
}

/// A text file's `@` name and contents. The name is its path when that has no spaces (a mention
/// ends at one), else its file name with `_` for them.
fn read_text(path: &Path) -> Result<(String, Option<String>), String> {
    let shown = path.display().to_string();
    let len = std::fs::metadata(path).map_err(|e| format!("{shown}: {e}"))?.len();
    if len > TEXT_MAX {
        return Err(format!("{shown}: too big to attach ({} KiB; at most {} KiB)", len / 1024, TEXT_MAX / 1024));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{shown}: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{shown}: not a text file or a picture; only those can be attached"))?;
    let name = if shown.contains(char::is_whitespace) {
        path.file_name().map_or_else(|| "dropped".to_string(), |n| n.to_string_lossy().replace(char::is_whitespace, "_"))
    } else {
        shown
    };
    Ok((name, Some(text)))
}

/// Read what was dropped on the tree: `items` are (file on this machine, path it gets in the
/// place). Folders come with everything in them; links inside them are skipped.
fn read_copy(place: Place, items: Vec<(PathBuf, String)>) -> Result<Copy, String> {
    let mut copy = Copy { place, dirs: Vec::new(), files: Vec::new(), reveal: items.first().map(|i| i.1.clone()).unwrap_or_default() };
    let mut total = 0u64;
    let mut stack: Vec<(PathBuf, String)> = items.into_iter().rev().collect();
    while let Some((src, dst)) = stack.pop() {
        let shown = src.display().to_string();
        let meta = std::fs::metadata(&src).map_err(|e| format!("{shown}: {e}"))?;
        if meta.is_dir() {
            copy.dirs.push(dst.clone());
            let mut kids: Vec<_> = std::fs::read_dir(&src).map_err(|e| format!("{shown}: {e}"))?.filter_map(Result::ok).collect();
            kids.sort_by_key(|k| std::cmp::Reverse(k.file_name()));
            for k in kids {
                if k.file_type().is_ok_and(|t| !t.is_symlink()) {
                    stack.push((k.path(), format!("{dst}/{}", k.file_name().to_string_lossy())));
                }
            }
        } else if meta.is_file() {
            total += meta.len();
            if total > COPY_MAX_BYTES || copy.files.len() >= COPY_MAX_FILES {
                return Err(format!("too much to copy at once (at most {} files, {} MiB)", COPY_MAX_FILES, COPY_MAX_BYTES >> 20));
            }
            let data = std::fs::read(&src).map_err(|e| format!("{shown}: {e}"))?;
            copy.files.push(PutFile { path: dst, data });
        }
        if copy.dirs.len() + copy.files.len() > COPY_MAX_FILES {
            return Err(format!("too much to copy at once (at most {COPY_MAX_FILES} files)"));
        }
    }
    Ok(copy)
}

/// Where the mouse is and where the panes are drawn.
fn locate() -> Task<Message> {
    let mouse = iced::window::latest().then(|id| match id {
        Some(id) => iced::window::run(id, mouse_in),
        None => Task::done(None),
    });
    mouse.then(|at| iced::advanced::widget::operate(FindPanes::default()).map(move |panes| Message::Drop(Msg::Located(at, panes))))
}

/// The mouse in the window's logical coordinates (AppKit points are those).
#[cfg(target_os = "macos")]
fn mouse_in(w: &dyn iced::window::Window) -> Option<Point> {
    use raw_window_handle::RawWindowHandle;
    let RawWindowHandle::AppKit(h) = w.window_handle().ok()?.as_raw() else { return None };
    // SAFETY: winit's view, alive while the window is; this runs on the main thread.
    let view: &objc2_app_kit::NSView = unsafe { h.ns_view.cast().as_ref() };
    let p = view.convertPoint_fromView(view.window()?.mouseLocationOutsideOfEventStream(), None);
    let y = if view.isFlipped() { p.y } else { view.bounds().size.height - p.y };
    Some(Point::new(p.x as f32, y as f32))
}

#[cfg(not(target_os = "macos"))]
fn mouse_in(_: &dyn iced::window::Window) -> Option<Point> {
    None
}

#[derive(Default)]
struct FindPanes(Panes);

impl Operation<Panes> for FindPanes {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Panes>)) {
        operate(self);
    }

    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        let Some(id) = id else { return };
        let slot = if *id == Id::new(TREE) {
            &mut self.0.tree
        } else if *id == Id::new(TREE_LIST) {
            &mut self.0.list
        } else if *id == Id::new(MAIN) {
            &mut self.0.main
        } else {
            return;
        };
        *slot = Some(bounds);
    }

    fn finish(&self) -> Outcome<Panes> {
        Outcome::Some(self.0.clone())
    }
}

/// `name` in `dir`, or `name 2`, `name 3`… (before the extension) if that is taken.
pub(super) fn free_name(dir: &str, name: &str, taken: impl Fn(&str) -> bool) -> String {
    let join = |n: &str| if dir.is_empty() { n.to_string() } else { format!("{dir}/{n}") };
    let first = join(name);
    if !taken(&first) {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (2..).map(|n| join(&format!("{stem} {n}{ext}"))).find(|p| !taken(p)).expect("some number is free")
}

#[cfg(test)]
mod tests {
    use super::free_name;

    #[test]
    fn taken_names_get_a_number() {
        let taken = |p: &str| ["a.png", "src/a.png", "src/a 2.png", "README"].contains(&p);
        assert_eq!(free_name("", "b.png", taken), "b.png");
        assert_eq!(free_name("", "a.png", taken), "a 2.png");
        assert_eq!(free_name("src", "a.png", taken), "src/a 3.png");
        assert_eq!(free_name("", "README", taken), "README 2");
        assert_eq!(free_name("", ".env", |p| p == ".env"), ".env 2");
    }
}
