//! Syntax highlighting for nib.
//!
//! The languages nib ships with are linked statically, with Helix's queries embedded (vendored
//! under `runtime/queries`, pinned to the same Helix tag as helix-core); more are loaded at
//! runtime from the config dir (`grammars/<name>.dylib` + `queries/<name>/`, put there by
//! `nib grammar add`). Either way nib drives `tree-house` (Helix's tree-sitter layer) directly,
//! not `helix_core::syntax::Loader`, which needs a whole Helix runtime dir. D-004, D-059.

use helix_core::{ChangeSet, Operation, Rope, RopeSlice};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tree_house::highlighter::{Highlight, HighlightEvent, Highlighter};
use tree_house::tree_sitter::{Grammar, InputEdit, Point};
use tree_house::{InjectionLanguageMarker, Language, LanguageConfig, LanguageLoader};

/// Highlight scopes nib understands. A capture like `function.builtin.special` resolves to the
/// longest dotted prefix present here. Index in this slice == `Highlight::get()`.
pub const SCOPES: &[&str] = &[
    "attribute",
    "comment",
    "comment.block.documentation",
    "constant",
    "constant.builtin",
    "constant.builtin.boolean",
    "constant.character.escape",
    "constant.numeric",
    "constructor",
    "function",
    "function.builtin",
    "function.macro",
    "function.method",
    "function.special",
    "keyword",
    "keyword.control",
    "keyword.control.conditional",
    "keyword.control.exception",
    "keyword.control.import",
    "keyword.control.repeat",
    "keyword.control.return",
    "keyword.directive",
    "keyword.function",
    "keyword.operator",
    "keyword.storage",
    "label",
    "markup.bold",
    "markup.heading",
    "markup.heading.marker",
    "markup.italic",
    "markup.link.label",
    "markup.link.text",
    "markup.link.url",
    "markup.list",
    "markup.quote",
    "markup.raw",
    "markup.strikethrough",
    "namespace",
    "operator",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "special",
    "string",
    "string.regexp",
    "string.special",
    "type",
    "type.builtin",
    "type.enum.variant",
    "variable",
    "variable.builtin",
    "variable.other.member",
    "variable.parameter",
];

pub fn scope_name(h: Highlight) -> &'static str {
    SCOPES[h.get() as usize]
}

struct LangDef {
    name: String,
    /// Extra names an injection may use for this language (e.g. a Markdown fence tagged `py`).
    aliases: Vec<String>,
    extensions: Vec<String>,
    filenames: Vec<String>,
    shebangs: Vec<String>,
    config: LanguageConfig,
}

pub struct Registry {
    langs: Vec<LangDef>,
    errors: Vec<String>,
}

/// The Helix release nib's queries (built-in and installed by `nib grammar add`) come from, and
/// whose `languages.toml` names each grammar's source. Same tag as helix-core in Cargo.toml.
pub const HELIX_TAG: &str = "25.07.1";

/// A language added at runtime (`[languages.<name>]` in config.toml): a grammar compiled to a
/// shared library in `<config dir>/grammars/` and queries in `<config dir>/queries/<name>/`.
/// A built-in's name replaces the built-in.
#[derive(Debug, Clone, Default)]
pub struct RuntimeLanguage {
    pub name: String,
    /// File stem of the library and its `tree_sitter_<grammar>` symbol.
    pub grammar: String,
    pub extensions: Vec<String>,
    pub filenames: Vec<String>,
    pub shebangs: Vec<String>,
    pub aliases: Vec<String>,
}

/// `grammars/<grammar>.{dylib,so,dll}` under the config dir.
pub fn grammar_path(config_dir: &Path, grammar: &str) -> PathBuf {
    config_dir.join("grammars").join(format!("{grammar}.{}", std::env::consts::DLL_EXTENSION))
}

struct Runtime {
    dir: Option<PathBuf>,
    langs: Vec<RuntimeLanguage>,
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Where runtime grammars and queries live (the config file's directory) and which languages to
/// load from there. Call before the first `registry()`; later calls are ignored.
pub fn configure(config_dir: Option<PathBuf>, langs: Vec<RuntimeLanguage>) {
    let _ = RUNTIME.set(Runtime { dir: config_dir, langs });
}

macro_rules! q {
    ($lang:literal, $file:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../runtime/queries/",
            $lang,
            "/",
            $file
        ))
    };
}

/// Queries compiled into the binary.
fn embedded(lang: &str, file: &str) -> Option<&'static str> {
    Some(match (lang, file) {
        ("python", "highlights.scm") => q!("python", "highlights.scm"),
        ("python", "injections.scm") => q!("python", "injections.scm"),
        ("python", "locals.scm") => q!("python", "locals.scm"),
        ("bash", "highlights.scm") => q!("bash", "highlights.scm"),
        ("bash", "injections.scm") => q!("bash", "injections.scm"),
        ("json", "highlights.scm") => q!("json", "highlights.scm"),
        ("yaml", "highlights.scm") => q!("yaml", "highlights.scm"),
        ("yaml", "injections.scm") => q!("yaml", "injections.scm"),
        ("toml", "highlights.scm") => q!("toml", "highlights.scm"),
        ("toml", "injections.scm") => q!("toml", "injections.scm"),
        ("markdown", "highlights.scm") => q!("markdown", "highlights.scm"),
        ("markdown", "injections.scm") => q!("markdown", "injections.scm"),
        ("markdown.inline", "highlights.scm") => q!("markdown.inline", "highlights.scm"),
        ("markdown.inline", "injections.scm") => q!("markdown.inline", "injections.scm"),
        ("html", "highlights.scm") => q!("html", "highlights.scm"),
        ("html", "injections.scm") => q!("html", "injections.scm"),
        _ => return None,
    })
}

/// `queries/<lang>/<file>` from the config dir if there is one, else the embedded copy; Helix's
/// `; inherits: a,b` lines are replaced by those languages' same file.
fn read_query(dir: Option<&Path>, lang: &str, file: &str, depth: u32) -> String {
    let text = dir
        .and_then(|d| std::fs::read_to_string(d.join("queries").join(lang).join(file)).ok())
        .or_else(|| embedded(lang, file).map(str::to_string))
        .unwrap_or_default();
    if depth > 8 || !text.contains("; inherits:") {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        match line.trim().strip_prefix("; inherits:") {
            Some(parents) => {
                for p in parents.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    out.push_str(&read_query(dir, p, file, depth + 1));
                    out.push('\n');
                }
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

fn resolve_scope(capture: &str) -> Option<Highlight> {
    let mut name = capture;
    loop {
        if let Some(i) = SCOPES.iter().position(|s| *s == name) {
            return Some(Highlight::new(i as u32));
        }
        name = &name[..name.rfind('.')?];
    }
}

fn build(name: &str, grammar: Grammar, dir: Option<&Path>) -> anyhow::Result<LanguageConfig> {
    let q = |file| read_query(dir, name, file, 0);
    let cfg = LanguageConfig::new(grammar, &q("highlights.scm"), &q("injections.scm"), &q("locals.scm"))
        .map_err(|e| anyhow::anyhow!("{name} queries: {e}"))?;
    cfg.configure(resolve_scope);
    Ok(cfg)
}

/// A grammar linked into the binary, with how files are matched to it.
struct Builtin {
    name: &'static str,
    grammar: tree_sitter_language::LanguageFn,
    aliases: &'static [&'static str],
    extensions: &'static [&'static str],
    filenames: &'static [&'static str],
    shebangs: &'static [&'static str],
}

/// The languages compiled into nib.
pub const BUILTIN_LANGUAGES: &[&str] = &["python", "bash", "json", "yaml", "toml", "markdown", "html"];

const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "python",
        grammar: tree_sitter_python::LANGUAGE,
        aliases: &["py", "python3"],
        extensions: &["py", "pyi", "pyw"],
        filenames: &[],
        shebangs: &["python", "python3", "uv"],
    },
    Builtin {
        name: "bash",
        grammar: tree_sitter_bash::LANGUAGE,
        aliases: &["sh", "shell", "zsh", "console"],
        extensions: &["sh", "bash", "zsh"],
        filenames: &[".bashrc", ".bash_profile", ".zshrc", ".profile", "PKGBUILD"],
        shebangs: &["sh", "bash", "zsh", "dash"],
    },
    Builtin {
        name: "json",
        grammar: tree_sitter_json::LANGUAGE,
        aliases: &["jsonc"],
        extensions: &["json", "jsonc", "jsonl", "ipynb", "geojson", "webmanifest", "map"],
        filenames: &[".babelrc", ".watchmanconfig", "composer.lock", "flake.lock"],
        shebangs: &[],
    },
    Builtin {
        name: "yaml",
        grammar: tree_sitter_yaml::LANGUAGE,
        aliases: &["yml"],
        extensions: &["yml", "yaml"],
        filenames: &[".prettierrc", ".clangd", ".clang-format", ".clang-tidy"],
        shebangs: &[],
    },
    Builtin {
        name: "toml",
        grammar: tree_sitter_toml_ng::LANGUAGE,
        aliases: &[],
        extensions: &["toml"],
        filenames: &["Cargo.lock", "uv.lock", "poetry.lock", "pdm.lock"],
        shebangs: &[],
    },
    Builtin {
        name: "markdown",
        grammar: tree_sitter_md::LANGUAGE,
        aliases: &["md"],
        extensions: &["md", "markdown", "mdx", "mkd", "mkdn", "mdwn", "mdown", "livemd"],
        filenames: &[],
        shebangs: &[],
    },
    Builtin {
        name: "html",
        grammar: tree_sitter_html::LANGUAGE,
        aliases: &["htm", "xhtml"],
        extensions: &["html", "htm", "xhtml", "shtml"],
        filenames: &[],
        shebangs: &[],
    },
    // Injected into markdown's `inline` nodes; never detected for a file on its own.
    Builtin {
        name: "markdown.inline",
        grammar: tree_sitter_md::INLINE_LANGUAGE,
        aliases: &[],
        extensions: &[],
        filenames: &[],
        shebangs: &[],
    },
];

impl Registry {
    fn load(rt: Option<&Runtime>) -> Self {
        let dir = rt.and_then(|r| r.dir.as_deref());
        let strings = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut errors = Vec::new();
        let mut langs = Vec::new();
        for b in BUILTINS {
            let grammar = Grammar::try_from(b.grammar).expect("bundled grammar");
            // Queries in the config dir may override the embedded ones; if they don't compile,
            // the embedded ones still do.
            let config = match (build(b.name, grammar, dir), dir) {
                (Ok(c), _) => c,
                (Err(e), Some(d)) => {
                    errors.push(format!("{e} (in {}; using the built-in queries)", d.display()));
                    build(b.name, grammar, None).expect("bundled queries must compile")
                }
                (Err(e), None) => panic!("bundled queries must compile: {e}"),
            };
            langs.push(LangDef {
                name: b.name.into(),
                aliases: strings(b.aliases),
                extensions: strings(b.extensions),
                filenames: strings(b.filenames),
                shebangs: strings(b.shebangs),
                config,
            });
        }
        for r in rt.map_or(&[][..], |r| &r.langs) {
            let Some(dir) = dir else { break };
            match load_runtime(dir, r) {
                Ok(config) => {
                    let def = LangDef {
                        name: r.name.clone(),
                        aliases: r.aliases.clone(),
                        extensions: r.extensions.clone(),
                        filenames: r.filenames.clone(),
                        shebangs: r.shebangs.clone(),
                        config,
                    };
                    match langs.iter().position(|l| l.name == r.name) {
                        Some(i) => langs[i] = def,
                        None => langs.push(def),
                    }
                }
                Err(e) => errors.push(format!("language {}: {e:#}", r.name)),
            }
        }
        Self { langs, errors }
    }

    /// Runtime languages that failed to load, and query overrides that didn't compile.
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn name(&self, lang: Language) -> &str {
        &self.langs[lang.idx()].name
    }

    /// Every language, built-in and runtime.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.langs.iter().map(|l| l.name.as_str())
    }

    pub fn by_name(&self, name: &str) -> Option<Language> {
        self.langs
            .iter()
            .position(|l| l.name == name)
            .map(|i| Language::new(i as u32))
    }

    // Runtime languages come after the built-ins, so the searches below go from the end: a
    // language the user added wins over a built-in that claims the same file.

    /// A language named by an injection (a Markdown fence's tag): its name, an alias or one of
    /// its extensions, case-insensitively.
    fn by_injection_name(&self, name: &str) -> Option<Language> {
        let name = name.trim().to_ascii_lowercase();
        let named = self.langs.iter().rposition(|l| l.name == name || l.aliases.contains(&name));
        named
            .or_else(|| self.langs.iter().rposition(|l| l.extensions.contains(&name)))
            .map(|i| Language::new(i as u32))
    }

    /// Detect from file name, then from a shebang on the first line.
    pub fn detect(&self, path: &Path, text: RopeSlice) -> Option<Language> {
        let fname = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let by_path = self.langs.iter().rposition(|l| {
            l.filenames.iter().any(|f| f == fname) || l.extensions.iter().any(|e| e == ext)
        });
        by_path
            .or_else(|| self.detect_shebang(text))
            .map(|i| Language::new(i as u32))
    }

    fn detect_shebang(&self, text: RopeSlice) -> Option<usize> {
        if text.len_lines() == 0 {
            return None;
        }
        let line: String = text.line(0).chars().take(128).collect();
        let rest = line.strip_prefix("#!")?;
        let mut parts = rest.split_whitespace();
        let mut prog = parts.next()?.rsplit('/').next()?;
        if prog == "env" {
            prog = parts.find(|p| !p.starts_with('-'))?;
        }
        let prog = prog.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
        self.langs.iter().rposition(|l| {
            l.shebangs
                .iter()
                .any(|s| s.trim_end_matches(|c: char| c.is_ascii_digit()) == prog)
        })
    }
}

/// Whether `r`'s grammar and queries in `dir` load (`nib grammar add` / `list`).
pub fn check_runtime(dir: &Path, r: &RuntimeLanguage) -> anyhow::Result<()> {
    load_runtime(dir, r).map(drop)
}

fn load_runtime(dir: &Path, r: &RuntimeLanguage) -> anyhow::Result<LanguageConfig> {
    let path = grammar_path(dir, &r.grammar);
    if !path.is_file() {
        anyhow::bail!("{} is missing; run `nib grammar add {}`", path.display(), r.name);
    }
    // SAFETY: a library in the user's own config dir, put there to be loaded as a grammar;
    // tree-house checks its ABI version.
    let grammar = unsafe { Grammar::new(&r.grammar, &path) }?;
    build(&r.name, grammar, Some(dir))
}

impl LanguageLoader for Registry {
    fn language_for_marker(&self, marker: InjectionLanguageMarker) -> Option<Language> {
        match marker {
            InjectionLanguageMarker::Name(name) => self.by_injection_name(name),
            InjectionLanguageMarker::Match(text) => {
                self.by_injection_name(&String::from(text))
            }
            InjectionLanguageMarker::Filename(text) => {
                self.detect(Path::new(&String::from(text)), RopeSlice::from(""))
            }
            InjectionLanguageMarker::Shebang(text) => {
                let s = format!("#!{}", String::from(text));
                self.detect_shebang(RopeSlice::from(s.as_str()))
                    .map(|i| Language::new(i as u32))
            }
        }
    }

    fn get_config(&self, lang: Language) -> Option<&LanguageConfig> {
        self.langs.get(lang.idx()).map(|l| &l.config)
    }
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

/// Built on first use, with what `configure` set (built-ins only if it wasn't called). Query
/// compilation takes tens of milliseconds, so clients warm it on a background thread and attach
/// syntax once `registry_if_ready` is `Some`.
pub fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| Registry::load(RUNTIME.get()))
}

pub fn registry_if_ready() -> Option<&'static Registry> {
    REGISTRY.get()
}

const PARSE_TIMEOUT: Duration = Duration::from_millis(500);

pub struct Syntax {
    inner: tree_house::Syntax,
    pub lang: Language,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub bytes: Range<usize>,
    pub scope: Option<Highlight>,
}

impl Syntax {
    pub fn new(text: &Rope, lang: Language) -> anyhow::Result<Self> {
        let inner = tree_house::Syntax::new(text.slice(..), lang, PARSE_TIMEOUT, registry())
            .map_err(|e| anyhow::anyhow!("parse: {e:?}"))?;
        Ok(Self { inner, lang })
    }

    /// Incremental reparse. `old` is the text before `changes` was applied, `new` after.
    pub fn update(&mut self, old: &Rope, new: &Rope, changes: &ChangeSet) -> anyhow::Result<()> {
        let edits = generate_edits(old.slice(..), changes);
        if edits.is_empty() {
            return Ok(());
        }
        self.inner
            .update(new.slice(..), PARSE_TIMEOUT, &edits, registry())
            .map_err(|e| anyhow::anyhow!("reparse: {e:?}"))
    }

    /// Flattened, non-overlapping highlight spans covering `bytes` (innermost capture wins).
    pub fn spans(&self, text: &Rope, bytes: Range<usize>) -> Vec<Span> {
        let start = bytes.start as u32;
        let end = bytes.end as u32;
        let mut out = Vec::new();
        let mut hl = Highlighter::new(&self.inner, text.slice(..), registry(), start..end);
        let mut stack: Vec<Highlight> = Vec::new();
        let mut pos = start;
        loop {
            let next = hl.next_event_offset().min(end);
            if next > pos {
                push_span(&mut out, pos as usize..next as usize, stack.last().copied());
                pos = next;
            }
            if next >= end {
                break;
            }
            let (ev, list) = hl.advance();
            match ev {
                HighlightEvent::Refresh => {
                    stack.clear();
                    stack.extend(list);
                }
                HighlightEvent::Push => stack.extend(list),
            }
        }
        out
    }
}

fn push_span(out: &mut Vec<Span>, bytes: Range<usize>, scope: Option<Highlight>) {
    if let Some(last) = out.last_mut() {
        if last.scope == scope && last.bytes.end == bytes.start {
            last.bytes.end = bytes.end;
            return;
        }
    }
    out.push(Span { bytes, scope });
}

/// A bundled language as the settings UI sees it: its grammar's highlight query and a sample
/// snippet that exercises most of its scopes.
pub struct LanguageInfo {
    pub name: &'static str,
    /// File name the sample is attached to (drives language detection).
    pub sample_path: &'static str,
    pub sample: &'static str,
    /// An identifier in `sample` the preview selects, to show the selection colour.
    pub sample_word: &'static str,
    /// Highlight queries whose captures can show up in a file of this language.
    highlights: &'static [&'static str],
}

pub const LANGUAGES: &[LanguageInfo] = &[
    LanguageInfo {
        name: "python",
        sample_path: "sample.py",
        sample: "import math\nfrom dataclasses import dataclass\n\n\n@dataclass\nclass Circle:\n    \"\"\"A circle in the plane.\"\"\"\n    radius: float = 1.0\n\n    def area(self) -> float:\n        # πr²\n        return math.pi * self.radius ** 2\n\n\nfor r in range(3):\n    if r and r % 2 == 0:\n        print(f\"r={r}\", Circle(r).area(), None, True)\n",
        sample_word: "area",
        highlights: &[q!("python", "highlights.scm")],
    },
    LanguageInfo {
        name: "bash",
        sample_path: "sample.sh",
        sample: "#!/usr/bin/env bash\nset -euo pipefail\n\n# Back up every log file\nreadonly DEST=\"${HOME}/backup\"\n\nbackup() {\n    local src=$1\n    cp -- \"$src\" \"$DEST/\" && echo \"copied $src\"\n}\n\nfor f in /var/log/*.log; do\n    if [[ -f $f ]]; then\n        backup \"$f\" || exit 1\n    fi\ndone\n",
        sample_word: "backup",
        highlights: &[q!("bash", "highlights.scm")],
    },
    LanguageInfo {
        name: "json",
        sample_path: "sample.json",
        sample: r#"{
  "name": "nib",
  "version": 1.5,
  "private": true,
  "license": null,
  "keywords": ["editor", "remote\tssh"],
  "scripts": {
    "build": "cargo build --release"
  }
}
"#,
        sample_word: "keywords",
        highlights: &[q!("json", "highlights.scm")],
    },
    LanguageInfo {
        name: "yaml",
        sample_path: "sample.yaml",
        sample: r#"# CI pipeline
name: build
on: [push, pull_request]

defaults: &defaults
  timeout: 30
  retry: true

jobs:
  test:
    <<: *defaults
    image: "rust:1.82"
    ratio: 0.75
    script: |
      cargo test --workspace
"#,
        sample_word: "image",
        highlights: &[q!("yaml", "highlights.scm")],
    },
    LanguageInfo {
        name: "toml",
        sample_path: "sample.toml",
        sample: r#"# Package manifest
[package]
name = "nib"
version = "0.1.0"
publish = false

[dependencies]
serde = { version = "1", features = ["derive"] }

[[bin]]
path = 'src/main.rs'
threads = 8
ratio = 0.5
built = 2026-01-01T00:00:00Z
"#,
        sample_word: "features",
        highlights: &[q!("toml", "highlights.scm")],
    },
    LanguageInfo {
        name: "markdown",
        sample_path: "sample.md",
        sample: r#"# nib

A *small*, **fast** editor with ~~no~~ `remote` editing.

## Install

1. Clone the [repository](https://example.com/nib)
2. Run the build script

- [x] highlighting
- [ ] plugins

> Edits happen on the client; files stay on the host.

```bash
./scripts/nib README.md
```
"#,
        sample_word: "editor",
        highlights: &[
            q!("markdown", "highlights.scm"),
            q!("markdown.inline", "highlights.scm"),
        ],
    },
    LanguageInfo {
        name: "html",
        sample_path: "sample.html",
        sample: r#"<!DOCTYPE html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <title>nib</title>
  </head>
  <body>
    <!-- main content -->
    <h1 class="title">Hello &amp; welcome</h1>
    <p>A <a href="https://example.com">link</a> and an image:</p>
    <img src="logo.png" alt="logo" width="64">
  </body>
</html>
"#,
        sample_word: "welcome",
        highlights: &[q!("html", "highlights.scm")],
    },
];

impl LanguageInfo {
    /// The scopes (from `SCOPES`) this language's highlight query can produce, in `SCOPES` order.
    pub fn scopes(&self) -> Vec<&'static str> {
        let mut used = vec![false; SCOPES.len()];
        for q in self.highlights {
            for (i, _) in q.match_indices('@') {
                // Skip "@" string literals (e.g. Python decorators) and comments.
                if q[..i].ends_with('"') {
                    continue;
                }
                let line_start = q[..i].rfind('\n').map_or(0, |n| n + 1);
                if q[line_start..i].contains(';') {
                    continue;
                }
                let cap: &str = q[i + 1..]
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_'))
                    .next()
                    .unwrap_or("");
                if cap.is_empty() || cap.starts_with('_') {
                    continue;
                }
                if let Some(h) = resolve_scope(cap) {
                    used[h.get() as usize] = true;
                }
            }
        }
        SCOPES
            .iter()
            .zip(used)
            .filter_map(|(s, u)| u.then_some(*s))
            .collect()
    }
}

/// Port of `helix_core::syntax::generate_edits` (private upstream). MPL-2.0, Helix contributors.
fn generate_edits(old_text: RopeSlice, changeset: &ChangeSet) -> Vec<InputEdit> {
    use Operation::*;
    let mut old_pos = 0;
    let mut edits = Vec::new();
    let mut iter = changeset.changes().iter().peekable();
    let edit = |start_byte: u32, old_end_byte: u32, new_end_byte: u32| InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte,
        start_point: Point::ZERO,
        old_end_point: Point::ZERO,
        new_end_point: Point::ZERO,
    };
    while let Some(change) = iter.next() {
        let len = match change {
            Delete(i) | Retain(i) => *i,
            Insert(_) => 0,
        };
        let mut old_end = old_pos + len;
        match change {
            Retain(_) => {}
            Delete(_) => {
                let s = old_text.char_to_byte(old_pos) as u32;
                edits.push(edit(s, old_text.char_to_byte(old_end) as u32, s));
            }
            Insert(ins) => {
                let s = old_text.char_to_byte(old_pos) as u32;
                if let Some(Delete(dlen)) = iter.peek() {
                    old_end = old_pos + dlen;
                    let oe = old_text.char_to_byte(old_end) as u32;
                    iter.next();
                    edits.push(edit(s, oe, s + ins.len() as u32));
                } else {
                    edits.push(edit(s, s, s + ins.len() as u32));
                }
            }
        }
        old_pos = old_end;
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::{Selection, Transaction};

    fn scopes_at(text: &str, needle: &str, syn: &Syntax, rope: &Rope) -> Option<&'static str> {
        let b = text.find(needle).unwrap();
        syn.spans(rope, 0..rope.len_bytes())
            .into_iter()
            .find(|s| s.bytes.contains(&b))
            .and_then(|s| s.scope.map(scope_name))
    }

    #[test]
    fn python_highlights() {
        let src =
            "import os\n\ndef f(x: int) -> str:\n    \"\"\"doc\"\"\"\n    return str(x + 1)  # c\n";
        let rope = Rope::from(src);
        let reg = registry();
        let lang = reg.detect(Path::new("a.py"), rope.slice(..)).unwrap();
        assert_eq!(reg.name(lang), "python");
        let syn = Syntax::new(&rope, lang).unwrap();
        assert!(scopes_at(src, "import", &syn, &rope)
            .unwrap()
            .starts_with("keyword"));
        assert!(scopes_at(src, "def", &syn, &rope)
            .unwrap()
            .starts_with("keyword"));
        assert_eq!(scopes_at(src, "f(x", &syn, &rope), Some("function"));
        assert!(scopes_at(src, "int", &syn, &rope)
            .unwrap()
            .starts_with("type"));
        assert!(scopes_at(src, "# c", &syn, &rope)
            .unwrap()
            .starts_with("comment"));
        assert!(scopes_at(src, "1)", &syn, &rope)
            .unwrap()
            .starts_with("constant.numeric"));
    }

    #[test]
    fn bash_and_shebang() {
        let src = "#!/usr/bin/env bash\nfor i in 1 2; do echo \"$i\"; done\n";
        let rope = Rope::from(src);
        let lang = registry()
            .detect(Path::new("script"), rope.slice(..))
            .unwrap();
        assert_eq!(registry().name(lang), "bash");
        let syn = Syntax::new(&rope, lang).unwrap();
        assert!(scopes_at(src, "for", &syn, &rope)
            .unwrap()
            .starts_with("keyword"));
    }

    /// Detects `path`, parses `src`, and checks each `(needle, scope prefix)` pair.
    fn check(path: &str, name: &str, src: &str, expect: &[(&str, &str)]) {
        let rope = Rope::from(src);
        let lang = registry().detect(Path::new(path), rope.slice(..)).unwrap();
        assert_eq!(registry().name(lang), name);
        let syn = Syntax::new(&rope, lang).unwrap();
        for (needle, scope) in expect {
            let got = scopes_at(src, needle, &syn, &rope);
            assert!(
                got.is_some_and(|g| g.starts_with(scope)),
                "{name}: {needle:?} is {got:?}, want {scope}"
            );
        }
    }

    #[test]
    fn json_highlights() {
        check(
            "a.json",
            "json",
            "{\"key\": [1, true, null, \"v\"]}\n",
            &[
                ("key", "variable.other.member"),
                ("1", "constant.numeric"),
                ("true", "constant.builtin"),
                ("v\"", "string"),
            ],
        );
    }

    #[test]
    fn yaml_highlights() {
        check(
            "a.yml",
            "yaml",
            "# c\nkey: value\nn: 3\nrun: echo hi\n",
            &[
                ("# c", "comment"),
                ("key", "variable.other.member"),
                ("value", "string"),
                ("3", "constant.numeric"),
            ],
        );
    }

    #[test]
    fn toml_highlights() {
        check(
            "Cargo.lock",
            "toml",
            "# c\n[package]\nname = \"x\"\nn = 3\nok = true\n",
            &[
                ("# c", "comment"),
                ("package", "type"),
                ("name", "variable.other.member"),
                ("\"x\"", "string"),
                ("3", "constant.numeric"),
                ("true", "constant.builtin"),
            ],
        );
    }

    #[test]
    fn markdown_highlights_and_injections() {
        check(
            "README.md",
            "markdown",
            "# Title\n\nSome **bold** and `code` and [text](http://x).\n\n```sh\nfor i in 1; do :; done\n```\n\n```python\ndef f(): pass\n```\n",
            &[
                ("Title", "markup.heading"),
                ("bold", "markup.bold"),
                ("code", "markup.raw"),
                ("text", "markup.link.text"),
                ("http", "markup.link.url"),
                // Fenced blocks are highlighted by the injected language, including via an alias.
                ("for", "keyword"),
                ("def", "keyword"),
            ],
        );
    }

    #[test]
    fn html_highlights() {
        check(
            "index.html",
            "html",
            "<!-- c -->\n<p class=\"x\">hi</p>\n",
            &[("<!-- c", "comment"), ("class", "attribute"), ("\"x\"", "string")],
        );
    }

    #[test]
    fn incremental_matches_full_reparse() {
        let src = "x = 1\ny = 'a'\n";
        let mut rope = Rope::from(src);
        let lang = registry().by_name("python").unwrap();
        let mut syn = Syntax::new(&rope, lang).unwrap();
        let old = rope.clone();
        let tx = Transaction::insert(&rope, &Selection::point(6), "def g():\n    pass\n".into());
        tx.apply(&mut rope);
        syn.update(&old, &rope, tx.changes()).unwrap();
        let fresh = Syntax::new(&rope, lang).unwrap();
        let n = rope.len_bytes();
        assert_eq!(syn.spans(&rope, 0..n), fresh.spans(&rope, 0..n));
    }

    #[test]
    fn language_scopes() {
        let py = LANGUAGES.iter().find(|l| l.name == "python").unwrap().scopes();
        for s in ["keyword", "string", "comment", "function", "constant.numeric"] {
            assert!(py.contains(&s), "python missing {s}: {py:?}");
        }
        let sh = LANGUAGES.iter().find(|l| l.name == "bash").unwrap().scopes();
        assert!(sh.contains(&"comment") && sh.contains(&"string"), "{sh:?}");
        // Every bundled language has a registry entry with the same name, its sample is detected
        // as that language, and the sample exercises the word the preview selects.
        for l in LANGUAGES {
            let lang = registry().by_name(l.name).unwrap();
            let rope = Rope::from(l.sample);
            assert_eq!(registry().detect(Path::new(l.sample_path), rope.slice(..)), Some(lang));
            assert!(l.sample.contains(l.sample_word), "{}", l.name);
            assert!(!l.scopes().is_empty(), "{}", l.name);
            let syn = Syntax::new(&rope, lang).unwrap();
            let spans = syn.spans(&rope, 0..rope.len_bytes());
            assert!(spans.iter().any(|s| s.scope.is_some()), "{}", l.name);
        }
    }
}
