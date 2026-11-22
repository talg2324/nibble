//! `nib` — Iced GUI client. Owns buffers and rendering; talks to `nibd` for everything on disk.

// `nib ctl state` builds one large `json!` object for the agents tab.
#![recursion_limit = "256"]

mod ai_cli;
mod app;
mod color_picker;
mod control;
mod diff_view;
mod editor_view;
mod fonts;
mod forward;
mod gif;
mod gpu_log;
mod gpu_pick;
mod grammar_cli;
mod headless;
mod icons;
mod links;
mod math;
mod media;
mod pdf;
mod preview;
mod ssh;
mod table;
mod table_view;
mod terminal_view;
mod theme;
mod timing;
mod transport;
mod web;
mod zoom;

use anyhow::{bail, Context, Result};
use nib_editor::{Command, Config, Keymap, Target};
use std::path::Path;
use std::sync::Mutex;

const USAGE: &str = "\
usage: nib [PATH]              open a local file or directory (default: .)
       nib @PROFILE [PATH]     open PATH (relative to the profile root) on a configured target
       nib --headless [@PROFILE] [PATH]
                               no window: same editor, driven only through `nib ctl`; prints
                               its session (pid, socket, run_dir, logs) on stdout, exits on `nib ctl run quit`
       nib --bench FILE        scroll FILE for 300 frames and print frames/s, then exit
       nib --commands          list command names usable in [keys]
       nib ctl COMMAND         drive a running window from a script or agent (nib ctl help)
       nib ai [@PROFILE] COMMAND
                               local models and sub-agents for scripts and agents: one call,
                               one agent run, or a resumable batch (nib ai help)
       nib gif FILE            an animated GIF as one contact sheet of its distinct frames, for
                               models that only see a GIF's first frame (nib gif help)
       nib --version";

/// Where a window works: the machine (`target`), the folder on it, and what to call it.
#[derive(Debug, Clone)]
pub struct Launch {
    pub target: Target,
    pub root: String,
    /// Path to open, relative to root (or absolute on the target). None = browse root.
    pub open: Option<String>,
    pub label: String,
    /// The `[profiles]` entry this came from.
    pub profile: Option<String>,
}

fn local_launch(p: Option<&str>) -> Result<Launch> {
    let path = std::path::absolute(p.unwrap_or("."))?;
    let (root, open) = if path.is_dir() {
        (path, None)
    } else {
        let parent = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| ".".into());
        (
            parent,
            path.file_name().map(|n| n.to_string_lossy().into_owned()),
        )
    };
    Ok(Launch {
        target: Target::Local { nibd: None },
        root: root.to_string_lossy().into_owned(),
        open,
        label: "local".into(),
        profile: None,
    })
}

fn parse_args(cfg: &Config, args: &[String]) -> Result<Option<(Launch, bool)>> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["-h" | "--help"] => {
            println!("{USAGE}");
            Ok(None)
        }
        ["-V" | "--version"] => {
            println!(
                "nib {} (protocol {})",
                env!("CARGO_PKG_VERSION"),
                nib_proto::PROTOCOL_VERSION
            );
            Ok(None)
        }
        ["--commands"] => {
            let km = Keymap::default();
            for c in Command::ALL {
                let keys: Vec<String> = km.chords_for(*c).iter().map(|k| k.to_string()).collect();
                println!(
                    "{:<22} {:<10} {:<40} {}",
                    c.name(),
                    c.category(),
                    c.title(),
                    keys.join(", ")
                );
            }
            Ok(None)
        }
        ["--bench", file] => Ok(Some((local_launch(Some(file))?, true))),
        [p, rest @ ..] if p.starts_with('@') && rest.len() <= 1 => {
            let name = &p[1..];
            let prof = cfg.profiles.get(name).with_context(|| {
                let known: Vec<_> = cfg.profiles.keys().map(|k| format!("@{k}")).collect();
                format!(
                    "unknown profile @{name} (config: {}; known: {})",
                    Config::path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    if known.is_empty() {
                        "none".into()
                    } else {
                        known.join(" ")
                    }
                )
            })?;
            Ok(Some((
                Launch {
                    target: prof.target()?,
                    root: prof.root.clone(),
                    open: rest.first().map(|s| s.to_string()),
                    label: format!("@{name}"),
                    profile: Some(name.to_owned()),
                },
                false,
            )))
        }
        [p, ..] if p.starts_with('-') => bail!("unknown option {p}\n{USAGE}"),
        [] => Ok(Some((local_launch(None)?, false))),
        [p] => Ok(Some((local_launch(Some(p))?, false))),
        _ => bail!("too many arguments\n{USAGE}"),
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // ssh runs nib as its askpass program (see `ssh::askpass_main`): answer and exit.
    if ssh::is_askpass() {
        std::process::exit(ssh::askpass_main(&args));
    }
    if args.first().map(String::as_str) == Some("ctl") {
        std::process::exit(control::ctl_main(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("ai") {
        std::process::exit(ai_cli::ai_main(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("grammar") {
        std::process::exit(grammar_cli::grammar_main(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("gif") {
        std::process::exit(gif::gif_main(&args[1..]));
    }
    if args.first().map(String::as_str) == Some(web::WATCH_ARG) {
        std::process::exit(web::watch_main(&args[1..]));
    }
    if args.first().map(String::as_str) == Some(gpu_pick::PROBE_ARG) {
        std::process::exit(gpu_pick::probe_main());
    }
    let headless = args.first().map(String::as_str) == Some("--headless");
    let args = if headless { &args[1..] } else { &args[..] };
    timing::start();
    let cfg = Config::load()?;
    let Some((launch, bench)) = parse_args(&cfg, args)? else {
        return Ok(());
    };
    if headless && bench {
        bail!("--headless and --bench don't mix");
    }
    let (keymap, mut warnings) = Keymap::with_overrides(&cfg.keys);
    let (theme, theme_warnings) = theme::Theme::from_config(&cfg.theme);
    warnings.extend(theme_warnings);

    // Renderer: CPU (tiny-skia) on GPU-less VMs; it redraws only damaged regions.
    if std::env::var_os("ICED_BACKEND").is_none() {
        match cfg.editor.renderer.as_str() {
            "cpu" => std::env::set_var("ICED_BACKEND", "tiny-skia"),
            "gpu" => std::env::set_var("ICED_BACKEND", "wgpu"),
            _ => {}
        }
    }
    // "auto" on WSL: find the GPU setup once and remember it (`gpu_pick`).
    let gpu_pick = if headless { None } else { gpu_pick::choose(&cfg.editor.renderer) };

    // Grammar/query compilation runs in parallel with connecting and window creation.
    nib_syntax::configure(
        Config::path().and_then(|p| p.parent().map(Path::to_path_buf)),
        grammar_cli::runtime_languages(&cfg),
    );
    let (syn_tx, syn_rx) = iced::futures::channel::oneshot::channel::<()>();
    std::thread::spawn(move || {
        nib_syntax::registry();
        let _ = syn_tx.send(());
    });

    // The window opens right away; the app connects in the background (`App::connect`). ssh
    // asks for passwords and codes in the window (askpass), never in a terminal.
    // Control socket: requests arrive as messages on the same channel kind as the daemon's.
    let (ctl_tx, ctl_rx) = iced::futures::channel::mpsc::unbounded::<app::Message>();
    let meta = serde_json::json!({
        "label": launch.label,
        "root": launch.root,
        "headless": headless,
        "logs": { "nibd": transport::daemon_log(&launch.target) },
    });
    // The web tabs' engine wakes the app the same way, when a page or its picture changed.
    let wake = ctl_tx.clone();
    web::wake_with(move || {
        let _ = wake.unbounded_send(app::Message::Web(app::WebMsg::Wake));
    });
    let ctl_guard = if bench {
        None
    } else if headless {
        // Headless, the socket is the only way in.
        let g = control::serve(ctl_tx, app::Message::Ctl, meta)?
            .context("--headless needs the control socket (NIB_CONTROL is off)")?;
        println!("{}", control::session().cloned().unwrap_or_else(|| serde_json::json!({ "socket": g.socket() })));
        Some(g)
    } else {
        control::serve(ctl_tx, app::Message::Ctl, meta).unwrap_or_else(|e| {
            warnings.push(format!("control socket off: {e:#}"));
            None
        })
    };

    if !headless {
        gpu_log::init();
    }
    log_panics();
    if let Some(gpu_pick::Outcome(note)) = &gpu_pick {
        control::log("gpu", note);
    }
    let title = format!("nib — {}", launch.label);
    let window = cfg.window.clone();
    let init = Mutex::new(Some((
        app::Init {
            conn: None,
            keymap,
            theme,
            warnings,
            tab_width: cfg.editor.tab_width,
            font_size: cfg.editor.font_size,
            font: cfg.editor.font.clone(),
            line_height: cfg.editor.line_height,
            beginner_mode: cfg.editor.beginner_mode,
            theme_cfg: cfg.theme.clone(),
            lsp: cfg
                .lsp
                .iter()
                .map(|(name, s)| nib_proto::LspServer {
                    name: name.clone(),
                    command: s.command.clone(),
                    extensions: s.extensions.clone(),
                    roots: s.roots.clone(),
                })
                .collect(),
            ai: cfg.ai.clone(),
            launch,
            bench,
            headless,
        },
        syn_rx,
        ctl_rx,
    )));

    if headless {
        let (init, syn_rx, ctl_rx) = init.into_inner().unwrap().expect("not taken yet");
        headless::run(init, syn_rx, ctl_rx);
        drop(ctl_guard);
        return Ok(());
    }

    if !bench {
        detach_own_console();
    }

    // WSL usually has no colour-emoji font, so the AI chat's marks (🤖 🔧 ✅ …) draw as nothing.
    // Windows' own is a mount away; text falls back to it when no Linux font has the glyph.
    if cfg!(target_os = "linux") && std::path::Path::new("/dev/dxg").exists() {
        if let (Ok(bytes), Ok(mut fs)) = (std::fs::read("/mnt/c/Windows/Fonts/seguiemj.ttf"), iced::advanced::graphics::text::font_system().write()) {
            fs.load_font(bytes.into());
        }
    }

    // Before any text is laid out: fonts named for installed ones (fonts.rs says why).
    fonts::pin();

    let result = iced::application(
        move || {
            let (init, syn_rx, ctl_rx) = init.lock().unwrap().take().expect("boot runs once");
            name_the_app();
            let (app, t) = app::App::boot(init);
            let probe = app::App::window_probe(window.fullscreen);
            let control = Task::run(ctl_rx, |m| m);
            let syntax = Task::perform(syn_rx, |_| app::Message::SyntaxReady);
            (app, Task::batch([t, control, syntax, probe]))
        },
        app::App::update,
        app::App::view,
    )
    .title(app::App::title)
    .subscription(app::App::subscription)
    .window(iced::window::Settings {
        size: iced::Size::new(window.width, window.height),
        icon: window_icon(),
        // macOS: no grey system title bar; the app's own black strip shows through (app::TITLEBAR_H).
        #[cfg(target_os = "macos")]
        platform_specific: iced::window::settings::PlatformSpecific {
            title_hidden: false,
            titlebar_transparent: true,
            fullsize_content_view: true,
        },
        // Linux: the Wayland app_id / X11 WM_CLASS, which names the app where the desktop shows
        // one. Desktops (and WSLg) match it to nibble.desktop for the name and the taskbar icon;
        // without one WSLg shows its penguin.
        #[cfg(target_os = "linux")]
        platform_specific: iced::window::settings::PlatformSpecific {
            application_id: "nibble".into(),
            ..Default::default()
        },
        // WSL: no frame from winit; the app draws its own (app/titlebar.rs says why).
        decorations: !app::own_frame(),
        ..Default::default()
    })
    .theme(|_: &app::App| iced::Theme::Dark)
    .default_font(crate::fonts::mono())
    .antialiasing(false)
    .run()
    .map_err(|e| anyhow::anyhow!("{title}: {e}"));
    // A window has no console to print this to: put it in nib.log (before `ctl_guard` logs `exit`).
    if let Err(e) = &result {
        control::log("error", &format!("{e:#}"));
    }
    result
}

/// Also write panics to nib.log: a window's stderr goes nowhere, so otherwise a crash leaves no trace.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        control::log("panic", &format!("thread {:?}: {info}", thread.name().unwrap_or("?")));
        default(info);
    }));
}

/// The window's own icon: the title bar and Alt-Tab on Windows, _NET_WM_ICON on X11. (The exe's
/// icon resource, build.rs, covers Explorer and the taskbar; Wayland and macOS ignore this.)
fn window_icon() -> Option<iced::window::Icon> {
    // The largest image in the .ico (256 px).
    let img = image::load_from_memory_with_format(include_bytes!("../../../assets/icon.ico"), image::ImageFormat::Ico)
        .ok()?
        .into_rgba8();
    let (w, h) = img.dimensions();
    iced::window::icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Windows: nib.exe is a console program (so `nib ctl` and the other subcommands print, and
/// shells wait for them), which gives it a console window of its own when started from Explorer
/// or a shortcut. The editor window doesn't need one: let it go when nothing else is using it.
/// Started from a terminal, the terminal is shared and stays. What nib starts later (nibd, git,
/// ssh) is started without a console window (`transport::no_console_window`).
#[cfg(windows)]
fn detach_own_console() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList, SetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    let mut pids = [0u32; 2];
    // SAFETY: plain Win32 calls; the buffer is as long as the count passed.
    unsafe {
        if GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) != 1 {
            return;
        }
        FreeConsole();
        // The old handles now point at nothing; with none at all, Rust's stdout and stderr
        // discard writes instead of failing.
        for h in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            SetStdHandle(h, std::ptr::null_mut());
        }
    }
}

#[cfg(not(windows))]
fn detach_own_console() {}

/// macOS: the window is a bare binary, not an app bundle, so the system names it after the file,
/// "nib", in the menu bar, the Dock and Cmd-Tab. The app is called nibble. (Windows takes the name
/// from nib.rc, Linux from the window's app id.)
///
/// The process name gives the app menu its items (About / Hide / Quit nibble). The name the system
/// shows comes from LaunchServices, which has no public call for an unbundled app: this is the one
/// the JDK uses for `-Xdock:name`. It needs the app checked in with LaunchServices, so call this
/// once the application has started; looked up at run time, it is skipped if a macOS lacks it.
#[cfg(target_os = "macos")]
fn name_the_app() {
    use std::ffi::c_void;
    let name = objc2_foundation::NSString::from_str("nibble");
    objc2_foundation::NSProcessInfo::processInfo().setProcessName(&name);
    type GetAsn = unsafe extern "C" fn() -> *const c_void;
    type SetItem = unsafe extern "C" fn(i32, *const c_void, *const c_void, *const c_void, *mut c_void) -> i32;
    // kLSDefaultSessionID
    const SESSION: i32 = -2;
    // SAFETY: symbols looked up by name and used with the signatures LaunchServices has had since
    // 10.x; the key is a CFStringRef global; NSString is toll-free bridged to CFStringRef.
    unsafe {
        let sym = |n: &std::ffi::CStr| libc::dlsym(libc::RTLD_DEFAULT, n.as_ptr());
        let (get, set, key) = (sym(c"_LSGetCurrentApplicationASN"), sym(c"_LSSetApplicationInformationItem"), sym(c"_kLSDisplayNameKey"));
        if get.is_null() || set.is_null() || key.is_null() {
            return;
        }
        let get: GetAsn = std::mem::transmute(get);
        let set: SetItem = std::mem::transmute(set);
        let key = *(key as *const *const c_void);
        let asn = get();
        if asn.is_null() || key.is_null() {
            return;
        }
        set(SESSION, asn, key, &*name as *const objc2_foundation::NSString as *const c_void, std::ptr::null_mut());
    }
}

#[cfg(not(target_os = "macos"))]
fn name_the_app() {}

use iced::Task;
