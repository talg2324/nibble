//! `nib ai`: local models and coding-assistant sub-agents for scripts and agents, without a
//! window. It talks to nibd directly (on this machine, or over SSH for `@profile`), so jobs run
//! where the GPUs and the data are; paths are relative to the project root there. Output is
//! JSON on stdout (JSON lines for a batch's progress). See the nib-ai skill.

use crate::transport::{self, SshEnv};
use anyhow::{anyhow, bail, Context, Result};
use nib_editor::{Config, Target};
use nib_proto::{AiAgent, ReqId, Request, Response, ServerMsg, ServerNotify};
use serde_json::{json, Map, Value};
use std::io::{BufReader, Write};
use std::process::ChildStdout;

const USAGE: &str = "\
usage: nib ai [@PROFILE] [--root DIR] COMMAND ...
  models                         assistants, their models (local ones: warm or cold), GPUs, and
                                 Ollama's downloaded models with their size on disk
  gpus                           this machine's GPUs (index, name, memory)
  install [NAME]                 install an assistant (default OpenCode, which comes with Ollama:
                                 either one that's missing) -> progress lines, then ok
  pull MODEL                     download an Ollama model (qwen3:8b) -> progress lines, then ok
  rm MODEL                       delete a downloaded Ollama model from disk
  generate MODEL [OPTIONS]       one model call, straight to Ollama -> result JSON
  run MODEL [OPTIONS]            one OpenCode sub-agent -> result JSON
  batch JOBS.jsonl --out DIR [--concurrency N] [--retries N]
                                 every job in the file; resumable -> progress lines, then a summary
  warm MODEL [--gpu SEL]         load a local model and keep it loaded (and primed for OpenCode)
  cold MODEL [--gpu SEL]         unload it

Options (the same keys as a job's JSON):
  --prompt TEXT | --prompt-file PATH   --var NAME=VALUE (fills {NAME} / {NAME:03d}; repeatable)
  --gpu all|cpu|0|0,1   --cwd DIR   --id ID   --job JSON (a whole job; flags win)
  --timeout SECS (default: run 900, generate 1800; 0: no limit)
  generate: --system TEXT  --schema FILE  --temperature T (0)  --seed N (0)  --ctx N
            --max-tokens N  --think | --no-think
  run:      --allow 'CMD PATTERN' (repeatable)  --write 'FILE PATTERN' (repeatable)  --keep-session

Paths are on the machine the jobs run on, relative to the root (--root, the profile's root,
or the current folder). Exit code: 0 ok, 1 the job / a batch job failed, 2 usage or connection.";

/// Entry point for `nib ai ...`; returns the exit code.
pub fn ai_main(args: &[String]) -> i32 {
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("nib ai: {e:#}");
            2
        }
    }
}

struct Args<'a> {
    rest: Vec<&'a str>,
}

impl<'a> Args<'a> {
    /// The value after `flag`, taken out.
    fn take(&mut self, flag: &str) -> Result<Option<&'a str>> {
        let Some(i) = self.rest.iter().position(|a| *a == flag) else { return Ok(None) };
        let v = *self.rest.get(i + 1).ok_or_else(|| anyhow!("{flag} needs a value"))?;
        self.rest.drain(i..=i + 1);
        Ok(Some(v))
    }

    fn take_all(&mut self, flag: &str) -> Result<Vec<&'a str>> {
        let mut out = Vec::new();
        while let Some(v) = self.take(flag)? {
            out.push(v);
        }
        Ok(out)
    }

    fn flag(&mut self, flag: &str) -> bool {
        let Some(i) = self.rest.iter().position(|a| *a == flag) else { return false };
        self.rest.remove(i);
        true
    }

    fn done(&self) -> Result<()> {
        if let Some(a) = self.rest.first() {
            bail!("unexpected argument {a:?}\n{USAGE}");
        }
        Ok(())
    }
}

fn run(args: &[String]) -> Result<i32> {
    let mut a = Args { rest: args.iter().map(String::as_str).collect() };
    if a.rest.is_empty() || a.flag("-h") || a.flag("--help") || a.rest.first() == Some(&"help") {
        println!("{USAGE}");
        return Ok(0);
    }
    let profile = a.rest.first().filter(|p| p.starts_with('@')).map(|p| p[1..].to_string());
    if profile.is_some() {
        a.rest.remove(0);
    }
    let root_flag = a.take("--root")?.map(str::to_string);
    let cmd = if a.rest.is_empty() { bail!("which command?\n{USAGE}") } else { a.rest.remove(0) };

    // The request, built before connecting so a usage mistake costs no connection.
    let req = match cmd {
        "models" => Request::AiAgents,
        "gpus" => Request::AiAgents,
        "warm" | "cold" => {
            let gpu = a.take("--gpu")?.map(str::to_string);
            let model = a.rest.first().copied().ok_or_else(|| anyhow!("{cmd} needs a model"))?.to_string();
            a.rest.remove(0);
            Request::AiWarm { model, warm: cmd == "warm", gpu }
        }
        "install" => {
            let name = a.rest.join(" ");
            a.rest.clear();
            let agent = if name.is_empty() {
                AiAgent::OpenCode
            } else {
                let want = name.to_lowercase();
                *AiAgent::ALL
                    .iter()
                    .find(|x| x.name().to_lowercase() == want || x.name().to_lowercase().split(' ').next() == Some(want.as_str()))
                    .ok_or_else(|| anyhow!("no assistant {name:?}: Claude Code, Codex, Grok or OpenCode"))?
            };
            Request::AiInstall { agent }
        }
        "pull" | "rm" => {
            let model = a.rest.first().copied().ok_or_else(|| anyhow!("{cmd} needs a model, like qwen3:8b"))?.to_string();
            a.rest.remove(0);
            if cmd == "pull" { Request::AiPull { model } } else { Request::AiDelete { model } }
        }
        "generate" | "run" => {
            let job = job(cmd, &mut a)?;
            Request::AiJob { job: job.to_string() }
        }
        "batch" => {
            let out = a.take("--out")?.ok_or_else(|| anyhow!("batch needs --out DIR"))?.to_string();
            let concurrency = a.take("--concurrency")?.map_or(Ok(1), str::parse).context("--concurrency")?;
            let retries = a.take("--retries")?.map_or(Ok(1), str::parse).context("--retries")?;
            let jobs = a.rest.first().copied().ok_or_else(|| anyhow!("batch needs a JOBS.jsonl file"))?.to_string();
            a.rest.remove(0);
            Request::AiBatch { jobs, out, concurrency, retries }
        }
        other => bail!("unknown command {other:?}\n{USAGE}"),
    };
    a.done()?;

    let cfg = Config::load()?;
    let (target, root) = match &profile {
        Some(name) => {
            let p = cfg.profiles.get(name).ok_or_else(|| anyhow!("unknown profile @{name}"))?;
            (p.target()?, root_flag.unwrap_or_else(|| p.root.clone()))
        }
        None => {
            let root = std::path::absolute(root_flag.as_deref().unwrap_or("."))?;
            (Target::Local { nibd: None }, root.to_string_lossy().into_owned())
        }
    };
    // An agent's connection never asks for a password: keys only.
    let env = SshEnv { batch: true, askpass: None };
    let (mut conn, mut rd) = transport::connect(&target, &env, &|msg, _| eprintln!("nib ai: {msg}"))?;
    let hello = conn.request(Request::Hello {
        protocol: nib_proto::PROTOCOL_VERSION,
        client_version: env!("CARGO_PKG_VERSION").into(),
        root,
        lsp: vec![],
    })?;
    wait(&mut rd, hello, |_| {})?;
    let id = conn.request(req)?;
    let mut out = std::io::stdout().lock();
    let answer = wait(&mut rd, id, |line| {
        let _ = writeln!(std::io::stdout().lock(), "{line}");
    });
    // Installing / downloading / deleting failed: said as a result, like a job that failed.
    let answer = match answer {
        Err(e) if matches!(cmd, "install" | "pull" | "rm") => {
            writeln!(out, "{}", json!({ "ok": false, "error": format!("{e:#}") }))?;
            return Ok(1);
        }
        other => other?,
    };
    match (cmd, answer) {
        ("models", Response::AiAgents(agents)) => {
            let gpus = agents.iter().find(|i| !i.gpus.is_empty()).map(|i| i.gpus.clone()).unwrap_or_default();
            let list: Vec<Value> = agents
                .iter()
                .map(|i| {
                    let models: Vec<Value> =
                        i.models.iter().map(|m| json!({ "id": m.id, "name": m.name, "local": m.local, "warm": m.warm })).collect();
                    json!({ "agent": i.agent.name(), "installed": i.installed, "signed_in": i.signed_in, "models": models })
                })
                .collect();
            let ollama = agents.iter().find_map(|i| i.ollama.as_ref()).map(|o| {
                let models: Vec<Value> = o
                    .models
                    .iter()
                    .map(|m| json!({ "name": m.name, "id": format!("ollama/{}", m.name), "bytes": m.bytes, "size": human_bytes(m.bytes), "warm": m.warm }))
                    .collect();
                json!({ "installed": o.installed, "disk_bytes": o.disk_bytes, "disk": human_bytes(o.disk_bytes), "models": models })
            });
            writeln!(out, "{}", serde_json::to_string_pretty(&json!({ "agents": list, "gpus": gpus_json(&gpus), "ollama": ollama }))?)?;
            Ok(0)
        }
        ("gpus", Response::AiAgents(agents)) => {
            let gpus = agents.iter().find(|i| !i.gpus.is_empty()).map(|i| i.gpus.clone()).unwrap_or_default();
            writeln!(out, "{}", serde_json::to_string_pretty(&gpus_json(&gpus))?)?;
            Ok(0)
        }
        (_, Response::Ok) => {
            writeln!(out, "{}", json!({ "ok": true }))?;
            Ok(0)
        }
        (_, Response::AiJobDone { result }) => {
            let v: Value = serde_json::from_str(&result)?;
            writeln!(out, "{}", serde_json::to_string_pretty(&v)?)?;
            Ok(if v["ok"] == true { 0 } else { 1 })
        }
        (_, Response::AiBatchDone { summary }) => {
            let v: Value = serde_json::from_str(&summary)?;
            writeln!(out, "{}", json!({ "summary": v }))?;
            Ok(if v["failed"].as_u64().unwrap_or(0) == 0 { 0 } else { 1 })
        }
        (_, other) => bail!("unexpected answer from nibd: {other:?}"),
    }
}

/// `4_700_000_000` -> `4.7 GB` (decimal units, as Ollama and the OS's file sizes count).
pub fn human_bytes(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < units.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else if v < 10.0 { format!("{v:.1} {}", units[i]) } else { format!("{v:.0} {}", units[i]) }
}

fn gpus_json(gpus: &[nib_proto::AiGpu]) -> Value {
    json!(gpus.iter().map(|g| json!({ "index": g.index, "name": g.name, "memory_mb": g.memory_mb, "kind": g.kind })).collect::<Vec<_>>())
}

/// A job from `--job` and the flags (flags win).
fn job(kind: &str, a: &mut Args) -> Result<Value> {
    let mut j: Map<String, Value> = match a.take("--job")? {
        Some(text) => match serde_json::from_str(text).context("--job isn't JSON")? {
            Value::Object(m) => m,
            _ => bail!("--job is a JSON object"),
        },
        None => Map::new(),
    };
    j.insert("kind".into(), json!(kind));
    if let Some(model) = a.rest.first().copied().filter(|m| !m.starts_with("--")) {
        j.insert("model".into(), json!(model));
        a.rest.remove(0);
    }
    let strings = [
        ("--prompt", "prompt"),
        ("--prompt-file", "prompt_file"),
        ("--gpu", "gpu"),
        ("--cwd", "cwd"),
        ("--id", "id"),
        ("--system", "system"),
        ("--schema", "schema_file"),
    ];
    for (flag, key) in strings {
        if let Some(v) = a.take(flag)? {
            j.insert(key.into(), json!(v));
        }
    }
    let numbers = [("--timeout", "timeout"), ("--temperature", "temperature"), ("--seed", "seed"), ("--ctx", "ctx"), ("--max-tokens", "max_tokens")];
    for (flag, key) in numbers {
        if let Some(v) = a.take(flag)? {
            let n: f64 = v.parse().with_context(|| format!("{flag} {v}"))?;
            j.insert(key.into(), json!(n));
        }
    }
    let vars = a.take_all("--var")?;
    if !vars.is_empty() {
        let m = j.entry("vars").or_insert_with(|| json!({}));
        for v in vars {
            let (k, val) = v.split_once('=').ok_or_else(|| anyhow!("--var takes NAME=VALUE, not {v:?}"))?;
            m[k] = json!(val);
        }
    }
    for (flag, key) in [("--allow", "allow"), ("--write", "write")] {
        let list = a.take_all(flag)?;
        if !list.is_empty() {
            j.insert(key.into(), json!(list));
        }
    }
    if a.flag("--think") {
        j.insert("think".into(), json!(true));
    }
    if a.flag("--no-think") {
        j.insert("think".into(), json!(false));
    }
    if a.flag("--keep-session") {
        j.insert("keep_session".into(), json!(true));
    }
    Ok(Value::Object(j))
}

/// Read until request `id` is answered; batch progress lines go to `progress` meanwhile.
fn wait(rd: &mut BufReader<ChildStdout>, id: ReqId, progress: impl Fn(&str)) -> Result<Response> {
    loop {
        let msg: ServerMsg = nib_proto::recv(rd)?.ok_or_else(|| anyhow!("nibd closed the connection"))?;
        match msg {
            ServerMsg::Response { id: got, result } if got == id => return result.map_err(|e| anyhow!("{}", e.msg)),
            ServerMsg::Notify(ServerNotify::AiProgress { req, line }) if req == id => progress(&line),
            _ => {}
        }
    }
}
