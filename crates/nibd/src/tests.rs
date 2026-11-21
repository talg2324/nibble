use super::*;
use nib_proto::{recv, send, ClientMsg, Request, Response, ServerMsg};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn run(reqs: Vec<Request>) -> Vec<Result<Response, nib_proto::RpcError>> {
    let mut input = Vec::new();
    for (i, req) in reqs.into_iter().enumerate() {
        send(&mut input, &ClientMsg::Request { id: i as u64, req }).unwrap();
    }
    let sink = Sink::default();
    serve(&input[..], sink.clone()).unwrap();
    let out = sink.0.lock().unwrap().clone();
    let mut r = &out[..];
    let mut res = Vec::new();
    while let Some(ServerMsg::Response { result, .. }) = recv(&mut r).unwrap() {
        res.push(result);
    }
    res
}

fn hello(root: &std::path::Path) -> Request {
    Request::Hello {
        protocol: PROTOCOL_VERSION,
        client_version: "t".into(),
        root: root.to_string_lossy().into(),
        lsp: vec![],
    }
}

#[test]
fn read_write_list_conflict() {
    let dir = std::env::temp_dir().join(format!("nibd-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("a.py"), b"x = 1\n").unwrap();

    let res = run(vec![
        hello(&dir),
        Request::ReadFile {
            path: "a.py".into(),
        },
        Request::WriteFile {
            path: "b.py".into(),
            data: b"y = 2\n".to_vec(),
            expected_mtime_ns: None,
        },
        Request::WriteFile {
            path: "a.py".into(),
            data: b"z\n".to_vec(),
            expected_mtime_ns: Some(1),
        },
        Request::ListDir { path: ".".into() },
    ]);
    assert!(matches!(res[0], Ok(Response::Hello { .. })));
    match &res[1] {
        Ok(Response::File { data, .. }) => assert_eq!(data, b"x = 1\n"),
        r => panic!("{r:?}"),
    }
    assert!(matches!(res[2], Ok(Response::Written { .. })));
    assert_eq!(std::fs::read(dir.join("b.py")).unwrap(), b"y = 2\n");
    assert_eq!(res[3].as_ref().unwrap_err().kind, ErrorKind::Conflict);
    assert_eq!(std::fs::read(dir.join("a.py")).unwrap(), b"x = 1\n");
    match &res[4] {
        Ok(Response::Dir { entries, .. }) => {
            let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["sub", "a.py", "b.py"]);
        }
        r => panic!("{r:?}"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn hello_required_first() {
    let res = run(vec![Request::ReadFile { path: "x".into() }]);
    assert_eq!(res[0].as_ref().unwrap_err().kind, ErrorKind::BadRequest);
}

#[test]
fn pty_runs_command_and_reports_exit() {
    let (tx, rx) = mpsc::channel();
    let mut ptys = pty::Ptys::default();
    let cmd = if cfg!(windows) {
        Some(vec!["cmd.exe".into(), "/C".into(), "echo hi-from-pty".into()])
    } else {
        Some(vec!["sh".into(), "-c".into(), "printf hi-from-pty".into()])
    };
    let id = ptys.open(&std::env::temp_dir(), cmd, 80, 24, &[], tx).unwrap();
    let mut out = Vec::new();
    let mut exited = false;
    while let Ok(m) = rx.recv_timeout(std::time::Duration::from_secs(5)) {
        match m {
            ServerMsg::Notify(ServerNotify::PtyOutput { id: i, data }) if i == id => {
                // Windows' ConPTY asks where the cursor is and waits for the answer (the client
                // gives it; see nib's `status_replies`).
                if data.windows(4).any(|w| w == b"\x1b[6n") {
                    ptys.input(id, b"\x1b[1;1R");
                }
                out.extend(data);
            }
            ServerMsg::Notify(ServerNotify::PtyExit { id: i, code }) if i == id => {
                assert_eq!(code, Some(0));
                exited = true;
                break;
            }
            _ => {}
        }
    }
    assert!(exited);
    assert!(String::from_utf8_lossy(&out).contains("hi-from-pty"));
}

#[test]
fn git_info_outside_and_inside_a_repo() {
    use nib_proto::GitInfo;
    let dir = std::env::temp_dir().join(format!("nibd-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("plain")).unwrap();
    std::fs::create_dir_all(dir.join("proj/sub")).unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(dir.join("proj")).args(args).output().map(|o| o.status.success())
    };
    if !git(&["init", "-q", "-b", "trunk"]).unwrap_or(false) {
        // No git here: that is the other answer.
        assert_eq!(git::info(&dir.join("plain")), GitInfo::NoGit);
        return;
    }
    // A stray repository above the temp dir would make "plain" look tracked.
    let outside = git::info(&dir.join("plain"));
    assert!(matches!(outside, GitInfo::NotRepo | GitInfo::Repo(_)));
    if std::process::Command::new("git").arg("-C").arg(&dir).args(["rev-parse", "--show-toplevel"]).output().is_ok_and(|o| !o.status.success()) {
        assert_eq!(outside, GitInfo::NotRepo);
    }

    for args in [&["config", "user.name", "Nib Tester"][..], &["config", "user.email", "nib@example.com"]] {
        assert!(git(args).unwrap());
    }
    let GitInfo::Repo(r) = git::info(&dir.join("proj")) else { panic!("not a repo") };
    assert_eq!((r.name.as_str(), r.branch.as_deref(), &r.last_commit), ("proj", Some("trunk"), &None));
    assert_eq!(r.user_name.as_deref(), Some("Nib Tester"));
    assert_eq!(r.user_email.as_deref(), Some("nib@example.com"));

    assert!(git(&["commit", "-q", "--allow-empty", "--no-gpg-sign", "-m", "first: a | b"]).unwrap());
    // Asked from a subdirectory, over the wire.
    let res = run(vec![hello(&dir.join("proj/sub")), Request::GitInfo]);
    let Ok(Response::Git(GitInfo::Repo(r))) = &res[1] else { panic!("{:?}", res[1]) };
    let c = r.last_commit.as_ref().unwrap();
    assert_eq!((r.name.as_str(), c.subject.as_str(), c.author.as_str()), ("proj", "first: a | b", "Nib Tester"));
    assert!(c.time > 1_600_000_000 && !c.hash.is_empty());

    assert!(git(&["checkout", "-q", "--detach"]).unwrap());
    let GitInfo::Repo(r) = git::info(&dir.join("proj")) else { panic!("not a repo") };
    assert_eq!(r.branch, None);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn git_details_and_operations() {
    use nib_proto::GitOp;
    let dir = std::env::temp_dir().join(format!("nibd-gitops-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (origin, work) = (dir.join("origin.git"), dir.join("work"));
    std::fs::create_dir_all(&work).unwrap();
    let sh = |cwd: &std::path::Path, args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(cwd).args(args).output().is_ok_and(|o| o.status.success())
    };
    if !sh(&dir, &["init", "-q", "--bare", "-b", "trunk", "origin.git"]) {
        return; // no git on this machine
    }
    assert!(sh(&work, &["init", "-q", "-b", "trunk"]));
    for args in [
        &["config", "user.name", "Nib Tester"][..],
        &["config", "user.email", "nib@example.com"],
        &["config", "commit.gpgsign", "false"],
        &["remote", "add", "origin", origin.to_str().unwrap()],
    ] {
        assert!(sh(&work, args));
    }
    assert_eq!(git::details(&work, 10), (vec![], vec![]));

    // Nothing staged: the commit takes everything, untracked files included.
    std::fs::write(work.join("a.txt"), "one\n").unwrap();
    std::fs::write(work.join("b c.txt"), "two\n").unwrap();
    let (_, changes) = git::details(&work, 10);
    let seen: Vec<_> = changes.iter().map(|c| (c.code.as_str(), c.path.as_str(), c.staged)).collect();
    assert_eq!(seen, [("??", "a.txt", false), ("??", "b c.txt", false)]);
    let (ok, out) = git::run(&work, &GitOp::Commit { message: "first".into() });
    assert!(ok, "{out}");
    assert_eq!(git::details(&work, 10).1, vec![]);

    // Something staged: only that is committed.
    std::fs::write(work.join("a.txt"), "one!\n").unwrap();
    std::fs::write(work.join("b c.txt"), "two!\n").unwrap();
    assert!(sh(&work, &["add", "a.txt"]));
    assert!(sh(&work, &["mv", "b c.txt", "d.txt"]));
    assert!(sh(&work, &["reset", "-q", "--", "b c.txt", "d.txt"]));
    let (ok, out) = git::run(&work, &GitOp::Commit { message: "second \"quoted\"".into() });
    assert!(ok, "{out}");
    let (commits, changes) = git::details(&work, 10);
    assert_eq!(commits.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), ["second \"quoted\"", "first"]);
    assert!(changes.iter().all(|c| !c.staged) && changes.iter().any(|c| c.path == "d.txt"), "{changes:?}");
    assert_eq!(git::details(&work, 1).0.len(), 1);

    // Stash and pop round-trip the leftovers (a plain stash leaves untracked files alone).
    assert!(git::run(&work, &GitOp::Stash).0);
    assert_eq!(git::details(&work, 10).1.iter().map(|c| c.code.as_str()).collect::<Vec<_>>(), ["??"]);
    assert!(git::run(&work, &GitOp::StashPop).0);
    assert_eq!(git::details(&work, 10).1.len(), changes.len());

    // A failure comes back as git's own words, over the wire.
    let res = run(vec![hello(&work), Request::GitRun { op: GitOp::Push }]);
    let Ok(Response::GitRan { ok, output }) = &res[1] else { panic!("{:?}", res[1]) };
    assert!(!ok && output.contains("upstream"), "{output}");
    assert!(sh(&work, &["push", "-q", "-u", "origin", "trunk"]));
    assert!(git::run(&work, &GitOp::Fetch).0);
    let (ok, out) = git::run(&work, &GitOp::Pull);
    assert!(ok && out.contains("up to date"), "{out}");
    std::fs::write(work.join("a.txt"), "three\n").unwrap();
    assert!(git::run(&work, &GitOp::Commit { message: "third".into() }).0);
    let (ok, out) = git::run(&work, &GitOp::Push);
    assert!(ok, "{out}");
    assert!(sh(&origin, &["rev-parse", "--verify", "-q", "trunk~2"]));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn tree_snapshot_file_operations_and_staging() {
    use nib_proto::{FsOp, GitOp};
    let dir = std::env::temp_dir().join(format!("nibd-tree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let work = dir.join("work");
    std::fs::create_dir_all(work.join("src/deep")).unwrap();
    std::fs::create_dir_all(work.join("target/debug")).unwrap();
    std::fs::write(work.join("src/a.rs"), "a\n").unwrap();
    std::fs::write(work.join("src/deep/b.rs"), "b\n").unwrap();
    std::fs::write(work.join("target/debug/big"), "x").unwrap();
    std::fs::write(work.join(".gitignore"), "target/\n").unwrap();
    let paths = |s: &nib_proto::TreeSnapshot| s.entries.as_ref().unwrap().iter().map(|e| e.path.clone()).collect::<Vec<_>>();

    // Without git: everything, breadth first.
    let plain = tree::snapshot(&work, None, true, &[]);
    if !plain.repo {
        assert_eq!(paths(&plain), [".gitignore", "src", "target", "src/a.rs", "src/deep", "target/debug", "src/deep/b.rs", "target/debug/big"]);
    }
    let sh = |args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(&work).args(args).output().is_ok_and(|o| o.status.success())
    };
    if !sh(&["init", "-q", "-b", "trunk"]) {
        return; // no git on this machine
    }
    for args in [&["config", "user.name", "Nib Tester"][..], &["config", "user.email", "nib@example.com"], &["config", "commit.gpgsign", "false"]] {
        assert!(sh(args));
    }
    let snap = tree::snapshot(&work, None, true, &[]);
    assert!(snap.repo && !snap.truncated);
    assert_eq!(paths(&snap), [".gitignore", "src", "target", "src/a.rs", "src/deep", "src/deep/b.rs"]);
    let target = &snap.entries.as_ref().unwrap()[2];
    assert!(target.dir && target.ignored);
    // An unfolded ignored folder: one level, everything in it ignored too.
    let opened = tree::snapshot(&work, None, true, &["target".into()]);
    assert_eq!(paths(&opened), [".gitignore", "src", "target", "src/a.rs", "src/deep", "target/debug", "src/deep/b.rs"]);
    assert!(opened.entries.as_ref().unwrap()[5].ignored);
    let deeper = tree::snapshot(&work, None, true, &["target".into(), "target/debug".into()]);
    assert_eq!(paths(&deeper).last().map(String::as_str), Some("target/debug/big"));
    let codes = |s: &nib_proto::TreeSnapshot| s.changes.iter().map(|c| (c.code.clone(), c.path.clone())).collect::<Vec<_>>();
    assert_eq!(codes(&snap).len(), 3, "{:?}", snap.changes);
    // Same listing: not sent again.
    assert_eq!(tree::snapshot(&work, Some(snap.stamp), true, &[]).entries, None);

    // Stage one file before the first commit, take it back, then commit everything.
    let stage = |p: &str| git::run(&work, &GitOp::Stage { paths: vec![p.into()] });
    assert!(stage("src/a.rs").0);
    assert!(codes(&tree::snapshot(&work, None, true, &[])).contains(&("A ".into(), "src/a.rs".into())));
    assert!(git::run(&work, &GitOp::Unstage { paths: vec!["src/a.rs".into()] }).0);
    assert!(codes(&tree::snapshot(&work, None, true, &[])).contains(&("??".into(), "src/a.rs".into())));
    assert!(git::run(&work, &GitOp::Stage { paths: vec![] }).0);
    assert!(tree::snapshot(&work, None, true, &[]).changes.iter().all(|c| !c.staged), "no paths must stage nothing");
    assert!(git::run(&work, &GitOp::Commit { message: "first".into() }).0);
    assert_eq!(tree::snapshot(&work, None, true, &[]).changes, vec![]);

    // Over the wire, from a subfolder of the repository: paths are relative to that folder.
    let res = run(vec![
        hello(&work.join("src")),
        Request::Fs { place: Default::default(), op: FsOp::CreateFile { path: "ui/panel.rs".into() } },
        Request::Fs { place: Default::default(), op: FsOp::CreateFile { path: "ui/panel.rs".into() } },
        Request::Fs { place: Default::default(), op: FsOp::Rename { from: "a.rs".into(), to: "z.rs".into() } },
        Request::Fs { place: Default::default(), op: FsOp::Delete { paths: vec!["deep".into()] } },
        Request::Fs { place: Default::default(), op: FsOp::CreateDir { path: "../escape".into() } },
    ]);
    assert_eq!(res[1], Ok(Response::Ok));
    assert!(res[2].is_err(), "creating over an existing file must fail");
    assert_eq!((&res[3], &res[4]), (&Ok(Response::Ok), &Ok(Response::Ok)));
    assert!(res[5].is_err() && !work.join("escape").exists());
    let res = run(vec![hello(&work.join("src")), Request::Tree { known: None, place: Default::default(), open: vec![] }]);
    let Ok(Response::Tree(snap)) = &res[1] else { panic!("{:?}", res[1]) };
    assert_eq!(paths(snap), ["ui", "z.rs", "ui/panel.rs"]);
    let mut got = codes(snap);
    got.sort();
    // The tracked file was renamed with `git mv`; the deleted one is only gone from disk.
    assert_eq!(got, [(" D".into(), "deep/b.rs".into()), ("??".into(), "ui/panel.rs".into()), ("R ".into(), "z.rs".into())]);

    // Unstage with a HEAD: the rename falls apart into a deletion and a new file.
    assert!(git::run(&work.join("src"), &GitOp::Unstage { paths: vec!["z.rs".into(), "a.rs".into()] }).0);
    assert!(tree::snapshot(&work.join("src"), None, true, &[]).changes.iter().all(|c| !c.staged));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn scratch_and_nibdir() {
    use nib_proto::FsOp;
    let paths = |s: &nib_proto::TreeSnapshot| s.entries.iter().flatten().map(|e| e.path.clone()).collect::<Vec<_>>();
    let dir = std::env::temp_dir().join(format!("nibd-places-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fs = |place, op| Request::Fs { place, op };
    let res = run(vec![
        hello(&dir),
        fs(Place::Nibdir, FsOp::CreateFile { path: "plots/loss.png".into() }),
        fs(Place::Scratch, FsOp::CreateFile { path: "try.py".into() }),
        fs(Place::Nibdir, FsOp::CreateDir { path: "../escape".into() }),
    ]);
    // Tree answers come from threads, in any order: one per run.
    let tree = |place| run(vec![hello(&dir), Request::Tree { known: None, place, open: vec![] }]).pop().unwrap();
    let Ok(Response::Hello { scratch, nibdir, .. }) = &res[0] else { panic!("{:?}", res[0]) };
    let (scratch, nibdir) = (std::path::PathBuf::from(scratch), std::path::PathBuf::from(nibdir));
    assert!(nibdir.join("plots/loss.png").is_file() && scratch.join("try.py").is_file());
    assert!(res[3].is_err() && !nibdir.parent().unwrap().join("escape").exists());
    let Ok(Response::Tree(n)) = tree(Place::Nibdir) else { panic!() };
    assert!(paths(&n).contains(&"plots/loss.png".to_string()) && !n.repo, "{:?}", paths(&n));
    let Ok(Response::Tree(s)) = tree(Place::Scratch) else { panic!() };
    assert_eq!(paths(&s), ["try.py"]);
    // A project in a temp directory gets a throwaway NIBDIR, next to its scratch.
    assert_eq!(nibdir.file_name().unwrap().to_string_lossy(), format!("{}.nibdir", scratch.file_name().unwrap().to_string_lossy()));
    // The same project always gets the same NIBDIR; scratch is per daemon.
    let again = run(vec![hello(&dir)]);
    let Ok(Response::Hello { nibdir: n2, .. }) = &again[0] else { panic!() };
    assert_eq!(std::path::PathBuf::from(n2), nibdir);
    let _ = std::fs::remove_dir_all(&nibdir);
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn search_names_and_lines_skipping_hidden_ignored_and_binary() {
    let dir = std::env::temp_dir().join(format!("nibd-search-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for d in ["src/deep", ".hidden", "build"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    std::fs::write(dir.join("src/main.rs"), "fn main() {\n    let Needle = 1;\n}\n").unwrap();
    std::fs::write(dir.join("src/deep/needle_util.rs"), "// a needle here\n").unwrap();
    std::fs::write(dir.join(".hidden/x.rs"), "needle\n").unwrap();
    std::fs::write(dir.join("build/out.rs"), "needle\n").unwrap();
    std::fs::write(dir.join("blob.bin"), b"needle\0\x01").unwrap();
    std::fs::write(dir.join(".gitignore"), "build/\n").unwrap();
    let git = std::process::Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]).status().is_ok_and(|s| s.success());

    let res = run(vec![hello(&dir), Request::Search { query: "needle".into(), max: 100 }]);
    let Ok(Response::Search(found)) = &res[1] else { panic!("{:?}", res[1]) };
    let mut hits: Vec<(String, u32, u32)> =
        found.lines.iter().map(|l| (l.path.rsplit_once(&format!("{}/", dir.file_name().unwrap().to_string_lossy())).unwrap().1.to_string(), l.start.line, l.start.col)).collect();
    hits.sort();
    let mut want = vec![("src/deep/needle_util.rs".to_string(), 0, 5), ("src/main.rs".to_string(), 1, 8)];
    if !git {
        want.insert(0, ("build/out.rs".to_string(), 0, 0));
    }
    assert_eq!(hits, want, "smart case, no hidden folders, no binaries, no ignored files");
    assert_eq!(found.files.first().map(String::as_str), Some("src/deep/needle_util.rs"));

    // A capital makes it exact.
    let res = run(vec![hello(&dir), Request::Search { query: "Needle".into(), max: 100 }]);
    let Ok(Response::Search(found)) = &res[1] else { panic!() };
    assert_eq!(found.lines.len(), 1);
    assert_eq!(found.lines[0].line_text, "    let Needle = 1;");
    // The cap.
    let res = run(vec![hello(&dir), Request::Search { query: "needle".into(), max: 1 }]);
    let Ok(Response::Search(found)) = &res[1] else { panic!() };
    assert!(found.lines.len() == 1 && found.truncated);
    std::fs::remove_dir_all(&dir).unwrap();
}
