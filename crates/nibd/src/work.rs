//! Organized work for agents (D-067): `nib-work`, a command beside `nib-mail` that an agent runs
//! to record its plan and to start a branch of its own in a git work tree of its own, so that
//! what it does can be shown, reviewed and landed like a pull request.
//!
//! Like the mailbox it is this same binary run as a plain command (`nibd work …`), so it works
//! with every assistant and over SSH. What it records is kept in the session's mailbox folder
//! (`work/<agent>.json`, `work/<agent>.plan.md`); a letter to `nib` tells the daemon to read it
//! again, and the daemon tells the client (`AiEvent::Work`).

use crate::mail::{self, Report};
use nib_proto::AiWork;
use serde::{Deserialize, Serialize};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// An agent's branch and where it is checked out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub branch: String,
    /// The branch it left and lands on.
    pub base: String,
    /// The top of its work tree.
    pub tree: PathBuf,
    /// Where it works: the project root's place in that work tree.
    pub dir: PathBuf,
}

fn record_file(mailbox: &Path, agent: &str) -> PathBuf {
    mailbox.join("work").join(format!("{agent}.json"))
}

fn plan_file(mailbox: &Path, agent: &str) -> PathBuf {
    mailbox.join("work").join(format!("{agent}.plan.md"))
}

/// The branch of the agent with mailbox id `agent`, if it started one.
pub fn record(mailbox: &Path, agent: &str) -> Option<Record> {
    serde_json::from_slice(&fs::read(record_file(mailbox, agent)).ok()?).ok()
}

/// Its plan, if it recorded one.
pub fn plan(mailbox: &Path, agent: &str) -> Option<String> {
    fs::read_to_string(plan_file(mailbox, agent)).ok().filter(|p| !p.trim().is_empty())
}

/// Record `text` as the agent's plan (also done for it when it asks approval for one).
pub fn set_plan(mailbox: &Path, agent: &str, text: &str) -> std::io::Result<()> {
    fs::create_dir_all(mailbox.join("work"))?;
    // Read by the window at any moment: never half there.
    crate::fs_ops::write_whole(&plan_file(mailbox, agent), text)
}

/// What is kept for an agent: its branch record, its plan, the review of its work.
const KEPT: [&str; 3] = ["json", "plan.md", "review.json"];

/// Copy what is kept for `agent` to `store` under `key` (its conversation's id), so that it
/// outlives this daemon: the mailbox folder is the session's, a conversation can be reopened
/// in another. What is gone here (the work landed) goes there too.
pub fn persist(mailbox: &Path, agent: &str, store: &Path, key: &str) {
    if key.is_empty() || fs::create_dir_all(store).is_err() {
        return;
    }
    for ext in KEPT {
        let (here, there) = (mailbox.join("work").join(format!("{agent}.{ext}")), store.join(format!("{key}.{ext}")));
        if here.is_file() {
            let _ = fs::copy(&here, &there);
        } else {
            let _ = fs::remove_file(&there);
        }
    }
}

/// Bring back what `persist` kept under `key`, for `agent` of this session. A branch whose
/// work tree is no longer there is not brought back.
pub fn restore(mailbox: &Path, agent: &str, store: &Path, key: &str) {
    if key.is_empty() || fs::create_dir_all(mailbox.join("work")).is_err() {
        return;
    }
    for ext in KEPT {
        let (here, there) = (mailbox.join("work").join(format!("{agent}.{ext}")), store.join(format!("{key}.{ext}")));
        if there.is_file() {
            let _ = fs::copy(&there, &here);
        }
    }
    if record(mailbox, agent).is_some_and(|r| !r.dir.is_dir()) {
        let _ = fs::remove_file(record_file(mailbox, agent));
    }
}

/// How things stand for `agent`, for the client.
pub fn state(mailbox: &Path, agent: &str) -> AiWork {
    let rec = record(mailbox, agent);
    let plan = plan(mailbox, agent).map(|p| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        p.hash(&mut h);
        h.finish()
    });
    AiWork {
        branch: rec.as_ref().map(|r| r.branch.clone()),
        dir: rec.as_ref().map(|r| nib_proto::path::to_wire(&r.dir)),
        base: rec.map(|r| r.base),
        plan,
        review: crate::review::stamp(mailbox, agent),
    }
}

/// Land `agent`'s branch on its base, checked out at `root`: everything it did as one commit
/// with `message`, then its work tree, branch and record are removed. `Err`: why not; nothing
/// was changed (what the agent had not committed is committed on its branch, that is all).
pub fn land(mailbox: &Path, agent: &str, root: &Path, message: &str) -> Result<String, String> {
    let rec = record(mailbox, agent).ok_or("this agent has no branch to land")?;
    let on = git(root, &["symbolic-ref", "--short", "-q", "HEAD"]).unwrap_or_default();
    if on != rec.base {
        let on = if on.is_empty() { "no branch (detached HEAD)".to_string() } else { on };
        return Err(format!("{} left {}, but the project is on {on}: check out {} to land it", rec.branch, rec.base, rec.base));
    }
    // A failed merge is undone with `reset --merge`, which would take staged changes with it.
    if git(root, &["diff", "--cached", "--quiet"]).is_err() {
        return Err(format!("something is staged in {}: commit or unstage it first, so that the landing is one commit of the agent's work only", root.display()));
    }
    // What it left uncommitted is part of its work.
    if !tree_git(root, &rec, &["status", "--porcelain"])?.is_empty() {
        tree_git(root, &rec, &["add", "-A"])?;
        tree_git(root, &rec, &["commit", "-q", "--no-verify", "-m", "nib: what was not committed"])?;
    }
    if let Err(e) = git(root, &["merge", "--squash", &rec.branch]) {
        let _ = git(root, &["reset", "--merge"]);
        return Err(e);
    }
    // Nothing staged by the merge: the branch had nothing the base lacks.
    let said = if git(root, &["diff", "--cached", "--quiet"]).is_ok() {
        format!("{} has nothing that {} lacks: nothing to commit", rec.branch, rec.base)
    } else {
        match git(root, &["commit", "-q", "-m", message]) {
            Ok(_) => git(root, &["log", "-1", "--format=%h %s"]).unwrap_or_default(),
            Err(e) => {
                // A commit hook said no: back to how it was.
                let _ = git(root, &["reset", "--merge"]);
                return Err(e);
            }
        }
    };
    // The work is on the base now; these only tidy up. The work tree can hold gigabytes of
    // build output, which take seconds to delete: it is moved aside (instant, on the same disk)
    // and deleted on a thread of its own, so the user hears it landed without waiting for that.
    let landed = rec.tree.parent().map(|p| p.join(LANDED));
    let aside = landed.as_ref().and_then(|d| {
        let name = rec.tree.file_name()?.to_string_lossy();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        fs::create_dir_all(d).ok()?;
        fs::rename(&rec.tree, d.join(format!("{name}-{stamp}"))).ok()
    });
    match (aside, landed) {
        // git forgets the work tree that is not where it was.
        (Some(()), Some(landed)) => {
            let _ = git(root, &["worktree", "prune"]);
            std::thread::spawn(move || sweep(&landed));
        }
        _ => {
            let _ = git(root, &["worktree", "remove", "--force", &rec.tree.to_string_lossy()]);
        }
    }
    let _ = git(root, &["branch", "-D", &rec.branch]);
    let _ = fs::remove_file(record_file(mailbox, agent));
    let _ = fs::remove_file(plan_file(mailbox, agent));
    crate::review::forget(mailbox, agent);
    let _ = git(root, &["update-ref", "-d", &crate::diff::round_ref(&rec.branch)]);
    Ok(format!("{said}\nlanded {} on {}", rec.branch, rec.base))
}

/// Where landed work trees wait to be deleted, beside the work trees (no slug starts with a dot).
const LANDED: &str = ".landed";

/// Delete everything in `landed`: the work tree just landed, and any a nibd that stopped
/// meanwhile left behind.
fn sweep(landed: &Path) {
    for e in fs::read_dir(landed).into_iter().flatten().flatten() {
        let _ = fs::remove_dir_all(e.path());
    }
}

/// git at `dir`: its stdout when it succeeded, else what it said.
/// git in an agent's work tree, run by the daemon (landing): pinned to the repository's own
/// administrative folder for that tree, so neither the tree's `.git` file nor that folder's
/// `commondir` is followed (both are the agent's to write, and a git folder of its own making
/// would bring a config of its own), and with background git's [`crate::git::guard`].
fn tree_git(root: &Path, rec: &Record, args: &[&str]) -> Result<String, String> {
    let common = PathBuf::from(git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
    // The folder git keeps for the tree: the one whose `gitdir` names it (usually its own name).
    let ours = |admin: &Path| fs::read_to_string(admin.join("gitdir")).is_ok_and(|g| canonical_near(Path::new(g.trim())) == canonical_near(&rec.tree.join(".git")));
    let named = rec.tree.file_name().map(|n| common.join("worktrees").join(n));
    let admin = named
        .filter(|a| ours(a))
        .or_else(|| fs::read_dir(common.join("worktrees")).ok()?.flatten().map(|e| e.path()).find(|a| ours(a)))
        .ok_or_else(|| format!("git keeps no work tree at {}", rec.tree.display()))?;
    // git follows `commondir` for branches even with `GIT_COMMON_DIR` set: back to the repository's.
    let points = fs::read_to_string(admin.join("commondir")).map(|c| canonical_near(&admin.join(c.trim())));
    if points.ok() != Some(canonical_near(&common)) {
        fs::write(admin.join("commondir"), "../..\n").map_err(|e| format!("{}: {e}", admin.display()))?;
    }
    let env = [("GIT_DIR", admin.as_path()), ("GIT_COMMON_DIR", common.as_path()), ("GIT_WORK_TREE", rec.tree.as_path())];
    let guard = crate::git::guard_env(&rec.tree, &env).ok_or("the work tree's git config names a filter nib cannot turn off: not landed")?;
    let guard: Vec<&str> = guard.iter().map(String::as_str).collect();
    git_in(&rec.tree, &env, &[&guard[..], args].concat())
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    git_in(dir, &[], args)
}

fn git_in(dir: &Path, env: &[(&str, &Path)], args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(env.iter().copied())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    if out.status.success() {
        Ok(text(&out.stdout))
    } else {
        Err([text(&out.stdout), text(&out.stderr)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n"))
    }
}

/// `fix login bug!` -> `fix-login-bug`: lowercase letters, digits and single dashes, at most 40.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.chars().take(40).collect::<String>().trim_end_matches('-').to_string()
}

const USAGE: &str = "usage:
  nib-work plan FILE|-                  record your plan (Markdown; - reads it from stdin). Run it again when the plan changes
  nib-work start SLUG [--fix] [--carry] make your own branch (nib/feat/SLUG, or nib/fix/SLUG) in your own work tree, and print where to work.
                                        --carry: also move what you already changed in the shared project folder into it
  nib-work carry                        move what you changed in the shared project folder into your work tree
  nib-work status                       your branch, where it is checked out, its base, the mode you work in, and whether worktree-per-feature is on
  nib-work sync                         merge the base branch into yours (when it moved, or the user's merge hit a conflict)";

/// The exit code with which `nib-work guard` says no; the hook's command turns it into Claude
/// Code's "blocked" (2), so that any other failure (an older nibd) lets the edit through.
pub const GUARD_NO: i32 = 3;

/// What `nib-work` needs to know about the agent that runs it.
pub struct Ctx {
    /// `$NIB_MAILBOX`.
    pub mailbox: PathBuf,
    /// `$NIB_AGENT`.
    pub agent: String,
    /// `$NIB_ROOT`: the project root.
    pub root: PathBuf,
    /// `$NIBDIR`: where the work trees go.
    pub nibdir: PathBuf,
}

/// `nibd work …`, run by an agent. Returns the exit code; prints to stdout / stderr.
pub fn cli(args: &[String]) -> i32 {
    if matches!(args.first().map(String::as_str), Some("-h" | "--help" | "help")) {
        println!("{USAGE}");
        return 0;
    }
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let (Some(mailbox), Ok(agent), Some(root), Some(nibdir)) = (var("NIB_MAILBOX"), std::env::var("NIB_AGENT"), var("NIB_ROOT"), var("NIBDIR")) else {
        eprintln!("nib-work: only works inside an agent that nib started (NIB_MAILBOX, NIB_AGENT, NIB_ROOT and NIBDIR are not all set)");
        return 2;
    };
    let stdin = || {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map(|_| s)
    };
    let (out, code) = run(&Ctx { mailbox, agent, root, nibdir }, args, stdin);
    if code == 0 {
        println!("{out}");
    } else {
        eprintln!("{out}");
    }
    code
}

/// `cli` for the agent `ctx` describes: what to print, and the exit code.
pub fn run(ctx: &Ctx, args: &[String], stdin: impl FnOnce() -> std::io::Result<String>) -> (String, i32) {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let told = || mail::report(&ctx.mailbox, &ctx.agent, &Report::Work);
    match words.as_slice() {
        ["plan", file] => {
            let text = if *file == "-" { stdin() } else { fs::read_to_string(file) };
            let text = match text {
                Ok(t) if !t.trim().is_empty() => t,
                Ok(_) => return ("nib-work plan: the plan is empty".to_string(), 1),
                Err(e) => return (format!("nib-work plan: {file}: {e}"), 1),
            };
            if let Err(e) = set_plan(&ctx.mailbox, &ctx.agent, &text) {
                return (format!("nib-work plan: {e}"), 1);
            }
            told();
            (format!("plan recorded ({} lines): the user sees it beside your changes. Run this again if the plan changes.", text.lines().count()), 0)
        }
        ["start", rest @ ..] => {
            let (fix, take) = (rest.contains(&"--fix"), rest.contains(&"--carry"));
            let names: Vec<&&str> = rest.iter().filter(|w| !matches!(**w, "--fix" | "--carry")).collect();
            let [name] = names.as_slice() else { return (USAGE.to_string(), 2) };
            match start(ctx, name, fix) {
                Ok(rec) => {
                    told();
                    let carried = if take {
                        match carry(ctx, &rec) {
                            Ok(said) => format!("\n{said}"),
                            Err(e) => format!("\nnib-work start --carry: the branch is made, but nothing was moved: {e}\nFix that, then run nib-work carry."),
                        }
                    } else {
                        String::new()
                    };
                    (
                        format!(
                            "branch {} from {}, in a work tree of its own:\n  {}\n\
                             Work only there, for every command and every edit: cd into it at the start of each command (or give it as the command's working folder), and use absolute paths under it for files you read and edit. \
                             It has the tracked files of {} and nothing else (no build output, no untracked or ignored files: build or copy what you need). \
                             Commit on this branch as you go. Do not switch branches there, and do not merge or push: \
                             when the work is done and checked, run nib-mail done, and the user lands it.",
                            rec.branch,
                            rec.base,
                            rec.dir.display(),
                            rec.base
                        ) + &carried,
                        0,
                    )
                }
                Err(e) => (format!("nib-work start: {e}"), 1),
            }
        }
        ["carry"] => {
            let Some(r) = record(&ctx.mailbox, &ctx.agent) else { return ("nib-work carry: you have no branch yet: nib-work start SLUG --carry".to_string(), 1) };
            match carry(ctx, &r) {
                Ok(said) => (said, 0),
                Err(e) => (format!("nib-work carry: {e}"), 1),
            }
        }
        ["guard"] => {
            let input = stdin().unwrap_or_default();
            match guard(ctx, &input) {
                Some(why) => (why, GUARD_NO),
                None => (String::new(), 0),
            }
        }
        ["status"] => {
            let mode = mail::mode_of(&ctx.mailbox, &ctx.agent).unwrap_or_else(|| "unknown".to_string());
            let plan = if plan(&ctx.mailbox, &ctx.agent).is_some() { "recorded" } else { "none yet (nib-work plan FILE)" };
            let branch = match record(&ctx.mailbox, &ctx.agent) {
                Some(r) => format!("branch: {}\nwork in: {}\nbase:   {}", r.branch, r.dir.display(), r.base),
                None => format!("branch: none yet (nib-work start SLUG); you are in the shared work tree, {}", ctx.root.display()),
            };
            let wpf = if mail::wpf(&ctx.mailbox) {
                "on (every change goes on a branch of your own: nib-work start; never edit the shared project folder)"
            } else {
                "off (change files in the project folder; accepting your work commits nothing)"
            };
            (format!("{branch}\nplan:   {plan}\nmode:   {mode}\nwpf:    {wpf}"), 0)
        }
        ["sync"] => {
            let Some(r) = record(&ctx.mailbox, &ctx.agent) else { return ("nib-work sync: you have no branch (nib-work start SLUG)".to_string(), 1) };
            match git(&r.tree, &["merge", "--no-edit", &r.base]) {
                Ok(out) => (format!("{out}\n{} is merged into {}.", r.base, r.branch), 0),
                Err(e) => (
                    format!("{e}\nThe merge of {} stopped. In {}: resolve the conflicts, git add the files, git commit; then check your work again.", r.base, r.tree.display()),
                    1,
                ),
            }
        }
        _ => (USAGE.to_string(), 2),
    }
}

/// Move what the agent changed in the shared project folder (`mail::stray`) into its work tree,
/// and put those files back as they are at HEAD there. Tracked files go as a patch (three-way, so
/// a base that moved since is fine); new files are copied. Nothing is changed in the shared
/// folder unless all of it went in. What it says to the agent.
fn carry(ctx: &Ctx, rec: &Record) -> Result<String, String> {
    let files = mail::stray(&ctx.mailbox, &ctx.agent, &ctx.root);
    if files.is_empty() {
        return Ok("nothing to carry: you have no changes left in the shared project folder.".to_string());
    }
    let root = &ctx.root;
    let (tracked, new): (Vec<&String>, Vec<&String>) = files.iter().partition(|f| git(root, &["cat-file", "-e", &format!("HEAD:./{f}")]).is_ok());
    for f in &new {
        if rec.dir.join(f).exists() {
            return Err(format!("{f} is new in the shared project folder, but your work tree has one too: move it by hand"));
        }
    }
    if !tracked.is_empty() {
        let mut args = vec!["diff", "--binary", "HEAD", "--"];
        args.extend(tracked.iter().map(|f| f.as_str()));
        let patch = git_raw(root, &args)?;
        let file = ctx.mailbox.join("work").join(format!("{}.carry.patch", ctx.agent));
        fs::create_dir_all(ctx.mailbox.join("work")).map_err(|e| e.to_string())?;
        fs::write(&file, patch).map_err(|e| e.to_string())?;
        let applied = git(&rec.tree, &["apply", "--3way", &file.to_string_lossy()]);
        let _ = fs::remove_file(&file);
        applied.map_err(|e| format!("{e}\nyour changes did not apply in your work tree; the shared project folder is untouched"))?;
    }
    for f in &new {
        let (from, to) = (root.join(f), rec.dir.join(f));
        if let Some(p) = to.parent() {
            fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        fs::copy(&from, &to).map_err(|e| format!("{f}: {e}"))?;
    }
    // In the work tree now: the shared folder goes back to how it was.
    if !tracked.is_empty() {
        let mut args = vec!["restore", "--source=HEAD", "--staged", "--worktree", "--"];
        args.extend(tracked.iter().map(|f| f.as_str()));
        if git(root, &args).is_err() {
            // A sandbox (Codex's) may not let the agent write the repository's index: then the
            // files come back as the index has them, which leaves the index as it is.
            let mut args = vec!["checkout-index", "-f", "--"];
            args.extend(tracked.iter().map(|f| f.as_str()));
            git(root, &args)?;
        }
    }
    for f in &new {
        let _ = fs::remove_file(root.join(f));
    }
    Ok(format!(
        "carried into {}: {}. They are back as they were in the shared project folder. If another agent had also changed one of them there, its changes came along: check the diff.",
        rec.dir.display(),
        files.join(", ")
    ))
}

/// For nib's Claude Code hook before an edit tool runs: why the edit in `input` (the hook's
/// JSON) must not happen, if it must not. With worktree-per-feature on, an agent edits only in
/// its own work tree: a file of the shared project folder that git does not ignore is refused.
fn guard(ctx: &Ctx, input: &str) -> Option<String> {
    if !mail::wpf(&ctx.mailbox) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let given = ["file_path", "notebook_path", "path"].iter().find_map(|k| v["tool_input"][k].as_str())?;
    let mut path = PathBuf::from(given);
    if path.is_relative() {
        path = PathBuf::from(v["cwd"].as_str()?).join(path);
    }
    let path = canonical_near(&path);
    let root = canonical_near(&ctx.root);
    let rel = path.strip_prefix(&root).ok()?;
    if let Some(r) = record(&ctx.mailbox, &ctx.agent) {
        if path.starts_with(canonical_near(&r.tree)) {
            return None;
        }
    }
    // Ignored files never land; they are not the user's work to protect.
    if ignored(&root, rel) {
        return None;
    }
    let to = match record(&ctx.mailbox, &ctx.agent) {
        Some(r) => format!("Edit {} instead.", r.dir.join(rel).display()),
        None => "Record your plan (nib-work plan FILE), run nib-work start SLUG, and edit the same file under the work tree it prints.".to_string(),
    };
    Some(format!(
        "nib: worktree-per-feature is on, and {} is in the shared project folder, where the user's own work is. Agents change files only on a branch of their own (nib-work skill). {to}",
        path.display()
    ))
}

/// Whether git ignores `rel` (a path under `root`, which may not exist yet).
pub fn ignored(root: &Path, rel: &Path) -> bool {
    git(root, &["check-ignore", "-q", "--", &rel.to_string_lossy()]).is_ok()
}

/// `p` with its nearest existing ancestor made canonical (the file may not exist yet).
pub fn canonical_near(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = p;
    loop {
        if let Ok(c) = nib_proto::path::canonical(at) {
            return rest.iter().rev().fold(c, |acc: PathBuf, part| acc.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(up), Some(name)) => {
                rest.push(name.to_os_string());
                at = up;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// `git` in `dir`, its output as it is (a patch must keep its last newline).
fn git_raw(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).output().map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn start(ctx: &Ctx, name: &str, fix: bool) -> Result<Record, String> {
    if let Some(r) = record(&ctx.mailbox, &ctx.agent) {
        return Err(format!("you already have a branch, {}, in {}", r.branch, r.dir.display()));
    }
    if plan(&ctx.mailbox, &ctx.agent).is_none() {
        return Err("record your plan first: nib-work plan FILE".to_string());
    }
    let slug = slug(name);
    if slug.is_empty() {
        return Err(format!("{name:?} has no letters or digits to name a branch with"));
    }
    let root = &ctx.root;
    git(root, &["rev-parse", "--show-toplevel"]).map_err(|_| format!("{} is not in a git repository", root.display()))?;
    // The root's place in the repository (with a trailing slash; empty at the top).
    let prefix = git(root, &["rev-parse", "--show-prefix"])?;
    let base = git(root, &["symbolic-ref", "--short", "-q", "HEAD"]).map_err(|_| "the project is not on a branch (detached HEAD): there is nothing to land on".to_string())?;
    git(root, &["rev-parse", "--verify", "-q", "HEAD"]).map_err(|_| format!("{base} has no commits yet: make the first commit before branching"))?;
    let branch = format!("nib/{}/{slug}", if fix { "fix" } else { "feat" });
    if git(root, &["rev-parse", "--verify", "-q", &format!("refs/heads/{branch}")]).is_ok() {
        return Err(format!("the branch {branch} already exists: pick another name"));
    }
    let tree = ctx.nibdir.join("worktrees").join(&slug);
    if tree.exists() {
        return Err(format!("{} already exists: pick another name", tree.display()));
    }
    fs::create_dir_all(ctx.nibdir.join("worktrees")).map_err(|e| e.to_string())?;
    git(root, &["worktree", "add", "-q", "-b", &branch, &tree.to_string_lossy(), "HEAD"])?;
    let tree = nib_proto::path::canonical(&tree).unwrap_or(tree);
    let dir = if prefix.is_empty() { tree.clone() } else { tree.join(prefix.trim_end_matches('/')) };
    let rec = Record { branch, base, tree, dir };
    fs::create_dir_all(ctx.mailbox.join("work")).map_err(|e| e.to_string())?;
    crate::fs_ops::write_whole(&record_file(&ctx.mailbox, &ctx.agent), serde_json::to_vec_pretty(&rec).unwrap_or_default()).map_err(|e| e.to_string())?;
    Ok(rec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Fix the Login bug!"), "fix-the-login-bug");
        assert_eq!(slug("  --a__b--  "), "a-b");
        assert_eq!(slug("!!!"), "");
        assert_eq!(slug(&"x".repeat(60)).len(), 40);
    }

    /// Worktree-per-feature (D-080): what an agent left in the shared project folder is not
    /// done while it is on; `--carry` moves it to the agent's branch; the hook's guard refuses
    /// edits there; off, the same summary goes through and says what is left uncommitted.
    #[test]
    fn worktree_per_feature_holds_an_agent_to_its_branch() {
        let dir = std::env::temp_dir().join(format!("nibd-wpf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (repo, mailbox, nibdir) = (dir.join("repo"), dir.join("mailbox"), dir.join("nibdir"));
        fs::create_dir_all(repo.join("app")).unwrap();
        fs::create_dir_all(mailbox.join("outbox")).unwrap();
        if git(&repo, &["init", "-q", "-b", "trunk"]).is_err() {
            return; // no git on this machine
        }
        for args in [&["config", "user.name", "T"][..], &["config", "user.email", "t@example.com"], &["config", "commit.gpgsign", "false"]] {
            git(&repo, args).unwrap();
        }
        fs::write(repo.join("app/a.txt"), "one\n").unwrap();
        fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-q", "-m", "first"]).unwrap();
        let root = repo.join("app");
        let ctx = Ctx { mailbox: mailbox.clone(), agent: "claude-1".into(), root: root.clone(), nibdir };
        let go = |args: &[&str]| run(&ctx, &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(), || Ok("# Plan\n1. do it\n".to_string()));
        let done = || mail::run_in(&mailbox, "claude-1", Some(&root), &["done", "--asked", "a sum", "--did", "added it"].map(String::from));
        let guard_on = |path: &Path| {
            let input = serde_json::json!({ "tool_name": "Edit", "cwd": root, "tool_input": { "file_path": path } }).to_string();
            run(&ctx, &["guard".to_string()], || Ok(input))
        };

        // The agent edited the project folder: a tracked file, a new one, and an ignored one.
        fs::write(root.join("a.txt"), "one\nmore\n").unwrap();
        fs::write(root.join("n.txt"), "new\n").unwrap();
        fs::write(root.join("x.log"), "noise\n").unwrap();
        let me = mail::Entry {
            id: "claude-1".into(),
            name: "claude-1".into(),
            assistant: "Claude Code".into(),
            model: None,
            task: "a sum".into(),
            busy: true,
            files: vec!["a.txt".into(), "n.txt".into(), "x.log".into(), "/elsewhere/notes.md".into()],
            mode: "auto".into(),
        };
        mail::write_roster(&mailbox, &[me]);
        assert_eq!(mail::stray(&mailbox, "claude-1", &root), ["a.txt", "n.txt"], "ignored files and files outside the project do not count");

        // On (no file yet: on by default): status says so, the guard refuses, done refuses.
        assert!(go(&["status"]).0.contains("wpf:    on"), "{}", go(&["status"]).0);
        let (why, code) = guard_on(&root.join("a.txt"));
        assert!(code == GUARD_NO && why.contains("worktree-per-feature is on") && why.contains("nib-work start"), "{code} {why}");
        assert_eq!(guard_on(&root.join("x.log")).1, 0, "an ignored file may be edited");
        assert_eq!(guard_on(Path::new("/elsewhere/notes.md")).1, 0, "outside the project");
        let (out, code) = done();
        assert!(code == 1 && out.contains("refused") && out.contains("a.txt, n.txt") && out.contains("--carry"), "{out}");
        assert!(mail::collect(&mailbox).is_empty(), "the user is not told it is done");

        // Off: the guard lets it be, and done goes through, saying what stays uncommitted.
        mail::set_wpf(&mailbox, false);
        assert!(go(&["status"]).0.contains("wpf:    off"));
        assert_eq!(guard_on(&root.join("a.txt")).1, 0);
        assert_eq!(done().1, 0);
        let letters = mail::collect(&mailbox);
        let f: nib_proto::AiFinished = serde_json::from_str(&letters[0].text).unwrap();
        assert_eq!(f.stray, ["a.txt", "n.txt"]);

        // On again: the agent moves its work onto a branch with --carry.
        mail::set_wpf(&mailbox, true);
        assert_eq!(go(&["plan", "-"]).1, 0);
        let (out, code) = go(&["start", "Add Sum", "--carry"]);
        assert!(code == 0 && out.contains("carried into") && out.contains("a.txt, n.txt"), "{out}");
        let rec = record(&mailbox, "claude-1").unwrap();
        assert_eq!(fs::read_to_string(rec.dir.join("a.txt")).unwrap(), "one\nmore\n", "the change is on the branch");
        assert_eq!(fs::read_to_string(rec.dir.join("n.txt")).unwrap(), "new\n");
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "one\n", "and the project folder is as it was");
        assert!(!root.join("n.txt").exists());
        assert_eq!(git(&repo, &["status", "--porcelain"]).unwrap(), "", "nothing of it is left there (x.log is ignored)");
        assert_eq!(go(&["carry"]).0, "nothing to carry: you have no changes left in the shared project folder.");
        // Its own work tree is where it edits; the project folder stays refused and says where to go.
        assert_eq!(guard_on(&rec.dir.join("a.txt")).1, 0);
        let (why, code) = guard_on(&root.join("a.txt"));
        assert!(code == GUARD_NO && why.contains(&rec.dir.join("a.txt").display().to_string()), "{why}");
        // Done now goes through, and lands.
        assert_eq!(done().1, 0);
        let f: nib_proto::AiFinished = serde_json::from_str(&mail::collect(&mailbox).last().unwrap().text).unwrap();
        assert!(f.stray.is_empty());
        land(&mailbox, "claude-1", &root, "Add sum").unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "one\nmore\n");
        assert_eq!(git(&repo, &["log", "--format=%s", "-1"]).unwrap(), "Add sum");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_then_branch_in_a_work_tree_then_sync() {
        let dir = std::env::temp_dir().join(format!("nibd-work-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (repo, mailbox, nibdir) = (dir.join("repo"), dir.join("mailbox"), dir.join("nibdir"));
        fs::create_dir_all(repo.join("app")).unwrap();
        fs::create_dir_all(mailbox.join("outbox")).unwrap();
        if git(&repo, &["init", "-q", "-b", "trunk"]).is_err() {
            return; // no git on this machine
        }
        for args in [&["config", "user.name", "T"][..], &["config", "user.email", "t@example.com"], &["config", "commit.gpgsign", "false"]] {
            git(&repo, args).unwrap();
        }
        fs::write(repo.join("app/a.txt"), "one\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-q", "-m", "first"]).unwrap();
        // The project root is a folder of the repository.
        let ctx = Ctx { mailbox: mailbox.clone(), agent: "claude-1".into(), root: repo.join("app"), nibdir };
        let go = |args: &[&str]| run(&ctx, &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(), || Ok("# Plan\n1. do it\n".to_string()));

        assert_eq!(go(&["status"]).1, 0);
        let (out, code) = go(&["start", "Add Sum"]);
        assert!(code == 1 && out.contains("plan first"), "{out}");
        assert_eq!(go(&["plan", "-"]).1, 0);
        assert_eq!(plan(&mailbox, "claude-1").as_deref(), Some("# Plan\n1. do it\n"));
        assert_eq!(state(&mailbox, "claude-1").branch, None);

        let (out, code) = go(&["start", "Add Sum"]);
        assert_eq!(code, 0, "{out}");
        let rec = record(&mailbox, "claude-1").unwrap();
        assert_eq!((rec.branch.as_str(), rec.base.as_str()), ("nib/feat/add-sum", "trunk"));
        assert!(rec.dir.ends_with("worktrees/add-sum/app") && rec.dir.join("a.txt").is_file(), "{}", rec.dir.display());
        assert_eq!(git(&rec.tree, &["symbolic-ref", "--short", "HEAD"]).unwrap(), "nib/feat/add-sum");
        let st = state(&mailbox, "claude-1");
        assert!(st.branch.as_deref() == Some("nib/feat/add-sum") && st.plan.is_some() && st.base.as_deref() == Some("trunk"));
        // One branch at a time; the daemon was told each time something changed.
        assert_eq!(go(&["start", "other"]).1, 1);
        assert_eq!(mail::collect(&mailbox).len(), 2);

        // The base moves on; sync brings it in.
        fs::write(repo.join("app/b.txt"), "two\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-q", "-m", "second"]).unwrap();
        let (out, code) = go(&["sync"]);
        assert_eq!(code, 0, "{out}");
        assert!(rec.dir.join("b.txt").is_file());

        // Kept by conversation: reopened in another session, under another name, it is all back.
        let (store, later) = (dir.join("store"), dir.join("mailbox2"));
        persist(&mailbox, "claude-1", &store, "conv-7");
        restore(&later, "claude-4", &store, "conv-7");
        assert_eq!(record(&later, "claude-4"), Some(rec.clone()));
        assert_eq!(plan(&later, "claude-4").as_deref(), Some("# Plan\n1. do it\n"));
        restore(&later, "claude-5", &store, "another");
        assert!(record(&later, "claude-5").is_none());

        // The agent's work: one commit, and a file it never committed.
        fs::write(rec.dir.join("a.txt"), "one\nmore\n").unwrap();
        git(&rec.tree, &["commit", "-q", "-am", "wip"]).unwrap();
        fs::write(rec.dir.join("new.txt"), "n\n").unwrap();
        // Something staged in the project: refused, and nothing changed.
        fs::write(repo.join("app/b.txt"), "two!\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        let e = land(&mailbox, "claude-1", &ctx.root, "Add sum").unwrap_err();
        assert!(e.contains("staged"), "{e}");
        git(&repo, &["restore", "--staged", "."]).unwrap();
        // A conflict with what the base did meanwhile: refused, the project as it was.
        git(&repo, &["checkout", "-q", "--", "."]).unwrap();
        fs::write(repo.join("app/a.txt"), "one\nother\n").unwrap();
        git(&repo, &["commit", "-q", "-am", "third"]).unwrap();
        assert!(land(&mailbox, "claude-1", &ctx.root, "Add sum").is_err());
        assert_eq!(git(&repo, &["status", "--porcelain"]).unwrap(), "");
        assert_eq!(fs::read_to_string(repo.join("app/a.txt")).unwrap(), "one\nother\n");
        assert!(record(&mailbox, "claude-1").is_some());
        // The agent syncs and resolves; then it lands as one commit, and its branch is gone.
        assert_eq!(go(&["sync"]).1, 1);
        fs::write(rec.dir.join("a.txt"), "one\nother\nmore\n").unwrap();
        git(&rec.tree, &["commit", "-q", "-am", "merged"]).unwrap();
        // The user's own unstaged change elsewhere stays theirs.
        fs::write(repo.join("app/b.txt"), "mine\n").unwrap();
        let out = land(&mailbox, "claude-1", &ctx.root, "Add sum\n\nAsked: a sum").unwrap();
        assert!(out.contains("Add sum") && out.contains("landed nib/feat/add-sum on trunk"), "{out}");
        assert_eq!(git(&repo, &["log", "--format=%s", "-2"]).unwrap(), "Add sum\nthird");
        assert_eq!(fs::read_to_string(repo.join("app/a.txt")).unwrap(), "one\nother\nmore\n");
        assert!(repo.join("app/new.txt").is_file());
        assert_eq!(git(&repo, &["status", "--porcelain"]).unwrap(), "M app/b.txt");
        assert!(!rec.tree.exists() && record(&mailbox, "claude-1").is_none() && plan(&mailbox, "claude-1").is_none());
        assert_eq!(git(&repo, &["worktree", "list", "--porcelain"]).unwrap().matches("worktree ").count(), 1, "git forgot it");
        // Its files are deleted after the answer, on their own.
        let landed = ctx.nibdir.join("worktrees").join(LANDED);
        let gone = || fs::read_dir(&landed).map_or(true, |mut d| d.next().is_none());
        for _ in 0..200 {
            if gone() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(gone(), "the landed work tree was deleted");
        assert!(git(&repo, &["rev-parse", "--verify", "-q", "refs/heads/nib/feat/add-sum"]).is_err());
        assert!(land(&mailbox, "claude-1", &ctx.root, "again").is_err());
        // Landed: nothing is kept any more, and a branch whose work tree is gone stays gone.
        restore(&later, "claude-6", &store, "conv-7");
        assert!(record(&later, "claude-6").is_none(), "its work tree was removed");
        persist(&mailbox, "claude-1", &store, "conv-7");
        assert!(!store.join("conv-7.json").exists() && !store.join("conv-7.plan.md").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// An agent may write its work tree's `.git` file (it is in its tree) and git's folder for
    /// that tree: pointed at a git folder of its own making, whose config runs things, landing
    /// must still use the repository's own and run none of it.
    #[cfg(unix)]
    #[test]
    fn landing_ignores_a_git_folder_the_agent_made_for_its_work_tree() {
        let dir = std::env::temp_dir().join(format!("nibd-land-pinned-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (repo, mailbox, nibdir, marks) = (dir.join("repo"), dir.join("mailbox"), dir.join("nibdir"), dir.join("marks"));
        for d in [&repo, &mailbox.join("outbox"), &marks] {
            fs::create_dir_all(d).unwrap();
        }
        if git(&repo, &["init", "-q", "-b", "trunk"]).is_err() {
            return; // no git on this machine
        }
        for args in [&["config", "user.name", "T"][..], &["config", "user.email", "t@example.com"], &["config", "commit.gpgsign", "false"]] {
            git(&repo, args).unwrap();
        }
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-q", "-m", "first"]).unwrap();
        let ctx = Ctx { mailbox: mailbox.clone(), agent: "codex-1".into(), root: repo.clone(), nibdir };
        let go = |args: &[&str]| run(&ctx, &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(), || Ok("# Plan\n".to_string()));
        assert_eq!(go(&["plan", "-"]).1, 0);
        assert_eq!(go(&["start", "pinned"]).1, 0);
        let rec = record(&mailbox, "codex-1").unwrap();
        fs::write(rec.tree.join("b.txt"), "the agent's work\n").unwrap();

        // The agent's git folder: a copy of the real one, with a config and hooks of its own.
        let fake = dir.join("nibdir/fake");
        let real_admin = PathBuf::from(git(&rec.tree, &["rev-parse", "--absolute-git-dir"]).unwrap());
        assert!(Command::new("cp").arg("-R").arg(git(&repo, &["rev-parse", "--absolute-git-dir"]).unwrap()).arg(&fake).status().unwrap().success());
        let fake_admin = fake.join("worktrees").join(real_admin.file_name().unwrap());
        fs::write(fake_admin.join("commondir"), "../..\n").unwrap();
        let hooks = dir.join("nibdir/hooks");
        fs::create_dir_all(&hooks).unwrap();
        for hook in ["post-commit", "reference-transaction", "post-index-change"] {
            fs::write(hooks.join(hook), format!("#!/bin/sh\ntouch '{}/{hook}'\n", marks.display())).unwrap();
            fs::set_permissions(hooks.join(hook), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        let mut config = fs::read_to_string(fake.join("config")).unwrap();
        config.push_str(&format!("[core]\n\tfsmonitor = \"touch '{}/fsmonitor'; false #\"\n\thooksPath = {}\n", marks.display(), hooks.display()));
        fs::write(fake.join("config"), config).unwrap();
        // It points its tree there (its `.git` file), and git's own folder for the tree too.
        fs::write(rec.tree.join(".git"), format!("gitdir: {}\n", fake_admin.display())).unwrap();
        fs::write(real_admin.join("commondir"), format!("{}\n", fake.display())).unwrap();

        let out = land(&mailbox, "codex-1", &repo, "The agent's work");
        let ran: Vec<String> = fs::read_dir(&marks).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        let landed = fs::read_to_string(repo.join("b.txt")).ok();
        let _ = fs::remove_dir_all(&dir);
        assert!(ran.is_empty(), "landing ran what the agent's git folder says: {ran:?} ({out:?})");
        assert!(out.is_ok(), "{out:?}");
        assert_eq!(landed.as_deref(), Some("the agent's work\n"), "{out:?}");
    }

    /// In a sandbox that may not write the repository's index (Codex's), carry still puts the
    /// shared folder back.
    #[cfg(unix)]
    #[test]
    fn carry_works_when_the_index_cannot_be_written() {
        let dir = std::env::temp_dir().join(format!("nibd-carry-ro-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (repo, mailbox, nibdir) = (dir.join("repo"), dir.join("mailbox"), dir.join("nibdir"));
        for d in [&repo, &mailbox.join("outbox")] {
            fs::create_dir_all(d).unwrap();
        }
        if git(&repo, &["init", "-q", "-b", "trunk"]).is_err() {
            return; // no git on this machine
        }
        for args in [&["config", "user.name", "T"][..], &["config", "user.email", "t@example.com"], &["config", "commit.gpgsign", "false"]] {
            git(&repo, args).unwrap();
        }
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-q", "-m", "first"]).unwrap();
        let ctx = Ctx { mailbox: mailbox.clone(), agent: "codex-2".into(), root: repo.clone(), nibdir };
        let go = |args: &[&str]| run(&ctx, &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(), || Ok("# Plan\n".to_string()));
        assert_eq!(go(&["plan", "-"]).1, 0);
        // A change made in the shared folder before the branch (what carry is for).
        fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        let me = mail::Entry { id: "codex-2".into(), name: "codex-2".into(), assistant: "Codex".into(), model: None, task: "t".into(), busy: true, files: vec!["a.txt".into()], mode: "auto".into() };
        mail::write_roster(&mailbox, &[me]);
        assert_eq!(go(&["start", "ro"]).1, 0);
        let rec = record(&mailbox, "codex-2").unwrap();
        // git's folder made read-only, as the sandbox has it: no `index.lock` can be made there.
        let gitdir = repo.join(".git");
        fs::set_permissions(&gitdir, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let (out, code) = go(&["carry"]);
        fs::set_permissions(&gitdir, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let (shared, tree) = (fs::read_to_string(repo.join("a.txt")).unwrap(), fs::read_to_string(rec.tree.join("a.txt")).unwrap());
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(code, 0, "{out}");
        assert_eq!((shared.as_str(), tree.as_str()), ("one\n", "one\ntwo\n"), "{out}");
    }
}
