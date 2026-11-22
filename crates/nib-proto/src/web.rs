//! The words of `nib ctl web …` and of an agent's `nib-web …` as the request the window's
//! control socket takes (`{"cmd": "web.read", …}`): one parser for both, and what an agent may
//! ask for of it.

use serde_json::{json, Map, Value};

/// What an agent may ask for through `nib-web`: reading and operating a page as a person at
/// it would. Not `eval` (the page's own scripting world), not `pointer` (the user's mouse in
/// the window), not `quit` (the user's browser), and nothing that is not listed here.
pub const AGENT: &[&str] = &["open", "list", "read", "click", "fill", "select", "type", "key", "scroll", "wait", "shot", "dialog", "upload", "back", "reload", "close"];

/// Whether an agent may make request `cmd` (`web.read`); if not, why.
pub fn agent_may(cmd: &str) -> Result<(), String> {
    match cmd.strip_prefix("web.") {
        Some(c) if AGENT.contains(&c) => Ok(()),
        Some(c @ ("eval" | "pointer" | "quit")) => Err(format!("web {c} is not for agents: read the page and act on its elements by number (read, click N, fill N TEXT, key CHORD)")),
        _ => Err(format!("{cmd:?} is not a request an agent can make of the web tab ({})", AGENT.join(", "))),
    }
}

/// How long a `web.*` request may take before whoever waits for its answer gives up, when
/// that is longer than most: starting the browser, and waiting for a page as long as was asked.
pub fn reply_secs(req: &Value) -> Option<f64> {
    match req.get("cmd").and_then(Value::as_str)? {
        "web.open" => Some(90.0),
        "web.wait" => Some(req.get("secs").and_then(Value::as_f64).unwrap_or(10.0).clamp(0.0, 600.0) + 15.0),
        _ => None,
    }
}

/// `\n`, `\t` and `\\` in text to type, so a shell argument can carry Enter and Tab.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// `p` as an absolute path on this machine (files named on a command line are relative to
/// where it was typed; the window may be elsewhere).
fn absolute(p: &str) -> Result<String, String> {
    std::path::absolute(p).map(|p| p.to_string_lossy().into_owned()).map_err(|e| format!("web: {p}: {e}"))
}

/// The words after `web` as a request. `--tab T` (anywhere) says which web tab; `--secs`,
/// `--full`, `--gone`, `--no-wait`, `--background`, `--ref N` belong to the requests that take
/// them. An error starting `web: unknown request` means the words are no request at all.
pub fn request(a: &[&str]) -> Result<Value, String> {
    let mut req = Map::new();
    let mut words: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < a.len() {
        let value = |name: &str| a.get(i + 1).copied().ok_or_else(|| format!("web: {name} needs a value"));
        match a[i] {
            "--tab" => {
                req.insert("tab".into(), json!(value("--tab")?));
                i += 1;
            }
            "--secs" => {
                req.insert("secs".into(), json!(value("--secs")?.parse::<f64>().map_err(|_| "web: --secs takes a number")?));
                i += 1;
            }
            "--ref" => {
                req.insert("ref".into(), json!(value("--ref")?));
                i += 1;
            }
            "--full" => drop(req.insert("full".into(), json!(true))),
            "--background" => drop(req.insert("background".into(), json!(true))),
            "--gone" => drop(req.insert("gone".into(), json!(true))),
            "--no-wait" => drop(req.insert("wait".into(), json!(false))),
            w => words.push(w),
        }
        i += 1;
    }
    let mut set = |k: &str, v: Value| drop(req.insert(k.into(), v));
    let number = |what: &str, n: &str| n.parse::<f64>().map_err(|_| format!("web {what}"));
    let cmd = match words.as_slice() {
        ["open", url] => {
            set("url", json!(url));
            "open"
        }
        [c @ ("list" | "read" | "close" | "quit" | "back" | "reload")] => c,
        [c @ "click", r] => {
            set("ref", json!(r));
            c
        }
        ["fill", r, text @ ..] => {
            set("ref", json!(r));
            set("text", json!(unescape(&text.join(" "))));
            "fill"
        }
        ["select", r, option @ ..] if !option.is_empty() => {
            set("ref", json!(r));
            set("option", json!(option.join(" ")));
            "select"
        }
        ["type", text] => {
            set("text", json!(unescape(text)));
            "type"
        }
        ["key", chord] => {
            set("chord", json!(chord));
            "key"
        }
        ["scroll", by] => {
            // A plain number is an element; +N / -N is that many pixels down / up.
            match by.strip_prefix(['+', '-']) {
                Some(_) => set("dy", json!(number("scroll: +PIXELS, -PIXELS or an element's number", by)?)),
                None => set("ref", json!(by)),
            }
            "scroll"
        }
        ["pointer", x, y, rest @ ..] => {
            set("x", json!(number("pointer: X Y in the page's pixels", x)?));
            set("y", json!(number("pointer: X Y in the page's pixels", y)?));
            match rest {
                [] => {}
                [what @ ("move" | "click" | "down" | "up")] => set("do", json!(what)),
                ["wheel", dy] => {
                    set("do", json!("wheel"));
                    set("dy", json!(number("pointer wheel: pixels, + is down", dy)?));
                }
                _ => return Err("web pointer X Y [move|click|down|up|wheel PX]".into()),
            }
            "pointer"
        }
        ["wait"] => "wait",
        ["wait", what @ ..] => {
            // A number alone is an element; anything else is text to look for.
            match what {
                [n] if n.parse::<u64>().is_ok() => set("ref", json!(n)),
                _ => set("text", json!(what.join(" "))),
            }
            "wait"
        }
        ["shot"] => "shot",
        ["shot", file] => {
            set("path", json!(absolute(file)?));
            "shot"
        }
        ["dialog", how @ ("accept" | "dismiss"), text @ ..] => {
            set("accept", json!(*how == "accept"));
            if !text.is_empty() {
                set("text", json!(text.join(" ")));
            }
            "dialog"
        }
        ["upload", r, files @ ..] if !files.is_empty() => {
            set("ref", json!(r));
            set("files", json!(files.iter().map(|f| absolute(f)).collect::<Result<Vec<_>, _>>()?));
            "upload"
        }
        ["eval", js @ ..] if !js.is_empty() => {
            set("js", json!(js.join(" ")));
            "eval"
        }
        _ => return Err(format!("web: unknown request {:?}", a.join(" "))),
    };
    req.insert("cmd".into(), json!(format!("web.{cmd}")));
    Ok(Value::Object(req))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_become_the_sockets_requests() {
        let r = |a: &[&str]| request(a).unwrap();
        assert_eq!(r(&["open", "8000"]), json!({ "cmd": "web.open", "url": "8000" }));
        assert_eq!(r(&["open", "gpu:6006", "--background"]), json!({ "cmd": "web.open", "url": "gpu:6006", "background": true }));
        assert_eq!(r(&["read", "--full", "--tab", "8000"]), json!({ "cmd": "web.read", "full": true, "tab": "8000" }));
        assert_eq!(r(&["click", "3"]), json!({ "cmd": "web.click", "ref": "3" }));
        assert_eq!(r(&["fill", "2", "two", "words\\n", "--no-wait"]), json!({ "cmd": "web.fill", "ref": "2", "text": "two words\n", "wait": false }));
        assert_eq!(r(&["select", "4", "New", "York"]), json!({ "cmd": "web.select", "ref": "4", "option": "New York" }));
        assert_eq!(r(&["type", "a\\tb"]), json!({ "cmd": "web.type", "text": "a\tb" }));
        assert_eq!(r(&["key", "shift+enter"]), json!({ "cmd": "web.key", "chord": "shift+enter" }));
        assert_eq!(r(&["scroll", "+300"]), json!({ "cmd": "web.scroll", "dy": 300.0 }));
        assert_eq!(r(&["scroll", "7"]), json!({ "cmd": "web.scroll", "ref": "7" }));
        assert_eq!(r(&["wait", "Saved", "at", "--secs", "30"]), json!({ "cmd": "web.wait", "text": "Saved at", "secs": 30.0 }));
        assert_eq!(r(&["wait", "5", "--gone"]), json!({ "cmd": "web.wait", "ref": "5", "gone": true }));
        assert_eq!(r(&["wait"]), json!({ "cmd": "web.wait" }));
        assert_eq!(r(&["shot", "--ref", "2", "--full"]), json!({ "cmd": "web.shot", "ref": "2", "full": true }));
        assert_eq!(r(&["dialog", "accept", "my", "name"]), json!({ "cmd": "web.dialog", "accept": true, "text": "my name" }));
        assert_eq!(r(&["dialog", "dismiss"]), json!({ "cmd": "web.dialog", "accept": false }));
        assert_eq!(r(&["pointer", "10", "20", "wheel", "-40"]), json!({ "cmd": "web.pointer", "x": 10.0, "y": 20.0, "do": "wheel", "dy": -40.0 }));
        assert_eq!(r(&["eval", "document.title", "+", "1"]), json!({ "cmd": "web.eval", "js": "document.title + 1" }));
        assert_eq!(r(&["close"]), json!({ "cmd": "web.close" }));
        // Files are named from where the command was typed.
        let here = std::env::current_dir().unwrap();
        assert_eq!(r(&["shot", "a.png"])["path"], json!(here.join("a.png")));
        assert_eq!(r(&["upload", "3", "a.csv", "b.csv"])["files"], json!([here.join("a.csv"), here.join("b.csv")]));
        for wrong in [&["dance"][..], &[], &["open"], &["click"], &["select", "1"], &["upload", "1"], &["scroll", "+x"], &["read", "--tab"], &["wait", "--secs", "soon"], &["pointer", "1", "2", "hop"], &["dialog", "maybe"]] {
            assert!(request(wrong).is_err(), "{wrong:?}");
        }
        assert!(request(&["dance"]).unwrap_err().starts_with("web: unknown request"));
    }

    #[test]
    fn an_agent_gets_less_than_the_socket() {
        for c in AGENT {
            assert_eq!(agent_may(&format!("web.{c}")), Ok(()));
        }
        for c in ["web.eval", "web.pointer", "web.quit"] {
            assert!(agent_may(c).unwrap_err().contains("not for agents"), "{c}");
        }
        // Nothing that is not the web tab's, whatever it is called.
        for c in ["web.dance", "web.", "", "state", "run", "ssh.connect", "serve.up", "forward.add", "type", "web.read.x", "WEB.READ", " web.read"] {
            assert!(agent_may(c).is_err(), "{c}");
        }
    }

    #[test]
    fn a_wait_may_take_as_long_as_was_asked() {
        assert_eq!(reply_secs(&json!({ "cmd": "web.wait", "secs": 120 })), Some(135.0));
        assert_eq!(reply_secs(&json!({ "cmd": "web.wait" })), Some(25.0));
        assert_eq!(reply_secs(&json!({ "cmd": "web.open", "url": "1" })), Some(90.0));
        assert_eq!(reply_secs(&json!({ "cmd": "web.read" })), None);
    }
}
