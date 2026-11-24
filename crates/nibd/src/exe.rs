//! Finding the programs nibd starts for the user (language servers, compilers), and what to tell
//! the user when one isn't installed.

use std::path::PathBuf;

/// A bare command name looked up in `$VIRTUAL_ENV`, `$PATH`, and the usual install dirs. (Over
/// ssh nibd runs from a non-login shell, so `$PATH` often lacks `~/.local/bin`, `~/.cargo/bin`,
/// `~/go/bin`, or TeX's own folder.) Paths are left alone.
///
/// Never in the project itself (`.venv/bin`, `node_modules/.bin`): those come with whatever was
/// cloned or unpacked, and a server starts as soon as one of its files is opened, before anybody
/// has looked at the repository. A project's own program is named by path in the config.
pub fn find_exe(name: &str) -> Option<PathBuf> {
    if name.contains(std::path::is_separator) {
        return None;
    }
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    search_dirs().into_iter().map(|d| d.join(&exe)).find(|p| p.is_file())
}

fn search_dirs() -> Vec<PathBuf> {
    let bin = if cfg!(windows) { "Scripts" } else { "bin" };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(v) = std::env::var_os("VIRTUAL_ENV") {
        dirs.push(PathBuf::from(v).join(bin));
    }
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    if let Some(h) = nib_proto::path::home_dir() {
        dirs.extend([h.join(".local/bin"), h.join(".cargo/bin"), h.join("go/bin")]);
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs.extend(tex_dirs());
    dirs
}

/// Where TeX distributions put `latexmk` and friends, outside `$PATH` for a non-login shell:
/// MacTeX's `/Library/TeX/texbin`, TeX Live's `/usr/local/texlive/<year>/bin/<platform>` (newest
/// year first), MiKTeX's per-user folder on Windows.
fn tex_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/Library/TeX/texbin")];
    if let Ok(years) = std::fs::read_dir("/usr/local/texlive") {
        let mut years: Vec<PathBuf> = years.flatten().map(|e| e.path().join("bin")).filter(|p| p.is_dir()).collect();
        years.sort();
        for bin in years.into_iter().rev() {
            if let Ok(platforms) = std::fs::read_dir(&bin) {
                dirs.extend(platforms.flatten().map(|e| e.path()));
            }
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join(r"Programs\MiKTeX\miktex\bin\x64"));
    }
    dirs
}

/// How to install `program` on this machine, for the programs nib knows by default.
pub fn install_hint(program: &str) -> Option<&'static str> {
    let (mac, linux, windows) = match program {
        "rust-analyzer" => ("rustup component add rust-analyzer", "rustup component add rust-analyzer", "rustup component add rust-analyzer"),
        "gopls" => ("go install golang.org/x/tools/gopls@latest", "go install golang.org/x/tools/gopls@latest", "go install golang.org/x/tools/gopls@latest"),
        "typescript-language-server" => (
            "npm install -g typescript-language-server typescript",
            "npm install -g typescript-language-server typescript",
            "npm install -g typescript-language-server typescript",
        ),
        "clangd" => ("brew install llvm", "sudo apt install clangd (or your distribution's clangd package)", "winget install LLVM.LLVM"),
        "tinymist" => (
            "brew install tinymist",
            "download it from https://github.com/Myriad-Dreamin/tinymist/releases",
            "download it from https://github.com/Myriad-Dreamin/tinymist/releases",
        ),
        "texlab" => (
            "brew install texlab",
            "download it from https://github.com/latex-lsp/texlab/releases",
            "download it from https://github.com/latex-lsp/texlab/releases",
        ),
        "typst" => ("brew install typst", "cargo install --locked typst-cli", "winget install --id Typst.Typst"),
        "latexmk" => (
            "install MacTeX: brew install --cask mactex-no-gui",
            "sudo apt install latexmk texlive-latex-recommended (or your distribution's TeX Live)",
            "install MiKTeX: winget install MiKTeX.MiKTeX",
        ),
        _ => return None,
    };
    Some(if cfg!(target_os = "macos") {
        mac
    } else if cfg!(windows) {
        windows
    } else {
        linux
    })
}

/// "`program` not found: install it with `…`" (or just "install it" for a program nib doesn't
/// know), followed by `otherwise`: where to set its path instead.
pub fn not_found(program: &str, what: &str, otherwise: &str) -> String {
    match install_hint(program) {
        Some(h) if h.contains(' ') && !h.starts_with("install ") && !h.starts_with("download ") => {
            format!("{program} not found: install it for {what} with `{h}`, or {otherwise}")
        }
        Some(h) => format!("{program} not found: to use it for {what}, {h}; or {otherwise}"),
        None => format!("{program} not found: install it for {what}, or {otherwise}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_not_searched_and_hints_name_the_command() {
        assert_eq!(find_exe("/no/such/typst"), None);
        let m = not_found("rust-analyzer", "rust files", "set `command` under [lsp.rust] in the config");
        assert!(m.starts_with("rust-analyzer not found: install it for rust files with `rustup component add rust-analyzer`, or set"), "{m}");
        let m = not_found("zls", "zig files", "set it");
        assert_eq!(m, "zls not found: install it for zig files, or set it");
    }
}
