#!/usr/bin/env python3
"""The pages scripts/test/e2e.sh shows in a web tab.

  site.py inside PORT OUTSIDE_URL   a small local app on 127.0.0.1:PORT
  site.py outside HOST PORT LOG     "another machine": every request it gets is written to LOG

Standard library only.
"""
import http.server
import sys

INSIDE = """<!doctype html><title>Orders - Test App</title>
<h1>Orders</h1>
<p>Type a name and press the button.</p>
<label>Name <input id="name" placeholder="your name"></label>
<select id="size" aria-label="Size"><option value="s">Small</option><option value="l">Large</option></select>
<table><tr><td><input type="checkbox" id="rush"></td><td>Rush order</td></tr></table>
<button id="go">Place order</button>
<button id="icon"><svg width="12" height="12"><title>Refresh list</title></svg></button>
<button id="fa"><i class="fa-solid fa-trash-can"></i></button>
<div id="out"></div>
<x-card></x-card>
<p><a id="away" href="OUT/via-link">elsewhere</a> <a id="hop" href="/hop">hop</a>
<a id="blank" href="OUT/via-blank" target="_blank">new window</a>
<button id="js" onclick="location.href='OUT/via-script'">script</button>
<button id="pop" onclick="window.open('OUT/via-open')">open outside</button>
<button id="popin" onclick="window.open('/second')">open inside</button>
<button id="ask" onclick="document.getElementById('out').textContent = confirm('Really?') ? 'confirmed' : 'declined'">ask</button>
<button id="call" onclick="fetch('OUT/via-fetch', {mode: 'no-cors'}).catch(() => {})">fetch</button>
<input type="file" id="file" onchange="document.getElementById('out').textContent = 'file ' + this.files[0].name">
<a id="frames" href="/framed">frames</a></p>
<img src="OUT/pixel.png" width="1" height="1">
<script>
customElements.define('x-card', class extends HTMLElement {
  constructor() { super(); const r = this.attachShadow({mode: 'open'}); r.innerHTML = '<p>Inside shadow</p><button>Shadow button</button>'; r.querySelector('button').onclick = () => { document.title = 'shadow clicked'; }; }
});
// The value is taken 350 ms after the last key, as Swagger UI does.
let typed = '', timer;
document.getElementById('name').addEventListener('input', (e) => { clearTimeout(timer); timer = setTimeout(() => { typed = e.target.value; }, 350); });
document.getElementById('go').onclick = async () => {
  document.getElementById('out').textContent = 'Ordered for ' + typed + ' size ' + document.getElementById('size').value + (document.getElementById('rush').checked ? ' rush' : '');
  console.error('order log failed');
  await fetch('/api/missing');
};
</script>
"""

FRAMED = """<!doctype html><title>Framed</title><h1>Frames</h1>
<iframe id="in" src="/inner" width="400" height="120"></iframe>
<iframe id="out" src="OUT/via-frame" width="400" height="60"></iframe>
"""

INNER = """<!doctype html><title>Inner</title><p>Inner frame text</p>
<button onclick="this.textContent = 'inner clicked'">Inner button</button>
<iframe src="OUT/via-nested-frame" width="100" height="30"></iframe>
"""


def inside(port, out):
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            path = self.path.split("?")[0]
            if path == "/hop":
                self.send_response(302)
                self.send_header("Location", out + "/via-redirect")
                self.end_headers()
                return
            pages = {"/": INSIDE, "/framed": FRAMED, "/inner": INNER, "/second": "<!doctype html><title>Second</title><p>Second page</p>"}
            body = pages.get(path)
            self.send_response(200 if body is not None else 404)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write((body or "missing").replace("OUT", out).encode())

    http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


def outside(host, port, log):
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            with open(log, "a") as f:
                f.write(self.path + "\n")
            self.send_response(200)
            self.send_header("Content-Type", "image/png" if self.path.endswith(".png") else "text/html")
            self.end_headers()
            self.wfile.write(b"<title>OUTSIDE</title>outside page")

    http.server.ThreadingHTTPServer((host, port), Handler).serve_forever()


if __name__ == "__main__":
    if sys.argv[1] == "inside":
        inside(int(sys.argv[2]), sys.argv[3])
    else:
        outside(sys.argv[2], int(sys.argv[3]), sys.argv[4])
