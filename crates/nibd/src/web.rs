//! `nib-web`: an agent reads and operates a page of the project's app in the window's web tab.
//!
//! The page lives in a browser on the machine at the keyboard, driven by the window; the agent
//! runs here, in the daemon's process tree, possibly on another machine, and cannot reach the
//! window's control socket. So `nib-web ARGS` (`nibd web ARGS`, see `cli`) leaves its request
//! as a file in the session's mailbox folder (`web/req/`), as `nib-serve` does; the daemon picks
//! it up and passes it to its window (`ServerNotify::Web`), which runs it as it would a
//! `web.*` request from its socket and answers (`ClientNotify::WebReply`); the daemon writes the
//! answer to `web/res/`, and the command prints it as JSON, as `nib ctl web` does.
//!
//! An agent gets less than the socket: what `nib_proto::web::AGENT` lists (no `eval`, no
//! `pointer`). Refused here before anything is sent, by the daemon, and again by the window.
//!
//! A `shot` is taken where the browser is and written where the agent is: the window answers
//! with the picture itself (`inline`), and the command writes the file.

use nib_proto::{ServerMsg, ServerNotify};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often requests are looked for: when nothing was asked lately, and for a while after
/// something was (an agent's requests come in runs: open, read, click, read).
const IDLE: Duration = Duration::from_millis(30);
const BUSY: Duration = Duration::from_millis(4);
const BUSY_FOR: Duration = Duration::from_secs(3);
/// An answer nobody came for is deleted after this long.
const KEEP: Duration = Duration::from_secs(600);

pub type Shared = Arc<Mutex<Relay>>;

pub struct Relay {
    /// `<mailbox>/web` (made by the first agent that asks).
    dir: PathBuf,
    tx: Sender<ServerMsg>,
    /// Requests passed to the window: the file its answer goes to, and since when.
    waiting: HashMap<u64, (String, Instant)>,
    next: u64,
}

/// The relay for the session whose mailbox folder is `mailbox`, and the thread that looks
/// for requests for as long as it is kept.
pub fn new(mailbox: &Path, tx: Sender<ServerMsg>) -> Shared {
    let shared = Arc::new(Mutex::new(Relay { dir: mailbox.join("web"), tx, waiting: HashMap::new(), next: 1 }));
    let weak = Arc::downgrade(&shared);
    thread::spawn(move || {
        let mut busy_until = Instant::now();
        loop {
            thread::sleep(if Instant::now() < busy_until { BUSY } else { IDLE });
            let Some(shared) = weak.upgrade() else { return };
            let mut relay = shared.lock().unwrap();
            if relay.pick_up() > 0 || !relay.waiting.is_empty() {
                busy_until = Instant::now() + BUSY_FOR;
            }
        }
    });
    shared
}

/// Whole or not at all (written beside, then renamed).
fn write_whole(dir: &Path, name: &str, text: &str) {
    let tmp = dir.join(format!(".{name}.tmp"));
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(tmp, dir.join(name));
    }
}

impl Relay {
    /// Pass the requests agents left to the window, oldest first. How many there were.
    fn pick_up(&mut self) -> usize {
        let Ok(read) = fs::read_dir(self.dir.join("req")) else { return 0 };
        let mut files: Vec<PathBuf> = read.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
        files.sort();
        for file in &files {
            let req: Option<Value> = fs::read(file).ok().and_then(|b| serde_json::from_slice(&b).ok());
            let _ = fs::remove_file(file);
            let Some(name) = file.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
            // Whatever wrote the file: only what an agent may ask goes to the window.
            let refused = match &req {
                Some(r) => nib_proto::web::agent_may(r["cmd"].as_str().unwrap_or("")).err(),
                None => Some("not a request".to_string()),
            };
            if let Some(why) = refused {
                write_whole(&self.dir.join("res"), &name, &json!({ "ok": false, "error": why }).to_string());
                continue;
            }
            let id = self.next;
            self.next += 1;
            self.waiting.insert(id, (name, Instant::now()));
            let _ = self.tx.send(ServerMsg::Notify(ServerNotify::Web { id, req: req.unwrap_or_default().to_string() }));
        }
        files.len()
    }

    /// The window answered request `id`.
    pub fn reply(&mut self, id: u64, reply: &str) {
        let res = self.dir.join("res");
        if let Some((name, _)) = self.waiting.remove(&id) {
            write_whole(&res, &name, reply);
        }
        // What nobody came for (an agent that gave up, or was stopped).
        self.waiting.retain(|_, (_, since)| since.elapsed() < KEEP);
        for old in fs::read_dir(&res).into_iter().flatten().flatten() {
            if old.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > KEEP)) {
                let _ = fs::remove_file(old.path());
            }
        }
    }
}

/// Standard base64 (as the window sends a picture), decoded. `None` if it is not that.
fn unbase64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits, mut pad) = (0u32, 0u32, 0usize);
    for b in text.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad += 1;
                continue;
            }
            b'\n' | b'\r' => continue,
            _ => return None,
        };
        // Nothing follows the padding.
        if pad > 0 {
            return None;
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // Whole bytes only: what is left over is zeros, and padded as such.
    (bits != 6 && acc == 0 && (bits / 2) as usize == pad).then_some(out)
}

const USAGE: &str = "usage: nib-web REQUEST   (the project's app in nib's web tab; answers are JSON)
Only pages of this machine (localhost) are shown; anything else is refused, and never opened.
Every request takes --tab T: a tab's number in `list`, its port, or the start of its name
(default: the tab used last).
  nib-web open PORT|URL [--background]
                               show an app (http://127.0.0.1:PORT/ for a port); the same app again
                               is the same tab. The port is the one on the machine you run on
  nib-web list                 the web tabs: name, address, loading
  nib-web read [--full]        the page as text with what can be acted on marked [N name], those
                               elements, and since the last read: console errors, failed requests,
                               downloads; `blocked`: an address outside this machine the page
                               tried to show; `dialog`: one waiting for an answer
  nib-web click N              click element N of the last read
  nib-web fill N TEXT          empty field N and type TEXT (a <select>: choose that option), then
                               wait 0.4 s (--no-wait: don't)
  nib-web select N OPTION      choose an option of <select> N by its text or value
  nib-web type TEXT | nib-web key CHORD
                               type into, or press a chord in, whatever has the keyboard in the
                               page (key shift+enter, key escape)
  nib-web scroll N | +PX | -PX bring element N into view, or scroll down / up
  nib-web wait [TEXT | N] [--gone] [--secs S]
                               until the page shows TEXT, or element N (--gone: no longer), or
                               with neither: has loaded. Default 10 s; an error if not
  nib-web shot [FILE] [--full] [--ref N]
                               a PNG of the page, of the whole document, or of one element
                               (default: $NIB_SCRATCH/web-NNN.png); answers its path and size
  nib-web dialog accept|dismiss [TEXT]
                               answer an alert / confirm / prompt
  nib-web upload N FILE…       give file input N its files (not when nib's window is on another
                               machine than this one)
  nib-web back | nib-web reload | nib-web close";

/// Where a picture goes when no file was named: `$NIB_SCRATCH/web-NNN.png`, the first unused.
fn next_shot() -> PathBuf {
    let dir = std::env::var_os("NIB_SCRATCH").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
    (1..).map(|n| dir.join(format!("web-{n:03}.png"))).find(|p| !p.exists()).unwrap_or_else(|| dir.join("web.png"))
}

/// Leave `req` for the daemon and wait for the window's answer.
fn ask(dir: &Path, req: &Value) -> Value {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let name = format!("{nanos:032}-{}-{}.json", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let tmp = dir.join("req").join(format!(".{name}.tmp"));
    let sent = fs::write(&tmp, req.to_string()).and_then(|_| fs::rename(&tmp, dir.join("req").join(&name)));
    if let Err(e) = sent {
        return json!({ "ok": false, "error": format!("nib-web: {e}") });
    }
    // As long as the window itself would wait for the page, and a little more for the way.
    let limit = Duration::from_secs_f64(nib_proto::web::reply_secs(req).unwrap_or(30.0) + 5.0);
    let res = dir.join("res").join(&name);
    let started = Instant::now();
    while started.elapsed() < limit {
        if let Some(v) = fs::read(&res).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            let _ = fs::remove_file(&res);
            return v;
        }
        thread::sleep(Duration::from_millis(if started.elapsed() < Duration::from_secs(2) { 4 } else { 25 }));
    }
    let _ = fs::remove_file(dir.join("req").join(&name));
    json!({ "ok": false, "error": "nib did not answer (is its window still open?)" })
}

/// What `nib-web WORDS` asks the window, and where a picture is to be written: or the answer
/// already, when the words are refused here.
fn prepare(words: &[&str]) -> Result<(Value, Option<PathBuf>), String> {
    let mut req = nib_proto::web::request(words)?;
    nib_proto::web::agent_may(req["cmd"].as_str().unwrap_or(""))?;
    let mut shot = None;
    if req["cmd"] == "web.shot" {
        // The picture comes back in the answer and is written here, where the agent is.
        shot = Some(req.as_object_mut().and_then(|o| o.remove("path")).and_then(|p| p.as_str().map(PathBuf::from)).unwrap_or_else(next_shot));
        req["inline"] = json!(true);
    }
    if let Some(missing) = req["files"].as_array().into_iter().flatten().filter_map(Value::as_str).find(|f| !Path::new(f).is_file()) {
        return Err(format!("{missing} is not a file"));
    }
    Ok((req, shot))
}

/// The window's answer to a `shot`, with the picture written to `path` in place of itself.
fn save_shot(mut reply: Value, path: &Path) -> Value {
    let Some(png) = reply.as_object_mut().and_then(|o| o.remove("png")) else { return reply };
    let Some(bytes) = png.as_str().and_then(unbase64) else {
        return json!({ "ok": false, "error": "the picture did not arrive whole" });
    };
    match fs::write(path, bytes) {
        Ok(()) => {
            reply["path"] = json!(path);
            reply
        }
        Err(e) => json!({ "ok": false, "error": format!("writing {}: {e}", path.display()) }),
    }
}

/// `nibd web …`, run by an agent as `nib-web`. Returns the exit code; prints JSON to stdout.
pub fn cli(args: &[String]) -> i32 {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    if matches!(words.as_slice(), [] | ["-h" | "--help" | "help"]) {
        println!("{USAGE}");
        return if words.is_empty() { 2 } else { 0 };
    }
    let dir = std::env::var_os("NIB_MAILBOX").map(PathBuf::from).filter(|d| d.join("outbox").is_dir()).map(|d| d.join("web"));
    let Some(dir) = dir.filter(|d| fs::create_dir_all(d.join("req")).is_ok() && fs::create_dir_all(d.join("res")).is_ok()) else {
        eprintln!("nib-web: only works inside an agent that nib started (NIB_MAILBOX is not set)");
        return 2;
    };
    let reply = match prepare(&words) {
        Ok((req, shot)) => {
            let reply = ask(&dir, &req);
            match shot {
                Some(path) if reply["ok"] == true => save_shot(reply, &path),
                _ => reply,
            }
        }
        Err(e) if e.starts_with("web: unknown request") => {
            eprintln!("nib-web: unknown request {:?}\n{USAGE}", words.join(" "));
            return 2;
        }
        Err(e) => json!({ "ok": false, "error": e }),
    };
    println!("{}", serde_json::to_string_pretty(&reply).unwrap_or_default());
    i32::from(reply["ok"] != true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nibd-web-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("web/req")).unwrap();
        fs::create_dir_all(d.join("web/res")).unwrap();
        d
    }

    #[test]
    fn base64_is_decoded_and_anything_else_is_not() {
        let d = |s: &str| unbase64(s).map(|b| String::from_utf8_lossy(&b).into_owned());
        assert_eq!([d(""), d("Zg=="), d("Zm8="), d("Zm9v"), d("Zm9vYg=="), d("Zm9vYmE="), d("Zm9v\nYmFy")], ["", "f", "fo", "foo", "foob", "fooba", "foobar"].map(|s| Some(s.to_string())));
        assert_eq!(unbase64("AP+A/w==").unwrap(), [0, 255, 128, 255]);
        // Every byte value, against the encoder the chat's pictures use.
        let all: Vec<u8> = (0..=255u8).chain(0..7).collect();
        for n in 0..all.len() {
            assert_eq!(unbase64(&crate::ai::base64(&all[..n])).as_deref(), Some(&all[..n]), "{n} bytes");
        }
        for wrong in ["Z", "Zg", "Zg=", "Zm8", "Zg==Zg==", "Zm9v=", "Zh==", "Zm9=", "Z g=", "Zm9v!", "====", "Zm9vY==="] {
            assert_eq!(unbase64(wrong), None, "{wrong:?}");
        }
    }

    #[test]
    fn an_agent_is_refused_what_is_not_for_it_before_anything_is_sent() {
        assert!(prepare(&["eval", "document.cookie"]).unwrap_err().contains("not for agents"));
        assert!(prepare(&["pointer", "1", "2"]).unwrap_err().contains("not for agents"));
        assert!(prepare(&["quit"]).unwrap_err().contains("not for agents"));
        assert!(prepare(&["dance"]).unwrap_err().starts_with("web: unknown request"));
        assert!(prepare(&["upload", "1", "/no/such/file.csv"]).unwrap_err().contains("is not a file"));
        assert_eq!(prepare(&["read", "--tab", "8000"]).unwrap(), (json!({ "cmd": "web.read", "tab": "8000" }), None));
        // A picture is asked for itself, and written where the agent said (or in its scratch).
        let (req, to) = prepare(&["shot", "/tmp/x.png", "--ref", "3"]).unwrap();
        assert_eq!((req, to), (json!({ "cmd": "web.shot", "ref": "3", "inline": true }), Some(PathBuf::from("/tmp/x.png"))));
        let (req, to) = prepare(&["shot"]).unwrap();
        assert!(req["inline"] == true && to.unwrap().file_name().unwrap().to_string_lossy().starts_with("web-"));
    }

    #[test]
    fn a_request_goes_to_the_window_and_its_answer_back_to_who_asked() {
        let d = temp("relay");
        let (tx, rx) = mpsc::channel();
        let relay = new(&d, tx);
        // The window: answers what it is sent.
        let window = relay.clone();
        thread::spawn(move || {
            for m in rx {
                let ServerMsg::Notify(ServerNotify::Web { id, req }) = m else { continue };
                let req: Value = serde_json::from_str(&req).unwrap();
                window.lock().unwrap().reply(id, &json!({ "ok": true, "echo": req }).to_string());
            }
        });
        let dir = d.join("web");
        let (req, _) = prepare(&["click", "4"]).unwrap();
        assert_eq!(ask(&dir, &req), json!({ "ok": true, "echo": { "cmd": "web.click", "ref": "4" } }));
        // Several at once each get their own answer.
        let asked: Vec<_> = (0..8).map(|n| (n, dir.clone())).map(|(n, dir)| thread::spawn(move || ask(&dir, &json!({ "cmd": "web.fill", "ref": n }))["echo"]["ref"].clone())).collect();
        assert_eq!(asked.into_iter().map(|t| t.join().unwrap()).collect::<Vec<_>>(), (0..8).map(|n| json!(n)).collect::<Vec<_>>());
        // A file that is not what `nib-web` writes never reaches the window.
        for (n, bad) in ["{ \"cmd\": \"web.eval\", \"js\": \"1\" }", "{ \"cmd\": \"run\", \"command\": \"quit\" }", "[]", "junk"].iter().enumerate() {
            fs::write(dir.join("req").join(format!("bad{n}.json")), bad).unwrap();
        }
        let t = Instant::now();
        while !(0..4).all(|n| dir.join("res").join(format!("bad{n}.json")).is_file()) {
            assert!(t.elapsed() < Duration::from_secs(10), "refusals never written");
            thread::sleep(Duration::from_millis(10));
        }
        for n in 0..4 {
            let v: Value = serde_json::from_slice(&fs::read(dir.join("res").join(format!("bad{n}.json"))).unwrap()).unwrap();
            assert_eq!(v["ok"], false, "{v}");
            assert!(v.get("echo").is_none());
        }
        assert!(relay.lock().unwrap().waiting.is_empty());
        drop(relay);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_picture_is_written_where_the_agent_is() {
        let d = temp("shot");
        let path = d.join("page.png");
        let png = b"\x89PNG\r\n\x1a\n rest of it";
        let got = save_shot(json!({ "ok": true, "png": crate::ai::base64(png), "width": 800, "height": 600 }), &path);
        assert_eq!(got, json!({ "ok": true, "path": path, "width": 800, "height": 600 }));
        assert_eq!(fs::read(&path).unwrap(), png);
        assert_eq!(save_shot(json!({ "ok": true, "png": "not base64!" }), &path)["ok"], false);
        assert_eq!(save_shot(json!({ "ok": true, "png": "Zm9v" }), &d.join("no/such/folder/x.png"))["ok"], false);
        let _ = fs::remove_dir_all(&d);
    }
}
