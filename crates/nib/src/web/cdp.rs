//! Chromium browsers (Chrome, Edge, Brave, Chromium), over the Chrome DevTools protocol: the
//! other half of what `engine.rs` asks of a browser.
//!
//! A tab is a "target"; nib attaches to it and gets a session, and commands about the tab
//! carry that session. Frames of a tab are named within its session. New targets (tabs nib
//! makes, windows pages open, frames run in another process) are handed to nib paused
//! (`Target.setAutoAttach` with `waitForDebuggerOnStart`), so that request holding
//! (`Fetch.enable`) is on before they load anything; each held request is announced
//! (`Fetch.requestPaused`) and answered `continueRequest`, or, when refused, fulfilled with
//! "204 No Content", which leaves the tab on the page it was on.

use super::engine::{allowed, keep, Arg, Browser, Engine, Frame, Popup, Ret, ANSWER_WITHIN};
use super::page::{key, Stroke};
use super::tab::{bit, Pointer};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// How new targets are handed over: attached, paused, in sessions of their own.
fn auto_attach() -> Value {
    json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true })
}

/// Whether a held request is for a document (a page or a frame): Chromium calls those
/// `Document` whatever started them. `None`: it did not say.
fn is_document(params: &Value) -> Option<bool> {
    params.get("resourceType").and_then(Value::as_str).map(|t| t == "Document")
}

impl Browser {
    /// Chromium holds a request and waits for our word on it.
    fn cdp_request(&mut self, sid: &str, p: &Value) {
        let Some(id) = p["requestId"].as_str().map(str::to_owned) else { return };
        let url = p["request"]["url"].as_str().unwrap_or("").to_owned();
        let stop = json!({ "requestId": id, "responseCode": 204 });
        if let Some((opener, target)) = self.popups.get(sid).map(|w| (w.opener, w.target.clone())) {
            self.send(Some(sid), "Fetch.fulfillRequest", stop);
            self.send(None, "Target.closeTarget", json!({ "targetId": target }));
            self.popup_wants(opener, url);
            return;
        }
        if allowed(&url, is_document(p)) {
            self.send(Some(sid), "Fetch.continueRequest", json!({ "requestId": id }));
            return;
        }
        self.send(Some(sid), "Fetch.fulfillRequest", stop);
        let top = self.top_page(sid).is_some_and(|page| p["frameId"].as_str() == Some(page.main_frame.as_str()));
        self.refused(sid, url, top);
    }

    /// A target was handed to nib, paused. Whatever can load documents gets request holding
    /// before it runs.
    fn cdp_attached(&mut self, via: Option<&str>, p: &Value) {
        let info = &p["targetInfo"];
        let Some(sid) = p["sessionId"].as_str().map(str::to_owned) else { return };
        let target = info["targetId"].as_str().unwrap_or("").to_owned();
        let kind = info["type"].as_str().unwrap_or("");
        if kind == "page" || kind == "iframe" {
            self.send_critical(Some(&sid), "Fetch.enable", json!({ "patterns": [{ "urlPattern": "*" }] }));
            self.send_critical(Some(&sid), "Target.setAutoAttach", auto_attach());
            for domain in ["Page.enable", "Runtime.enable", "Network.enable"] {
                self.send(Some(&sid), domain, json!({}));
            }
        }
        match kind {
            "page" => {
                self.send(Some(&sid), "Emulation.setFocusEmulationEnabled", json!({ "enabled": true }));
                match info["openerId"].as_str() {
                    // A window a page opened.
                    Some(opener) => {
                        let opener = self.pages.iter().find(|pg| pg.target == opener).map(|pg| pg.id);
                        self.popups.insert(sid.clone(), Popup { opener, target });
                    }
                    // A tab nib asked for (`cdp_new_tab` waits for it), or one from nowhere,
                    // which nothing will ever claim or show.
                    None if self.making => self.made = Some((sid.clone(), target)),
                    None => {
                        self.popups.insert(sid.clone(), Popup { opener: None, target });
                    }
                }
            }
            // A frame run in another process: part of the page its parent is part of.
            "iframe" => {
                if let Some(id) = via.and_then(|v| self.owner.get(v).copied()) {
                    self.owner.insert(sid.clone(), id);
                }
            }
            _ => {}
        }
        self.send(Some(&sid), "Runtime.runIfWaitingForDebugger", json!({}));
    }

    pub(super) fn cdp_event(&mut self, m: &Value) {
        let p = &m["params"];
        let sid = m["sessionId"].as_str();
        let session = sid.unwrap_or("");
        match m["method"].as_str().unwrap_or("") {
            "Fetch.requestPaused" => self.cdp_request(session, p),
            "Target.attachedToTarget" => self.cdp_attached(sid, p),
            "Target.detachedFromTarget" => {
                if let Some(gone) = p["sessionId"].as_str() {
                    self.forget(gone);
                }
            }
            "Target.targetDestroyed" | "Target.targetCrashed" => {
                let target = p["targetId"].as_str().unwrap_or("");
                if let Some(ctx) = self.pages.iter().find(|pg| pg.target == target).map(|pg| pg.ctx.clone()) {
                    self.forget(&ctx);
                }
            }
            "Runtime.consoleAPICalled" if matches!(p["type"].as_str(), Some("error" | "warning" | "assert")) => {
                let text: Vec<String> = p["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| match &a["value"] {
                        Value::String(s) => s.clone(),
                        Value::Null => a["description"].as_str().unwrap_or("").to_owned(),
                        v => v.to_string(),
                    })
                    .collect();
                let level = if p["type"] == "warning" { "warn" } else { "error" };
                if let Some(page) = self.page_of(session) {
                    keep(&mut page.console, json!({ "level": level, "text": text.join(" ") }));
                }
            }
            "Runtime.exceptionThrown" => {
                let d = &p["exceptionDetails"];
                let text = d["exception"]["description"].as_str().or(d["text"].as_str()).unwrap_or("uncaught error").lines().next().unwrap_or("").to_owned();
                if let Some(page) = self.page_of(session) {
                    keep(&mut page.console, json!({ "level": "error", "text": text }));
                }
            }
            "Network.requestWillBeSent" => {
                if self.urls.len() > 2000 {
                    self.urls.clear();
                }
                if let (Some(id), Some(url)) = (p["requestId"].as_str(), p["request"]["url"].as_str()) {
                    self.urls.insert(id.to_owned(), url.to_owned());
                }
            }
            "Network.loadingFinished" => {
                self.urls.remove(p["requestId"].as_str().unwrap_or(""));
            }
            "Network.loadingFailed" => {
                let url = self.urls.remove(p["requestId"].as_str().unwrap_or(""));
                // Given up by the page itself (it moved on) is not a failure to report.
                if p["canceled"] != true {
                    let error = p["errorText"].clone();
                    if let (Some(url), Some(page)) = (url, self.page_of(session)) {
                        keep(&mut page.failed, json!({ "url": url, "error": error }));
                    }
                }
            }
            "Network.responseReceived" if p["response"]["status"].as_u64().is_some_and(|s| s >= 400) => {
                let (url, status) = (p["response"]["url"].clone(), p["response"]["status"].clone());
                if let Some(page) = self.page_of(session) {
                    keep(&mut page.failed, json!({ "url": url, "status": status }));
                }
            }
            "Page.frameNavigated" if p["frame"]["parentId"].as_str().is_none() => {
                let frame = &p["frame"];
                if let Some(page) = self.top_page(session) {
                    page.url = format!("{}{}", frame["url"].as_str().unwrap_or(""), frame["urlFragment"].as_str().unwrap_or(""));
                    if let Some(id) = frame["id"].as_str() {
                        page.main_frame = id.to_owned();
                    }
                }
            }
            "Page.navigatedWithinDocument" => {
                if let Some(page) = self.top_page(session).filter(|pg| p["frameId"].as_str() == Some(pg.main_frame.as_str())) {
                    page.url = p["url"].as_str().unwrap_or("").to_owned();
                }
            }
            "Page.frameStartedLoading" => {
                if let Some(page) = self.top_page(session).filter(|pg| p["frameId"].as_str() == Some(pg.main_frame.as_str())) {
                    page.loading = true;
                }
            }
            // The page's own frame is done, whether or not its `load` event came (apps that
            // keep a connection open, marimo's and JupyterLab's, could leave it loading forever).
            "Page.frameStoppedLoading" => {
                if let Some(page) = self.top_page(session).filter(|pg| p["frameId"].as_str() == Some(pg.main_frame.as_str())) {
                    page.loading = false;
                }
            }
            "Page.domContentEventFired" => {
                if let Some(page) = self.top_page(session) {
                    page.dom = true;
                }
            }
            "Page.loadEventFired" => {
                if let Some(page) = self.top_page(session) {
                    page.dom = true;
                    page.loading = false;
                }
            }
            "Page.javascriptDialogOpening" => {
                let dialog = json!({ "type": p["type"], "message": p["message"], "default": p["defaultPrompt"] });
                if let Some(page) = self.page_of(session) {
                    page.dialog = Some(dialog);
                }
            }
            "Page.javascriptDialogClosed" => {
                if let Some(page) = self.page_of(session) {
                    page.dialog = None;
                }
            }
            "Browser.downloadWillBegin" | "Page.downloadWillBegin" => {
                let file = p["suggestedFilename"].clone();
                let id = self.owner.get(session).copied().or(self.current);
                if let Some(page) = id.and_then(|id| self.page(id)) {
                    page.saved.extend(file.as_str().map(str::to_owned));
                    keep(&mut page.downloads, file);
                }
            }
            _ => {}
        }
    }
}

/// A key as the DevTools protocol wants it: its name, its place on the keyboard, the old
/// Windows number for it, and the text it types (if it types any).
fn key_parts(c: char) -> (String, String, u32, Option<String>) {
    let named = |name: &str, code: &str, vk: u32| (name.to_string(), code.to_string(), vk, None);
    match c {
        key::SHIFT => named("Shift", "ShiftLeft", 16),
        key::CONTROL => named("Control", "ControlLeft", 17),
        key::ALT => named("Alt", "AltLeft", 18),
        key::META => named("Meta", "MetaLeft", 91),
        key::ENTER => ("Enter".into(), "Enter".into(), 13, Some("\r".into())),
        key::TAB => named("Tab", "Tab", 9),
        key::BACKSPACE => named("Backspace", "Backspace", 8),
        key::ESCAPE => named("Escape", "Escape", 27),
        key::DELETE => named("Delete", "Delete", 46),
        key::INSERT => named("Insert", "Insert", 45),
        key::LEFT => named("ArrowLeft", "ArrowLeft", 37),
        key::UP => named("ArrowUp", "ArrowUp", 38),
        key::RIGHT => named("ArrowRight", "ArrowRight", 39),
        key::DOWN => named("ArrowDown", "ArrowDown", 40),
        key::HOME => named("Home", "Home", 36),
        key::END => named("End", "End", 35),
        key::PAGE_UP => named("PageUp", "PageUp", 33),
        key::PAGE_DOWN => named("PageDown", "PageDown", 34),
        c if (key::F1..=char::from_u32(key::F1 as u32 + 11).unwrap_or(key::F1)).contains(&c) => {
            let n = c as u32 - key::F1 as u32 + 1;
            (format!("F{n}"), format!("F{n}"), 111 + n, None)
        }
        ' ' => (" ".into(), "Space".into(), 32, Some(" ".into())),
        c if c.is_ascii_alphabetic() => (c.to_string(), format!("Key{}", c.to_ascii_uppercase()), c.to_ascii_uppercase() as u32, Some(c.to_string())),
        c if c.is_ascii_digit() => (c.to_string(), format!("Digit{c}"), c as u32, Some(c.to_string())),
        c => (c.to_string(), String::new(), 0, Some(c.to_string())),
    }
}

/// The DevTools protocol's bit for a modifier key (0: it is not one).
fn modifier_bit(c: char) -> u32 {
    match c {
        key::ALT => 1,
        key::CONTROL => 2,
        key::META => 4,
        key::SHIFT => 8,
        _ => 0,
    }
}

/// On macOS a browser nobody sees does not turn Cmd+A and its kin into the editing they mean;
/// the protocol lets us name it.
fn mac_editing(c: char, mods: u32) -> Option<&'static str> {
    if !cfg!(target_os = "macos") || mods & 4 == 0 || mods & 3 != 0 {
        return None;
    }
    match (c.to_ascii_lowercase(), mods & 8 != 0) {
        ('a', false) => Some("selectAll"),
        ('c', false) => Some("copy"),
        ('x', false) => Some("cut"),
        ('v', false) => Some("paste"),
        ('z', false) => Some("undo"),
        ('z', true) => Some("redo"),
        _ => None,
    }
}

/// One key going down or up, as `Input.dispatchKeyEvent` takes it. `mods`: the modifiers held
/// while it happens.
fn key_event(down: bool, c: char, mods: u32) -> Value {
    let (name, code, vk, text) = key_parts(c);
    // Held Ctrl or Cmd makes a key a command, not a character.
    let text = text.filter(|_| down && mods & 6 == 0);
    let mut e = json!({
        "type": if !down { "keyUp" } else if text.is_some() { "keyDown" } else { "rawKeyDown" },
        "modifiers": mods, "key": name, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk,
    });
    if let Some(t) = text {
        e["text"] = json!(t);
        e["unmodifiedText"] = json!(t);
    }
    if let Some(command) = mac_editing(c, mods).filter(|_| down) {
        e["commands"] = json!([command]);
    }
    e
}

/// The middle of a four-cornered shape given as eight numbers, and its bounding box.
fn quad(q: &Value) -> Option<((f64, f64), (f64, f64, f64, f64))> {
    let n: Vec<f64> = q.as_array()?.iter().filter_map(Value::as_f64).collect();
    if n.len() != 8 {
        return None;
    }
    let (xs, ys) = ([n[0], n[2], n[4], n[6]], [n[1], n[3], n[5], n[7]]);
    let (x0, x1) = (xs.iter().cloned().fold(f64::INFINITY, f64::min), xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
    let (y0, y1) = (ys.iter().cloned().fold(f64::INFINITY, f64::min), ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
    Some((((x0 + x1) / 2.0, (y0 + y1) / 2.0), (x0, y0, x1 - x0, y1 - y0)))
}

impl Engine {
    /// A command about one page.
    fn cdp(&mut self, id: u32, method: &str, params: Value) -> Result<Value> {
        let sid = self.page(id)?.ctx.clone();
        self.command(Some(&sid), method, params)
    }

    pub(super) fn cdp_set_up(&mut self) -> Result<()> {
        // The local-only rule first: from here on every new tab, window and frame process is
        // handed to nib before it loads anything (see `Browser::cdp_attached`).
        self.command(None, "Target.setAutoAttach", auto_attach()).context("it would not hand nib its tabs before they load")?;
        let version = self.command(None, "Browser.getVersion", json!({}))?;
        let targets = self.command(None, "Target.getTargets", json!({}))?;
        if let Some(dir) = std::env::var_os("HOME").or(std::env::var_os("USERPROFILE")).map(|h| std::path::Path::new(&h).join("Downloads")) {
            let _ = self.command(None, "Browser.setDownloadBehavior", json!({ "behavior": "allow", "downloadPath": dir, "eventsEnabled": true }));
        }
        let b = self.live()?;
        b.version = version["product"].as_str().and_then(|p| p.split('/').nth(1)).unwrap_or("").to_owned();
        b.first = targets["targetInfos"].as_array().into_iter().flatten().find(|t| t["type"] == "page").and_then(|t| t["targetId"].as_str()).map(str::to_owned);
        Ok(())
    }

    pub(super) fn cdp_quit(&mut self) {
        let _ = self.command(None, "Browser.close", json!({}));
    }

    pub(super) fn cdp_new_tab(&mut self) -> Result<(String, String, String)> {
        self.live()?.made = None;
        let made = self.command(None, "Target.createTarget", json!({ "url": "about:blank" }))?;
        let target = made["targetId"].as_str().context("the browser made no tab")?.to_owned();
        // It is handed over (attached, with request holding on) by an event of its own.
        let want = target.clone();
        let came = self.until(Duration::from_secs(10), move |e| e.browser.as_ref().is_some_and(|b| b.made.as_ref().is_some_and(|(_, t)| *t == want)))?;
        let sid = self.live()?.made.take().filter(|_| came).map(|(sid, _)| sid).context("the browser did not hand over its new tab")?;
        // A tab's own document goes by the tab's name.
        Ok((sid, target.clone(), target))
    }

    pub(super) fn cdp_close_tab(&mut self, id: u32) {
        if let Some(b) = self.browser.as_mut() {
            if let Some(target) = b.page(id).map(|p| p.target.clone()) {
                b.send(None, "Target.closeTarget", json!({ "targetId": target }));
            }
        }
    }

    pub(super) fn cdp_close_first(&mut self, first: &str) {
        if let Some(b) = self.browser.as_mut() {
            b.send(None, "Target.closeTarget", json!({ "targetId": first }));
        }
    }

    /// Wait until the document asked for can be read.
    fn cdp_loaded(&mut self, id: u32) -> Result<()> {
        if !self.until(ANSWER_WITHIN, move |e| e.dialog_open() || e.page(id).map_or(true, |p| p.dom))? {
            bail!("the page did not load in {} s", ANSWER_WITHIN.as_secs());
        }
        Ok(())
    }

    pub(super) fn cdp_navigate(&mut self, id: u32, url: &str) -> Result<()> {
        self.page(id)?.dom = false;
        let went = self.cdp(id, "Page.navigate", json!({ "url": url }))?;
        if let Some(e) = went["errorText"].as_str().filter(|e| !e.is_empty()) {
            bail!("{e}");
        }
        self.cdp_loaded(id)
    }

    pub(super) fn cdp_reload(&mut self, id: u32) -> Result<()> {
        self.page(id)?.dom = false;
        self.cdp(id, "Page.reload", json!({}))?;
        self.cdp_loaded(id)
    }

    pub(super) fn cdp_frames(&mut self, id: u32) -> Result<Vec<(Frame, String)>> {
        let sid = self.page(id)?.ctx.clone();
        let tree = self.command(Some(&sid), "Page.getFrameTree", json!({}))?;
        fn flat(node: &Value, sid: &str, out: &mut Vec<(Frame, String)>) {
            if let Some(f) = node["frame"]["id"].as_str() {
                out.push((Frame { ctx: sid.to_owned(), frame: Some(f.to_owned()) }, node["frame"]["url"].as_str().unwrap_or("").to_owned()));
            }
            for c in node["childFrames"].as_array().into_iter().flatten() {
                flat(c, sid, out);
            }
        }
        let mut out = Vec::new();
        flat(&tree["frameTree"], &sid, &mut out);
        if let Some((top, _)) = out.first() {
            self.page(id)?.main_frame = top.frame.clone().unwrap_or_default();
        }
        Ok(out)
    }

    pub(super) fn cdp_call(&mut self, frame: &Frame, function: &str, args: &[Arg]) -> Result<Ret> {
        let fid = frame.frame.as_deref().context("no frame to run in")?;
        // nib's own world in that document: made once, found again by name while it lives.
        let world = self.command(Some(&frame.ctx), "Page.createIsolatedWorld", json!({ "frameId": fid, "worldName": "nib" }))?;
        let args: Vec<Value> = args
            .iter()
            .map(|a| match a {
                Arg::Num(n) => json!({ "value": n }),
                Arg::Str(s) => json!({ "value": s }),
            })
            .collect();
        let r = self.command(
            Some(&frame.ctx),
            "Runtime.callFunctionOn",
            json!({ "functionDeclaration": function, "arguments": args, "executionContextId": world["executionContextId"], "awaitPromise": true, "returnByValue": false }),
        )?;
        if let Some(e) = r.get("exceptionDetails").filter(|e| !e.is_null()) {
            let text = e["exception"]["description"].as_str().or(e["text"].as_str()).unwrap_or("script error").lines().next().unwrap_or("");
            bail!("{}", text.strip_prefix("Error: ").unwrap_or(text));
        }
        let result = &r["result"];
        let node = (result["subtype"] == "node").then(|| result["objectId"].as_str().map(|o| json!({ "objectId": o }))).flatten();
        Ok(Ret { value: result["value"].clone(), node })
    }

    pub(super) fn cdp_click(&mut self, frame: &Frame, node: &Value) -> Result<()> {
        let sid = Some(frame.ctx.as_str());
        let quads = self.command(sid, "DOM.getContentQuads", json!({ "objectId": node["objectId"] }))?;
        let ((x, y), _) = quads["quads"].as_array().and_then(|q| q.first()).and_then(quad).context("the element is not shown")?;
        self.command(sid, "Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y }))?;
        self.command(sid, "Input.dispatchMouseEvent", json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1 }))?;
        self.command(sid, "Input.dispatchMouseEvent", json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1 }))?;
        Ok(())
    }

    pub(super) fn cdp_strokes(&mut self, id: u32, strokes: &[Stroke]) -> Result<()> {
        let mut mods = 0u32;
        for (down, c) in strokes {
            let bit = modifier_bit(*c);
            if *down {
                mods |= bit;
            } else {
                mods &= !bit;
            }
            self.cdp(id, "Input.dispatchKeyEvent", key_event(*down, *c, mods))?;
        }
        Ok(())
    }

    pub(super) fn cdp_pointer(&mut self, id: u32, what: Pointer) -> Result<()> {
        let (x, y) = self.tab.at;
        let mods: u32 = self.tab.mods.iter().map(|c| modifier_bit(*c)).sum();
        let name = |b: u8| ["left", "middle", "right"][(b as usize).min(2)];
        let event = match what {
            Pointer::Move => json!({ "type": "mouseMoved", "x": x, "y": y, "buttons": self.tab.buttons, "modifiers": mods }),
            Pointer::Down { button, clicks } => json!({ "type": "mousePressed", "x": x, "y": y, "button": name(button), "buttons": self.tab.buttons, "clickCount": clicks, "modifiers": mods }),
            Pointer::Up { button, clicks } => json!({ "type": "mouseReleased", "x": x, "y": y, "button": name(button), "buttons": self.tab.buttons & !bit(button), "clickCount": clicks, "modifiers": mods }),
        };
        self.cdp(id, "Input.dispatchMouseEvent", event)?;
        Ok(())
    }

    pub(super) fn cdp_wheel(&mut self, id: u32, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        self.cdp(id, "Input.dispatchMouseEvent", json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy }))?;
        Ok(())
    }

    pub(super) fn cdp_shot(&mut self, id: u32, full: bool, element: Option<&(Frame, Value)>) -> Result<String> {
        let mut params = json!({ "format": "png" });
        if element.is_some() || full {
            let metrics = self.cdp(id, "Page.getLayoutMetrics", json!({}))?;
            let clip = match element {
                // The element's box is given from the corner of what shows; a picture is cut
                // from the corner of the document.
                Some((frame, node)) => {
                    let model = self.command(Some(&frame.ctx), "DOM.getBoxModel", json!({ "objectId": node["objectId"] }))?;
                    let (_, (x, y, w, h)) = quad(&model["model"]["border"]).context("the element is not shown")?;
                    let view = &metrics["cssVisualViewport"];
                    json!({ "x": x + view["pageX"].as_f64().unwrap_or(0.0), "y": y + view["pageY"].as_f64().unwrap_or(0.0), "width": w.max(1.0), "height": h.max(1.0), "scale": 1 })
                }
                None => {
                    let size = &metrics["cssContentSize"];
                    json!({ "x": 0, "y": 0, "width": size["width"], "height": size["height"], "scale": 1 })
                }
            };
            params["clip"] = clip;
            params["captureBeyondViewport"] = json!(true);
        }
        let r = self.cdp(id, "Page.captureScreenshot", params)?;
        Ok(r["data"].as_str().context("the browser gave no picture")?.to_owned())
    }

    pub(super) fn cdp_dialog(&mut self, id: u32, accept: bool, text: Option<&str>) -> Result<()> {
        let mut params = json!({ "accept": accept });
        if let Some(t) = text {
            params["promptText"] = json!(t);
        }
        self.cdp(id, "Page.handleJavaScriptDialog", params)?;
        Ok(())
    }

    pub(super) fn cdp_files(&mut self, frame: &Frame, node: &Value, files: &[String]) -> Result<()> {
        self.command(Some(&frame.ctx), "DOM.setFileInputFiles", json!({ "files": files, "objectId": node["objectId"] }))?;
        Ok(())
    }

    pub(super) fn cdp_eval(&mut self, id: u32, expression: &str) -> Result<String> {
        let r = self.cdp(id, "Runtime.evaluate", json!({ "expression": expression, "awaitPromise": true, "returnByValue": true }))?;
        if let Some(e) = r.get("exceptionDetails").filter(|e| !e.is_null()) {
            bail!("{}", e["exception"]["description"].as_str().or(e["text"].as_str()).unwrap_or("script error").lines().next().unwrap_or(""));
        }
        Ok(r["result"]["value"].as_str().unwrap_or("null").to_owned())
    }

    pub(super) fn cdp_viewport(&mut self, id: u32, width: u32, height: u32, scale: f32) -> Result<()> {
        self.cdp(id, "Emulation.setDeviceMetricsOverride", json!({ "width": width.max(50), "height": height.max(50), "deviceScaleFactor": scale, "mobile": false }))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chromium_says_which_requests_are_documents() {
        // What Chrome 157 sends (probed): pages and frames are both "Document".
        assert_eq!(is_document(&json!({ "resourceType": "Document" })), Some(true));
        assert_eq!(is_document(&json!({ "resourceType": "Image" })), Some(false));
        assert_eq!(is_document(&json!({ "resourceType": "XHR" })), Some(false));
        assert_eq!(is_document(&json!({ "resourceType": "Other" })), Some(false));
        // No word on what it is: for the rule to refuse.
        assert_eq!(is_document(&json!({ "request": { "url": "http://x/" } })), None);
        assert_eq!(is_document(&json!({ "resourceType": 3 })), None);
    }

    #[test]
    fn keys_are_spelled_for_the_devtools_protocol() {
        let a = key_event(true, 'a', 0);
        assert_eq!((a["type"].as_str(), a["key"].as_str(), a["code"].as_str(), a["text"].as_str()), (Some("keyDown"), Some("a"), Some("KeyA"), Some("a")));
        assert_eq!(a["windowsVirtualKeyCode"], 65);
        assert_eq!(key_event(false, 'a', 0)["type"], "keyUp");
        assert!(key_event(false, 'a', 0).get("text").is_none());
        // Enter types a line end; Shift held is said in `modifiers`.
        let enter = key_event(true, key::ENTER, 8);
        assert_eq!((enter["key"].as_str(), enter["text"].as_str(), enter["modifiers"].as_u64()), (Some("Enter"), Some("\r"), Some(8)));
        // A key that types nothing; a key made a command by Ctrl.
        assert_eq!(key_event(true, key::LEFT, 0)["type"], "rawKeyDown");
        let ctrl_s = key_event(true, 's', 2);
        assert_eq!(ctrl_s["type"], "rawKeyDown");
        assert!(ctrl_s.get("text").is_none());
        assert_eq!(key_event(true, char::from_u32(key::F1 as u32 + 4).unwrap(), 0)["key"], "F5");
        assert_eq!(key_event(true, 'é', 0)["text"], "é");
        assert_eq!((modifier_bit(key::SHIFT), modifier_bit(key::META), modifier_bit('a')), (8, 4, 0));
        if cfg!(target_os = "macos") {
            assert_eq!(key_event(true, 'a', 4)["commands"], json!(["selectAll"]));
            assert_eq!(key_event(true, 'z', 12)["commands"], json!(["redo"]));
            assert!(key_event(false, 'a', 4).get("commands").is_none());
        }
    }

    #[test]
    fn a_quad_has_a_middle_and_a_box() {
        let q = json!([10.0, 20.0, 110.0, 20.0, 110.0, 60.0, 10.0, 60.0]);
        assert_eq!(quad(&q), Some(((60.0, 40.0), (10.0, 20.0, 100.0, 40.0))));
        assert_eq!(quad(&json!([1, 2, 3])), None);
        assert_eq!(quad(&json!(null)), None);
    }
}
