//! New window: another `nib` process, on the same machine, for another project.
//!
//! A window is a process (its own connection, control socket and run directory), so a new one
//! is this binary started again, detached: it outlives this window, and this window's quit or
//! crash leaves it alone.

use super::{App, Message};
use iced::Task;
use nib_editor::Target;
use std::process::{Command, Stdio};

/// The arguments that open `dir` (absolute, on the connected machine) in a new window like
/// this one; `browse`: with the folder browser up. None: this connection can't be named on a
/// command line (an SSH host that isn't a saved connection).
fn args(target: &Target, profile: Option<&str>, headless: bool, dir: &str, browse: bool) -> Option<Vec<String>> {
    let mut a = Vec::new();
    if headless {
        a.push("--headless".to_owned());
    }
    if browse {
        a.push("--browse".to_owned());
    }
    match (profile, target) {
        (Some(p), _) => a.push(format!("@{p}")),
        (None, Target::Local { .. }) => {}
        (None, _) => return None,
    }
    a.push(dir.to_owned());
    Some(a)
}

/// The folder `dir` is in; `dir` itself at the top. Local paths go by the OS's rules, remote
/// ones are unix paths.
fn parent_of(dir: &str, local: bool) -> String {
    if local {
        return std::path::Path::new(dir).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| dir.to_owned());
    }
    match dir.trim_end_matches('/').rsplit_once('/') {
        Some(("", _)) => "/".into(),
        Some((p, _)) => p.to_owned(),
        None => dir.to_owned(),
    }
}

/// Start this binary with `args`, on its own: no terminal, no wait, its own process group.
fn spawn(args: &[String]) -> std::io::Result<()> {
    // The path it was started by: inside nibble.app on macOS, so the new one needn't re-exec.
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // A socket picked for this window (or a terminal's `nib ctl` target) isn't the new one's.
    if !matches!(std::env::var("NIB_CONTROL").as_deref(), Ok("off" | "0" | "")) {
        cmd.env_remove("NIB_CONTROL");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = cmd.spawn()?;
    // Collect its exit status when it ends, so it doesn't linger as a zombie.
    std::thread::spawn(move || drop(child.wait()));
    Ok(())
}

impl App {
    /// New window (⌘N): on this machine, the folder browser up at the folder this project is
    /// in, so a sibling project is a keystroke away.
    pub(super) fn new_window_here(&mut self) -> Task<Message> {
        let local = matches!(self.launch.target, Target::Local { .. });
        let dir = parent_of(&self.root, local);
        self.new_window(dir, true)
    }

    /// Open `dir` (absolute, on the connected machine) in a new window; `browse`: let the
    /// person pick the folder there first.
    pub(super) fn new_window(&mut self, dir: String, browse: bool) -> Task<Message> {
        let Some(args) = args(&self.launch.target, self.launch.profile.as_deref(), self.headless, &dir, browse) else {
            return self.flash_warn(format!("{} isn't a saved connection: save it (SSH panel) to open it in another window", self.label));
        };
        crate::control::log("window", &format!("new window: {}", args.join(" ")));
        match spawn(&args) {
            Ok(()) if browse => self.flash_info(format!("New window on {}…", self.label)),
            Ok(()) => self.flash_info(format!("Opening {} in a new window…", self.tilde(&dir))),
            Err(e) => self.flash_warn(format!("couldn't open a new window: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_name_the_connection() {
        let local = Target::Local { nibd: None };
        assert_eq!(args(&local, None, false, "/home/ada/tidepool", false).unwrap(), ["/home/ada/tidepool"]);
        assert_eq!(args(&local, None, true, "/home/ada", true).unwrap(), ["--headless", "--browse", "/home/ada"]);
        let wsl = Target::Wsl { distro: "Ubuntu".into(), nibd: None };
        assert_eq!(args(&wsl, Some("box"), false, "/srv/tidepool", false).unwrap(), ["@box", "/srv/tidepool"]);
        assert!(args(&wsl, None, false, "/srv", false).is_none());
    }

    #[test]
    fn parent_folder() {
        assert_eq!(parent_of("/home/ada/tidepool", false), "/home/ada");
        assert_eq!(parent_of("/home/ada/tidepool/", false), "/home/ada");
        assert_eq!(parent_of("/home", false), "/");
        assert_eq!(parent_of("/", false), "/");
        assert_eq!(parent_of("/home/ada/tidepool", true), "/home/ada");
        assert_eq!(parent_of("/", true), "/");
    }
}
