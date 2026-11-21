//! The agents' mailbox (D-049): how agents working in the same project at once tell each other
//! what they are doing, whatever assistant each one is.
//!
//! An agent sends with `nib-mail send NAME "text"` (the `nib-agent-mailbox` skill says when).
//! That is a script on its PATH that runs this same binary (`nibd mail …`) as a plain command: it drops a letter in a folder
//! the daemon made for the session (`$NIB_MAILBOX`), and the daemon that runs the agents picks
//! it up and delivers it. So it works with every assistant and over SSH, and no agent ever polls:
//! - delivery is the daemon's doing: the letter is said to the recipient as a message, which its
//!   harness shows it after its current step (an idle agent starts a turn for it);
//! - an agent that has nothing to do until another answers runs `nibd mail wait`, which blocks
//!   until a letter comes and prints it. While it waits, letters reach it there and are not also
//!   said to it.
//!
//! `nib-mail done --asked … --did …` (the `nib-finished` skill) is the agent telling the user its
//! job is done: a letter to `nib`, which the daemon turns into `AiEvent::Finished` for the
//! agent's own chat instead of delivering.
//!
//! Layout of the folder: `roster.json` (who is at work), `outbox/*.json` (letters not yet
//! delivered), `inbox/<name>.jsonl` (every letter to an agent), `inbox/<name>.read` (how many of
//! those it has seen), `waiting/<name>` (it is in `mail wait`).

use nib_proto::{AiAgent, AiFinished, AiId, AiStep, AiStepDone};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The sender of what nib itself tells the agents.
pub const NIB: &str = "nib";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Letter {
    pub from: String,
    /// An agent's name, or `all`.
    pub to: String,
    pub text: String,
    /// Unix seconds.
    pub at: u64,
}

/// One agent at work, for `mail who`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// The session's own id (`claude-3`, `$NIB_AGENT`): it never changes, and names the inbox.
    pub id: String,
    /// What the agents call it: made from its task (`fix-calc-sum`), or the id until it has one.
    pub name: String,
    pub assistant: String,
    pub model: Option<String>,
    /// What the user asked it to do (the start of its first message).
    pub task: String,
    pub busy: bool,
    /// The files it changed in this session (the last to change each), so another agent can
    /// tell whose work its own touches without asking everyone.
    #[serde(default)]
    pub files: Vec<String>,
}

/// Words that say nothing about a task.
const FILLER: &[&str] = &[
    "a", "an", "the", "to", "of", "in", "on", "at", "for", "with", "and", "or", "then", "this", "that", "these", "it", "its", "is", "are", "be", "by", "from", "as", "so",
    "please", "can", "could", "you", "me", "my", "i", "we", "our", "your", "do", "does", "now", "just", "also", "all", "some", "any", "what", "how", "why", "when", "which",
    "there", "here", "into", "about", "using", "use", "tool", "exactly", "one", "should", "would", "want", "need", "let", "lets", "make", "sure", "have", "has", "not", "if",
];

/// A name for an agent from what it was asked to do: the first three words that mean something,
/// `fix-calc-sum`. `None` when the task has none.
pub fn task_name(task: &str) -> Option<String> {
    let words: Vec<String> = task
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        // `calc.py` is `calc`; a sentence's full stop is nothing.
        .map(|w| w.split('.').next().unwrap_or("").to_lowercase())
        .filter(|w| w.chars().count() > 1 && w.chars().any(char::is_alphabetic) && !FILLER.contains(&w.as_str()))
        .take(3)
        .collect();
    let name: String = words.join("-").chars().take(28).collect();
    (!name.is_empty()).then(|| name.trim_end_matches('-').to_string())
}

/// An agent's id in the mailbox: its assistant and the session's number.
pub fn name(agent: AiAgent, id: AiId) -> String {
    agent.mail_name(id)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The session's mailbox folder, made if needed: in its scratch directory, else the temp dir.
pub fn dir(scratch: Option<&Path>) -> Option<PathBuf> {
    let dir = match scratch {
        Some(s) => s.join("mailbox"),
        None => std::env::temp_dir().join(format!("nibd-mailbox-{}", std::process::id())),
    };
    for sub in ["outbox", "inbox", "waiting", "bin"] {
        fs::create_dir_all(dir.join(sub)).ok()?;
    }
    // `nib-mail`: the command agents run (its folder goes on their PATH). One fixed name, so a
    // harness can be told once that it is allowed. It runs this very exe, wherever it was built
    // or installed. A `sh` script for Unix and for Git Bash (which is how Claude Code runs
    // commands on Windows), and on Windows also a `.cmd` for cmd and PowerShell.
    if let Ok(me) = std::env::current_exe() {
        let _ = write_launchers(&dir.join("bin"), &me);
    }
    Some(dir)
}

/// Write `nib-mail` (and on Windows `nib-mail.cmd`) into `bin`, each running `exe mail ARGS`.
fn write_launchers(bin: &Path, exe: &Path) -> std::io::Result<()> {
    let exe = exe.display().to_string();
    // `C:/…` is the form Git Bash and every MSYS tool take; on Unix there are no `\` to change.
    let sh_exe = if cfg!(windows) { exe.replace('\\', "/") } else { exe.clone() };
    let script = bin.join("nib-mail");
    fs::write(&script, format!("#!/bin/sh\nexec '{}' mail \"$@\"\n", sh_exe.replace('\'', "'\\''")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    }
    if cfg!(windows) {
        fs::write(bin.join("nib-mail.cmd"), format!("@\"{exe}\" mail %*\r\n"))?;
    }
    Ok(())
}

/// What a `nib ai run` sub-agent is doing, for its owner's chat (D-061): a letter to `nib` from
/// the owner's id, shown as a step of the owner's (the run, with the sub-agent's tool calls
/// nested in it), delivered to nobody.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Report {
    Step(AiStep),
    StepDone(AiStepDone),
}

/// Where to report to and as whom, when this process runs inside an agent nib started:
/// `$NIB_MAILBOX` and `$NIB_AGENT`, which the agent's commands inherit.
pub fn owner() -> Option<(PathBuf, String)> {
    let dir = PathBuf::from(std::env::var_os("NIB_MAILBOX")?);
    let me = std::env::var("NIB_AGENT").ok().filter(|m| !m.is_empty())?;
    dir.join("outbox").is_dir().then_some((dir, me))
}

/// Send `report` to the owner's chat. Best effort: a report that can't be written is dropped.
pub fn report(dir: &Path, owner: &str, report: &Report) {
    if let Ok(text) = serde_json::to_string(report) {
        let _ = post(dir, &Letter { from: owner.to_string(), to: NIB.into(), text, at: now() });
    }
}

/// Put a letter in the outbox for the daemon to deliver. Whole or not at all (written beside,
/// then renamed).
pub fn post(dir: &Path, letter: &Letter) -> std::io::Result<()> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("{nanos:032}-{}-{n}", std::process::id());
    let tmp = dir.join("outbox").join(format!(".{name}.tmp"));
    fs::write(&tmp, serde_json::to_vec(letter)?)?;
    fs::rename(tmp, dir.join("outbox").join(format!("{name}.json")))
}

/// Take every letter out of the outbox, oldest first.
pub fn collect(dir: &Path) -> Vec<Letter> {
    let Ok(read) = fs::read_dir(dir.join("outbox")) else { return Vec::new() };
    let mut files: Vec<PathBuf> = read.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|p| {
            let letter = fs::read(&p).ok().and_then(|b| serde_json::from_slice(&b).ok());
            let _ = fs::remove_file(&p);
            letter
        })
        .collect()
}

fn inbox(dir: &Path, name: &str) -> PathBuf {
    dir.join("inbox").join(format!("{name}.jsonl"))
}

/// Every letter `name` was ever sent.
fn letters(dir: &Path, name: &str) -> Vec<Letter> {
    fs::read_to_string(inbox(dir, name)).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn seen(dir: &Path, name: &str) -> usize {
    fs::read_to_string(dir.join("inbox").join(format!("{name}.read"))).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Add a letter to the inbox of the agent with id `to`.
pub fn deliver(dir: &Path, to: &str, letter: &Letter) {
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(inbox(dir, to)) {
        let _ = writeln!(f, "{}", serde_json::to_string(letter).unwrap_or_default());
    }
}

/// `name` is in `mail wait` (it said so within the last hour): letters reach it there.
pub fn waiting(dir: &Path, name: &str) -> bool {
    let fresh = |m: fs::Metadata| m.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age < Duration::from_secs(3600));
    fs::metadata(dir.join("waiting").join(name)).map(fresh).unwrap_or(false)
}

pub fn write_roster(dir: &Path, entries: &[Entry]) {
    let tmp = dir.join(".roster.tmp");
    if fs::write(&tmp, serde_json::to_vec_pretty(entries).unwrap_or_default()).is_ok() {
        let _ = fs::rename(tmp, dir.join("roster.json"));
    }
}

fn roster(dir: &Path) -> Vec<Entry> {
    fs::read(dir.join("roster.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// The letters `name` hasn't seen (from `from`, if given), marked seen.
pub fn unread(dir: &Path, name: &str, from: Option<&str>) -> Vec<Letter> {
    let all = letters(dir, name);
    let new: Vec<Letter> = all.iter().skip(seen(dir, name)).filter(|l| from.is_none_or(|f| l.from == f)).cloned().collect();
    if !new.is_empty() {
        let _ = fs::write(dir.join("inbox").join(format!("{name}.read")), all.len().to_string());
    }
    new
}

/// Letters as the daemon says them to an agent, in one message. Only a letter from another
/// agent to this one by name asks for an answer: one to `all` is news, and nib's own need none.
pub fn said(letters: &[Letter]) -> String {
    letters
        .iter()
        .map(|l| {
            let note = if l.from == NIB {
                String::new()
            } else if l.to == "all" {
                "\n\n(Sent to every agent: no answer needed. Write to it only if this collides with what you are doing.)".to_string()
            } else {
                // The sender may be blocked until the answer comes, and it only reads the
                // mailbox: what this agent says in its normal reply never reaches it.
                format!(
                    "\n\n({0} cannot see your replies to the user, and may be waiting. If this asks you something, answer it now, before going on, with this command: nib-mail send {0} \"your answer\")",
                    l.from
                )
            };
            format!("[nib mailbox] Message from {}:\n{}{note}", l.from, l.text)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn show(letters: &[Letter]) -> String {
    letters.iter().map(|l| format!("from {}: {}", l.from, l.text)).collect::<Vec<_>>().join("\n")
}

const USAGE: &str = "usage:
  nib-mail who                          who is working in this project (you are marked)
  nib-mail send NAME|all TEXT...        send a message (delivered by nib after the recipient's current step)
  nib-mail wait [--from NAME] [--timeout SECONDS]    block until a message comes, and print it (default 100 s)
  nib-mail inbox                        print messages you have not seen, without waiting
  nib-mail done --asked TEXT [--found TEXT] --did TEXT    tell the user the job you were given is done (at most 100 words in all)";

/// `nibd mail …`, run by an agent. Returns the exit code; prints to stdout / stderr.
pub fn cli(args: &[String]) -> i32 {
    let (Some(dir), Some(me)) = (std::env::var_os("NIB_MAILBOX").map(PathBuf::from), std::env::var("NIB_AGENT").ok()) else {
        eprintln!("nib-mail: only works inside an agent that nib started (NIB_MAILBOX and NIB_AGENT are not set)");
        return 2;
    };
    let (out, code) = run(&dir, &me, args);
    if code == 0 {
        println!("{out}");
    } else {
        eprintln!("{out}");
    }
    code
}

/// `cli`, for the agent `me` with its mailbox in `dir`: what to print, and the exit code.
pub fn run(dir: &Path, me: &str, args: &[String]) -> (String, i32) {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let everyone = roster(dir);
    // What the others call this agent.
    let my_name = everyone.iter().find(|e| e.id == me).map_or(me.to_string(), |e| e.name.clone());
    match words.as_slice() {
        ["who"] => {
            let lines: Vec<String> = everyone
                .iter()
                .map(|e| {
                    let you = if e.id == me { " (you)" } else { "" };
                    let model = e.model.as_deref().map(|m| format!(" {m}")).unwrap_or_default();
                    let files = if e.files.is_empty() { String::new() } else { format!("\n    changed: {}", e.files.join(", ")) };
                    format!("{}{you}  {}{model}  {}  {}{files}", e.name, e.assistant, if e.busy { "working" } else { "idle" }, e.task)
                })
                .collect();
            (if lines.is_empty() { "nobody else is here".to_string() } else { lines.join("\n") }, 0)
        }
        ["send", to, text @ ..] if !text.is_empty() => {
            let names: Vec<String> = everyone.iter().filter(|e| e.id != me).map(|e| e.name.clone()).collect();
            if *to != "all" && !names.iter().any(|n| n == to) {
                let who = if names.is_empty() { "no other agent is at work".to_string() } else { format!("the others are: {}", names.join(", ")) };
                return (format!("nib-mail: no agent named {to:?}; {who}"), 1);
            }
            match post(dir, &Letter { from: my_name, to: to.to_string(), text: text.join(" "), at: now() }) {
                Ok(()) => (format!("sent to {to}"), 0),
                Err(e) => (format!("nibd mail: {e}"), 1),
            }
        }
        ["inbox"] => {
            let new = unread(dir, me, None);
            (if new.is_empty() { "no new messages".to_string() } else { show(&new) }, 0)
        }
        ["wait", opts @ ..] => {
            let (mut from, mut timeout) = (None, 100u64);
            let mut it = opts.iter();
            while let Some(o) = it.next() {
                match (*o, it.next()) {
                    ("--from", Some(n)) => from = Some(*n),
                    ("--timeout", Some(s)) if s.parse::<u64>().is_ok() => timeout = s.parse().unwrap_or(timeout),
                    _ => return (USAGE.to_string(), 2),
                }
            }
            let flag = dir.join("waiting").join(me);
            let _ = fs::write(&flag, "");
            let started = Instant::now();
            let found = loop {
                let new = unread(dir, me, from);
                if !new.is_empty() || started.elapsed() >= Duration::from_secs(timeout) {
                    break new;
                }
                std::thread::sleep(Duration::from_millis(200));
            };
            let _ = fs::remove_file(&flag);
            match found.is_empty() {
                true => (format!("no message after {timeout} s (wait again if you still need the answer)"), 1),
                false => (show(&found), 0),
            }
        }
        ["done", opts @ ..] => {
            let mut f = AiFinished::default();
            let mut it = opts.iter();
            while let Some(o) = it.next() {
                let Some(v) = it.next().map(|v| v.trim().to_string()) else { return (USAGE.to_string(), 2) };
                match *o {
                    "--asked" => f.asked = v,
                    "--found" => f.found = Some(v).filter(|v| !v.is_empty()),
                    "--did" => f.did = v,
                    _ => return (USAGE.to_string(), 2),
                }
            }
            if f.asked.is_empty() || f.did.is_empty() {
                return ("nib-mail done: --asked and --did are both needed".to_string(), 2);
            }
            let n = f.words();
            if n > AiFinished::WORDS {
                return (format!("nib-mail done: {n} words; say it in at most {} (all three parts together) and run it again", AiFinished::WORDS), 1);
            }
            let text = serde_json::to_string(&f).unwrap_or_default();
            match post(dir, &Letter { from: my_name, to: NIB.to_string(), text, at: now() }) {
                Ok(()) => ("the user has your summary: they accept it (and end this session) or tell you what is left. Stop here and wait.".to_string(), 0),
                Err(e) => (format!("nibd mail: {e}"), 1),
            }
        }
        _ => (USAGE.to_string(), 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_runs_this_exe() {
        let bin = temp("launcher");
        fs::create_dir_all(&bin).unwrap();
        write_launchers(&bin, Path::new("/opt/it's nib/nibd")).unwrap();
        let sh = fs::read_to_string(bin.join("nib-mail")).unwrap();
        assert_eq!(sh, "#!/bin/sh\nexec '/opt/it'\\''s nib/nibd' mail \"$@\"\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(bin.join("nib-mail")).unwrap().permissions().mode() & 0o111, 0o111);
        }
        assert_eq!(bin.join("nib-mail.cmd").exists(), cfg!(windows));
        let _ = fs::remove_dir_all(&bin);
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nibd-mail-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        dir(Some(&d)).unwrap()
    }
    #[test]
    fn a_sub_agent_report_goes_to_nib_and_is_not_a_finished_job() {
        let d = temp("report");
        let step = AiStep { id: "run:t1".into(), parent: Some("run".into()), kind: nib_proto::AiStepKind::Read { path: "a.rs".into(), offset: None, limit: None } };
        report(&d, "claude-0", &Report::Step(step.clone()));
        let letters = collect(&d);
        assert_eq!(letters.len(), 1);
        assert_eq!((letters[0].from.as_str(), letters[0].to.as_str()), ("claude-0", NIB));
        assert!(serde_json::from_str::<AiFinished>(&letters[0].text).is_err());
        assert_eq!(serde_json::from_str::<Report>(&letters[0].text).unwrap(), Report::Step(step));
    }

    fn args(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }
    fn entry(name: &str, task: &str) -> Entry {
        Entry { id: name.into(), name: name.into(), assistant: "Claude Code".into(), model: None, task: task.into(), busy: true, files: Vec::new() }
    }

    #[test]
    fn a_letter_goes_from_one_agent_to_another() {
        let d = temp("send");
        write_roster(&d, &[entry("claude-1", "colours"), entry("codex-2", "tests")]);
        assert_eq!(run(&d, "claude-1", &args("who")).0, "claude-1 (you)  Claude Code  working  colours\ncodex-2  Claude Code  working  tests");
        write_roster(&d, &[entry("claude-1", "colours"), Entry { files: vec!["theme.rs".into(), "app.rs".into()], ..entry("codex-2", "tests") }]);
        assert!(run(&d, "claude-1", &args("who")).0.ends_with("codex-2  Claude Code  working  tests\n    changed: theme.rs, app.rs"), "what each changed");
        assert_eq!(run(&d, "claude-1", &args("send nobody hi")).1, 1, "an unknown name is refused, with the names there are");
        assert_eq!(run(&d, "claude-1", &args("send codex-2 I am editing theme.rs")), ("sent to codex-2".to_string(), 0));
        // The daemon takes it out and delivers it.
        let out = collect(&d);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].from.as_str(), out[0].to.as_str(), out[0].text.as_str()), ("claude-1", "codex-2", "I am editing theme.rs"));
        assert!(collect(&d).is_empty(), "taken once");
        deliver(&d, "codex-2", &out[0]);
        assert_eq!(run(&d, "codex-2", &args("inbox")).0, "from claude-1: I am editing theme.rs");
        assert_eq!(run(&d, "codex-2", &args("inbox")).0, "no new messages", "seen once");
        // Said to the agent by the daemon: seen, so a later wait doesn't repeat it.
        deliver(&d, "codex-2", &Letter { text: "second".into(), ..out[0].clone() });
        unread(&d, "codex-2", None);
        assert_eq!(run(&d, "codex-2", &args("wait --timeout 0")).1, 1);
        let _ = fs::remove_dir_all(d.parent().unwrap());
    }

    #[test]
    fn an_agent_is_named_after_its_task_and_keeps_its_inbox() {
        assert_eq!(task_name("calc.py prints the wrong sum. Fix the bug with the Edit tool").as_deref(), Some("calc-prints-wrong"));
        assert_eq!(task_name("Please fix the flaky SSH askpass test").as_deref(), Some("fix-flaky-ssh"));
        assert_eq!(task_name("Colour-code skill groups in the / menu").as_deref(), Some("colour-code-skill"));
        assert_eq!(task_name("??? 42 the of"), None);
        // Renamed after its task: it is found by the new name, and its inbox (by id) is the same.
        let d = temp("names");
        let named = |id: &str, name: &str| Entry { name: name.into(), ..entry(id, "") };
        write_roster(&d, &[named("claude-1", "fix-calc-sum"), named("codex-2", "write-tests")]);
        assert_eq!(run(&d, "codex-2", &args("send fix-calc-sum which file?")), ("sent to fix-calc-sum".to_string(), 0));
        let out = collect(&d);
        assert_eq!((out[0].from.as_str(), out[0].to.as_str()), ("write-tests", "fix-calc-sum"));
        deliver(&d, "claude-1", &out[0]);
        assert_eq!(run(&d, "claude-1", &args("inbox")).0, "from write-tests: which file?");
        assert!(run(&d, "claude-1", &args("who")).0.starts_with("fix-calc-sum (you)"));
        let _ = fs::remove_dir_all(d.parent().unwrap());
    }

    #[test]
    fn only_a_letter_to_one_agent_asks_for_an_answer() {
        let l = |from: &str, to: &str| Letter { from: from.into(), to: to.into(), text: "x".into(), at: 0 };
        assert!(said(&[l("a", "b")]).contains("nib-mail send a"), "to one: it may be a question");
        assert!(said(&[l("a", "all")]).contains("no answer needed") && !said(&[l("a", "all")]).contains("nib-mail send"));
        assert_eq!(said(&[l(NIB, "b")]), "[nib mailbox] Message from nib:\nx");
        assert_eq!(said(&[]), "");
    }

    #[test]
    fn done_goes_to_nib_within_the_word_limit() {
        let d = temp("done");
        write_roster(&d, &[entry("claude-1", "colours")]);
        let run_done = |asked: &str, did: &str| run(&d, "claude-1", &["done".into(), "--asked".into(), asked.into(), "--did".into(), did.into()]);
        assert_eq!(run_done("", "x").1, 2, "both parts are needed");
        assert_eq!(run_done("a", &"word ".repeat(100)).1, 1, "over the limit is refused");
        assert!(collect(&d).is_empty());
        assert_eq!(run_done("Make it green", "Made it green").1, 0);
        let out = collect(&d);
        assert_eq!((out[0].from.as_str(), out[0].to.as_str()), ("claude-1", NIB));
        let f: AiFinished = serde_json::from_str(&out[0].text).unwrap();
        assert_eq!((f.asked.as_str(), f.found, f.did.as_str()), ("Make it green", None, "Made it green"));
        let _ = fs::remove_dir_all(d.parent().unwrap());
    }

    #[test]
    fn waiting_blocks_until_the_answer_comes() {
        let d = temp("wait");
        write_roster(&d, &[entry("claude-1", ""), entry("codex-2", "")]);
        let d2 = d.clone();
        let sender = std::thread::spawn(move || {
            // Until the other is seen waiting, then answer.
            let t = Instant::now();
            while !waiting(&d2, "claude-1") && t.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(20));
            }
            deliver(&d2, "claude-1", &Letter { from: NIB.into(), to: "claude-1".into(), text: "not the one".into(), at: 0 });
            deliver(&d2, "claude-1", &Letter { from: "codex-2".into(), to: "claude-1".into(), text: "go ahead".into(), at: 0 });
        });
        let (out, code) = run(&d, "claude-1", &args("wait --from codex-2 --timeout 5"));
        sender.join().unwrap();
        assert_eq!((out.as_str(), code), ("from codex-2: go ahead", 0));
        assert!(!waiting(&d, "claude-1"), "no longer waiting");
        let _ = fs::remove_dir_all(d.parent().unwrap());
    }
}
