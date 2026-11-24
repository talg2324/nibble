//! `nib grammar`: add languages to nib without rebuilding it.
//!
//! `nib grammar add rust` first looks for rust in the grammar bundle of nib's own release
//! (`nib-grammars-<version>-<triple>.tar.gz`, built by CI with `nib grammar build`, checked
//! against the release's `SHA256SUMS`, cached in `~/.cache/nib/`): no compiler or git needed.
//! A language the bundle doesn't have, or no bundle (a dev build, offline), is built from
//! source: the language is looked up in Helix's `languages.toml` at the tag nib's queries come
//! from (`nib_syntax::HELIX_TAG`), its grammar fetched at the pinned commit and compiled with the
//! system C compiler. Either way the grammar goes into `<config dir>/grammars/`, its queries
//! into `<config dir>/queries/`, the two are checked to load together, and only then is
//! `[languages.rust]` written into config.toml.

use anyhow::{anyhow, bail, Context, Result};
use nib_editor::config::LanguageEntry;
use nib_editor::Config;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "\
usage: nib grammar add [--from-source | --from DIR] LANGUAGE...
                                       turn on languages (rust, go, javascript, …): from nib's
                                       prebuilt bundle when it has them, else compiled from source
       nib grammar rm LANGUAGE...      turn them off and delete their files (also: remove)
       nib grammar list [--available]  languages nib highlights; --available: all it can add
       nib grammar build DIR [LANGUAGE...]
                                       compile a bundle into DIR (what releases ship; default:
                                       the languages nib offers on first run)

--from DIR uses an unpacked bundle instead of downloading one. Compiling needs git and `cc`
(or $CC; C++ scanners `c++` / $CXX; on Windows `clang`). Changes apply to nib windows opened
afterwards.";

/// Languages nib offers to add on first run (Settings → Languages). Each release prebuilds these.
pub const OFFERED: &[&str] = &["python", "rust", "go", "zig", "java", "javascript"];

/// Where releases live; `NIB_GRAMMARS_URL` replaces it (a mirror, or a test server).
const RELEASES: &str = "https://github.com/talg2324/nibble/releases/download";

pub fn grammar_main(args: &[String]) -> i32 {
    let r = match args.first().map(String::as_str) {
        Some("add") if args.len() > 1 => add_cmd(&args[1..]),
        Some("rm" | "remove") if args.len() > 1 => remove_all(&args[1..]),
        Some("list") => list(args.get(1).is_some_and(|a| a == "--available")),
        Some("build") if args.len() > 1 => {
            let offered: Vec<String> = OFFERED.iter().map(|s| s.to_string()).collect();
            build(Path::new(&args[1]), if args.len() > 2 { &args[2..] } else { &offered })
        }
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("nib grammar: {e:#}");
            1
        }
    }
}

fn add_cmd(args: &[String]) -> Result<()> {
    let mut source = Source::Release;
    let mut langs = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--from-source" => source = Source::Compile,
            "--from" => source = Source::Dir(it.next().context("--from needs a directory")?.into()),
            _ => langs.push(a.clone()),
        }
    }
    if langs.is_empty() {
        bail!("no language given");
    }
    add_all(&langs, &source)
}

/// The `[languages]` entries as nib-syntax takes them.
pub fn runtime_languages(cfg: &Config) -> Vec<nib_syntax::RuntimeLanguage> {
    cfg.languages
        .iter()
        .map(|(name, l)| nib_syntax::RuntimeLanguage {
            name: name.clone(),
            grammar: l.grammar.clone().unwrap_or_else(|| name.clone()),
            extensions: l.extensions.clone(),
            filenames: l.filenames.clone(),
            shebangs: l.shebangs.clone(),
            aliases: l.aliases.clone(),
        })
        .collect()
}

fn config_dir() -> Result<PathBuf> {
    Config::path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .ok_or_else(|| anyhow!("no config location (HOME unset)"))
}

fn cache_dir() -> Result<PathBuf> {
    Ok(nib_proto::path::nib_cache_dir())
}

/// Run a command; on failure, its output is the error.
fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output().with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !out.status.success() {
        bail!(
            "{:?} failed:\n{}{}",
            cmd,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Helix's catalog
// ---------------------------------------------------------------------------------------------

/// `languages.toml` and `runtime/queries/` of Helix at `HELIX_TAG`, from the tag's source
/// tarball (~2.5 MB; curl and tar, which every platform nib releases for has). Not a sparse git
/// clone: git before 2.37 (Ubuntu 22.04) checks one out without the root files.
fn helix_dir() -> Result<PathBuf> {
    let tag = nib_syntax::HELIX_TAG;
    let dir = cache_dir()?.join(format!("helix-{tag}"));
    if dir.join("languages.toml").is_file() && dir.join("runtime/queries").is_dir() {
        return Ok(dir);
    }
    eprintln!("fetching the language catalog (Helix {tag})…");
    let tmp = cache_dir()?.join(format!("helix-{tag}.part"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let archive = tmp.join("helix.tar.gz");
    curl(&format!("https://github.com/helix-editor/helix/archive/refs/tags/{tag}.tar.gz"), &archive)?;
    let top = format!("helix-{tag}");
    run(Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&tmp)
        .arg(format!("{top}/languages.toml"))
        .arg(format!("{top}/runtime/queries")))?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(tmp.join(&top), &dir)?;
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(dir)
}

struct Catalog {
    languages: Vec<toml::Table>,
    grammars: Vec<toml::Table>,
}

impl Catalog {
    fn load(helix: &Path) -> Result<Self> {
        let path = helix.join("languages.toml");
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut doc: toml::Table = toml::from_str(&text).context("Helix languages.toml")?;
        let tables = |v: Option<toml::Value>| -> Vec<toml::Table> {
            match v {
                Some(toml::Value::Array(a)) => a.into_iter().filter_map(|v| v.as_table().cloned()).collect(),
                _ => vec![],
            }
        };
        Ok(Self { languages: tables(doc.remove("language")), grammars: tables(doc.remove("grammar")) })
    }

    /// By name, else by a file extension it claims (`js` -> javascript).
    fn language(&self, want: &str) -> Option<&toml::Table> {
        let want = want.to_ascii_lowercase();
        let name = |l: &toml::Table| l.get("name").and_then(|v| v.as_str()).map(str::to_string);
        self.languages.iter().find(|l| name(l).as_deref() == Some(&want)).or_else(|| {
            self.languages.iter().find(|l| file_types(l).0.contains(&want))
        })
    }

    fn grammar(&self, name: &str) -> Option<&toml::Table> {
        self.grammars.iter().find(|g| g.get("name").and_then(|v| v.as_str()) == Some(name))
    }
}

fn strs(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// `file-types`: (extensions, whole file names). Globs with wildcards are skipped.
fn file_types(l: &toml::Table) -> (Vec<String>, Vec<String>) {
    let (mut exts, mut names) = (vec![], vec![]);
    for v in l.get("file-types").and_then(|v| v.as_array()).into_iter().flatten() {
        match v {
            toml::Value::String(s) => exts.push(s.clone()),
            toml::Value::Table(t) => {
                if let Some(g) = t.get("glob").and_then(|g| g.as_str()) {
                    if !g.contains(['*', '?', '[', '{']) {
                        names.push(g.rsplit('/').next().unwrap_or(g).to_string());
                    }
                }
            }
            _ => {}
        }
    }
    (exts, names)
}

// ---------------------------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------------------------

enum Source {
    /// The bundle of nib's release, downloaded; compile what it lacks.
    Release,
    /// An unpacked bundle; compile what it lacks.
    Dir(PathBuf),
    /// Compile everything.
    Compile,
}

fn add_all(langs: &[String], source: &Source) -> Result<()> {
    let dir = config_dir()?;
    let bundle = match source {
        Source::Compile => None,
        Source::Dir(d) => Some(Bundle::open(d)?),
        Source::Release => match Bundle::download() {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("no prebuilt grammars ({e:#}); compiling instead");
                None
            }
        },
    };
    // Helix's catalog only when something has to be compiled (it is a download).
    let mut helix: Option<(PathBuf, Catalog)> = None;
    let mut failed = 0;
    for want in langs {
        let r = match bundle.as_ref().and_then(|b| b.find(want).map(|n| (b, n))) {
            Some((b, name)) => b.install(&dir, &name),
            None => (|| {
                if helix.is_none() {
                    let h = helix_dir()?;
                    let c = Catalog::load(&h)?;
                    helix = Some((h, c));
                }
                let (h, c) = helix.as_ref().unwrap();
                add(c, h, &dir, want)
            })(),
        };
        match r {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("{want}: {e:#}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!("{failed} of {} not added", langs.len());
    }
    println!("Open a new nib window to use them.");
    Ok(())
}

/// What compiling `want` from source produced in `dir`: its name, entry and query dirs.
struct Built {
    name: String,
    entry: LanguageEntry,
    queries: Vec<String>,
}

/// Fetch and compile `want`'s grammar into `dir/grammars/`, copy its queries into `dir/queries/`.
fn compile_language(catalog: &Catalog, helix: &Path, dir: &Path, want: &str) -> Result<Built> {
    let lang = catalog
        .language(want)
        .ok_or_else(|| anyhow!("not a language nib knows; see `nib grammar list --available`"))?;
    let name = lang.get("name").and_then(|v| v.as_str()).unwrap().to_string();
    let grammar = lang.get("grammar").and_then(|v| v.as_str()).unwrap_or(&name).to_string();
    let source = catalog
        .grammar(&grammar)
        .and_then(|g| g.get("source"))
        .and_then(|s| s.as_table())
        .ok_or_else(|| anyhow!("{name} has no grammar to compile (Helix highlights it without one)"))?;
    let (Some(git), Some(rev)) = (source.get("git").and_then(|v| v.as_str()), source.get("rev").and_then(|v| v.as_str())) else {
        bail!("{name}'s grammar has no git source");
    };
    let subpath = source.get("subpath").and_then(|v| v.as_str());

    let src = fetch(&grammar, git, rev)?;
    let src = subpath.map_or(src.clone(), |p| src.join(p)).join("src");
    compile(&grammar, &src, &nib_syntax::grammar_path(dir, &grammar))?;
    let queries = copy_queries(helix, dir, &name)?;
    let (extensions, filenames) = file_types(lang);
    let entry = LanguageEntry {
        grammar: Some(grammar),
        extensions,
        filenames,
        shebangs: strs(lang.get("shebangs")),
        aliases: vec![],
    };
    Ok(Built { name, entry, queries })
}

fn runtime_language(name: &str, l: &LanguageEntry) -> nib_syntax::RuntimeLanguage {
    nib_syntax::RuntimeLanguage {
        name: name.to_string(),
        grammar: l.grammar.clone().unwrap_or_else(|| name.to_string()),
        extensions: l.extensions.clone(),
        filenames: l.filenames.clone(),
        shebangs: l.shebangs.clone(),
        aliases: l.aliases.clone(),
    }
}

/// Check that the grammar loads with its queries, then write `[languages.<name>]`.
fn turn_on(dir: &Path, name: &str, entry: &LanguageEntry, queries: &[String], how: &str) -> Result<String> {
    nib_syntax::check_runtime(dir, &runtime_language(name, entry)).context("the grammar doesn't load with its queries")?;
    let cfg = Config::save_language(name, entry)?;
    let lib = nib_syntax::grammar_path(dir, entry.grammar.as_deref().unwrap_or(name));
    Ok(format!(
        "added {name} ({how}): {} + queries/{} → [languages.{name}] in {}",
        lib.strip_prefix(dir).unwrap_or(&lib).display(),
        queries.join(", queries/"),
        cfg.display()
    ))
}

fn add(catalog: &Catalog, helix: &Path, dir: &Path, want: &str) -> Result<String> {
    let b = compile_language(catalog, helix, dir, want)?;
    turn_on(dir, &b.name, &b.entry, &b.queries, "compiled")
}

// ---------------------------------------------------------------------------------------------
// prebuilt bundles
// ---------------------------------------------------------------------------------------------

/// `manifest.toml` of a bundle: `[languages.<name>]` as in config.toml, plus the query dirs.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Manifest {
    languages: BTreeMap<String, ManifestEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ManifestEntry {
    entry: LanguageEntry,
    /// `queries/<dir>` to copy: the language's own and those it inherits.
    queries: Vec<String>,
}

/// `nib grammar build DIR LANG...`: compile into DIR and write its manifest (CI, for releases).
fn build(out: &Path, langs: &[String]) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let helix = helix_dir()?;
    let catalog = Catalog::load(&helix)?;
    let mut manifest = Manifest::default();
    for want in langs {
        let b = compile_language(&catalog, &helix, out, want).with_context(|| want.clone())?;
        nib_syntax::check_runtime(out, &runtime_language(&b.name, &b.entry))
            .with_context(|| format!("{}: the grammar doesn't load with its queries", b.name))?;
        println!("built {}", b.name);
        manifest.languages.insert(b.name, ManifestEntry { entry: b.entry, queries: b.queries });
    }
    std::fs::write(out.join("manifest.toml"), toml::to_string(&manifest)?)?;
    Ok(())
}

/// The release triple this nib was built for, as in the archive names.
fn triple() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    })
}

/// nib's release tag: workspace 0.4.0 <-> v0.4.
fn release_tag() -> String {
    let v = env!("CARGO_PKG_VERSION");
    format!("v{}", v.strip_suffix(".0").unwrap_or(v))
}

struct Bundle {
    dir: PathBuf,
    manifest: Manifest,
}

/// The repository whose release workflow builds what nib downloads.
const REPO: &str = "talg2324/nibble";

/// Check `archive` against GitHub's record (Sigstore) that the release workflow of [`REPO`]
/// built it: `gh attestation verify`. That takes the GitHub CLI, signed in; without one it is
/// not checked, and that is said. `Err`: it was asked, and GitHub has no such record.
fn provenance(archive: &Path) -> Result<()> {
    let gh = |args: &[&str]| crate::transport::command("gh").args(args).stdin(std::process::Stdio::null()).output();
    if !gh(&["auth", "status"]).is_ok_and(|o| o.status.success()) {
        eprintln!("provenance not checked: that takes the GitHub CLI, signed in (gh auth login)");
        return Ok(());
    }
    let file = archive.to_string_lossy();
    let out = gh(&["attestation", "verify", &file, "--repo", REPO, "--signer-workflow", &format!("{REPO}/.github/workflows/release.yml")])?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let last: Vec<&str> = said.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
        bail!("{} was not built by {REPO}'s release workflow, or GitHub has no record that it was: {}", archive.file_name().unwrap_or_default().to_string_lossy(), last.join(" "));
    }
    Ok(())
}

impl Bundle {
    fn open(dir: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(dir.join("manifest.toml"))
            .with_context(|| format!("{} is not a grammar bundle (no manifest.toml)", dir.display()))?;
        Ok(Self { dir: dir.to_path_buf(), manifest: toml::from_str(&text).context("manifest.toml")? })
    }

    /// The release's bundle for this machine, downloaded and unpacked once into the cache.
    ///
    /// What is in it is run (the grammars are native libraries), so it is checked twice: against
    /// the release's `SHA256SUMS` (a broken download), and against its build provenance
    /// ([`provenance`]: that nibble's release workflow built this very file).
    fn download() -> Result<Self> {
        let triple = triple().ok_or_else(|| anyhow!("releases have none for this platform"))?;
        let tag = release_tag();
        let name = format!("nib-grammars-{tag}-{triple}");
        let cache = cache_dir()?.join(&name);
        if let Ok(b) = Self::open(&cache.join(&name)) {
            return Ok(b);
        }
        let base = std::env::var("NIB_GRAMMARS_URL").unwrap_or_else(|_| format!("{RELEASES}/{tag}"));
        let tmp = cache_dir()?.join(format!("{name}.part"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let archive = tmp.join(format!("{name}.tar.gz"));
        eprintln!("downloading {name}…");
        curl(&format!("{base}/{name}.tar.gz"), &archive)?;
        curl(&format!("{base}/SHA256SUMS"), &tmp.join("SHA256SUMS"))?;
        let sums = std::fs::read_to_string(tmp.join("SHA256SUMS"))?;
        let want = sums
            .lines()
            .find_map(|l| l.split_once(char::is_whitespace).filter(|(_, f)| f.trim().trim_start_matches('*') == format!("{name}.tar.gz")))
            .map(|(h, _)| h.to_ascii_lowercase())
            .ok_or_else(|| anyhow!("{name}.tar.gz is not in the release's SHA256SUMS"))?;
        let got = hex(&Sha256::digest(std::fs::read(&archive)?));
        if got != want {
            bail!("{name}.tar.gz doesn't match SHA256SUMS ({got} ≠ {want})");
        }
        // The sums come from where the bundle comes from. A release's own bundle is also
        // checked against GitHub's record of who built it, when that can be asked.
        if std::env::var_os("NIB_GRAMMARS_URL").is_none() {
            provenance(&archive)?;
        }
        run(Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(&tmp))?;
        let _ = std::fs::remove_file(&archive);
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::rename(&tmp, &cache)?;
        Self::open(&cache.join(&name))
    }

    /// The bundle's name for `want`: by name, else by an extension it claims (`js`).
    fn find(&self, want: &str) -> Option<String> {
        let want = want.to_ascii_lowercase();
        if self.manifest.languages.contains_key(&want) {
            return Some(want);
        }
        self.manifest.languages.iter().find(|(_, l)| l.entry.extensions.contains(&want)).map(|(n, _)| n.clone())
    }

    fn install(&self, dir: &Path, name: &str) -> Result<String> {
        let l = &self.manifest.languages[name];
        let grammar = l.entry.grammar.as_deref().unwrap_or(name);
        let (from, to) = (nib_syntax::grammar_path(&self.dir, grammar), nib_syntax::grammar_path(dir, grammar));
        std::fs::create_dir_all(to.parent().unwrap())?;
        // Replaced by a rename, so nib windows that have the old one loaded keep working.
        let tmp = to.with_extension("tmp");
        std::fs::copy(&from, &tmp).with_context(|| format!("copying {}", from.display()))?;
        std::fs::rename(&tmp, &to)?;
        for q in &l.queries {
            let (from, to) = (self.dir.join("queries").join(q), dir.join("queries").join(q));
            std::fs::create_dir_all(&to)?;
            for e in std::fs::read_dir(&from)? {
                let p = e?.path();
                std::fs::copy(&p, to.join(p.file_name().unwrap()))?;
            }
        }
        turn_on(dir, name, &l.entry, &l.queries, "prebuilt")
    }
}

fn curl(url: &str, out: &Path) -> Result<()> {
    run(Command::new("curl").args(["-fsSL", "--retry", "2", "-o"]).arg(out).arg(url))
        .with_context(|| format!("downloading {url}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The grammar's repository at `rev`, cached.
fn fetch(grammar: &str, git: &str, rev: &str) -> Result<PathBuf> {
    let dir = cache_dir()?.join("grammar-src").join(format!("{grammar}-{}", &rev[..rev.len().min(12)]));
    if dir.join(".git").is_dir() && std::fs::read_dir(&dir)?.count() > 1 {
        return Ok(dir);
    }
    eprintln!("fetching {grammar} grammar ({git})…");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let git_in = || {
        let mut c = Command::new("git");
        c.arg("-C").arg(&dir).args(["-c", "advice.detachedHead=false"]);
        c
    };
    run(git_in().args(["init", "-q"]))?;
    run(git_in().args(["fetch", "-q", "--depth", "1", git, rev]))?;
    run(git_in().args(["checkout", "-q", "FETCH_HEAD"]))?;
    Ok(dir)
}

/// `parser.c` (+ `scanner.c` or `scanner.cc`) into a shared library at `out`, replaced by a
/// rename so nib windows that have the old one loaded keep working.
fn compile(grammar: &str, src: &Path, out: &Path) -> Result<()> {
    if !src.join("parser.c").is_file() {
        bail!("no parser.c in {}", src.display());
    }
    eprintln!("compiling {grammar}…");
    let build = cache_dir()?.join("build").join(grammar);
    let _ = std::fs::remove_dir_all(&build);
    std::fs::create_dir_all(&build)?;
    let (cc_default, cxx_default) = if cfg!(windows) { ("clang", "clang++") } else { ("cc", "c++") };
    let cc = std::env::var("CC").unwrap_or_else(|_| cc_default.into());
    let cxx = std::env::var("CXX").unwrap_or_else(|_| cxx_default.into());
    let mut objs = vec![];
    let mut cpp = false;
    for (file, is_cpp) in [("parser.c", false), ("scanner.c", false), ("scanner.cc", true)] {
        let path = src.join(file);
        if !path.is_file() {
            continue;
        }
        let obj = build.join(format!("{file}.o"));
        let mut c = Command::new(if is_cpp { &cxx } else { &cc });
        c.args(["-c", "-O2", "-w", if is_cpp { "-std=c++14" } else { "-std=c11" }]);
        if !cfg!(windows) {
            c.arg("-fPIC");
        }
        c
            .arg("-I")
            .arg(src)
            .arg(&path)
            .arg("-o")
            .arg(&obj);
        run(&mut c).with_context(|| format!("compiling {file} (is a C compiler installed?)"))?;
        objs.push(obj);
        cpp |= is_cpp;
    }
    std::fs::create_dir_all(out.parent().unwrap())?;
    let tmp = out.with_extension("tmp");
    let mut link = Command::new(if cpp { &cxx } else { &cc });
    link.arg("-shared").args(&objs).arg("-o").arg(&tmp);
    if cfg!(windows) {
        // Older grammars don't mark their entry point dllexport.
        link.arg(format!("-Wl,/EXPORT:tree_sitter_{grammar}"));
    }
    run(&mut link)?;
    std::fs::rename(&tmp, out)?;
    let _ = std::fs::remove_dir_all(&build);
    Ok(())
}

/// Helix's queries for `lang`, and for every language they inherit from. Returns the dirs.
fn copy_queries(helix: &Path, dir: &Path, lang: &str) -> Result<Vec<String>> {
    let mut todo = vec![lang.to_string()];
    let mut done: Vec<String> = vec![];
    while let Some(l) = todo.pop() {
        if done.contains(&l) {
            continue;
        }
        let from = helix.join("runtime/queries").join(&l);
        if !from.is_dir() {
            bail!("Helix has no queries for {l}");
        }
        let to = dir.join("queries").join(&l);
        std::fs::create_dir_all(&to)?;
        for e in std::fs::read_dir(&from)? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "scm") {
                let text = std::fs::read_to_string(&p)?;
                for line in text.lines() {
                    if let Some(parents) = line.trim().strip_prefix("; inherits:") {
                        todo.extend(parents.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
                    }
                }
                std::fs::write(to.join(p.file_name().unwrap()), text)?;
            }
        }
        done.push(l);
    }
    Ok(done)
}

// ---------------------------------------------------------------------------------------------
// remove, list
// ---------------------------------------------------------------------------------------------

fn remove_all(langs: &[String]) -> Result<()> {
    let dir = config_dir()?;
    let cfg = Config::load()?;
    for name in langs {
        let Some(entry) = cfg.languages.get(name) else {
            bail!("{name} wasn't added (see `nib grammar list`)");
        };
        Config::remove_language(name)?;
        let grammar = entry.grammar.clone().unwrap_or_else(|| name.clone());
        let shared = cfg.languages.iter().any(|(n, l)| n != name && l.grammar.as_deref().unwrap_or(n) == grammar);
        if !shared {
            let _ = std::fs::remove_file(nib_syntax::grammar_path(&dir, &grammar));
        }
        let _ = std::fs::remove_dir_all(dir.join("queries").join(name));
        println!("removed {name}");
    }
    Ok(())
}

fn list(available: bool) -> Result<()> {
    let cfg = Config::load()?;
    let dir = config_dir()?;
    println!("built in: {}", nib_syntax::BUILTIN_LANGUAGES.join(", "));
    if cfg.languages.is_empty() {
        println!("added: none (nib grammar add LANGUAGE)");
    } else {
        println!("added:");
        for rt in runtime_languages(&cfg) {
            let (status, why) = match nib_syntax::check_runtime(&dir, &rt) {
                Ok(()) => ("ok", String::new()),
                Err(e) => ("broken", format!("\n      {e:#}")),
            };
            println!("  {:<12} {:<6} {}{why}", rt.name, status, rt.extensions.join(" "));
        }
    }
    if available {
        let catalog = Catalog::load(&helix_dir()?)?;
        let names: Vec<&str> = catalog
            .languages
            .iter()
            .filter(|l| {
                let g = l.get("grammar").or_else(|| l.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                catalog.grammar(g).and_then(|g| g.get("source")).is_some()
            })
            .filter_map(|l| l.get("name").and_then(|v| v.as_str()))
            .collect();
        println!("available ({}): {}", names.len(), names.join(" "));
    }
    Ok(())
}
