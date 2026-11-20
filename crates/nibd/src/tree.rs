//! The file tree's data: every file and folder under the project root (breadth first, so a
//! truncated listing is missing the deepest levels) and git's view of them.

use crate::git;
use nib_proto::{TreeEntry, TreeSnapshot};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Most entries one listing carries.
const MAX_ENTRIES: usize = 20_000;

/// `with_git: false` for directories outside the project (scratch, NIBDIR): a repository that
/// happens to contain them (dotfiles in `~`) is not theirs.
pub fn snapshot(root: &Path, known: Option<u64>, with_git: bool) -> TreeSnapshot {
    let git = if with_git { git::tree_state(root) } else { None };
    let (entries, truncated) = walk(root, git.as_ref().map(|g| &g.ignored), MAX_ENTRIES, true);
    let mut h = DefaultHasher::new();
    entries.hash(&mut h);
    let stamp = h.finish();
    let mut snap = TreeSnapshot {
        stamp,
        entries: (known != Some(stamp)).then_some(entries),
        truncated,
        repo: git.is_some(),
        ..Default::default()
    };
    if let Some(g) = git {
        snap.changes = g.changes;
        snap.upstream = g.upstream;
        snap.ahead = g.ahead;
        snap.behind = g.behind;
    }
    snap
}

/// The files search reads (`search`): not ignored, not inside a folder whose name starts with
/// `.`, at most `max` of them (`true`: there were more).
pub fn files(root: &Path, max: usize) -> (Vec<String>, bool) {
    let git = git::tree_state(root);
    let (entries, truncated) = walk(root, git.as_ref().map(|g| &g.ignored), max, false);
    (entries.into_iter().filter(|e| !e.dir && !e.ignored).map(|e| e.path).collect(), truncated)
}

/// Everything under `root` except `.git`; ignored folders are listed but not entered.
/// `dot_dirs: false`: folders whose name starts with `.` are left out altogether.
fn walk(root: &Path, ignored: Option<&HashSet<String>>, max: usize, dot_dirs: bool) -> (Vec<TreeEntry>, bool) {
    let mut out = Vec::new();
    let mut queue = VecDeque::from([String::new()]);
    while let Some(rel) = queue.pop_front() {
        let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(&rel) };
        let Ok(read) = fs::read_dir(&dir) else { continue };
        // (name, shown as a folder, a real folder: entered)
        let mut found = Vec::new();
        for e in read.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            if name == ".git" || name == ".DS_Store" {
                continue;
            }
            // A link to a folder is shown as one but never followed (it may loop).
            let is_dir = ft.is_dir() || (ft.is_symlink() && fs::metadata(e.path()).is_ok_and(|m| m.is_dir()));
            if is_dir && !dot_dirs && name.starts_with('.') {
                continue;
            }
            found.push((name, is_dir, ft.is_dir()));
        }
        found.sort();
        for (name, is_dir, enter) in found {
            if out.len() >= max {
                return (out, true);
            }
            let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let is_ignored = ignored.is_some_and(|s| s.contains(&path));
            if enter && !is_ignored {
                queue.push_back(path.clone());
            }
            out.push(TreeEntry { path, dir: is_dir, ignored: is_ignored });
        }
    }
    (out, false)
}
