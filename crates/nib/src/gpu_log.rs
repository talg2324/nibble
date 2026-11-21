//! What the window draws with, in `nib.log`: the GPU adapter wgpu picked (and the ones it could
//! have), or that iced fell back to the tiny-skia software renderer. iced reports the adapter only
//! through the `log` crate and falls back silently, so this is a `log` sink for iced's renderer
//! crates and nothing else. Useful where the driver stack is shaky (WSLg prints Mesa/EGL noise on
//! stderr and it isn't obvious whether the GPU is in use).

use std::sync::atomic::{AtomicBool, Ordering};

static PICKED: AtomicBool = AtomicBool::new(false);
/// The adapter wgpu picked is a GPU (not llvmpipe / lavapipe / another software one).
static ON_GPU: AtomicBool = AtomicBool::new(false);

struct Sink;

fn ours(target: &str) -> bool {
    ["iced_wgpu", "iced_renderer", "iced_tiny_skia"].iter().any(|c| target.starts_with(c))
}

impl log::Log for Sink {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info && ours(m.target())
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let msg = r.args().to_string();
        if let Some(info) = msg.strip_prefix("Selected: ") {
            PICKED.store(true, Ordering::Relaxed);
            ON_GPU.store(!info.contains("device_type: Cpu"), Ordering::Relaxed);
            crate::control::log("gpu", &format!("wgpu: {}", compact(info)));
        } else if let Some(list) = msg.strip_prefix("Available adapters: ") {
            crate::control::log("gpu", &format!("available: {}", adapters(list)));
        } else if r.level() <= log::Level::Warn {
            crate::control::log("gpu", &compact(&msg));
        }
        // The rest (settings, surface formats, alpha modes) is detail nobody reads.
    }

    fn flush(&self) {}
}

/// Install the sink. Info level is needed for the adapter lines; it also makes iced list the
/// adapters once at startup (`Instance::enumerate_adapters`).
pub fn init() {
    if log::set_logger(&Sink).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

/// Once the window is up: no adapter was picked, so iced is drawing with tiny-skia on the CPU.
pub fn window_up() {
    if !PICKED.load(Ordering::Relaxed) {
        crate::control::log("gpu", "tiny-skia (software): wgpu found no usable adapter, or ICED_BACKEND chose it");
    }
    crate::gpu_pick::window_up(ON_GPU.load(Ordering::Relaxed));
}

/// A `{:#?}` dump on one line: `AdapterInfo { name: "…", …, backend: Vulkan }`.
fn compact(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").replace(", }", " }").replace(",}", "}")
}

/// `[AdapterInfo { name: "a", device_type: Cpu, backend: Gl, … }, …]` as `a (Cpu, Gl); …`.
fn adapters(list: &str) -> String {
    let field = |block: &str, key: &str| -> Option<String> {
        let rest = &block[block.find(&format!("{key}: "))? + key.len() + 2..];
        // Quoted values (names) can hold commas: end at the closing quote.
        if let Some(q) = rest.strip_prefix('"') {
            return Some(q[..q.find('"').unwrap_or(q.len())].to_owned());
        }
        let end = rest.find([',', '\n', '}']).unwrap_or(rest.len());
        Some(rest[..end].trim().to_owned())
    };
    let found: Vec<String> = list
        .split("AdapterInfo")
        .skip(1)
        .map(|b| {
            let name = field(b, "name").unwrap_or_default();
            let kind = field(b, "device_type").unwrap_or_default();
            let backend = field(b, "backend").unwrap_or_default();
            format!("{name} ({kind}, {backend})")
        })
        .collect();
    if found.is_empty() { "none".into() } else { found.join("; ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"[
    AdapterInfo {
        name: "D3D12 (NVIDIA RTX A2000)",
        vendor: 4318,
        device: 0,
        device_type: DiscreteGpu,
        driver: "",
        driver_info: "Mesa 24.2",
        backend: Gl,
    },
    AdapterInfo {
        name: "llvmpipe (LLVM 19.1.1, 256 bits)",
        vendor: 65541,
        device: 0,
        device_type: Cpu,
        driver: "llvmpipe",
        driver_info: "Mesa 24.2",
        backend: Vulkan,
    },
]"#;

    #[test]
    fn lists_adapters_on_one_line() {
        assert_eq!(adapters(DUMP), "D3D12 (NVIDIA RTX A2000) (DiscreteGpu, Gl); llvmpipe (LLVM 19.1.1, 256 bits) (Cpu, Vulkan)");
        assert_eq!(adapters("[]"), "none");
    }

    #[test]
    fn compacts_a_pretty_dump() {
        let one = "AdapterInfo {\n    name: \"x\",\n    backend: Gl,\n}";
        assert_eq!(compact(one), "AdapterInfo { name: \"x\", backend: Gl }");
    }
}
