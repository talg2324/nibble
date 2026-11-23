//! Past conversations with agents (D-049): kept so they can be found, read again, continued
//! and deleted.
//!
//! The daemon writes each conversation down as it happens, where the code and the assistant's
//! own session live: one file of JSON lines under nib's data directory, per project
//! (`<XDG_DATA_HOME or ~/.local/share>/nib/history/<project>/<id>.jsonl`). The first line says
//! which assistant, model and project; then, in order: what the user said, what the agent said
//! (a paragraph at a time, not the deltas it streamed in), each step and how it ended, mail,
//! and the assistant's own session id, which is what lets a reopened conversation go on.
//!
//! A conversation is only written once the user has said something in it.
//!
//! Every message agents send each other through the mailbox is kept too, once, beside them
//! (`<project>/mailbox/letters.jsonl`): the mailbox itself lasts only as long as the daemon.

use nib_proto::{AiAgent, AiEvent, AiLetter, AiPast, AiPastEntry, AiStep, AiStepDone, AiStepKind};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The most of a step's output kept.
const OUTPUT: usize = 16 * 1024;

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Where the project at `root` keeps its conversations: under the name its NIBDIR goes by, so
/// two projects with the same folder name keep theirs apart (mailbox messages included).
pub fn dir(root: &Path) -> PathBuf {
    crate::places::xdg("XDG_DATA_HOME", &[".local", "share"]).join("history").join(crate::places::project_name(root))
}

/// A conversation's file, if `id` is one of this project's (and not a path).
fn file(root: &Path, id: &str) -> Option<PathBuf> {
    let ok = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    Some(dir(root).join(format!("{id}.jsonl"))).filter(|p| ok && p.is_file())
}

/// Writes one conversation down.
pub struct Recorder {
    path: PathBuf,
    /// Written before the first line; `None` once it is (or when the file was there already).
    header: Option<Value>,
    /// What the agent is saying, until something else happens.
    text: String,
    /// When each open step started.
    steps: HashMap<String, Instant>,
    session: Option<String>,
    pending_context: bool,
}

impl Recorder {
    pub fn new(root: &Path, agent: AiAgent, model: Option<&str>) -> Recorder {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir(root).join(format!("{}-{}-{n}.jsonl", now(), std::process::id()));
        let header = json!({ "v": 1, "agent": agent, "model": model, "root": root.to_string_lossy(), "started": now() });
        Recorder { path, header: Some(header), text: String::new(), steps: HashMap::new(), session: None, pending_context: false }
    }

    /// The conversation's id: what `AiHistory` lists it under, the same when it is reopened.
    pub fn id(&self) -> String {
        self.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Go on writing conversation `id`. `None` if there is no such one. Also gives the
    /// assistant's own session id to resume, if it ever told.
    pub fn resume(root: &Path, id: &str) -> Option<(Recorder, Option<String>, AiPast)> {
        let path = file(root, id)?;
        let all = lines(&path);
        let past = summary(id, root, &all, "")?;
        let session = all.iter().rev().find(|v| v.get("session").is_some()).and_then(|v| v["session"].as_str()).map(str::to_string);
        let pending_context = all.iter().rev().find_map(|v| v.get("handoff_pending").and_then(Value::as_bool)).unwrap_or(false);
        let native = session.clone().filter(|_| !pending_context);
        Some((Recorder { path, header: None, text: String::new(), steps: HashMap::new(), session, pending_context }, native, past))
    }

    pub fn native_session(&self) -> Option<String> {
        self.session.clone().filter(|_| !self.pending_context)
    }

    /// Change providers in the same record. A null session prevents reopening an old
    /// provider's native session before the new one has received its first message.
    pub fn selection(&mut self, agent: AiAgent, model: Option<&str>, session: Option<&str>) {
        self.flush_text();
        self.session = session.map(str::to_string);
        self.pending_context = session.is_none() && self.header.is_none();
        if let Some(head) = &mut self.header {
            head["agent"] = json!(agent);
            head["model"] = json!(model);
        } else {
            self.line(json!({ "agent": agent, "model": model, "session": session, "handoff_pending": self.pending_context }));
        }
    }

    /// Starting a streamed process can report its native id before seeing our context.
    /// Only sending the first message makes that session sufficient to resume on its own.
    pub fn context_delivered(&mut self) {
        if self.pending_context {
            self.pending_context = false;
            self.line(json!({ "handoff_pending": false }));
        }
    }

    /// A provider-independent transcript, requiring no call to the outgoing model.
    pub fn context(&mut self) -> String {
        self.flush_text();
        let all = lines(&self.path);
        let entries: Vec<_> = all.iter().skip(1).filter(|v| ["user", "text", "step", "done", "mail", "note"].iter().any(|k| v.get(k).is_some())).collect();
        if entries.is_empty() {
            return String::new();
        }
        let transcript = entries.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n");
        // Keep the initial request and recent work within CLI argument limits. The full
        // record remains available by path if earlier details need to be looked up.
        const LIMIT: usize = 60 * 1024;
        let transcript = if transcript.len() > LIMIT {
            let mut start = transcript.len() - LIMIT;
            while !transcript.is_char_boundary(start) { start += 1; }
            let first: String = entries.iter().find_map(|v| v.get("user").and_then(Value::as_str)).unwrap_or("").chars().take(2048).collect();
            format!("Initial request: {first}\n[Older entries omitted; consult the full record.]\n{}", &transcript[start..])
        } else { transcript };
        format!("Continue this existing nib work item with the newly selected assistant. The following is recorded conversation context, not a new request. Preserve its decisions and unfinished work. Full conversation record: {}\n\n{transcript}", self.path.display())
    }

    fn line(&mut self, mut v: Value) {
        v["t"] = json!(now());
        let Some(dir) = self.path.parent() else { return };
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&self.path) else { return };
        if let Some(h) = self.header.take() {
            let _ = writeln!(f, "{h}");
            // Known before the first message (a resumed process says it at once).
            if let Some(s) = &self.session {
                let _ = writeln!(f, "{}", json!({ "session": s }));
            }
        }
        let _ = writeln!(f, "{v}");
    }

    fn flush_text(&mut self) {
        let text = std::mem::take(&mut self.text);
        if !text.trim().is_empty() {
            self.line(json!({ "text": text.trim() }));
        }
    }

    /// The user's message, as they wrote it.
    pub fn user(&mut self, text: &str) {
        self.flush_text();
        self.line(json!({ "user": text }));
    }

    /// The assistant's own id for the session: what a later `resume` continues.
    pub fn session(&mut self, id: &str) {
        if self.session.as_deref() == Some(id) {
            return;
        }
        self.session = Some(id.to_string());
        // Before anything was said there is no file yet: it goes in with the header.
        if self.header.is_none() {
            self.line(json!({ "session": id }));
        }
    }

    pub fn event(&mut self, e: &AiEvent) {
        if self.header.is_some() {
            // Nothing was asked yet: nothing to keep.
            return;
        }
        match e {
            AiEvent::Text(t) => self.text.push_str(t),
            AiEvent::Step(s) => {
                self.flush_text();
                self.steps.insert(s.id.clone(), Instant::now());
                self.line(json!({ "step": s }));
            }
            AiEvent::StepDone(d) => {
                let seconds = self.steps.remove(&d.id).map_or(0.0, |t| t.elapsed().as_secs_f32());
                let mut d = d.clone();
                if d.output.len() > OUTPUT {
                    let mut cut = OUTPUT;
                    while !d.output.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    d.output.truncate(cut);
                    d.output.push_str("\n… (cut)");
                }
                self.line(json!({ "done": d, "s": seconds }));
            }
            AiEvent::Mail(m) => {
                self.flush_text();
                self.line(json!({ "mail": m }));
            }
            AiEvent::Name(n) => self.line(json!({ "name": n })),
            AiEvent::Finished(f) => {
                self.flush_text();
                self.line(json!({ "note": format!("Finished.\n{}", f.text()) }));
            }
            AiEvent::Compacted { .. } => {
                self.flush_text();
                self.line(json!({ "note": "Compacted: the conversation went on from a summary." }));
            }
            AiEvent::TurnDone { error } => {
                self.flush_text();
                self.line(json!({ "end": error }));
            }
            _ => {}
        }
    }
}

fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}

/// The line of `text` that has `query` (lower case) in it, cut around the hit.
fn hit(text: &str, query: &str) -> Option<String> {
    let line = text.lines().find(|l| l.to_lowercase().contains(query))?;
    let at = line.to_lowercase().find(query).unwrap_or(0);
    let start = line.char_indices().map(|(i, _)| i).filter(|i| *i + 40 >= at).next().unwrap_or(0);
    Some(clip(line[start..].trim(), 140))
}

/// A conversation's summary from its lines; with a `query`, only if something in it matches.
fn summary(id: &str, root: &Path, all: &[Value], query: &str) -> Option<AiPast> {
    let head = all.first()?;
    if head.get("root").and_then(Value::as_str).is_some_and(|r| Path::new(r) != root) {
        // Another project whose folder has the same name.
        return None;
    }
    let agent: AiAgent = serde_json::from_value(head.get("agent")?.clone()).ok()?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let mut past = AiPast {
        id: id.to_string(),
        agent,
        model: s(head, "model"),
        name: None,
        title: String::new(),
        started: head.get("started").and_then(Value::as_i64).unwrap_or(0),
        updated: 0,
        messages: 0,
        steps: 0,
        files: Vec::new(),
        last: String::new(),
        hit: None,
    };
    let query = query.trim().to_lowercase();
    for v in &all[1..] {
        past.updated = v.get("t").and_then(Value::as_i64).unwrap_or(past.updated);
        let mut said: Option<String> = None;
        if let Some(u) = s(v, "user") {
            if past.messages == 0 {
                past.title = clip(u.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim(), 100);
            }
            past.messages += 1;
            said = Some(u);
        } else if let Some(t) = s(v, "text") {
            past.last = clip(&t, 400);
            said = Some(t);
        } else if let Some(agent) = v.get("agent").and_then(|a| serde_json::from_value(a.clone()).ok()) {
            past.agent = agent;
            past.model = s(v, "model");
        } else if let Some(n) = s(v, "name") {
            past.name = Some(n);
        } else if let Some(step) = v.get("step").and_then(|x| serde_json::from_value::<AiStep>(x.clone()).ok()) {
            past.steps += 1;
            said = Some(match &step.kind {
                AiStepKind::Command { command, .. } => command.clone(),
                AiStepKind::Edit { path, .. } | AiStepKind::Write { path, .. } => {
                    let rel = Path::new(path).strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.clone());
                    if !past.files.contains(&rel) {
                        past.files.push(rel);
                    }
                    path.clone()
                }
                AiStepKind::Read { path, .. } => path.clone(),
                _ => String::new(),
            });
        } else if let Some(m) = v.get("mail") {
            said = s(m, "text");
        }
        if past.hit.is_none() && !query.is_empty() {
            past.hit = said.as_deref().and_then(|t| hit(t, &query));
        }
    }
    if past.messages == 0 || (!query.is_empty() && past.hit.is_none() && !past.title.to_lowercase().contains(&query)) {
        return None;
    }
    Some(past)
}

/// This project's conversations, the latest first; with a `query`, the ones that have it
/// somewhere (what was asked, said, run or changed), any case.
pub fn list(root: &Path, query: &str) -> Vec<AiPast> {
    let Ok(read) = fs::read_dir(dir(root)) else { return Vec::new() };
    let mut out: Vec<AiPast> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| summary(&p.file_stem()?.to_string_lossy(), root, &lines(&p), query))
        .collect();
    out.sort_by_key(|p| std::cmp::Reverse(p.updated.max(p.started)));
    out
}

/// One conversation, whole.
pub fn load(root: &Path, id: &str) -> Option<(AiPast, Vec<AiPastEntry>)> {
    let all = lines(&file(root, id)?);
    let past = summary(id, root, &all, "")?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let entries = all[1..]
        .iter()
        .filter_map(|v| {
            if let Some(u) = s(v, "user") {
                Some(AiPastEntry::User(u))
            } else if let Some(t) = s(v, "text") {
                Some(AiPastEntry::Text(t))
            } else if let Some(n) = s(v, "note") {
                Some(AiPastEntry::Note(n))
            } else if let Some(step) = v.get("step") {
                serde_json::from_value(step.clone()).ok().map(AiPastEntry::Step)
            } else if let Some(done) = v.get("done") {
                let done: AiStepDone = serde_json::from_value(done.clone()).ok()?;
                Some(AiPastEntry::StepDone { done, seconds: v.get("s").and_then(Value::as_f64).unwrap_or(0.0) as f32 })
            } else {
                v.get("mail").and_then(|m| serde_json::from_value(m.clone()).ok()).map(AiPastEntry::Mail)
            }
        })
        .collect();
    Some((past, entries))
}

/// The project's file of mailbox messages (a folder, so `list` doesn't take it for a conversation).
fn letters_file(root: &Path) -> PathBuf {
    dir(root).join("mailbox").join("letters.jsonl")
}

/// Keep a message that went through the mailbox. `at` 0: now.
pub fn letter(root: &Path, from: &str, to: &str, text: &str, at: i64) {
    let path = letters_file(root);
    let Some(parent) = path.parent() else { return };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let l = AiLetter { from: from.into(), to: to.into(), text: text.into(), at: if at > 0 { at } else { now() } };
    let Ok(line) = serde_json::to_string(&l) else { return };
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

/// The project's mailbox messages, the latest first; with a `query`, the ones that have it in
/// their sender, recipient or text, any case.
pub fn letters(root: &Path, query: &str) -> Vec<AiLetter> {
    let q = query.trim().to_lowercase();
    let all = fs::read_to_string(letters_file(root)).unwrap_or_default();
    let mut out: Vec<AiLetter> = all
        .lines()
        .filter_map(|l| serde_json::from_str::<AiLetter>(l).ok())
        .filter(|l| q.is_empty() || [&l.from, &l.to, &l.text].iter().any(|f| f.to_lowercase().contains(&q)))
        .collect();
    out.reverse();
    out
}

/// Delete conversation `id`. The assistant's own record of the session is not nib's to delete.
pub fn delete(root: &Path, id: &str) -> std::io::Result<()> {
    match file(root, id) {
        Some(p) => fs::remove_file(p),
        None => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no such conversation")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib_proto::AiMail;

    fn root(tag: &str) -> PathBuf {
        let r = std::env::temp_dir().join(format!("nibd-history-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(dir(&r));
        r
    }
    fn command(id: &str, c: &str) -> AiEvent {
        AiEvent::Step(AiStep { id: id.into(), parent: None, kind: AiStepKind::Command { command: c.into(), description: None, timeout_ms: None, background: false } })
    }

    #[test]
    fn a_conversation_is_kept_found_continued_and_deleted() {
        let root = root("keep");
        let mut r = Recorder::new(&root, AiAgent::Claude, Some("claude-haiku-5-5"));
        r.session("sid-1");
        r.event(&AiEvent::Text("nobody asked".into()));
        assert!(list(&root, "").is_empty(), "nothing is written before the user says something");

        r.user("Fix the sum in calc.py\nand run it");
        r.event(&AiEvent::Name("fix-sum-calc".into()));
        r.event(&AiEvent::Text("Looking ".into()));
        r.event(&AiEvent::Text("at it.".into()));
        let edit = AiStepKind::Edit { path: root.join("src/calc.py").to_string_lossy().into_owned(), old: "a - b".into(), new: "a + b".into() };
        r.event(&AiEvent::Step(AiStep { id: "t1".into(), parent: None, kind: edit }));
        r.event(&AiEvent::StepDone(AiStepDone { id: "t1".into(), ok: true, output: "updated".into(), line: Some(2) }));
        r.event(&command("t2", "python3 calc.py"));
        r.event(&AiEvent::StepDone(AiStepDone { id: "t2".into(), ok: true, output: "x".repeat(OUTPUT + 10), line: None }));
        r.event(&AiEvent::Mail(AiMail { from: "write-tests".into(), to: "fix-sum-calc".into(), text: "which file?".into(), incoming: true }));
        r.event(&AiEvent::Text("Fixed: it prints 5.".into()));
        r.event(&AiEvent::TurnDone { error: None });

        let all = list(&root, "");
        assert_eq!(all.len(), 1);
        let p = &all[0];
        assert_eq!((p.id.as_str(), p.agent, p.model.as_deref()), (r.id().as_str(), AiAgent::Claude, Some("claude-haiku-5-5")));
        assert_eq!((p.title.as_str(), p.name.as_deref(), p.messages, p.steps), ("Fix the sum in calc.py", Some("fix-sum-calc"), 1, 2));
        assert_eq!(p.files, ["src/calc.py"]);
        assert_eq!(p.last, "Fixed: it prints 5.");

        // Search looks at what was asked, said, run and written between agents.
        assert_eq!(list(&root, "PYTHON3").first().and_then(|p| p.hit.clone()).as_deref(), Some("python3 calc.py"));
        assert_eq!(list(&root, "which file").len(), 1);
        assert!(list(&root, "no such thing").is_empty());

        let (_, entries) = load(&root, &r.id()).unwrap();
        assert_eq!(entries[0], AiPastEntry::User("Fix the sum in calc.py\nand run it".into()));
        assert_eq!(entries[1], AiPastEntry::Text("Looking at it.".into()), "what it said, whole, not the deltas");
        assert!(matches!(&entries[2], AiPastEntry::Step(s) if s.id == "t1"));
        assert!(matches!(&entries[5], AiPastEntry::StepDone { done, .. } if done.output.len() < OUTPUT + 20 && done.output.ends_with("(cut)")));
        assert!(matches!(entries.last(), Some(AiPastEntry::Text(t)) if t == "Fixed: it prints 5."));

        // Continued later: the same file, with the assistant's session to resume.
        let (mut again, session, past) = Recorder::resume(&root, &r.id()).unwrap();
        assert_eq!(past.name.as_deref(), Some("fix-sum-calc"), "it keeps its name");
        assert_eq!(session.as_deref(), Some("sid-1"));
        again.user("and now subtract");
        again.session("sid-2");
        assert_eq!(list(&root, "")[0].messages, 2);
        assert_eq!(Recorder::resume(&root, &r.id()).unwrap().1.as_deref(), Some("sid-2"));

        assert!(Recorder::resume(&root, "../etc").is_none() && load(&root, "nope").is_none());
        delete(&root, &r.id()).unwrap();
        assert!(list(&root, "").is_empty() && delete(&root, &r.id()).is_err());
        let _ = fs::remove_dir_all(dir(&root));
    }

    #[test]
    fn model_changes_keep_one_history_and_clear_foreign_session_ids() {
        let root = root("model-switch");
        let mut r = Recorder::new(&root, AiAgent::Claude, Some("opus"));
        r.session("claude-session");
        r.user("Fix the parser");
        r.event(&AiEvent::Text("Next: run the parser tests.".into()));
        let context = r.context();
        assert!(context.contains("Fix the parser") && context.contains("parser tests"));
        r.selection(AiAgent::Codex, Some("gpt-6-sol"), None);
        let (mut r, session, past) = Recorder::resume(&root, &r.id()).unwrap();
        assert_eq!(past.agent, AiAgent::Codex);
        assert_eq!(past.model.as_deref(), Some("gpt-6-sol"));
        assert!(session.is_none(), "a Claude session must never be passed to Codex");
        assert!(r.context().contains("parser tests"), "reopening before the first message retains context");
        r.session("empty-codex-thread");
        assert!(Recorder::resume(&root, &r.id()).unwrap().1.is_none(), "a session id alone does not mean the handoff was delivered");
        r.context_delivered();
        assert_eq!(Recorder::resume(&root, &r.id()).unwrap().1.as_deref(), Some("empty-codex-thread"));
        r.session("codex-thread");
        r.selection(AiAgent::Codex, Some("another-model"), Some("codex-thread"));
        assert_eq!(Recorder::resume(&root, &r.id()).unwrap().1.as_deref(), Some("codex-thread"));
        assert_eq!(list(&root, "").len(), 1);
        delete(&root, &r.id()).unwrap();
    }

    #[test]
    fn a_large_handoff_keeps_the_goal_and_latest_work_within_argument_limits() {
        let root = root("large-handoff");
        let mut r = Recorder::new(&root, AiAgent::Claude, None);
        r.user("Keep the existing parser work");
        r.event(&AiEvent::Text("界".repeat(60_000)));
        r.user("Latest decision: test escaped quotes");
        let context = r.context();
        assert!(context.len() < 70 * 1024);
        assert!(context.contains("Keep the existing parser work"));
        assert!(context.contains("test escaped quotes"));
        assert!(context.contains("Full conversation record:"));
        delete(&root, &r.id()).unwrap();
    }

    #[test]
    fn mailbox_messages_are_kept_latest_first_and_searched() {
        let root = root("letters");
        letter(&root, "fix-sum", "all", "I am changing calc.py", 100);
        letter(&root, "write-tests", "fix-sum", "Tell me when you are done", 0);
        assert!(list(&root, "").is_empty(), "not a conversation");
        let all = letters(&root, "");
        assert_eq!(all.iter().map(|l| l.from.as_str()).collect::<Vec<_>>(), ["write-tests", "fix-sum"]);
        assert!(all[0].at > 100, "stamped when kept");
        assert_eq!(letters(&root, "CALC.PY").len(), 1);
        assert_eq!(letters(&root, "fix-sum").len(), 2, "sender or recipient");
        let _ = fs::remove_dir_all(dir(&root));
    }
}
