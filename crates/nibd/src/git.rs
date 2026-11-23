//! Git for the project root, through the system `git` (nibd links no git library): what the
//! client shows (branch, user, history, changes) and the few operations it can ask for.

use nib_proto::{GitBlameCommit, GitChange, GitCommit, GitInfo, GitOp, GitRepo};
use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// `git -C dir`, as every git command nibd runs by itself starts: paths printed as they are (not
/// quoted, so that they can be joined to the top of the work tree), no optional lock taken (it
/// runs in the background, repeatedly, and must not get in the user's way), and nobody to answer
/// a prompt. What the repository's config could make it run is turned off by [`guard`], which
/// the caller adds.
pub(crate) fn cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=off"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    cmd
}

/// What a repository's own config could make a background git command run, turned off: the
/// `-c` options for every command nibd runs by itself (status, log, diff, the shadow index).
///
/// A folder can arrive with its `.git` (an archive, a copied directory, a sandboxed agent that
/// may write in `.git`), and opening it must not run what that config says: `core.fsmonitor`
/// (run by `status`), `log.showSignature` with `gpg.program` (run by `log`), and filter drivers
/// (run by `status` and `add` on any file `.gitattributes` gives one), hooks (`add` runs
/// `post-index-change`, `update-ref` runs `reference-transaction`), and the `gc` or
/// `maintenance` a command may start on its own. Filters the user's own
/// config defines (global, system: Git LFS) are left alone, as are the standard Git LFS
/// commands in the repository's config (`git lfs install --local`). `None`: a filter name that
/// cannot be turned off from the command line; the caller then runs nothing.
pub(crate) fn guard(dir: &Path) -> Option<Vec<String>> {
    guard_env(dir, &[])
}

/// [`guard`] for git run with `env` (`GIT_DIR` and the like).
pub(crate) fn guard_env(dir: &Path, env: &[(&str, &Path)]) -> Option<Vec<String>> {
    const OFF: [&str; 5] = ["core.fsmonitor=false", "log.showSignature=false", "core.hooksPath=/dev/null", "gc.auto=0", "maintenance.auto=false"];
    let mut args: Vec<String> = OFF.iter().flat_map(|c| ["-c".to_string(), c.to_string()]).collect();
    // Both scopes at once (each is a process start, ~10 ms). `--includes`: a file scope reads no
    // includes unless told, and `.git/config` may have one.
    let read = |scope: &'static str| {
        move || {
            cmd(dir)
                .args(["config", scope, "--includes", "--null", "--get-regexp", r"^filter\."])
                .envs(env.iter().copied())
                .stderr(Stdio::null())
                .output()
                .map(|o| o.stdout)
                .unwrap_or_default()
        }
    };
    let found = thread::scope(|s| {
        let worktree = s.spawn(read("--worktree"));
        let local = read("--local")();
        [local, worktree.join().unwrap_or_default()]
    });
    let mut names: Vec<String> = Vec::new();
    for out in found {
        // `-z`: entries end with NUL, the key and its value are split by a newline.
        for entry in String::from_utf8_lossy(&out).split('\0').filter(|e| !e.is_empty()) {
            let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
            let Some((name, var)) = key.strip_prefix("filter.").and_then(|k| k.rsplit_once('.')) else { continue };
            if !matches!(var, "clean" | "smudge" | "process") || LFS.contains(&value.trim()) {
                continue;
            }
            // `-c a=b` splits at the first `=`: a name with one would turn off something else.
            if name.contains('=') {
                return None;
            }
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    for name in names {
        for var in ["clean=", "smudge=", "process=", "required=false"] {
            args.extend(["-c".to_string(), format!("filter.{name}.{var}")]);
        }
    }
    Some(args)
}

/// What `git lfs install` writes: safe to run, and without it every LFS file shows as changed.
const LFS: [&str; 3] = ["git-lfs clean -- %f", "git-lfs smudge -- %f", "git-lfs filter-process"];

/// `Err`: git could not be started. `Ok(None)`: it ran and failed.
fn git(dir: &Path, args: &[&str]) -> io::Result<Option<String>> {
    match guard(dir) {
        Some(guard) => git_with(dir, &guard, args),
        None => Ok(None),
    }
}

/// [`git`] with a [`guard`] already found (one for several commands in a row).
fn git_with(dir: &Path, guard: &[String], args: &[&str]) -> io::Result<Option<String>> {
    let out = cmd(dir).args(guard).args(args).output()?;
    if !out.status.success() {
        // macOS ships a `git` stub that only asks to install the developer tools.
        if String::from_utf8_lossy(&out.stderr).contains("xcode-select") {
            return Err(io::ErrorKind::NotFound.into());
        }
        return Ok(None);
    }
    let s = String::from_utf8_lossy(&out.stdout).trim_end_matches(['\n', '\r']).to_string();
    Ok(Some(s))
}

/// Remove every work tree of the repository at `root` that lies inside `dir` (an agent's own
/// scratch folder, when the agent ends), and forget work trees whose folder is gone. Run with
/// the repository's own config turned off (`guard`), like everything nibd runs by itself.
pub fn remove_worktrees_under(root: &Path, dir: &Path) {
    let Some(guard) = guard(root) else { return };
    let git = |args: &[&str]| cmd(root).args(&guard).args(args).output();
    // Compared as git prints them: real paths (`/private/var/…`, not `/var/…`).
    let dir = nib_proto::path::canonical(dir).unwrap_or_else(|_| dir.to_path_buf());
    if let Ok(out) = git(&["worktree", "list", "--porcelain"]) {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Some(tree) = line.strip_prefix("worktree ").map(Path::new) else { continue };
            if tree.starts_with(&dir) && tree != dir {
                let removed = git(&["worktree", "remove", "--force", "--force", &tree.to_string_lossy()]).is_ok_and(|o| o.status.success());
                if !removed {
                    let _ = std::fs::remove_dir_all(tree);
                }
            }
        }
    }
    let _ = git(&["worktree", "prune"]);
}

/// Forget work trees whose folder is gone (a scratch folder swept after its session ended).
pub fn prune_worktrees(root: &Path) {
    if let Some(guard) = guard(root) {
        let _ = cmd(root).args(&guard).args(["worktree", "prune"]).output();
    }
}

pub fn info(dir: &Path) -> GitInfo {
    // Independent of each other: run at once (each is a process start, ~10 ms).
    let Some(guard) = guard(dir) else { return GitInfo::NotRepo };
    let guard = &guard;
    let get = |args: &'static [&'static str]| move || git_with(dir, guard, args);
    let (top, branch, last, user_name, user_email) = thread::scope(|s| {
        let h = [
            s.spawn(get(&["symbolic-ref", "--short", "-q", "HEAD"])),
            s.spawn(get(&["log", "-1", LOG_FORMAT])),
            s.spawn(get(&["config", "user.name"])),
            s.spawn(get(&["config", "user.email"])),
        ];
        let top = git_with(dir, guard, &["rev-parse", "--show-toplevel"]);
        let [a, b, c, d] = h.map(|h| h.join().ok().and_then(Result::ok).flatten().filter(|s| !s.is_empty()));
        (top, a, b, c, d)
    });
    let top = match top {
        Err(_) => return GitInfo::NoGit,
        Ok(None) => return GitInfo::NotRepo,
        Ok(Some(top)) => top,
    };
    let name = Path::new(&top)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or(top);
    // `symbolic-ref` also names the branch of a repository with no commits yet.
    let last_commit = last.and_then(|s| parse_commit(&s));
    GitInfo::Repo(GitRepo { name, branch, user_name, user_email, last_commit })
}

/// Who last changed lines `first..=last` (1-based) of `file`: one entry per commit, in the order
/// git lists them. `contents`: blame that text instead of the file on disk (the editor's unsaved
/// buffer). Empty when git can't say (not tracked, not a repository, the range is past the end).
pub fn blame(file: &Path, first: u32, last: u32, contents: Option<&str>) -> Vec<GitBlameCommit> {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else { return Vec::new() };
    let Some(guard) = guard(dir) else { return Vec::new() };
    let range = format!("{},{}", first.max(1), last.max(first).max(1));
    let mut c = cmd(dir);
    c.args(&guard).args(["blame", "--porcelain", "-L", &range]);
    if contents.is_some() {
        c.args(["--contents", "-"]).stdin(Stdio::piped());
    }
    let Ok(mut child) = c.arg("--").arg(name).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else {
        return Vec::new();
    };
    if let (Some(text), Some(mut stdin)) = (contents, child.stdin.take()) {
        // Written from a thread of its own: git may fill its stdout before it has read all of it.
        let text = text.to_string();
        thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => parse_blame(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

/// `git blame --porcelain`: a header line `<hash> <orig line> <final line> [<group size>]` before
/// every line, followed (the first time a commit appears) by its `author`, `author-time`, …, and
/// then the line itself after a tab.
fn parse_blame(out: &str) -> Vec<GitBlameCommit> {
    let mut commits: Vec<GitBlameCommit> = Vec::new();
    let mut full: Vec<String> = Vec::new();
    let mut cur: Option<usize> = None;
    for line in out.lines() {
        if line.starts_with('\t') {
            continue;
        }
        let mut words = line.split(' ');
        let first = words.next().unwrap_or_default();
        let is_header = first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) && words.next().is_some_and(|w| w.parse::<u32>().is_ok());
        if is_header {
            let i = match full.iter().position(|h| h == first) {
                Some(i) => i,
                None => {
                    full.push(first.to_string());
                    commits.push(GitBlameCommit {
                        hash: first[..7].to_string(),
                        author: String::new(),
                        time: 0,
                        committed: first.bytes().any(|b| b != b'0'),
                        lines: 0,
                    });
                    commits.len() - 1
                }
            };
            commits[i].lines += 1;
            cur = Some(i);
        } else if let Some(c) = cur.map(|i| &mut commits[i]) {
            if let Some(a) = line.strip_prefix("author ") {
                c.author = a.to_string();
            } else if let Some(t) = line.strip_prefix("author-time ") {
                c.time = t.parse().unwrap_or(0);
            }
        }
    }
    commits
}

const LOG_FORMAT: &str = "--format=%h%x1f%s%x1f%an%x1f%ct";

fn parse_commit(line: &str) -> Option<GitCommit> {
    let mut f = line.split('\x1f');
    Some(GitCommit {
        hash: f.next()?.to_string(),
        subject: f.next()?.to_string(),
        author: f.next()?.to_string(),
        time: f.next()?.parse().ok()?,
    })
}

/// The newest `log_limit` commits and what `git status` lists. Both empty outside a repository.
pub fn details(dir: &Path, log_limit: u32) -> (Vec<GitCommit>, Vec<GitChange>) {
    let Some(guard) = guard(dir) else { return (Vec::new(), Vec::new()) };
    let get = |args: &[&str]| git_with(dir, &guard, args).ok().flatten().unwrap_or_default();
    let n = log_limit.to_string();
    // Independent of each other: run at once.
    let (log, status) = thread::scope(|s| {
        // `--ignore-submodules=dirty`: a submodule's work tree is not looked into, so its own
        // config runs nothing either (a moved submodule still shows).
        let status = s.spawn(|| get(&["status", "--porcelain=v1", "-z", "--ignore-submodules=dirty"]));
        (get(&["log", "-n", &n, LOG_FORMAT]), status.join().unwrap_or_default())
    });
    (log.lines().filter_map(parse_commit).collect(), parse_status(&status))
}

/// `git status --porcelain=v1 -z`: `XY path` records separated by NUL; a rename or copy is
/// followed by one more record, the path it came from.
pub(crate) fn parse_status(out: &str) -> Vec<GitChange> {
    let mut changes = Vec::new();
    let mut records = out.split('\0');
    while let Some(rec) = records.next() {
        let (Some(code), Some(path)) = (rec.get(..2), rec.get(3..)) else { continue };
        if code.contains(['R', 'C']) {
            records.next();
        }
        let staged = !matches!(code.as_bytes()[0], b' ' | b'?' | b'!');
        changes.push(GitChange { path: path.to_string(), code: code.to_string(), staged });
    }
    changes
}

/// Longest a single git command may run (a fetch on a slow link) before it is killed.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Run git for the user: (succeeded, what it printed). There is nobody to answer a prompt, so
/// anything that would ask (credentials, a passphrase, an editor) fails instead of waiting.
fn run_git(dir: &Path, args: &[&str]) -> (bool, String) {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        let ssh = git(dir, &["config", "core.sshCommand"]).ok().flatten().filter(|s| !s.is_empty());
        cmd.env("GIT_SSH_COMMAND", format!("{} -o BatchMode=yes", ssh.as_deref().unwrap_or("ssh")));
    }
    // A group of its own, so a timeout ends git's children too (the `ssh` of a fetch): one left
    // alive keeps the pipes open, and reading them would wait for it forever.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (false, format!("could not run git: {e}")),
    };
    // Drain both pipes while waiting, or a chatty git would block on a full pipe.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if started.elapsed() < TIMEOUT => thread::sleep(Duration::from_millis(20)),
            _ => {
                #[cfg(unix)]
                let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]).stderr(Stdio::null()).status();
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let mut text = String::new();
    for part in [out, err] {
        let bytes = part.join().unwrap_or_default();
        let s = String::from_utf8_lossy(&bytes);
        // Progress meters redraw with carriage returns: keep each line's final state.
        for line in s.lines() {
            let line = line.rsplit('\r').find(|l| !l.trim().is_empty()).unwrap_or("").trim_end();
            if !line.is_empty() {
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    match status {
        Some(s) => (s.success(), text.trim_end().to_string()),
        None => (false, format!("{text}timed out after {} s", TIMEOUT.as_secs())),
    }
}

pub fn run(dir: &Path, op: &GitOp) -> (bool, String) {
    match op {
        GitOp::Fetch => run_git(dir, &["fetch"]),
        GitOp::Pull => run_git(dir, &["pull"]),
        GitOp::Push => run_git(dir, &["push"]),
        GitOp::Stash => run_git(dir, &["stash"]),
        GitOp::StashPop => run_git(dir, &["stash", "pop"]),
        GitOp::Stage { paths } => run_on(dir, &["add", "-A"], paths),
        GitOp::Unstage { paths } => {
            // Before the first commit there is no HEAD to restore from.
            let has_head = git(dir, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok_and(|o| o.is_some());
            let args: &[&str] = if has_head { &["restore", "--staged"] } else { &["rm", "-r", "-q", "--cached"] };
            run_on(dir, args, paths)
        }
        GitOp::Commit { message } => {
            // `diff --cached --quiet` fails when something is staged (or there is no repository,
            // which the commit then reports).
            let nothing_staged = git(dir, &["diff", "--cached", "--quiet"]).is_ok_and(|o| o.is_some());
            if nothing_staged {
                let (ok, out) = run_git(dir, &["add", "-A"]);
                if !ok {
                    return (false, out);
                }
            }
            run_git(dir, &["commit", "-m", message])
        }
    }
}

/// `git <args> -- <paths>`, the paths taken literally (no globs or pathspec magic). No paths:
/// nothing to do (git would take that as "everything").
fn run_on(dir: &Path, args: &[&str], paths: &[String]) -> (bool, String) {
    if paths.is_empty() {
        return (true, String::new());
    }
    let mut all = vec!["--literal-pathspecs"];
    all.extend_from_slice(args);
    all.push("--");
    all.extend(paths.iter().map(String::as_str));
    run_git(dir, &all)
}

/// `git mv` if git tracks `from` (a folder: anything in it). `false`: not moved, rename it plainly.
pub fn mv(dir: &Path, from: &str, to: &str) -> bool {
    let ran = |args: &[&str]| git(dir, args).is_ok_and(|o| o.is_some());
    ran(&["--literal-pathspecs", "ls-files", "--error-unmatch", "--", from])
        && ran(&["--literal-pathspecs", "mv", "--", from, to])
}

/// What the file tree shows of git. Paths are relative to `dir`, not to the top of the work tree.
pub struct TreeState {
    pub changes: Vec<GitChange>,
    /// Ignored files and folders (a folder stands for everything in it).
    pub ignored: HashSet<String>,
    pub upstream: Vec<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// `None`: `dir` is not in a work tree (or there is no git).
pub fn tree_state(dir: &Path) -> Option<TreeState> {
    let range = "HEAD...@{upstream}";
    let guard = guard(dir)?;
    let get = |args: &[&str]| git_with(dir, &guard, args).ok().flatten().unwrap_or_default();
    // Independent of each other: run at once (each is a process start, ~10 ms).
    let (prefix, status, upstream, counts) = thread::scope(|s| {
        let status = s.spawn(|| get(&["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignored=matching", "--ignore-submodules=dirty"]));
        let upstream = s.spawn(|| get(&["diff", "--name-only", "-z", "--relative", range]));
        let counts = s.spawn(|| get(&["rev-list", "--left-right", "--count", range]));
        let prefix = git_with(dir, &guard, &["rev-parse", "--show-prefix"]);
        let j = |h: thread::ScopedJoinHandle<'_, String>| h.join().unwrap_or_default();
        (prefix, j(status), j(upstream), j(counts))
    });
    // `dir` relative to the top, with a trailing slash; empty at the top.
    let prefix = prefix.ok().flatten()?;
    let mut st = TreeState { changes: Vec::new(), ignored: HashSet::new(), upstream: Vec::new(), ahead: 0, behind: 0 };
    for mut c in parse_status(&status) {
        let Some(rel) = c.path.strip_prefix(&prefix) else { continue };
        let rel = rel.trim_end_matches('/').to_string();
        if rel.is_empty() {
            continue;
        }
        if c.code == "!!" {
            st.ignored.insert(rel);
        } else {
            c.path = rel;
            st.changes.push(c);
        }
    }
    st.upstream = upstream
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    let mut n = counts.split_whitespace().map(|n| n.parse().unwrap_or(0));
    st.ahead = n.next().unwrap_or(0);
    st.behind = n.next().unwrap_or(0);
    Some(st)
}
