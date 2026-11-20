//! Jobs for `nib ai`, for scripts and for an agent orchestrating sub-agents: one model call
//! (`generate`: straight to Ollama, schema-constrained and greedy if asked) or one agent run
//! (`run`: OpenCode with only the commands and files the job allows), described in JSON, and
//! batches of them that resume, retry and keep what failed. Everything happens on the daemon's
//! machine, next to the GPUs and the data: paths are relative to the project root.
//!
//! A job, as JSON (the CLI's flags and a batch's lines are the same keys):
//! - `kind`: `"generate"` or `"run"`; `model`; `gpu`: `"all"` (default), `"cpu"`, `"0,1"`
//! - `id` (batches: required, a file-name-safe string)
//! - `prompt` or `prompt_file`; `vars`: `{name}` / `{name:03d}` in the prompt filled in
//! - `cwd`: where it runs and what its other paths are relative to (default: the root)
//! - `timeout`: seconds; `0`: no limit (default: 1800 for generate, 900 for run: a sub-agent
//!   that hasn't finished by then is usually going in circles)
//! - generate: `system`, `schema` (a JSON schema) or `schema_file`, `temperature` (default 0),
//!   `seed` (default 0), `ctx`, `max_tokens`, `think`
//! - run: `allow` (command patterns it may run, `*` a wildcard), `write` (file patterns it may
//!   create or change), `keep_session`
//!
//! A run started from inside an agent nib runs (`$NIB_MAILBOX`, `$NIB_AGENT`) shows in that
//! agent's chat as it happens (D-061): a step for the run, its tool calls nested in it. In a batch
//! only the step for each job, so thousands of jobs don't bury the chat.

use crate::{ai, mail, ollama};
use nib_proto::{AiStep, AiStepDone, AiStepKind};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

const DEFAULT_TIMEOUT: u64 = 1800;
/// A sub-agent's: most finish in a few minutes; one still going after this is usually looping.
const RUN_TIMEOUT: u64 = 900;

const KEYS: &[&str] = &[
    "id", "kind", "model", "gpu", "prompt", "prompt_file", "vars", "cwd", "timeout", "system", "schema", "schema_file",
    "temperature", "seed", "ctx", "max_tokens", "think", "allow", "write", "keep_session",
];

/// Why a job failed, with what the model said when there is something.
#[derive(Debug)]
pub struct Failure {
    pub error: String,
    pub raw: Option<String>,
}

impl From<String> for Failure {
    fn from(error: String) -> Self {
        Failure { error, raw: None }
    }
}

impl From<&str> for Failure {
    fn from(error: &str) -> Self {
        Failure { error: error.into(), raw: None }
    }
}

#[derive(Debug)]
struct Job {
    v: Map<String, Value>,
    root: PathBuf,
}

impl Job {
    fn parse(text: &str, root: &Path) -> Result<Job, Failure> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("not a JSON job: {e}"))?;
        let Value::Object(v) = v else { return Err("a job is a JSON object".into()) };
        if let Some(k) = v.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(format!("unknown job key \"{k}\" (keys: {})", KEYS.join(", ")).into());
        }
        let job = Job { v, root: root.to_path_buf() };
        match job.kind()? {
            "generate" | "run" => {}
            k => return Err(format!("kind is \"generate\" or \"run\", not \"{k}\"").into()),
        }
        if job.str("model").is_none() {
            return Err("a job needs a model".into());
        }
        Ok(job)
    }

    fn str(&self, k: &str) -> Option<&str> {
        self.v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
    }

    fn num(&self, k: &str) -> Option<f64> {
        self.v.get(k).and_then(Value::as_f64)
    }

    fn list(&self, k: &str) -> Vec<String> {
        match self.v.get(k) {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
            _ => Vec::new(),
        }
    }

    fn kind(&self) -> Result<&str, Failure> {
        self.str("kind").ok_or_else(|| "a job needs kind: \"generate\" or \"run\"".into())
    }

    fn id(&self) -> Option<&str> {
        self.str("id")
    }

    fn model(&self) -> &str {
        self.str("model").unwrap_or_default()
    }

    /// Where it runs: `cwd` under the root.
    fn cwd(&self) -> PathBuf {
        match self.str("cwd") {
            Some(c) => resolve(&self.root, c),
            None => self.root.clone(),
        }
    }

    fn path(&self, p: &str) -> PathBuf {
        resolve(&self.cwd(), p)
    }

    /// `None`: no limit (`timeout: 0`).
    fn timeout(&self) -> Option<Duration> {
        let default = if self.str("kind") == Some("run") { RUN_TIMEOUT } else { DEFAULT_TIMEOUT };
        match self.num("timeout") {
            Some(t) if t <= 0.0 => None,
            Some(t) => Some(Duration::from_secs(t.max(1.0) as u64)),
            None => Some(Duration::from_secs(default)),
        }
    }

    fn gpu(&self) -> Result<ollama::GpuSel, Failure> {
        Ok(ollama::GpuSel::parse(self.str("gpu"))?)
    }

    /// The prompt, with `vars` filled in.
    fn prompt(&self) -> Result<String, Failure> {
        let text = match (self.str("prompt"), self.str("prompt_file")) {
            (Some(p), None) => p.to_string(),
            (None, Some(f)) => std::fs::read_to_string(self.path(f)).map_err(|e| format!("prompt_file {f}: {e}"))?,
            (Some(_), Some(_)) => return Err("give prompt or prompt_file, not both".into()),
            (None, None) => return Err("a job needs a prompt or a prompt_file".into()),
        };
        let mut vars = BTreeMap::new();
        if let Some(Value::Object(m)) = self.v.get("vars") {
            for (k, v) in m {
                let s = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                vars.insert(k.clone(), s);
            }
        }
        Ok(fill(&text, &vars))
    }

    fn schema(&self) -> Result<Option<Value>, Failure> {
        match (self.v.get("schema"), self.str("schema_file")) {
            (Some(s), None) => Ok(Some(s.clone())),
            (None, Some(f)) => {
                let text = std::fs::read_to_string(self.path(f)).map_err(|e| format!("schema_file {f}: {e}"))?;
                Ok(Some(serde_json::from_str(&text).map_err(|e| format!("schema_file {f} isn't JSON: {e}"))?))
            }
            (Some(_), Some(_)) => Err("give schema or schema_file, not both".into()),
            (None, None) => Ok(None),
        }
    }
}

fn resolve(base: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { base.join(p) }
}

/// `{name}` and `{name:03d}` (zero-padded, as in Python) for each of `vars`; other braces (JSON
/// in a prompt) stay as they are.
pub fn fill(text: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let replaced = after.find('}').and_then(|close| {
            let inner = &after[..close];
            let (key, spec) = inner.split_once(':').map_or((inner, None), |(k, s)| (k, Some(s)));
            let value = vars.get(key)?;
            let value = match spec {
                None => value.clone(),
                Some(spec) => {
                    let width: usize = spec.trim_end_matches('d').trim_start_matches('0').parse().ok()?;
                    if spec.starts_with('0') { format!("{value:0>width$}") } else { format!("{value:>width$}") }
                }
            };
            Some((value, close))
        });
        match replaced {
            Some((value, close)) => {
                out.push_str(&value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// How much of a run its owner's chat is shown (D-061).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Show {
    /// The run and every tool call in it.
    Steps,
    /// Only the run: for a batch's jobs.
    Run,
}

/// Run one job (JSON) at `root` -> its result (JSON). A failure is a result too, with `ok: false`.
pub fn run_job(text: &str, root: &Path) -> Value {
    run_job_showing(text, root, Show::Steps)
}

fn run_job_showing(text: &str, root: &Path, show: Show) -> Value {
    let start = Instant::now();
    let outcome = Job::parse(text, root).and_then(|job| {
        let mut r = match job.kind()? {
            "generate" => generate(&job)?,
            _ => agent(&job, show)?,
        };
        if let Some(id) = job.id() {
            r.insert("id".into(), json!(id));
        }
        Ok(r)
    });
    let mut r = match outcome {
        Ok(r) => r,
        Err(f) => failed(&f),
    };
    r.insert("seconds".into(), json!(round(start.elapsed().as_secs_f64())));
    Value::Object(r)
}

fn failed(f: &Failure) -> Map<String, Value> {
    let mut r = Map::new();
    r.insert("ok".into(), json!(false));
    r.insert("error".into(), json!(f.error));
    if let Some(raw) = &f.raw {
        r.insert("raw".into(), json!(raw));
    }
    r
}

fn round(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn secs(ns: Option<&Value>) -> Value {
    json!(round(ns.and_then(Value::as_f64).unwrap_or(0.0) / 1e9))
}

/// The Ollama name for `model`: as given, or without an OpenCode provider in front
/// (`ollama/qwen3:8b`) when that's the one Ollama has.
fn ollama_model(model: &str) -> String {
    let pulled = ollama::pulled();
    if pulled.contains(&ollama::name(model)) {
        return model.to_string();
    }
    match model.split_once('/') {
        Some((_, rest)) if pulled.contains(&ollama::name(rest)) => rest.to_string(),
        _ => model.to_string(),
    }
}

/// One model call, straight to Ollama: greedy and seeded unless the job says otherwise.
fn generate(job: &Job) -> Result<Map<String, Value>, Failure> {
    let gpu = job.gpu()?;
    let addr = ollama::server(&gpu)?;
    let model = ollama_model(job.model());
    let schema = job.schema()?;
    let mut messages = Vec::new();
    if let Some(system) = job.str("system") {
        messages.push(json!({ "role": "system", "content": system }));
    }
    messages.push(json!({ "role": "user", "content": job.prompt()? }));
    let mut options = Map::new();
    options.insert("temperature".into(), json!(job.num("temperature").unwrap_or(0.0)));
    options.insert("seed".into(), json!(job.num("seed").map_or(0, |s| s as i64)));
    if let Some(ctx) = job.num("ctx") {
        options.insert("num_ctx".into(), json!(ctx as u64));
    }
    if let Some(n) = job.num("max_tokens") {
        options.insert("num_predict".into(), json!(n as u64));
    }
    let mut body = json!({ "model": model, "messages": messages, "stream": false, "options": options });
    if let Some(think) = job.v.get("think").and_then(Value::as_bool) {
        body["think"] = json!(think);
    }
    if let Some(s) = &schema {
        body["format"] = s.clone();
    }
    let started = Instant::now();
    // No limit: a week, as far as a socket's timeout goes.
    let answer = ollama::chat(addr, &body, job.timeout().unwrap_or(Duration::from_secs(7 * 24 * 3600)))?;
    let text = answer.pointer("/message/content").and_then(Value::as_str).unwrap_or_default().to_string();
    let mut r = Map::new();
    r.insert("ok".into(), json!(true));
    r.insert("kind".into(), json!("generate"));
    if schema.is_some() {
        // Constrained output that still isn't JSON (cut off at max_tokens): a failure to retry.
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| Failure { error: format!("the answer isn't the JSON the schema asks for: {e}"), raw: Some(text.clone()) })?;
        r.insert("json".into(), parsed);
    }
    r.insert("text".into(), json!(text));
    if let Some(t) = answer.pointer("/message/thinking").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        r.insert("thinking".into(), json!(t));
    }
    r.insert("model".into(), json!(model));
    r.insert("digest".into(), json!(ollama::digest(addr, &model)));
    r.insert("ollama".into(), json!(ollama::version(addr)));
    r.insert("gpu".into(), json!(gpu.label()));
    r.insert("options".into(), Value::Object(options));
    r.insert("think".into(), body.get("think").cloned().unwrap_or(Value::Null));
    r.insert("constrained".into(), json!(schema.is_some()));
    r.insert("prompt_tokens".into(), answer.get("prompt_eval_count").cloned().unwrap_or(Value::Null));
    r.insert("new_tokens".into(), answer.get("eval_count").cloned().unwrap_or(Value::Null));
    r.insert("done_reason".into(), answer.get("done_reason").cloned().unwrap_or(Value::Null));
    r.insert("prompt_seconds".into(), secs(answer.get("prompt_eval_duration")));
    r.insert("eval_seconds".into(), secs(answer.get("eval_duration")));
    r.insert("load_seconds".into(), secs(answer.get("load_duration")));
    r.insert("call_seconds".into(), json!(round(started.elapsed().as_secs_f64())));
    Ok(r)
}

/// The git repository `dir` is in, which OpenCode calls its worktree; `None` outside one.
fn repo_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// OpenCode matches a file against `edit` patterns by its path relative to its worktree (the
/// git repository's root, `/` outside one), not to the folder it runs in. So each pattern,
/// relative to `cwd` (or absolute), becomes that; both forms, so either worktree matches.
fn edit_patterns(cwd: &Path, write: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for p in write {
        let abs = resolve(cwd, p);
        let abs = abs.to_string_lossy().replace('\\', "/");
        let mut bases: Vec<PathBuf> = repo_root(cwd).into_iter().collect();
        bases.push(PathBuf::from("/"));
        for base in bases {
            let base = base.to_string_lossy().replace('\\', "/");
            let base = base.trim_end_matches('/');
            if let Some(rel) = abs.strip_prefix(&format!("{base}/")) {
                out.push(rel.to_string());
            }
        }
    }
    out.dedup();
    out
}

/// What an agent run may do: read inside its folder; run only the `allow`ed commands; create or
/// change only the `write` patterns; nothing on the web, no sub-agents, no questions.
fn permissions(allow: &[String], write: &[String]) -> Value {
    let rules = |patterns: &[String]| {
        let mut m = Map::new();
        m.insert("*".into(), json!("deny"));
        for p in patterns {
            m.insert(p.clone(), json!("allow"));
        }
        Value::Object(m)
    };
    json!({
        "read": "allow", "glob": "allow", "grep": "allow", "list": "allow", "lsp": "allow", "todowrite": "allow",
        "edit": rules(write),
        "bash": rules(allow),
        "webfetch": "deny", "websearch": "deny", "task": "deny", "skill": "deny",
        "external_directory": "deny", "question": "deny", "doom_loop": "deny",
    })
}

/// A run as seen from its owner's chat (D-061): the agent nib runs that started it, through the
/// mailbox. `None` when it wasn't started from inside one.
struct Owner {
    dir: PathBuf,
    me: String,
    /// The run's step in the owner's chat; its tool calls are nested in it.
    step: String,
    show: Show,
}

impl Owner {
    fn start(job: &Job, prompt: &str, show: Show) -> Option<Owner> {
        static N: AtomicU64 = AtomicU64::new(0);
        let (dir, me) = mail::owner()?;
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let step = format!("nib-run-{}-{nanos}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
        let what = job.id().map(str::to_string).unwrap_or_else(|| {
            let first = prompt.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
            let mut w: String = first.chars().take(80).collect();
            if w.len() < first.len() {
                w.push('…');
            }
            w
        });
        let description = format!("nib ai run · {} · {what}", job.model());
        let owner = Owner { dir, me, step, show };
        owner.send(mail::Report::Step(AiStep { id: owner.step.clone(), parent: None, kind: AiStepKind::Agent { description } }));
        Some(owner)
    }

    fn send(&self, r: mail::Report) {
        mail::report(&self.dir, &self.me, &r);
    }

    /// One line of OpenCode's output: a finished tool call becomes a step inside the run's.
    fn line(&self, line: &str) {
        static UNNAMED: AtomicU64 = AtomicU64::new(0);
        if self.show != Show::Steps {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        if v.get("type").and_then(Value::as_str) != Some("tool_use") {
            return;
        }
        let part = v.get("part").cloned().unwrap_or_default();
        let Some((mut step, mut done)) = ai::opencode_step(&part, || format!("t{}", UNNAMED.fetch_add(1, Ordering::Relaxed))) else { return };
        step.id = format!("{}:{}", self.step, step.id);
        step.parent = Some(self.step.clone());
        done.id = step.id.clone();
        self.send(mail::Report::Step(step));
        self.send(mail::Report::StepDone(done));
    }

    fn end(&self, ok: bool, output: String) {
        self.send(mail::Report::StepDone(AiStepDone { id: self.step.clone(), ok, output, line: None }));
    }
}

/// One OpenCode run: the reply, the files it wrote, every tool it called. Shown in its owner's
/// chat as it goes, when it has one.
fn agent(job: &Job, show: Show) -> Result<Map<String, Value>, Failure> {
    let prompt = job.prompt()?;
    let owner = Owner::start(job, &prompt, show);
    let r = agent_run(job, &prompt, owner.as_ref());
    if let Some(o) = &owner {
        match &r {
            Ok(r) => {
                let files = r.get("files").and_then(Value::as_array).map(|f| f.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                let reply = r.get("reply").and_then(Value::as_str).unwrap_or_default();
                let output = if files.is_empty() { reply.to_string() } else { format!("{reply}\nchanged: {files}") };
                o.end(true, output);
            }
            Err(f) => o.end(false, f.error.clone()),
        }
    }
    r
}

fn agent_run(job: &Job, prompt: &str, owner: Option<&Owner>) -> Result<Map<String, Value>, Failure> {
    let exe = ai::opencode_exe().ok_or("OpenCode isn't installed on this machine")?;
    let gpu = job.gpu()?;
    if job.model().contains('/') {
        // Its provider's server, started for this GPU selection if it has to be.
        ollama::server(&gpu)?;
    }
    let cwd = job.cwd();
    if !cwd.is_dir() {
        return Err(format!("cwd {} isn't a folder", cwd.display()).into());
    }
    let write = job.list("write");
    let config = ai::opencode_config(permissions(&job.list("allow"), &edit_patterns(&cwd, &write)), job.model(), &gpu);
    let mut cmd = Command::new(&exe);
    // OpenCode takes its project folder from `--dir` / `$PWD`, not the process's working
    // folder: without both it inherits nibd's root and refuses `cwd` as an outside folder.
    cmd.env("OPENCODE_CONFIG_CONTENT", &config)
        .env("PWD", &cwd)
        .args(["run", "--format", "json", "--model", job.model(), "--dir"])
        .arg(&cwd)
        .args(["--", prompt])
        .current_dir(&cwd);
    let local = job.model().split_once('/').is_some_and(|(_, m)| ollama::pulled().contains(&ollama::name(m)));
    if local {
        // A local model needs no internet: OpenCode mustn't wait on it either.
        ai::opencode_offline(&mut cmd);
    }
    let before = SystemTime::now();
    let (out, err, code) = run_streaming(cmd, job.timeout(), &mut |l| {
        if let Some(o) = owner {
            o.line(l);
        }
    })?;

    let (mut reply, mut tools, mut error, mut session) = (String::new(), Vec::new(), None, None);
    let mut tokens = Map::new();
    for line in out.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if session.is_none() {
            session = v.get("sessionID").and_then(Value::as_str).map(str::to_string);
        }
        let part = v.get("part").cloned().unwrap_or_default();
        match v.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    // Its last words: what it said before calling tools was narration.
                    reply = t.to_string();
                }
            }
            Some("tool_use") => {
                let state = part.get("state").cloned().unwrap_or_default();
                let mut input = state.get("input").map(Value::to_string).unwrap_or_default();
                input.truncate(300);
                tools.push(json!({
                    "tool": part.get("tool"),
                    "status": state.get("status"),
                    "input": input,
                    "error": state.get("error"),
                }));
            }
            Some("step_finish") => {
                if let Some(Value::Object(t)) = part.get("tokens") {
                    for k in ["input", "output", "reasoning"] {
                        let add = t.get(k).and_then(Value::as_u64).unwrap_or(0);
                        let cur = tokens.get(k).and_then(Value::as_u64).unwrap_or(0);
                        tokens.insert(k.into(), json!(cur + add));
                    }
                }
            }
            Some("error") => {
                error = v.pointer("/error/data/message").or_else(|| v.pointer("/error/name")).and_then(Value::as_str).map(str::to_string);
            }
            _ => {}
        }
    }
    let files = written(&cwd, &write, before);
    if let Some(id) = session.as_deref().filter(|_| !job.v.get("keep_session").and_then(Value::as_bool).unwrap_or(false)) {
        // A sub-agent's run isn't a chat of the user's: out of OpenCode's session list.
        let mut del = Command::new(&exe);
        del.args(["session", "delete", id]).env("PWD", &cwd).current_dir(&cwd);
        let _ = run_capture(del, Duration::from_secs(30));
        session = None;
    }
    let denied = tools.iter().filter(|t| t["status"] == "error").count();
    if let Some(e) = error.or_else(|| (code != Some(0)).then(|| format!("opencode exited ({code:?}): {}", tail(&err)))) {
        return Err(Failure { error: e, raw: Some(reply) });
    }
    let mut r = Map::new();
    r.insert("ok".into(), json!(true));
    r.insert("kind".into(), json!("run"));
    r.insert("reply".into(), json!(reply));
    r.insert("files".into(), json!(files));
    r.insert("tools".into(), Value::Array(tools));
    r.insert("tool_errors".into(), json!(denied));
    r.insert("tokens".into(), Value::Object(tokens));
    r.insert("model".into(), json!(job.model()));
    if local {
        // Provenance, as for generate: the weights and the server that ran them.
        let name = job.model().split_once('/').map_or(job.model(), |(_, m)| m);
        if let Some(addr) = ollama::addr_of(&gpu) {
            r.insert("digest".into(), json!(ollama::digest(addr, name)));
            r.insert("ollama".into(), json!(ollama::version(addr)));
        }
    }
    r.insert("gpu".into(), json!(gpu.label()));
    if let Some(s) = session {
        r.insert("session".into(), json!(s));
    }
    Ok(r)
}

fn tail(s: &str) -> String {
    let s = s.trim();
    let cut = s.len().saturating_sub(600);
    let cut = (cut..=s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(s.len());
    s[cut..].to_string()
}

/// Run `cmd` to the end or until `limit` -> (stdout, stderr, exit code). Stopped at the limit.
fn run_capture(cmd: Command, limit: Duration) -> Result<(String, String, Option<i32>), Failure> {
    run_streaming(cmd, Some(limit), &mut |_| {})
}

/// `run_capture`, also handing each line of stdout to `on_line` as it comes; `limit` `None`:
/// no limit. At the limit it is stopped, and what it printed is the failure's `raw`.
fn run_streaming(mut cmd: Command, limit: Option<Duration>, on_line: &mut dyn FnMut(&str)) -> Result<(String, String, Option<i32>), Failure> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("starting {cmd:?}: {e}"))?;
    let (so, mut se) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    let (tx, rx) = mpsc::channel::<String>();
    let reader = thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(so).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let err = thread::spawn(move || {
        let mut s = String::new();
        let _ = se.read_to_string(&mut s);
        s
    });
    let mut out = String::new();
    let mut take = |line: String, out: &mut String| {
        on_line(&line);
        out.push_str(&line);
        out.push('\n');
    };
    let start = Instant::now();
    let code = loop {
        while let Ok(line) = rx.try_recv() {
            take(line, &mut out);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if limit.is_none_or(|l| start.elapsed() < l) => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                while let Ok(line) = rx.try_recv() {
                    take(line, &mut out);
                }
                let secs = limit.map_or(0, |l| l.as_secs());
                return Err(Failure { error: format!("timed out after {secs}s (the job's timeout; 0 for none)"), raw: Some(out) });
            }
        }
    };
    let _ = reader.join();
    for line in rx.try_iter() {
        take(line, &mut out);
    }
    Ok((out, err.join().unwrap_or_default(), code))
}

/// `*` within a path segment, `**` across them.
fn glob_match(pattern: &str, path: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => {
                let rest = p[2..].strip_prefix(b"/").unwrap_or(&p[2..]);
                (0..=s.len()).any(|i| go(rest, &s[i..]))
            }
            Some(b'*') => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| go(&p[1..], &s[i..])),
            Some(&c) => s.first() == Some(&c) && go(&p[1..], &s[1..]),
        }
    }
    go(pattern.as_bytes(), path.as_bytes())
}

/// The files under `cwd` matching `patterns` that changed since `since`, relative to `cwd`.
fn written(cwd: &Path, patterns: &[String], since: SystemTime) -> Vec<String> {
    let mut found = Vec::new();
    for pat in patterns {
        // Walk from the part before the first wildcard.
        let fixed: Vec<&str> = pat.split('/').take_while(|seg| !seg.contains('*')).collect();
        let (base, depth) = if fixed.len() == pat.split('/').count() {
            (cwd.join(pat), 0)
        } else {
            (cwd.join(fixed.join("/")), if pat.contains("**") { 12 } else { pat.split('/').count() - fixed.len() })
        };
        let mut stack = vec![(base, 0usize)];
        while let Some((p, d)) = stack.pop() {
            if p.is_dir() && d < depth.max(1) {
                for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                    stack.push((e.path(), d + 1));
                }
            } else if p.is_file() {
                let rel = p.strip_prefix(cwd).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                let fresh = p.metadata().and_then(|m| m.modified()).is_ok_and(|m| m >= since);
                if fresh && glob_match(pat, &rel) {
                    found.push(rel);
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 200 && id.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) && !id.starts_with('.')
}

fn done_ok(path: &Path) -> bool {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).is_some_and(|v| v["ok"] == true)
}

fn write_json(path: &Path, v: &Value) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(v).unwrap_or_default() + "\n")?;
    std::fs::rename(tmp, path)
}

/// Every job in the JSONL file `jobs` (relative to `root`), results under `out`: `<id>.json` per
/// job that worked, `<id>_error.json` (with the raw answer) per job that failed every try,
/// `progress.jsonl` (one line per job, appended) and `manifest.json`. Jobs whose `<id>.json`
/// is there already are skipped, so running it again resumes. `progress` hears each progress
/// line as it's written. Returns the summary.
pub fn run_batch(jobs: &str, out: &str, concurrency: u32, retries: u32, root: &Path, progress: impl Fn(&Value) + Send + Sync + 'static) -> Result<Value, String> {
    let jobs_path = resolve(root, jobs);
    let out_dir = resolve(root, out);
    let text = std::fs::read_to_string(&jobs_path).map_err(|e| format!("{}: {e}", jobs_path.display()))?;
    let mut list = Vec::new();
    let mut ids = HashSet::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let job = Job::parse(line, root).map_err(|f| format!("{}:{}: {}", jobs_path.display(), n + 1, f.error))?;
        let id = job.id().ok_or_else(|| format!("{}:{}: a batch job needs an id", jobs_path.display(), n + 1))?.to_string();
        if !safe_id(&id) {
            return Err(format!("{}:{}: id \"{id}\" must be letters, digits, '.', '_' or '-'", jobs_path.display(), n + 1));
        }
        if !ids.insert(id.clone()) {
            return Err(format!("{}:{}: id \"{id}\" appears twice", jobs_path.display(), n + 1));
        }
        list.push((id, line.to_string()));
    }
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    let meta = std::fs::metadata(&jobs_path).ok();
    let mut manifest = json!({
        "jobs_file": jobs_path.display().to_string(),
        "jobs_bytes": meta.as_ref().map(|m| m.len()),
        "jobs_modified": meta.and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()),
        "jobs": list.len(),
        "concurrency": concurrency,
        "retries": retries,
        "nibd": env!("CARGO_PKG_VERSION"),
        "started": now(),
    });
    let _ = write_json(&out_dir.join("manifest.json"), &manifest);

    let (todo, resumed): (Vec<_>, Vec<_>) = list.into_iter().partition(|(id, _)| !done_ok(&out_dir.join(format!("{id}.json"))));
    let queue = Arc::new(Mutex::new(todo.into_iter().collect::<VecDeque<_>>()));
    let log = Arc::new(Mutex::new(
        OpenOptions::new().create(true).append(true).open(out_dir.join("progress.jsonl")).map_err(|e| e.to_string())?,
    ));
    let tally = Arc::new(Mutex::new((0usize, 0usize, BTreeMap::<String, Value>::new())));
    let progress = Arc::new(progress);
    let workers: Vec<_> = (0..concurrency.max(1))
        .map(|_| {
            let (queue, log, tally, progress, out_dir, root) =
                (queue.clone(), log.clone(), tally.clone(), progress.clone(), out_dir.clone(), root.to_path_buf());
            thread::spawn(move || loop {
                let Some((id, line)) = queue.lock().unwrap().pop_front() else { return };
                let (mut result, mut attempts) = (Value::Null, 0);
                for _ in 0..=retries {
                    attempts += 1;
                    result = run_job_showing(&line, &root, Show::Run);
                    if result["ok"] == true {
                        break;
                    }
                }
                result["attempts"] = json!(attempts);
                let ok = result["ok"] == true;
                let (done, error) = (out_dir.join(format!("{id}.json")), out_dir.join(format!("{id}_error.json")));
                if ok {
                    let _ = write_json(&done, &result);
                    let _ = std::fs::remove_file(&error);
                } else {
                    let _ = write_json(&error, &result);
                }
                let mut line = json!({ "id": id, "ok": ok, "attempts": attempts, "seconds": result["seconds"], "time": now() });
                if !ok {
                    line["error"] = result["error"].clone();
                }
                {
                    let mut t = tally.lock().unwrap();
                    if ok { t.0 += 1 } else { t.1 += 1 }
                    if let Some(model) = result["model"].as_str() {
                        let key = format!("{model} @ {}", result["gpu"].as_str().unwrap_or("all"));
                        t.2.entry(key).or_insert_with(|| json!({ "model": model, "gpu": result["gpu"], "digest": result["digest"], "ollama": result["ollama"] }));
                    }
                }
                let _ = writeln!(log.lock().unwrap(), "{line}");
                progress(&line);
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    let (ok, failed, models) = std::mem::take(&mut *tally.lock().unwrap());
    manifest["finished"] = json!(now());
    manifest["done"] = json!(ok);
    manifest["failed"] = json!(failed);
    manifest["resumed"] = json!(resumed.len());
    manifest["models"] = json!(models.into_values().collect::<Vec<_>>());
    let _ = write_json(&out_dir.join("manifest.json"), &manifest);
    Ok(json!({
        "out": out_dir.display().to_string(),
        "jobs": manifest["jobs"], "done": ok, "failed": failed, "resumed": resumed.len(),
        "manifest": out_dir.join("manifest.json").display().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_has_a_shorter_default_timeout() {
        let root = Path::new("/tmp");
        assert_eq!(Job::parse(r#"{"kind":"run","model":"m"}"#, root).unwrap().timeout(), Some(Duration::from_secs(900)));
        assert_eq!(Job::parse(r#"{"kind":"generate","model":"m"}"#, root).unwrap().timeout(), Some(Duration::from_secs(1800)));
        assert_eq!(Job::parse(r#"{"kind":"run","model":"m","timeout":0}"#, root).unwrap().timeout(), None);
        assert_eq!(Job::parse(r#"{"kind":"run","model":"m","timeout":5}"#, root).unwrap().timeout(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn vars_fill_named_and_padded_slots_and_leave_json_alone() {
        let vars = BTreeMap::from([("i".to_string(), "7".to_string()), ("RUN".to_string(), "swarm/a".to_string())]);
        assert_eq!(fill("show_row.py {i} -> {RUN}/row_{i:03d}.json {\"row\": 1}", &vars), "show_row.py 7 -> swarm/a/row_007.json {\"row\": 1}");
        assert_eq!(fill("{missing} {i", &vars), "{missing} {i");
    }

    #[test]
    fn globs_match_within_and_across_folders() {
        assert!(glob_match("swarm/a/row_*.json", "swarm/a/row_007.json"));
        assert!(!glob_match("swarm/a/row_*.json", "swarm/a/sub/row_007.json"));
        assert!(glob_match("out/**/*.json", "out/x/y/z.json"));
        assert!(glob_match("out/**/*.json", "out/z.json"));
        assert!(!glob_match("*.json", "notes.txt"));
    }

    #[test]
    fn a_job_names_its_kind_model_and_known_keys_only() {
        let root = Path::new("/tmp");
        assert!(Job::parse(r#"{"kind":"generate","model":"m","prompt":"hi"}"#, root).is_ok());
        assert!(Job::parse(r#"{"kind":"generate","prompt":"hi"}"#, root).unwrap_err().error.contains("model"));
        assert!(Job::parse(r#"{"kind":"gen","model":"m"}"#, root).unwrap_err().error.contains("kind"));
        assert!(Job::parse(r#"{"kind":"run","model":"m","alow":["x"]}"#, root).unwrap_err().error.contains("alow"));
    }

    #[test]
    fn write_patterns_are_relative_to_the_repository_like_opencode_matches_them() {
        let dir = std::env::temp_dir().join(format!("nibd-repo-{}", std::process::id()));
        let cwd = dir.join("gradient/echo-reports/manual");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let got = edit_patterns(&cwd, &["swarm/a/row_*.json".into()]);
        assert_eq!(got[0], "gradient/echo-reports/manual/swarm/a/row_*.json");
        assert!(got[1].ends_with("gradient/echo-reports/manual/swarm/a/row_*.json") && !got[1].starts_with('/'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_agent_may_do_only_what_its_job_allows() {
        let p = permissions(&["uv run python show_row.py *".into()], &["swarm/a/row_*.json".into()]);
        assert_eq!(p["bash"]["*"], "deny");
        assert_eq!(p["bash"]["uv run python show_row.py *"], "allow");
        assert_eq!(p["edit"]["swarm/a/row_*.json"], "allow");
        assert_eq!(p["webfetch"], "deny");
        assert_eq!(p["external_directory"], "deny");
    }

    #[test]
    fn a_batch_resumes_and_keeps_failures() {
        let dir = std::env::temp_dir().join(format!("nibd-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("out")).unwrap();
        // One job done in an earlier run; one that fails (no prompt) every time.
        std::fs::write(dir.join("out/a.json"), r#"{"ok": true}"#).unwrap();
        std::fs::write(
            dir.join("jobs.jsonl"),
            "{\"id\":\"a\",\"kind\":\"generate\",\"model\":\"m\",\"prompt\":\"x\"}\n// a comment\n{\"id\":\"b\",\"kind\":\"generate\",\"model\":\"m\"}\n",
        )
        .unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = lines.clone();
        let summary = run_batch("jobs.jsonl", "out", 2, 1, &dir, move |l| seen.lock().unwrap().push(l.clone())).unwrap();
        assert_eq!((summary["done"].as_u64(), summary["failed"].as_u64(), summary["resumed"].as_u64()), (Some(0), Some(1), Some(1)));
        let err: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("out/b_error.json")).unwrap()).unwrap();
        assert_eq!(err["attempts"], 2);
        assert!(err["error"].as_str().unwrap().contains("prompt"));
        assert_eq!(lines.lock().unwrap().len(), 1);
        assert!(dir.join("out/manifest.json").is_file() && dir.join("out/progress.jsonl").is_file());
        let dup = std::fs::write(dir.join("dup.jsonl"), "{\"id\":\"x\",\"kind\":\"run\",\"model\":\"m\"}\n{\"id\":\"x\",\"kind\":\"run\",\"model\":\"m\"}\n");
        assert!(dup.is_ok());
        assert!(run_batch("dup.jsonl", "out2", 1, 0, &dir, |_| {}).unwrap_err().contains("twice"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
