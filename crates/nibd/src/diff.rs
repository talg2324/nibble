//! What changed in the project, for nib's diff view: `git diff` run by the system git and parsed
//! into files, hunks and lines. Nothing here writes to the repository's index or work tree.

use crate::ai::DiffTarget;
use nib_proto::{AiId, DiffFile, DiffHunk, DiffLine, DiffLineKind, DiffScope, DiffSet, DiffStatus};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

/// Most diff lines of one file, and of a whole diff, that an answer carries.
const FILE_LINES: usize = 5_000;
const ALL_LINES: usize = 60_000;

/// git's stdout, or `None` when it could not run or failed. `index`: the index file to use
/// instead of the repository's.
fn git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Option<Vec<u8>> {
    let guard = crate::git::guard(dir)?;
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(["-c", "core.quotepath=off"]).args(&guard).args(args);
    cmd.env("GIT_OPTIONAL_LOCKS", "0").env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null());
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let out = cmd.output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// `git add FLAG -- .` into the shadow `index`, submodules left out: git would look into a
/// submodule's work tree to tell whether it changed, and run the filters its own config names
/// there (no option of `add` stops that). A submodule's own changes are not shown anyway.
fn add(dir: &Path, index: &Path, flag: &str) -> Option<Vec<u8>> {
    let listed = git(dir, Some(index), &["ls-files", "-s", "-z", "--", "."])?;
    let listed = String::from_utf8_lossy(&listed);
    // `MODE HASH STAGE\tPATH`; a submodule's mode is 160000.
    let skip: Vec<String> = listed.split('\0').filter(|e| e.starts_with("160000 ")).filter_map(|e| e.split_once('\t')).map(|(_, p)| format!(":(exclude,literal){p}")).collect();
    let mut args = vec!["add", flag, "--", "."];
    args.extend(skip.iter().map(String::as_str));
    git(dir, Some(index), &args)
}

fn text(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, None, args).map(|o| String::from_utf8_lossy(&o).trim_end_matches(['\n', '\r']).to_string())
}

/// A copy of the repository's index that git may change freely: untracked files are added to it,
/// so that one `git diff` shows them too, without the user's own index being touched. Deleted
/// when dropped.
struct ShadowIndex(PathBuf);

impl ShadowIndex {
    fn new(dir: &Path) -> Option<ShadowIndex> {
        Self::of(PathBuf::from(index_path(dir)?))
    }

    /// A copy of `real`, the repository's index file (`index_path`).
    fn of(real: PathBuf) -> Option<ShadowIndex> {
        static N: AtomicU32 = AtomicU32::new(0);
        let copy = crate::places::private_temp()?.join(format!("index-{}", N.fetch_add(1, Ordering::Relaxed)));
        // No index yet (nothing was ever added): git starts an empty one at `copy`.
        if real.exists() {
            std::fs::copy(&real, &copy).ok()?;
        }
        Some(ShadowIndex(copy))
    }
}

/// Where the repository at `dir` keeps its index, absolute.
fn index_path(dir: &Path) -> Option<String> {
    text(dir, &["rev-parse", "--path-format=absolute", "--git-path", "index"])
}

impl Drop for ShadowIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("lock"));
    }
}

/// What is not as the last commit has it in the work tree `dir` is in, for telling what a
/// command changed (one of these before it and one after): each changed or untracked file
/// (absolute), with git's two status letters, its time and its size.
pub type Changed = std::collections::HashMap<PathBuf, (String, u128, u64)>;

pub fn changed(dir: &Path) -> Changed {
    let Some(top) = text(dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from) else { return Changed::new() };
    let Some(out) = git(dir, None, &["status", "--porcelain=v1", "-z", "--untracked-files=all"]) else { return Changed::new() };
    let out = String::from_utf8_lossy(&out);
    let mut all = Changed::new();
    let mut records = out.split('\0');
    while let Some(rec) = records.next() {
        let (Some(code), Some(path)) = (rec.get(..2), rec.get(3..)) else { continue };
        if code.contains(['R', 'C']) {
            // Followed by the path it came from.
            records.next();
        }
        let path = top.join(path);
        let (time, size) = std::fs::metadata(&path)
            .map(|m| (m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos()), m.len()))
            .unwrap_or((0, 0));
        all.insert(path, (code.to_string(), time, size));
    }
    all
}

/// The diff of `scope` at `dir`. `known`: the stamp the client has (see `Request::Diff`).
/// `agents`: for the agents' scopes, where the work of each one asked about is.
pub fn diff(dir: &Path, scope: DiffScope, known: Option<u64>, agents: &[(AiId, DiffTarget)]) -> DiffSet {
    // What git printed for the shared work tree, and for each agent's own.
    let shared = match scope {
        DiffScope::Agent(_) if matches!(agents.first(), Some((_, DiffTarget::Tree { .. }))) => None,
        DiffScope::Since(_) => None,
        DiffScope::Agent(_) | DiffScope::Everyone => raw(dir, DiffScope::Head),
        s => raw(dir, s),
    };
    if shared.is_none() && text(dir, &["rev-parse", "--show-toplevel"]).is_none() {
        return DiffSet::default();
    }
    let trees: Vec<(AiId, &Path, Vec<u8>)> = agents
        .iter()
        .filter_map(|(id, t)| match t {
            DiffTarget::Tree { dir, base } => Some((*id, dir.as_path(), raw_branch(dir, base)?)),
            DiffTarget::Since { dir, commit } => Some((*id, dir.as_path(), raw_since(dir, commit)?)),
            DiffTarget::Paths(_) => None,
        })
        .collect();
    // In the shared work tree, whose each changed file is.
    let owners = |path: &str| -> Vec<AiId> {
        let abs = dir.join(path);
        agents.iter().filter(|(_, t)| matches!(t, DiffTarget::Paths(p) if p.contains(&abs))).map(|(id, _)| *id).collect()
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (scope, &shared).hash(&mut h);
    for (id, tree, raw) in &trees {
        (id, tree, raw).hash(&mut h);
    }
    for (id, t) in agents {
        if let DiffTarget::Paths(p) = t {
            (id, p).hash(&mut h);
        }
    }
    let stamp = h.finish();
    if known == Some(stamp) {
        return DiffSet { stamp, files: None, repo: true };
    }
    let mut files = shared.map(|raw| parse(&String::from_utf8_lossy(&raw))).unwrap_or_default();
    match scope {
        DiffScope::Agent(_) => {
            for f in &mut files {
                f.agents = owners(&f.path);
            }
            files.retain(|f| !f.agents.is_empty());
        }
        DiffScope::Everyone => {
            for f in &mut files {
                f.agents = owners(&f.path);
            }
        }
        _ => {}
    }
    for (id, tree, raw) in trees {
        let at = nib_proto::path::to_wire(tree);
        files.extend(parse(&String::from_utf8_lossy(&raw)).into_iter().map(|f| DiffFile { agents: vec![id], dir: Some(at.clone()), ..f }));
    }
    DiffSet { stamp, files: Some(files), repo: true }
}

/// The ref that keeps an agent's last reviewed state from being collected, for `branch`.
pub fn round_ref(branch: &str) -> String {
    format!("refs/nib/rounds/{}", branch.trim_start_matches("nib/"))
}

/// The work tree `dir` is in as it is now, uncommitted and untracked files included, as a
/// commit on top of its HEAD that no branch has (kept by `round_ref`): what a later
/// `DiffScope::Since` compares with. `None`: git could not.
pub fn snapshot(dir: &Path, branch: &str) -> Option<String> {
    let top = PathBuf::from(text(dir, &["rev-parse", "--show-toplevel"])?);
    let shadow = ShadowIndex::new(&top)?;
    add(&top, &shadow.0, "-A")?;
    let tree = String::from_utf8_lossy(&git(&top, Some(&shadow.0), &["write-tree"])?).trim().to_string();
    let made = Command::new("git")
        .arg("-C")
        .arg(&top)
        .args(["commit-tree", &tree, "-p", "HEAD", "-m", "nib: as reviewed"])
        // Whoever the user is, or nobody: this commit is nib's own bookkeeping.
        .envs([("GIT_AUTHOR_NAME", "nib"), ("GIT_AUTHOR_EMAIL", "nib@localhost"), ("GIT_COMMITTER_NAME", "nib"), ("GIT_COMMITTER_EMAIL", "nib@localhost")])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let commit = String::from_utf8_lossy(&made.stdout).trim().to_string();
    if !made.status.success() || commit.is_empty() {
        return None;
    }
    git(&top, None, &["update-ref", &round_ref(branch), &commit])?;
    Some(commit)
}

/// What `git diff` prints for the work tree at `dir` against `commit` exactly.
fn raw_since(dir: &Path, commit: &str) -> Option<Vec<u8>> {
    let shadow = ShadowIndex::new(dir)?;
    add(dir, &shadow.0, "-A")?;
    git(dir, Some(&shadow.0), &[&DIFF[..], &["--cached", commit]].concat())
}

/// What `git diff` prints for an agent's branch checked out at `dir`: from where it left
/// `base` to its work tree as it is, committed or not.
fn raw_branch(dir: &Path, base: &str) -> Option<Vec<u8>> {
    let from = text(dir, &["merge-base", base, "HEAD"]).filter(|s| !s.is_empty()).unwrap_or_else(|| base.to_string());
    let shadow = ShadowIndex::new(dir)?;
    add(dir, &shadow.0, "-A")?;
    git(dir, Some(&shadow.0), &[&DIFF[..], &["--cached", &from]].concat())
}

/// `git diff`, giving the same output whatever the user's config says (and running nothing a
/// submodule's config names); paths relative to the folder it runs in.
const DIFF: [&str; 10] = ["diff", "--relative", "-M", "-U3", "--no-color", "--no-ext-diff", "--no-textconv", "--ignore-submodules=dirty", "--src-prefix=a/", "--dst-prefix=b/"];

/// What `git diff` prints for `scope`; `None` outside a work tree.
fn raw(dir: &Path, scope: DiffScope) -> Option<Vec<u8>> {
    // Independent of each other: asked at once (each is a process start, ~10 ms).
    let shadowed = matches!(scope, DiffScope::Unstaged | DiffScope::Head);
    let (top, has_head, index) = std::thread::scope(|s| {
        let has_head = s.spawn(|| text(dir, &["rev-parse", "--verify", "-q", "HEAD"]).is_some());
        let index = shadowed.then(|| s.spawn(|| index_path(dir)));
        let top = text(dir, &["rev-parse", "--show-toplevel"]);
        (top, has_head.join().unwrap_or(false), index.and_then(|h| h.join().ok().flatten()))
    });
    top?;
    let run = |index: Option<&Path>, more: &[&str]| git(dir, index, &[&DIFF[..], more].concat());
    // Before the first commit there is no HEAD: `--cached` alone then compares with nothing.
    let head: &[&str] = if has_head { &["HEAD"] } else { &[] };
    let shadow = || ShadowIndex::of(PathBuf::from(index.clone()?));
    match scope {
        DiffScope::Staged => run(None, &["--cached"]),
        DiffScope::Unstaged => {
            let shadow = shadow()?;
            // Intent to add: the untracked files show as new files of the work tree.
            add(dir, &shadow.0, "-N")?;
            run(Some(&shadow.0), &[])
        }
        DiffScope::Head => {
            let shadow = shadow()?;
            add(dir, &shadow.0, "-A")?;
            run(Some(&shadow.0), &[&["--cached"], head].concat())
        }
        // Made of the others, in `diff`.
        DiffScope::Agent(_) | DiffScope::Everyone | DiffScope::Since(_) => None,
    }
}

/// Unified diff text (`git diff` with `a/` and `b/` prefixes) into files.
pub fn parse(out: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut total = 0usize;
    // Lines of the current hunk still to come on each side: a removed line that itself starts
    // with `-- ` must not be taken for a `--- ` header.
    let (mut old_left, mut new_left) = (0u32, 0u32);
    for line in out.split('\n') {
        if old_left > 0 || new_left > 0 {
            let Some(f) = files.last_mut() else { continue };
            let (kind, rest) = match line.as_bytes().first() {
                Some(b'+') => (DiffLineKind::Add, &line[1..]),
                Some(b'-') => (DiffLineKind::Del, &line[1..]),
                Some(b' ') => (DiffLineKind::Context, &line[1..]),
                // "\ No newline at end of file" belongs to the line before it.
                Some(b'\\') => continue,
                // A context line that lost its leading space (some tools strip it).
                _ => (DiffLineKind::Context, line),
            };
            match kind {
                DiffLineKind::Add => (new_left, f.adds) = (new_left.saturating_sub(1), f.adds + 1),
                DiffLineKind::Del => (old_left, f.dels) = (old_left.saturating_sub(1), f.dels + 1),
                DiffLineKind::Context => (old_left, new_left) = (old_left.saturating_sub(1), new_left.saturating_sub(1)),
            }
            if f.truncated {
                continue;
            }
            let kept: usize = f.hunks.iter().map(|h| h.lines.len()).sum();
            if kept >= FILE_LINES || total >= ALL_LINES {
                f.truncated = true;
                continue;
            }
            total += 1;
            if let Some(h) = f.hunks.last_mut() {
                h.lines.push(DiffLine { kind, text: rest.trim_end_matches('\r').to_string() });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            files.push(DiffFile { path: header_path(rest), ..Default::default() });
            continue;
        }
        let Some(f) = files.last_mut() else { continue };
        if let Some(rest) = line.strip_prefix("@@ -") {
            let Some(h) = hunk(rest) else { continue };
            (old_left, new_left) = (h.old_len, h.new_len);
            f.hunks.push(h);
        } else if line.starts_with("new file mode") {
            f.status = DiffStatus::Added;
        } else if line.starts_with("deleted file mode") {
            f.status = DiffStatus::Deleted;
        } else if let Some(p) = line.strip_prefix("rename from ") {
            f.status = DiffStatus::Renamed;
            f.old_path = Some(unquote(p));
        } else if let Some(p) = line.strip_prefix("rename to ") {
            f.path = unquote(p);
        } else if let Some(p) = line.strip_prefix("+++ b/").or_else(|| line.strip_prefix("+++ \"b/")) {
            f.path = unquote_rest(p, line.starts_with("+++ \""));
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            f.binary = true;
        }
    }
    files
}

/// `old_start[,old_len] +new_start[,new_len] @@ header`, what follows `@@ -`.
fn hunk(rest: &str) -> Option<DiffHunk> {
    let (ranges, header) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let ((old_start, old_len), (new_start, new_len)) = (range(old)?, range(new)?);
    Some(DiffHunk { old_start, old_len, new_start, new_len, header: header.trim().to_string(), lines: Vec::new() })
}

/// The path in `a/P b/P` (what follows `diff --git `). The two are the same unless the file was
/// renamed, and then `rename to` gives it; a path with unusual characters is in quotes.
fn header_path(rest: &str) -> String {
    if let Some(q) = rest.strip_prefix('"') {
        // "a/P" "b/P" or "a/P" b/P: the first quoted string.
        let end = closing_quote(q).unwrap_or(q.len());
        return unquote(&format!("\"{}\"", &q[..end])).trim_start_matches("a/").to_string();
    }
    // a/P b/P: 2 + |P| + 1 + 2 + |P| bytes.
    let n = rest.len().saturating_sub(5) / 2;
    match rest.get(2..2 + n) {
        Some(p) if rest.len() >= 5 && rest[2 + n..].starts_with(" b/") => p.to_string(),
        // A rename: the old name, replaced by `rename to` on a later line.
        _ => rest.split(" b/").next().unwrap_or(rest).trim_start_matches("a/").to_string(),
    }
}

/// Index of the `"` that ends a quoted string (`s` starts after the opening one).
fn closing_quote(s: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some(i),
            _ => {}
        }
    }
    None
}

/// `p` as it follows `+++ b/`; `quoted`: the whole `"b/…"` was in quotes (the closing one is
/// still on `p`).
fn unquote_rest(p: &str, quoted: bool) -> String {
    if quoted {
        unquote(&format!("\"{p}"))
    } else {
        // git ends an unquoted name that has a space in it with a tab.
        p.trim_end_matches('\t').to_string()
    }
}

/// A path as git prints it: plain, or in double quotes with C escapes when it has a quote, a
/// backslash or a control character in it.
fn unquote(p: &str) -> String {
    let Some(inner) = p.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else { return p.to_string() };
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let b = inner.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 == b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b[i] {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'0'..=b'7' => {
                let end = (i + 3).min(b.len());
                let digits = &inner[i..end];
                let n = digits.bytes().take_while(|d| (b'0'..=b'7').contains(d)).count();
                out.push(u8::from_str_radix(&digits[..n], 8).unwrap_or(b'?'));
                i += n - 1;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use DiffLineKind::{Add, Context, Del};

    #[test]
    fn parses_hunks_and_counts() {
        let out = "diff --git a/src/a b.rs b/src/a b.rs\nindex 1..2 100644\n--- a/src/a b.rs\t\n+++ b/src/a b.rs\t\n\
                   @@ -1,3 +1,3 @@ fn main() {\n one\n--- two\n+++ 2\n three\n\\ No newline at end of file\n\
                   @@ -10 +10,2 @@\n ten\n+eleven\n";
        let files = parse(out);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!((f.path.as_str(), f.status, f.adds, f.dels, f.binary), ("src/a b.rs", DiffStatus::Modified, 2, 1, false));
        assert_eq!(f.hunks.len(), 2);
        let h = &f.hunks[0];
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len, h.header.as_str()), (1, 3, 1, 3, "fn main() {"));
        let lines: Vec<_> = h.lines.iter().map(|l| (l.kind, l.text.as_str())).collect();
        assert_eq!(lines, [(Context, "one"), (Del, "-- two"), (Add, "++ 2"), (Context, "three")]);
        assert_eq!((f.hunks[1].old_len, f.hunks[1].new_len, f.hunks[1].lines.len()), (1, 2, 2));
    }

    #[test]
    fn parses_added_deleted_renamed_and_binary() {
        let out = "diff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0..1\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hi\n\
                   diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\nindex 1..0\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n\
                   diff --git a/old name.txt b/dir/new name.txt\nsimilarity index 90%\nrename from old name.txt\nrename to dir/new name.txt\n\
                   diff --git a/pic.png b/pic.png\nindex 1..2 100644\nBinary files a/pic.png and b/pic.png differ\n\
                   diff --git \"a/q\\\"uote\\303\\251.txt\" \"b/q\\\"uote\\303\\251.txt\"\nnew file mode 100644\nindex 0..e69de29\n";
        let files = parse(out);
        let seen: Vec<_> = files.iter().map(|f| (f.path.as_str(), f.status, f.old_path.as_deref(), f.binary, f.hunks.len())).collect();
        assert_eq!(
            seen,
            [
                ("new.txt", DiffStatus::Added, None, false, 1),
                ("gone.txt", DiffStatus::Deleted, None, false, 1),
                ("dir/new name.txt", DiffStatus::Renamed, Some("old name.txt"), false, 0),
                ("pic.png", DiffStatus::Modified, None, true, 0),
                ("q\"uoteé.txt", DiffStatus::Added, None, false, 0),
            ]
        );
    }

    #[test]
    fn long_files_are_cut_but_counted() {
        let mut out = format!("diff --git a/big b/big\n--- a/big\n+++ b/big\n@@ -0,0 +1,{} @@\n", FILE_LINES + 10);
        for _ in 0..FILE_LINES + 10 {
            out.push_str("+x\n");
        }
        out.push_str("diff --git a/z b/z\n--- a/z\n+++ b/z\n@@ -1 +1 @@\n-a\n+b\n");
        let files = parse(&out);
        assert!(files[0].truncated && files[0].hunks[0].lines.len() == FILE_LINES && files[0].adds as usize == FILE_LINES + 10);
        assert_eq!((files[1].path.as_str(), files[1].adds, files[1].dels, files[1].truncated), ("z", 1, 1, false));
    }

    #[test]
    fn scopes_against_a_real_repository() {
        let dir = std::env::temp_dir().join(format!("nibd-diff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let sh = |args: &[&str]| Command::new("git").arg("-C").arg(&dir).args(args).output().is_ok_and(|o| o.status.success());
        assert!(!diff(&dir.join("sub"), DiffScope::Head, None, &[]).repo || sh(&["rev-parse", "--show-toplevel"]));
        if !sh(&["init", "-q", "-b", "trunk"]) {
            return; // no git on this machine
        }
        for args in [&["config", "user.name", "T"][..], &["config", "user.email", "t@example.com"], &["config", "commit.gpgsign", "false"]] {
            assert!(sh(args));
        }
        let paths = |scope| {
            let set = diff(&dir, scope, None, &[]);
            assert!(set.repo);
            set.files.unwrap().into_iter().map(|f| (f.path, f.status, f.adds, f.dels)).collect::<Vec<_>>()
        };
        // Before the first commit: everything is new, and nothing is staged.
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        assert_eq!(paths(DiffScope::Head), [("a.txt".to_string(), DiffStatus::Added, 2, 0)]);
        assert_eq!(paths(DiffScope::Unstaged), [("a.txt".to_string(), DiffStatus::Added, 2, 0)]);
        assert_eq!(paths(DiffScope::Staged), []);
        assert!(sh(&["add", "-A"]) && sh(&["commit", "-q", "-m", "first"]));
        assert_eq!(paths(DiffScope::Head), []);

        // One staged change, one unstaged, one untracked file in a folder.
        std::fs::write(dir.join("a.txt"), "one\n2\n").unwrap();
        assert!(sh(&["add", "a.txt"]));
        std::fs::write(dir.join("a.txt"), "one\n2\nthree\n").unwrap();
        std::fs::write(dir.join("sub/new.txt"), "n\n").unwrap();
        assert_eq!(paths(DiffScope::Staged), [("a.txt".to_string(), DiffStatus::Modified, 1, 1)]);
        assert_eq!(
            paths(DiffScope::Unstaged),
            [("a.txt".to_string(), DiffStatus::Modified, 1, 0), ("sub/new.txt".to_string(), DiffStatus::Added, 1, 0)]
        );
        assert_eq!(
            paths(DiffScope::Head),
            [("a.txt".to_string(), DiffStatus::Modified, 2, 1), ("sub/new.txt".to_string(), DiffStatus::Added, 1, 0)]
        );
        // The user's index was not touched: the new file is still untracked.
        let status = Command::new("git").arg("-C").arg(&dir).args(["status", "--porcelain"]).output().unwrap().stdout;
        assert!(String::from_utf8_lossy(&status).contains("?? sub/"), "{}", String::from_utf8_lossy(&status));

        // Asked from a folder: only what is under it, paths relative to it.
        assert_eq!(paths_in(&dir.join("sub")), ["new.txt"]);
        // The same diff again is not sent twice.
        let first = diff(&dir, DiffScope::Head, None, &[]);
        assert_eq!(diff(&dir, DiffScope::Head, Some(first.stamp), &[]).files, None);
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        assert!(diff(&dir, DiffScope::Head, Some(first.stamp), &[]).files.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn agents_scopes_take_shared_files_and_branches() {
        let dir = std::env::temp_dir().join(format!("nibd-diff-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (repo, tree) = (dir.join("repo"), dir.join("tree"));
        std::fs::create_dir_all(&repo).unwrap();
        let sh = |at: &Path, args: &[&str]| Command::new("git").arg("-C").arg(at).args(args).output().is_ok_and(|o| o.status.success());
        if !sh(&repo, &["init", "-q", "-b", "trunk"]) {
            return; // no git on this machine
        }
        for args in [&["config", "user.name", "T"][..], &["config", "user.email", "t@example.com"], &["config", "commit.gpgsign", "false"]] {
            assert!(sh(&repo, args));
        }
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        std::fs::write(repo.join("b.txt"), "one\n").unwrap();
        assert!(sh(&repo, &["add", "-A"]) && sh(&repo, &["commit", "-q", "-m", "first"]));
        // Agent 1 has a branch: one commit on it and one file not committed. The base moved on.
        assert!(sh(&repo, &["worktree", "add", "-q", "-b", "nib/feat/x", tree.to_str().unwrap(), "HEAD"]));
        std::fs::write(tree.join("a.txt"), "one\ntwo\n").unwrap();
        assert!(sh(&tree, &["commit", "-q", "-am", "on the branch"]));
        std::fs::write(tree.join("new.txt"), "n\n").unwrap();
        std::fs::write(repo.join("c.txt"), "later\n").unwrap();
        assert!(sh(&repo, &["add", "-A"]) && sh(&repo, &["commit", "-q", "-m", "second"]));
        // Agent 2 is in the shared tree and changed b.txt; somebody else changed c.txt.
        std::fs::write(repo.join("b.txt"), "ONE\n").unwrap();
        std::fs::write(repo.join("c.txt"), "later!\n").unwrap();
        let agents = [(1, DiffTarget::Tree { dir: tree.clone(), base: "trunk".into() }), (2, DiffTarget::Paths(vec![repo.join("b.txt")]))];
        let seen = |scope, agents: &[(AiId, DiffTarget)]| {
            diff(&repo, scope, None, agents).files.unwrap().into_iter().map(|f| (f.path, f.agents, f.dir.is_some())).collect::<Vec<_>>()
        };
        // The branch against where it left trunk: `c.txt` (trunk's own later commit) is not in it.
        assert_eq!(seen(DiffScope::Agent(1), &agents[..1]), [("a.txt".to_string(), vec![1], true), ("new.txt".to_string(), vec![1], true)]);
        assert_eq!(seen(DiffScope::Agent(2), &agents[1..]), [("b.txt".to_string(), vec![2], false)]);
        assert_eq!(
            seen(DiffScope::Everyone, &agents),
            [
                ("b.txt".to_string(), vec![2], false),
                ("c.txt".to_string(), vec![], false),
                ("a.txt".to_string(), vec![1], true),
                ("new.txt".to_string(), vec![1], true),
            ]
        );
        // Reviewed as it is now; then one more change: only that is "since the review".
        let commit = snapshot(&tree, "nib/feat/x").unwrap();
        assert_eq!(text(&repo, &["rev-parse", &round_ref("nib/feat/x")]).as_deref(), Some(commit.as_str()));
        assert_eq!(seen(DiffScope::Since(1), &[(1, DiffTarget::Since { dir: tree.clone(), commit: commit.clone() })]), []);
        std::fs::write(tree.join("new.txt"), "n\nmore\n").unwrap();
        let since = diff(&repo, DiffScope::Since(1), None, &[(1, DiffTarget::Since { dir: tree.clone(), commit })]).files.unwrap();
        assert_eq!(since.iter().map(|f| (f.path.as_str(), f.adds, f.dels)).collect::<Vec<_>>(), [("new.txt", 1, 0)]);
        // Never reviewed: nothing to compare with.
        assert_eq!(seen(DiffScope::Since(1), &[]), []);
        // An agent nib doesn't know: nothing, but still a repository.
        let none = diff(&repo, DiffScope::Agent(9), None, &[]);
        assert!(none.repo && none.files == Some(vec![]));
        assert!(sh(&repo, &["worktree", "remove", "--force", tree.to_str().unwrap()]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn paths_in(dir: &Path) -> Vec<String> {
        diff(dir, DiffScope::Head, None, &[]).files.unwrap().into_iter().map(|f| f.path).collect()
    }
}
