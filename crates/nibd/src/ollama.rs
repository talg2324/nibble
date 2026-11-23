//! Ollama, which runs OpenCode's local models: which models it has the weights of (read from
//! disk), which are loaded in memory ("warm": they answer at once), and loading / unloading one.
//! Its HTTP API is a few small JSON calls to a local server, so a plain `TcpStream` does them.
//! The server itself is started here when it isn't running. Models are downloaded (pulled) and
//! deleted through it too; what each takes on disk is read from its manifest.

use nib_proto::{AiGpu, OllamaInfo, OllamaModel};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long a quick question (is it up, what's loaded) may take.
const QUICK: Duration = Duration::from_millis(800);
/// Loading a big model from disk can take minutes.
const LOAD: Duration = Duration::from_secs(600);
/// How long a server nib started gets to come up.
const START: Duration = Duration::from_secs(20);

fn home() -> Option<PathBuf> {
    nib_proto::path::home_dir()
}

/// Where Ollama keeps its models: `$OLLAMA_MODELS`, the user's, or a Linux service install's.
fn dirs() -> Vec<PathBuf> {
    if let Some(d) = std::env::var_os("OLLAMA_MODELS").filter(|d| !d.is_empty()) {
        return vec![PathBuf::from(d)];
    }
    let mut dirs: Vec<PathBuf> = home().map(|h| h.join(".ollama/models")).into_iter().collect();
    dirs.push(PathBuf::from("/usr/share/ollama/.ollama/models"));
    dirs
}

/// The models Ollama has the weights of on this machine.
pub fn pulled() -> HashSet<String> {
    manifests_in(&dirs()).into_keys().collect()
}

/// Ollama here: installed or not, and its downloaded models with what each takes on disk.
pub fn info() -> OllamaInfo {
    let manifests = manifests_in(&dirs());
    let loaded = if manifests.is_empty() { HashSet::new() } else { loaded() };
    let (mut models, disk_bytes) = sizes(&manifests);
    for m in &mut models {
        m.warm = loaded.contains(&m.name);
    }
    OllamaInfo { installed: exe().is_some(), models, disk_bytes }
}

/// Each model's size (its manifest's config and layers), by name; and all of them together, a
/// file shared by two models counted once.
fn sizes(manifests: &HashMap<String, PathBuf>) -> (Vec<OllamaModel>, u64) {
    let mut seen: HashMap<String, u64> = HashMap::new();
    let mut models: Vec<OllamaModel> = manifests
        .iter()
        .map(|(name, path)| {
            let v: serde_json::Value = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            let blobs = v.get("config").into_iter().chain(v.get("layers").and_then(|l| l.as_array()).into_iter().flatten());
            let mut bytes = 0;
            for b in blobs {
                let size = b.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                bytes += size;
                if let Some(d) = b.get("digest").and_then(|d| d.as_str()) {
                    seen.insert(d.to_string(), size);
                }
            }
            OllamaModel { name: name.clone(), bytes, warm: false }
        })
        .collect();
    models.sort_by(|a, b| a.name.cmp(&b.name));
    (models, seen.values().sum())
}

/// The models whose weights are in `dirs`, as `name:tag` -> its manifest, at
/// `manifests/<registry>/<namespace>/<name>/<tag>`. Read from disk, so the server needn't be
/// running. Its cloud models (`…-cloud` tags) have a manifest but no weights: left out.
fn manifests_in(dirs: &[PathBuf]) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    let entries = |p: &Path| std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).collect::<Vec<_>>();
    let file = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for dir in dirs {
        for registry in entries(&dir.join("manifests")).into_iter().filter(|p| p.is_dir()) {
            for ns in entries(&registry).into_iter().filter(|p| p.is_dir()) {
                for model in entries(&ns).into_iter().filter(|p| p.is_dir()) {
                    for tag in entries(&model).into_iter().filter(|p| p.is_file()) {
                        let (r, n, m, t) = (file(&registry), file(&ns), file(&model), file(&tag));
                        if t.contains("cloud") {
                            continue;
                        }
                        let path = match (r.as_str(), n.as_str()) {
                            ("registry.ollama.ai", "library") => m,
                            ("registry.ollama.ai", _) => format!("{n}/{m}"),
                            _ => format!("{r}/{n}/{m}"),
                        };
                        out.entry(format!("{path}:{t}")).or_insert(tag);
                    }
                }
            }
        }
    }
    out
}

/// A model as Ollama names it, with its tag (`qwen3` is `qwen3:latest`).
pub fn name(model: &str) -> String {
    let tagged = model.rsplit('/').next().is_some_and(|last| last.contains(':'));
    if tagged { model.to_string() } else { format!("{model}:latest") }
}

/// The model of an OpenCode model id (`provider/model`: `ollama/qwen3:8b`), as Ollama is asked
/// for it: whatever the user named the provider, it is dropped. What starts with a registry's
/// host (`hf.co/user/model:q4`, a name Ollama takes as it is) or has no `/` is given back whole.
pub fn model_of(id: &str) -> &str {
    let id = id.trim();
    match id.split_once('/') {
        Some((first, rest)) if !first.contains(['.', ':']) => rest,
        _ => id,
    }
}

/// [`name`] of [`model_of`]: the name Ollama lists an OpenCode model id's weights under.
pub fn name_of(id: &str) -> String {
    name(model_of(id))
}

/// Which GPUs a local model runs on. Ollama picks its GPUs when its server starts, so each
/// selection other than all of them gets a server of its own, on a port of its own, that sees
/// only those GPUs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GpuSel {
    /// The user's own server, with every GPU.
    All,
    Cpu,
    /// These GPUs (`AiGpu::index`), sorted.
    Devices(Vec<u32>),
}

impl GpuSel {
    /// `"all"` (or nothing), `"cpu"`, or GPU indices: `"1"`, `"0,2"`. Checked against this
    /// machine's GPUs; a selection of every GPU there is is `All`.
    pub fn parse(s: Option<&str>) -> Result<Self, String> {
        let s = s.map(str::trim).unwrap_or("").to_ascii_lowercase();
        if s.is_empty() || s == "all" {
            return Ok(GpuSel::All);
        }
        let have = gpus();
        let apple = have.iter().any(|g| g.kind == "apple");
        if s == "cpu" {
            if apple {
                return Err("on Apple Silicon Ollama always runs on the GPU: there's no CPU-only server".into());
            }
            return Ok(GpuSel::Cpu);
        }
        let mut ids = Vec::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let id: u32 = part.parse().map_err(|_| format!("bad GPU \"{part}\": use all, cpu, or indices like 0,1"))?;
            if !have.iter().any(|g| g.index == id) {
                let known: Vec<String> = have.iter().map(|g| format!("{} ({})", g.index, g.name)).collect();
                return Err(format!("no GPU {id} here; GPUs: {}", if known.is_empty() { "none".into() } else { known.join(", ") }));
            }
            ids.push(id);
        }
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() || ids.len() == have.len() {
            return Ok(GpuSel::All);
        }
        Ok(GpuSel::Devices(ids))
    }

    /// How a result names it: `all`, `cpu`, `0,2`.
    pub fn label(&self) -> String {
        match self {
            GpuSel::All => "all".into(),
            GpuSel::Cpu => "cpu".into(),
            GpuSel::Devices(ids) => ids.iter().map(u32::to_string).collect::<Vec<_>>().join(","),
        }
    }

    /// The port of its own server: fixed per selection, so a later nibd finds it again.
    fn port(&self) -> Option<u16> {
        match self {
            GpuSel::All => None,
            GpuSel::Cpu => Some(11499),
            GpuSel::Devices(ids) => {
                let mask: u64 = ids.iter().fold(0, |m, i| m | (1u64 << (i % 64)));
                Some(11500 + (mask % 400) as u16)
            }
        }
    }

    /// What its server sees: only these GPUs, in the order `nvidia-smi` lists them.
    fn env(&self) -> Vec<(&'static str, String)> {
        let list = match self {
            GpuSel::All => return Vec::new(),
            // An id no GPU has: Ollama then runs on the CPU.
            GpuSel::Cpu => "-1".to_string(),
            GpuSel::Devices(_) => self.label(),
        };
        vec![
            ("CUDA_DEVICE_ORDER", "PCI_BUS_ID".into()),
            ("CUDA_VISIBLE_DEVICES", list.clone()),
            ("ROCR_VISIBLE_DEVICES", list.clone()),
            ("GGML_VK_VISIBLE_DEVICES", list),
        ]
    }
}

/// The user's server: `$OLLAMA_HOST` (`host`, `host:port` or a URL), else 127.0.0.1:11434.
fn default_addr() -> Option<SocketAddr> {
    let host = std::env::var("OLLAMA_HOST").unwrap_or_default();
    let host = host.trim().trim_start_matches("http://").trim_end_matches('/');
    let host = match host {
        "" => "127.0.0.1:11434".to_string(),
        h if h.contains(':') => h.to_string(),
        h => format!("{h}:11434"),
    };
    // Listening on every interface: reach it on this one.
    let host = host.replace("0.0.0.0", "127.0.0.1");
    host.to_socket_addrs().ok()?.next()
}

/// Where the server for `sel` listens.
pub fn addr_of(sel: &GpuSel) -> Option<SocketAddr> {
    match sel.port() {
        None => default_addr(),
        Some(port) => Some(SocketAddr::from(([127, 0, 0, 1], port))),
    }
}

/// OpenCode's address for that server (its OpenAI-compatible API).
pub fn base_url(addr: SocketAddr) -> String {
    format!("http://{addr}/v1")
}

/// One HTTP/1.1 request -> (status, body).
fn http(addr: SocketAddr, method: &str, path: &str, body: Option<&str>, timeout: Duration) -> io::Result<(u16, String)> {
    let mut s = TcpStream::connect_timeout(&addr, QUICK)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(QUICK * 10))?;
    let body = body.unwrap_or("");
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let raw = String::from_utf8_lossy(&raw);
    let (head, rest) = raw.split_once("\r\n\r\n").ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no HTTP header"))?;
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let chunked = head.lines().any(|l| l.to_ascii_lowercase().replace(' ', "") == "transfer-encoding:chunked");
    Ok((status, if chunked { unchunk(rest) } else { rest.to_string() }))
}

/// A chunked body, joined.
fn unchunk(mut s: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = s.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16) else { break };
        if n == 0 || rest.len() < n {
            break;
        }
        out.push_str(&rest[..n]);
        s = rest[n..].trim_start_matches("\r\n");
    }
    out
}

/// One HTTP/1.1 request whose answer is JSON lines as they come (Ollama's streamed replies):
/// `on_line` gets each. `idle`: how long it may go quiet. -> its status, and the body when that
/// isn't 200.
fn http_lines(addr: SocketAddr, method: &str, path: &str, body: &str, idle: Duration, mut on_line: impl FnMut(&str)) -> io::Result<(u16, String)> {
    let mut s = TcpStream::connect_timeout(&addr, QUICK)?;
    s.set_read_timeout(Some(idle))?;
    s.set_write_timeout(Some(QUICK * 10))?;
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut r = BufReader::new(s);
    let mut head = Vec::new();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no HTTP header"));
        }
        if line == "\r\n" {
            break;
        }
        head.push(line.trim_end().to_string());
    }
    let status = head.first().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let chunked = head.iter().any(|l| l.to_ascii_lowercase().replace(' ', "") == "transfer-encoding:chunked");
    let mut pending = Vec::new();
    let mut all = Vec::new();
    let mut take = |bytes: &[u8], pending: &mut Vec<u8>| {
        all.extend_from_slice(bytes);
        pending.extend_from_slice(bytes);
        while let Some(i) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line);
            if !line.trim().is_empty() {
                on_line(line.trim());
            }
        }
    };
    if chunked {
        loop {
            let mut size = String::new();
            if r.read_line(&mut size)? == 0 {
                break;
            }
            let Ok(n) = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16) else { break };
            if n == 0 {
                break;
            }
            let mut chunk = vec![0; n];
            r.read_exact(&mut chunk)?;
            let mut crlf = [0; 2];
            r.read_exact(&mut crlf)?;
            take(&chunk, &mut pending);
        }
    } else {
        let mut buf = [0; 8192];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            take(&buf[..n], &mut pending);
        }
    }
    drop(take);
    if !pending.is_empty() {
        let rest = std::mem::take(&mut pending);
        let line = String::from_utf8_lossy(&rest);
        if !line.trim().is_empty() {
            on_line(line.trim());
        }
    }
    Ok((status, if status == 200 { String::new() } else { String::from_utf8_lossy(&all).into_owned() }))
}

/// A model name as `ollama pull` takes it: OpenCode's `ollama/` in front is dropped. Only that
/// provider, unlike [`model_of`]: here `someone/tiny` is a model of Ollama's, not a provider's.
fn bare(model: &str) -> &str {
    model.trim().strip_prefix("ollama/").unwrap_or(model.trim())
}

/// Download `model` (`qwen3:8b`) on the user's server, started if it has to be. `progress`
/// gets Ollama's own lines (`{"status", "digest", "total", "completed"}`), at most a few a
/// second: each new status, and every 250 ms in between.
pub fn pull(model: &str, mut progress: impl FnMut(&serde_json::Value)) -> Result<(), String> {
    let model = bare(model);
    if model.is_empty() || model.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("\"{model}\" isn't a model name (like qwen3:8b)"));
    }
    let addr = server(&GpuSel::All)?;
    let body = serde_json::json!({ "model": model, "stream": true }).to_string();
    let (mut last_status, mut last_at) = (String::new(), Instant::now());
    let mut failed = None;
    let result = http_lines(addr, "POST", "/api/pull", &body, Duration::from_secs(300), |line| {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            failed = Some(e.to_string());
            return;
        }
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string();
        if status != last_status || last_at.elapsed() >= Duration::from_millis(250) {
            progress(&v);
            last_status = status;
            last_at = Instant::now();
        }
    });
    match result {
        _ if failed.is_some() => Err(failed.unwrap_or_default()),
        Ok((200, _)) => Ok(()),
        Ok((code, body)) => Err(error_of(code, &body)),
        Err(e) => Err(format!("downloading {model}: {e}")),
    }
}

/// Delete `model`'s weights (files no other model uses) on the user's server, started if it
/// has to be.
pub fn delete(model: &str) -> Result<(), String> {
    let model = bare(model);
    let addr = server(&GpuSel::All)?;
    let body = serde_json::json!({ "model": model }).to_string();
    match http(addr, "DELETE", "/api/delete", Some(&body), QUICK * 30) {
        Ok((200, _)) => Ok(()),
        Ok((404, _)) => Err(format!("Ollama has no model {model}")),
        Ok((code, body)) => Err(error_of(code, &body)),
        Err(e) => Err(format!("talking to Ollama: {e}")),
    }
}

fn get_json(addr: SocketAddr, path: &str) -> Option<serde_json::Value> {
    match http(addr, "GET", path, None, QUICK) {
        Ok((200, body)) => serde_json::from_str(&body).ok(),
        _ => None,
    }
}

/// The server answers.
fn running(addr: SocketAddr) -> bool {
    matches!(http(addr, "GET", "/api/version", None, QUICK), Ok((200, _)))
}

/// Its version, for provenance.
pub fn version(addr: SocketAddr) -> Option<String> {
    get_json(addr, "/api/version")?.get("version")?.as_str().map(str::to_string)
}

/// The digest of `model`'s weights there, which pins exactly what ran.
pub fn digest(addr: SocketAddr, model: &str) -> Option<String> {
    let want = name(model);
    get_json(addr, "/api/tags")?.get("models")?.as_array()?.iter().find_map(|m| {
        let n = m.get("name").or_else(|| m.get("model")).and_then(|n| n.as_str()).map(name)?;
        if n == want { m.get("digest")?.as_str().map(str::to_string) } else { None }
    })
}

fn ps(addr: SocketAddr) -> Vec<serde_json::Value> {
    get_json(addr, "/api/ps").and_then(|v| v.get("models")?.as_array().cloned()).unwrap_or_default()
}

/// The models loaded in memory now on the user's server. Empty when it isn't running.
pub fn loaded() -> HashSet<String> {
    let Some(addr) = default_addr() else { return HashSet::new() };
    ps(addr).iter().filter_map(|m| m.get("name").or_else(|| m.get("model")).and_then(|n| n.as_str()).map(name)).collect()
}

/// How many tokens of context `model` (`name:tag`) has as loaded now (Ollama's `num_ctx`,
/// which may be less than the model can take). `None` when it isn't loaded.
pub fn context_length(model: &str) -> Option<u64> {
    let addr = default_addr()?;
    ps(addr).iter().find_map(|m| {
        let n = m.get("name").or_else(|| m.get("model")).and_then(|n| n.as_str()).map(name)?;
        if n == model { m.get("context_length")?.as_u64() } else { None }
    })
}

/// Ollama's binary, if it's installed here.
pub fn exe() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "ollama.exe" } else { "ollama" };
    let on_path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    let more = ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/Applications/Ollama.app/Contents/Resources"].map(PathBuf::from);
    on_path.into_iter().chain(more).map(|d| d.join(exe)).find(|p| p.is_file())
}

/// The user's server, started if it isn't running.
pub fn ensure_running() -> Result<(), String> {
    server(&GpuSel::All).map(|_| ())
}

/// The server for `sel`, started if it isn't running, and wait for it. It outlives nibd: the
/// user's own Ollama is used by other programs too, and a GPU's server keeps its loaded models
/// for the next job.
pub fn server(sel: &GpuSel) -> Result<SocketAddr, String> {
    let addr = addr_of(sel).ok_or("bad OLLAMA_HOST")?;
    if running(addr) {
        return Ok(addr);
    }
    let exe = exe().ok_or("Ollama isn't installed on this machine (get it from ollama.com)")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("serve").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if sel.port().is_some() {
        cmd.env("OLLAMA_HOST", addr.to_string());
        for (k, v) in sel.env() {
            cmd.env(k, v);
        }
    }
    #[cfg(unix)]
    {
        // Its own process group: closing nib's session doesn't stop it.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("starting {}: {e}", exe.display()))?;
    let start = Instant::now();
    while start.elapsed() < START {
        if running(addr) {
            // Reaped by its own thread when it exits, so it never lingers as a zombie.
            thread::spawn(move || child.wait());
            return Ok(addr);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("ollama serve exited ({status})"));
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err("Ollama didn't start in time".into())
}

fn error_of(code: u16, body: &str) -> String {
    let why = serde_json::from_str::<serde_json::Value>(body).ok().and_then(|v| v.get("error")?.as_str().map(str::to_string));
    why.unwrap_or_else(|| format!("Ollama answered {code}"))
}

/// Load `model` into memory on `sel`'s server and keep it there ("warm"), or unload it
/// ("cold"). Warming starts the server if it has to; cooling one that isn't running is done.
pub fn set_warm(model: &str, warm: bool, sel: &GpuSel) -> Result<(), String> {
    let addr = if warm {
        server(sel)?
    } else {
        match addr_of(sel) {
            Some(a) if running(a) => a,
            _ => return Ok(()),
        }
    };
    let body = serde_json::json!({ "model": name(model), "keep_alive": if warm { -1 } else { 0 }, "stream": false }).to_string();
    match http(addr, "POST", "/api/generate", Some(&body), if warm { LOAD } else { QUICK * 10 }) {
        Ok((200, _)) => Ok(()),
        Ok((code, body)) => Err(error_of(code, &body)),
        Err(e) => Err(format!("talking to Ollama: {e}")),
    }
}

/// One `/api/chat` call (not streamed) -> Ollama's answer. `timeout`: the whole reply.
pub fn chat(addr: SocketAddr, body: &serde_json::Value, timeout: Duration) -> Result<serde_json::Value, String> {
    match http(addr, "POST", "/api/chat", Some(&body.to_string()), timeout) {
        Ok((200, text)) => serde_json::from_str(&text).map_err(|e| format!("Ollama's answer isn't JSON: {e}")),
        Ok((code, text)) => Err(error_of(code, &text)),
        Err(e) => Err(format!("talking to Ollama: {e}")),
    }
}

/// The GPUs that can run local models here, in the order their drivers list them (what a
/// `gpu` selection's indices mean). Empty: only the CPU. Apple Silicon (Metal, MLX): one; an
/// NVIDIA GPU (via `nvidia-smi`, Linux, WSL and Windows); an AMD GPU (ROCm's topology on Linux,
/// the driver on Windows).
pub fn gpus() -> Vec<AiGpu> {
    let gpu = |index, name: &str, memory_mb, kind: &str| AiGpu { index, name: name.to_string(), memory_mb, kind: kind.to_string() };
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        let sysctl = |k: &str| {
            let mut c = Command::new("sysctl");
            c.args(["-n", k]);
            crate::ai::run_for(c, QUICK * 3).map(|s| s.trim().to_string())
        };
        let chip = sysctl("machdep.cpu.brand_string").unwrap_or_else(|| "Apple Silicon".into());
        let mem = sysctl("hw.memsize").and_then(|m| m.parse::<u64>().ok()).map(|b| b / (1024 * 1024));
        return vec![gpu(0, &format!("{chip} GPU (Metal, MLX)"), mem, "apple")];
    }
    if cfg!(target_os = "macos") {
        // An Intel Mac: Ollama runs on the CPU there, and MLX needs Apple Silicon.
        return Vec::new();
    }
    if let Some(smi) = nvidia_smi() {
        let mut c = Command::new(smi);
        c.args(["--query-gpu=index,name,memory.total", "--format=csv,noheader,nounits"]);
        let listed: Vec<AiGpu> = crate::ai::run_for(c, QUICK * 6)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut f = l.split(',').map(str::trim);
                let index = f.next()?.parse().ok()?;
                let name = f.next()?.to_string();
                let mem = f.next().and_then(|m| m.parse().ok());
                Some(gpu(index, &name, mem, "nvidia"))
            })
            .collect();
        if !listed.is_empty() {
            return listed;
        }
        return vec![gpu(0, "NVIDIA GPU", None, "nvidia")];
    }
    if cfg!(windows) {
        let sys = system32();
        let hip = std::fs::read_dir(&sys).into_iter().flatten().flatten().any(|e| e.file_name().to_string_lossy().to_lowercase().starts_with("amdhip64"));
        if hip || sys.join("atiadlxx.dll").is_file() {
            return vec![gpu(0, "AMD GPU", None, "amd")];
        }
        return Vec::new();
    }
    if Path::new("/dev/kfd").exists() {
        return amd_gpus();
    }
    Vec::new()
}

#[cfg(windows)]
fn system32() -> PathBuf {
    std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from).join("System32")
}

#[cfg(not(windows))]
fn system32() -> PathBuf {
    PathBuf::new()
}

/// `nvidia-smi`: on `PATH`, WSL's copy of the Windows driver's, or the Windows driver's own.
fn nvidia_smi() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "nvidia-smi.exe" } else { "nvidia-smi" };
    let on_path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    let mut dirs = on_path;
    dirs.push(PathBuf::from("/usr/lib/wsl/lib"));
    if cfg!(windows) {
        dirs.push(system32());
        dirs.push(PathBuf::from(r"C:\Program Files\NVIDIA Corporation\NVSMI"));
    }
    dirs.into_iter().map(|d| d.join(exe)).find(|p| p.is_file())
}

/// AMD GPUs from ROCm's topology: the nodes with compute units, in ROCm's order.
fn amd_gpus() -> Vec<AiGpu> {
    let nodes = Path::new("/sys/class/kfd/kfd/topology/nodes");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(nodes).into_iter().flatten().flatten().map(|e| e.path()).collect();
    dirs.sort_by_key(|d| d.file_name().and_then(|n| n.to_str()?.parse::<u32>().ok()).unwrap_or(u32::MAX));
    let prop = |text: &str, key: &str| text.lines().find_map(|l| l.strip_prefix(key)?.trim().parse::<u64>().ok());
    let mut out = Vec::new();
    for d in dirs {
        let props = std::fs::read_to_string(d.join("properties")).unwrap_or_default();
        if prop(&props, "simd_count ").unwrap_or(0) == 0 {
            continue;
        }
        let name = std::fs::read_to_string(d.join("name")).map(|n| n.trim().to_string()).ok().filter(|n| !n.is_empty());
        let mem = std::fs::read_to_string(d.join("mem_banks/0/properties")).ok().and_then(|p| prop(&p, "size_in_bytes ")).map(|b| b / (1024 * 1024));
        let index = out.len() as u32;
        out.push(AiGpu { index, name: name.unwrap_or_else(|| "AMD GPU".into()), memory_mb: mem, kind: "amd".into() });
    }
    if out.is_empty() {
        out.push(AiGpu { index: 0, name: "AMD GPU (ROCm)".into(), memory_mb: None, kind: "amd".into() });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulled_models_are_read_from_disk_without_cloud_ones() {
        let dir = std::env::temp_dir().join(format!("nibd-ollama-{}", std::process::id()));
        let m = dir.join("manifests");
        for path in [
            "registry.ollama.ai/library/qwen3.8/27b-mlx",
            "registry.ollama.ai/library/llama4/latest",
            "registry.ollama.ai/library/gpt-oss/120b-cloud",
            "registry.ollama.ai/someone/tiny/q4",
            "hf.co/org/repo/Q8_0",
        ] {
            let p = m.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "{}").unwrap();
        }
        let found = manifests_in(&[dir.clone(), dir.join("missing")]);
        let mut got: Vec<_> = found.keys().cloned().collect();
        got.sort();
        assert_eq!(got, ["hf.co/org/repo:Q8_0", "llama4:latest", "qwen3.8:27b-mlx", "someone/tiny:q4"]);
        assert_eq!(name("llama4"), "llama4:latest");
        // Sizes: each model's config and layers; on disk, a shared file once.
        let manifest = |layers: &[(&str, u64)]| {
            let l: Vec<_> = layers.iter().map(|(d, n)| serde_json::json!({ "digest": d, "size": n })).collect();
            serde_json::json!({ "config": { "digest": "cfg", "size": 1 }, "layers": l }).to_string()
        };
        std::fs::write(m.join("registry.ollama.ai/library/llama4/latest"), manifest(&[("a", 100), ("b", 10)])).unwrap();
        std::fs::write(m.join("registry.ollama.ai/someone/tiny/q4"), manifest(&[("a", 100)])).unwrap();
        let (models, disk) = sizes(&found);
        let _ = std::fs::remove_dir_all(&dir);
        let size = |n: &str| models.iter().find(|x| x.name == n).map(|x| x.bytes);
        assert_eq!((size("llama4:latest"), size("someone/tiny:q4"), size("qwen3.8:27b-mlx")), (Some(111), Some(101), Some(0)));
        assert_eq!(disk, 111);
        assert_eq!(name("qwen3.8:27b-mlx"), "qwen3.8:27b-mlx");
    }

    #[test]
    fn model_names() {
        assert_eq!(name_of("ollama/qwen3:8b"), "qwen3:8b");
        assert_eq!(name_of("qwen3:8b"), "qwen3:8b");
        assert_eq!(name_of("qwen3"), "qwen3:latest");
        assert_eq!(name_of("ollama/qwen3"), "qwen3:latest");
        assert_eq!(name_of("hf.co/user/model:q4"), "hf.co/user/model:q4");
        assert_eq!(name_of("ollama/hf.co/user/model:q4"), "hf.co/user/model:q4");
        assert_eq!(name_of("hf.co/user/model"), "hf.co/user/model:latest");
        assert_eq!(model_of("mine/qwen3:8b"), "qwen3:8b");
        assert_eq!(bare(" ollama/qwen3:8b "), "qwen3:8b");
        assert_eq!(bare("someone/tiny:q4"), "someone/tiny:q4");
    }

    #[test]
    fn chunked_bodies_are_joined() {
        assert_eq!(unchunk("4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n"), "{\"a\":1}");
        assert_eq!(unchunk("garbage"), "");
    }
}
