//! Finding the browser to show pages with, giving it a profile of nib's own, starting it
//! headless, and making sure it never outlives the window it belongs to.
//!
//! The browser is the one the operating system opens links with, when nib can drive it:
//! Firefox (over WebDriver BiDi) or a Chromium browser — Chrome, Edge, Brave, Chromium (over
//! the DevTools protocol). Safari cannot be run unseen or driven this way, so with it as the
//! default, or any other, nib takes the first of those that is installed. nib installs none:
//! without one, the web tab and the agents' page requests are not there, and pages
//! open in the system's browser instead.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// `nib WATCH_ARG NIB_PID BROWSER_PID`: the watcher (see [`watch_main`]).
pub const WATCH_ARG: &str = "--web-watch";

/// What nib keeps in a profile folder about who is using it.
const OWNER_FILE: &str = "nib-owner.json";

/// How nib talks to a browser.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// WebDriver BiDi.
    Firefox,
    /// The Chrome DevTools protocol.
    Chromium,
}

/// A browser nib knows how to run: where it is installed, and how the system names it when it
/// is the default (a bundle id on macOS, a ProgId on Windows, a desktop file on Linux).
struct Known {
    name: &'static str,
    kind: Kind,
    mac: &'static str,
    windows: &'static str,
    linux: &'static [&'static str],
    ids: &'static [&'static str],
}

/// In the order they are tried when the system's default is not one of them.
const KNOWN: &[Known] = &[
    Known {
        name: "Chrome",
        kind: Kind::Chromium,
        mac: "Google Chrome.app/Contents/MacOS/Google Chrome",
        windows: "Google\\Chrome\\Application\\chrome.exe",
        linux: &["google-chrome", "google-chrome-stable"],
        ids: &["com.google.chrome", "chromehtml", "google-chrome"],
    },
    Known {
        name: "Edge",
        kind: Kind::Chromium,
        mac: "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        windows: "Microsoft\\Edge\\Application\\msedge.exe",
        linux: &["microsoft-edge", "microsoft-edge-stable"],
        ids: &["com.microsoft.edgemac", "msedgehtm", "microsoft-edge"],
    },
    Known {
        name: "Firefox",
        kind: Kind::Firefox,
        mac: "Firefox.app/Contents/MacOS/firefox",
        windows: "Mozilla Firefox\\firefox.exe",
        linux: &["firefox", "firefox-esr"],
        ids: &["org.mozilla.firefox", "firefoxurl", "firefoxhtml", "firefox"],
    },
    Known {
        name: "Brave",
        kind: Kind::Chromium,
        mac: "Brave Browser.app/Contents/MacOS/Brave Browser",
        windows: "BraveSoftware\\Brave-Browser\\Application\\brave.exe",
        linux: &["brave-browser", "brave"],
        ids: &["com.brave.browser", "bravehtml", "brave-browser"],
    },
    Known {
        name: "Chromium",
        kind: Kind::Chromium,
        mac: "Chromium.app/Contents/MacOS/Chromium",
        windows: "Chromium\\Application\\chrome.exe",
        linux: &["chromium", "chromium-browser"],
        ids: &["org.chromium.chromium", "chromiumhtm", "chromium"],
    },
];

/// The browser nib will use, and why that one.
#[derive(Clone, Debug)]
pub struct Found {
    pub name: String,
    pub kind: Kind,
    pub exe: PathBuf,
    /// It is the one the system opens links with.
    pub default: bool,
}

impl Known {
    fn installed(&self) -> Option<PathBuf> {
        let mut places: Vec<PathBuf> = Vec::new();
        if cfg!(target_os = "macos") {
            places.push(Path::new("/Applications").join(self.mac));
            if let Some(home) = std::env::var_os("HOME") {
                places.push(Path::new(&home).join("Applications").join(self.mac));
            }
        } else if cfg!(windows) {
            for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
                if let Some(dir) = std::env::var_os(var) {
                    places.push(Path::new(&dir).join(self.windows));
                }
            }
        } else if let Some(path) = std::env::var_os("PATH") {
            places.extend(std::env::split_paths(&path).flat_map(|d| self.linux.iter().map(move |n| d.join(n))));
        }
        places.into_iter().find(|p| p.is_file())
    }
}

/// How the system names its default browser, lower-cased (empty: it would not say).
fn system_default() -> String {
    let said = |program: &str, args: &[&str]| -> String {
        Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase()).unwrap_or_default()
    };
    if cfg!(target_os = "macos") {
        mac_default(&said("defaults", &["read", "com.apple.LaunchServices/com.apple.launchservices.secure", "LSHandlers"]))
    } else if cfg!(windows) {
        said("reg", &["query", "HKCU\\Software\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\http\\UserChoice", "/v", "ProgId"])
    } else {
        said("xdg-settings", &["get", "default-web-browser"])
    }
}

/// The bundle id that handles `http` in macOS's list of handlers (lower-cased text of
/// `defaults read … LSHandlers`). Nothing listed means Safari, which nobody had to choose.
fn mac_default(handlers: &str) -> String {
    let mut role = "";
    for line in handlers.lines().map(str::trim) {
        if line == "{" {
            role = "";
        } else if let Some(v) = line.strip_prefix("lshandlerroleall = ") {
            let v = v.trim_end_matches(';').trim_matches('"');
            if v != "-" {
                role = v;
            }
        } else if line == "lshandlerurlscheme = http;" && !role.is_empty() {
            return role.to_string();
        }
    }
    "com.apple.safari".into()
}

fn choose() -> Option<Found> {
    // A browser named outright wins (for trying one that is not the default).
    if let Some(p) = std::env::var_os("NIB_BROWSER").filter(|p| !p.is_empty()) {
        let exe = PathBuf::from(p);
        let file = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        let kind = if file.contains("firefox") { Kind::Firefox } else { Kind::Chromium };
        return exe.is_file().then(|| Found { name: exe.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), kind, exe, default: false });
    }
    let default = system_default();
    let installed: Vec<(&Known, PathBuf)> = KNOWN.iter().filter_map(|k| Some((k, k.installed()?))).collect();
    let pick = installed.iter().find(|(k, _)| k.ids.iter().any(|id| default.contains(id))).map(|f| (f, true)).or(installed.first().map(|f| (f, false)));
    pick.map(|((k, exe), default)| Found { name: k.name.to_string(), kind: k.kind, exe: exe.clone(), default })
}

/// The browser to show pages with, if there is one. Looked up once.
pub fn find() -> Option<Found> {
    static FOUND: std::sync::OnceLock<Option<Found>> = std::sync::OnceLock::new();
    FOUND.get_or_init(choose).clone()
}

/// What `state` says about it.
pub fn describe() -> Value {
    match find() {
        Some(f) => json!({ "name": f.name, "path": f.exe, "protocol": if f.kind == Kind::Firefox { "bidi" } else { "cdp" }, "default": f.default }),
        None => Value::Null,
    }
}

/// Firefox preferences for a browser nobody sees and only local apps are shown in: no first-run pages,
/// no questions, nothing remembered about people, nothing sent home.
const USER_JS: &str = r#"user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.cache.disk.enable", false);
user_pref("browser.cache.memory.enable", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("startup.homepage_welcome_url", "");
user_pref("startup.homepage_welcome_url.additional", "");
user_pref("browser.startup.page", 0);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("browser.tabs.warnOnClose", false);
user_pref("browser.warnOnQuit", false);
user_pref("app.update.auto", false);
user_pref("app.update.enabled", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);
user_pref("datareporting.healthreport.uploadEnabled", false);
user_pref("toolkit.telemetry.reporting.policy.firstRun", false);
user_pref("toolkit.telemetry.enabled", false);
user_pref("browser.formfill.enable", false);
user_pref("signon.rememberSignons", false);
user_pref("signon.autofillForms", false);
user_pref("extensions.formautofill.addresses.enabled", false);
user_pref("extensions.formautofill.creditCards.enabled", false);
user_pref("identity.fxaccounts.enabled", false);
user_pref("browser.download.useDownloadDir", true);
user_pref("browser.download.folderList", 1);
user_pref("browser.download.alwaysOpenPanel", false);
user_pref("browser.download.manager.addToRecentDocs", false);
user_pref("focusmanager.testmode", true);
"#;

/// Letters, digits, `-` and `_` of the project folder's name: the profile folder's name.
fn slug(root: &str) -> String {
    let name: String = root
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(40)
        .collect();
    if name.is_empty() { "project".into() } else { name }
}

fn owner(profile: &Path) -> Option<(u32, u32)> {
    let v: Value = serde_json::from_slice(&std::fs::read(profile.join(OWNER_FILE)).ok()?).ok()?;
    Some((v["nib"].as_u64()? as u32, v["firefox"].as_u64()? as u32))
}

/// A profile folder for this window: the project's, or `<project>-2`, … when another nib window
/// that is still running has it. A browser left behind by a nib that is gone is stopped first.
/// Each browser keeps its own (`proj.firefox`, `proj.chrome`): their profiles are not alike.
fn claim(base: &Path, root: &str, browser: &str) -> Result<PathBuf> {
    let name = format!("{}.{}", slug(root), browser.to_lowercase());
    for n in 1..=20 {
        let dir = base.join(if n == 1 { name.clone() } else { format!("{name}-{n}") });
        match owner(&dir) {
            Some((nib, _)) if nib != std::process::id() && sys::alive(nib) => continue,
            Some((_, firefox)) => {
                if sys::alive(firefox) && sys::is_browser(firefox) {
                    sys::kill(firefox);
                    let until = Instant::now() + Duration::from_secs(5);
                    while sys::alive(firefox) && Instant::now() < until {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            None => {}
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        return Ok(dir);
    }
    bail!("twenty nib windows already show pages of this project")
}

/// A running browser that belongs to this window.
pub struct Process {
    child: Child,
    pub found: Found,
    pub profile: PathBuf,
    /// The port it listens on for nib (loopback), and the path to connect to there.
    pub port: u16,
    pub path: String,
}

/// What a Chromium browser is started with: unseen, asking nothing, sending nothing home, no
/// extensions, nothing kept of passwords.
const CHROMIUM_ARGS: &[&str] = &[
    "--headless=new",
    "--remote-debugging-port=0",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-extensions",
    "--disable-component-extensions-with-background-pages",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-sync",
    "--disable-search-engine-choice-screen",
    "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication",
    "--password-store=basic",
    "--use-mock-keychain",
];

/// Where the started browser says how to reach it: Firefox a JSON file with the port; Chromium
/// a file of two lines, the port and the path.
fn listening(kind: Kind, profile: &Path) -> Option<(u16, String)> {
    match kind {
        Kind::Firefox => {
            let v: Value = serde_json::from_slice(&std::fs::read(profile.join("WebDriverBiDiServer.json")).ok()?).ok()?;
            let port = v["ws_port"].as_u64().filter(|p| *p > 0 && *p <= u16::MAX as u64)?;
            Some((port as u16, "/session".into()))
        }
        Kind::Chromium => {
            let text = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
            let mut lines = text.lines();
            let port = lines.next()?.trim().parse::<u16>().ok().filter(|p| *p > 0)?;
            let path = lines.next()?.trim();
            path.starts_with('/').then(|| (port, path.to_string()))
        }
    }
}

impl Process {
    /// Start one for the project at `root`, with its profile under `base`, and wait until it
    /// can be talked to.
    pub fn start(base: &Path, root: &str) -> Result<Self> {
        let found = find().context("no browser nib can show pages with is installed (Chrome, Edge, Firefox, Brave or Chromium; NIB_BROWSER names one elsewhere)")?;
        let profile = claim(base, root, &found.name)?;
        let mut c = Command::new(&found.exe);
        match found.kind {
            Kind::Firefox => {
                std::fs::write(profile.join("user.js"), USER_JS)?;
                let _ = std::fs::remove_file(profile.join("WebDriverBiDiServer.json"));
                c.arg("--headless").arg("--remote-debugging-port=0").arg("--no-remote").arg("--profile").arg(&profile);
            }
            Kind::Chromium => {
                let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));
                c.args(CHROMIUM_ARGS).arg(format!("--user-data-dir={}", profile.display()));
            }
        }
        c.arg("about:blank").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let child = c.spawn().with_context(|| format!("starting {}", found.exe.display()))?;
        let mut fox = Self { child, found, profile, port: 0, path: String::new() };
        let me = std::process::id();
        let _ = std::fs::write(fox.profile.join(OWNER_FILE), json!({ "nib": me, "firefox": fox.child.id() }).to_string());
        // Whatever happens to this process (a crash, a kill, a window closed in a way that
        // skips our own cleanup), the watcher stops Firefox once we are gone.
        if let Ok(nib) = crate::app_bundle::exe() {
            let _ = Command::new(nib)
                .arg(WATCH_ARG)
                .arg(me.to_string())
                .arg(fox.child.id().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some((port, path)) = listening(fox.found.kind, &fox.profile) {
                (fox.port, fox.path) = (port, path);
                return Ok(fox);
            }
            if fox.gone() {
                bail!("{} ended before it could be used ({})", fox.found.name, fox.found.exe.display());
            }
            if Instant::now() > until {
                fox.stop();
                bail!("{} did not start listening within 30 s", fox.found.name);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// It has ended (the user cannot see it, so: it crashed or was killed).
    pub fn gone(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(self.profile.join(OWNER_FILE));
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `nib --web-watch NIB FIREFOX`: wait until that nib is gone, then stop that Firefox. Ends by
/// itself when the Firefox ends first. Started next to every Firefox, so no way of leaving nib
/// leaves a browser behind.
pub fn watch_main(args: &[String]) -> i32 {
    let (Some(nib), Some(firefox)) = (args.first().and_then(|a| a.parse::<u32>().ok()), args.get(1).and_then(|a| a.parse::<u32>().ok())) else {
        return 2;
    };
    loop {
        if !sys::alive(firefox) {
            return 0;
        }
        if !sys::alive(nib) {
            if sys::is_browser(firefox) {
                sys::kill(firefox);
            }
            return 0;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(unix)]
mod sys {
    use std::process::Command;

    pub fn alive(pid: u32) -> bool {
        // Signal 0 only asks; "no permission" still means it is there.
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// Process ids are reused: before stopping one from a note on disk, check what it is (a
    /// browser nib started names nib's profile folder on its command line).
    pub fn is_browser(pid: u32) -> bool {
        Command::new("ps").args(["-p", &pid.to_string(), "-o", "command="]).output().is_ok_and(|o| {
            let c = String::from_utf8_lossy(&o.stdout).to_lowercase();
            c.contains("--remote-debugging-port") || super::KNOWN.iter().any(|k| c.contains(&k.name.to_lowercase()))
        })
    }

    pub fn kill(pid: u32) {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
}

#[cfg(windows)]
mod sys {
    use std::process::Command;

    fn listed(pid: u32) -> String {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase())
            .unwrap_or_default()
    }

    pub fn alive(pid: u32) -> bool {
        listed(pid).contains(&format!("\"{pid}\""))
    }

    pub fn is_browser(pid: u32) -> bool {
        let l = listed(pid);
        ["firefox", "chrome", "msedge", "brave"].iter().any(|n| l.contains(n))
    }

    pub fn kill(pid: u32) {
        let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_is_named_after_the_project_folder() {
        assert_eq!(slug("/Users/ada/src/tidepool"), "tidepool");
        assert_eq!(slug("/Users/ada/src/my project/"), "my_project");
        assert_eq!(slug("C:\\src\\app"), "app");
        assert_eq!(slug("/"), "project");
    }

    #[test]
    fn a_profile_held_by_a_running_nib_is_left_alone() {
        let base = std::env::temp_dir().join(format!("nib-web-claim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // Free: the project's own.
        let first = claim(&base, "/x/proj", "Firefox").unwrap();
        assert_eq!(first.file_name().unwrap(), "proj.firefox");
        assert_eq!(claim(&base, "/x/proj", "Chrome").unwrap().file_name().unwrap(), "proj.chrome");
        // Held by a process that is alive and is not us (pid 1 always is, on Unix): the next one.
        if cfg!(unix) {
            std::fs::write(first.join(OWNER_FILE), json!({ "nib": 1, "firefox": 0 }).to_string()).unwrap();
            assert_eq!(claim(&base, "/x/proj", "Firefox").unwrap().file_name().unwrap(), "proj.firefox-2");
        }
        // Held by a nib that is gone: taken back.
        std::fs::write(first.join(OWNER_FILE), json!({ "nib": 0x7fff_fff0u32, "firefox": 0x7fff_fff1u32 }).to_string()).unwrap();
        assert_eq!(claim(&base, "/x/proj", "Firefox").unwrap().file_name().unwrap(), "proj.firefox");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_macs_default_browser_is_read_from_its_handlers() {
        let handlers = r#"(
        {
        LSHandlerContentType = "public.html";
        LSHandlerRoleAll = "com.apple.safari";
    },
        {
        LSHandlerModificationDate = 0;
        LSHandlerPreferredVersions =         {
            LSHandlerRoleAll = "-";
        };
        LSHandlerRoleAll = "com.Google.Chrome";
        LSHandlerURLScheme = http;
    },
        {
        LSHandlerRoleAll = "org.mozilla.firefox";
        LSHandlerURLScheme = mailto;
    }
)"#
        .to_lowercase();
        assert_eq!(mac_default(&handlers), "com.google.chrome");
        // Nothing chosen: Safari.
        assert_eq!(mac_default(""), "com.apple.safari");
        assert_eq!(mac_default("(\n)"), "com.apple.safari");
        // And Safari is not a browser nib can drive: no entry of KNOWN is it.
        assert!(!KNOWN.iter().any(|k| k.ids.iter().any(|id| "com.apple.safari".contains(id))));
    }

    #[test]
    fn a_started_browser_says_where_it_listens() {
        let dir = std::env::temp_dir().join(format!("nib-web-port-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(listening(Kind::Chromium, &dir), None);
        std::fs::write(dir.join("DevToolsActivePort"), "56765\n/devtools/browser/de24-7119\n").unwrap();
        assert_eq!(listening(Kind::Chromium, &dir), Some((56765, "/devtools/browser/de24-7119".into())));
        std::fs::write(dir.join("DevToolsActivePort"), "56765\n").unwrap();
        assert_eq!(listening(Kind::Chromium, &dir), None, "half written: not yet");
        std::fs::write(dir.join("WebDriverBiDiServer.json"), r#"{"ws_host":"127.0.0.1","ws_port":54327}"#).unwrap();
        assert_eq!(listening(Kind::Firefox, &dir), Some((54327, "/session".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
