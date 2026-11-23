#!/usr/bin/env python3
"""Drives a nib window's web requests against scripts/test/e2e-web/site.py and checks what comes back.

  check.py NIB PID INSIDE_PORT OUTSIDE_URL OUTSIDE_LOG

Exits 1 at the first thing that is not as it should be. Standard library only.
"""
import json
import os
import subprocess
import sys
import tempfile
import time

NIB, PID, PORT, OUT, LOG = sys.argv[1:6]


def web(*args, ok=True):
    r = subprocess.run([NIB, "ctl", "-p", PID, "web", *args], capture_output=True, text=True)
    try:
        v = json.loads(r.stdout)
    except ValueError:
        sys.exit(f"web {' '.join(args)}: no JSON: {r.stdout!r} {r.stderr!r}")
    if ok and not v.get("ok"):
        sys.exit(f"web {' '.join(args)}: {v.get('error')}")
    return v


def check(what, cond, got=None):
    if not cond:
        sys.exit(f"FAILED: {what}" + (f"\n  got: {got!r}" if got is not None else ""))
    print(f"ok  {what}")


def ref(page, name):
    found = [e["ref"] for e in page["elements"] if e["name"] == name]
    if not found:
        sys.exit(f"no element named {name!r} in {[e['name'] for e in page['elements']]}")
    return str(found[0])


def outside_saw():
    try:
        with open(LOG) as f:
            return sorted(set(line.strip() for line in f))
    except FileNotFoundError:
        return []


# ---- opening, and only local addresses ----
r = web("open", "https://example.com/", ok=False)
check("an outside address is refused by open", not r["ok"] and "not this machine" in r["error"], r)
r = web("open", "http://localhost@example.com/", ok=False)
check("a host hidden behind user-info is refused", not r["ok"], r)
r = web("open", str(int(PORT) + 7), ok=False)
check("a port nothing listens on says so", not r["ok"] and "nothing is listening" in r["error"], r)
check("no tab is kept for a page that never came", web("list")["tabs"] == [], web("list"))

r = web("open", PORT)
check("the app opens and its tab is named after it", r["name"] == "Orders" and r["tab"] == 0, r)
page = web("read")
check("read gives the text with numbered elements", "# Orders" in page["text"] and f"[{ref(page, 'Place order')} Place order]" in page["text"], page["text"])
names = [e["name"] for e in page["elements"]]
check("controls are named: label, aria-label, row text, icon title, icon class, shadow DOM",
      all(n in names for n in ["Name", "Size", "Rush order", "Refresh list", "trash can", "Shadow button"]), names)
check("a control alone in a busy box is not named after the whole box", all(len(n) < 30 for n in names), names)

# ---- acting ----
web("fill", ref(page, "Name"), "Tal", "Grutman")
web("fill", ref(page, "Size"), "Large")
web("click", ref(page, "Rush order"))
web("click", ref(page, "Place order"))
web("wait", "Ordered for")
page = web("read")
check("fill then click in a row keeps the typed value (the page takes it 350 ms after the last key)",
      "Ordered for Tal Grutman size l rush" in page["text"], page["text"])
check("the page's console error and failed request are reported",
      any("order log failed" in c["text"] for c in page["console"]) and any("/api/missing" in f["url"] for f in page["failed"]), [page["console"], page["failed"]])
values = {e["name"]: e for e in page["elements"]}
check("values and ticks are read back", values["Name"]["value"] == "Tal Grutman" and values["Size"]["value"] == "l" and values["Rush order"]["checked"] is True, values)
web("select", ref(page, "Size"), "Small")
check("select chooses by text", {e["name"]: e for e in web("read")["elements"]}["Size"]["value"] == "s")
r = web("select", ref(page, "Size"), "Huge", ok=False)
check("an option that is not there says which are", "Small | Large" in r.get("error", ""), r)
r = web("click", ref(page, "Shadow button"))
check("a button inside a shadow root is clicked", r["title"] == "shadow clicked", r)
web("click", ref(page, "Name"))
web("type", "x")
web("key", "backspace")
r = web("wait", "nothing like this", "--secs", "0.3", ok=False)
check("wait gives up after its time", not r["ok"] and "not there" in r["error"], r)
r = web("click", "999", ok=False)
check("an element that does not exist says to read again", "read" in r.get("error", ""), r)

# ---- dialogs ----
page = web("read")
r = web("click", ref(page, "ask"))
check("a dialog is reported by what opened it", r.get("dialog", {}).get("message") == "Really?", r)
r = web("click", ref(page, "Place order"), ok=False)
check("nothing else is done while a dialog waits", "dialog" in r.get("error", ""), r)
check("read shows the waiting dialog", web("read").get("dialog", {}).get("type") == "confirm")
web("dialog", "dismiss")
check("the answer reaches the page", "declined" in web("read")["text"])

# ---- a file ----
with tempfile.NamedTemporaryFile(suffix=".txt", delete=False) as f:
    f.write(b"hello")
page = web("read")
web("upload", [e for e in page["elements"] if e["role"] == "file"][0]["ref"].__str__(), f.name)
check("a file input gets its file", "file " + os.path.basename(f.name) in web("read")["text"])
os.unlink(f.name)

# ---- nothing outside is ever shown ----
before = outside_saw()
check("what a page uses from outside still loads (its image)", "/pixel.png" in before, before)
for name, why in [("elsewhere", "a link"), ("hop", "a local link that redirects out"), ("script", "a script setting location"),
                  ("open outside", "window.open"), ("new window", "target=_blank")]:
    page = web("read")
    r = web("click", ref(page, name))
    time.sleep(0.4)
    now = web("read")
    check(f"{why} to outside: the tab stays, and says what was not opened",
          now["url"].startswith(f"http://127.0.0.1:{PORT}/") and OUT in (now.get("blocked") or ""), [r, now.get("url"), now.get("blocked")])
check("only one tab exists after all that", len(web("list")["tabs"]) == 1, web("list"))
page = web("read")
web("click", ref(page, "fetch"))
time.sleep(0.3)
seen = outside_saw()
check("the outside server was asked for the image and the fetch, and for no page",
      seen == ["/pixel.png", "/via-fetch"], seen)

# ---- a window a page opens on a local address becomes a tab ----
web("click", ref(web("read"), "open inside"))
time.sleep(0.6)
tabs = web("list")["tabs"]
check("a local window.open stays in the same tab set, as the app's tab", len(tabs) == 1 and tabs[0]["url"].endswith("/second"), tabs)
web("back")
web("wait", "Orders")

# ---- frames ----
web("click", ref(web("read"), "frames"))
web("wait", "Inner frame text")
page = web("read")
check("a local frame's text and elements are part of the page", "Inner frame text" in page["text"] and any(e.get("frame") for e in page["elements"]), page["text"])
web("click", ref(page, "Inner button"))
check("an element inside a frame is clicked", "inner clicked" in web("read")["text"])
seen = outside_saw()
check("outside frames, also nested ones, were never requested", not any("frame" in s for s in seen), seen)

# ---- pictures, tabs ----
with tempfile.TemporaryDirectory() as d:
    shot = web("shot", os.path.join(d, "page.png"))
    with open(shot["path"], "rb") as f:
        head = f.read(8)
    check("shot writes a PNG of the page", head == b"\x89PNG\r\n\x1a\n" and shot["width"] > 0, shot)
state = json.loads(subprocess.run([NIB, "ctl", "-p", PID, "state"], capture_output=True, text=True).stdout)["web"]
check("state lists the tab", state["running"] and state["tabs"][0]["name"] == "Framed", state)
web("close")
state = json.loads(subprocess.run([NIB, "ctl", "-p", PID, "state"], capture_output=True, text=True).stdout)["web"]
check("closing the last tab stops the browser", not state["running"] and state["tabs"] == [], state)
print("all web checks passed")
