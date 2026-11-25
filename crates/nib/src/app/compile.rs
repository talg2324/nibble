//! Compile on save (Typst, LaTeX): saving a file that a `[compile.<lang>]` entry covers asks nibd
//! to run the compiler on the machine with the files; its errors are drawn on their lines, and
//! the PDF it writes is the tab's preview, next to the source, read again on each build.

use super::{App, Message, Pending, Tab};
use crate::editor_view::Mark;
use crate::pdf::PdfView;
use iced::Color;
use nib_editor::config::CompileConfig;
use nib_proto::{CompileError, Request, Response, RpcError};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A compiler, with the user's `[compile.<lang>]` applied over the built-in one.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiler {
    pub lang: String,
    /// Empty: turned off.
    pub command: Vec<String>,
    pub extensions: Vec<String>,
    pub pdf: String,
}

const DEFAULT_PDF: &str = "{dir}/{stem}.pdf";

fn v(s: &[&str]) -> Vec<String> {
    s.iter().map(|s| s.to_string()).collect()
}

/// nib's own entries: what Settings → Languages shows as the default.
pub fn builtin() -> Vec<Compiler> {
    vec![
        Compiler {
            lang: "typst".into(),
            command: v(&["typst", "compile", "--root", "{root}", "--diagnostic-format", "short", "{main}", "{pdf}"]),
            extensions: v(&["typ"]),
            pdf: DEFAULT_PDF.into(),
        },
        Compiler {
            lang: "latex".into(),
            command: v(&["latexmk", "-pdf", "-interaction=nonstopmode", "-halt-on-error", "-file-line-error", "-outdir={dir}", "{main}"]),
            extensions: v(&["tex"]),
            pdf: DEFAULT_PDF.into(),
        },
    ]
}

/// The built-in compilers with `[compile.*]` applied: a built-in name changes the fields it sets,
/// another name adds a compiler when it has a command and extensions. An extension a later entry
/// claims is taken from the others.
pub fn compilers(user: &BTreeMap<String, CompileConfig>) -> Vec<Compiler> {
    let mut all = builtin();
    for (lang, c) in user {
        let i = match all.iter().position(|b| &b.lang == lang) {
            Some(i) => i,
            None if c.command.as_ref().is_some_and(|c| !c.is_empty()) && !c.extensions.is_empty() => {
                all.push(Compiler { lang: lang.clone(), command: Vec::new(), extensions: Vec::new(), pdf: DEFAULT_PDF.into() });
                all.len() - 1
            }
            None => continue,
        };
        if let Some(cmd) = &c.command {
            all[i].command = cmd.clone();
        }
        if !c.extensions.is_empty() {
            let exts = c.extensions.clone();
            for (j, other) in all.iter_mut().enumerate() {
                if j != i {
                    other.extensions.retain(|e| !exts.contains(e));
                }
            }
            all[i].extensions = exts;
        }
        if let Some(pdf) = &c.pdf {
            all[i].pdf = pdf.clone();
        }
    }
    all
}

/// The compiler for `path`, judged by its extension (case aside); none when it's turned off.
pub fn for_path<'a>(list: &'a [Compiler], path: &str) -> Option<&'a Compiler> {
    let name = nib_proto::path::file_name(path);
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    list.iter().find(|c| !c.command.is_empty() && c.extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext)))
}

/// The file to compile when `path` is saved: the root a `% !TEX root = main.tex` (LaTeX) or
/// `// !typst root = main.typ` line in its first 20 lines names, relative to `path`'s folder;
/// else `path` itself.
pub fn main_of<'a>(path: &str, head: impl Iterator<Item = &'a str>) -> String {
    for line in head.take(20) {
        let l = line.trim_start().trim_start_matches(['%', '/', ' ', '\t']);
        let lower = l.to_ascii_lowercase();
        let Some(rest) = ["!tex root", "!typst root"].iter().find_map(|k| lower.starts_with(k).then(|| &l[k.len()..])) else { continue };
        let Some(root) = rest.trim_start().strip_prefix('=') else { continue };
        let root = root.trim();
        if !root.is_empty() {
            return normalize(&nib_proto::path::join(dir_of(path), root));
        }
    }
    path.to_string()
}

/// [`main_of`] with the tab's text as it is now.
fn main_for(t: &Tab, path: &str) -> String {
    let head: Vec<String> = t.doc.text.lines().take(20).map(|l| l.to_string()).collect();
    main_of(path, head.iter().map(|l| l.trim_end_matches(['\n', '\r'])))
}

/// The folder part of a daemon path (either separator).
fn dir_of(path: &str) -> &str {
    path.rfind(['/', '\\']).map(|i| if i == 0 { "/" } else { &path[..i] }).unwrap_or(".")
}

/// `a/./b` → `a/b`, `a/x/../b` → `a/b`.
fn normalize(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for (i, part) in p.split('/').enumerate() {
        match part {
            "." => {}
            "" if i > 0 => {}
            ".." if out.last().is_some_and(|l| !l.is_empty() && *l != "..") => {
                out.pop();
            }
            _ => out.push(part),
        }
    }
    out.join("/")
}

/// The project folder for `main`: `project` (the folder nib has open) when `main` is inside it,
/// as an editor's workspace folder is; else `main`'s own folder.
fn root_for<'a>(project: &'a str, main: &'a str) -> &'a str {
    let p = project.trim_end_matches(['/', '\\']);
    let inside = !p.is_empty() && main.strip_prefix(p).is_some_and(|rest| rest.starts_with(['/', '\\']));
    if inside { p } else { dir_of(main) }
}

/// `Request::Compile` for `main` with `c`, in the project `project`: placeholders filled in.
pub fn request(c: &Compiler, main: &str, project: &str) -> Request {
    let dir = dir_of(main).to_string();
    let root = root_for(project, main);
    let name = nib_proto::path::file_name(main);
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s).to_string();
    let fill = |s: &str, pdf: &str| {
        s.replace("{main}", main).replace("{dir}", &dir).replace("{root}", root).replace("{stem}", &stem).replace("{pdf}", pdf)
    };
    let pdf = fill(&c.pdf, "");
    let argv = c.command.iter().map(|a| fill(a, &pdf)).collect();
    Request::Compile { main: main.to_string(), argv, cwd: dir, pdf }
}

/// The output a source tab shows as its preview.
pub struct Output {
    /// The PDF, on the daemon's machine.
    pub pdf: String,
    pub view: Option<Box<PdfView>>,
    /// The stamp of the PDF `view` holds, so a change seen twice (the compile's answer and
    /// the watch) is read once.
    pub mtime_ns: Option<u128>,
    /// A compile is going on.
    pub running: bool,
    /// The preview was opened for the user once (the first good build); after that it is theirs
    /// to show or hide.
    pub opened: bool,
}

impl Output {
    pub fn new(pdf: String) -> Self {
        Output { pdf, view: None, mtime_ns: None, running: false, opened: false }
    }
}

impl App {
    /// A tab was just opened: if a compiler covers it, it gets an output (its preview), and the
    /// PDF already built, if there is one, is read.
    pub(super) fn compile_attach(&mut self, tab: u64) -> Task {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) else { return Task::none() };
        if t.local || t.media.is_some() {
            return Task::none();
        }
        let Some(path) = t.doc.path.as_ref().map(|p| p.to_string_lossy().into_owned()) else { return Task::none() };
        let Some(c) = for_path(&self.compilers, &path) else { return Task::none() };
        let main = main_for(t, &path);
        let Request::Compile { pdf, .. } = request(c, &main, &self.root) else { unreachable!() };
        t.out = Some(Output::new(pdf.clone()));
        self.request(Request::ReadFile { path: pdf }, Pending::CompiledPdf { tab, show: false })
    }

    /// Tab `tab` was saved: compile it (or the root it names).
    pub(super) fn compile_after_save(&mut self, tab: u64) -> Task {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) else { return Task::none() };
        let Some(path) = t.doc.path.as_ref().map(|p| p.to_string_lossy().into_owned()) else { return Task::none() };
        let Some(c) = for_path(&self.compilers, &path) else { return Task::none() };
        let main = main_for(t, &path);
        let req = request(c, &main, &self.root);
        let Request::Compile { pdf, .. } = &req else { unreachable!() };
        let out = t.out.get_or_insert_with(|| Output::new(pdf.clone()));
        if out.pdf != *pdf {
            // A root line was added or changed: another PDF.
            *out = Output { opened: out.opened, ..Output::new(pdf.clone()) };
        }
        out.running = true;
        let lang = c.lang.clone();
        self.request(req, Pending::Compile { tab, lang })
    }

    pub(super) fn on_compiled(&mut self, tab: u64, lang: String, result: Result<Response, RpcError>) -> Task {
        let Ok(Response::Compiled { main, ok, cancelled, missing, pdf, errors, log }) = result else {
            if let Some(o) = self.tabs.iter_mut().find(|t| t.id == tab).and_then(|t| t.out.as_mut()) {
                o.running = false;
            }
            return match result {
                Err(e) => self.flash_warn(format!("{lang}: {e}")),
                _ => Task::none(),
            };
        };
        if cancelled {
            // A newer save's compile is going on; its answer will say.
            return Task::none();
        }
        if let Some(o) = self.tabs.iter_mut().find(|t| t.id == tab).and_then(|t| t.out.as_mut()) {
            o.running = false;
        }
        if missing {
            return self.flash_warn(log);
        }
        let (n_err, n_warn) = (errors.iter().filter(|e| !e.warning).count(), errors.iter().filter(|e| e.warning).count());
        let first = errors.iter().find(|e| !e.warning).map(|e| {
            let file = nib_proto::path::file_name(&e.path);
            format!("{file}:{}: {}", e.line + 1, e.message)
        });
        if errors.is_empty() {
            self.compile_errors.remove(&main);
        } else {
            self.compile_errors.insert(main.clone(), errors);
        }
        let plural = |n: usize, w: &str| if n == 1 { format!("1 {w}") } else { format!("{n} {w}s") };
        if !ok {
            let why = first.unwrap_or_else(|| log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim().to_string());
            let count = if n_err > 1 { format!(" ({})", plural(n_err, "error")) } else { String::new() };
            return self.flash_warn(format!("{lang}: {why}{count}"));
        }
        let mut tasks = vec![];
        let name = pdf.as_deref().map(nib_proto::path::file_name).unwrap_or("the PDF").to_string();
        let warned = if n_warn > 0 { format!(" ({})", plural(n_warn, "warning")) } else { String::new() };
        tasks.push(self.flash_info(format!("compiled → {name}{warned}")));
        if let Some(pdf) = pdf {
            tasks.push(self.request(Request::ReadFile { path: pdf }, Pending::CompiledPdf { tab, show: true }));
        }
        Task::batch(tasks)
    }

    /// The output PDF of tab `tab` arrived (after a build, a change seen on disk, or the tab
    /// opening): parse it, keeping where it was being read. `show`: a build of the user's made it,
    /// so the first time the preview opens by itself.
    pub(super) fn on_compiled_pdf(&mut self, tab: u64, show: bool, result: Result<Response, RpcError>) -> Task {
        let Ok(Response::File { path, data, mtime_ns }) = result else { return Task::none() };
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) else { return Task::none() };
        let Some(o) = t.out.as_mut() else { return Task::none() };
        if show && !o.opened {
            o.opened = true;
            t.rendered = true;
        }
        if !nib_proto::path::same(&o.pdf, &path) || (o.view.is_some() && o.mtime_ns == Some(mtime_ns)) {
            return Task::none();
        }
        o.mtime_ns = Some(mtime_ns);
        Task::perform(async move { crate::pdf::load(data).map(Arc::new) }, move |r| Message::Pdf(crate::pdf::Msg::Loaded(tab, r)))
    }

    /// The output PDFs of the open tabs, for nibd to watch (built by `typst watch` in a terminal
    /// too).
    pub(super) fn compile_watch_paths(&self) -> impl Iterator<Item = String> + '_ {
        self.tabs.iter().filter_map(|t| t.out.as_ref()).map(|o| o.pdf.clone())
    }

    /// `path` changed on disk: the tabs whose preview it is read it again.
    pub(super) fn compile_file_changed(&mut self, path: &str) -> Task {
        let tabs: Vec<u64> = self.tabs.iter().filter(|t| t.out.as_ref().is_some_and(|o| nib_proto::path::same(&o.pdf, path))).map(|t| t.id).collect();
        Task::batch(tabs.into_iter().map(|tab| self.request(Request::ReadFile { path: path.to_string() }, Pending::CompiledPdf { tab, show: false })).collect::<Vec<_>>())
    }

    /// The compiler's errors and warnings in `tab`'s file: row tints and the messages at the
    /// lines' ends.
    pub(super) fn compile_marks(&self, tab: &Tab) -> (Vec<Mark>, Vec<(usize, String, Color)>) {
        let Some(path) = tab.doc.path.as_ref().filter(|_| !tab.local).map(|p| p.to_string_lossy()) else { return Default::default() };
        let text = &tab.doc.text;
        let (mut marks, mut notes) = (Vec::new(), Vec::new());
        for e in self.compile_errors.values().flatten().filter(|e| nib_proto::path::same(&e.path, &path)) {
            let line = e.line as usize;
            if line >= text.len_lines() {
                continue;
            }
            let color = if e.warning { self.theme.warn } else { self.theme.diff_del };
            let from = text.line_to_char(line);
            let to = text.line_to_char((line + 1).min(text.len_lines())).max(from);
            marks.push(Mark { from, to, color: Color { a: 0.035, ..color }, rows: true });
            if !notes.iter().any(|(l, _, _): &(usize, String, Color)| *l == line) {
                notes.push((line, e.message.clone(), color));
            }
        }
        (marks, notes)
    }

    /// The errors compiling left in the open files, for `nib ctl state`.
    pub(super) fn compile_errors_in(&self, path: &str) -> Vec<&CompileError> {
        self.compile_errors.values().flatten().filter(|e| nib_proto::path::same(&e.path, path)).collect()
    }
}

type Task = iced::Task<Message>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_entries_apply_over_the_built_ins() {
        let mut user = BTreeMap::new();
        user.insert("typst".to_string(), CompileConfig { command: Some(v(&["/opt/typst/bin/typst", "compile", "{main}"])), ..Default::default() });
        user.insert("latex".to_string(), CompileConfig { command: Some(vec![]), ..Default::default() });
        user.insert("pandoc".to_string(), CompileConfig { command: Some(v(&["pandoc", "{main}", "-o", "{pdf}"])), extensions: v(&["md"]), pdf: Some("{dir}/out/{stem}.pdf".into()) });
        user.insert("half".to_string(), CompileConfig { extensions: v(&["x"]), ..Default::default() });
        let all = compilers(&user);
        assert_eq!(all.len(), 3);
        assert_eq!(for_path(&all, "/d/a.TYP").unwrap().command[0], "/opt/typst/bin/typst");
        assert_eq!(for_path(&all, "/d/a.tex"), None, "turned off");
        assert_eq!(for_path(&all, "/d/notes.md").unwrap().lang, "pandoc");
        assert_eq!(for_path(&all, "/d/a.x"), None);
        assert_eq!(for_path(&all, "/d/Makefile"), None);
    }

    #[test]
    fn placeholders_are_filled_in() {
        let all = compilers(&BTreeMap::new());
        let Request::Compile { main, argv, cwd, pdf } = request(for_path(&all, "/d/paper.typ").unwrap(), "/d/paper.typ", "/d") else { panic!() };
        assert_eq!((main.as_str(), cwd.as_str(), pdf.as_str()), ("/d/paper.typ", "/d", "/d/paper.pdf"));
        assert_eq!(argv, v(&["typst", "compile", "--root", "/d", "--diagnostic-format", "short", "/d/paper.typ", "/d/paper.pdf"]));
        let Request::Compile { argv, .. } = request(for_path(&all, "C:/d/x.tex").unwrap(), "C:/d/x.tex", "C:/d") else { panic!() };
        assert!(argv.contains(&"-outdir=C:/d".to_string()) && argv.last().unwrap() == "C:/d/x.tex");
    }

    /// Typst's root is the folder nib has open, as VS Code's workspace folder is for tinymist:
    /// a document in a subfolder can import `/shared/x.typ` or `../../shared/x.typ`.
    #[test]
    fn typst_root_is_the_project_folder() {
        let all = compilers(&BTreeMap::new());
        let c = for_path(&all, "/p/docs/a/main.typ").unwrap();
        let Request::Compile { argv, cwd, pdf, .. } = request(c, "/p/docs/a/main.typ", "/p/docs") else { panic!() };
        assert_eq!((argv[3].as_str(), cwd.as_str(), pdf.as_str()), ("/p/docs", "/p/docs/a", "/p/docs/a/main.pdf"));
        let Request::Compile { argv, .. } = request(c, "/p/docs/a/main.typ", "/p/docs/") else { panic!() };
        assert_eq!(argv[3], "/p/docs");
        // Outside the project (or only a name prefix of it): the file's own folder.
        let Request::Compile { argv, .. } = request(c, "/tmp/x/main.typ", "/p/docs") else { panic!() };
        assert_eq!(argv[3], "/tmp/x");
        let Request::Compile { argv, .. } = request(c, "/p/docs-old/main.typ", "/p/docs") else { panic!() };
        assert_eq!(argv[3], "/p/docs-old");
        let Request::Compile { argv, .. } = request(c, "C:/p/a/main.typ", "C:/p") else { panic!() };
        assert_eq!(argv[3], "C:/p");
    }

    #[test]
    fn saving_compiles_draws_errors_and_opens_the_pdf_once() {
        use super::super::tests::app;
        use nib_proto::ServerMsg;
        let mut a = app();
        a.link = super::super::Link::Live;
        let answer = |a: &mut App, id: u64, r: Response| {
            let _ = a.update(Message::Server(ServerMsg::Response { id, result: Ok(r) }));
        };
        let pending = |a: &App, f: &dyn Fn(&Pending) -> bool| a.pending.iter().filter(|(_, p)| f(p)).map(|(id, _)| *id).max();

        // Opening: the tab gets an output, and the PDF already built is asked for (not shown).
        a.pending.insert(1, Pending::Open { path: "/d/paper.typ".into(), new_tab: true, goto: None });
        answer(&mut a, 1, Response::File { path: "/d/paper.typ".into(), data: b"= Hi\n#foo\n".to_vec(), mtime_ns: 1 });
        let tab = a.tab().unwrap().id;
        assert_eq!(a.tab().unwrap().out.as_ref().unwrap().pdf, "/d/paper.pdf");
        assert!(pending(&a, &|p| matches!(p, Pending::CompiledPdf { show: false, .. })).is_some());
        assert!(!a.tab().unwrap().rendered);

        // Saved: compiled.
        a.pending.insert(2, Pending::Save { tab, revision: 0 });
        answer(&mut a, 2, Response::Written { path: "/d/paper.typ".into(), mtime_ns: 2 });
        let id = pending(&a, &|p| matches!(p, Pending::Compile { .. })).expect("a compile");
        assert!(a.tab().unwrap().out.as_ref().unwrap().running);

        // It failed: the error is on its line, and said.
        let err = CompileError { path: "/d/paper.typ".into(), line: 1, col: 0, message: "unknown variable: foo".into(), warning: false };
        answer(&mut a, id, Response::Compiled { main: "/d/paper.typ".into(), ok: false, cancelled: false, missing: false, pdf: None, errors: vec![err], log: String::new() });
        let (marks, notes) = a.compile_marks(a.tab().unwrap());
        assert_eq!((marks.len(), notes[0].0, notes[0].1.as_str()), (1, 1, "unknown variable: foo"));
        assert!(a.flash.as_ref().is_some_and(|f| f.0.contains("paper.typ:2: unknown variable: foo")));

        // Fixed and saved again: the errors go, and the PDF is read and shown the first time.
        a.pending.insert(3, Pending::Save { tab, revision: 0 });
        answer(&mut a, 3, Response::Written { path: "/d/paper.typ".into(), mtime_ns: 3 });
        let id = pending(&a, &|p| matches!(p, Pending::Compile { .. })).unwrap();
        answer(&mut a, id, Response::Compiled { main: "/d/paper.typ".into(), ok: true, cancelled: false, missing: false, pdf: Some("/d/paper.pdf".into()), errors: vec![], log: String::new() });
        assert!(a.compile_marks(a.tab().unwrap()).0.is_empty());
        let id = pending(&a, &|p| matches!(p, Pending::CompiledPdf { show: true, .. })).expect("the PDF read");
        answer(&mut a, id, Response::File { path: "/d/paper.pdf".into(), data: b"not a pdf".to_vec(), mtime_ns: 4 });
        assert!(a.tab().unwrap().rendered && a.tab().unwrap().out.as_ref().unwrap().opened);

        // Hidden by the user, it stays hidden after the next build.
        let _ = a.update(Message::Control(nib_editor::Command::TogglePreview));
        assert!(!a.tab().unwrap().rendered);
        a.pending.insert(4, Pending::CompiledPdf { tab, show: true });
        answer(&mut a, 4, Response::File { path: "/d/paper.pdf".into(), data: b"x".to_vec(), mtime_ns: 5 });
        assert!(!a.tab().unwrap().rendered);

        // nibd watches the PDF too.
        assert!(a.compile_watch_paths().any(|p| p == "/d/paper.pdf"));
    }

    #[test]
    fn a_root_line_names_the_file_to_compile() {
        assert_eq!(main_of("/d/ch/intro.tex", ["% !TEX root = ../main.tex", "\\section{x}"].into_iter()), "/d/main.tex");
        assert_eq!(main_of("/d/ch/intro.tex", ["%!tex root=main.tex"].into_iter()), "/d/ch/main.tex");
        assert_eq!(main_of("/d/ch/a.typ", ["// !typst root = /abs/book.typ"].into_iter()), "/abs/book.typ");
        assert_eq!(main_of("/d/a.tex", ["\\documentclass{article}", "% root = x.tex"].into_iter()), "/d/a.tex");
        let late = std::iter::repeat_n("x", 25).chain(["% !TEX root = m.tex"]);
        assert_eq!(main_of("/d/a.tex", late), "/d/a.tex");
    }
}
