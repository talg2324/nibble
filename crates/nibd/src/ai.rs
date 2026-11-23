//! Chat sessions with the user's own coding assistant: Claude Code, Codex, Grok or OpenCode, each
//! the vendor's CLI run unmodified where the code lives. Claude Code is one long-lived process in
//! print mode with streaming JSON in and out and every tool turned off (a plain chat). The others
//! have no such mode, so each message is one headless run that resumes the previous one's
//! session; they may read the project but not change it.
//!
//! When an assistant is missing or signed out, nibd doesn't send the user away: `setup_cmd` is a
//! terminal that runs the vendor's own curl installer and then its headless sign-in (a device
//! code / a link to open anywhere). The assistant signs itself in; nibd only asks whether that
//! happened and never touches the credentials. OpenCode is for local models, so it comes with
//! Ollama: its setup installs whichever of the two is missing and signs nothing in, and nib tells
//! OpenCode about every model Ollama has downloaded (an `ollama` provider), so a fresh install
//! needs no config.

use crate::history;
use crate::mail;
use crate::diff;
use crate::ollama;
use crate::review;
use crate::work;
use nib_proto::{
    AiAgent, AiAgentInfo, AiAsk, AiEvent, AiOption, AiQuestion, AiId, AiImage, AiLimit, AiMode, AiModel, AiSkill, AiStep, AiStepDone, AiStepKind, AiUsage, ErrorKind, RpcError, ServerMsg, ServerNotify, SkillGroup,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// The last bytes of an assistant's stderr kept for the error message.
const STDERR_TAIL: usize = 2000;

/// How to get, sign in and find one assistant.
struct Spec {
    exe: &'static str,
    /// The vendor's installer, piped to `runner`. Each installs a self-contained binary: no Node.
    /// `None`: nib doesn't install or sign it in.
    install_url: Option<&'static str>,
    runner: &'static str,
    /// Sign-in that works without a browser on this machine (a link or device code to open on
    /// any device).
    login: &'static [&'static str],
    /// Sign-in through a browser on this machine, used instead of `login` when nibd isn't
    /// running under SSH. Codex's device code can be turned off by a ChatGPT workspace admin;
    /// its browser sign-in can't, but it waits on `localhost`, so it only works here.
    local_login: Option<&'static [&'static str]>,
    /// Where its installer puts it, under `$HOME`, besides `PATH`.
    home_dirs: &'static [&'static str],
}

fn spec(agent: AiAgent) -> Spec {
    match agent {
        AiAgent::Claude => Spec {
            exe: "claude",
            install_url: Some("https://claude.ai/install.sh"),
            runner: "bash",
            login: &["auth", "login"],
            local_login: None,
            home_dirs: &[".local/bin", ".claude/local", ".npm-global/bin"],
        },
        AiAgent::Codex => Spec {
            exe: "codex",
            install_url: Some("https://chatgpt.com/codex/install.sh"),
            runner: "sh",
            login: &["login", "--device-auth"],
            local_login: Some(&["login"]),
            home_dirs: &[".local/bin", ".codex/bin", ".npm-global/bin"],
        },
        AiAgent::Grok => Spec {
            exe: "grok",
            install_url: Some("https://x.ai/cli/install.sh"),
            runner: "bash",
            login: &["login", "--device-auth"],
            local_login: None,
            home_dirs: &[".grok/bin", ".local/bin"],
        },
        AiAgent::OpenCode => Spec {
            exe: "opencode",
            install_url: Some("https://opencode.ai/install"),
            runner: "bash",
            login: &[],
            local_login: None,
            home_dirs: &[".opencode/bin", ".local/bin", ".npm-global/bin", ".bun/bin"],
        },
    }
}

/// Ollama, which comes with OpenCode: its installer (Linux and macOS) and how to run it.
const OLLAMA_URL: &str = "https://ollama.com/install.sh";
const OLLAMA_RUNNER: &str = "sh";

fn home() -> Option<PathBuf> {
    nib_proto::path::home_dir()
}

/// Where `agent` itself looks for skills in the user's home (it loads them without nib).
fn provider_skill_dirs(agent: AiAgent) -> &'static [&'static str] {
    match agent {
        AiAgent::Claude => &[".claude/skills"],
        AiAgent::Codex => &[".agents/skills", ".codex/skills"],
        AiAgent::Grok => &[".agents/skills", ".grok/skills"],
        AiAgent::OpenCode => &[".agents/skills", ".config/opencode/skills"],
    }
}

/// A project's skills, nib's neutral folder first.
const REPO_SKILL_DIRS: [&str; 3] = [".skills", ".claude/skills", ".agents/skills"];

/// The skills a chat with `agent` can use from the `/` menu that live on this machine: the
/// assistant's own and the project's. A name already listed in its group is skipped.
pub fn skills(agent: AiAgent, root: Option<&Path>) -> Vec<AiSkill> {
    let provider = home().into_iter().flat_map(|h| provider_skill_dirs(agent).iter().map(move |d| (h.join(d), SkillGroup::Provider)));
    let repo = root.into_iter().flat_map(|r| REPO_SKILL_DIRS.iter().map(move |d| (r.join(d), SkillGroup::Repo)));
    let mut out: Vec<AiSkill> = Vec::new();
    for (dir, group) in provider.chain(repo) {
        for s in nib_proto::skills::list(&dir, group) {
            if !out.iter().any(|o| o.group == s.group && o.name == s.name) {
                out.push(s);
            }
        }
    }
    out
}

fn host() -> String {
    crate::fs_ops::hostname()
}

/// Whether nibd was started over SSH (sshd sets these; nib starts a remote nibd through `ssh`
/// and a local one directly), so a browser it opens would be on another machine.
fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT"].iter().any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}

/// The sign-in command's arguments, and whether it opens a browser on this machine.
fn login_args(sp: &Spec, remote: bool) -> (&'static [&'static str], bool) {
    match sp.local_login {
        Some(l) if !remote => (l, true),
        _ => (sp.login, false),
    }
}

/// Where the installers put things, besides `PATH` (a GUI app or a non-login SSH shell often has
/// a shorter `PATH` than the user's terminal).
fn install_dirs(agent: AiAgent) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if agent == AiAgent::Grok {
        if let Some(g) = std::env::var_os("GROK_HOME") {
            dirs.push(PathBuf::from(g).join("bin"));
        }
    }
    if let Some(h) = home() {
        dirs.extend(spec(agent).home_dirs.iter().map(|d| h.join(d)));
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs
}

/// Where the assistant's binary is: on `PATH`, or where its installer puts it.
fn find(agent: AiAgent) -> Option<PathBuf> {
    let exe = spec(agent).exe;
    // Windows: the native installers make `.exe`s; npm makes `.cmd` shims, which `Command` runs
    // through cmd.exe (with the arguments escaped for it).
    let names: Vec<String> = if cfg!(windows) { vec![format!("{exe}.exe"), format!("{exe}.cmd")] } else { vec![exe.to_string()] };
    let on_path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    let dirs: Vec<PathBuf> = on_path.into_iter().chain(install_dirs(agent)).collect();
    names.iter().find_map(|n| dirs.iter().map(|d| d.join(n)).find(|p| p.is_file()))
}

/// `PATH` with every assistant's install folder in front, for the setup terminal: what it
/// installs is found right away, before the user's shell profile learns about it.
fn setup_path() -> OsString {
    let mut dirs: Vec<PathBuf> = AiAgent::ALL.iter().flat_map(|a| install_dirs(*a)).collect();
    // Ollama's macOS app, should its link in /usr/local/bin not be made.
    dirs.push(PathBuf::from("/Applications/Ollama.app/Contents/Resources"));
    dirs.dedup();
    dirs.extend(std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default());
    std::env::join_paths(dirs).unwrap_or_default()
}

fn env_set(k: &str) -> bool {
    std::env::var_os(k).is_some_and(|v| !v.is_empty())
}

/// Whether the assistant is signed in, from what its sign-in leaves on disk: nothing is started
/// (asking `claude auth status` / `codex login status` takes seconds on a slow remote). `None`:
/// can't tell from here (the chat is started anyway and reports its own error).
fn signed_in(agent: AiAgent) -> Option<bool> {
    let dir = |env: &str, name: &str| std::env::var_os(env).map(PathBuf::from).or_else(|| home().map(|h| h.join(name)));
    match agent {
        AiAgent::Claude => {
            if env_set("ANTHROPIC_API_KEY") || env_set("CLAUDE_CODE_OAUTH_TOKEN") {
                return Some(true);
            }
            // `claude auth login` records the account here; the token itself may be in the
            // keychain.
            let config = match std::env::var_os("CLAUDE_CONFIG_DIR") {
                Some(d) => PathBuf::from(d).join(".claude.json"),
                None => home()?.join(".claude.json"),
            };
            let Ok(text) = std::fs::read_to_string(&config) else { return Some(false) };
            let v: Value = serde_json::from_str(&text).ok()?;
            if v.get("oauthAccount").is_some_and(|a| a.is_object()) || v.get("primaryApiKey").is_some_and(|k| k.is_string()) {
                return Some(true);
            }
            // The installer's first run writes this file before any sign-in. Without an account
            // it is signed out, unless it gets in another way: a cloud provider, a token, or an
            // `apiKeyHelper` in its settings.
            let other = ["ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY"].into_iter().any(env_set);
            let settings = config.with_file_name(".claude").join("settings.json");
            let settings = match std::env::var_os("CLAUDE_CONFIG_DIR") {
                Some(d) => PathBuf::from(d).join("settings.json"),
                None => settings,
            };
            let configured = std::fs::read_to_string(settings)
                .is_ok_and(|s| ["apiKeyHelper", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_USE_"].iter().any(|k| s.contains(k)));
            if other || configured { None } else { Some(false) }
        }
        AiAgent::Codex => {
            if env_set("OPENAI_API_KEY") {
                return Some(true);
            }
            let dir = dir("CODEX_HOME", ".codex")?;
            if dir.join("auth.json").is_file() {
                return Some(true);
            }
            // Kept in the OS keyring instead: can't see it from here.
            let keyring = std::fs::read_to_string(dir.join("config.toml")).is_ok_and(|c| c.contains("cli_auth_credentials_store"));
            if keyring { None } else { Some(false) }
        }
        AiAgent::Grok => {
            if env_set("XAI_API_KEY") {
                return Some(true);
            }
            // `grok login` leaves its token here.
            Some(dir("GROK_HOME", ".grok")?.join("auth.json").is_file())
        }
        // Signing in is per model provider, and a local one needs none.
        AiAgent::OpenCode => None,
    }
}

fn can_install(agent: AiAgent) -> bool {
    cfg!(unix) && spec(agent).install_url.is_some()
}

/// How long `opencode models` may take before nib gives up on its list.
const LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// Claude Code's aliases for the latest model of each family: the list when its own isn't there.
const CLAUDE_MODELS: [(&str, &str); 4] = [("fable", "Fable"), ("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")];

/// The models an installed assistant can chat with, best first. Claude Code keeps the list its
/// account may use in `cache/model-catalog` (else: an alias for each family's latest); Codex in
/// `models_cache.json`;
/// OpenCode lists what its providers have (local ones included). Grok: none, it runs its own.
fn models(agent: AiAgent, exe: &Path, root: Option<&Path>) -> Vec<AiModel> {
    let model = |id: &str, name: &str| AiModel { id: id.to_string(), name: name.to_string(), efforts: Vec::new(), local: false, warm: None };
    match agent {
        AiAgent::Claude => {
            let listed = claude_catalog().map(|text| claude_models(&text)).unwrap_or_default();
            if listed.is_empty() {
                CLAUDE_MODELS.iter().map(|(id, name)| model(id, name)).collect()
            } else {
                listed
            }
        }
        AiAgent::Codex => {
            let dir = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| home().map(|h| h.join(".codex")));
            let text = dir.and_then(|d| std::fs::read_to_string(d.join("models_cache.json")).ok()).unwrap_or_default();
            codex_models(&text)
        }
        AiAgent::OpenCode => {
            let mut cmd = Command::new(exe);
            // Listing is offline: its catalog as last fetched, not a download from models.dev.
            opencode_offline(&mut cmd);
            // With Ollama's downloaded models in it.
            cmd.env("OPENCODE_CONFIG_CONTENT", opencode_config(Value::Null, "", &ollama::GpuSel::All));
            cmd.arg("models");
            if let Some(root) = root {
                // Its project config may add providers.
                cmd.current_dir(root);
            }
            let out = run_for(cmd, LIST_TIMEOUT).unwrap_or_default();
            let pulled = ollama::pulled();
            // Asked only when there's something local to be warm.
            let loaded = if pulled.is_empty() { Default::default() } else { ollama::loaded() };
            let ids: Vec<&str> = out.lines().map(str::trim).filter(|l| l.contains('/') && !l.contains(' ')).collect();
            // A local model the user's own provider (under another name) lists too: only theirs.
            let theirs: std::collections::HashSet<String> = ids
                .iter()
                .filter_map(|id| id.split_once('/'))
                .filter(|(p, m)| *p != "ollama" && pulled.contains(&ollama::name(m)))
                .map(|(_, m)| ollama::name(m))
                .collect();
            ids.into_iter()
                .filter(|id| !id.strip_prefix("ollama/").is_some_and(|m| theirs.contains(&ollama::name(m))))
                .map(|id| {
                    // `provider/model`: whatever the user named the provider, the model is local
                    // if Ollama has its weights.
                    let name = ollama::name_of(id);
                    let local = pulled.contains(&name);
                    AiModel { local, warm: local.then(|| loaded.contains(&name)), ..model(id, id) }
                })
                .collect()
        }
        AiAgent::Grok => Vec::new(),
    }
}

/// Claude Code's own model list for the signed-in account (what its `/model` shows), the newest
/// of `<config dir>/cache/model-catalog/*-cc.json`.
fn claude_catalog() -> Option<String> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| home().map(|h| h.join(".claude")))?;
    let newest = std::fs::read_dir(dir.join("cache/model-catalog"))
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with("-cc.json") && !name.starts_with("published-")
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())?;
    std::fs::read_to_string(newest.path()).ok()
}

/// The models in Claude Code's catalog, in its order (the current ones, then older versions).
fn claude_models(text: &str) -> Vec<AiModel> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let Some(list) = v.pointer("/catalog/config/models").and_then(Value::as_array) else { return Vec::new() };
    list.iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?;
            let name = m.get("name").and_then(Value::as_str).unwrap_or(id);
            Some(AiModel { id: id.to_string(), name: name.to_string(), efforts: Vec::new(), local: false, warm: None })
        })
        .collect()
}

/// The models in Codex's `models_cache.json` that its own picker shows, in its order.
fn codex_models(text: &str) -> Vec<AiModel> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let Some(list) = v.get("models").and_then(Value::as_array) else { return Vec::new() };
    let mut listed: Vec<(i64, AiModel)> = list
        .iter()
        .filter(|m| m.get("visibility").and_then(Value::as_str).is_none_or(|v| v == "list"))
        .filter_map(|m| {
            let id = m.get("slug").and_then(Value::as_str)?;
            let name = m.get("display_name").and_then(Value::as_str).unwrap_or(id);
            let priority = m.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX);
            let efforts = m.get("supported_reasoning_levels").and_then(Value::as_array).into_iter().flatten()
                .filter_map(|level| level.get("effort").and_then(Value::as_str).map(str::to_string)).collect();
            Some((priority, AiModel { id: id.to_string(), name: name.to_string(), efforts, local: false, warm: None }))
        })
        .collect();
    listed.sort_by_key(|(p, _)| *p);
    listed.into_iter().map(|(_, m)| m).collect()
}

/// `cmd`'s stdout, if it exits 0 within `limit`; stopped otherwise.
pub(crate) fn run_for(mut cmd: Command, limit: Duration) -> Option<String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < limit => thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let out = reader.join().ok()?;
    status.success().then_some(out)
}

/// Every assistant, in `AiAgent::ALL` order, with the models of each one installed. Quick: a
/// look at `PATH` and a few files each, and `opencode models` (local) when it's there.
pub fn agents(root: Option<&Path>) -> Vec<AiAgentInfo> {
    // Each assistant, and OpenCode's models, GPUs and Ollama, looked at all at once: the slow
    // ones start a program (`opencode models`, `sysctl`) or ask Ollama over HTTP.
    thread::scope(|s| {
        let handles: Vec<_> = AiAgent::ALL
            .iter()
            .map(|&agent| {
                s.spawn(move || {
                    let local = agent == AiAgent::OpenCode;
                    thread::scope(|s| {
                        let gpus = local.then(|| s.spawn(ollama::gpus));
                        let ollama = local.then(|| s.spawn(ollama::info));
                        let exe = find(agent);
                        let installed = exe.is_some();
                        let signed = installed.then(|| s.spawn(move || signed_in(agent)));
                        let models = exe.map(|e| models(agent, &e, root)).unwrap_or_default();
                        AiAgentInfo {
                            agent,
                            installed,
                            signed_in: signed.and_then(|h| h.join().ok()).flatten(),
                            can_install: can_install(agent),
                            models,
                            gpus: gpus.and_then(|h| h.join().ok()).unwrap_or_default(),
                            ollama: ollama.and_then(|h| h.join().ok()),
                        }
                    })
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("looking at an assistant panicked")).collect()
    })
}

/// The setup terminal: install the assistant if it's missing (OpenCode: and Ollama), sign it in if
/// it isn't, then say it's ready and exit 0. Run without a terminal (`AiInstall`), its prompts
/// read nothing and fail. A step that fails says why and waits for Enter, so the message isn't
/// gone with the terminal. Returns the command and the environment to run it with.
pub fn setup_cmd(agent: AiAgent, force_login: bool) -> Result<(Vec<String>, OsString), RpcError> {
    let sp = spec(agent);
    let Some(install_url) = sp.install_url else {
        return Err(RpcError::new(
            ErrorKind::BadRequest,
            format!("nib doesn't set up {}. Install it and set up a model for it yourself, then try again.", agent.name()),
        ));
    };
    if !can_install(agent) {
        return Err(RpcError::new(
            ErrorKind::BadRequest,
            format!("nib can't install {} on Windows yet. Install it from its website, sign in, then try again.", agent.name()),
        ));
    }
    let login = !sp.login.is_empty() && (force_login || find(agent).is_none() || signed_in(agent) != Some(true));
    // Arguments stay arguments: nothing below is pasted into the script.
    const SCRIPT: &str = r#"name=$1 exe=$2 url=$3 runner=$4 host=$5 login=$6 xname=$7 xexe=$8 xurl=$9 xrunner=${10} browser=${11}; shift 11
fail() { printf '\n\033[31m%s\033[0m\nPress Enter to close.\n' "$1"; read -r _; exit 1; }
install() {
  command -v "$2" >/dev/null 2>&1 && return 0
  printf '\033[1mInstalling %s on %s\033[0m with its own installer:\n  curl -fsSL %s | %s\n\n' "$1" "$host" "$3" "$4"
  command -v curl >/dev/null 2>&1 || fail "curl isn't installed on $host, so nib can't install $1 here."
  command -v "$4" >/dev/null 2>&1 || fail "The $1 installer needs $4, which isn't installed on $host."
  script=$(curl -fsSL "$3") || fail "Couldn't download $3."
  "$4" -c "$script" "$4" || fail "The $1 installer failed."
  command -v "$2" >/dev/null 2>&1 || fail "The installer finished, but nib can't find $2 on $host."
  echo
}
install "$name" "$exe" "$url" "$runner"
[ -n "$xname" ] && install "$xname" "$xexe" "$xurl" "$xrunner"
if [ "$login" = yes ]; then
  if [ "$browser" = yes ]; then
    printf '\033[1mSigning %s in on %s.\033[0m Finish in the browser that opens (or open the link below here).\n\n' "$name" "$host"
  else
    printf '\033[1mSigning %s in on %s.\033[0m Open the link below on any device and follow it.\n\n' "$name" "$host"
  fi
  "$exe" "$@" || fail "Signing in to $name didn't finish."
fi
if [ -n "$xname" ]; then printf '\n\033[32m%s and %s are ready on %s.\033[0m\n' "$name" "$xname" "$host"
else printf '\n\033[32m%s is ready on %s.\033[0m\n' "$name" "$host"; fi
sleep 1
"#;
    let mut cmd: Vec<String> = ["sh", "-c", SCRIPT, "nib-setup", agent.name(), sp.exe, install_url, sp.runner]
        .map(String::from)
        .into();
    cmd.push(host());
    cmd.push(if login { "yes" } else { "no" }.into());
    let extra: [&str; 4] = if agent == AiAgent::OpenCode { ["Ollama", "ollama", OLLAMA_URL, OLLAMA_RUNNER] } else { [""; 4] };
    cmd.extend(extra.map(String::from));
    let (login_args, browser) = login_args(&sp, over_ssh());
    cmd.push(if browser { "yes" } else { "no" }.into());
    cmd.extend(login_args.iter().map(|s| s.to_string()));
    Ok((cmd, setup_path()))
}

/// `AiInstall`: the setup without a terminal. `progress` gets each line it prints (colours taken
/// out); a prompt reads nothing, so a step that asks (a `sudo` password) fails, and so does a
/// sign-in, which needs someone to open its link: those want `setup_cmd`'s terminal.
pub fn install(agent: AiAgent, mut progress: impl FnMut(&str)) -> Result<(), String> {
    if !spec(agent).login.is_empty() && find(agent).is_some() {
        return Ok(());
    }
    let (cmd, path) = setup_cmd(agent, false).map_err(|e| e.msg)?;
    // Its errors are part of what it says.
    let mut sh = Command::new("sh");
    sh.args(["-c", "exec 2>&1; exec \"$@\"", "sh"]).args(&cmd).env("PATH", path);
    let mut child = sh.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| format!("starting the installer: {e}"))?;
    let mut out = child.stdout.take().ok_or("no installer output")?;
    let mut last = String::new();
    let mut line = Vec::new();
    let mut buf = [0u8; 4096];
    let mut flush = |line: &mut Vec<u8>, last: &mut String| {
        let text = strip_ansi(&String::from_utf8_lossy(line));
        line.clear();
        let text = text.trim_end();
        if !text.trim().is_empty() && text.trim() != "Press Enter to close." {
            progress(text);
            *last = text.trim().to_string();
        }
    };
    loop {
        let n = out.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                flush(&mut line, &mut last);
            } else {
                line.push(b);
            }
        }
    }
    flush(&mut line, &mut last);
    match child.wait() {
        Ok(st) if st.success() => Ok(()),
        _ if last.is_empty() => Err(format!("installing {} failed", agent.name())),
        _ => Err(last),
    }
}

/// `s` without terminal colour codes.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for c in it.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The update terminal: the assistant's own updater (Claude Code, OpenCode), else its installer
/// again, which fetches the latest. Says what happened and waits for Enter, so the new version
/// can be read. Returns the command and the environment to run it with.
pub fn update_cmd(agent: AiAgent) -> Result<(Vec<String>, OsString), RpcError> {
    let sp = spec(agent);
    let Some(exe) = find(agent) else {
        return Err(not_ready(agent, ErrorKind::NotFound, "isn't installed"));
    };
    let exe = exe.to_string_lossy().into_owned();
    // Arguments stay arguments: nothing below is pasted into the script.
    const SCRIPT: &str = r#"name=$1 exe=$2 host=$3 url=$4 runner=$5; shift 5
printf '\033[1mUpdating %s on %s\033[0m\n' "$name" "$host"
printf 'Before: %s\n\n' "$("$exe" --version 2>/dev/null | head -n 1)"
if [ $# -gt 0 ]; then
  "$exe" "$@"
else
  command -v curl >/dev/null 2>&1 || { printf '\n\033[31mcurl isn'"'"'t installed on %s.\033[0m\n' "$host"; read -r _; exit 1; }
  script=$(curl -fsSL "$url") && "$runner" -c "$script" "$runner"
fi
code=$?
printf '\nNow: %s\n' "$("$exe" --version 2>/dev/null | head -n 1)"
if [ $code -eq 0 ]; then printf '\033[32m%s is up to date.\033[0m' "$name"; else printf '\033[31mThe update failed.\033[0m' ; fi
printf ' Press Enter to close.\n'; read -r _; exit $code
"#;
    let updater: &[&str] = match agent {
        AiAgent::Claude => &["update"],
        AiAgent::OpenCode => &["upgrade"],
        AiAgent::Codex | AiAgent::Grok => &[],
    };
    if updater.is_empty() && (sp.install_url.is_none() || !cfg!(unix)) {
        return Err(RpcError::new(ErrorKind::BadRequest, format!("nib can't update {} here: update it the way you installed it.", agent.name())));
    }
    let mut cmd: Vec<String> = ["sh", "-c", SCRIPT, "nib-update", agent.name()].map(String::from).into();
    cmd.extend([exe, host(), sp.install_url.unwrap_or("").to_string(), sp.runner.to_string()]);
    cmd.extend(updater.iter().map(|s| s.to_string()));
    Ok((cmd, setup_path()))
}

/// How long priming a model may take: loading it and reading OpenCode's instructions.
const PRIME_TIMEOUT: Duration = Duration::from_secs(900);

/// Make a local model warm (`model` is OpenCode's id, `ollama/qwen3:8b`): Ollama loads it and
/// keeps it in memory, then throwaway OpenCode messages, sent the way a chat in `root` sends
/// them, have Ollama read OpenCode's own instructions (thousands of tokens, most of the wait for
/// a first reply) and keep them. Ollama keeps that work at the points where prompts it has seen
/// part ways, so it takes two messages that part where a chat's own message starts. Where that
/// is depends on the first word (a token earlier for some), so they are one of each kind: the
/// point they part at is one every message gets to. The next chat's first message then only
/// pays for itself. Cold: unloaded. Answers once that's all done.
/// `gpu`: which GPUs' Ollama server (the user's own for all of them).
pub fn set_warm(model: &str, warm: bool, gpu: &ollama::GpuSel, root: &Path) -> Result<(), String> {
    ollama::set_warm(model.split_once('/').map_or(model, |(_, m)| m), warm, gpu)?;
    if warm {
        // Loaded is most of warm: a prime that fails only costs the first reply its speed.
        let config = opencode_config(read_only_permissions(), model, gpu);
        if let Err(e) = prime(model, root, &config) {
            log!("priming {model}: {e}");
        }
    }
    Ok(())
}

fn prime(model: &str, root: &Path, config: &str) -> Result<(), String> {
    let exe = find(AiAgent::OpenCode).ok_or("OpenCode isn't installed")?;
    // "Reply…" parts from the chat's prefix a token later than "What…" (and "`…", "/…") do.
    for text in ["Reply with just: ok", "What is 1+1? Answer with the number only."] {
        prime_once(&exe, model, root, text, config)?;
    }
    Ok(())
}

/// OpenCode's config for one run, on top of the user's own: these `permission`s, and for a
/// model on another GPU selection's Ollama, its provider pointed at that server.
pub(crate) fn opencode_config(permission: Value, model: &str, gpu: &ollama::GpuSel) -> String {
    let mut config = serde_json::json!({});
    if !permission.is_null() {
        config["permission"] = permission;
    }
    // Every model Ollama has downloaded, as OpenCode's `ollama` provider (merged into the user's
    // own, if they have one by that name).
    let pulled = ollama::pulled();
    if let Some(addr) = ollama::addr_of(&ollama::GpuSel::All).filter(|_| !pulled.is_empty()) {
        let models: serde_json::Map<String, Value> = pulled.into_iter().map(|m| (m, serde_json::json!({}))).collect();
        config["provider"]["ollama"] = serde_json::json!({
            "npm": "@ai-sdk/openai-compatible",
            "name": "Ollama (local)",
            "options": { "baseURL": ollama::base_url(addr) },
            "models": models,
        });
    }
    if *gpu != ollama::GpuSel::All {
        if let (Some((provider, _)), Some(addr)) = (model.split_once('/'), ollama::addr_of(gpu)) {
            config["provider"][provider]["options"]["baseURL"] = Value::from(ollama::base_url(addr));
        }
    }
    config.to_string()
}

/// OpenCode's own trips to the internet turned off: refreshing its model catalog, checking for
/// updates, fetching language servers and plugins. For listing models and for anything that runs
/// a local model, so an editor without internet access (and a local model) still works. Chats
/// with a cloud model are left alone: they go online anyway.
pub(crate) fn opencode_offline(cmd: &mut Command) {
    for k in ["OPENCODE_DISABLE_MODELS_FETCH", "OPENCODE_DISABLE_AUTOUPDATE", "OPENCODE_DISABLE_LSP_DOWNLOAD", "OPENCODE_DISABLE_DEFAULT_PLUGINS"] {
        cmd.env(k, "1");
    }
}

/// The OpenCode binary, if it's installed here.
pub(crate) fn opencode_exe() -> Option<PathBuf> {
    find(AiAgent::OpenCode)
}

fn prime_once(exe: &Path, model: &str, root: &Path, text: &str, config: &str) -> Result<(), String> {
    let mut cmd = Command::new(exe);
    // Priming is for local models only.
    opencode_offline(&mut cmd);
    cmd.env("OPENCODE_CONFIG_CONTENT", config)
        .args(one_shot_args(AiAgent::OpenCode, Some(model), None, AiMode::Plan, &[], text))
        .env("PWD", root)
        .current_dir(root);
    let out = run_for(cmd, PRIME_TIMEOUT).ok_or("the warm-up message didn't finish")?;
    // Not a chat of the user's: out of OpenCode's session list.
    let session = out.lines().find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        v.get("sessionID").and_then(Value::as_str).map(str::to_string)
    });
    if let Some(id) = session {
        let mut del = Command::new(exe);
        del.args(["session", "delete", &id]).current_dir(root);
        run_for(del, LIST_TIMEOUT * 4);
    }
    Ok(())
}

/// One chat.
enum Session {
    /// Claude Code: one process for the whole chat; messages go in on stdin.
    /// `asks`: what it asked leave for and is waiting on, by request id: the tool's input, which
    /// goes back with a yes.
    Stream { stdin: ChildStdin, child: Child, asks: Asks },
    /// Codex / Grok / OpenCode: one process per message.
    PerTurn(Arc<PerTurn>),
}

type Asks = Arc<Mutex<HashMap<String, Value>>>;

struct PerTurn {
    agent: AiAgent,
    /// For its runs' environment: nib's folders and the mailbox.
    env: Vec<(&'static str, OsString)>,
    watch: Watch,
    /// Letters that came during a run: they start the next one.
    mail: Mutex<Vec<String>>,
    /// It may act (within `mode`), not only read.
    tools: bool,
    mode: Mutex<AiMode>,
    exe: PathBuf,
    /// `None`: the one it's set up with.
    model: Option<String>,
    /// Thinking level and system prompt, as `agent` takes them.
    tuning: Tuning,
    /// OpenCode: the system prompt, written out (its config takes instruction files).
    prompt_file: Option<PathBuf>,
    root: PathBuf,
    tx: Sender<ServerMsg>,
    id: AiId,
    /// The assistant's own session id, from the first reply on: the next message continues it.
    resume: Mutex<Option<String>>,
    /// The run answering now.
    child: Mutex<Option<Child>>,
    /// Codex app-server's JSON-RPC input while a turn is running.
    codex_input: Mutex<Option<ChildStdin>>,
    /// Question IDs kept until the user answers the matching app-server request.
    codex_questions: Mutex<HashMap<String, (Value, Vec<(String, Vec<String>)>)>>,
    /// The model runs on this machine (an Ollama one): its speed is shown, not a cost.
    local: bool,
    /// What the chat has cost so far (OpenCode).
    cost: Mutex<f64>,
    /// Codex's record of the chat, once found: it has the context window and usage limits.
    rollout: Mutex<Option<PathBuf>>,
    /// The summary a compaction left: it goes in front of the next message, which starts a new
    /// session.
    carry: Mutex<Option<String>>,
}

impl Drop for PerTurn {
    fn drop(&mut self) {
        if let Some(f) = &self.prompt_file {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// What a Codex / Grok / OpenCode run is for.
#[derive(Clone, Copy, PartialEq)]
enum Run {
    /// Answer the user: the reply streams to the client.
    Chat,
    /// Summarise the chat for `compact`: the reply is kept, not shown.
    Compact,
}

/// Asks an assistant with no compaction of its own for the summary that replaces the chat.
const COMPACT_PROMPT: &str = "Summarise our conversation so far for a fresh session that will continue it without seeing \
it. Keep what's needed to carry on: the goal, decisions made and why, facts established, file \
and symbol names, open questions and the next step. Leave out pleasantries and anything settled \
that no longer matters. Reply with the summary only.";

/// Heads the summary in the first message after a compaction.
const CARRY_HEAD: &str = "(Our conversation so far was compacted. Summary of it:)";

#[derive(Default)]
pub struct Sessions {
    next: AiId,
    map: HashMap<AiId, Session>,
    /// Context for the first message after changing providers (also for streamed sessions).
    carry: HashMap<AiId, String>,
    /// Sessions whose processes are frozen (`suspend`).
    frozen: std::collections::HashSet<AiId>,
    /// The agents' mailbox folder (`mail`), once a session has started.
    mail: Option<PathBuf>,
    /// Worktree-per-feature (D-080) is off: the user said agents may change the shared project
    /// folder. Off only when told (`set_worktree_per_feature`).
    wpf_off: bool,
    /// Sessions already told they changed the shared project folder with worktree-per-feature on.
    wpf_told: std::collections::HashSet<AiId>,
    /// The project the sessions work in: where every letter is kept (`history::letter`).
    root: Option<PathBuf>,
    /// The post office thread is running.
    posting: bool,
    meta: HashMap<AiId, Meta>,
    /// Who last changed each file, and the pairs already told about sharing one.
    edits: HashMap<String, AiId>,
    warned: std::collections::HashSet<(String, AiId, AiId)>,
    /// Every file each session changed with its edit tools: its share of the shared work tree
    /// (`DiffScope::Agent`).
    touched: HashMap<AiId, std::collections::BTreeSet<String>>,
    /// When each file was last changed with an edit tool, and by whom.
    edit_at: HashMap<String, (AiId, Instant)>,
    /// Where agents' branch records, plans and reviews are kept between sessions
    /// (`$NIBDIR/work`, by conversation).
    store: Option<PathBuf>,
    /// A command (or, for an assistant that reports its commands only when they are over, a
    /// whole run) of a session in the shared work tree is under way: what was changed there
    /// when it began, and when that was. What differs when it ends is the session's doing.
    windows: HashMap<(AiId, String), (diff::Changed, Instant)>,
}

/// Where an agent's work is, for a diff of it.
pub enum DiffTarget {
    /// On its own branch, checked out at `dir` (`nib-work start`), which left `base`.
    Tree { dir: PathBuf, base: String },
    /// In the shared work tree: the files it changed (absolute).
    Paths(Vec<PathBuf>),
    /// On its own branch at `dir`: what changed there since `commit` (the last review).
    Since { dir: PathBuf, commit: String },
}

/// What session `id` changed since its work was last reviewed (`None`: never, or it has no
/// work tree of its own).
pub fn diff_since(sessions: &Shared, id: AiId) -> Option<DiffTarget> {
    let s = sessions.lock().unwrap();
    let (dir, agent) = (s.mail.as_deref()?, &s.meta.get(&id)?.id_name);
    Some(DiffTarget::Since { dir: work::record(dir, agent)?.dir, commit: review::since(dir, agent)? })
}

/// The mailbox folder, session `id`'s name in it, and where it works (its work tree, or the
/// project).
fn review_of(sessions: &Shared, id: AiId) -> Option<(PathBuf, String, PathBuf, Option<work::Record>)> {
    let s = sessions.lock().unwrap();
    let (dir, agent) = (s.mail.clone()?, s.meta.get(&id)?.id_name.clone());
    let rec = work::record(&dir, &agent);
    let at = rec.as_ref().map(|r| r.dir.clone()).or_else(|| s.root.clone())?;
    Some((dir, agent, at, rec))
}

/// The review of session `id`'s work (`review::load`).
pub fn review(sessions: &Shared, id: AiId) -> nib_proto::Review {
    review_of(sessions, id).map(|(dir, agent, at, _)| review::load(&dir, &agent, &at)).unwrap_or_default()
}

/// Change it (`review::edit`); sending it also gives what to say to the agent.
pub fn review_edit(sessions: &Shared, id: AiId, op: nib_proto::ReviewOp) -> Result<(nib_proto::Review, Option<String>), String> {
    let (dir, agent, at, rec) = review_of(sessions, id).ok_or("no such agent")?;
    let message = match op {
        nib_proto::ReviewOp::Send => {
            // On its own branch: its work as it is now is kept, to compare the next round with.
            let snapshot = rec.as_ref().and_then(|r| diff::snapshot(&r.tree, &r.branch));
            Some(review::send(&dir, &agent, snapshot)?)
        }
        op => {
            review::edit(&dir, &agent, op)?;
            None
        }
    };
    // The client hears that the review changed, as it does when the agent answers.
    mail::report(&dir, &agent, &mail::Report::Work);
    Ok((review::load(&dir, &agent, &at), message))
}

/// The work of session `id` (`None`: no such agent).
pub fn diff_target(sessions: &Shared, id: AiId) -> Option<DiffTarget> {
    let s = sessions.lock().unwrap();
    let m = s.meta.get(&id)?;
    if let Some(r) = s.mail.as_deref().and_then(|dir| work::record(dir, &m.id_name)) {
        return Some(DiffTarget::Tree { dir: r.dir, base: r.base });
    }
    Some(DiffTarget::Paths(s.touched.get(&id).map(|t| t.iter().map(PathBuf::from).collect()).unwrap_or_default()))
}

/// The work of every agent, in the order they were started.
pub fn diff_targets(sessions: &Shared) -> Vec<(AiId, DiffTarget)> {
    let mut ids: Vec<AiId> = sessions.lock().unwrap().meta.keys().copied().collect();
    ids.sort();
    ids.into_iter().filter_map(|id| Some((id, diff_target(sessions, id)?))).collect()
}

/// Land session `id`'s branch on its base (`work::land`): succeeded, and what to tell the user.
pub fn land(sessions: &Shared, id: AiId, root: &Path, message: &str) -> (bool, String) {
    let found = {
        let s = sessions.lock().unwrap();
        s.mail.clone().zip(s.meta.get(&id).map(|m| m.id_name.clone()))
    };
    let Some((dir, agent)) = found else { return (false, "no such agent".to_string()) };
    match work::land(&dir, &agent, root, message) {
        Ok(out) => {
            // Its branch is gone: the client hears so.
            mail::report(&dir, &agent, &mail::Report::Work);
            (true, out)
        }
        Err(e) => (false, e),
    }
}

/// The plan session `id` recorded.
pub fn plan(sessions: &Shared, id: AiId) -> Option<String> {
    let s = sessions.lock().unwrap();
    work::plan(s.mail.as_deref()?, &s.meta.get(&id)?.id_name)
}

pub type Shared = Arc<Mutex<Sessions>>;

fn not_ready(agent: AiAgent, kind: ErrorKind, what: &str) -> RpcError {
    RpcError::new(kind, format!("{} {what} on {}.", agent.name(), host()))
}

/// What the daemon knows about a session beside the session itself: for the mailbox.
struct Meta {
    /// The session's id in the mailbox (`claude-3`): its inbox, and `$NIB_AGENT`.
    id_name: String,
    /// What the agents call it: after its task once it has one.
    name: String,
    agent: AiAgent,
    model: Option<String>,
    /// The start of the user's first message.
    task: String,
    /// How much it may do without asking, as last heard.
    mode: AiMode,
    busy: Arc<AtomicBool>,
    tx: Sender<ServerMsg>,
    /// Writes the conversation down (`history`).
    rec: Arc<Mutex<history::Recorder>>,
    /// Its own scratch folder (`$NIB_SCRATCH` for it): the git work trees it makes there are
    /// removed when it ends (`close`).
    scratch: Option<PathBuf>,
}

/// What a session's reader thread tells the rest: that its turn ended, and which files it
/// changed (two agents in one file are told about each other).
#[derive(Clone)]
struct Watch {
    sessions: std::sync::Weak<Mutex<Sessions>>,
    busy: Arc<AtomicBool>,
    rec: Arc<Mutex<history::Recorder>>,
}

/// The window of a whole run (`Sessions::windows`).
const RUN: &str = "run";

impl Watch {
    /// Something that may change files of the shared work tree begins for session `id`
    /// (`Sessions::windows`). Nothing to watch for a session on its own branch: its work is its
    /// branch.
    fn begins(&self, id: AiId, key: &str) {
        let Some(shared) = self.sessions.upgrade() else { return };
        let root = {
            let s = shared.lock().unwrap();
            let own_tree = s.mail.as_deref().zip(s.meta.get(&id)).is_some_and(|(dir, m)| work::record(dir, &m.id_name).is_some());
            if own_tree || !s.meta.contains_key(&id) {
                return;
            }
            s.root.clone()
        };
        let Some(root) = root else { return };
        // git runs without the lock held.
        let before = diff::changed(&root);
        shared.lock().unwrap().windows.insert((id, key.to_string()), (before, Instant::now()));
    }

    /// It ended: the files that differ from when it began are session `id`'s.
    fn ends(&self, id: AiId, key: &str) {
        let Some(shared) = self.sessions.upgrade() else { return };
        let (window, root) = {
            let mut s = shared.lock().unwrap();
            (s.windows.remove(&(id, key.to_string())), s.root.clone())
        };
        let (Some((before, since)), Some(root)) = (window, root) else { return };
        let after = diff::changed(&root);
        let mut s = shared.lock().unwrap();
        let mut mine = Vec::new();
        for (path, now) in after {
            if before.get(&path) == Some(&now) {
                continue;
            }
            let path = path.to_string_lossy().into_owned();
            // Another agent's edit tool changed it meanwhile: theirs.
            if s.edit_at.get(&path).is_some_and(|(by, at)| *by != id && *at >= since) {
                continue;
            }
            s.touched.entry(id).or_default().insert(path.clone());
            s.edits.entry(path.clone()).or_insert(id);
            mine.push(path);
        }
        drop(s);
        if let Some(first) = mine.iter().min() {
            self.stray_alarm(id, Path::new(first));
        }
    }

    /// Session `id` changed `path` in the shared project folder. With worktree-per-feature on
    /// that is not where its work goes (D-080): it is told so, once, and how to move what it
    /// did onto a branch of its own. Ignored files are not counted.
    fn stray_alarm(&self, id: AiId, path: &Path) {
        let Some(shared) = self.sessions.upgrade() else { return };
        let (root, dir, name, branch) = {
            let s = shared.lock().unwrap();
            if s.wpf_off || s.wpf_told.contains(&id) {
                return;
            }
            let (Some(root), Some(dir), Some(m)) = (s.root.clone(), s.mail.clone(), s.meta.get(&id)) else { return };
            (root.clone(), dir.clone(), m.name.clone(), work::record(&dir, &m.id_name))
        };
        let path = if path.is_relative() { root.join(path) } else { path.to_path_buf() };
        let (path, root) = (work::canonical_near(&path), work::canonical_near(&root));
        let Ok(rel) = path.strip_prefix(&root) else { return };
        if rel.as_os_str().is_empty() || work::ignored(&root, rel) {
            return;
        }
        let to = match &branch {
            Some(r) => format!("Your work tree is {}: make your changes there, and run `nib-work carry` to move what you changed here into it.", r.dir.display()),
            None => "Stop changing files here. Record your plan (`nib-work plan FILE`), then run `nib-work start SLUG --carry`: it moves what you changed here into your new work tree and puts these files back as they were. From then on work only in that work tree.".to_string(),
        };
        let text = format!(
            "Worktree-per-feature is on, and you changed (or tried to change) {} in the shared project folder ({}), where the user's own work is: it would not land, and the user would find it uncommitted in their tree. {to} (nib-work skill)",
            rel.display(),
            root.display()
        );
        if !shared.lock().unwrap().wpf_told.insert(id) {
            return;
        }
        let _ = mail::post(&dir, &mail::Letter { from: mail::NIB.into(), to: name, text, at: 0 });
    }

    fn see(&self, id: AiId, event: &AiEvent) {
        self.rec.lock().unwrap().event(event);
        match event {
            AiEvent::Step(AiStep { id: step, kind: AiStepKind::Command { background: false, .. }, .. }) => self.begins(id, step),
            AiEvent::StepDone(done) => self.ends(id, &done.id),
            _ => {}
        }
        match event {
            AiEvent::TurnDone { .. } | AiEvent::Exit { .. } => {
                self.busy.store(false, Ordering::Relaxed);
                self.ends(id, RUN);
            }
            AiEvent::Step(AiStep { kind: AiStepKind::Edit { path, .. } | AiStepKind::Write { path, .. }, .. }) if !path.is_empty() => {
                if let Some(s) = self.sessions.upgrade() {
                    s.lock().unwrap().changed(id, path);
                }
                self.stray_alarm(id, Path::new(path));
            }
            AiEvent::Mode(mode) => {
                if let Some(s) = self.sessions.upgrade() {
                    if let Some(m) = s.lock().unwrap().meta.get_mut(&id) {
                        m.mode = *mode;
                    }
                }
            }
            // A plan it asks approval for is its plan, whether or not it also runs `nib-work plan`.
            AiEvent::Ask(ask) if ask.tool == "ExitPlanMode" && !ask.what.trim().is_empty() => {
                let Some(s) = self.sessions.upgrade() else { return };
                let s = s.lock().unwrap();
                if let (Some(dir), Some(m)) = (&s.mail, s.meta.get(&id)) {
                    if work::set_plan(dir, &m.id_name, &ask.what).is_ok() {
                        mail::report(dir, &m.id_name, &mail::Report::Work);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Sessions {
    /// Session `id` is changing `path`. If another live agent changed it before, both are told,
    /// once: they share the working tree.
    fn changed(&mut self, id: AiId, path: &str) {
        self.touched.entry(id).or_default().insert(path.to_string());
        self.edit_at.insert(path.to_string(), (id, Instant::now()));
        let before = self.edits.insert(path.to_string(), id).filter(|b| *b != id && self.meta.contains_key(b));
        let (Some(other), Some(dir)) = (before, self.mail.clone()) else { return };
        if !self.warned.insert((path.to_string(), id.min(other), id.max(other))) {
            return;
        }
        let (Some(a), Some(b)) = (self.meta.get(&id), self.meta.get(&other)) else { return };
        let text = format!(
            "{} is changing {path}, which {} changed earlier in this session. You share one working tree: agree by mailbox who goes on with this file before either of you edits it again, and read it again before you do (nib-agent-mailbox skill).",
            a.name, b.name
        );
        for to in [&a.name, &b.name] {
            let _ = mail::post(&dir, &mail::Letter { from: mail::NIB.into(), to: to.clone(), text: text.clone(), at: 0 });
        }
    }

    fn roster(&self) -> Vec<mail::Entry> {
        let mut ids: Vec<&AiId> = self.meta.keys().collect();
        ids.sort();
        // An agent nobody has given a task yet is not at work: it is not in the mailbox.
        ids.into_iter()
            .filter(|id| !self.meta[id].task.is_empty())
            .map(|id| {
                let m = &self.meta[id];
                let rel = |p: &String| self.root.as_deref().and_then(|r| Path::new(p).strip_prefix(r).ok()).map_or(p.clone(), |r| r.to_string_lossy().into_owned());
                let mut files: Vec<String> = self.edits.iter().filter(|(_, by)| *by == id).map(|(p, _)| rel(p)).collect();
                files.sort();
                mail::Entry {
                    id: m.id_name.clone(),
                    name: m.name.clone(),
                    assistant: m.agent.name().to_string(),
                    model: m.model.clone(),
                    task: m.task.clone(),
                    busy: m.busy.load(Ordering::Relaxed),
                    files,
                    mode: match m.mode {
                        AiMode::Auto => "auto",
                        AiMode::Manual => "manual",
                        AiMode::AcceptEdits => "accept-edits",
                        AiMode::Plan => "plan",
                    }
                    .to_string(),
                }
            })
            .collect()
    }
}

/// Deliver the letters agents put in the outbox, and keep the roster current, for as long as
/// the daemon's sessions live.
fn post_office(sessions: std::sync::Weak<Mutex<Sessions>>) {
    let mut last = String::new();
    loop {
        thread::sleep(Duration::from_millis(300));
        let Some(shared) = sessions.upgrade() else { return };
        let (dir, roster) = {
            let s = shared.lock().unwrap();
            (s.mail.clone(), s.roster())
        };
        let Some(dir) = dir else { continue };
        let now = serde_json::to_string(&roster).unwrap_or_default();
        if now != last {
            mail::write_roster(&dir, &roster);
            last = now;
        }
        for letter in mail::collect(&dir) {
            route(&shared, &dir, letter);
        }
    }
}

/// Deliver one letter: into the recipient's inbox, and to both chats for the user to see. Only a
/// letter to the recipient by name is said to it as a message: from another agent (it may be a
/// question: an idle agent starts a turn for it), or nib's own note while it is working (its
/// harness shows it after its current step). A letter to `all` is news: it waits in the inbox
/// and goes with the agent's next message, so it never interrupts or wakes anyone (D-062).
/// Agents with no task get nothing.
fn route(shared: &Shared, dir: &Path, letter: mail::Letter) {
    let mut later: Vec<(Arc<PerTurn>, String)> = Vec::new();
    {
        let mut s = shared.lock().unwrap();
        // To nib: an agent saying its job is done, or what a sub-agent it started is doing
        // (D-061). Shown in its own chat, delivered to nobody.
        if letter.to == mail::NIB {
            let event = match serde_json::from_str::<mail::Report>(&letter.text) {
                Ok(mail::Report::Finished(f)) => Some(AiEvent::Finished(f)),
                Ok(mail::Report::Step(step)) => Some(AiEvent::Step(step)),
                Ok(mail::Report::StepDone(done)) => Some(AiEvent::StepDone(done)),
                Ok(mail::Report::Work) => {
                    // Kept for when this conversation is reopened in another session.
                    if let (Some(store), Some(m)) = (&s.store, s.meta.values().find(|m| m.id_name == letter.from)) {
                        work::persist(dir, &letter.from, store, &m.rec.lock().unwrap().id());
                    }
                    Some(AiEvent::Work(work::state(dir, &letter.from)))
                }
                Err(_) => None,
            };
            if let (Some((id, m)), Some(event)) = (s.meta.iter().find(|(_, m)| m.name == letter.from || m.id_name == letter.from), event) {
                m.rec.lock().unwrap().event(&event);
                let _ = m.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id: *id, event }));
            }
            return;
        }
        if let Some(root) = &s.root {
            history::letter(root, &letter.from, &letter.to, &letter.text, letter.at as i64);
        }
        let sender = s.meta.iter().find(|(_, m)| m.name == letter.from).map(|(id, _)| *id);
        let to_one = letter.to != "all";
        let targets: Vec<AiId> = s
            .meta
            .iter()
            .filter(|(id, m)| Some(**id) != sender && !m.task.is_empty() && (!to_one || m.name == letter.to || m.id_name == letter.to))
            .map(|(id, _)| *id)
            .collect();
        let mail_event = |m: &Meta, id: AiId, to: &str, incoming: bool| {
            let event = AiEvent::Mail(nib_proto::AiMail { from: letter.from.clone(), to: to.to_string(), text: letter.text.clone(), incoming });
            let _ = m.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id, event }));
        };
        if let Some(m) = sender.and_then(|id| s.meta.get(&id)) {
            mail_event(m, sender.unwrap_or_default(), &letter.to, false);
        }
        for id in targets {
            let Some(m) = s.meta.get(&id) else { continue };
            let (name, inbox) = (m.name.clone(), m.id_name.clone());
            mail::deliver(dir, &inbox, &letter);
            mail_event(m, id, &name, true);
            if mail::waiting(dir, &inbox) {
                continue;
            }
            let wake = to_one && (letter.from != mail::NIB || m.busy.load(Ordering::Relaxed));
            if !wake {
                continue;
            }
            // With it, whatever was waiting in the inbox.
            let text = mail::said(&mail::unread(dir, &inbox, None));
            if text.is_empty() {
                continue;
            }
            m.busy.store(true, Ordering::Relaxed);
            match s.map.get_mut(&id) {
                Some(Session::Stream { stdin, .. }) => {
                    let msg = serde_json::json!({ "type": "user", "message": { "role": "user", "content": text } });
                    let _ = writeln!(stdin, "{msg}").and_then(|_| stdin.flush());
                }
                // One run at a time: a letter that comes during a run starts the next one.
                Some(Session::PerTurn(p)) if p.child.lock().unwrap().is_some() => p.mail.lock().unwrap().push(text),
                Some(Session::PerTurn(p)) => later.push((Arc::clone(p), text)),
                None => {}
            }
        }
    }
    for (p, text) in later {
        p.send(&text, &[]);
    }
}

/// Session `id`'s system prompt as a file, for OpenCode's `instructions`.
fn prompt_file(id: AiId, prompt: &str) -> Option<PathBuf> {
    let Some(dir) = crate::places::private_temp() else {
        log!("system prompt: no private temp directory");
        return None;
    };
    let f = dir.join(format!("nib-prompt-{id}.md"));
    match std::fs::write(&f, prompt) {
        Ok(()) => Some(f),
        Err(e) => {
            log!("system prompt: {}: {e}", f.display());
            None
        }
    }
}

/// Where a session's agent finds its scratch and artifact folders.
pub struct Dirs<'a> {
    pub scratch: Option<&'a Path>,
    pub nibdir: Option<&'a Path>,
}

/// What the user set for a session beside the model: how hard it thinks and what is added to
/// the assistant's system prompt.
#[derive(Default, Clone)]
pub struct Tuning {
    /// One of `AiAgent::efforts`; anything else is dropped.
    pub effort: Option<String>,
    pub prompt: Option<String>,
}

impl Tuning {
    /// Only what `agent` takes: a level it has, a prompt with something in it.
    fn for_agent(self, agent: AiAgent) -> Tuning {
        Tuning {
            effort: self.effort.filter(|e| agent.efforts().contains(&e.as_str())),
            prompt: self.prompt.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()),
        }
    }

    /// Claude Code's flags for it.
    fn claude_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(e) = &self.effort {
            a.extend(["--effort".to_string(), e.clone()]);
        }
        if let Some(p) = &self.prompt {
            a.extend(["--append-system-prompt".to_string(), p.clone()]);
        }
        a
    }

}

/// Codex has no `--plugin-dir` equivalent. Give it the bundled skills' metadata and paths so it
/// can read the relevant SKILL.md on demand, without copying skills into the user's home/repo.
fn codex_skill_catalog(skills: &[AiSkill]) -> String {
    let listed: Vec<String> = skills.iter().filter(|s| s.name != "nib-upstream").map(|s| {
        format!("- {}: {} ({})", s.name, s.description, s.path)
    }).collect();
    if listed.is_empty() {
        return String::new();
    }
    format!("Nibble skills available in this session. When a task calls for one, read its SKILL.md at the listed path before using it. User instructions take precedence over skill guidance.\n{}", listed.join("\n"))
}

/// Check that `agent` is there and signed in and start a chat at `root` with `model` (`None`:
/// the one it's set up with); what it says streams to the client as `AiEvent`s.
#[allow(clippy::too_many_arguments)]
pub fn open(sessions: &Shared, replace: Option<AiId>, agent: AiAgent, model: Option<String>, tuning: Tuning, tools: bool, mode: AiMode, resume: Option<&str>, root: &Path, dirs: Dirs, tx: Sender<ServerMsg>) -> Result<AiId, RpcError> {
    let tuning = tuning.for_agent(agent);
    let Some(exe) = find(agent) else {
        return Err(not_ready(agent, ErrorKind::NotFound, "isn't installed"));
    };
    if signed_in(agent) == Some(false) {
        return Err(not_ready(agent, ErrorKind::PermissionDenied, "isn't signed in"));
    }
    if agent == AiAgent::OpenCode && !ollama::pulled().is_empty() {
        // Its local models need the server; a cold model then loads with the first message.
        if let Err(e) = ollama::ensure_running() {
            log!("ollama: {e}");
        }
    }
    open_ready(sessions, replace, agent, model, tuning, tools, mode, resume, root, dirs, tx, exe)
}

/// Start a validated assistant. Keeping this separate also lets lifecycle tests run without
/// installed assistants, credentials, or paid model calls.
#[allow(clippy::too_many_arguments)]
fn open_ready(sessions: &Shared, replace: Option<AiId>, agent: AiAgent, model: Option<String>, tuning: Tuning, tools: bool, mode: AiMode, resume: Option<&str>, root: &Path, dirs: Dirs, tx: Sender<ServerMsg>, exe: PathBuf) -> Result<AiId, RpcError> {
    let mut s = sessions.lock().unwrap();
    if let Some(old) = replace {
        let m = s.meta.get(&old).ok_or_else(|| RpcError::new(ErrorKind::NotFound, "the previous conversation is no longer available"))?;
        if m.busy.load(Ordering::Relaxed) {
            return Err(RpcError::new(ErrorKind::BadRequest, "stop the current reply before switching models"));
        }
    }
    let id = s.next;
    s.next += 1;
    if s.mail.is_none() {
        s.mail = mail::dir(dirs.scratch);
        if let Some(dir) = &s.mail {
            mail::set_wpf(dir, !s.wpf_off);
        }
    }
    if s.root.is_none() {
        s.root = Some(root.to_path_buf());
    }
    if s.store.is_none() {
        s.store = dirs.nibdir.map(|d| d.join("work"));
    }
    if !std::mem::replace(&mut s.posting, true) {
        let weak = Arc::downgrade(sessions);
        thread::spawn(move || post_office(weak));
    }
    // What the agent's commands see: nib's folders (the nib-artifacts skill), and the mailbox.
    let name = replace.and_then(|old| s.meta.get(&old)).map(|m| m.id_name.clone()).unwrap_or_else(|| mail::name(agent, id));
    let mut env: Vec<(&'static str, OsString)> = Vec::new();
    // A scratch folder of its own inside the window's, so what it makes there (a work tree to
    // compare against, say) is known to be its, and goes when it does (`close`).
    let own_scratch = dirs.scratch.map(|p| p.join("agents").join(&name)).filter(|p| std::fs::create_dir_all(p).is_ok());
    env.extend(own_scratch.as_ref().map(|p| ("NIB_SCRATCH", p.as_os_str().to_os_string())));
    env.extend(dirs.nibdir.map(|p| ("NIBDIR", p.as_os_str().to_os_string())));
    if let (true, Some(dir), Ok(me)) = (tools, &s.mail, std::env::current_exe()) {
        env.push(("NIB_MAILBOX", dir.as_os_str().to_os_string()));
        env.push(("NIB_AGENT", name.clone().into()));
        // What its `nib-app` and `nib-web` requests carry, so the daemon knows they are its.
        env.push((crate::tokens::VAR, crate::tokens::issue(&name).into()));
        env.push(("NIBD", me.into_os_string()));
        // For `nib-work`: where the project is, whatever folder the agent is in.
        env.push(("NIB_ROOT", root.as_os_str().to_os_string()));
        // `nib-mail` is in the mailbox's `bin`.
        // `join_paths` uses the system's separator (`;` on Windows).
        let rest = std::env::var_os("PATH").unwrap_or_default();
        let dirs = std::iter::once(dir.join("bin")).chain(std::env::split_paths(&rest));
        if let Ok(path) = std::env::join_paths(dirs) {
            env.push(("PATH", path));
        }
    }
    // A past conversation goes on in its own record, with the assistant's session of it.
    // It also keeps its task and the name made from it.
    let (rec, session, past, previous_agent) = if let Some((old, m)) = replace.and_then(|old| s.meta.get(&old).map(|m| (old, m))) {
        let session = match s.map.get(&old) {
            Some(Session::PerTurn(p)) => p.resume.lock().unwrap().clone(),
            _ => m.rec.lock().unwrap().native_session(),
        };
        (m.rec.clone(), session, None, Some(m.agent))
    } else {
        match resume.and_then(|id| history::Recorder::resume(root, id)) {
            Some((rec, session, past)) => {
                let previous = past.agent;
                (Arc::new(Mutex::new(rec)), session, Some(past), Some(previous))
            }
            None => (Arc::new(Mutex::new(history::Recorder::new(root, agent, model.as_deref()))), None, None, None),
        }
    };
    let session = session.filter(|_| previous_agent == Some(agent));
    let mut recording = rec.lock().unwrap();
    let mut carry = if session.is_none() { recording.context() } else { String::new() };
    let watch = Watch { sessions: Arc::downgrade(sessions), busy: Arc::new(AtomicBool::new(false)), rec: rec.clone() };
    let native = session.clone();
    let session = match agent {
        AiAgent::Claude => open_claude(&exe, model.as_deref(), &tuning, tools, mode, session.as_deref(), root, &env, watch.clone(), id, tx.clone())?,
        AiAgent::Codex | AiAgent::Grok | AiAgent::OpenCode => Session::PerTurn(Arc::new(PerTurn {
            agent,
            tools,
            mode: Mutex::new(mode),
            env,
            watch: watch.clone(),
            mail: Mutex::new(Vec::new()),
            exe,
            local: agent == AiAgent::OpenCode && model.as_deref().is_some_and(|m| ollama::pulled().contains(&ollama::name_of(m))),
            cost: Mutex::new(0.0),
            rollout: Mutex::new(None),
            model: model.clone(),
            prompt_file: (agent == AiAgent::OpenCode).then(|| tuning.prompt.as_deref().and_then(|p| prompt_file(id, p))).flatten(),
            tuning: tuning.clone(),
            root: root.to_path_buf(),
            tx: tx.clone(),
            id,
            resume: Mutex::new(session),
            child: Mutex::new(None),
            codex_input: Mutex::new(None),
            codex_questions: Mutex::new(HashMap::new()),
            carry: Mutex::new(None),
        })),
    };
    recording.selection(agent, model.as_deref(), native.as_deref());
    drop(recording);
    s.map.insert(id, session);
    let old = replace.and_then(|old| {
        let m = s.meta.remove(&old)?;
        let touched = s.touched.remove(&old).unwrap_or_default();
        s.touched.insert(id, touched);
        for by in s.edits.values_mut() { if *by == old { *by = id; } }
        for (by, _) in s.edit_at.values_mut() { if *by == old { *by = id; } }
        s.carry.remove(&old);
        let session = s.map.remove(&old);
        Some((m, session, s.frozen.remove(&old)))
    });
    if tools {
        let taken: Vec<&String> = s.meta.values().map(|m| &m.name).collect();
        let (task, called) = if let Some((m, _, _)) = &old {
            (m.task.clone(), m.name.clone())
        } else {
            match past {
                Some(p) => (p.title, p.name.filter(|n| !taken.contains(&n)).unwrap_or_else(|| name.clone())),
                None => (String::new(), name.clone()),
            }
        };
        if called != name {
            let _ = tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id, event: AiEvent::Name(called.clone()) }));
        }
        // A reopened conversation: its branch, plan and review come back, and the files it
        // changed with its edit tools are its again (the session that knew is gone).
        if let Some(past) = resume.filter(|_| replace.is_none()) {
            if let (Some(dir), Some(store)) = (s.mail.clone(), s.store.clone()) {
                work::restore(&dir, &name, &store, past);
                mail::report(&dir, &name, &mail::Report::Work);
            }
            let edited = history::load(root, past).map(|(_, entries)| entries).unwrap_or_default().into_iter().filter_map(|e| match e {
                nib_proto::AiPastEntry::Step(AiStep { kind: AiStepKind::Edit { path, .. } | AiStepKind::Write { path, .. }, .. }) if !path.is_empty() => Some(path),
                _ => None,
            });
            s.touched.entry(id).or_default().extend(edited);
        }
        s.meta.insert(id, Meta { id_name: name, name: called, agent, model, task, mode, busy: watch.busy, tx, rec: watch.rec, scratch: own_scratch });
    }
    if !carry.is_empty() {
        if let (Some(dir), Some(m)) = (&s.mail, s.meta.get(&id)) {
            if let Some(record) = work::record(dir, &m.id_name) {
                carry.push_str(&format!("\n\nContinue in the existing work tree: {}. Branch: {}. Do not start a new work item.\n", record.dir.display(), record.branch));
            }
            if let Some(plan) = work::plan(dir, &m.id_name) {
                carry.push_str(&format!("Current plan:\n{plan}\n"));
            }
            carry.push_str("Use nib-review comments to read any outstanding review before continuing.\n");
        }
        s.carry.insert(id, carry);
    }
    drop(s);
    if let Some((_, Some(session), frozen)) = old {
        if let (true, Some(pid)) = (frozen, pid(&session)) { let _ = signal_tree(pid, "CONT"); }
        end(session);
    }
    Ok(id)
}

/// Codex's sandbox lets a run write in the folder it is started in and nowhere else (that
/// folder's `.git` included). Nib's scratch, persistent data, mailbox and work trees may all live
/// outside it, and an agent's work tree commits into the repository's git folder.
///
/// Of that git folder, only what a work tree's commits need: the objects, the agents' branches
/// (`refs/heads/nib/…` and their logs), and git's folders for work trees. Not its `config` or
/// `hooks` (what they name runs outside the sandbox, by the next git command nib or the user runs),
/// `HEAD`, the user's own branches, or `info/`. The folders are made here: the sandbox cannot
/// let a run make one inside a folder it may not write.
fn codex_roots(root: &Path, env: &[(&'static str, OsString)]) -> Vec<String> {
    let var = |key: &str| env.iter().find(|(k, _)| *k == key).map(|(_, v)| PathBuf::from(v));
    let mut roots: Vec<PathBuf> = Vec::new();
    roots.extend(var("NIB_SCRATCH"));
    roots.extend(var("NIBDIR"));
    roots.extend(var("NIB_MAILBOX"));
    let git = crate::git::cmd(root).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).output();
    if let Ok(out) = git {
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !dir.is_empty() {
            let common = PathBuf::from(dir);
            for part in ["objects", "refs/heads/nib", "logs/refs/heads/nib", "worktrees"] {
                let p = common.join(part);
                if std::fs::create_dir_all(&p).is_ok() {
                    roots.push(p);
                }
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots.iter().map(|r| r.to_string_lossy().into_owned()).collect()
}

/// The folder agents' work trees go in (`$NIBDIR/worktrees`), made if needed.
fn work_trees(env: &[(&'static str, OsString)]) -> Option<PathBuf> {
    let dir = PathBuf::from(&env.iter().find(|(k, _)| *k == "NIBDIR")?.1).join("worktrees");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Collects the end of stderr on its own thread (a full pipe would stall the assistant).
fn stderr_tail(mut stderr: ChildStderr) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        let s = String::from_utf8_lossy(&buf);
        let s = s.trim();
        let cut = s.len().saturating_sub(STDERR_TAIL);
        let cut = (cut..=s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(s.len());
        s[cut..].to_string()
    })
}

#[allow(clippy::too_many_arguments)]
fn open_claude(exe: &Path, model: Option<&str>, tuning: &Tuning, tools: bool, mode: AiMode, resume: Option<&str>, root: &Path, env: &[(&'static str, OsString)], watch: Watch, id: AiId, tx: Sender<ServerMsg>) -> Result<Session, RpcError> {
    let mut cmd = Command::new(exe);
    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
    cmd.args(tuning.claude_args());
    let mut child = cmd
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            // None of the user's MCP servers.
            "--strict-mcp-config",
        ])
        // An agent works in the mode picked; what that mode makes it ask comes to nib over
        // stdout (`can_use_tool`) and is answered on stdin. A plain chat has no tools at all.
        .args(if tools { vec!["--permission-mode", claude_mode(mode), "--permission-prompt-tool", "stdio"] } else { vec!["--tools", ""] })
        .args(resume.map(|id| vec!["--resume", id]).unwrap_or_default())
        // The mailbox is nib's own, and the agents use it at each other's request, not only the
        // user's: without this, auto mode's safety check refuses a reply as unasked-for.
        // Not `nib-app set`: registering an app is a command nib keeps and the agent may run, so
        // it is asked about like any other command the agent would run.
        .args(if tools { vec!["--allowedTools", "Bash(nib-mail:*)", "Bash(nib-work:*)", "Bash(nib-review:*)", "Bash(nib-app list:*)", "Bash(nib-app open:*)", "Bash(nib-app up:*)", "Bash(nib-app down:*)", "Bash(nib-app restart:*)", "Bash(nib-app log:*)", "Bash(nib-app rm:*)", "Bash(nib-web:*)"] } else { vec![] })
        // Where `nib-work start` puts an agent's own work tree: its commands may `cd` there.
        .args(work_trees(env).filter(|_| tools).map(|d| vec!["--add-dir".to_string(), d.to_string_lossy().into_owned()]).unwrap_or_default())
        // nib's own skills, as a plugin for this session (they show up as `nib:<name>`).
        .args(skills_plugin().filter(|_| tools).map(|d| vec!["--plugin-dir".to_string(), d.to_string_lossy().into_owned()]).unwrap_or_default())
        .current_dir(root)
        .envs(env.iter().map(|(k, v)| (*k, v.as_os_str())))
        // Started from a terminal Claude Code runs in, this would mark the chat as nested.
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RpcError::new(ErrorKind::Io, format!("starting {}: {e}", exe.display())))?;
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let errors = stderr_tail(child.stderr.take().expect("piped"));
    let asks = Asks::default();
    let reader_asks = asks.clone();
    thread::spawn(move || {
        let send = |event: AiEvent| {
            watch.see(id, &event);
            tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id, event })).is_ok()
        };
        let mut turn = Turn { asks: reader_asks, ..Turn::default() };
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            // Its own id for the session: what a reopened conversation resumes.
            if let (Some("system"), Some(sid)) = (v.get("type").and_then(Value::as_str), v.get("session_id").and_then(Value::as_str)) {
                watch.rec.lock().unwrap().session(sid);
            }
            for event in turn.read(&v) {
                if !send(event) {
                    return;
                }
            }
        }
        let tail = errors.join().unwrap_or_default();
        send(AiEvent::Exit { msg: Some(tail).filter(|t| !t.is_empty()) });
    });
    Ok(Session::Stream { stdin, child, asks })
}

/// Turns Claude Code's stream-json lines into `AiEvent`s.
#[derive(Default)]
struct Turn {
    /// Text was already sent this turn (a second text block starts a new paragraph).
    wrote: bool,
    /// The last request's prompt and reply, in tokens: what the context holds now.
    context: Option<u64>,
    /// The account's usage limits, from its last `rate_limit_event`.
    limits: Vec<AiLimit>,
    /// See `Session::Stream`.
    asks: Asks,
    /// The steps of sub-agents running in the background: their launch came back at once, and
    /// a `task_notification` ends them.
    background: HashSet<String>,
}

/// The sum of the token counts at `keys` in `usage` (`None` when it has none of them).
fn tokens(usage: &Value, keys: &[&str]) -> Option<u64> {
    let counts: Vec<u64> = keys.iter().filter_map(|k| usage.pointer(k).and_then(Value::as_u64)).collect();
    (!counts.is_empty()).then(|| counts.iter().sum())
}

/// Claude Code's usage windows (`unifiedWindows`: `five_hour`, `seven_day`, ...), shortest
/// first. Ones nib doesn't know the length of are left out.
fn claude_limits(info: &Value) -> Vec<AiLimit> {
    let minutes = |name: &str| match name {
        "five_hour" => Some(300),
        n if n.starts_with("seven_day") => Some(7 * 24 * 60),
        _ => None,
    };
    let mut out: Vec<AiLimit> = info
        .get("unifiedWindows")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, w)| {
            Some(AiLimit {
                window_minutes: minutes(name)?,
                used: w.get("utilization")?.as_f64()? as f32,
                resets_at: w.get("resetsAt").and_then(Value::as_i64),
            })
        })
        .collect();
    out.sort_by_key(|l| l.window_minutes);
    out
}

impl Turn {
    fn read(&mut self, v: &Value) -> Vec<AiEvent> {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        match v.get("type").and_then(Value::as_str) {
            Some("system") if s(v, "subtype").as_deref() == Some("init") => {
                s(v, "model").map(|model| vec![AiEvent::Started { model }]).unwrap_or_default()
            }
            Some("stream_event") => {
                let Some(e) = v.get("event") else { return vec![] };
                match e.get("type").and_then(Value::as_str) {
                    Some("content_block_start") => {
                        let text_block = e.pointer("/content_block/type").and_then(Value::as_str) == Some("text");
                        if text_block && self.wrote {
                            return vec![AiEvent::Text("\n\n".into())];
                        }
                        vec![]
                    }
                    Some("message_start" | "message_delta") => {
                        let usage = e.get("usage").or_else(|| e.pointer("/message/usage"));
                        let keys = ["/input_tokens", "/cache_creation_input_tokens", "/cache_read_input_tokens", "/output_tokens"];
                        if let Some(n) = usage.and_then(|u| tokens(u, &keys)) {
                            self.context = Some(n);
                        }
                        vec![]
                    }
                    Some("content_block_delta") if e.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta") => {
                        match e.pointer("/delta/text").and_then(Value::as_str) {
                            Some(t) if !t.is_empty() => {
                                self.wrote = true;
                                vec![AiEvent::Text(t.to_string())]
                            }
                            _ => vec![],
                        }
                    }
                    _ => vec![],
                }
            }
            // `/compact` (see `compact`): the conversation is a summary now.
            Some("system") if s(v, "subtype").as_deref() == Some("compact_boundary") => {
                let n = |k: &str| v.pointer(&format!("/compact_metadata/{k}")).and_then(Value::as_u64);
                let (before, after) = (n("pre_tokens"), n("post_tokens"));
                if after.is_some() {
                    self.context = after;
                }
                vec![AiEvent::Compacted { before, after }]
            }
            // A finished assistant message: its text already came as deltas; its tool calls are
            // the steps.
            Some("assistant") => {
                let parent = s(v, "parent_tool_use_id");
                let blocks = v.pointer("/message/content").and_then(Value::as_array);
                blocks
                    .into_iter()
                    .flatten()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    .filter_map(|b| {
                        let kind = claude_step(b.get("name")?.as_str()?, b.get("input").unwrap_or(&Value::Null));
                        Some(AiEvent::Step(AiStep { id: s(b, "id")?, parent: parent.clone(), kind }))
                    })
                    .collect()
            }
            // Tool results come back as a user message.
            Some("user") => {
                // Where an edit landed: the hunk starts with unchanged lines for context.
                let line = v.pointer("/tool_use_result/structuredPatch/0").and_then(|hunk| {
                    let start = hunk.get("newStart")?.as_u64()?;
                    let lines = hunk.get("lines")?.as_array()?;
                    let context = lines.iter().take_while(|l| l.as_str().is_some_and(|l| l.starts_with(' '))).count();
                    Some((start + context as u64) as u32)
                });
                // A sub-agent sent to the background: its launch is not its end.
                let launched = v.pointer("/tool_use_result/status").and_then(Value::as_str) == Some("async_launched");
                let blocks = v.pointer("/message/content").and_then(Value::as_array);
                blocks
                    .into_iter()
                    .flatten()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    .filter_map(|b| {
                        if launched {
                            let id = s(b, "tool_use_id")?;
                            self.background.insert(id.clone());
                            return Some(AiEvent::Background { id });
                        }
                        let output = match b.get("content") {
                            Some(Value::String(t)) => t.clone(),
                            Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
                            _ => String::new(),
                        };
                        let ok = !b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        Some(AiEvent::StepDone(AiStepDone { id: s(b, "tool_use_id")?, ok, output: clip(output), line }))
                    })
                    .collect()
            }
            // A sub-agent in the background ended: its step does.
            Some("system") if s(v, "subtype").as_deref() == Some("task_notification") => {
                let Some(id) = s(v, "tool_use_id").filter(|id| self.background.remove(id)) else { return vec![] };
                let ok = s(v, "status").as_deref() == Some("completed");
                vec![AiEvent::StepDone(AiStepDone { id, ok, output: clip(s(v, "summary").unwrap_or_default()), line: None })]
            }
            // Its mode makes it ask before this tool call: the user answers (`answer`).
            Some("control_request") if v.pointer("/request/subtype").and_then(Value::as_str) == Some("can_use_tool") => {
                let (Some(request), Some(r)) = (s(v, "request_id"), v.get("request")) else { return vec![] };
                let tool = s(r, "tool_name").unwrap_or_default();
                let input = r.get("input").cloned().unwrap_or(Value::Null);
                let questions = if tool == "AskUserQuestion" { claude_questions(&input) } else { vec![] };
                // A plan to approve, or what the harness says the call is.
                let what = if questions.is_empty() { s(&input, "plan").or_else(|| s(r, "description")).unwrap_or_default() } else { String::new() };
                self.asks.lock().unwrap().insert(request.clone(), input);
                vec![AiEvent::Ask(AiAsk { request, step: s(r, "tool_use_id"), tool, what, questions })]
            }
            // The answer to `set_mode`.
            Some("control_response") => {
                let mode = v.pointer("/response/response/mode").and_then(Value::as_str).and_then(|m| match m {
                    "default" | "manual" => Some(AiMode::Manual),
                    "acceptEdits" => Some(AiMode::AcceptEdits),
                    "plan" => Some(AiMode::Plan),
                    "auto" => Some(AiMode::Auto),
                    _ => None,
                });
                mode.map(AiEvent::Mode).into_iter().collect()
            }
            Some("rate_limit_event") => {
                if let Some(info) = v.get("rate_limit_info") {
                    self.limits = claude_limits(info);
                }
                vec![]
            }
            Some("result") => {
                self.wrote = false;
                // `modelUsage` and the cost add up over the whole chat; the window is the model's.
                let window = v.get("modelUsage").and_then(Value::as_object).into_iter().flatten().filter_map(|(_, m)| m.get("contextWindow")?.as_u64()).max();
                let usage = AiUsage {
                    context_tokens: self.context,
                    context_window: window,
                    limits: self.limits.clone(),
                    cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
                    tokens_per_sec: None,
                };
                let failed = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let error = failed.then(|| {
                    s(v, "result")
                        .filter(|r| !r.is_empty())
                        .or_else(|| s(v, "subtype"))
                        .unwrap_or_else(|| "the reply failed".into())
                });
                vec![AiEvent::Usage(usage), AiEvent::TurnDone { error }]
            }
            _ => vec![],
        }
    }
}

/// Claude Code's name for a mode.
fn claude_mode(mode: AiMode) -> &'static str {
    match mode {
        AiMode::Manual => "default",
        AiMode::AcceptEdits => "acceptEdits",
        AiMode::Plan => "plan",
        AiMode::Auto => "auto",
    }
}

/// The questions of an `AskUserQuestion` call.
fn claude_questions(input: &Value) -> Vec<AiQuestion> {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let questions = input.get("questions").and_then(Value::as_array).into_iter().flatten();
    questions
        .map(|q| AiQuestion {
            header: text(q, "header"),
            question: text(q, "question"),
            options: (q.get("options").and_then(Value::as_array).into_iter().flatten())
                .map(|o| AiOption { label: text(o, "label"), description: text(o, "description"), preview: o.get("preview").and_then(Value::as_str).map(str::to_string) })
                .collect(),
            multi: q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
        })
        .collect()
}

/// nib's own skills as a Claude Code plugin for one session: a folder under nib's cache with a
/// link to each skill that ships next to this binary (in a checkout: the nearest `.skills`
/// above it). `None` when there are none here (a daemon deployed without them).
fn bundled_skills() -> Vec<AiSkill> {
    let Some(exe) = std::env::current_exe().ok() else { return Vec::new() };
    let Some(src) = exe.ancestors().skip(1).map(|d| d.join(".skills")).find(|d| d.is_dir()) else { return Vec::new() };
    nib_proto::skills::list(&src, SkillGroup::Nib)
}

fn skills_plugin() -> Option<PathBuf> {
    let skills = bundled_skills();
    link_plugin(&crate::places::xdg("XDG_CACHE_HOME", &[".cache"]).join("plugin").join("nib"), &skills)
}

/// The hook in nib's plugin: before Claude Code edits a file, `nib-work guard` says whether
/// worktree-per-feature allows it (D-080). Its "no" (exit 3) becomes Claude Code's "blocked"
/// (2) with the reason; anything else (not in nib, an older nibd) lets the edit through.
const PLUGIN_HOOKS: &str = r#"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Edit|Write|MultiEdit|NotebookEdit",
        "hooks": [{ "type": "command", "command": "nib-work guard || exit $(( $? == 3 ? 2 : 0 ))" }]
      }
    ]
  }
}
"#;

fn write_plugin_hooks(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join("hooks"))?;
    std::fs::write(dir.join("hooks").join("hooks.json"), PLUGIN_HOOKS)
}

#[cfg(unix)]
fn link_plugin(dir: &Path, skills: &[AiSkill]) -> Option<PathBuf> {
    // Not offered unless the user opted in at install (D-044).
    let skills: Vec<&AiSkill> = skills.iter().filter(|s| s.name != "nib-upstream").collect();
    if skills.is_empty() {
        return None;
    }
    std::fs::create_dir_all(dir.join(".claude-plugin")).ok()?;
    std::fs::write(dir.join(".claude-plugin/plugin.json"), r#"{"name":"nib","description":"Skills for working in the nibble editor","version":"0.1.0"}"#).ok()?;
    write_plugin_hooks(dir).ok()?;
    let links = dir.join("skills");
    let _ = std::fs::remove_dir_all(&links);
    std::fs::create_dir_all(&links).ok()?;
    for s in skills {
        let folder = Path::new(&s.path).parent()?;
        std::os::unix::fs::symlink(folder, links.join(folder.file_name()?)).ok()?;
    }
    Some(dir.to_path_buf())
}

/// Windows: symlinks need Developer Mode or admin, so the skill folders are copied instead.
#[cfg(not(unix))]
fn link_plugin(dir: &Path, skills: &[AiSkill]) -> Option<PathBuf> {
    fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            if e.file_type()?.is_dir() {
                copy_dir(&e.path(), &to.join(e.file_name()))?;
            } else {
                std::fs::copy(e.path(), to.join(e.file_name()))?;
            }
        }
        Ok(())
    }
    // Not offered unless the user opted in at install (D-044).
    let skills: Vec<&AiSkill> = skills.iter().filter(|s| s.name != "nib-upstream").collect();
    if skills.is_empty() {
        return None;
    }
    std::fs::create_dir_all(dir.join(".claude-plugin")).ok()?;
    std::fs::write(dir.join(".claude-plugin/plugin.json"), r#"{"name":"nib","description":"Skills for working in the nibble editor","version":"0.1.0"}"#).ok()?;
    write_plugin_hooks(dir).ok()?;
    let copies = dir.join("skills");
    let _ = std::fs::remove_dir_all(&copies);
    for s in skills {
        let folder = Path::new(&s.path).parent()?;
        copy_dir(folder, &copies.join(folder.file_name()?)).ok()?;
    }
    Some(dir.to_path_buf())
}

/// Base64, for pictures in a message.
pub(crate) fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Pictures for an assistant that takes them as files: saved in nibd's private temp directory,
/// named by run.
fn image_files(images: &[AiImage]) -> Vec<PathBuf> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let Some(dir) = crate::places::private_temp().map(|d| d.join("images")) else { return Vec::new() };
    if images.is_empty() || std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    images
        .iter()
        .filter_map(|i| {
            let ext = i.media_type.rsplit('/').next().unwrap_or("png");
            let path = dir.join(format!("{}.{ext}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            std::fs::write(&path, &i.data).ok().map(|_| path)
        })
        .collect()
}

/// The most of a step's output that goes to the client.
const STEP_OUTPUT: usize = 64 * 1024;

/// An OpenCode `tool_use` part as a step and its end, once the call is over (`None` before).
/// `unnamed`: an id for a call OpenCode gave none. Also how a `nib ai run` sub-agent's calls are
/// shown in its owner's chat (`jobs`).
pub(crate) fn opencode_step(part: &Value, unnamed: impl FnOnce() -> String) -> Option<(AiStep, AiStepDone)> {
    let state = &part["state"];
    let status = state.get("status").and_then(Value::as_str).unwrap_or("");
    if !matches!(status, "completed" | "error") {
        return None;
    }
    let input = &state["input"];
    let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    let n = |k: &str| input.get(k).and_then(Value::as_u64);
    let text = |k: &str| s(k).unwrap_or_default();
    let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
    let kind = match tool {
        "bash" => AiStepKind::Command { command: text("command"), description: s("description"), timeout_ms: n("timeout"), background: false },
        "read" => AiStepKind::Read { path: text("filePath"), offset: n("offset"), limit: n("limit") },
        "edit" => AiStepKind::Edit { path: text("filePath"), old: text("oldString"), new: text("newString") },
        "write" => AiStepKind::Write { path: text("filePath"), content: text("content") },
        "grep" | "glob" | "list" => AiStepKind::Search { pattern: s("pattern").or_else(|| s("path")).unwrap_or_default(), path: s("path") },
        "skill" => AiStepKind::Skill { name: text("name") },
        "task" => AiStepKind::Agent { description: text("description") },
        _ => AiStepKind::Other { tool: tool.to_string(), input: if input.is_null() { String::new() } else { input.to_string() } },
    };
    let id = part.get("callID").or_else(|| part.get("id")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(unnamed);
    let output = state.get("output").or_else(|| state.get("error")).and_then(Value::as_str).unwrap_or_default().to_string();
    Some((AiStep { id: id.clone(), parent: None, kind }, AiStepDone { id, ok: status == "completed", output: clip(output), line: None }))
}


/// `text`, with the middle left out when it is longer than `STEP_OUTPUT`.
fn clip(text: String) -> String {
    if text.len() <= STEP_OUTPUT {
        return text;
    }
    let half = STEP_OUTPUT / 2;
    let (mut a, mut b) = (half, text.len() - half);
    while !text.is_char_boundary(a) {
        a -= 1;
    }
    while !text.is_char_boundary(b) {
        b += 1;
    }
    format!("{}\n… {} bytes left out …\n{}", &text[..a], b - a, &text[b..])
}

/// A Claude Code tool call as a step.
fn claude_step(tool: &str, input: &Value) -> AiStepKind {
    let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    let n = |k: &str| input.get(k).and_then(Value::as_u64);
    let text = |k: &str| s(k).unwrap_or_default();
    match tool {
        "Bash" => AiStepKind::Command {
            command: text("command"),
            description: s("description"),
            // Its own default when the call names none: two minutes.
            timeout_ms: n("timeout"),
            background: input.get("run_in_background").and_then(Value::as_bool).unwrap_or(false),
        },
        "Read" => AiStepKind::Read { path: text("file_path"), offset: n("offset"), limit: n("limit") },
        "Edit" => AiStepKind::Edit { path: text("file_path"), old: text("old_string"), new: text("new_string") },
        "Write" => AiStepKind::Write { path: text("file_path"), content: text("content") },
        "Skill" => AiStepKind::Skill { name: text("skill") },
        "Grep" | "Glob" => AiStepKind::Search { pattern: text("pattern"), path: s("path") },
        "Agent" | "Task" => AiStepKind::Agent { description: s("description").or_else(|| s("prompt")).unwrap_or_default() },
        _ => AiStepKind::Other { tool: tool.to_string(), input: input.to_string() },
    }
}

/// OpenCode's permissions for a chat, on top of the user's own config: it may read the project
/// but not edit it, run commands, fetch pages, start sub-agents or look outside the project.
const OPENCODE_READ_ONLY: &str =
    r#"{"permission":{"edit":"deny","bash":"deny","webfetch":"deny","task":"deny","external_directory":"deny"}}"#;

fn read_only_permissions() -> Value {
    serde_json::from_str::<Value>(OPENCODE_READ_ONLY).map(|v| v["permission"].clone()).unwrap_or_default()
}

/// OpenCode's permissions for an agent in `mode`. A run can't ask, so manual is as careful as
/// plan: it reads only. Never outside the project, never sub-agents of its own.
fn opencode_permissions(mode: AiMode) -> Value {
    let (edit, bash) = match mode {
        AiMode::Manual | AiMode::Plan => return read_only_permissions(),
        AiMode::AcceptEdits => ("allow", "deny"),
        AiMode::Auto => ("allow", "allow"),
    };
    serde_json::json!({ "edit": edit, "bash": bash, "webfetch": bash, "task": "deny", "external_directory": "deny" })
}

/// The arguments for one Grok / OpenCode run. Grok has no model list, so no `model`.
fn one_shot_args(agent: AiAgent, model: Option<&str>, resume: Option<&str>, _mode: AiMode, images: &[PathBuf], text: &str) -> Vec<String> {
    let files = |flag: &str| images.iter().flat_map(|p| [flag.to_string(), p.to_string_lossy().into_owned()]).collect::<Vec<_>>();
    let mut a: Vec<String> = Vec::new();
    match agent {
        AiAgent::Grok => {
            a.extend(["--output-format", "streaming-json", "--tools", "read_file,grep,list_dir"].map(String::from));
            if let Some(id) = resume {
                a.extend(["--resume".to_string(), id.to_string()]);
            }
            // A message starting with `-` would read as a flag. It takes no pictures: it is told
            // where they are.
            let text = if text.starts_with('-') { format!(" {text}") } else { text.to_string() };
            let text = images.iter().fold(text, |t, p| format!("{t}\n(image: {})", p.display()));
            a.extend(["-p".to_string(), text]);
        }
        AiAgent::OpenCode => {
            a.extend(["run", "--format", "json"].map(String::from));
            if let Some(m) = model {
                a.extend(["--model".to_string(), m.to_string()]);
            }
            if let Some(id) = resume {
                a.extend(["--session".to_string(), id.to_string()]);
            }
            a.extend(files("--file"));
            a.extend(["--".to_string(), text.to_string()]);
        }
        AiAgent::Claude | AiAgent::Codex => unreachable!("this assistant does not use one-shot arguments"),
    }
    a
}

/// Turns one Codex `exec --json` / Grok `streaming-json` / OpenCode `run --format json` run into
/// `AiEvent`s.
#[derive(Default)]
struct OneShot {
    wrote: bool,
    /// `TurnDone` was sent.
    done: bool,
    /// The assistant's session id, to continue with the next message.
    session: Option<String>,
    /// The last error it reported, for when the run ends without finishing the turn.
    error: Option<String>,
    /// The last request's prompt and reply, in tokens.
    context: Option<u64>,
    /// What this run cost (OpenCode, summed over its steps).
    cost: f64,
    /// Output tokens written, and the milliseconds spent writing them (OpenCode).
    written: (u64, u64),
    /// The step under way: when it started here, and the span its parts say they took
    /// (`time.start` / `time.end`, in ms).
    step: Option<(Instant, Option<(i64, i64)>)>,
    /// Codex items already shown as a step (`item.started` came).
    started: std::collections::HashSet<String>,
    /// For steps the assistant gives no id.
    unnamed: u32,
}

impl OneShot {
    fn read(&mut self, agent: AiAgent, v: &Value) -> Vec<AiEvent> {
        let s = |v: &Value, p: &str| v.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        if agent == AiAgent::OpenCode && self.session.is_none() {
            self.session = s(v, "/sessionID");
        }
        match (agent, ty) {
            (AiAgent::Codex, "thread.started") => self.session = s(v, "/thread_id"),
            (AiAgent::Codex, "item.started" | "item.completed") if matches!(v.pointer("/item/type").and_then(Value::as_str), Some("command_execution" | "file_change")) => {
                return self.codex_item(&v["item"], ty == "item.completed");
            }
            (AiAgent::Codex, "item.completed") => {
                let message = matches!(v.pointer("/item/type").and_then(Value::as_str), Some("agent_message" | "assistant_message"));
                if let Some(t) = s(v, "/item/text").filter(|_| message) {
                    return self.paragraph(t);
                }
            }
            (AiAgent::Codex, "turn.completed") => {
                // `input_tokens` counts the cached ones too.
                self.context = tokens(v, &["/usage/input_tokens", "/usage/output_tokens"]);
                return self.finish(None);
            }
            (AiAgent::Codex, "turn.failed") => {
                let e = s(v, "/error/message").or_else(|| self.error.clone()).unwrap_or_else(|| "the reply failed".into());
                return self.finish(Some(e));
            }
            (AiAgent::Codex, "error") => self.error = s(v, "/message"),
            (AiAgent::Grok, "text") => {
                if let Some(t) = s(v, "/data") {
                    self.wrote = true;
                    return vec![AiEvent::Text(t)];
                }
            }
            (AiAgent::Grok, "end") => {
                if let Some(id) = s(v, "/sessionId") {
                    self.session = Some(id);
                }
                return self.finish(None);
            }
            (AiAgent::Grok, "error") => self.error = s(v, "/message"),
            (AiAgent::OpenCode, "step_start") => self.step = Some((Instant::now(), None)),
            (AiAgent::OpenCode, "tool_use") => {
                self.part_time(v);
                return self.opencode_tool(&v["part"]);
            }
            (AiAgent::OpenCode, "text") => {
                self.part_time(v);
                if let Some(t) = s(v, "/part/text") {
                    return self.paragraph(t);
                }
            }
            (AiAgent::OpenCode, "step_finish") => {
                self.step_done(v);
                // A step that ends in tool calls is followed by another; any other ending ends
                // the turn.
                if s(v, "/part/reason").as_deref() != Some("tool-calls") {
                    return self.finish(None);
                }
            }
            (AiAgent::OpenCode, _) if v.pointer("/part/time").is_some() => self.part_time(v),
            (AiAgent::OpenCode, "error") => {
                self.error = s(v, "/error/data/message").or_else(|| s(v, "/error/name"));
            }
            _ => {}
        }
        vec![]
    }

    /// A Codex command or file change as steps. It reports a command when it starts and again
    /// when it ends; a file change only once done, without the diff.
    fn codex_item(&mut self, item: &Value, completed: bool) -> Vec<AiEvent> {
        let s = |k: &str| item.get(k).and_then(Value::as_str).map(str::to_string);
        let Some(id) = s("id") else { return vec![] };
        let mut out = Vec::new();
        if item.get("type").and_then(Value::as_str) == Some("file_change") {
            let ok = s("status").as_deref() != Some("failed");
            for (i, c) in item.get("changes").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let path = c.get("path").and_then(Value::as_str).unwrap_or_default().to_string();
                let kind = c.get("kind").and_then(Value::as_str).unwrap_or("update").to_string();
                let step = format!("{id}:{i}");
                out.push(AiEvent::Step(AiStep { id: step.clone(), parent: None, kind: AiStepKind::Edit { path, old: String::new(), new: String::new() } }));
                out.push(AiEvent::StepDone(AiStepDone { id: step, ok, output: kind, line: None }));
            }
            return out;
        }
        if self.started.insert(id.clone()) {
            let kind = AiStepKind::Command { command: s("command").unwrap_or_default(), description: None, timeout_ms: None, background: false };
            out.push(AiEvent::Step(AiStep { id: id.clone(), parent: None, kind }));
        }
        if completed {
            let ok = item.get("exit_code").and_then(Value::as_i64).map_or(s("status").as_deref() == Some("completed"), |c| c == 0);
            out.push(AiEvent::StepDone(AiStepDone { id, ok, output: clip(s("aggregated_output").unwrap_or_default()), line: None }));
        }
        out
    }

    /// An OpenCode tool call, reported once it is over: the step and its end together.
    fn opencode_tool(&mut self, part: &Value) -> Vec<AiEvent> {
        let Some((step, done)) = opencode_step(part, || {
            self.unnamed += 1;
            format!("step-{}", self.unnamed)
        }) else {
            return vec![];
        };
        vec![AiEvent::Step(step), AiEvent::StepDone(done)]
    }

    /// An OpenCode part (text, reasoning, a tool call) says when it was written: the step's
    /// writing spans all of them.
    fn part_time(&mut self, v: &Value) {
        let (Some(a), Some(b)) = (v.pointer("/part/time/start").and_then(Value::as_i64), v.pointer("/part/time/end").and_then(Value::as_i64)) else {
            return;
        };
        let step = self.step.get_or_insert((Instant::now(), None));
        step.1 = Some(step.1.map_or((a, b), |(s, e)| (s.min(a), e.max(b))));
    }

    /// An OpenCode step's tokens and cost: what the context holds, and how fast it wrote.
    fn step_done(&mut self, v: &Value) {
        let n = |p: &str| v.pointer(p).and_then(Value::as_u64).unwrap_or(0);
        let out = n("/part/tokens/output") + n("/part/tokens/reasoning");
        let prompt = tokens(v, &["/part/tokens/input", "/part/tokens/cache/read", "/part/tokens/cache/write"]);
        if let Some(p) = prompt {
            self.context = Some(p + n("/part/tokens/output"));
        }
        self.cost += v.pointer("/part/cost").and_then(Value::as_f64).unwrap_or(0.0);
        // The parts' own times are the writing alone; without them, the whole step (which also
        // reads the prompt) will do.
        let ms = match self.step.take() {
            Some((_, Some((a, b)))) if b > a => (b - a) as u64,
            Some((started, _)) => started.elapsed().as_millis() as u64,
            None => 0,
        };
        if out > 0 && ms > 0 {
            self.written.0 += out;
            self.written.1 += ms;
        }
    }

    fn tokens_per_sec(&self) -> Option<f64> {
        let (n, ms) = self.written;
        (ms > 0).then(|| n as f64 * 1000.0 / ms as f64)
    }

    /// A whole block of text, set apart from the one before it.
    fn paragraph(&mut self, t: String) -> Vec<AiEvent> {
        let mut out = Vec::new();
        if self.wrote {
            out.push(AiEvent::Text("\n\n".into()));
        }
        self.wrote = true;
        out.push(AiEvent::Text(t));
        out
    }

    fn finish(&mut self, error: Option<String>) -> Vec<AiEvent> {
        if self.done {
            return vec![];
        }
        self.done = true;
        vec![AiEvent::TurnDone { error }]
    }

    /// The run is over: a turn it didn't finish ends with the best reason there is.
    fn exited(&mut self, agent: AiAgent, code: Option<i32>, stderr: String) -> Vec<AiEvent> {
        let why = self
            .error
            .clone()
            .or_else(|| Some(stderr).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| match code {
                Some(c) => format!("{} exited with code {c}", agent.name()),
                None => format!("{} was stopped", agent.name()),
            });
        self.finish(Some(why))
    }
}

impl PerTurn {
    /// Answer one message: run the assistant, resuming the chat so far. After a compaction it
    /// starts a new session with the summary in front.
    fn send(self: &Arc<Self>, text: &str, images: &[AiImage]) {
        let images = image_files(images);
        match self.carry.lock().unwrap().take() {
            Some(summary) => self.run(&format!("{CARRY_HEAD}\n\n{summary}\n\n---\n\n{text}"), &images, Run::Chat),
            None => self.run(text, &images, Run::Chat),
        }
    }

    /// These assistants can't compact a session they resume, so the chat is asked for a summary,
    /// and the next message starts a fresh session with it (`send`).
    fn compact(self: &Arc<Self>, focus: Option<&str>) {
        let Some(_) = self.resume.lock().unwrap().as_ref() else {
            // Nothing said yet, or already compacted: nothing to do.
            let _ = self.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id: self.id, event: AiEvent::TurnDone { error: None } }));
            return;
        };
        let prompt = match focus.map(str::trim).filter(|f| !f.is_empty()) {
            Some(f) => format!("{COMPACT_PROMPT} Focus on: {f}"),
            None => COMPACT_PROMPT.to_string(),
        };
        self.run(&prompt, &[], Run::Compact);
    }

    fn run(self: &Arc<Self>, text: &str, images: &[PathBuf], purpose: Run) {
        // A chat only reads; so does the run that writes a compaction's summary.
        let mode = if self.tools && purpose == Run::Chat { *self.mode.lock().unwrap() } else { AiMode::Plan };
        if self.agent == AiAgent::Codex {
            self.run_codex(text, images, purpose, mode);
            return;
        }
        let send = |event| {
            let _ = self.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id: self.id, event }));
        };
        let resume = self.resume.lock().unwrap().clone();
        let mut cmd = Command::new(&self.exe);
        if self.agent == AiAgent::OpenCode {
            let mut config = opencode_config(opencode_permissions(mode), self.model.as_deref().unwrap_or(""), &ollama::GpuSel::All);
            if let Some(f) = &self.prompt_file {
                let mut c: Value = serde_json::from_str(&config).unwrap_or_default();
                c["instructions"] = serde_json::json!([f]);
                config = c.to_string();
            }
            // An agent that may work may also work in its own work tree (`nib-work start`),
            // which is outside the project: the one folder out there it is let into.
            if let (true, Some(trees)) = (matches!(mode, AiMode::AcceptEdits | AiMode::Auto), work_trees(&self.env)) {
                let mut c: Value = serde_json::from_str(&config).unwrap_or_default();
                let trees = trees.to_string_lossy().into_owned();
                // The last rule that matches decides (the keys are kept sorted: `*` comes first).
                c["permission"]["external_directory"] = serde_json::json!({ "*": "deny", format!("{trees}/*"): "allow", trees: "allow" });
                config = c.to_string();
            }
            cmd.env("OPENCODE_CONFIG_CONTENT", config);
            if self.local {
                opencode_offline(&mut cmd);
            }
        }
        // Grok takes no system prompt: it heads the first message instead.
        let headed;
        let text = match (&self.tuning.prompt, self.agent, &resume) {
            (Some(p), AiAgent::Grok, None) => {
                headed = format!("{p}\n\n{text}");
                headed.as_str()
            }
            _ => text,
        };
        let args = one_shot_args(self.agent, self.model.as_deref(), resume.as_deref(), mode, images, text);
        if self.agent == AiAgent::OpenCode && purpose == Run::Chat {
            // It tells of a command only once it is over: what the whole run changes in the
            // shared work tree is its (nothing is watched for an agent on its own branch).
            self.watch.begins(self.id, RUN);
        }
        // An agent on its own branch stays a process of the project folder: OpenCode keeps a
        // session to the folder it began in, so it reaches its work tree by path (it is let
        // into that folder: see the `external_directory` rule, and Codex's writable roots).
        let dir = &self.root;
        let spawned = cmd
            .envs(self.env.iter().map(|(k, v)| (*k, v.as_os_str())))
            // OpenCode takes its project folder from `$PWD`, not from the folder it is started
            // in: left as nibd inherited it, the agent would work wherever nib was started from.
            .env("PWD", dir)
            .args(args)
            .current_dir(dir)
            // One-shot agents receive their prompt as an argument.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => return send(AiEvent::TurnDone { error: Some(format!("starting {}: {e}", self.exe.display())) }),
        };
        let stdout = child.stdout.take().expect("piped");
        let errors = stderr_tail(child.stderr.take().expect("piped"));
        *self.child.lock().unwrap() = Some(child);
        let me = Arc::clone(self);
        thread::spawn(move || {
            let mut shot = OneShot::default();
            let send = |events: Vec<AiEvent>| {
                events.into_iter().all(|event| {
                    me.watch.see(me.id, &event);
                    me.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id: me.id, event })).is_ok()
                })
            };
            // A compaction's summary, kept instead of shown; its turn ends once it's in place.
            let mut summary = String::new();
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let mut events = shot.read(me.agent, &v);
                if purpose == Run::Compact {
                    events.retain(|e| match e {
                        AiEvent::Text(t) => {
                            summary.push_str(t);
                            false
                        }
                        e => !matches!(e, AiEvent::TurnDone { .. }),
                    });
                }
                if !send(events) {
                    return;
                }
            }
            let tail = errors.join().unwrap_or_default();
            // Closed meanwhile: `close` took the child and stopped it.
            let code = me.child.lock().unwrap().take().and_then(|mut c| c.wait().ok()).and_then(|s| s.code());
            if let Some(id) = shot.session.take() {
                if purpose == Run::Chat {
                    me.watch.rec.lock().unwrap().session(&id);
                }
                *me.resume.lock().unwrap() = Some(id);
            }
            if purpose == Run::Compact {
                let summary = summary.trim();
                let error = (summary.is_empty() || code != Some(0)).then(|| {
                    shot.error.clone().or(Some(tail).filter(|t| !t.is_empty())).unwrap_or_else(|| format!("{} gave no summary", me.agent.name()))
                });
                let mut events = Vec::new();
                if error.is_none() {
                    // A new session from here: the old one's record no longer describes the chat.
                    *me.resume.lock().unwrap() = None;
                    *me.rollout.lock().unwrap() = None;
                    *me.carry.lock().unwrap() = Some(summary.to_string());
                    // The summary run read the whole chat: its prompt is what the context held.
                    // What it holds after shows with the next reply.
                    events.push(AiEvent::Compacted { before: shot.context, after: None });
                }
                events.push(AiEvent::TurnDone { error });
                send(events);
                return;
            }
            let usage = me.usage(&shot);
            let mut events = shot.exited(me.agent, code, tail);
            events.extend(usage.map(AiEvent::Usage));
            send(events);
            // Letters that came while it ran are its next message.
            let letters = std::mem::take(&mut *me.mail.lock().unwrap());
            if !letters.is_empty() {
                me.watch.busy.store(true, Ordering::Relaxed);
                me.send(&letters.join("\n\n"), &[]);
            }
        });
    }

    /// What the chat uses after a run: from the run itself, and for Codex its session record
    /// (written by then), which also has the context window and the account's limits. `None`:
    /// nothing to tell (Grok says nothing).
    fn usage(&self, shot: &OneShot) -> Option<AiUsage> {
        match self.agent {
            AiAgent::Codex => {
                let thread = self.resume.lock().unwrap().clone()?;
                let mut found = self.rollout.lock().unwrap();
                if found.is_none() {
                    *found = codex_rollout(&thread);
                }
                let from_record = found.as_deref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| codex_usage(&t));
                from_record.or_else(|| shot.context.map(|n| AiUsage { context_tokens: Some(n), ..Default::default() }))
            }
            AiAgent::OpenCode => {
                let mut cost = self.cost.lock().unwrap();
                *cost += shot.cost;
                let model = self.model.as_deref().map(ollama::name_of);
                Some(AiUsage {
                    context_tokens: shot.context,
                    // Ollama's own, for the model as loaded: what it really keeps.
                    context_window: model.filter(|_| self.local).and_then(|m| ollama::context_length(&m)),
                    limits: Vec::new(),
                    cost_usd: (!self.local && *cost > 0.0).then_some(*cost),
                    tokens_per_sec: if self.local { shot.tokens_per_sec() } else { None },
                })
            }
            AiAgent::Grok | AiAgent::Claude => None,
        }
    }

    fn stop(&self) {
        self.codex_questions.lock().unwrap().clear();
        self.codex_input.lock().unwrap().take();
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn codex_rpc(input: &mut ChildStdin, value: &Value) -> std::io::Result<()> {
    writeln!(input, "{value}")?;
    input.flush()
}

/// App-server's item names are camelCase; the existing Codex event reader uses exec's snake_case.
fn codex_app_event(shot: &mut OneShot, method: &str, params: &Value) -> Vec<AiEvent> {
    match method {
        "item/started" | "item/completed" => {
            let item = &params["item"];
            let ty = item["type"].as_str().unwrap_or("");
            if method == "item/started" && ty != "commandExecution" { return Vec::new() }
            let translated = match ty {
                "commandExecution" => serde_json::json!({
                    "id": item["id"], "type": "command_execution", "command": item["command"],
                    "aggregated_output": item["aggregatedOutput"], "exit_code": item["exitCode"], "status": item["status"],
                }),
                "fileChange" => serde_json::json!({ "id": item["id"], "type": "file_change", "changes": item["changes"], "status": item["status"] }),
                "agentMessage" => serde_json::json!({ "id": item["id"], "type": "agent_message", "text": item["text"] }),
                _ => return Vec::new(),
            };
            shot.read(AiAgent::Codex, &serde_json::json!({ "type": if method == "item/started" { "item.started" } else { "item.completed" }, "item": translated }))
        }
        "turn/completed" => {
            let turn = &params["turn"];
            let failed = turn["status"].as_str() != Some("completed");
            let error = turn.pointer("/error/message").and_then(Value::as_str).unwrap_or("the reply was interrupted");
            shot.finish(failed.then(|| error.to_string()))
        }
        "error" => shot.read(AiAgent::Codex, &serde_json::json!({ "type": "error", "message": params["message"] })),
        _ => Vec::new(),
    }
}

fn codex_question(request: &Value) -> Option<(AiAsk, Vec<(String, Vec<String>)>)> {
    let id = request.get("id")?;
    let key = format!("codex-{id}");
    let params = request.get("params")?;
    let mut ids = Vec::new();
    let questions = params.get("questions")?.as_array()?.iter().filter_map(|q| {
        let question = q.get("question")?.as_str()?;
        let question_id = q.get("id")?.as_str()?.to_string();
        let options: Vec<AiOption> = q.get("options").and_then(Value::as_array).into_iter().flatten().filter_map(|o| Some(AiOption {
            label: o.get("label")?.as_str()?.to_string(),
            description: o.get("description").and_then(Value::as_str).unwrap_or("").to_string(), preview: None,
        })).collect();
        ids.push((question_id, options.iter().map(|o| o.label.clone()).collect()));
        Some(AiQuestion { header: q.get("header").and_then(Value::as_str).unwrap_or("").to_string(), question: question.to_string(), options, multi: false })
    }).collect::<Vec<_>>();
    if questions.is_empty() { return None }
    Some((AiAsk {
        request: key, step: params.get("itemId").and_then(Value::as_str).map(str::to_string),
        tool: "AskUserQuestion".into(), what: String::new(), questions,
    }, ids))
}

fn codex_answers(questions: Vec<(String, Vec<String>)>, allow: bool, answers: &[String]) -> Value {
    let values: serde_json::Map<String, Value> = questions.into_iter().zip(answers).filter_map(|((id, options), answer)| {
        if !allow || answer.trim().is_empty() { return None }
        let answer = if options.iter().any(|o| o == answer) { answer.clone() } else { format!("user_note: {answer}") };
        Some((id, serde_json::json!({ "answers": [answer] })))
    }).collect();
    serde_json::json!({ "answers": values })
}

impl PerTurn {
    /// One Codex turn on app-server. A fresh process per turn preserves nib's existing lifecycle
    /// while allowing a server request to pause the turn until nib answers it.
    fn run_codex(self: &Arc<Self>, text: &str, images: &[PathBuf], purpose: Run, mode: AiMode) {
        let mut cmd = Command::new(&self.exe);
        cmd.args(["app-server", "--stdio", "--enable", "default_mode_request_user_input"])
            .current_dir(&self.root)
            .envs(self.env.iter().map(|(k, v)| (*k, v.as_os_str())))
            .env("PWD", &self.root)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = self.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id: self.id, event: AiEvent::TurnDone { error: Some(format!("starting Codex app-server: {e}")) } }));
                return;
            }
        };
        let stdout = child.stdout.take().expect("piped");
        let errors = stderr_tail(child.stderr.take().expect("piped"));
        let mut input = child.stdin.take().expect("piped");
        let init = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "clientInfo": { "name": "nibble", "title": "nibble", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true }
        }});
        if let Err(e) = codex_rpc(&mut input, &init) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = self.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id: self.id, event: AiEvent::TurnDone { error: Some(format!("initializing Codex app-server: {e}")) } }));
            return;
        }
        *self.codex_input.lock().unwrap() = Some(input);
        *self.child.lock().unwrap() = Some(child);
        let me = Arc::clone(self);
        let prompt = text.to_string();
        let pictures = images.to_vec();
        thread::spawn(move || {
            let send = |event: AiEvent| {
                me.watch.see(me.id, &event);
                me.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id: me.id, event })).is_ok()
            };
            let mut shot = OneShot::default();
            let mut summary = String::new();
            let mut failure = None;
            let mut context_window = None;
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if v.get("method").and_then(Value::as_str) == Some("item/tool/requestUserInput") {
                    if let Some((ask, ids)) = codex_question(&v) {
                        me.codex_questions.lock().unwrap().insert(ask.request.clone(), (v["id"].clone(), ids));
                        let _ = send(AiEvent::Ask(ask));
                    }
                    continue;
                }
                if let Some(id) = v.get("id") {
                    if let Some(error) = v.pointer("/error/message").and_then(Value::as_str) {
                        failure = Some(error.to_string());
                        break;
                    }
                    match id.as_i64() {
                        Some(1) => {
                            let mut stdin = me.codex_input.lock().unwrap();
                            let Some(stdin) = stdin.as_mut() else { break };
                            let _ = codex_rpc(stdin, &serde_json::json!({ "jsonrpc": "2.0", "method": "initialized" }));
                            let acts = matches!(mode, AiMode::AcceptEdits | AiMode::Auto);
                            let roots = if acts { codex_roots(&me.root, &me.env) } else { Vec::new() };
                            let config = if roots.is_empty() { Value::Null } else { serde_json::json!({ "sandbox_workspace_write": { "writable_roots": roots } }) };
                            let mut instructions = me.tuning.prompt.clone().unwrap_or_default();
                            if me.tools {
                                let catalog = codex_skill_catalog(&bundled_skills());
                                if !catalog.is_empty() { if !instructions.is_empty() { instructions.push_str("\n\n"); } instructions.push_str(&catalog); }
                            }
                            let resume_id = me.resume.lock().unwrap().clone();
                            let mut params = serde_json::json!({
                                "cwd": me.root,
                                "model": me.model, "sandbox": if acts { "workspace-write" } else { "read-only" },
                                "approvalPolicy": "never", "developerInstructions": if instructions.is_empty() { None } else { Some(instructions) },
                                "config": config,
                            });
                            let method = if let Some(id) = resume_id { params["threadId"] = Value::String(id); "thread/resume" } else { "thread/start" };
                            let _ = codex_rpc(stdin, &serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": method, "params": params }));
                        }
                        Some(2) => {
                            let Some(thread_id) = v.pointer("/result/thread/id").and_then(Value::as_str) else { failure = Some("Codex did not return a thread ID".into()); break };
                            if purpose == Run::Chat { me.watch.rec.lock().unwrap().session(thread_id); }
                            *me.resume.lock().unwrap() = Some(thread_id.to_string());
                            let Some(model) = v.pointer("/result/model").and_then(Value::as_str).or(me.model.as_deref()) else {
                                failure = Some("Codex did not return its model".into());
                                break;
                            };
                            let mut items: Vec<Value> = pictures.iter().map(|p| serde_json::json!({ "type": "localImage", "path": p })).collect();
                            items.push(serde_json::json!({ "type": "text", "text": prompt }));
                            let mut params = serde_json::json!({ "threadId": thread_id, "input": items, "model": me.model, "effort": me.tuning.effort });
                            if mode == AiMode::Plan {
                                params["collaborationMode"] = serde_json::json!({ "mode": "plan", "settings": { "model": model, "reasoning_effort": me.tuning.effort } });
                            }
                            if let Some(stdin) = me.codex_input.lock().unwrap().as_mut() {
                                let _ = codex_rpc(stdin, &serde_json::json!({ "jsonrpc": "2.0", "id": 3, "method": "turn/start", "params": params }));
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                let method = v.get("method").and_then(Value::as_str).unwrap_or("");
                if method == "serverRequest/resolved" {
                    let request = format!("codex-{}", v["params"]["requestId"]);
                    if me.codex_questions.lock().unwrap().remove(&request).is_some() {
                        let _ = send(AiEvent::AskResolved { request });
                    }
                    continue;
                }
                if method == "thread/tokenUsage/updated" {
                    shot.context = v.pointer("/params/tokenUsage/last/totalTokens").and_then(Value::as_u64);
                    context_window = v.pointer("/params/tokenUsage/modelContextWindow").and_then(Value::as_u64);
                }
                let mut events = codex_app_event(&mut shot, method, &v["params"]);
                if purpose == Run::Compact {
                    events.retain(|e| match e {
                        AiEvent::Text(t) => { summary.push_str(t); false },
                        e => !matches!(e, AiEvent::TurnDone { .. }),
                    });
                }
                if !events.into_iter().all(&send) { return }
                if method == "turn/completed" { break }
            }
            me.codex_input.lock().unwrap().take();
            me.codex_questions.lock().unwrap().clear();
            let code = me.child.lock().unwrap().take().and_then(|mut c| { let _ = c.kill(); c.wait().ok() }).and_then(|s| s.code());
            let tail = errors.join().unwrap_or_default();
            if purpose == Run::Compact {
                let summary = summary.trim();
                let error = (summary.is_empty() || failure.is_some()).then(|| failure.unwrap_or_else(|| format!("Codex gave no summary: {tail}")));
                if error.is_none() {
                    *me.resume.lock().unwrap() = None;
                    *me.rollout.lock().unwrap() = None;
                    *me.carry.lock().unwrap() = Some(summary.to_string());
                    let _ = send(AiEvent::Compacted { before: shot.context, after: None });
                }
                let _ = send(AiEvent::TurnDone { error });
                return;
            }
            let usage = me.usage(&shot).or_else(|| (shot.context.is_some() || context_window.is_some()).then(|| AiUsage { context_tokens: shot.context, context_window, ..Default::default() }));
            if let Some(error) = failure { let _ = send(AiEvent::TurnDone { error: Some(error) }); }
            else { for event in shot.exited(AiAgent::Codex, code, tail) { let _ = send(event); } }
            if let Some(usage) = usage { let _ = send(AiEvent::Usage(usage)); }
            let letters = std::mem::take(&mut *me.mail.lock().unwrap());
            if !letters.is_empty() { me.watch.busy.store(true, Ordering::Relaxed); me.send(&letters.join("\n\n"), &[]); }
        });
    }

    fn answer_codex(&self, request: &str, allow: bool, answers: &[String]) {
        let Some((id, questions)) = self.codex_questions.lock().unwrap().remove(request) else { return };
        if let Some(stdin) = self.codex_input.lock().unwrap().as_mut() {
            let _ = codex_rpc(stdin, &serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": codex_answers(questions, allow, answers) }));
        }
    }
}


/// Codex's record of thread `id`: `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl` under its
/// home, in one of the last days' folders.
fn codex_rollout(id: &str) -> Option<PathBuf> {
    let root = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| home().map(|h| h.join(".codex")))?.join("sessions");
    let newest_first = |p: &Path| {
        let mut v: Vec<PathBuf> = std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        v.sort();
        v.reverse();
        v
    };
    let days = newest_first(&root).into_iter().flat_map(|y| newest_first(&y)).flat_map(|m| newest_first(&m)).take(3);
    let suffix = format!("-{id}.jsonl");
    days.flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()))
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&suffix)))
}

/// The last `token_count` in a Codex session record: the last request's tokens, the model's
/// context window and the account's limit windows.
fn codex_usage(record: &str) -> Option<AiUsage> {
    let v = record.lines().rev().find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        (v.pointer("/payload/type").and_then(Value::as_str) == Some("token_count") && v.pointer("/payload/info").is_some_and(|i| !i.is_null())).then_some(v)
    })?;
    let p = &v["payload"];
    let mut limits: Vec<AiLimit> = ["primary", "secondary"]
        .iter()
        .filter_map(|k| {
            let w = p.pointer(&format!("/rate_limits/{k}")).filter(|w| w.is_object())?;
            let resets_at = w.get("resets_at").and_then(Value::as_i64).or_else(|| {
                let at = v.get("timestamp").and_then(Value::as_str).and_then(unix_secs)?;
                Some(at + w.get("resets_in_seconds")?.as_i64()?)
            });
            Some(AiLimit {
                window_minutes: w.get("window_minutes")?.as_u64()? as u32,
                used: (w.get("used_percent")?.as_f64()? / 100.0) as f32,
                resets_at,
            })
        })
        .collect();
    limits.sort_by_key(|l| l.window_minutes);
    Some(AiUsage {
        context_tokens: p.pointer("/info/last_token_usage/total_tokens").and_then(Value::as_u64),
        context_window: p.pointer("/info/model_context_window").and_then(Value::as_u64),
        limits,
        ..Default::default()
    })
}

/// `2026-09-15T13:49:31.532Z` in Unix seconds (UTC only, which is what Codex writes).
fn unix_secs(t: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| t.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, s) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y, mo) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * mo + 2) / 5 + d - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + s)
}

pub fn send(sessions: &Shared, id: AiId, text: &str, images: &[AiImage]) {
    let mut s = sessions.lock().unwrap();
    // An agent's first message is its task. With other agents at work, it is told about them
    // (the others are not told about it: it writes to the ones its work touches, D-062), and
    // how to say when the job is done.
    let mut text = text.to_string();
    if let Some(m) = s.meta.get(&id) {
        m.rec.lock().unwrap().user(&text);
    }
    let first = s.meta.get(&id).is_some_and(|m| m.task.is_empty());
    // Its name from then on says what it is doing (two with the same task get a number).
    let taken: Vec<String> = s.meta.iter().filter(|(other, _)| **other != id).map(|(_, m)| m.name.clone()).collect();
    if let Some(m) = s.meta.get_mut(&id) {
        m.busy.store(true, Ordering::Relaxed);
        if first {
            // The whole message, not its first line: that may be only `three things:`.
            m.task = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(140).collect();
            if let Some(base) = mail::task_name(&m.task) {
                m.name = (1..).map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") }).find(|n| !taken.contains(n)).unwrap_or(base);
                let named = AiEvent::Name(m.name.clone());
                m.rec.lock().unwrap().event(&named);
                let _ = m.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id, event: named }));
            }
        }
    }
    if let (true, Some(me)) = (first, s.meta.get(&id)) {
        let others: Vec<&Meta> = s.meta.iter().filter(|(other, m)| **other != id && !m.task.is_empty()).map(|(_, m)| m).collect();
        if !others.is_empty() {
            let list: Vec<String> = others.iter().map(|m| format!("{} ({}): {}", m.name, if m.busy.load(Ordering::Relaxed) { "working" } else { "idle" }, m.task)).collect();
            let how = if s.wpf_off {
                "share this working tree right now:\n{list}\nIf you will change files, first run `nib-mail who` (it lists the files each agent changed) and write only to the agents whose files or task overlap yours, by name (nib-agent-mailbox skill). If none do, write to nobody."
            } else {
                "are at work in this project right now:\n{list}\nEach changes files on a branch of its own (nib-work), so you do not share files with them. If your task may overlap theirs, run `nib-mail who` and write only to those agents, by name (nib-agent-mailbox skill)."
            };
            text.push_str(&format!("\n\n(nib: you are {}. Other agents {})", me.name, how.replace("{list}", &list.join("\n"))));
        }
        if s.mail.is_some() {
            text.push_str(finish_hint(!s.wpf_off));
        }
    }
    // Letters that waited in its inbox while it was idle come with it.
    if let (Some(dir), Some(m)) = (&s.mail, s.meta.get(&id)) {
        let waited = mail::said(&mail::unread(dir, &m.id_name, None));
        if !waited.is_empty() {
            text.push_str(&format!("\n\n(nib: mail that came while you were idle)\n{waited}"));
        }
    }
    if let Some(context) = s.carry.remove(&id) {
        if let Some(m) = s.meta.get(&id) { m.rec.lock().unwrap().context_delivered(); }
        text = format!("{context}\n\n---\nCurrent user message:\n{text}");
    }
    let text = text.as_str();
    match s.map.get_mut(&id) {
        Some(Session::Stream { stdin, .. }) => {
            // Pictures go in front of the text they come with.
            let content = if images.is_empty() {
                Value::from(text)
            } else {
                let mut blocks: Vec<Value> = images
                    .iter()
                    .map(|i| serde_json::json!({ "type": "image", "source": { "type": "base64", "media_type": i.media_type, "data": base64(&i.data) } }))
                    .collect();
                blocks.push(serde_json::json!({ "type": "text", "text": text }));
                Value::from(blocks)
            };
            let msg = serde_json::json!({ "type": "user", "message": { "role": "user", "content": content } });
            let _ = writeln!(stdin, "{msg}").and_then(|_| stdin.flush());
        }
        Some(Session::PerTurn(p)) => {
            let p = Arc::clone(p);
            drop(s);
            p.send(text, images);
        }
        None => {}
    }
}

/// Replace the chat so far with a summary of it, focused on `focus`. Claude Code compacts its
/// own session (`/compact`); the others are asked for a summary that starts the next session.
pub fn compact(sessions: &Shared, id: AiId, focus: Option<&str>) {
    let mut s = sessions.lock().unwrap();
    match s.map.get_mut(&id) {
        Some(Session::Stream { stdin, .. }) => {
            let cmd = match focus.map(str::trim).filter(|f| !f.is_empty()) {
                Some(f) => format!("/compact {f}"),
                None => "/compact".to_string(),
            };
            let msg = serde_json::json!({ "type": "user", "message": { "role": "user", "content": cmd } });
            let _ = writeln!(stdin, "{msg}").and_then(|_| stdin.flush());
        }
        Some(Session::PerTurn(p)) => {
            let p = Arc::clone(p);
            drop(s);
            p.compact(focus);
        }
        None => {}
    }
}

/// The user's answer to what the agent asked leave for: yes (the call goes ahead as it was), or
/// no, with what to do instead.
pub fn answer(sessions: &Shared, id: AiId, request: &str, allow: bool, message: Option<&str>, answers: &[String]) {
    let mut s = sessions.lock().unwrap();
    if let Some(Session::PerTurn(p)) = s.map.get(&id) {
        if p.agent == AiAgent::Codex {
            p.answer_codex(request, allow, answers);
        }
        return;
    }
    let Some(Session::Stream { stdin, asks, .. }) = s.map.get_mut(&id) else { return };
    let Some(input) = asks.lock().unwrap().remove(request) else { return };
    let response = if allow {
        serde_json::json!({ "behavior": "allow", "updatedInput": with_answers(input, answers) })
    } else {
        serde_json::json!({ "behavior": "deny", "message": message.filter(|m| !m.trim().is_empty()).unwrap_or("The user refused this.") })
    };
    let msg = serde_json::json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request, "response": response } });
    let _ = writeln!(stdin, "{msg}").and_then(|_| stdin.flush());
}

/// An `AskUserQuestion` call's input with the user's answers in it, the way Claude Code reads
/// them: by question text. Any other call's input as it was.
fn with_answers(mut input: Value, answers: &[String]) -> Value {
    if answers.is_empty() {
        return input;
    }
    let questions = input.get("questions").and_then(Value::as_array).into_iter().flatten();
    let by_question: serde_json::Map<String, Value> = questions
        .filter_map(|q| q.get("question")?.as_str())
        .zip(answers)
        .map(|(q, a)| (q.to_string(), Value::String(a.clone())))
        .collect();
    if let Some(o) = input.as_object_mut() {
        o.insert("answers".into(), Value::Object(by_question));
    }
    input
}

/// Worktree-per-feature on or off (D-080), for every agent: the hints they are given from now
/// on, and what `nib-mail done` and `nib-work guard` hold them to.
pub fn set_worktree_per_feature(sessions: &Shared, on: bool) {
    let mut s = sessions.lock().unwrap();
    s.wpf_off = !on;
    if let Some(dir) = &s.mail {
        mail::set_wpf(dir, on);
    }
}

/// What a new agent's first message is followed by: how to change files, and how to say it is
/// done, by whether worktree-per-feature is on.
fn finish_hint(wpf: bool) -> &'static str {
    if wpf {
        "\n\n(nib: worktree-per-feature is on. If this asks you to change files, follow the nib-work skill: plan first (when the request leaves choices open, hand the plan over in plan mode for the user to approve; when it is already clear, record it with `nib-work plan`), run `nib-work start SLUG`, and make every change in the work tree it prints, never in this project folder. When it is done and checked, run `nib-mail done` as the last thing you do (nib-finished skill): the user then lands your branch as one commit. A question or research with no change asked for: just answer.)"
    } else {
        "\n\n(nib: worktree-per-feature is off: change files here, in the project folder; nothing is committed for you, and accepting your work commits nothing. If this is a clear job with an end, then when you have done it and checked it, run `nib-mail done` as the last thing you do (nib-finished skill), so the user sees it is finished.)"
    }
}

/// Change the agent's mode. Claude Code switches at once and confirms (`AiEvent::Mode`); the
/// others run their next message in it.
pub fn set_mode(sessions: &Shared, id: AiId, mode: AiMode) {
    let mut s = sessions.lock().unwrap();
    match s.map.get_mut(&id) {
        Some(Session::Stream { stdin, .. }) => {
            let msg = serde_json::json!({ "type": "control_request", "request_id": format!("nib-mode-{id}"), "request": { "subtype": "set_permission_mode", "mode": claude_mode(mode) } });
            let _ = writeln!(stdin, "{msg}").and_then(|_| stdin.flush());
        }
        Some(Session::PerTurn(p)) => {
            *p.mode.lock().unwrap() = mode;
            let _ = p.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id, event: AiEvent::Mode(mode) }));
        }
        None => {}
    }
}

/// Stop the reply under way; the session stays. Claude Code is asked to (it kills the running
/// command and ends the turn); the others' run is killed.
pub fn interrupt(sessions: &Shared, id: AiId) {
    let mut s = sessions.lock().unwrap();
    match s.map.get_mut(&id) {
        Some(Session::Stream { stdin, .. }) => {
            let msg = serde_json::json!({ "type": "control_request", "request_id": format!("nib-{id}"), "request": { "subtype": "interrupt" } });
            let _ = writeln!(stdin, "{msg}").and_then(|_| stdin.flush());
        }
        Some(Session::PerTurn(p)) => {
            let p = Arc::clone(p);
            drop(s);
            p.stop();
        }
        None => {}
    }
}

/// The session's top process: the assistant, or the run answering now.
fn pid(session: &Session) -> Option<u32> {
    match session {
        Session::Stream { child, .. } => Some(child.id()),
        Session::PerTurn(p) => p.child.lock().unwrap().as_ref().map(Child::id),
    }
}

/// `pid` and everything it started, parents first, from `ps`'s `pid ppid` lines.
#[cfg_attr(not(unix), allow(dead_code))]
fn descendants(pid: u32, table: &str) -> Vec<u32> {
    let pairs: Vec<(u32, u32)> = table
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect();
    let mut out = vec![pid];
    let mut at = 0;
    while at < out.len() {
        let parent = out[at];
        let children: Vec<u32> = pairs.iter().filter(|(c, p)| *p == parent && !out.contains(c)).map(|(c, _)| *c).collect();
        out.extend(children);
        at += 1;
    }
    out
}

/// Send `signal` (`STOP` / `CONT`) to `pid` and everything under it.
#[cfg(unix)]
fn signal_tree(pid: u32, signal: &str) -> Result<(), String> {
    let mut ps = Command::new("ps");
    ps.args(["-axo", "pid=,ppid="]);
    let table = run_for(ps, Duration::from_secs(5)).ok_or("couldn't list processes (ps)")?;
    let mut pids = descendants(pid, &table);
    // Freeze the parent first so it can't start more; thaw it last so its children are ready.
    if signal == "CONT" {
        pids.reverse();
    }
    let status = Command::new("kill").arg(format!("-{signal}")).args(pids.iter().map(u32::to_string)).stderr(Stdio::null()).status();
    // A process that exited in between is fine.
    status.map(|_| ()).map_err(|e| format!("kill: {e}"))
}

#[cfg(not(unix))]
fn signal_tree(_pid: u32, _signal: &str) -> Result<(), String> {
    Err("this machine can't suspend processes".into())
}

/// Freeze (`on`) or thaw every process of the session. An assistant with no run under way has
/// nothing to freeze, which is fine.
pub fn suspend(sessions: &Shared, id: AiId, on: bool) -> Result<(), String> {
    let mut s = sessions.lock().unwrap();
    let Some(session) = s.map.get(&id) else { return Err("no such session".into()) };
    if let Some(pid) = pid(session) {
        signal_tree(pid, if on { "STOP" } else { "CONT" })?;
    }
    if on {
        s.frozen.insert(id);
    } else {
        s.frozen.remove(&id);
    }
    Ok(())
}

fn end(session: Session) {
    match session {
        Session::Stream { mut child, .. } => {
            let _ = child.kill();
            let _ = child.wait();
        }
        Session::PerTurn(p) => p.stop(),
    }
}

pub fn close(sessions: &Shared, id: AiId) {
    let (session, frozen, ended) = {
        let mut s = sessions.lock().unwrap();
        let meta = s.meta.remove(&id);
        s.carry.remove(&id);
        // The agent itself ends with its last session (a model switch handed its name on, so
        // there it goes on): then the work trees it made in its scratch go too.
        let ended = meta
            .filter(|m| !s.meta.values().any(|o| o.id_name == m.id_name))
            .and_then(|m| Some((s.root.clone()?, m.scratch?)));
        (s.map.remove(&id), s.frozen.remove(&id), ended)
    };
    if let Some(s) = session {
        // Killing a frozen assistant would leave the commands it started frozen for good.
        if let (true, Some(pid)) = (frozen, pid(&s)) {
            let _ = signal_tree(pid, "CONT");
        }
        end(s);
    }
    if let Some((root, dir)) = ended {
        thread::spawn(move || crate::git::remove_worktrees_under(&root, &dir));
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        for (id, s) in self.map.drain() {
            if let (true, Some(pid)) = (self.frozen.contains(&id), pid(&s)) {
                let _ = signal_tree(pid, "CONT");
            }
            end(s);
        }
        // The window closed: every agent ended, and with it the work trees they made.
        if let Some(root) = &self.root {
            for dir in self.meta.values().filter_map(|m| m.scratch.as_ref()) {
                crate::git::remove_worktrees_under(root, dir);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_open(shared: &Shared, replace: Option<AiId>, agent: AiAgent, model: &str, root: &Path, tx: &Sender<ServerMsg>) -> Result<AiId, RpcError> {
        open_ready(shared, replace, agent, Some(model.into()), Tuning::default(), true, AiMode::Auto, None, root, Dirs { scratch: Some(root), nibdir: Some(root) }, tx.clone(), root.join("missing-assistant"))
    }

    #[test]
    fn model_switch_transfers_native_context_work_and_review_without_an_old_model_call() {
        let root = std::env::temp_dir().join(format!("nibd-switch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let shared: Shared = Default::default();
        let (tx, _rx) = std::sync::mpsc::channel();
        let first = test_open(&shared, None, AiAgent::Codex, "first-model", &root, &tx).unwrap();
        let (name, history_id, mailbox) = {
            let mut s = shared.lock().unwrap();
            let m = s.meta.get_mut(&first).unwrap();
            m.task = "fix parser".into();
            m.name = "parser".into();
            let mut rec = m.rec.lock().unwrap();
            rec.user("Fix parser; retain escaped quotes");
            rec.session("codex-native");
            rec.event(&AiEvent::Text("Next: run parser tests.".into()));
            let name = m.id_name.clone();
            let history_id = rec.id();
            drop(rec);
            if let Session::PerTurn(p) = &s.map[&first] { *p.resume.lock().unwrap() = Some("codex-native".into()); }
            s.touched.entry(first).or_default().insert("parser.rs".into());
            s.edits.insert("parser.rs".into(), first);
            (name, history_id, s.mail.clone().unwrap())
        };
        work::set_plan(&mailbox, &name, "# Parser\nRun escaped quote tests").unwrap();
        let record = work::Record { branch: "nib/feat/parser".into(), base: "main".into(), tree: root.join("parser-tree"), dir: root.join("parser-tree") };
        std::fs::create_dir_all(&record.dir).unwrap();
        std::fs::write(mailbox.join("work").join(format!("{name}.json")), serde_json::to_vec(&record).unwrap()).unwrap();
        review_edit(&shared, first, nib_proto::ReviewOp::Comment(nib_proto::ReviewComment { body: "Cover escaped quotes".into(), ..Default::default() })).unwrap();
        let work_before = work::state(&mailbox, &name);
        work::persist(&mailbox, &name, &root.join("work"), &history_id);
        let second = test_open(&shared, Some(first), AiAgent::Codex, "second-model", &root, &tx).unwrap();
        {
            let s = shared.lock().unwrap();
            assert!(!s.meta.contains_key(&first));
            assert_eq!(s.meta[&second].id_name, name);
            assert_eq!(s.meta[&second].name, "parser");
            assert_eq!(s.meta[&second].rec.lock().unwrap().id(), history_id);
            assert!(s.touched[&second].contains("parser.rs"));
            assert_eq!(s.edits["parser.rs"], second);
            let Session::PerTurn(p) = &s.map[&second] else { panic!("per-turn") };
            assert_eq!(p.resume.lock().unwrap().as_deref(), Some("codex-native"));
            assert_eq!(p.model.as_deref(), Some("second-model"));
            assert!(!s.carry.contains_key(&second), "same provider resumes natively");
        }
        let third = test_open(&shared, Some(second), AiAgent::Grok, "other-provider", &root, &tx).unwrap();
        {
            let s = shared.lock().unwrap();
            let Session::PerTurn(p) = &s.map[&third] else { panic!("per-turn") };
            assert!(p.resume.lock().unwrap().is_none());
            assert!(s.carry[&third].contains("retain escaped quotes"));
            assert!(s.carry[&third].contains("Next: run parser tests"));
            assert!(s.carry[&third].contains("parser-tree"));
            assert!(s.carry[&third].contains("Run escaped quote tests"));
            assert_eq!(p.env.iter().find(|(key, _)| *key == "NIB_AGENT").unwrap().1, std::ffi::OsString::from(&name));
        }
        assert_eq!(work::state(&mailbox, &name).review, work_before.review);
        assert_eq!(review(&shared, third).comments[0].body, "Cover escaped quotes");
        assert_eq!(plan(&shared, third).as_deref(), Some("# Parser\nRun escaped quote tests"));
        assert!(matches!(diff_target(&shared, third), Some(DiffTarget::Tree { .. })));
        // Repeated selection before sending must not lose the waiting context.
        let fourth = test_open(&shared, Some(third), AiAgent::Grok, "another-model", &root, &tx).unwrap();
        assert!(shared.lock().unwrap().carry[&fourth].contains("retain escaped quotes"));
        // Failure starting a streamed provider leaves the existing work owner intact.
        assert!(test_open(&shared, Some(fourth), AiAgent::Claude, "opus", &root, &tx).is_err());
        assert!(shared.lock().unwrap().meta.contains_key(&fourth));
        assert_eq!(history::list(&root, "")[0].agent, AiAgent::Grok);
        close(&shared, fourth);
        let reopened = open_ready(&shared, None, AiAgent::Grok, Some("another-model".into()), Tuning::default(), true, AiMode::Auto, Some(&history_id), &root, Dirs { scratch: Some(&root), nibdir: Some(&root) }, tx, root.join("missing-assistant")).unwrap();
        assert_eq!(plan(&shared, reopened).as_deref(), Some("# Parser\nRun escaped quote tests"));
        assert_eq!(review(&shared, reopened).comments[0].body, "Cover escaped quotes");
        assert!(shared.lock().unwrap().carry[&reopened].contains("parser-tree"));
        close(&shared, reopened);
        history::delete(&root, &history_id).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn a_provider_handoff_delivers_context_with_the_next_message() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("nibd-switch-send-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let exe = root.join("assistant");
        // A local stand-in for Claude's streaming transport; never contacts a provider.
        std::fs::write(&exe, "#!/bin/sh\nIFS= read -r message\nprintf '%s' \"$message\" > \"$NIB_SCRATCH/received.json\"\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"continued\"}'\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let shared: Shared = Default::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let old = test_open(&shared, None, AiAgent::Codex, "first", &root, &tx).unwrap();
        let history_id = {
            let s = shared.lock().unwrap();
            let mut r = s.meta[&old].rec.lock().unwrap();
            r.user("Fix escaped quotes");
            r.event(&AiEvent::Text("Parser changed; tests still need running.".into()));
            r.id()
        };
        let new = open_ready(&shared, Some(old), AiAgent::Claude, Some("opus".into()), Tuning::default(), true, AiMode::Auto, None, &root, Dirs { scratch: Some(&root), nibdir: Some(&root) }, tx, exe).unwrap();
        send(&shared, new, "Run the tests now", &[]);
        loop {
            let msg = rx.recv_timeout(Duration::from_secs(5)).expect("the stand-in should answer");
            if matches!(msg, ServerMsg::Notify(ServerNotify::AiEvent { id, event: AiEvent::TurnDone { .. } }) if id == new) { break; }
        }
        // The agent's own scratch folder, inside the window's.
        let own = shared.lock().unwrap().meta[&new].scratch.clone().unwrap();
        assert!(own.starts_with(root.join("agents")));
        let received: Value = serde_json::from_str(&std::fs::read_to_string(own.join("received.json")).unwrap()).unwrap();
        let prompt = received.pointer("/message/content").unwrap().as_str().unwrap();
        assert!(prompt.contains("Fix escaped quotes"));
        assert!(prompt.contains("tests still need running"));
        assert!(prompt.contains("Current user message:\nRun the tests now"));
        assert!(!shared.lock().unwrap().carry.contains_key(&new));
        let (_, entries) = history::load(&root, &history_id).unwrap();
        assert!(entries.iter().any(|e| matches!(e, nib_proto::AiPastEntry::User(t) if t == "Run the tests now")), "the UI/history retain the user's actual message, without injected context");
        close(&shared, new);
        history::delete(&root, &history_id).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    /// The git work trees an agent made in its own scratch folder go when the agent ends; a
    /// model switch (its name goes on) keeps them, and another agent's or the user's stay.
    #[test]
    fn an_agents_own_work_trees_go_when_it_ends() {
        let dir = std::env::temp_dir().join(format!("nibd-ai-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let sh = |args: &[&str]| Command::new("git").arg("-C").arg(&root).args(args).output().is_ok_and(|o| o.status.success());
        if !sh(&["init", "-q"]) {
            return; // no git on this machine
        }
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        assert!(sh(&["add", "a.txt"]) && sh(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "one"]));
        let mine = dir.join("scratch/agents/claude-1");
        let other = dir.join("scratch/agents/claude-2");
        for (d, t) in [(&mine, "base"), (&other, "base")] {
            std::fs::create_dir_all(d).unwrap();
            assert!(sh(&["worktree", "add", "-q", "--detach", &d.join(t).to_string_lossy()]));
        }
        let listed = |p: &Path| {
            let out = Command::new("git").arg("-C").arg(&root).args(["worktree", "list", "--porcelain"]).output().unwrap();
            let p = nib_proto::path::canonical(p).unwrap_or_else(|_| p.to_path_buf());
            String::from_utf8_lossy(&out.stdout).lines().any(|l| l.strip_prefix("worktree ").is_some_and(|t| Path::new(t) == p))
        };
        assert!(listed(&mine.join("base")) && listed(&other.join("base")));

        let shared: Shared = Default::default();
        let (tx, _rx) = std::sync::mpsc::channel();
        let watch = Watch { sessions: Arc::downgrade(&shared), busy: Arc::new(AtomicBool::new(false)), rec: Arc::new(Mutex::new(history::Recorder::new(&root, AiAgent::Claude, None))) };
        let meta = |id_name: &str, scratch: &Path| Meta { id_name: id_name.into(), name: id_name.into(), agent: AiAgent::Claude, model: None, task: "t".into(), mode: AiMode::Auto, busy: watch.busy.clone(), tx: tx.clone(), rec: watch.rec.clone(), scratch: Some(scratch.to_path_buf()) };
        {
            let mut s = shared.lock().unwrap();
            s.root = Some(root.clone());
            // Sessions 1 and 3 are the same agent (3 took over after a model switch); 2 another.
            s.meta.insert(1, meta("claude-1", &mine));
            s.meta.insert(3, meta("claude-1", &mine));
            s.meta.insert(2, meta("claude-2", &other));
        }
        let settle = || std::thread::sleep(std::time::Duration::from_millis(400));
        close(&shared, 1);
        settle();
        assert!(listed(&mine.join("base")), "its name goes on: kept");
        close(&shared, 3);
        settle();
        assert!(!listed(&mine.join("base")) && !mine.join("base").exists(), "it ended: its work tree is gone");
        assert!(listed(&other.join("base")), "another agent's stays");
        crate::git::remove_worktrees_under(&root, &other);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Worktree-per-feature (D-080): the first message says which way to work, and an agent
    /// that changes the shared project folder while it is on is written to, once.
    #[test]
    fn worktree_per_feature_hints_and_the_letter_when_an_agent_edits_the_shared_tree() {
        assert!(finish_hint(true).contains("nib-work start") && finish_hint(true).contains("never in this project folder"));
        assert!(finish_hint(false).contains("accepting your work commits nothing") && !finish_hint(false).contains("nib-work start"));

        let dir = std::env::temp_dir().join(format!("nibd-ai-wpf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("mailbox/outbox")).unwrap();
        let root = dir.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let sh = |args: &[&str]| Command::new("git").arg("-C").arg(&root).args(args).output().is_ok_and(|o| o.status.success());
        if !sh(&["init", "-q"]) {
            return; // no git on this machine
        }
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        let mailbox = dir.join("mailbox");
        let shared: Shared = Default::default();
        let (tx, _rx) = std::sync::mpsc::channel();
        let watch = Watch { sessions: Arc::downgrade(&shared), busy: Arc::new(AtomicBool::new(false)), rec: Arc::new(Mutex::new(history::Recorder::new(&root, AiAgent::Claude, None))) };
        {
            let mut s = shared.lock().unwrap();
            s.root = Some(root.clone());
            s.mail = Some(mailbox.clone());
            let m = Meta { id_name: "claude-1".into(), name: "add-sum".into(), agent: AiAgent::Claude, model: None, task: "t".into(), mode: AiMode::Auto, busy: watch.busy.clone(), tx, rec: watch.rec.clone(), scratch: None };
            s.meta.insert(1, m);
        }
        let letters = || mail::collect(&mailbox);
        // Off: nothing said. Ignored files and files elsewhere: nothing said either.
        set_worktree_per_feature(&shared, false);
        assert!(!mail::wpf(&mailbox));
        watch.stray_alarm(1, &root.join("a.txt"));
        set_worktree_per_feature(&shared, true);
        assert!(mail::wpf(&mailbox));
        watch.stray_alarm(1, &root.join("x.log"));
        watch.stray_alarm(1, &dir.join("scratch.md"));
        assert!(letters().is_empty());
        // On: told once, with the way out.
        watch.stray_alarm(1, &root.join("a.txt"));
        watch.stray_alarm(1, &root.join("b.txt"));
        let got = letters();
        assert_eq!(got.len(), 1, "once");
        assert_eq!((got[0].from.as_str(), got[0].to.as_str()), (mail::NIB, "add-sum"));
        assert!(got[0].text.contains("a.txt") && got[0].text.contains("nib-work start SLUG --carry"), "{}", got[0].text);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The hook's command: `nib-work guard`'s "no" (3) blocks the edit (2, its reason to Claude
    /// Code); anything else lets it through, so an older nibd's usage error (2) never blocks.
    #[cfg(unix)]
    #[test]
    fn the_plugin_hook_blocks_only_on_the_guards_no() {
        let v: serde_json::Value = serde_json::from_str(PLUGIN_HOOKS).unwrap();
        let hook = &v["hooks"]["PreToolUse"][0];
        assert_eq!(hook["matcher"], "Edit|Write|MultiEdit|NotebookEdit");
        let cmd = hook["hooks"][0]["command"].as_str().unwrap();
        let bin = std::env::temp_dir().join(format!("nibd-hook-{}", std::process::id()));
        std::fs::create_dir_all(&bin).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        for (guard_exit, hook_exit) in [(0, 0), (crate::work::GUARD_NO, 2), (2, 0), (1, 0)] {
            std::fs::write(bin.join("nib-work"), format!("#!/bin/sh\nexit {guard_exit}\n")).unwrap();
            Command::new("chmod").arg("+x").arg(bin.join("nib-work")).status().unwrap();
            let got = Command::new("bash").arg("-c").arg(cmd).env("PATH", &path).status().unwrap().code();
            assert_eq!(got, Some(hook_exit), "guard exited {guard_exit}");
        }
        let plugin = bin.join("plugin");
        write_plugin_hooks(&plugin).unwrap();
        assert_eq!(std::fs::read_to_string(plugin.join("hooks/hooks.json")).unwrap(), PLUGIN_HOOKS);
        let _ = std::fs::remove_dir_all(bin);
    }

    #[test]
    fn what_a_command_changes_in_the_shared_tree_is_its_agents() {
        let dir = std::env::temp_dir().join(format!("nibd-ai-windows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |args: &[&str]| Command::new("git").arg("-C").arg(&dir).args(args).output().is_ok_and(|o| o.status.success());
        if !sh(&["init", "-q", "-b", "trunk"]) {
            return; // no git on this machine
        }
        for args in [&["config", "user.name", "T"][..], &["config", "user.email", "t@example.com"], &["config", "commit.gpgsign", "false"]] {
            assert!(sh(args));
        }
        let dir = nib_proto::path::canonical(&dir).unwrap();
        for f in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(f), "one\n").unwrap();
        }
        assert!(sh(&["add", "-A"]) && sh(&["commit", "-q", "-m", "first"]));
        // c.txt was changed before anybody ran anything: nobody's.
        std::fs::write(dir.join("c.txt"), "mine\n").unwrap();

        let shared: Shared = Default::default();
        let (tx, _rx) = std::sync::mpsc::channel();
        let watch = Watch { sessions: Arc::downgrade(&shared), busy: Arc::new(AtomicBool::new(false)), rec: Arc::new(Mutex::new(history::Recorder::new(&dir, AiAgent::Claude, None))) };
        {
            let mut s = shared.lock().unwrap();
            s.root = Some(dir.clone());
            for id in [1, 2] {
                let m = Meta { id_name: format!("claude-{id}"), name: format!("claude-{id}"), agent: AiAgent::Claude, model: None, task: "t".into(), mode: AiMode::Auto, busy: watch.busy.clone(), tx: tx.clone(), rec: watch.rec.clone(), scratch: None };
                s.meta.insert(id, m);
            }
        }
        let path = |f: &str| dir.join(f).to_string_lossy().into_owned();
        // Agent 1 runs a script that rewrites a.txt and makes n.txt; meanwhile agent 2 edits
        // b.txt with its edit tool.
        watch.begins(1, "cmd");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.join("n.txt"), "new\n").unwrap();
        shared.lock().unwrap().changed(2, &path("b.txt"));
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        watch.ends(1, "cmd");
        let touched = |id: AiId| shared.lock().unwrap().touched.get(&id).map(|t| t.iter().cloned().collect::<Vec<_>>()).unwrap_or_default();
        assert_eq!(touched(1), [path("a.txt"), path("n.txt")]);
        assert_eq!(touched(2), [path("b.txt")]);
        assert!(matches!(diff_target(&shared, 1), Some(DiffTarget::Paths(p)) if p.len() == 2));
        // A command that changes nothing adds nothing; one that was never begun is ignored.
        watch.begins(2, "cmd");
        watch.ends(2, "cmd");
        watch.ends(2, "other");
        assert_eq!(touched(2), [path("b.txt")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn codex_may_write_where_an_agent_must() {
        let env: Vec<(&'static str, OsString)> = vec![
            ("NIB_SCRATCH", "/s/scratch".into()),
            ("NIBDIR", "/s/nibdir".into()),
            ("NIB_MAILBOX", "/s/mailbox".into()),
        ];
        let roots = codex_roots(Path::new("/nonexistent-nib-root"), &env);
        assert_eq!(roots, ["/s/mailbox", "/s/nibdir", "/s/scratch"]);
        assert!(codex_roots(Path::new("/nonexistent-nib-root"), &[]).is_empty());
    }

    #[test]
    fn codex_gets_bundled_skills_without_the_opt_in_skill() {
        let skills = vec![
            AiSkill { name: "nib-control".into(), description: "Drive nib".into(), path: "/skills/nib-control/SKILL.md".into(), group: SkillGroup::Nib },
            AiSkill { name: "nib-upstream".into(), description: "Send upstream".into(), path: "/skills/nib-upstream/SKILL.md".into(), group: SkillGroup::Nib },
        ];
        let catalog = codex_skill_catalog(&skills);
        assert!(catalog.contains("nib-control: Drive nib (/skills/nib-control/SKILL.md)"));
        assert!(!catalog.contains("nib-upstream"));
        assert!(codex_skill_catalog(&skills[1..]).is_empty());
    }


    fn read(t: &mut Turn, line: &str) -> Vec<AiEvent> {
        t.read(&serde_json::from_str(line).unwrap())
    }

    #[test]
    fn stream_json_to_events() {
        let mut t = Turn::default();
        assert_eq!(
            read(&mut t, r#"{"type":"system","subtype":"init","model":"claude-opus-5-5"}"#),
            vec![AiEvent::Started { model: "claude-opus-5-5".into() }]
        );
        let start = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#;
        assert!(read(&mut t, start).is_empty(), "the first block needs no break");
        let delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}"#;
        assert_eq!(read(&mut t, delta), vec![AiEvent::Text("hi".into())]);
        assert_eq!(read(&mut t, start), vec![AiEvent::Text("\n\n".into())]);
        // The whole message repeated at the end is not text again.
        assert!(read(&mut t, r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#).is_empty());
        assert_eq!(
            read(&mut t, r#"{"type":"result","subtype":"success","is_error":false,"result":"hi"}"#),
            vec![AiEvent::Usage(AiUsage::default()), AiEvent::TurnDone { error: None }]
        );
        assert!(read(&mut t, start).is_empty(), "a new turn starts fresh");
        assert_eq!(
            read(&mut t, r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":""}"#)[1],
            AiEvent::TurnDone { error: Some("error_during_execution".into()) }
        );
    }

    #[test]
    fn claude_tool_calls_are_steps() {
        let mut t = Turn::default();
        let call = |name: &str, input: &str| format!(r#"{{"type":"assistant","parent_tool_use_id":null,"message":{{"content":[{{"type":"tool_use","id":"t1","name":"{name}","input":{input}}}]}}}}"#);
        let kind = |t: &mut Turn, name: &str, input: &str| match read(t, &call(name, input)).pop() {
            Some(AiEvent::Step(AiStep { id, parent: None, kind })) if id == "t1" => kind,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            kind(&mut t, "Bash", r#"{"command":"sleep 2 && ls","description":"Wait","timeout":120000}"#),
            AiStepKind::Command { command: "sleep 2 && ls".into(), description: Some("Wait".into()), timeout_ms: Some(120000), background: false }
        );
        assert_eq!(kind(&mut t, "Read", r#"{"file_path":"/p/a.rs","offset":10,"limit":5}"#), AiStepKind::Read { path: "/p/a.rs".into(), offset: Some(10), limit: Some(5) });
        assert_eq!(kind(&mut t, "Edit", r#"{"file_path":"/p/a.rs","old_string":"beta","new_string":"gamma"}"#), AiStepKind::Edit { path: "/p/a.rs".into(), old: "beta".into(), new: "gamma".into() });
        assert_eq!(kind(&mut t, "Skill", r#"{"skill":"nib-control"}"#), AiStepKind::Skill { name: "nib-control".into() });
        assert_eq!(kind(&mut t, "WebFetch", r#"{"url":"x"}"#), AiStepKind::Other { tool: "WebFetch".into(), input: r#"{"url":"x"}"#.into() });
        // The text of the same message is not repeated: it came as deltas.
        assert!(read(&mut t, r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#).is_empty());

        // Results: an edit says where it landed, a failed command says so.
        let edit = r#"{"type":"user","message":{"content":[{"tool_use_id":"t1","type":"tool_result","content":"updated"}]},"tool_use_result":{"structuredPatch":[{"oldStart":5,"newStart":5,"lines":[" a"," b","-beta","+gamma"," c"]}]}}"#;
        assert_eq!(read(&mut t, edit), [AiEvent::StepDone(AiStepDone { id: "t1".into(), ok: true, output: "updated".into(), line: Some(7) })]);
        let failed = r#"{"type":"user","message":{"content":[{"tool_use_id":"t2","type":"tool_result","is_error":true,"content":[{"type":"text","text":"exit 1"}]}]}}"#;
        assert_eq!(read(&mut t, failed), [AiEvent::StepDone(AiStepDone { id: "t2".into(), ok: false, output: "exit 1".into(), line: None })]);
        // What the user typed is not a step.
        assert!(read(&mut t, r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#).is_empty());
    }

    #[test]
    fn claude_background_sub_agent_ends_when_it_says_so() {
        let mut t = Turn::default();
        // Its launch comes back at once: the step goes on.
        let launched = r#"{"type":"user","message":{"content":[{"tool_use_id":"a1","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]},"tool_use_result":{"isAsync":true,"status":"async_launched","agentId":"x"}}"#;
        assert_eq!(read(&mut t, launched), [AiEvent::Background { id: "a1".into() }]);
        // What it does is under it.
        let inner = r#"{"type":"assistant","parent_tool_use_id":"a1","message":{"content":[{"type":"tool_use","id":"t9","name":"Bash","input":{"command":"echo hi"}}]}}"#;
        assert!(matches!(&read(&mut t, inner)[..], [AiEvent::Step(AiStep { parent: Some(p), .. })] if p == "a1"));
        // A background command's notice is not about a step of this kind.
        assert!(read(&mut t, r#"{"type":"system","subtype":"task_notification","tool_use_id":"b7","status":"completed","summary":"x"}"#).is_empty());
        let end = r#"{"type":"system","subtype":"task_notification","task_id":"x","tool_use_id":"a1","status":"completed","summary":"done"}"#;
        assert_eq!(read(&mut t, end), [AiEvent::StepDone(AiStepDone { id: "a1".into(), ok: true, output: "done".into(), line: None })]);
        assert!(read(&mut t, end).is_empty(), "it ends once");
        // One that failed or was stopped did not end well.
        read(&mut t, &launched.replace("a1", "a2"));
        let failed = read(&mut t, r#"{"type":"system","subtype":"task_notification","tool_use_id":"a2","status":"failed","summary":"no"}"#);
        assert_eq!(failed, [AiEvent::StepDone(AiStepDone { id: "a2".into(), ok: false, output: "no".into(), line: None })]);
    }

    #[test]
    fn claude_asks_leave_and_reports_its_mode() {
        let mut t = Turn::default();
        let ask = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/p/a.txt","content":"hi"},"description":"a.txt","tool_use_id":"t1"}}"#;
        assert_eq!(read(&mut t, ask), [AiEvent::Ask(AiAsk { request: "r1".into(), step: Some("t1".into()), tool: "Write".into(), what: "a.txt".into(), questions: vec![] })]);
        assert_eq!(t.asks.lock().unwrap()["r1"]["content"], "hi", "the input is kept: it goes back with a yes");
        let plan = r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{"plan":"1. Do it"},"tool_use_id":"t2"}}"#;
        assert!(matches!(&read(&mut t, plan)[..], [AiEvent::Ask(a)] if a.what == "1. Do it" && a.questions.is_empty()));
        let q = r#"{"type":"control_request","request_id":"r3","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Rename it too?","options":[{"label":"Yes","description":"keep an alias"},{"label":"No"}]}]}}}"#;
        let [AiEvent::Ask(a)] = &read(&mut t, q)[..] else { panic!("a question") };
        let yes = AiOption { label: "Yes".into(), description: "keep an alias".into(), preview: None };
        let no = AiOption { label: "No".into(), description: String::new(), preview: None };
        assert_eq!(a.questions, [AiQuestion { header: String::new(), question: "Rename it too?".into(), options: vec![yes, no], multi: false }]);
        // The answers go back in the call's input, by question, with a yes.
        let input = t.asks.lock().unwrap().remove("r3").unwrap();
        let answered = with_answers(input, &["No".into()]);
        assert_eq!(answered["answers"], serde_json::json!({ "Rename it too?": "No" }));
        assert_eq!(answered["questions"][0]["options"][0]["label"], "Yes", "the rest stays");
        assert_eq!(with_answers(serde_json::json!({ "content": "hi" }), &[]), serde_json::json!({ "content": "hi" }));
        let mode = r#"{"type":"control_response","response":{"subtype":"success","request_id":"m1","response":{"mode":"acceptEdits"}}}"#;
        assert_eq!(read(&mut t, mode), [AiEvent::Mode(AiMode::AcceptEdits)]);
        assert!(read(&mut t, r#"{"type":"control_response","response":{"subtype":"success","request_id":"i1","response":{"still_queued":[]}}}"#).is_empty());
    }

    #[test]
    fn codex_and_opencode_runs_show_their_steps() {
        let (_, events) = shot(
            AiAgent::Codex,
            &[
                r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"cargo test","status":"in_progress"}}"#,
                r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"cargo test","aggregated_output":"ok","exit_code":0,"status":"completed"}}"#,
                r#"{"type":"item.completed","item":{"id":"i2","type":"file_change","changes":[{"path":"src/a.rs","kind":"update"}],"status":"completed"}}"#,
            ],
        );
        let command = AiStepKind::Command { command: "cargo test".into(), description: None, timeout_ms: None, background: false };
        assert_eq!(
            events,
            [
                AiEvent::Step(AiStep { id: "i1".into(), parent: None, kind: command }),
                AiEvent::StepDone(AiStepDone { id: "i1".into(), ok: true, output: "ok".into(), line: None }),
                AiEvent::Step(AiStep { id: "i2:0".into(), parent: None, kind: AiStepKind::Edit { path: "src/a.rs".into(), old: String::new(), new: String::new() } }),
                AiEvent::StepDone(AiStepDone { id: "i2:0".into(), ok: true, output: "update".into(), line: None }),
            ]
        );
        let (_, events) = shot(
            AiAgent::OpenCode,
            &[
                r#"{"type":"tool_use","sessionID":"s","part":{"type":"tool","tool":"bash","callID":"c1","state":{"status":"completed","input":{"command":"ls","timeout":5000},"output":"a.rs\n"}}}"#,
                r#"{"type":"tool_use","sessionID":"s","part":{"type":"tool","tool":"edit","callID":"c2","state":{"status":"error","input":{"filePath":"a.rs","oldString":"x","newString":"y"},"error":"denied"}}}"#,
                r#"{"type":"tool_use","sessionID":"s","part":{"type":"tool","tool":"read","state":{"status":"running"}}}"#,
            ],
        );
        assert_eq!(
            events,
            [
                AiEvent::Step(AiStep { id: "c1".into(), parent: None, kind: AiStepKind::Command { command: "ls".into(), description: None, timeout_ms: Some(5000), background: false } }),
                AiEvent::StepDone(AiStepDone { id: "c1".into(), ok: true, output: "a.rs\n".into(), line: None }),
                AiEvent::Step(AiStep { id: "c2".into(), parent: None, kind: AiStepKind::Edit { path: "a.rs".into(), old: "x".into(), new: "y".into() } }),
                AiEvent::StepDone(AiStepDone { id: "c2".into(), ok: false, output: "denied".into(), line: None }),
            ]
        );
    }

    #[test]
    fn the_mode_decides_what_a_run_may_do() {
        assert_eq!(opencode_permissions(AiMode::Plan), read_only_permissions());
        assert_eq!(opencode_permissions(AiMode::AcceptEdits)["bash"], "deny");
        assert_eq!((&opencode_permissions(AiMode::Auto)["edit"], &opencode_permissions(AiMode::Auto)["bash"]), (&Value::from("allow"), &Value::from("allow")));
        assert_eq!(opencode_permissions(AiMode::Auto)["external_directory"], "deny");
        // Pictures: files for OpenCode, a mention for Grok.
        let img = [PathBuf::from("/t/1.png")];
        assert_eq!(&one_shot_args(AiAgent::OpenCode, None, None, AiMode::Plan, &img, "hi")[3..], ["--file", "/t/1.png", "--", "hi"]);
        assert!(one_shot_args(AiAgent::Grok, None, None, AiMode::Plan, &img, "hi").last().unwrap().ends_with("(image: /t/1.png)"));
    }

    #[test]
    fn base64_pads() {
        assert_eq!([base64(b""), base64(b"f"), base64(b"fo"), base64(b"foo"), base64(b"foob")], ["", "Zg==", "Zm8=", "Zm9v", "Zm9vYg=="]);
    }

    #[cfg(unix)]
    #[test]
    fn nibs_skills_become_a_plugin_without_the_opt_in_one() {
        let dir = std::env::temp_dir().join(format!("nibd-plugin-{}", std::process::id()));
        let src = dir.join("src");
        for name in ["nib-control", "nib-upstream"] {
            std::fs::create_dir_all(src.join(name)).unwrap();
            std::fs::write(src.join(name).join("SKILL.md"), format!("---\nname: {name}\ndescription: d\n---\nbody")).unwrap();
        }
        let skills = nib_proto::skills::list(&src, SkillGroup::Nib);
        let plugin = link_plugin(&dir.join("plugin"), &skills).unwrap();
        assert!(plugin.join(".claude-plugin/plugin.json").is_file());
        assert!(plugin.join("skills/nib-control/SKILL.md").is_file());
        assert!(!plugin.join("skills/nib-upstream").exists());
        assert!(link_plugin(&dir.join("none"), &[]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_output_loses_its_middle() {
        let long = format!("head{}tail", "é".repeat(STEP_OUTPUT));
        let c = clip(long);
        assert!(c.len() < STEP_OUTPUT + 100 && c.starts_with("head") && c.ends_with("tail") && c.contains("bytes left out"));
        assert_eq!(clip("short".into()), "short");
    }

    #[test]
    fn a_process_tree_is_found_parents_first() {
        let table = "  10   1\n  11  10\n  12  11\n  13  10\n  20   1\n bad line\n";
        assert_eq!(descendants(10, table), [10, 11, 13, 12]);
        assert_eq!(descendants(99, table), [99]);
    }

    #[cfg(unix)]
    #[test]
    fn a_frozen_process_tree_stops_and_carries_on() {
        let dir = std::env::temp_dir().join(format!("nibd-freeze-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let script = format!("while true; do echo x >> '{}'; sleep 0.05; done", log.display());
        // A shell that starts another: the grandchild writes.
        let mut child = Command::new("sh").args(["-c", &format!("sh -c \"{script}\" & wait")]).spawn().unwrap();
        let lines = || std::fs::read_to_string(&log).map(|t| t.lines().count()).unwrap_or(0);
        let wait_for = |f: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !f() && t.elapsed() < Duration::from_secs(5) {
                thread::sleep(Duration::from_millis(20));
            }
        };
        wait_for(&|| lines() > 2);
        signal_tree(child.id(), "STOP").unwrap();
        thread::sleep(Duration::from_millis(150));
        let frozen_at = lines();
        thread::sleep(Duration::from_millis(300));
        assert_eq!(lines(), frozen_at, "nothing is written while frozen");
        signal_tree(child.id(), "CONT").unwrap();
        wait_for(&|| lines() > frozen_at + 2);
        assert!(lines() > frozen_at + 2, "it carries on");
        let mut ps = Command::new("ps");
        ps.args(["-axo", "pid=,ppid="]);
        let all = descendants(child.id(), &run_for(ps, Duration::from_secs(5)).unwrap());
        let _ = Command::new("kill").arg("-KILL").args(all.iter().map(u32::to_string)).status();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_compacts_its_own_session() {
        let mut t = Turn { context: Some(4528), ..Default::default() };
        assert!(read(&mut t, r#"{"type":"system","subtype":"status","status":"compacting"}"#).is_empty());
        let boundary = r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":4528,"post_tokens":893}}"#;
        assert_eq!(read(&mut t, boundary), [AiEvent::Compacted { before: Some(4528), after: Some(893) }]);
        let done = read(&mut t, r#"{"type":"result","is_error":false,"num_turns":0,"total_cost_usd":0.0014}"#);
        assert!(matches!(&done[0], AiEvent::Usage(u) if u.context_tokens == Some(893)), "{done:?}");
        assert_eq!(done[1], AiEvent::TurnDone { error: None });
    }

    #[test]
    fn claude_reports_its_context_and_limits() {
        let mut t = Turn::default();
        read(&mut t, r#"{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":2,"cache_creation_input_tokens":3860,"cache_read_input_tokens":649,"output_tokens":4}}}}"#);
        read(&mut t, r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"input_tokens":2,"cache_creation_input_tokens":3860,"cache_read_input_tokens":649,"output_tokens":14}}}"#);
        read(
            &mut t,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","unifiedWindows":{"seven_day":{"utilization":0.19,"resetsAt":1795341600},"five_hour":{"utilization":0.25,"resetsAt":1795197000},"mystery":{"utilization":1.0}}}}"#,
        );
        let got = read(
            &mut t,
            r#"{"type":"result","is_error":false,"total_cost_usd":0.5,"modelUsage":{"claude-haiku-5-5":{"inputTokens":4,"contextWindow":1000000}}}"#,
        );
        let AiEvent::Usage(u) = &got[0] else { panic!("{got:?}") };
        assert_eq!((u.context_tokens, u.context_window, u.cost_usd), (Some(4525), Some(1_000_000), Some(0.5)));
        assert_eq!(
            u.limits,
            [AiLimit { window_minutes: 300, used: 0.25, resets_at: Some(1795197000) }, AiLimit { window_minutes: 10080, used: 0.19, resets_at: Some(1795341600) }]
        );
    }

    #[test]
    fn codex_usage_comes_from_its_session_record() {
        let record = r#"{"timestamp":"2026-09-15T13:49:31.532Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":166203},"model_context_window":258400},"rate_limits":{"primary":{"used_percent":42.0,"window_minutes":300,"resets_in_seconds":60},"secondary":{"used_percent":10.0,"window_minutes":10080,"resets_at":1795341600}}}}
{"timestamp":"2026-09-15T13:49:32.000Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"primary":null}}}"#;
        let u = codex_usage(record).unwrap();
        assert_eq!((u.context_tokens, u.context_window), (Some(166203), Some(258400)));
        assert_eq!(u.limits[0], AiLimit { window_minutes: 300, used: 0.42, resets_at: Some(unix_secs("2026-09-15T13:50:31Z").unwrap()) });
        assert_eq!(u.limits[1].resets_at, Some(1795341600));
        assert_eq!(unix_secs("1970-01-02T00:00:01Z"), Some(86401));
        assert_eq!(unix_secs("2026-11-20T16:41:56.116Z"), Some(1795192916));
    }

    #[test]
    fn opencode_reports_context_cost_and_speed() {
        let (s, _) = shot(
            AiAgent::OpenCode,
            &[
                r#"{"type":"step_start","sessionID":"s","part":{"type":"step-start"}}"#,
                r#"{"type":"text","sessionID":"s","part":{"type":"text","text":"Hi","time":{"start":1000,"end":3000}}}"#,
                r#"{"type":"step_finish","sessionID":"s","part":{"type":"step-finish","reason":"stop","cost":0.01,"tokens":{"input":100,"output":80,"reasoning":20,"cache":{"read":900,"write":0}}}}"#,
            ],
        );
        assert_eq!((s.context, s.cost), (Some(1080), 0.01));
        assert_eq!(s.tokens_per_sec(), Some(50.0));
    }

    fn shot(agent: AiAgent, lines: &[&str]) -> (OneShot, Vec<AiEvent>) {
        let mut s = OneShot::default();
        let events = lines.iter().flat_map(|l| s.read(agent, &serde_json::from_str(l).unwrap())).collect();
        (s, events)
    }

    #[test]
    fn codex_exec_json_to_events() {
        let (mut s, events) = shot(
            AiAgent::Codex,
            &[
                r#"{"type":"thread.started","thread_id":"0199a213-81c0"}"#,
                r#"{"type":"turn.started"}"#,
                r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"thinking"}}"#,
                r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"Hello"}}"#,
                r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"Again"}}"#,
                r#"{"type":"turn.completed","usage":{"input_tokens":1}}"#,
            ],
        );
        assert_eq!(
            events,
            vec![AiEvent::Text("Hello".into()), AiEvent::Text("\n\n".into()), AiEvent::Text("Again".into()), AiEvent::TurnDone { error: None }]
        );
        assert_eq!(s.session.as_deref(), Some("0199a213-81c0"));
        assert!(s.exited(AiAgent::Codex, Some(0), String::new()).is_empty(), "the turn already ended");

        let (_, events) = shot(AiAgent::Codex, &[r#"{"type":"error","message":"stream disconnected"}"#, r#"{"type":"turn.failed","error":{}}"#]);
        assert_eq!(events, vec![AiEvent::TurnDone { error: Some("stream disconnected".into()) }]);
    }

    #[test]
    fn codex_app_server_questions_and_steps_become_nib_events() {
        let request = serde_json::json!({ "jsonrpc":"2.0", "id":91, "method":"item/tool/requestUserInput", "params": {
            "itemId":"q1", "questions":[{ "id":"target", "header":"Target", "question":"Which file?", "options":[
                {"label":"A", "description":"First"}, {"label":"B", "description":"Second"}
            ]}]
        }});
        let (ask, ids) = codex_question(&request).unwrap();
        assert_eq!(ask.request, "codex-91");
        assert_eq!(ask.questions[0].options[1].label, "B");
        assert_eq!(ids, [("target".into(), vec!["A".into(), "B".into()])]);
        assert_eq!(codex_answers(ids.clone(), true, &["B".into()]), serde_json::json!({ "answers": { "target": { "answers": ["B"] } } }));
        assert_eq!(codex_answers(ids.clone(), true, &["another file".into()]), serde_json::json!({ "answers": { "target": { "answers": ["user_note: another file"] } } }));
        assert_eq!(codex_answers(ids, false, &[]), serde_json::json!({ "answers": {} }));
        let mut shot = OneShot::default();
        let started = serde_json::json!({ "item": { "id":"c1", "type":"commandExecution", "command":"pwd", "status":"inProgress" } });
        let finished = serde_json::json!({ "item": { "id":"c1", "type":"commandExecution", "command":"pwd", "status":"completed", "exitCode":0, "aggregatedOutput":"/p\n" } });
        assert!(matches!(&codex_app_event(&mut shot, "item/started", &started)[..], [AiEvent::Step(_)]));
        assert!(matches!(&codex_app_event(&mut shot, "item/completed", &finished)[..], [AiEvent::StepDone(d)] if d.ok && d.output == "/p\n"));
        assert_eq!(codex_app_event(&mut shot, "turn/completed", &serde_json::json!({ "turn": { "status":"completed" } })), [AiEvent::TurnDone { error: None }]);
    }

    #[cfg(unix)]
    #[test]
    fn codex_app_server_question_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("nibd-codex-question-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("codex");
        std::fs::write(&fake, r#"#!/usr/bin/env python3
import json, sys
def recv(): return json.loads(sys.stdin.readline())
def send(v): print(json.dumps(v), flush=True)
assert recv()['method'] == 'initialize'
send({'jsonrpc':'2.0','id':1,'result':{}})
assert recv()['method'] == 'initialized'
thread = recv()
assert thread['method'] in ('thread/start', 'thread/resume')
if thread['method'] == 'thread/resume': assert thread['params']['threadId'] == 'fake-thread'
assert thread['params']['sandbox'] == ('read-only' if thread['method'] == 'thread/resume' else 'workspace-write')
if thread['method'] == 'thread/start': assert thread['params']['config']['sandbox_workspace_write']['writable_roots']
send({'jsonrpc':'2.0','id':2,'result':{'thread':{'id':'fake-thread'},'model':'gpt-6-sol'}})
turn = recv()
assert turn['method'] == 'turn/start'
assert turn['params']['input'][-1]['text'] == 'Which file?'
if thread['method'] == 'thread/resume': assert turn['params']['collaborationMode']['mode'] == 'plan'
send({'jsonrpc':'2.0','id':91,'method':'item/tool/requestUserInput','params':{'threadId':'fake-thread','turnId':'t1','itemId':'q1','questions':[{'id':'target','header':'Target','question':'Which file?','options':[{'label':'A','description':'First'},{'label':'B','description':'Second'}]}]}})
reply = recv()
if reply.get('id') != 91 or reply.get('result') != {'answers': {'target': {'answers': ['B']}}}:
    send({'method':'turn/completed','params':{'turn':{'status':'failed','error':{'message':'wrong answer'}}}})
else:
    send({'method':'item/completed','params':{'item':{'id':'m1','type':'agentMessage','text':'You chose B.'}}})
    send({'method':'turn/completed','params':{'turn':{'status':'completed'}}})
"#).unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();
        let shared: Shared = Default::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let watch = Watch { sessions: Arc::downgrade(&shared), busy: Arc::new(AtomicBool::new(true)), rec: Arc::new(Mutex::new(history::Recorder::new(&dir, AiAgent::Codex, None))) };
        let p = Arc::new(PerTurn {
            agent: AiAgent::Codex, env: vec![("NIB_SCRATCH", dir.as_os_str().to_os_string())], watch,
            mail: Mutex::new(Vec::new()), tools: true, mode: Mutex::new(AiMode::Auto), exe: fake,
            model: None, tuning: Tuning { effort: None, prompt: None }, prompt_file: None,
            root: dir.clone(), tx, id: 1, resume: Mutex::new(None), child: Mutex::new(None),
            codex_input: Mutex::new(None), codex_questions: Mutex::new(HashMap::new()),
            local: false, cost: Mutex::new(0.0), rollout: Mutex::new(None), carry: Mutex::new(None),
        });
        shared.lock().unwrap().map.insert(1, Session::PerTurn(Arc::clone(&p)));
        p.send("Which file?", &[]);
        let ask = loop {
            let msg = rx.recv_timeout(Duration::from_secs(5)).expect("Codex question");
            if let ServerMsg::Notify(ServerNotify::AiEvent { event: AiEvent::Ask(ask), .. }) = msg { break ask }
        };
        assert_eq!(ask.questions[0].question, "Which file?");
        answer(&shared, 1, &ask.request, true, None, &["B".into()]);
        let mut text = String::new();
        let done = loop {
            let msg = rx.recv_timeout(Duration::from_secs(5)).expect("Codex reply");
            if let ServerMsg::Notify(ServerNotify::AiEvent { event, .. }) = msg {
                match event {
                    AiEvent::Text(t) => text.push_str(&t),
                    AiEvent::TurnDone { error } => break error,
                    _ => {}
                }
            }
        };
        assert_eq!(done, None);
        assert_eq!(text, "You chose B.");
        *p.mode.lock().unwrap() = AiMode::Plan;
        p.send("Which file?", &[]);
        let ask = loop {
            let msg = rx.recv_timeout(Duration::from_secs(5)).expect("resumed Codex question");
            if let ServerMsg::Notify(ServerNotify::AiEvent { event: AiEvent::Ask(ask), .. }) = msg { break ask }
        };
        answer(&shared, 1, &ask.request, true, None, &["B".into()]);
        let done = loop {
            let msg = rx.recv_timeout(Duration::from_secs(5)).expect("resumed Codex reply");
            if let ServerMsg::Notify(ServerNotify::AiEvent { event: AiEvent::TurnDone { error }, .. }) = msg { break error }
        };
        assert_eq!(done, None);
        close(&shared, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grok_streaming_json_to_events() {
        let (s, events) = shot(
            AiAgent::Grok,
            &[
                r#"{"type":"thought","data":"hmm"}"#,
                r#"{"type":"text","data":"Hel"}"#,
                r#"{"type":"text","data":"lo"}"#,
                r#"{"type":"end","stopReason":"end_turn","sessionId":"s-1"}"#,
            ],
        );
        assert_eq!(events, vec![AiEvent::Text("Hel".into()), AiEvent::Text("lo".into()), AiEvent::TurnDone { error: None }]);
        assert_eq!(s.session.as_deref(), Some("s-1"));

        let (mut s, events) = shot(AiAgent::Grok, &[r#"{"type":"error","message":"Couldn't start session: not signed in"}"#]);
        assert!(events.is_empty());
        assert_eq!(
            s.exited(AiAgent::Grok, Some(1), "noise".into()),
            vec![AiEvent::TurnDone { error: Some("Couldn't start session: not signed in".into()) }]
        );
    }

    #[test]
    fn opencode_run_json_to_events() {
        let (s, events) = shot(
            AiAgent::OpenCode,
            &[
                r#"{"type":"step_start","sessionID":"ses_1","part":{"type":"step-start"}}"#,
                r#"{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"Looking."}}"#,
                r#"{"type":"step_finish","sessionID":"ses_1","part":{"type":"step-finish","reason":"tool-calls"}}"#,
                r#"{"type":"step_start","sessionID":"ses_1","part":{"type":"step-start"}}"#,
                r#"{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"Five crates."}}"#,
                r#"{"type":"step_finish","sessionID":"ses_1","part":{"type":"step-finish","reason":"stop"}}"#,
            ],
        );
        assert_eq!(
            events,
            vec![AiEvent::Text("Looking.".into()), AiEvent::Text("\n\n".into()), AiEvent::Text("Five crates.".into()), AiEvent::TurnDone { error: None }]
        );
        assert_eq!(s.session.as_deref(), Some("ses_1"));

        let (mut s, events) = shot(
            AiAgent::OpenCode,
            &[r#"{"type":"error","sessionID":"ses_2","error":{"name":"UnknownError","data":{"message":"Unexpected server error."}}}"#],
        );
        assert!(events.is_empty());
        assert_eq!(s.exited(AiAgent::OpenCode, Some(1), String::new()), vec![AiEvent::TurnDone { error: Some("Unexpected server error.".into()) }]);
    }

    #[test]
    #[cfg(unix)]
    fn opencode_is_installed_with_ollama_and_never_signed_in() {
        assert!(can_install(AiAgent::OpenCode));
        let (cmd, _) = setup_cmd(AiAgent::OpenCode, false).unwrap();
        // name exe url runner host login, then Ollama's, browser, then no sign-in arguments.
        assert_eq!(cmd[4..8], ["OpenCode", "opencode", "https://opencode.ai/install", "bash"]);
        assert_eq!(cmd[9], "no", "OpenCode has no sign-in");
        assert_eq!(cmd[10..], ["Ollama", "ollama", OLLAMA_URL, OLLAMA_RUNNER, "no"]);
        let (claude, _) = setup_cmd(AiAgent::Claude, false).unwrap();
        assert_eq!(claude[10..14], ["", "", "", ""], "only OpenCode brings Ollama");
    }

    #[test]
    fn opencode_sees_every_downloaded_ollama_model() {
        let c: Value = serde_json::from_str(&opencode_config(read_only_permissions(), "ollama/x:1", &ollama::GpuSel::Cpu)).unwrap();
        assert_eq!(c["permission"]["edit"], "deny");
        // A GPU selection's own server, for the model's provider.
        assert!(c["provider"]["ollama"]["options"]["baseURL"].as_str().unwrap().contains(":11499/"));
        let listing: Value = serde_json::from_str(&opencode_config(Value::Null, "", &ollama::GpuSel::All)).unwrap();
        assert!(listing.get("permission").is_none());
        if !ollama::pulled().is_empty() {
            assert_eq!(listing["provider"]["ollama"]["npm"], "@ai-sdk/openai-compatible");
        }
    }

    #[test]
    fn a_run_that_says_nothing_reports_how_it_ended() {
        let mut s = OneShot::default();
        assert_eq!(s.exited(AiAgent::Codex, Some(2), String::new()), vec![AiEvent::TurnDone { error: Some("Codex exited with code 2".into()) }]);
        let mut s = OneShot::default();
        assert_eq!(s.exited(AiAgent::Codex, Some(1), "Not logged in".into()), vec![AiEvent::TurnDone { error: Some("Not logged in".into()) }]);
    }

    #[test]
    fn follow_up_messages_resume_the_session() {
        let grok = one_shot_args(AiAgent::Grok, None, Some("s1"), AiMode::Plan, &[], "-x");
        assert_eq!(&grok[4..], ["--resume", "s1", "-p", " -x"]);
        assert_eq!(one_shot_args(AiAgent::OpenCode, None, None, AiMode::Plan, &[], "hi"), ["run", "--format", "json", "--", "hi"]);
        assert_eq!(&one_shot_args(AiAgent::OpenCode, None, Some("ses_1"), AiMode::Plan, &[], "-x")[3..], ["--session", "ses_1", "--", "-x"]);
    }

    #[test]
    fn the_chosen_model_goes_on_every_run() {
        let oc = one_shot_args(AiAgent::OpenCode, Some("ollama/qwen3:8b"), None, AiMode::Plan, &[], "hi");
        assert_eq!(&oc[3..], ["--model", "ollama/qwen3:8b", "--", "hi"]);
    }

    #[test]
    fn claude_models_come_from_its_catalog() {
        let catalog = r#"{"version":2,"catalog":{"config":{"models":[
            {"id":"claude-opus-5-5","name":"Opus 5.5","section":"main"},
            {"id":"claude-sonnet-4-6","name":"Sonnet 4.6","section":"overflow"}]}}}"#;
        let got: Vec<_> = claude_models(catalog).into_iter().map(|m| (m.id, m.name)).collect();
        assert_eq!(got, [("claude-opus-5-5".to_string(), "Opus 5.5".to_string()), ("claude-sonnet-4-6".into(), "Sonnet 4.6".into())]);
        assert!(claude_models("{}").is_empty());
    }

    #[test]
    fn codex_models_are_the_ones_its_picker_lists() {
        let cache = r#"{"models":[
            {"slug":"b","display_name":"B","visibility":"list","priority":2},
            {"slug":"hidden","display_name":"H","visibility":"hide","priority":0},
            {"slug":"a","display_name":"A","visibility":"list","priority":1,"supported_reasoning_levels":[{"effort":"low"},{"effort":"max"}]}]}"#;
        let ids: Vec<_> = codex_models(cache).into_iter().map(|m| (m.id, m.name, m.efforts)).collect();
        assert_eq!(ids, [("a".to_string(), "A".to_string(), vec!["low".into(), "max".into()]), ("b".into(), "B".into(), vec![])]);
        assert!(codex_models("not json").is_empty());
    }

    #[test]
    fn setup_installs_then_signs_in() {
        // Run the setup script with stand-ins: a "curl" that serves an installer, which drops a
        // fake assistant whose `login` succeeds.
        let dir = std::env::temp_dir().join(format!("nibd-ai-setup-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let installer = dir.join("install.sh");
        let fake = bin.join("fakeai");
        std::fs::write(&installer, format!("printf '#!/bin/sh\\necho signed-in \"$*\"\\n' > {0}; chmod +x {0}\n", fake.display())).unwrap();
        let curl = bin.join("curl");
        std::fs::write(&curl, format!("#!/bin/sh\ncat {}\n", installer.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
            let (base, _) = setup_cmd(AiAgent::Codex, false).unwrap();
            let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
            // Over SSH the device code, here the browser.
            for (remote, said, signed) in [
                (true, "on any device", "signed-in login --device-auth\n"),
                (false, "browser that opens", "signed-in login\n"),
            ] {
                // Same script; the stand-in assistant, not installed and not signed in.
                let (args, browser) = login_args(&spec(AiAgent::Codex), remote);
                let mut cmd = base[..14].to_vec();
                cmd[5] = "fakeai".into();
                cmd[9] = "yes".into();
                cmd.push(if browser { "yes" } else { "no" }.into());
                cmd.extend(args.iter().map(|s| s.to_string()));
                let _ = std::fs::remove_file(&fake);
                let out = Command::new(&cmd[0]).args(&cmd[1..]).env("PATH", &path).stdin(Stdio::null()).output().unwrap();
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(out.status.success(), "{stdout}");
                assert!(stdout.contains("Installing Codex"), "{stdout}");
                assert!(stdout.contains(said), "{stdout}");
                assert!(stdout.contains(signed), "{stdout}");
                assert!(stdout.contains("Codex is ready"), "{stdout}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On a machine shared with other users, the temp dir is theirs too. A name nibd uses there
    /// that they can guess, they can make first: here, the folder for pictures and the system
    /// prompt's file, made in advance with links to one of the user's files.
    #[cfg(unix)]
    #[test]
    fn files_handed_to_assistants_cannot_be_planted_in_the_temp_dir() {
        let victim_dir = std::env::temp_dir().join(format!("nibd-victim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&victim_dir);
        std::fs::create_dir_all(&victim_dir).unwrap();
        let victim = victim_dir.join("bashrc");
        std::fs::write(&victim, "the user's own file\n").unwrap();
        // What another user prepares, knowing nibd's pid (pids are few and handed out in order).
        let pid = std::process::id();
        let images = std::env::temp_dir().join(format!("nibd-images-{pid}"));
        let _ = std::fs::remove_dir_all(&images);
        std::fs::create_dir_all(&images).unwrap();
        for n in 0..4 {
            std::os::unix::fs::symlink(&victim, images.join(format!("{n}.png"))).unwrap();
        }
        let prompt = std::env::temp_dir().join(format!("nib-prompt-{pid}-7.md"));
        let _ = std::fs::remove_file(&prompt);
        std::os::unix::fs::symlink(&victim, &prompt).unwrap();

        let saved = image_files(&[AiImage { media_type: "image/png".into(), data: b"PNG bytes".to_vec() }]);
        let prompt_at = prompt_file(7, "the system prompt");
        let after = std::fs::read_to_string(&victim).unwrap();
        let (planted_images, planted_prompt) = (saved.iter().any(|p| p.starts_with(&images)), prompt_at.as_deref() == Some(prompt.as_path()));
        let _ = std::fs::remove_dir_all(&images);
        let _ = std::fs::remove_file(&prompt);
        let _ = std::fs::remove_dir_all(&victim_dir);
        assert_eq!(after, "the user's own file\n", "a link planted in the temp dir made nibd overwrite the user's file");
        assert!(!planted_images && !planted_prompt, "nibd used a folder or file somebody else made: images {saved:?}, prompt {prompt_at:?}");
        // Where they went instead: a folder only this user can open.
        #[allow(clippy::unnecessary_cast)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = crate::places::private_temp().unwrap();
            assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    /// Codex's sandbox may write what a work tree's commits need in the repository's git folder,
    /// and nothing that makes a later git command (nib's, or the user's) run something.
    #[test]
    fn codex_may_commit_in_its_work_tree_but_not_write_gits_config_or_hooks() {
        let dir = std::env::temp_dir().join(format!("nibd-codex-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if !Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]).status().is_ok_and(|s| s.success()) {
            return; // no git on this machine
        }
        let git = nib_proto::path::canonical(&dir.join(".git")).unwrap();
        let roots: Vec<PathBuf> = codex_roots(&dir, &[]).into_iter().map(PathBuf::from).collect();
        let writable = |p: &Path| roots.iter().any(|r| p.starts_with(r));
        for no in ["config", "config.worktree", "hooks/pre-commit", "hooks/post-commit", "HEAD", "index", "info/attributes", "refs/heads/main", "packed-refs", "logs/HEAD"] {
            assert!(!writable(&git.join(no)), "the sandbox may write .git/{no}: {roots:?}");
        }
        for yes in ["objects/ab/cdef", "refs/heads/nib/feat/x", "logs/refs/heads/nib/feat/x", "worktrees/x/index", "worktrees/x/HEAD"] {
            assert!(writable(&git.join(yes)), "the sandbox may not write .git/{yes}: {roots:?}");
        }
        for made in ["refs/heads/nib", "logs/refs/heads/nib", "worktrees"] {
            assert!(git.join(made).is_dir(), "{made} was not made");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
