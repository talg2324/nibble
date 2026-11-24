//! The clipboard, for copy and paste anywhere in the window: iced's, except under WSL, where
//! it is Windows' own.
//!
//! In WSL nib is a Wayland window drawn by WSLg, whose clipboard WSLg passes to and from
//! Windows unreliably: a copy in nib often never reaches Notepad, a copy in Notepad never
//! reaches nib. So there a copy is also handed to `clip.exe`, and a paste reads Windows'
//! clipboard through PowerShell (about half a second: it is slow to start), falling back to
//! iced's when PowerShell can't be run.

use iced::Task;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// nib runs in WSL (Linux nib on Windows, through WSLg).
pub fn is_wsl() -> bool {
    static WSL: OnceLock<bool> = OnceLock::new();
    *WSL.get_or_init(|| {
        cfg!(target_os = "linux")
            && (std::env::var_os("WSL_DISTRO_NAME").is_some()
                || std::fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|r| r.to_ascii_lowercase().contains("microsoft")))
    })
}

/// Put `text` on the clipboard.
pub fn write<T>(text: String) -> Task<T> {
    if is_wsl() {
        let copy = text.clone();
        std::thread::spawn(move || {
            if let Err(e) = to_windows(&copy) {
                crate::control::log("warn", &format!("copy to the Windows clipboard (clip.exe): {e}"));
            }
        });
    }
    iced::clipboard::write(text)
}

/// The text on the clipboard (`None`: none, or no text).
pub fn read() -> Task<Option<String>> {
    if !is_wsl() {
        return iced::clipboard::read();
    }
    Task::perform(async { windows_text() }, |t| t).then(|t| match t {
        Some(t) => Task::done(Some(t)),
        None => iced::clipboard::read(),
    })
}

/// What `clip.exe` reads: UTF-16LE with a byte-order mark, the one input it takes without
/// mangling what isn't ASCII.
fn utf16_for_clip(text: &str) -> Vec<u8> {
    let mut out = vec![0xff, 0xfe];
    out.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    out
}

fn to_windows(text: &str) -> std::io::Result<()> {
    let mut child = Command::new("clip.exe").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    child.stdin.take().expect("piped").write_all(&utf16_for_clip(text))?;
    child.wait()?;
    Ok(())
}

/// Windows' clipboard text, line ends as Linux has them, under WSL. `Some("")` when it holds
/// no text; `None` off WSL, or when PowerShell couldn't be asked. Blocking.
pub fn windows_text() -> Option<String> {
    if !is_wsl() {
        return None;
    }
    const SCRIPT: &str = "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
        $t = Get-Clipboard -Raw; if ($t) { [Console]::Out.Write($t) }";
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(from_windows_text(&out.stdout))
}

fn from_windows_text(bytes: &[u8]) -> String {
    let t = String::from_utf8_lossy(bytes);
    t.strip_prefix('\u{feff}').unwrap_or(&t).replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_for_clip_exe_is_utf16le_with_a_bom() {
        assert_eq!(utf16_for_clip("hé"), [0xff, 0xfe, b'h', 0, 0xe9, 0]);
        assert_eq!(utf16_for_clip("🦀").len(), 2 + 4, "a surrogate pair");
    }

    #[test]
    fn windows_text_gets_linux_line_ends() {
        assert_eq!(from_windows_text("\u{feff}a\r\nb\r\n".as_bytes()), "a\nb\n");
        assert_eq!(from_windows_text(b""), "");
    }
}
