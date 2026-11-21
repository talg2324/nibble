//! Find the GPU on machines where wgpu's default lands on a software renderer, and remember it.
//!
//! WSL is the case today. Its GPU is reached through `/dev/dxg` and Mesa's D3D12 driver, but wgpu
//! prefers Vulkan, and the only Vulkan driver there is lavapipe (Mesa's CPU rasterizer); Mesa's
//! GL loader also tries Zink (GL on that same Vulkan) before D3D12. The window then draws on the
//! CPU: Follow with three agents costs ~2.5 cores instead of ~0.2. `WGPU_BACKEND=gl` plus
//! `GALLIUM_DRIVER=d3d12` puts it on the GPU.
//!
//! Mesa reads `GALLIUM_DRIVER` once, when its driver loads, so one process can't try several
//! setups: each candidate is probed by running this binary again (`nib --gpu-probe`) with the
//! candidate's environment, which lists the adapters wgpu would see and exits. The best one is
//! kept in `<cache>/nib/gpu.json`, keyed by what would change the answer (kernel, Windows GPU
//! driver, nib version), so probing happens once, not on every launch. If the window later comes
//! up without a GPU adapter after all, the cache is dropped and the next launch probes again.
//!
//! Only with `renderer = "auto"` and when none of `ICED_BACKEND`, `WGPU_BACKEND` or
//! `GALLIUM_DRIVER` is already set: anything the user chose wins.

use iced::wgpu;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The hidden argument that makes `nib` probe instead of opening a window.
pub const PROBE_ARG: &str = "--gpu-probe";

/// A setup to try: its name (cached) and the environment it runs with.
struct Candidate {
    name: &'static str,
    env: &'static [(&'static str, &'static str)],
}

/// In order of preference when they find equally good adapters: wgpu's own choice first.
const CANDIDATES: &[Candidate] = &[
    Candidate { name: "default", env: &[] },
    Candidate { name: "wsl-d3d12", env: &[("WGPU_BACKEND", "gl"), ("GALLIUM_DRIVER", "d3d12")] },
];

/// A broken driver can hang instead of failing: give up on a probe after this long.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Set when this launch applied a cached or probed GPU setup (so a fallback can drop the cache).
static APPLIED: AtomicBool = AtomicBool::new(false);

/// What `choose` did, for `nib.log` (`gpu` lines) once the log exists.
pub struct Outcome(pub String);

/// Pick the setup for this launch and put its environment in place. Call before iced starts.
/// `None` when there is nothing to choose (not WSL, or the user already chose).
pub fn choose(renderer: &str) -> Option<Outcome> {
    if cfg!(target_os = "linux") && is_wsl() && std::env::var_os("EGL_LOG_LEVEL").is_none() {
        // WSL has no DRM device, so Mesa's EGL loader prints "failed to get driver name for
        // fd -1" / "MESA-LOADER: failed to retrieve device information" to the terminal on
        // every launch (and in every probe), whichever setup is used. Harmless; keep them out.
        std::env::set_var("EGL_LOG_LEVEL", "fatal");
    }
    if renderer != "auto" || !cfg!(target_os = "linux") || !is_wsl() {
        return None;
    }
    if ["ICED_BACKEND", "WGPU_BACKEND", "GALLIUM_DRIVER"].iter().any(|k| std::env::var_os(k).is_some()) {
        return Some(Outcome("renderer set by the environment; not probing".into()));
    }
    let key = cache_key();
    let (pick, note) = match read_cache(&key) {
        Some((pick, adapter)) => (pick, format!("cached: {adapter}")),
        None => {
            let started = Instant::now();
            let (pick, adapter, tried) = probe_all();
            write_cache(&key, &pick, &adapter, &tried);
            (pick, format!("probed in {} ms: {adapter} [{}]", started.elapsed().as_millis(), tried.join("; ")))
        }
    };
    let Some(c) = CANDIDATES.iter().find(|c| c.name == pick) else {
        return Some(Outcome(format!("unknown cached setup {pick:?}; using wgpu's default")));
    };
    for (k, v) in c.env {
        // Before iced, its renderer threads, or any driver are started.
        std::env::set_var(k, v);
    }
    APPLIED.store(!c.env.is_empty(), Ordering::Relaxed);
    Some(Outcome(format!("setup {pick} ({note})")))
}

/// The window is up, on a GPU adapter or not (no adapter, or a software one): if a setup was
/// applied and it didn't give a GPU after all, forget it so the next launch probes again.
pub fn window_up(on_gpu: bool) {
    if !on_gpu && APPLIED.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(cache_path());
        crate::control::log("gpu", "the chosen GPU setup gave no adapter; it will be probed again next launch");
    }
}

/// `nib --gpu-probe`: list the adapters wgpu sees with this environment, one JSON line, and exit.
/// Mirrors how iced builds its instance (all backends unless `WGPU_BACKEND` narrows them).
pub fn probe_main() -> i32 {
    let backends = wgpu::Backends::from_env().unwrap_or(wgpu::Backends::all());
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends, ..Default::default() });
    let adapters: Vec<Value> = instance
        .enumerate_adapters(backends)
        .iter()
        .map(|a| {
            let i = a.get_info();
            json!({ "name": i.name, "kind": format!("{:?}", i.device_type), "backend": format!("{:?}", i.backend) })
        })
        .collect();
    println!("{}", json!({ "adapters": adapters }));
    0
}

fn is_wsl() -> bool {
    std::path::Path::new("/dev/dxg").exists()
}

/// Higher is better; 0 = software (no better than wgpu's default). Through GL, wgpu can't tell
/// the device type and says `Other` even for a discrete GPU (WSL's "D3D12 (NVIDIA …)"), so
/// `Other` ranks above software; software renderers are also caught by name.
fn rank(name: &str, kind: &str) -> u8 {
    let lower = name.to_ascii_lowercase();
    if ["llvmpipe", "lavapipe", "softpipe", "swiftshader", "software"].iter().any(|s| lower.contains(s)) {
        return 0;
    }
    match kind {
        "DiscreteGpu" => 4,
        "IntegratedGpu" => 3,
        "VirtualGpu" => 2,
        "Other" => 1,
        _ => 0,
    }
}

/// Probe every candidate; the best adapter wins, ties go to the earlier candidate.
/// Returns (candidate name, its best adapter, a line per candidate for the log).
fn probe_all() -> (String, String, Vec<String>) {
    let mut best: Option<(u8, &str, String)> = None;
    let mut tried = Vec::new();
    for c in CANDIDATES {
        let adapters = probe(c);
        let top = adapters.iter().max_by_key(|(name, kind, _)| rank(name, kind));
        tried.push(match top {
            Some((name, kind, backend)) => format!("{}: {name} ({kind}, {backend})", c.name),
            None => format!("{}: no adapter", c.name),
        });
        if let Some((name, kind, backend)) = top {
            let r = rank(name, kind);
            if best.as_ref().map_or(true, |(br, _, _)| r > *br) {
                best = Some((r, c.name, format!("{name} ({kind}, {backend})")));
            }
        }
    }
    match best {
        Some((_, name, adapter)) => (name.to_owned(), adapter, tried),
        None => ("default".into(), "no adapter found".into(), tried),
    }
}

/// (name, device type, backend) of every adapter one candidate sees; empty if the probe failed.
fn probe(c: &Candidate) -> Vec<(String, String, String)> {
    let Ok(exe) = std::env::current_exe() else { return Vec::new() };
    let mut cmd = Command::new(exe);
    cmd.arg(PROBE_ARG).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    for k in ["ICED_BACKEND", "WGPU_BACKEND", "GALLIUM_DRIVER"] {
        cmd.env_remove(k);
    }
    cmd.envs(c.env.iter().copied());
    let Ok(mut child) = cmd.spawn() else { return Vec::new() };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        use std::io::Read;
        let _ = s.read_to_string(&mut out);
    }
    parse_probe(&out)
}

fn parse_probe(out: &str) -> Vec<(String, String, String)> {
    let Some(v) = out.lines().rev().find_map(|l| serde_json::from_str::<Value>(l).ok()) else {
        return Vec::new();
    };
    let s = |a: &Value, k: &str| a[k].as_str().unwrap_or_default().to_owned();
    v["adapters"]
        .as_array()
        .map(|list| list.iter().map(|a| (s(a, "name"), s(a, "kind"), s(a, "backend"))).collect())
        .unwrap_or_default()
}

fn cache_path() -> PathBuf {
    crate::transport::cache_dir().join("gpu.json")
}

/// What would change the answer: the kernel (a WSL update), the Windows GPU driver (it lives in
/// /usr/lib/wsl/lib, replaced on a driver update), and nib itself.
fn cache_key() -> String {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let driver = std::fs::metadata("/usr/lib/wsl/lib/libd3d12core.so")
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    format!("{}|{driver}|{}", kernel.trim(), env!("CARGO_PKG_VERSION"))
}

/// (setup, adapter) from the cache, if it was made under the same key.
fn read_cache(key: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(cache_path()).ok()?).ok()?;
    if v["key"].as_str()? != key {
        return None;
    }
    Some((v["setup"].as_str()?.to_owned(), v["adapter"].as_str().unwrap_or_default().to_owned()))
}

fn write_cache(key: &str, setup: &str, adapter: &str, tried: &[String]) {
    let path = cache_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let v = json!({ "key": key, "setup": setup, "adapter": adapter, "tried": tried });
    let _ = std::fs::write(&path, format!("{v:#}\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_probe_line_after_driver_noise() {
        let out = "libEGL warning: something\n{\"adapters\":[{\"name\":\"D3D12 (NVIDIA GeForce RTX 2080 Ti)\",\"kind\":\"DiscreteGpu\",\"backend\":\"Gl\"},{\"name\":\"llvmpipe\",\"kind\":\"Cpu\",\"backend\":\"Gl\"}]}\n";
        let a = parse_probe(out);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0], ("D3D12 (NVIDIA GeForce RTX 2080 Ti)".into(), "DiscreteGpu".into(), "Gl".into()));
        assert!(parse_probe("garbage").is_empty());
    }

    #[test]
    fn software_adapters_never_beat_the_default() {
        assert_eq!(rank("llvmpipe (LLVM 20.1.2, 256 bits)", "Cpu"), 0);
        // What Mesa's GL says for lavapipe-backed Zink or a software GL that hides its type.
        assert_eq!(rank("zink Vulkan 1.4(llvmpipe (LLVM 20.1.2, 256 bits))", "Other"), 0);
        assert!(rank("NVIDIA GeForce RTX 2080 Ti", "DiscreteGpu") > rank("Intel UHD", "IntegratedGpu"));
    }

    #[test]
    fn a_gpu_seen_through_gl_beats_software() {
        // WSL: the GPU through Mesa's D3D12 driver; wgpu's GL backend can't tell its type.
        assert!(rank("D3D12 (NVIDIA GeForce RTX 2080 Ti)", "Other") > rank("llvmpipe (LLVM 20.1.2, 256 bits)", "Cpu"));
    }
}
