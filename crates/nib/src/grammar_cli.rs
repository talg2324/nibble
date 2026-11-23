//! `nib grammar`: add languages to nib without rebuilding it.
//!
//! `nib grammar add rust` looks the language up in Helix's `languages.toml` at the tag nib's
//! queries come from (`nib_syntax::HELIX_TAG`), fetches its grammar at the pinned commit,
//! compiles it with the system C compiler into `<config dir>/grammars/`, copies Helix's queries
//! for it into `<config dir>/queries/`, checks that the two load together, and only then writes
//! `[languages.rust]` into config.toml. Sources are cached in `~/.cache/nib/`.

use anyhow::{anyhow, bail, Context, Result};
use nib_editor::config::LanguageEntry;
use nib_editor::Config;
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "\
usage: nib grammar add LANGUAGE...     fetch, compile and turn on languages (rust, go, javascript, …)
       nib grammar rm LANGUAGE...      turn them off and delete their files (also: remove)
       nib grammar list [--available]  languages nib highlights; --available: all it can add

Grammars are compiled with `cc` (or $CC; C++ scanners with `c++` / $CXX) and need git.
Changes apply to nib windows opened afterwards.";

pub fn grammar_main(args: &[String]) -> i32 {
    let r = match args.first().map(String::as_str) {
        Some("add") if args.len() > 1 => add_all(&args[1..]),
        Some("rm" | "remove") if args.len() > 1 => remove_all(&args[1..]),
        Some("list") => list(args.get(1).is_some_and(|a| a == "--available")),
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

/// `languages.toml` and `runtime/queries/` of Helix at `HELIX_TAG` (a sparse clone, ~5 MB).
fn helix() -> Result<PathBuf> {
    let dir = cache_dir()?.join(format!("helix-{}", nib_syntax::HELIX_TAG));
    if dir.join("languages.toml").is_file() && dir.join("runtime/queries").is_dir() {
        return Ok(dir);
    }
    eprintln!("fetching the language catalog (Helix {})…", nib_syntax::HELIX_TAG);
    let tmp = dir.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.parent().unwrap())?;
    run(Command::new("git")
        .args(["-c", "advice.detachedHead=false", "clone", "-q", "--depth", "1", "--filter=blob:none", "--sparse"])
        .args(["--branch", nib_syntax::HELIX_TAG, "https://github.com/helix-editor/helix"])
        .arg(&tmp))?;
    run(Command::new("git").arg("-C").arg(&tmp).args(["sparse-checkout", "set", "runtime/queries"]))?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir)?;
    Ok(dir)
}

struct Catalog {
    languages: Vec<toml::Table>,
    grammars: Vec<toml::Table>,
}

impl Catalog {
    fn load(helix: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(helix.join("languages.toml"))?;
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

fn add_all(langs: &[String]) -> Result<()> {
    if cfg!(windows) {
        bail!("compiling grammars isn't supported on Windows yet");
    }
    let helix = helix()?;
    let catalog = Catalog::load(&helix)?;
    let dir = config_dir()?;
    let mut failed = 0;
    for want in langs {
        match add(&catalog, &helix, &dir, want) {
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

fn add(catalog: &Catalog, helix: &Path, dir: &Path, want: &str) -> Result<String> {
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
    let lib = nib_syntax::grammar_path(dir, &grammar);
    compile(&grammar, &src, &lib)?;
    let copied = copy_queries(helix, dir, &name)?;

    let (extensions, filenames) = file_types(lang);
    let entry = LanguageEntry {
        grammar: Some(grammar.clone()),
        extensions,
        filenames,
        shebangs: strs(lang.get("shebangs")),
        aliases: vec![],
    };
    let rt = nib_syntax::RuntimeLanguage {
        name: name.clone(),
        grammar: grammar.clone(),
        extensions: entry.extensions.clone(),
        filenames: entry.filenames.clone(),
        shebangs: entry.shebangs.clone(),
        aliases: vec![],
    };
    nib_syntax::check_runtime(dir, &rt).context("the compiled grammar doesn't load with its queries")?;
    let cfg = Config::save_language(&name, &entry)?;
    Ok(format!(
        "added {name}: {} + queries/{} → [languages.{name}] in {}",
        lib.strip_prefix(dir).unwrap_or(&lib).display(),
        copied.join(", queries/"),
        cfg.display()
    ))
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
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let cxx = std::env::var("CXX").unwrap_or_else(|_| "c++".into());
    let mut objs = vec![];
    let mut cpp = false;
    for (file, is_cpp) in [("parser.c", false), ("scanner.c", false), ("scanner.cc", true)] {
        let path = src.join(file);
        if !path.is_file() {
            continue;
        }
        let obj = build.join(format!("{file}.o"));
        let mut c = Command::new(if is_cpp { &cxx } else { &cc });
        c.args(["-c", "-fPIC", "-O2", "-w", if is_cpp { "-std=c++14" } else { "-std=c11" }])
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
    run(Command::new(if cpp { &cxx } else { &cc }).arg("-shared").args(&objs).arg("-o").arg(&tmp))?;
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
        let catalog = Catalog::load(&helix()?)?;
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
