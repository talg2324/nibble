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
use crate::ollama;
use nib_proto::{
    AiAgent, AiAgentInfo, AiAsk, AiEvent, AiId, AiImage, AiLimit, AiMode, AiModel, AiSkill, AiStep, AiStepDone, AiStepKind, AiUsage, ErrorKind, RpcError, ServerMsg, ServerNotify, SkillGroup,
};
use serde_json::Value;
use std::collections::HashMap;
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
            home_dirs: &[".local/bin", ".claude/local", ".npm-global/bin"],
        },
        AiAgent::Codex => Spec {
            exe: "codex",
            install_url: Some("https://chatgpt.com/codex/install.sh"),
            runner: "sh",
            login: &["login", "--device-auth"],
            home_dirs: &[".local/bin", ".codex/bin", ".npm-global/bin"],
        },
        AiAgent::Grok => Spec {
            exe: "grok",
            install_url: Some("https://x.ai/cli/install.sh"),
            runner: "bash",
            login: &["login", "--device-auth"],
            home_dirs: &[".grok/bin", ".local/bin"],
        },
        AiAgent::OpenCode => Spec {
            exe: "opencode",
            install_url: Some("https://opencode.ai/install"),
            runner: "bash",
            login: &[],
            home_dirs: &[".opencode/bin", ".local/bin", ".npm-global/bin", ".bun/bin"],
        },
    }
}

/// Ollama, which comes with OpenCode: its installer (Linux and macOS) and how to run it.
const OLLAMA_URL: &str = "https://ollama.com/install.sh";
const OLLAMA_RUNNER: &str = "sh";

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
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
    let exe = if cfg!(windows) { format!("{exe}.exe") } else { exe.to_string() };
    let on_path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    on_path.into_iter().chain(install_dirs(agent)).map(|d| d.join(&exe)).find(|p| p.is_file())
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
            if env_set("CODEX_API_KEY") {
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
    let model = |id: &str, name: &str| AiModel { id: id.to_string(), name: name.to_string(), local: false, warm: None };
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
                    let name = ollama::name(id.split_once('/').map_or(id, |(_, m)| m));
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
            Some(AiModel { id: id.to_string(), name: name.to_string(), local: false, warm: None })
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
            Some((priority, AiModel { id: id.to_string(), name: name.to_string(), local: false, warm: None }))
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
    AiAgent::ALL
        .iter()
        .map(|&agent| {
            let exe = find(agent);
            let installed = exe.is_some();
            AiAgentInfo {
                agent,
                installed,
                signed_in: if installed { signed_in(agent) } else { None },
                can_install: can_install(agent),
                models: exe.map(|e| models(agent, &e, root)).unwrap_or_default(),
                gpus: if agent == AiAgent::OpenCode { ollama::gpus() } else { Vec::new() },
                ollama: (agent == AiAgent::OpenCode).then(ollama::info),
            }
        })
        .collect()
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
    const SCRIPT: &str = r#"name=$1 exe=$2 url=$3 runner=$4 host=$5 login=$6 xname=$7 xexe=$8 xurl=$9 xrunner=${10}; shift 10
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
  printf '\033[1mSigning %s in on %s.\033[0m Open the link below on any device and follow it.\n\n' "$name" "$host"
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
    cmd.extend(sp.login.iter().map(|s| s.to_string()));
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
            eprintln!("nibd: priming {model}: {e}");
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
    root: PathBuf,
    tx: Sender<ServerMsg>,
    id: AiId,
    /// The assistant's own session id, from the first reply on: the next message continues it.
    resume: Mutex<Option<String>>,
    /// The run answering now.
    child: Mutex<Option<Child>>,
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
    /// Sessions whose processes are frozen (`suspend`).
    frozen: std::collections::HashSet<AiId>,
    /// The agents' mailbox folder (`mail`), once a session has started.
    mail: Option<PathBuf>,
    /// The project the sessions work in: where every letter is kept (`history::letter`).
    root: Option<PathBuf>,
    /// The post office thread is running.
    posting: bool,
    meta: HashMap<AiId, Meta>,
    /// Who last changed each file, and the pairs already told about sharing one.
    edits: HashMap<String, AiId>,
    warned: std::collections::HashSet<(String, AiId, AiId)>,
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
    busy: Arc<AtomicBool>,
    tx: Sender<ServerMsg>,
    /// Writes the conversation down (`history`).
    rec: Arc<Mutex<history::Recorder>>,
}

/// What a session's reader thread tells the rest: that its turn ended, and which files it
/// changed (two agents in one file are told about each other).
#[derive(Clone)]
struct Watch {
    sessions: std::sync::Weak<Mutex<Sessions>>,
    busy: Arc<AtomicBool>,
    rec: Arc<Mutex<history::Recorder>>,
}

impl Watch {
    fn see(&self, id: AiId, event: &AiEvent) {
        self.rec.lock().unwrap().event(event);
        match event {
            AiEvent::TurnDone { .. } | AiEvent::Exit { .. } => self.busy.store(false, Ordering::Relaxed),
            AiEvent::Step(AiStep { kind: AiStepKind::Edit { path, .. } | AiStepKind::Write { path, .. }, .. }) if !path.is_empty() => {
                if let Some(s) = self.sessions.upgrade() {
                    s.lock().unwrap().changed(id, path);
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
            let event = match serde_json::from_str::<nib_proto::AiFinished>(&letter.text) {
                Ok(f) => Some(AiEvent::Finished(f)),
                Err(_) => match serde_json::from_str::<mail::Report>(&letter.text) {
                    Ok(mail::Report::Step(step)) => Some(AiEvent::Step(step)),
                    Ok(mail::Report::StepDone(done)) => Some(AiEvent::StepDone(done)),
                    Err(_) => None,
                },
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

/// Where a session's agent finds its scratch and artifact folders.
pub struct Dirs<'a> {
    pub scratch: Option<&'a Path>,
    pub nibdir: Option<&'a Path>,
}

/// Check that `agent` is there and signed in and start a chat at `root` with `model` (`None`:
/// the one it's set up with); what it says streams to the client as `AiEvent`s.
#[allow(clippy::too_many_arguments)]
pub fn open(sessions: &Shared, agent: AiAgent, model: Option<String>, tools: bool, mode: AiMode, resume: Option<&str>, root: &Path, dirs: Dirs, tx: Sender<ServerMsg>) -> Result<AiId, RpcError> {
    let Some(exe) = find(agent) else {
        return Err(not_ready(agent, ErrorKind::NotFound, "isn't installed"));
    };
    if signed_in(agent) == Some(false) {
        return Err(not_ready(agent, ErrorKind::PermissionDenied, "isn't signed in"));
    }
    if agent == AiAgent::OpenCode && !ollama::pulled().is_empty() {
        // Its local models need the server; a cold model then loads with the first message.
        if let Err(e) = ollama::ensure_running() {
            eprintln!("nibd: ollama: {e}");
        }
    }
    let mut s = sessions.lock().unwrap();
    let id = s.next;
    s.next += 1;
    if s.mail.is_none() {
        s.mail = mail::dir(dirs.scratch);
    }
    if s.root.is_none() {
        s.root = Some(root.to_path_buf());
    }
    if !std::mem::replace(&mut s.posting, true) {
        let weak = Arc::downgrade(sessions);
        thread::spawn(move || post_office(weak));
    }
    // What the agent's commands see: nib's folders (the nib-artifacts skill), and the mailbox.
    let name = mail::name(agent, id);
    let mut env: Vec<(&'static str, OsString)> = Vec::new();
    env.extend(dirs.scratch.map(|p| ("NIB_SCRATCH", p.as_os_str().to_os_string())));
    env.extend(dirs.nibdir.map(|p| ("NIBDIR", p.as_os_str().to_os_string())));
    if let (true, Some(dir), Ok(me)) = (tools, &s.mail, std::env::current_exe()) {
        env.push(("NIB_MAILBOX", dir.as_os_str().to_os_string()));
        env.push(("NIB_AGENT", name.clone().into()));
        env.push(("NIBD", me.into_os_string()));
        // `nib-mail` is in the mailbox's `bin`.
        let mut path = dir.join("bin").into_os_string();
        if let Some(rest) = std::env::var_os("PATH") {
            path.push(":");
            path.push(rest);
        }
        env.push(("PATH", path));
    }
    // A past conversation goes on in its own record, with the assistant's session of it.
    // It also keeps its task and the name made from it.
    let (rec, session, past) = match resume.and_then(|id| history::Recorder::resume(root, id)) {
        Some((rec, session, past)) => (rec, session, Some(past)),
        None => (history::Recorder::new(root, agent, model.as_deref()), None, None),
    };
    let watch = Watch { sessions: Arc::downgrade(sessions), busy: Arc::new(AtomicBool::new(false)), rec: Arc::new(Mutex::new(rec)) };
    let session = match agent {
        AiAgent::Claude => open_claude(&exe, model.as_deref(), tools, mode, session.as_deref(), root, &env, watch.clone(), id, tx.clone())?,
        AiAgent::Codex | AiAgent::Grok | AiAgent::OpenCode => Session::PerTurn(Arc::new(PerTurn {
            agent,
            tools,
            mode: Mutex::new(mode),
            env,
            watch: watch.clone(),
            mail: Mutex::new(Vec::new()),
            exe,
            local: agent == AiAgent::OpenCode && model.as_deref().is_some_and(|m| ollama::pulled().contains(&ollama_name(m))),
            cost: Mutex::new(0.0),
            rollout: Mutex::new(None),
            model: model.clone(),
            root: root.to_path_buf(),
            tx: tx.clone(),
            id,
            resume: Mutex::new(session),
            child: Mutex::new(None),
            carry: Mutex::new(None),
        })),
    };
    s.map.insert(id, session);
    if tools {
        let taken: Vec<&String> = s.meta.values().map(|m| &m.name).collect();
        let (task, called) = match past {
            Some(p) => (p.title, p.name.filter(|n| !taken.contains(&n)).unwrap_or_else(|| name.clone())),
            None => (String::new(), name.clone()),
        };
        if called != name {
            let _ = tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id, event: AiEvent::Name(called.clone()) }));
        }
        s.meta.insert(id, Meta { id_name: name, name: called, agent, model, task, busy: watch.busy, tx, rec: watch.rec });
    }
    Ok(id)
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
fn open_claude(exe: &Path, model: Option<&str>, tools: bool, mode: AiMode, resume: Option<&str>, root: &Path, env: &[(&'static str, OsString)], watch: Watch, id: AiId, tx: Sender<ServerMsg>) -> Result<Session, RpcError> {
    let mut cmd = Command::new(exe);
    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
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
        .args(if tools { vec!["--allowedTools", "Bash(nib-mail:*)"] } else { vec![] })
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
                let blocks = v.pointer("/message/content").and_then(Value::as_array);
                blocks
                    .into_iter()
                    .flatten()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    .filter_map(|b| {
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
            // Its mode makes it ask before this tool call: the user answers (`answer`).
            Some("control_request") if v.pointer("/request/subtype").and_then(Value::as_str) == Some("can_use_tool") => {
                let (Some(request), Some(r)) = (s(v, "request_id"), v.get("request")) else { return vec![] };
                let tool = s(r, "tool_name").unwrap_or_default();
                let input = r.get("input").cloned().unwrap_or(Value::Null);
                let question = tool == "AskUserQuestion";
                let what = if question {
                    claude_questions(&input)
                } else {
                    // A plan to approve, or what the harness says the call is.
                    s(&input, "plan").or_else(|| s(r, "description")).unwrap_or_default()
                };
                self.asks.lock().unwrap().insert(request.clone(), input);
                vec![AiEvent::Ask(AiAsk { request, step: s(r, "tool_use_id"), tool, what, question })]
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

/// The questions of an `AskUserQuestion` call as text: each with its options.
fn claude_questions(input: &Value) -> String {
    let mut out = String::new();
    for q in input.get("questions").and_then(Value::as_array).into_iter().flatten() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(q.get("question").and_then(Value::as_str).unwrap_or(""));
        for (i, o) in q.get("options").and_then(Value::as_array).into_iter().flatten().enumerate() {
            let label = o.get("label").and_then(Value::as_str).unwrap_or("");
            let about = o.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()).map(|d| format!(": {d}")).unwrap_or_default();
            out.push_str(&format!("\n  {}. {label}{about}", i + 1));
        }
    }
    out
}

/// nib's own skills as a Claude Code plugin for one session: a folder under nib's cache with a
/// link to each skill that ships next to this binary (in a checkout: the nearest `.skills`
/// above it). `None` when there are none here (a daemon deployed without them).
fn skills_plugin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let src = exe.ancestors().skip(1).map(|d| d.join(".skills")).find(|d| d.is_dir())?;
    let skills = nib_proto::skills::list(&src, SkillGroup::Nib);
    link_plugin(&crate::places::xdg("XDG_CACHE_HOME", &[".cache"]).join("plugin").join("nib"), &skills)
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
    let links = dir.join("skills");
    let _ = std::fs::remove_dir_all(&links);
    std::fs::create_dir_all(&links).ok()?;
    for s in skills {
        let folder = Path::new(&s.path).parent()?;
        std::os::unix::fs::symlink(folder, links.join(folder.file_name()?)).ok()?;
    }
    Some(dir.to_path_buf())
}

#[cfg(not(unix))]
fn link_plugin(_dir: &Path, _skills: &[AiSkill]) -> Option<PathBuf> {
    None
}

/// Base64, for pictures in a message.
fn base64(data: &[u8]) -> String {
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

/// Pictures for an assistant that takes them as files: saved under the temp dir, named by run.
fn image_files(images: &[AiImage]) -> Vec<PathBuf> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("nibd-images-{}", std::process::id()));
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

/// The arguments for one Codex / Grok / OpenCode run. Grok has no model list, so no `model`.
fn one_shot_args(agent: AiAgent, model: Option<&str>, resume: Option<&str>, mode: AiMode, images: &[PathBuf], text: &str) -> Vec<String> {
    let files = |flag: &str| images.iter().flat_map(|p| [flag.to_string(), p.to_string_lossy().into_owned()]).collect::<Vec<_>>();
    let acts = matches!(mode, AiMode::AcceptEdits | AiMode::Auto);
    let mut a: Vec<String> = Vec::new();
    match agent {
        AiAgent::Codex => {
            // Its sandbox: look at the project only, or work in it (edits and commands, nothing
            // outside, no network). A run can't ask, so there is nothing between the two.
            a.extend(["exec", "--json", "--skip-git-repo-check", "--sandbox", if acts { "workspace-write" } else { "read-only" }].map(String::from));
            a.extend(files("--image"));
            if let Some(m) = model {
                a.extend(["--model".to_string(), m.to_string()]);
            }
            if let Some(id) = resume {
                a.extend(["resume".to_string(), id.to_string()]);
            }
            a.extend(["--".to_string(), text.to_string()]);
        }
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
        AiAgent::Claude => unreachable!("Claude Code chats over one process"),
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
        let send = |event| {
            let _ = self.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id: self.id, event }));
        };
        let resume = self.resume.lock().unwrap().clone();
        let mut cmd = Command::new(&self.exe);
        if self.agent == AiAgent::OpenCode {
            let config = opencode_config(opencode_permissions(mode), self.model.as_deref().unwrap_or(""), &ollama::GpuSel::All);
            cmd.env("OPENCODE_CONFIG_CONTENT", config);
            if self.local {
                opencode_offline(&mut cmd);
            }
        }
        let spawned = cmd
            .envs(self.env.iter().map(|(k, v)| (*k, v.as_os_str())))
            .args(one_shot_args(self.agent, self.model.as_deref(), resume.as_deref(), mode, images, text))
            .current_dir(&self.root)
            // Codex adds piped-in text to the prompt; there is none.
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
                let model = self.model.as_deref().map(ollama_name);
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
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// OpenCode's model id (`ollama/qwen3:8b`) as Ollama names it.
fn ollama_name(model: &str) -> String {
    ollama::name(model.split_once('/').map_or(model, |(_, m)| m))
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
            text.push_str(&format!(
                "\n\n(nib: you are {}. Other agents share this working tree right now:\n{}\nIf you will change files, first run `nib-mail who` (it lists the files each agent changed) and write only to the agents whose files or task overlap yours, by name (nib-agent-mailbox skill). If none do, write to nobody.)",
                me.name,
                list.join("\n")
            ));
        }
        if s.mail.is_some() {
            text.push_str("\n\n(nib: if this is a clear job with an end, then when you have done it and checked it, run `nib-mail done` as the last thing you do (nib-finished skill), so the user sees it is finished.)");
        }
    }
    // Letters that waited in its inbox while it was idle come with it.
    if let (Some(dir), Some(m)) = (&s.mail, s.meta.get(&id)) {
        let waited = mail::said(&mail::unread(dir, &m.id_name, None));
        if !waited.is_empty() {
            text.push_str(&format!("\n\n(nib: mail that came while you were idle)\n{waited}"));
        }
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
pub fn answer(sessions: &Shared, id: AiId, request: &str, allow: bool, message: Option<&str>) {
    let mut s = sessions.lock().unwrap();
    let Some(Session::Stream { stdin, asks, .. }) = s.map.get_mut(&id) else { return };
    let Some(input) = asks.lock().unwrap().remove(request) else { return };
    let response = if allow {
        serde_json::json!({ "behavior": "allow", "updatedInput": input })
    } else {
        serde_json::json!({ "behavior": "deny", "message": message.filter(|m| !m.trim().is_empty()).unwrap_or("The user refused this.") })
    };
    let msg = serde_json::json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request, "response": response } });
    let _ = writeln!(stdin, "{msg}").and_then(|_| stdin.flush());
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
    let (session, frozen) = {
        let mut s = sessions.lock().unwrap();
        s.meta.remove(&id);
        (s.map.remove(&id), s.frozen.remove(&id))
    };
    if let Some(s) = session {
        // Killing a frozen assistant would leave the commands it started frozen for good.
        if let (true, Some(pid)) = (frozen, pid(&s)) {
            let _ = signal_tree(pid, "CONT");
        }
        end(s);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn claude_asks_leave_and_reports_its_mode() {
        let mut t = Turn::default();
        let ask = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/p/a.txt","content":"hi"},"description":"a.txt","tool_use_id":"t1"}}"#;
        assert_eq!(read(&mut t, ask), [AiEvent::Ask(AiAsk { request: "r1".into(), step: Some("t1".into()), tool: "Write".into(), what: "a.txt".into(), question: false })]);
        assert_eq!(t.asks.lock().unwrap()["r1"]["content"], "hi", "the input is kept: it goes back with a yes");
        let plan = r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{"plan":"1. Do it"},"tool_use_id":"t2"}}"#;
        assert!(matches!(&read(&mut t, plan)[..], [AiEvent::Ask(a)] if a.what == "1. Do it" && !a.question));
        let q = r#"{"type":"control_request","request_id":"r3","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Rename it too?","options":[{"label":"Yes","description":"keep an alias"},{"label":"No"}]}]}}}"#;
        assert!(matches!(&read(&mut t, q)[..], [AiEvent::Ask(a)] if a.question && a.what == "Rename it too?\n  1. Yes: keep an alias\n  2. No"));
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
        let sandbox = |m| one_shot_args(AiAgent::Codex, None, None, m, &[], "hi")[4].clone();
        assert_eq!([sandbox(AiMode::Manual), sandbox(AiMode::Plan), sandbox(AiMode::AcceptEdits), sandbox(AiMode::Auto)], ["read-only", "read-only", "workspace-write", "workspace-write"]);
        assert_eq!(opencode_permissions(AiMode::Plan), read_only_permissions());
        assert_eq!(opencode_permissions(AiMode::AcceptEdits)["bash"], "deny");
        assert_eq!((&opencode_permissions(AiMode::Auto)["edit"], &opencode_permissions(AiMode::Auto)["bash"]), (&Value::from("allow"), &Value::from("allow")));
        assert_eq!(opencode_permissions(AiMode::Auto)["external_directory"], "deny");
        // Pictures: files for Codex and OpenCode, a mention for Grok.
        let img = [PathBuf::from("/t/1.png")];
        assert_eq!(&one_shot_args(AiAgent::Codex, None, None, AiMode::Plan, &img, "hi")[5..], ["--image", "/t/1.png", "--", "hi"]);
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
        // name exe url runner host login, then Ollama's, then no sign-in arguments.
        assert_eq!(cmd[4..8], ["OpenCode", "opencode", "https://opencode.ai/install", "bash"]);
        assert_eq!(cmd[9], "no", "OpenCode has no sign-in");
        assert_eq!(cmd[10..], ["Ollama", "ollama", OLLAMA_URL, OLLAMA_RUNNER]);
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
        let first = one_shot_args(AiAgent::Codex, None, None, AiMode::Plan, &[], "hi");
        assert_eq!(first, ["exec", "--json", "--skip-git-repo-check", "--sandbox", "read-only", "--", "hi"]);
        let next = one_shot_args(AiAgent::Codex, None, Some("t1"), AiMode::Plan, &[], "-v?");
        assert_eq!(&next[5..], ["resume", "t1", "--", "-v?"]);
        let grok = one_shot_args(AiAgent::Grok, None, Some("s1"), AiMode::Plan, &[], "-x");
        assert_eq!(&grok[4..], ["--resume", "s1", "-p", " -x"]);
        assert_eq!(one_shot_args(AiAgent::OpenCode, None, None, AiMode::Plan, &[], "hi"), ["run", "--format", "json", "--", "hi"]);
        assert_eq!(&one_shot_args(AiAgent::OpenCode, None, Some("ses_1"), AiMode::Plan, &[], "-x")[3..], ["--session", "ses_1", "--", "-x"]);
    }

    #[test]
    fn the_chosen_model_goes_on_every_run() {
        let codex = one_shot_args(AiAgent::Codex, Some("gpt-6-sol"), Some("t1"), AiMode::Plan, &[], "hi");
        assert_eq!(&codex[5..], ["--model", "gpt-6-sol", "resume", "t1", "--", "hi"]);
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
            {"slug":"a","display_name":"A","visibility":"list","priority":1}]}"#;
        let ids: Vec<_> = codex_models(cache).into_iter().map(|m| (m.id, m.name)).collect();
        assert_eq!(ids, [("a".to_string(), "A".to_string()), ("b".into(), "B".into())]);
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
            let (mut cmd, _) = setup_cmd(AiAgent::Codex, false).unwrap();
            // Same script; the stand-in assistant, not installed and not signed in.
            cmd[5] = "fakeai".into();
            cmd[9] = "yes".into();
            let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
            let out = Command::new(&cmd[0]).args(&cmd[1..]).env("PATH", path).stdin(Stdio::null()).output().unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{stdout}");
            assert!(stdout.contains("Installing Codex"), "{stdout}");
            assert!(stdout.contains("signed-in login --device-auth"), "{stdout}");
            assert!(stdout.contains("Codex is ready"), "{stdout}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
