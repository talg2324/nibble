//! The user's review of an agent's work (D-067, after Crit): comments on lines of its diff and
//! of its plan (D-070), sent to it in rounds; it answers each one (`nib-review reply`) and the
//! user resolves them.
//!
//! Kept beside the agent's branch record, in the session's mailbox folder
//! (`work/<agent>.review.json`). Like `nib-mail` and `nib-work`, `nib-review` is this binary
//! run by the agent as a plain command.

use crate::mail::{self, Report};
use crate::work;
use nib_proto::{Review, ReviewComment, ReviewOp, ReviewReply};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Who writes what the user writes.
pub const USER: &str = "user";

/// What is kept: the review, and the work tree as it was when the review was last sent (a
/// commit made for that, `diff::snapshot`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct Kept {
    review: Review,
    #[serde(default)]
    since: Option<String>,
    /// Ids given so far.
    #[serde(default)]
    next: u32,
}

fn file(mailbox: &Path, agent: &str) -> PathBuf {
    mailbox.join("work").join(format!("{agent}.review.json"))
}

fn kept(mailbox: &Path, agent: &str) -> Kept {
    fs::read(file(mailbox, agent)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn keep(mailbox: &Path, agent: &str, k: &Kept) -> std::io::Result<()> {
    fs::create_dir_all(mailbox.join("work"))?;
    crate::fs_ops::write_whole(&file(mailbox, agent), serde_json::to_vec_pretty(k).unwrap_or_default())
}

/// Forget the review (the agent's work landed).
pub fn forget(mailbox: &Path, agent: &str) {
    let _ = fs::remove_file(file(mailbox, agent));
}

/// A number that changes when the review does; `None`: there is none.
pub fn stamp(mailbox: &Path, agent: &str) -> Option<u64> {
    let bytes = fs::read(file(mailbox, agent)).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    Some(h.finish())
}

/// The commit `DiffScope::Since` compares with.
pub fn since(mailbox: &Path, agent: &str) -> Option<String> {
    kept(mailbox, agent).since
}

/// The review, with every comment on the file as it is now put where its lines are: `dir` is
/// where the agent works. A comment whose lines moved follows them; one whose lines are gone
/// is marked `drifted`. The same for comments on the plan, in the plan as it is recorded now.
pub fn load(mailbox: &Path, agent: &str, dir: &Path) -> Review {
    let mut k = kept(mailbox, agent);
    let before = k.review.clone();
    let plan = work::plan(mailbox, agent);
    for c in &mut k.review.comments {
        if c.plan {
            match &plan {
                Some(text) => anchor(c, text),
                None => c.drifted = true,
            }
            continue;
        }
        let Some(path) = c.path.as_deref().filter(|_| !c.old && c.start_line > 0) else { continue };
        match fs::read_to_string(dir.join(path)) {
            Ok(text) => anchor(c, &text),
            // The file is gone (or is not text any more).
            Err(_) => c.drifted = true,
        }
    }
    if k.review != before {
        let _ = keep(mailbox, agent, &k);
    }
    k.review.since = k.since.is_some();
    k.review
}

/// Put `c` on the lines of `text` that say what its anchor says.
fn anchor(c: &mut ReviewComment, text: &str) {
    let lines: Vec<&str> = text.lines().collect();
    let want: Vec<&str> = c.anchor.lines().collect();
    // Written without its lines (`nib ctl diff comment`): they are the ones it is on now.
    if c.anchor.is_empty() {
        let (a, b) = (c.start_line as usize, (c.end_line as usize).min(lines.len()));
        if a >= 1 && a <= b {
            c.anchor = lines[a - 1..b].join("\n");
            c.end_line = b as u32;
        }
        return;
    }
    let n = want.len().max(1);
    let at = |start: usize| start >= 1 && start + n - 1 <= lines.len() && lines[start - 1..start - 1 + n] == want[..];
    if at(c.start_line as usize) {
        c.drifted = false;
        return;
    }
    // The nearest place that says the same.
    let found = (1..=lines.len().saturating_sub(n - 1)).filter(|s| at(*s)).min_by_key(|s| (*s as i64 - c.start_line as i64).abs());
    match found.filter(|_| !want.is_empty()) {
        Some(start) => {
            c.start_line = start as u32;
            c.end_line = (start + n - 1) as u32;
            c.drifted = false;
        }
        None => {
            c.drifted = true;
            // Still inside the file, so it has a line to show on.
            let last = lines.len().max(1) as u32;
            c.end_line = c.end_line.min(last);
            c.start_line = c.start_line.min(c.end_line).max(1);
        }
    }
}

/// Carry out `op` for the user. `Err`: what was wrong with it. `Send` is `send`.
pub fn edit(mailbox: &Path, agent: &str, op: ReviewOp) -> Result<(), String> {
    let mut k = kept(mailbox, agent);
    let find = |k: &mut Kept, id: &str| k.review.comments.iter().position(|c| c.id == id).ok_or(format!("no comment {id}"));
    match op {
        ReviewOp::Comment(c) => {
            if c.body.trim().is_empty() {
                return Err("the comment is empty".to_string());
            }
            if c.plan {
                let lines = work::plan(mailbox, agent).ok_or("this agent has recorded no plan")?.lines().count() as u32;
                if c.path.is_some() || c.start_line == 0 || c.end_line < c.start_line || c.start_line > lines {
                    return Err(format!("a comment on the plan is on its lines: 1 to {lines}"));
                }
            }
            k.next += 1;
            k.review.comments.push(ReviewComment { id: format!("c{}", k.next), author: USER.to_string(), round: 0, resolved: false, drifted: false, replies: Vec::new(), body: c.body.trim().to_string(), ..c });
        }
        ReviewOp::Reply { to, body } => reply(&mut k, &to, &body, USER)?,
        ReviewOp::Resolve { comment, resolved } => {
            let i = find(&mut k, &comment)?;
            k.review.comments[i].resolved = resolved;
        }
        ReviewOp::Remove { comment } => {
            let i = find(&mut k, &comment)?;
            k.review.comments.remove(i);
        }
        ReviewOp::Send => return Err("sending is not an edit".to_string()),
    }
    keep(mailbox, agent, &k).map_err(|e| e.to_string())
}

fn reply(k: &mut Kept, to: &str, body: &str, author: &str) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("the reply is empty".to_string());
    }
    let c = k.review.comments.iter_mut().find(|c| c.id == to).ok_or(format!("no comment {to}"))?;
    k.next += 1;
    c.replies.push(ReviewReply { id: format!("r{}", k.next), body: body.trim().to_string(), author: author.to_string() });
    Ok(())
}

/// Where a comment is, as said to the agent: `src/a.rs:40-44`, `your plan:6-7`.
fn place(c: &ReviewComment) -> String {
    let lines = match (c.start_line, c.end_line) {
        (0, _) => String::new(),
        (a, b) if a == b => format!(":{a}"),
        (a, b) => format!(":{a}-{b}"),
    };
    let old = if c.old { " (a line you removed)" } else { "" };
    let moved = if c.drifted { " (these lines changed since; the numbers are approximate)" } else { "" };
    match &c.path {
        Some(p) => format!("{p}{lines}{old}{moved}"),
        None if c.plan => format!("your plan{lines}{moved}"),
        None => "the work as a whole".to_string(),
    }
}

/// One comment as text: its id and place, the lines it is about, what was said.
fn said(c: &ReviewComment) -> String {
    let mut out = format!("[{}] {}\n", c.id, place(c));
    // Enough of the lines to know them by.
    const QUOTED: usize = 6;
    let quoted: Vec<&str> = c.anchor.lines().collect();
    for l in quoted.iter().take(QUOTED) {
        out.push_str(&format!("  > {l}\n"));
    }
    if quoted.len() > QUOTED {
        out.push_str(&format!("  > … {} more lines\n", quoted.len() - QUOTED));
    }
    out.push_str(&format!("{}: {}\n", c.author, c.body));
    for r in &c.replies {
        out.push_str(&format!("  {}: {}\n", r.author, r.body));
    }
    out
}

/// Send the review: the next round. Gives what to say to the agent. `snapshot`: the agent's
/// work tree as it is now, as a commit (`None`: it has no work tree of its own).
pub fn send(mailbox: &Path, agent: &str, snapshot: Option<String>) -> Result<String, String> {
    let mut k = kept(mailbox, agent);
    let open: Vec<&ReviewComment> = k.review.comments.iter().filter(|c| !c.resolved).collect();
    if open.is_empty() {
        return Err("there are no open comments to send".to_string());
    }
    let round = k.review.round + 1;
    let n = open.len();
    let mut text = format!(
        "(nib: the user reviewed your work: round {round}, {n} open comment{}. For each one: make the change, or say why not, and answer it with `nib-review reply ID \"what you did\"`. \
         Do not leave one unanswered. When all are answered and you have checked your work again, run `nib-mail done` again. `nib-review comments` lists them again. nib-work skill, \"A review\".)\n",
        if n == 1 { "" } else { "s" }
    );
    if open.iter().any(|c| c.plan) {
        text.push_str(
            "(A comment on \"your plan\" is about the plan you recorded, and quotes its lines: if it changes what you will do, revise the plan and record it again with `nib-work plan FILE`, \
             change the work to match, then answer the comment.)\n",
        );
    }
    for c in open {
        text.push('\n');
        text.push_str(&said(c));
    }
    k.review.round = round;
    for c in k.review.comments.iter_mut().filter(|c| c.round == 0) {
        c.round = round;
    }
    if snapshot.is_some() {
        k.since = snapshot;
    }
    keep(mailbox, agent, &k).map_err(|e| e.to_string())?;
    Ok(text)
}

const USAGE: &str = "usage:
  nib-review comments [--all]     the user's open review comments on your work (--all: the resolved ones too)
  nib-review reply ID TEXT...     answer comment ID: what you changed, or why not";

/// `nibd review …`, run by an agent. Returns the exit code; prints to stdout / stderr.
pub fn cli(args: &[String]) -> i32 {
    if matches!(args.first().map(String::as_str), Some("-h" | "--help" | "help")) {
        println!("{USAGE}");
        return 0;
    }
    let (Some(mailbox), Ok(agent)) = (std::env::var_os("NIB_MAILBOX").map(PathBuf::from), std::env::var("NIB_AGENT")) else {
        eprintln!("nib-review: only works inside an agent that nib started (NIB_MAILBOX and NIB_AGENT are not set)");
        return 2;
    };
    let (out, code) = run(&mailbox, &agent, args);
    if code == 0 {
        println!("{out}");
    } else {
        eprintln!("{out}");
    }
    code
}

/// `cli` for the agent with mailbox id `agent`: what to print, and the exit code.
pub fn run(mailbox: &Path, agent: &str, args: &[String]) -> (String, i32) {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["comments"] | ["comments", "--all"] => {
            let all = words.len() == 2;
            let k = kept(mailbox, agent);
            let shown: Vec<String> = k.review.comments.iter().filter(|c| c.round > 0 && (all || !c.resolved)).map(|c| format!("{}{}", if c.resolved { "(resolved) " } else { "" }, said(c))).collect();
            (if shown.is_empty() { "no open review comments".to_string() } else { shown.join("\n") }, 0)
        }
        ["reply", id, text @ ..] if !text.is_empty() => {
            let mut k = kept(mailbox, agent);
            // What the agents call this one, if the roster knows.
            let me = mail::name_of(mailbox, agent).unwrap_or_else(|| agent.to_string());
            if !k.review.comments.iter().any(|c| c.id == *id && c.round > 0) {
                let ids: Vec<&str> = k.review.comments.iter().filter(|c| c.round > 0 && !c.resolved).map(|c| c.id.as_str()).collect();
                return (format!("nib-review: no comment {id}; the open ones are: {}", if ids.is_empty() { "none".to_string() } else { ids.join(", ") }), 1);
            }
            if let Err(e) = reply(&mut k, id, &text.join(" "), &me).and_then(|_| keep(mailbox, agent, &k).map_err(|e| e.to_string())) {
                return (format!("nib-review: {e}"), 1);
            }
            mail::report(mailbox, agent, &Report::Work);
            let left = k.review.comments.iter().filter(|c| c.round > 0 && !c.resolved && c.replies.last().is_none_or(|r| r.author == USER)).count();
            (format!("answered {id}; {left} still to answer"), 0)
        }
        _ => (USAGE.to_string(), 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(path: &str, start: u32, end: u32, anchor: &str, body: &str) -> ReviewOp {
        ReviewOp::Comment(ReviewComment { path: Some(path.into()), start_line: start, end_line: end, anchor: anchor.into(), body: body.into(), ..Default::default() })
    }

    #[test]
    fn comments_follow_their_lines_and_rounds_go_to_the_agent() {
        let dir = std::env::temp_dir().join(format!("nibd-review-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (mailbox, tree) = (dir.join("mailbox"), dir.join("tree"));
        fs::create_dir_all(mailbox.join("outbox")).unwrap();
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("a.rs"), "fn a() {}\nfn b() {\n    1\n}\n").unwrap();
        let me = "claude-1";
        let go = |args: &[&str]| run(&mailbox, me, &args.iter().map(|a| a.to_string()).collect::<Vec<_>>());

        assert!(stamp(&mailbox, me).is_none() && load(&mailbox, me, &tree).comments.is_empty());
        edit(&mailbox, me, comment("a.rs", 2, 3, "fn b() {\n    1", "Return 2 here.")).unwrap();
        edit(&mailbox, me, ReviewOp::Comment(ReviewComment { body: "Add a test.".into(), ..Default::default() })).unwrap();
        assert!(edit(&mailbox, me, comment("a.rs", 1, 1, "fn a() {}", "  ")).is_err());
        // Not sent yet: the agent sees nothing, and cannot answer.
        assert_eq!(go(&["comments"]), ("no open review comments".to_string(), 0));
        assert_eq!(go(&["reply", "c1", "done"]).1, 1);
        assert!(send(&mailbox, me, None).is_ok());
        let text = send(&mailbox, me, Some("abc123".into())).unwrap();
        assert!(text.contains("round 2, 2 open comments") && text.contains("[c1] a.rs:2-3\n  > fn b() {\n  >     1\nuser: Return 2 here.") && text.contains("[c2] the work as a whole"), "{text}");
        assert_eq!(since(&mailbox, me).as_deref(), Some("abc123"));
        let r = load(&mailbox, me, &tree);
        assert!(r.round == 2 && r.since && r.comments.iter().all(|c| c.round == 1));

        // The agent answers; the file moves under the comment, which follows it.
        let before = stamp(&mailbox, me);
        let (out, code) = go(&["reply", "c1", "now", "returns", "2"]);
        assert_eq!((out.as_str(), code), ("answered c1; 1 still to answer", 0));
        assert_ne!(stamp(&mailbox, me), before);
        assert_eq!(mail::collect(&mailbox).len(), 1);
        fs::write(tree.join("a.rs"), "// new\n// lines\nfn a() {}\nfn b() {\n    1\n}\n").unwrap();
        let c = load(&mailbox, me, &tree).comments.remove(0);
        assert_eq!((c.start_line, c.end_line, c.drifted, c.replies[0].author.as_str(), c.replies[0].body.as_str()), (4, 5, false, "claude-1", "now returns 2"));
        // Rewritten: it stays near where it was, marked.
        fs::write(tree.join("a.rs"), "fn a() {}\nfn b() {\n    2\n}\n").unwrap();
        let c = load(&mailbox, me, &tree).comments.remove(0);
        assert!(c.drifted && c.start_line <= c.end_line && c.end_line <= 4, "{c:?}");

        // Resolved comments are not sent again; with none open there is nothing to send.
        edit(&mailbox, me, ReviewOp::Reply { to: "c1".into(), body: "thanks".into() }).unwrap();
        edit(&mailbox, me, ReviewOp::Resolve { comment: "c1".into(), resolved: true }).unwrap();
        edit(&mailbox, me, ReviewOp::Remove { comment: "c2".into() }).unwrap();
        assert!(send(&mailbox, me, None).is_err());
        assert!(go(&["comments", "--all"]).0.starts_with("(resolved) [c1]"));
        assert!(edit(&mailbox, me, ReviewOp::Resolve { comment: "c9".into(), resolved: true }).is_err());
        forget(&mailbox, me);
        assert!(stamp(&mailbox, me).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn comments_on_the_plan_quote_it_and_follow_it() {
        let dir = std::env::temp_dir().join(format!("nibd-review-plan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (mailbox, tree) = (dir.join("mailbox"), dir.join("tree"));
        fs::create_dir_all(mailbox.join("outbox")).unwrap();
        fs::create_dir_all(&tree).unwrap();
        let me = "claude-1";
        let go = |args: &[&str]| run(&mailbox, me, &args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        let on_plan = |start: u32, end: u32, anchor: &str, body: &str| ReviewOp::Comment(ReviewComment { plan: true, start_line: start, end_line: end, anchor: anchor.into(), body: body.into(), ..Default::default() });

        // No plan, no lines, lines past its end: refused.
        assert!(edit(&mailbox, me, on_plan(1, 1, "", "why?")).is_err());
        work::set_plan(&mailbox, me, "# Add sum\n\n1. write it\n2. test it\n").unwrap();
        assert!(edit(&mailbox, me, on_plan(0, 0, "", "why?")).is_err() && edit(&mailbox, me, on_plan(9, 9, "", "why?")).is_err());
        // Written with its lines' text (the plan pane), and without (`nib ctl diff comment --on plan:4`).
        edit(&mailbox, me, on_plan(3, 4, "1. write it\n2. test it", "Test first.")).unwrap();
        edit(&mailbox, me, on_plan(4, 4, "", "With what?")).unwrap();
        let r = load(&mailbox, me, &tree);
        assert!(r.comments.iter().all(|c| c.plan && c.path.is_none() && !c.drifted) && r.comments[1].anchor == "2. test it", "{r:?}");

        let text = send(&mailbox, me, None).unwrap();
        assert!(text.contains("`nib-work plan FILE`") && text.contains("[c1] your plan:3-4\n  > 1. write it\n  > 2. test it\nuser: Test first.") && text.contains("[c2] your plan:4\n"), "{text}");
        assert!(go(&["comments"]).0.contains("[c2] your plan:4\n  > 2. test it\nuser: With what?"));
        assert_eq!(go(&["reply", "c2", "cargo", "test"]), ("answered c2; 1 still to answer".to_string(), 0));

        // The plan is recorded again: a comment follows its lines; one whose lines are gone
        // stays, marked, still quoting what it was about.
        work::set_plan(&mailbox, me, "# Add sum\n\nWhy: asked.\n\n1. test it\n2. test it\n3. write it\n").unwrap();
        let r = load(&mailbox, me, &tree);
        let (c1, c2) = (&r.comments[0], &r.comments[1]);
        assert!(c1.drifted && c1.anchor == "1. write it\n2. test it" && c1.end_line <= 7, "{c1:?}");
        assert_eq!((c2.start_line, c2.end_line, c2.drifted, c2.replies[0].body.as_str()), (6, 6, false, "cargo test"));
        assert!(go(&["comments"]).0.contains("[c1] your plan:3-4 (these lines changed since; the numbers are approximate)"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
