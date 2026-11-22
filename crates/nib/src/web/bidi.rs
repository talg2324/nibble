//! Firefox, over WebDriver BiDi: one half of what `engine.rs` asks of a browser.
//!
//! A tab or a frame is a "browsing context" with its own id; commands name the context they
//! are about. Firefox holds each request a page makes (`network.addIntercept`) and tells us
//! (`network.beforeRequestSent`); we answer `continueRequest` or `failRequest`.

use super::engine::{allowed, keep, Arg, Browser, Engine, Frame, Popup, Ret};
use super::page::Stroke;
use super::tab::Pointer;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Whether a request Firefox is about to send is for a document (a page or a frame). `None`:
/// an event we cannot read.
fn is_document(params: &Value) -> Option<bool> {
    let request = params.get("request")?;
    let navigation = params.get("navigation")?;
    match request.get("destination").and_then(Value::as_str) {
        Some("document" | "iframe" | "frame" | "embed" | "object") => Some(true),
        _ if !navigation.is_null() => Some(true),
        Some(_) => Some(false),
        None => None,
    }
}

impl Browser {
    /// Firefox is about to send a request and waits for our word on it.
    fn bidi_request(&mut self, p: &Value) {
        if p["isBlocked"] != true {
            return;
        }
        let Some(id) = p["request"]["request"].as_str().map(str::to_owned) else { return };
        let url = p["request"]["url"].as_str().unwrap_or("").to_owned();
        let ctx = p["context"].as_str().unwrap_or("").to_owned();
        if let Some(opener) = self.popups.get(&ctx).map(|w| w.opener) {
            self.send(None, "network.failRequest", json!({ "request": id }));
            self.send(None, "browsingContext.close", json!({ "context": ctx }));
            self.popup_wants(opener, url);
            return;
        }
        if allowed(&url, is_document(p)) {
            self.send(None, "network.continueRequest", json!({ "request": id }));
            return;
        }
        self.send(None, "network.failRequest", json!({ "request": id }));
        let top = self.top_page(&ctx).is_some();
        self.refused(&ctx, url, top);
    }

    pub(super) fn bidi_event(&mut self, m: &Value) {
        let p = &m["params"];
        let ctx = p["context"].as_str().unwrap_or("");
        match m["method"].as_str().unwrap_or("") {
            "network.beforeRequestSent" => self.bidi_request(p),
            "log.entryAdded" if matches!(p["level"].as_str(), Some("error" | "warn")) => {
                let ctx = p["source"]["context"].as_str().unwrap_or("").to_owned();
                if let Some(page) = self.page_of(&ctx) {
                    keep(&mut page.console, json!({ "level": p["level"], "text": p["text"] }));
                }
            }
            "network.fetchError" => {
                let (url, error) = (p["request"]["url"].clone(), p["errorText"].clone());
                if let Some(page) = self.page_of(ctx) {
                    keep(&mut page.failed, json!({ "url": url, "error": error }));
                }
            }
            "network.responseCompleted" if p["response"]["status"].as_u64().is_some_and(|s| s >= 400) => {
                let (url, status) = (p["request"]["url"].clone(), p["response"]["status"].clone());
                if let Some(page) = self.page_of(ctx) {
                    keep(&mut page.failed, json!({ "url": url, "status": status }));
                }
            }
            "browsingContext.contextCreated" => {
                if let Some(parent) = p["parent"].as_str() {
                    // A frame: it belongs to the page its parent belongs to.
                    if let Some(id) = self.owner.get(parent).copied() {
                        self.owner.insert(ctx.to_owned(), id);
                    }
                } else if self.making {
                    // The tab nib just asked for (`Engine::new_page` takes it from here).
                    self.making = false;
                } else if !self.owner.contains_key(ctx) && self.first.as_deref() != Some(ctx) {
                    // A window we did not make: a page did (`window.open`, `target=_blank`).
                    let opener = p["originalOpener"].as_str().and_then(|o| self.owner.get(o).copied());
                    self.popups.insert(ctx.to_owned(), Popup { opener, target: String::new() });
                }
            }
            "browsingContext.contextDestroyed" => self.forget(ctx),
            "browsingContext.navigationStarted" => {
                if let Some(page) = self.top_page(ctx) {
                    page.loading = true;
                }
            }
            "browsingContext.load" | "browsingContext.domContentLoaded" | "browsingContext.fragmentNavigated" | "browsingContext.historyUpdated" => {
                let done = m["method"] == "browsingContext.load";
                if let Some(page) = self.top_page(ctx) {
                    if let Some(url) = p["url"].as_str() {
                        page.url = url.to_owned();
                    }
                    if done {
                        page.loading = false;
                    }
                }
            }
            "browsingContext.userPromptOpened" => {
                let dialog = json!({ "type": p["type"], "message": p["message"], "default": p["defaultValue"] });
                if let Some(page) = self.page_of(ctx) {
                    page.dialog = Some(dialog);
                }
            }
            "browsingContext.userPromptClosed" => {
                if let Some(page) = self.page_of(ctx) {
                    page.dialog = None;
                }
            }
            "browsingContext.downloadWillBegin" => {
                let file = p["suggestedFilename"].clone();
                if let Some(page) = self.page_of(ctx) {
                    page.saved.extend(file.as_str().map(str::to_owned));
                    keep(&mut page.downloads, file);
                }
            }
            _ => {}
        }
    }
}

impl Engine {
    fn bidi(&mut self, method: &str, params: Value) -> Result<Value> {
        self.command(None, method, params)
    }

    pub(super) fn bidi_set_up(&mut self) -> Result<()> {
        // Dialogs wait for an answer (ours, or the user's in the tab) instead of vanishing.
        let session = self.bidi("session.new", json!({ "capabilities": { "alwaysMatch": { "unhandledPromptBehavior": { "default": "ignore" } } } }))?;
        let version = session["capabilities"]["browserVersion"].as_str().unwrap_or("").to_owned();
        // The local-only rule first. Without the events it needs, or without Firefox holding
        // requests for it, pages would load unjudged: then there is no web tab at all.
        self.bidi("session.subscribe", json!({ "events": ["network.beforeRequestSent", "browsingContext.contextCreated", "browsingContext.contextDestroyed"] }))
            .context("it would not tell nib about requests")?;
        self.bidi("network.addIntercept", json!({ "phases": ["beforeRequestSent"] })).context("it would not hold requests for nib")?;
        // The rest is for telling what a page is doing; an older Firefox may lack some.
        for event in [
            "log.entryAdded", "network.fetchError", "network.responseCompleted",
            "browsingContext.navigationStarted", "browsingContext.load", "browsingContext.domContentLoaded",
            "browsingContext.fragmentNavigated", "browsingContext.historyUpdated",
            "browsingContext.userPromptOpened", "browsingContext.userPromptClosed", "browsingContext.downloadWillBegin",
        ] {
            let _ = self.bidi("session.subscribe", json!({ "events": [event] }));
        }
        let tree = self.bidi("browsingContext.getTree", json!({ "maxDepth": 0 }))?;
        let b = self.live()?;
        b.version = version;
        b.first = tree["contexts"][0]["context"].as_str().map(str::to_owned);
        Ok(())
    }

    pub(super) fn bidi_quit(&mut self) {
        let _ = self.bidi("session.end", json!({}));
        let _ = self.bidi("browser.close", json!({}));
    }

    pub(super) fn bidi_new_tab(&mut self) -> Result<(String, String, String)> {
        let made = self.bidi("browsingContext.create", json!({ "type": "tab" }))?;
        Ok((made["context"].as_str().context("Firefox made no tab")?.to_owned(), String::new(), String::new()))
    }

    pub(super) fn bidi_close_tab(&mut self, id: u32) {
        if let Some(b) = self.browser.as_mut() {
            if let Some(ctx) = b.page(id).map(|p| p.ctx.clone()) {
                b.send(None, "browsingContext.close", json!({ "context": ctx }));
            }
        }
    }

    pub(super) fn bidi_close_first(&mut self, first: &str) {
        if let Some(b) = self.browser.as_mut() {
            b.send(None, "browsingContext.close", json!({ "context": first }));
        }
    }

    pub(super) fn bidi_navigate(&mut self, id: u32, url: &str) -> Result<()> {
        let ctx = self.page(id)?.ctx.clone();
        self.bidi("browsingContext.navigate", json!({ "context": ctx, "url": url, "wait": "interactive" }))?;
        Ok(())
    }

    pub(super) fn bidi_reload(&mut self, id: u32) -> Result<()> {
        let ctx = self.page(id)?.ctx.clone();
        self.bidi("browsingContext.reload", json!({ "context": ctx, "wait": "interactive" }))?;
        Ok(())
    }

    pub(super) fn bidi_frames(&mut self, id: u32) -> Result<Vec<(Frame, String)>> {
        let ctx = self.page(id)?.ctx.clone();
        let tree = self.bidi("browsingContext.getTree", json!({ "root": ctx }))?;
        fn flat(nodes: &Value, out: &mut Vec<(Frame, String)>) {
            for n in nodes.as_array().into_iter().flatten() {
                if let Some(c) = n["context"].as_str() {
                    out.push((Frame { ctx: c.to_owned(), frame: None }, n["url"].as_str().unwrap_or("").to_owned()));
                }
                flat(&n["children"], out);
            }
        }
        let mut out = Vec::new();
        flat(&tree["contexts"], &mut out);
        Ok(out)
    }

    pub(super) fn bidi_call(&mut self, frame: &Frame, function: &str, args: &[Arg]) -> Result<Ret> {
        let args: Vec<Value> = args
            .iter()
            .map(|a| match a {
                Arg::Num(n) => json!({ "type": "number", "value": n }),
                Arg::Str(s) => json!({ "type": "string", "value": s }),
            })
            .collect();
        let r = self.bidi(
            "script.callFunction",
            json!({ "functionDeclaration": function, "arguments": args, "awaitPromise": true, "target": { "context": frame.ctx, "sandbox": "nib" } }),
        )?;
        if r["type"] == "exception" {
            let text = r["exceptionDetails"]["text"].as_str().unwrap_or("script error");
            bail!("{}", text.strip_prefix("Error: ").unwrap_or(text));
        }
        let result = &r["result"];
        let node = result["sharedId"].as_str().map(|s| json!({ "sharedId": s }));
        Ok(Ret { value: result["value"].clone(), node })
    }

    pub(super) fn bidi_click(&mut self, frame: &Frame, node: &Value) -> Result<()> {
        let actions = json!([{ "type": "pointer", "id": "mouse", "actions": [
            { "type": "pointerMove", "x": 0, "y": 0, "origin": { "type": "element", "element": node } },
            { "type": "pointerDown", "button": 0 },
            { "type": "pointerUp", "button": 0 },
        ] }]);
        let done = self.bidi("input.performActions", json!({ "context": frame.ctx, "actions": actions }));
        // Buttons and keys let go, whatever happened: the next action starts clean.
        if let Some(b) = self.browser.as_mut() {
            b.send(None, "input.releaseActions", json!({ "context": frame.ctx }));
        }
        done.map(|_| ())
    }

    pub(super) fn bidi_strokes(&mut self, id: u32, strokes: &[Stroke]) -> Result<()> {
        let ctx = self.page(id)?.ctx.clone();
        let acts: Vec<Value> = strokes.iter().map(|(down, c)| json!({ "type": if *down { "keyDown" } else { "keyUp" }, "value": c.to_string() })).collect();
        self.bidi("input.performActions", json!({ "context": ctx, "actions": [{ "type": "key", "id": "keys", "actions": acts }] }))?;
        Ok(())
    }

    pub(super) fn bidi_pointer(&mut self, id: u32, what: Pointer) -> Result<()> {
        let ctx = self.page(id)?.ctx.clone();
        let (x, y) = self.tab.at;
        let mods = self.tab.mods.clone();
        // The modifier keys held with the button: down before it (a pause keeps the mouse a
        // step behind), up after it.
        let held = |down: bool| -> Vec<Value> { mods.iter().map(|c| json!({ "type": if down { "keyDown" } else { "keyUp" }, "value": c.to_string() })).collect() };
        let pause = json!({ "type": "pause" });
        let mouse = |acts: Vec<Value>| json!({ "type": "pointer", "id": "mouse", "actions": acts });
        let keys = |acts: Vec<Value>| json!({ "type": "key", "id": "held", "actions": acts });
        // Firefox counts double clicks itself, from the timing of the presses it is given.
        let actions = match what {
            Pointer::Move => json!([mouse(vec![json!({ "type": "pointerMove", "x": x.round() as i64, "y": y.round() as i64, "duration": 0 })])]),
            Pointer::Down { button, .. } if mods.is_empty() => json!([mouse(vec![json!({ "type": "pointerDown", "button": button })])]),
            Pointer::Down { button, .. } => json!([keys(held(true)), mouse(vec![pause, json!({ "type": "pointerDown", "button": button })])]),
            Pointer::Up { button, .. } if mods.is_empty() => json!([mouse(vec![json!({ "type": "pointerUp", "button": button })])]),
            Pointer::Up { button, .. } => {
                let mut up = vec![pause];
                up.extend(held(false));
                json!([mouse(vec![json!({ "type": "pointerUp", "button": button })]), keys(up)])
            }
        };
        self.bidi("input.performActions", json!({ "context": ctx, "actions": actions }))?;
        Ok(())
    }

    pub(super) fn bidi_wheel(&mut self, id: u32, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        let ctx = self.page(id)?.ctx.clone();
        let wheel = json!([{ "type": "wheel", "id": "wheel", "actions": [{
            "type": "scroll", "x": x.round() as i64, "y": y.round() as i64, "deltaX": dx.round() as i64, "deltaY": dy.round() as i64, "duration": 0,
        }] }]);
        self.bidi("input.performActions", json!({ "context": ctx, "actions": wheel }))?;
        Ok(())
    }

    pub(super) fn bidi_shot(&mut self, id: u32, full: bool, element: Option<&(Frame, Value)>) -> Result<String> {
        let params = match element {
            Some((frame, node)) => json!({ "context": frame.ctx, "clip": { "type": "element", "element": node } }),
            None => json!({ "context": self.page(id)?.ctx, "origin": if full { "document" } else { "viewport" } }),
        };
        let r = self.bidi("browsingContext.captureScreenshot", params)?;
        Ok(r["data"].as_str().context("Firefox gave no picture")?.to_owned())
    }

    pub(super) fn bidi_dialog(&mut self, id: u32, accept: bool, text: Option<&str>) -> Result<()> {
        let mut params = json!({ "context": self.page(id)?.ctx, "accept": accept });
        if let Some(t) = text {
            params["userText"] = json!(t);
        }
        self.bidi("browsingContext.handleUserPrompt", params)?;
        Ok(())
    }

    pub(super) fn bidi_files(&mut self, frame: &Frame, node: &Value, files: &[String]) -> Result<()> {
        self.bidi("input.setFiles", json!({ "context": frame.ctx, "element": node, "files": files }))?;
        Ok(())
    }

    pub(super) fn bidi_eval(&mut self, id: u32, expression: &str) -> Result<String> {
        let ctx = self.page(id)?.ctx.clone();
        let r = self.bidi("script.evaluate", json!({ "expression": expression, "awaitPromise": true, "target": { "context": ctx } }))?;
        if r["type"] == "exception" {
            bail!("{}", r["exceptionDetails"]["text"].as_str().unwrap_or("script error"));
        }
        Ok(r["result"]["value"].as_str().unwrap_or("null").to_owned())
    }

    pub(super) fn bidi_viewport(&mut self, id: u32, width: u32, height: u32, scale: f32) -> Result<()> {
        let ctx = self.page(id)?.ctx.clone();
        self.bidi("browsingContext.setViewport", json!({ "context": ctx, "viewport": { "width": width.max(50), "height": height.max(50) }, "devicePixelRatio": scale }))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(destination: Option<&str>, navigation: Value) -> Value {
        let mut request = json!({ "request": "r1", "url": "http://x/" });
        if let Some(d) = destination {
            request["destination"] = json!(d);
        }
        json!({ "context": "c", "isBlocked": true, "navigation": navigation, "request": request })
    }

    #[test]
    fn firefox_says_which_requests_are_documents() {
        let nav = json!("n1");
        // What Firefox 160 sends (probed): a page, a frame, an image, a fetch.
        assert_eq!(is_document(&request(Some("document"), nav.clone())), Some(true));
        assert_eq!(is_document(&request(Some("iframe"), nav.clone())), Some(true));
        assert_eq!(is_document(&request(Some("image"), Value::Null)), Some(false));
        assert_eq!(is_document(&request(Some(""), Value::Null)), Some(false));
        // A navigation is a document whatever it calls itself; so is a plugin's content.
        assert_eq!(is_document(&request(Some(""), nav.clone())), Some(true));
        assert_eq!(is_document(&request(Some("image"), nav)), Some(true));
        assert_eq!(is_document(&request(Some("object"), Value::Null)), Some(true));
        // An event shape we do not know: no word on it.
        assert_eq!(is_document(&json!({ "request": { "url": "http://x/" } })), None);
        assert_eq!(is_document(&json!({ "navigation": null, "request": { "url": "http://x/" } })), None);
        assert_eq!(is_document(&json!({})), None);
    }
}
