#!/usr/bin/env bash
# GUI end-to-end smoke test: runs the real `nib`, drives it through its control socket
# (`nib ctl`), and checks the files on disk. macOS: runs in a normal window. Linux: under Xvfb on
# the CPU renderer (apt install xvfb). Optional ssh leg via NIB_E2E_SSH_HOST.
#   cargo build --release && scripts/test/e2e.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
NIB=${NIB:-target/release/nib}   # also built by `scripts/build/build.sh client` (host target)
TMP=$(mktemp -d)
XPID=
KIDS=
trap '[ -n "$XPID" ] && kill $XPID 2>/dev/null; kill $KIDS $(cat "$TMP"/fssh/mux-* 2>/dev/null) 2>/dev/null || true; rm -rf "$TMP"' EXIT
export NIB_CONFIG="$TMP/config.toml"
# The agent legs (not in CI) start a real agent: Claude Code, with the user's own sign-in. What
# nib keeps of it (its conversation, what was approved) stays in $TMP.
export XDG_DATA_HOME="$TMP/data"
AI_CONFIG=$(printf '[ai]\nagent = "claude"\npick_model = false\n%s' "${NIB_E2E_MODEL:+model = \"$NIB_E2E_MODEL\"}")
printf '%s\n' "$AI_CONFIG" > "$NIB_CONFIG"

if [ "$(uname)" = Linux ] && [ -z "${NIB_E2E_DISPLAY:-}" ]; then
  Xvfb :77 -screen 0 1280x900x24 >/dev/null 2>&1 & XPID=$!
  export DISPLAY=:77 ICED_BACKEND=tiny-skia
  sleep 1
fi

ctl() { "$NIB" ctl -s "$SOCK" "$@" >/dev/null; }

# $@ = nib arguments, with $TEXT typed at the end of the file, saved, then quit.
run_edit() {
  SOCK="$TMP/ctl-$RANDOM.sock"
  NIB_CONTROL="$SOCK" "$NIB" "$@" &
  local pid=$!
  for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
  [ -S "$SOCK" ] || { echo "FAIL: nib never opened its control socket"; exit 1; }
  ctl wait 20 || { echo "FAIL: file never opened"; exit 1; }
  ctl run move_doc_end
  ctl type "$TEXT"
  ctl run save
  ctl wait 20 || { echo "FAIL: save never finished"; exit 1; }
  ctl run quit
  wait $pid
}

# NIB_E2E_ONLY=web,forward runs only those legs (local, headless, apps, web, forward, bench).
# Every test web server: python3 -m http.server without its slow reverse DNS lookup (see serve.py).
SERVE="$PWD/scripts/test/e2e-web/serve.py"
want() { [ -z "${NIB_E2E_ONLY:-}" ] || [[ ",$NIB_E2E_ONLY," == *",$1,"* ]]; }
answers() { python3 -c "import socket, sys; socket.create_connection(('127.0.0.1', int(sys.argv[1])), 1)" "$1" 2>/dev/null; }
# On a failure: the servers' terminals, so the log says why (a command not found, a slow start).
show_srv() { for s in one two; do echo "--- pair/$s:"; "$NIB" ctl -s "$SOCK" app log pair "$s" --lines 15 2>&1 | tail -15; done; }
srv() { "$NIB" ctl -s "$SOCK" app list | python3 -c "import json, sys; print(' '.join(a['name'] + '/' + s['name'] + '=' + s['state'] for a in json.load(sys.stdin)['apps'] for s in a['servers']))"; }
closed() { for _ in $(seq "${2:-50}"); do answers "$1" || return 0; sleep 0.1; done; return 1; }   # $2: tries, 0.1 s apart
# Apps the user approved, as their own click in the Apps panel leaves them (nib ctl defines apps,
# but never approves one): $1 the window's socket, $2 {name: definition}. The definitions go in
# NIBDIR's apps.json; what was approved goes in the daemon's own folder for the project
# (nibd/src/places.rs: <data>/nib/private/<folder>-<first 8 bytes of the SHA-256 of the root>),
# which no agent is given to write, as { "approved": { name: hash } }. The hash is the daemon's
# (serve::hash, pinned by its test): SHA-256 of the definition as compact JSON, fields in their
# order, maps by key, nothing empty.
approved_apps() {
  "$NIB" ctl -s "$1" state | python3 -c '
import hashlib, json, os, re, sys
state, apps = json.load(sys.stdin), json.loads(sys.argv[1])
root, nibdir = state["root"], state["roots"]["nibdir"]
defs, approved = {}, {}
for name, d in apps.items():
    servers = {s: {k: v for k, v in (("cmd", x["cmd"]), ("port", x.get("port")), ("after", x.get("after") or None)) if v is not None} for s, x in sorted(d["servers"].items())}
    canon = {k: v for k, v in (("title", d.get("title")), ("page", d.get("page")), ("servers", servers)) if v is not None}
    defs[name] = canon
    approved[name] = hashlib.sha256(json.dumps(canon, separators=(",", ":"), ensure_ascii=False).encode()).hexdigest()
json.dump({"apps": defs}, open(nibdir + "/apps.json", "w"))
if nibdir.endswith(".nibdir"):  # a project in a temp folder: all three are next to its scratch
    private = nibdir[: -len(".nibdir")] + ".private"
else:
    data = os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
    slug = re.sub(r"[^\w.-]", "_", os.path.basename(root.rstrip("/")) or "root")
    private = os.path.join(data, "nib", "private", slug + "-" + hashlib.sha256(root.encode()).hexdigest()[:16])
os.makedirs(private, mode=0o700, exist_ok=True)
json.dump({"approved": approved}, open(private + "/approvals.json", "w"))
' "$2"
}
if want local; then
echo "e2e: local"
printf 'import os\n' > "$TMP/a.py"
TEXT='x = 1' run_edit "$TMP/a.py"
[ "$(cat "$TMP/a.py")" = $'import os\nx = 1' ] || { echo "FAIL: local content: $(cat "$TMP/a.py")"; exit 1; }
echo "  ok"

fi  # local
if want headless; then
echo "e2e: headless"
printf 'import os\n' > "$TMP/h.py"
SOCK="$TMP/h.sock"
NIB_CONTROL="$SOCK" "$NIB" --headless "$TMP/h.py" > "$TMP/h.out" &
HPID=$!
for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
ctl wait 20
ctl run move_doc_end; ctl type 'y = 2'; ctl run save; ctl wait 20
ctl run new_terminal; ctl wait 20
ctl type 'echo $((6*7))\n'
for _ in $(seq 50); do "$NIB" ctl -s "$SOCK" state | grep -qF '\n42\n' && break; sleep 0.1; done
"$NIB" ctl -s "$SOCK" state | grep -qF '\n42\n' || { echo "FAIL: headless terminal"; exit 1; }
ctl run quit; wait $HPID
[ "$(cat "$TMP/h.py")" = $'import os\ny = 2' ] || { echo "FAIL: headless content: $(cat "$TMP/h.py")"; exit 1; }
echo "  ok"

fi  # headless
# The project's apps: defined, started in order, read, stopped; a port something else holds
# is left alone; what nib ctl defines waits for the user; nothing is left listening once the
# window is gone.
if want apps; then
echo "e2e: apps"
mkdir -p "$TMP/srv"; echo hello > "$TMP/srv/index.html"
read -r P1 P2 P3 <<< "$(python3 -c "
import socket
s = [socket.socket() for _ in range(3)]
for x in s: x.bind(('127.0.0.1', 0))
print(*[x.getsockname()[1] for x in s])")"
SOCK="$TMP/s.sock"
NIB_CONTROL="$SOCK" "$NIB" --headless "$TMP/srv" > "$TMP/s.out" &
SPID=$!
KIDS="$KIDS $SPID"
for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
ctl wait 20
python3 $SERVE "$P3" --bind 127.0.0.1 >/dev/null 2>&1 & HOLD=$!
KIDS="$KIDS $HOLD"
for _ in $(seq 50); do answers "$P3" && break; sleep 0.1; done
approved_apps "$SOCK" "{
  \"pair\": { \"servers\": { \"one\": { \"cmd\": \"python3 $SERVE $P1 --bind 127.0.0.1\", \"port\": $P1 },
                           \"two\": { \"cmd\": \"python3 $SERVE $P2 --bind 127.0.0.1\", \"port\": $P2, \"after\": [\"one\"] } } },
  \"held\": { \"servers\": { \"held\": { \"cmd\": \"python3 $SERVE $P3 --bind 127.0.0.1\", \"port\": $P3 } } } }"
echo '{ "servers": { "s": { "cmd": "true", "after": ["nobody"] } } }' | { ! "$NIB" ctl -s "$SOCK" app set bad --json - >/dev/null 2>&1; } || { echo "FAIL: a server after one the app does not have was accepted"; exit 1; }
# What nib ctl defines waits for the user: listed, not started, not approvable from here.
echo "{ \"servers\": { \"s\": { \"cmd\": \"python3 $SERVE $P3 --bind 127.0.0.1\", \"port\": $P3 } } }" | ctl app set mine --json -
! "$NIB" ctl -s "$SOCK" app up mine >/dev/null 2>&1 || { echo "FAIL: nib ctl started an app the user did not approve"; exit 1; }
! "$NIB" ctl -s "$SOCK" raw '{"cmd":"app.approve","name":"mine","hash":"x"}' 2>/dev/null | grep -q '"ok": *true' || { echo "FAIL: nib ctl approved an app"; exit 1; }
ctl app rm mine
[ "$(srv)" = "held/held=stopped pair/one=stopped pair/two=stopped" ] || { echo "FAIL: apps defined: $(srv)"; exit 1; }
ctl app up
for _ in $(seq 150); do [ "$(srv)" = "held/held=port-taken pair/one=ready pair/two=ready" ] && break; sleep 0.1; done
[ "$(srv)" = "held/held=port-taken pair/one=ready pair/two=ready" ] || { echo "FAIL: apps up: $(srv)"; show_srv; exit 1; }
answers "$P1" && answers "$P2" || { echo "FAIL: a ready server's port does not answer"; exit 1; }
python3 -c "import sys, urllib.request; sys.exit(0 if b'hello' in urllib.request.urlopen('http://127.0.0.1:$P1/index.html').read() else 1)" || { echo "FAIL: server one does not serve the project"; exit 1; }
for _ in $(seq 50); do "$NIB" ctl -s "$SOCK" app log pair one | grep -qF 'GET /index.html' && break; sleep 0.1; done
"$NIB" ctl -s "$SOCK" app log pair one --lines 5 | grep -qF 'GET /index.html' || { echo "FAIL: app log"; exit 1; }
ctl app restart pair one
for _ in $(seq 150); do [ "$(srv)" = "held/held=port-taken pair/one=ready pair/two=ready" ] && break; sleep 0.1; done
[ "$(srv)" = "held/held=port-taken pair/one=ready pair/two=ready" ] || { echo "FAIL: server restarted: $(srv)"; show_srv; exit 1; }
ctl app down
[ "$(srv)" = "held/held=stopped pair/one=stopped pair/two=stopped" ] || { echo "FAIL: apps down: $(srv)"; exit 1; }
closed "$P1" && closed "$P2" || { echo "FAIL: a stopped server still listens"; exit 1; }
answers "$P3" || { echo "FAIL: stopping the apps stopped what held the taken port"; exit 1; }
# Running again when the window quits: they go with it.
ctl app up pair
for _ in $(seq 150); do [ "$(srv)" = "held/held=stopped pair/one=ready pair/two=ready" ] && break; sleep 0.1; done
[ "$(srv)" = "held/held=stopped pair/one=ready pair/two=ready" ] || { echo "FAIL: apps up again: $(srv)"; show_srv; exit 1; }
ctl run quit; wait $SPID
closed "$P1" && closed "$P2" || { echo "FAIL: a server outlived the window"; exit 1; }
! pgrep -f -- "serve.py ($P1|$P2) " >/dev/null || { echo "FAIL: a server process outlived the window"; exit 1; }
kill $HOLD 2>/dev/null; wait $HOLD 2>/dev/null || true
echo "  ok"

fi  # apps
# The web tab's engine and the agents' page requests, against scripts/test/e2e-web/site.py: a small
# local app, and a stand-in for "another machine" on this one's network address, which must
# never be asked for a page. Runs with the browser nib would use; NIB_E2E_BROWSERS (paths,
# separated by ":") runs it once for each of those instead.
# What only an agent can do, done by one. nibd answers `nib-web` (and `nib-app`) only from an
# agent it started itself: each gets a token from the daemon that runs it, kept in that daemon's
# memory, and leaves its requests in a folder of its own (crates/nibd/src/tokens.rs, mail.rs).
# A script has neither, and a token from another nib (this test run from inside an agent, say)
# is nothing to this test's daemon. So the test starts a real agent in its own window and asks
# it to run the checks: a Python file, which writes what it found beside itself (FILE.out,
# ending in a line PASS or FAIL: …). The checks are the file's; the model only starts it.
# Spends model tokens, so not in CI.  $1 the window's socket, $2 the file, $3 what it checks.
as_agent() {
  local sock=$1 file=$2 out="$2.out"
  command -v claude >/dev/null || { echo "  $3: skipped, Claude Code (claude) is not installed"; return 0; }
  rm -f "$out"
  "$NIB" ctl -s "$sock" wait 20 >/dev/null
  "$NIB" ctl -s "$sock" run toggle_ai >/dev/null
  "$NIB" ctl -s "$sock" type "Run this one command, then tell me its last line of output: python3 $file    (It is one of this project's test scripts: it calls the nib-web command a few times and writes what it found to $out. Some calls are meant to be refused. Change no files and do not use nib-work.)" >/dev/null
  "$NIB" ctl -s "$sock" key enter >/dev/null
  for _ in $(seq "${NIB_E2E_AGENT_SECS:-300}"); do grep -qE '^(PASS|FAIL)' "$out" 2>/dev/null && break; sleep 1; done
  if ! grep -q '^PASS' "$out" 2>/dev/null; then
    echo "--- $out:"; cat "$out" 2>/dev/null || echo "(not written: the agent did not run it)"
    echo "--- the agent:"; "$NIB" ctl -s "$sock" state | python3 -c 'import json, sys; a = json.load(sys.stdin)["ai"]; print(json.dumps({k: a.get(k) for k in ("mode", "busy", "ask", "error")})); [print(" ", (t.get("text") or "")[:300]) for t in (a.get("turns") or [])[-4:]]' 2>/dev/null || true
    echo "FAIL: $3"; exit 1
  fi
  echo "  ok ($3)"
}
# The start of a checks file for `as_agent`: `web(...)` runs nib-web as the agent has it on its
# PATH, `check(what, ok, got)` ends the file with FAIL at the first thing that is not so, and
# `done()` ends it with PASS.
AGENT_PRELUDE='
import json, os, subprocess, sys
OUT = open(__file__ + ".out", "w")
def say(line):
    OUT.write(line + "\n"); OUT.flush(); print(line)
def web(*args):
    r = subprocess.run(["nib-web", *args], capture_output=True, text=True, timeout=120)
    try:
        return r.returncode, json.loads(r.stdout)
    except ValueError:
        return r.returncode, {"stdout": r.stdout, "stderr": r.stderr}
def check(what, ok, got):
    if not ok:
        say(f"FAIL: {what}: {got}"); sys.exit(1)
    say(f"ok {what}")
def done():
    say("PASS")
'
# An agent's way to the page: `nib-web`, through the daemon to the window and back. Less than
# the socket: no eval, no pointer, no file written where the window is, no other machine.
agent_leg() {
  { echo "$AGENT_PRELUDE"; echo "port, tmp = $(printf '%s' "$WEB_PORT" | python3 -c 'import json, sys; print(json.dumps(sys.stdin.read()))'), $(printf '%s' "$TMP" | python3 -c 'import json, sys; print(json.dumps(sys.stdin.read()))')"; cat <<'PY'; } > "$TMP/agent_web.py"
scratch = os.environ["NIB_SCRATCH"]
check("the agent has a token and a folder of its own", os.environ.get("NIB_TOKEN") and os.path.isdir(os.path.join(os.environ["NIB_MAILBOX"], "agents", os.environ["NIB_AGENT"], "web")), sorted(k for k in os.environ if k.startswith("NIB")))
code, v = web("open", port)
check("open", code == 0 and v.get("ok") and f":{port}/" in v.get("url", ""), v)
code, v = web("read")
check("read", code == 0 and v.get("ok") and isinstance(v.get("text"), str) and v["text"].strip() and isinstance(v.get("elements"), list), v)
code, v = web("list")
check("list", code == 0 and any(f":{port}/" in t["url"] for t in v.get("tabs", [])), v)
code, v = web("wait", "--secs", "5")
check("wait", code == 0 and v.get("ok"), v)
code, v = web("shot")
png = open(v.get("path", "/nonexistent"), "rb").read() if code == 0 and os.path.isfile(v.get("path", "")) else b""
check("shot goes to the agent's scratch", v.get("ok") and v["path"] == os.path.join(scratch, "web-001.png") and png[:8] == b"\x89PNG\r\n\x1a\n" and v["width"] > 0 and v["height"] > 0 and "png" not in v, {k: v[k] for k in v if k != "png"})
code, v = web("shot", os.path.join(tmp, "agent.png"), "--full")
check("shot FILE", v.get("ok") and open(os.path.join(tmp, "agent.png"), "rb").read()[:4] == b"\x89PNG", v)
for refused in (["eval", "document.title"], ["pointer", "5", "5"], ["quit"]):
    code, v = web(*refused)
    check(f"{refused[0]} is refused", code == 1 and v.get("ok") is False and "not for agents" in v.get("error", ""), v)
code, v = web("click", "99999")
check("the window's refusal is the answer", code == 1 and v.get("ok") is False and v.get("error"), v)
code, v = web("open", "http://example.com/")
check("an outside address is refused", code == 1 and "not this machine" in v.get("error", ""), v)
code, v = web("open", "gpu:8888")
check("another saved connection is refused", code == 1 and "saved connections" in v.get("error", ""), v)
code, v = web("upload", "1", os.path.join(tmp, "no-such-file"))
check("upload of a file that is not there", code == 1 and "is not a file" in v.get("error", ""), v)
code, v = web("dance")
check("usage for what is no request", code == 2, v)
# A request written by hand, with the agent's own token: a picture with a file's name on the
# window's machine comes back as bytes, and no such file is written.
import time
own = os.path.join(os.environ["NIB_MAILBOX"], "agents", os.environ["NIB_AGENT"], "web")
planted = os.path.join(tmp, "planted-by-agent.png")
name = "%032d-raw.json" % time.time_ns()
with open(os.path.join(own, "req", ".raw.tmp"), "w") as f:
    json.dump({"cmd": "web.shot", "path": planted, "token": os.environ["NIB_TOKEN"]}, f)
os.rename(os.path.join(own, "req", ".raw.tmp"), os.path.join(own, "req", name))
for _ in range(200):
    if os.path.isfile(os.path.join(own, "res", name)): break
    time.sleep(0.1)
v = json.load(open(os.path.join(own, "res", name))) if os.path.isfile(os.path.join(own, "res", name)) else {}
check("a raw shot request writes no file where the window is", v.get("ok") and "png" in v and "path" not in v and not os.path.exists(planted), {k: v[k] for k in v if k != "png"})
done()
PY
  as_agent "$SOCK" "$TMP/agent_web.py" "nib-web: an agent's requests through the daemon"
}
web_leg() {
  SOCK="$TMP/w.sock"; : > "$TMP/outside.log"
  NIB_CONTROL="$SOCK" "$NIB" --headless "$TMP" > "$TMP/w.out" &
  local pid=$!
  KIDS="$KIDS $pid"
  for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
  if "$NIB" ctl -s "$SOCK" state | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["web"]["browser"] else 1)'; then
    python3 scripts/test/e2e-web/check.py "$NIB" "$pid" "$WEB_PORT" "http://$OUTSIDE:$((WEB_PORT + 1))" "$TMP/outside.log" > "$TMP/web.log" 2>&1 \
      || { tail -5 "$TMP/web.log"; window_log "$TMP/w.out"; echo "FAIL: web"; exit 1; }
    echo "  ok ($(grep -c '^ok' "$TMP/web.log") checks)"
    # A page whose server has stopped is not shown from the browser's cache: its file is old, so
    # a cache would keep the page for long, and the second open must find nothing listening.
    mkdir -p "$TMP/stale"; echo '<title>Stale</title>an old page' > "$TMP/stale/index.html"; touch -t 202001010000 "$TMP/stale/index.html"
    SP=$(python3 -c "import socket; s = socket.socket(); s.bind(('127.0.0.1', 0)); print(s.getsockname()[1])")
    python3 $SERVE "$SP" --bind 127.0.0.1 --directory "$TMP/stale" >/dev/null 2>&1 & STALE=$!
    KIDS="$KIDS $STALE"
    for _ in $(seq 50); do answers "$SP" && break; sleep 0.1; done
    st=$("$NIB" ctl -s "$SOCK" web open "$SP" | python3 -c "import json, sys; v = json.load(sys.stdin); print(v.get('tab', '') if v.get('title') == 'Stale' else '')")
    [ -n "$st" ] || { echo "FAIL: the stale-page check's page did not open"; exit 1; }
    "$NIB" ctl -s "$SOCK" web close --tab "$st" >/dev/null
    kill $STALE; closed "$SP" || { echo "FAIL: the stale-page check's server did not stop"; exit 1; }
    "$NIB" ctl -s "$SOCK" web open "$SP" > "$TMP/stale.out" 2>&1 || true
    grep -qF "nothing is listening" "$TMP/stale.out" || { cat "$TMP/stale.out"; echo "FAIL: a page whose server stopped was shown from the browser's cache"; exit 1; }
    echo "  ok (a stopped server's page is not shown from the cache)"
    # Done by a real agent (`as_agent`), which spends model tokens: not in CI.
    if [ -n "${CI:-}" ]; then
      echo "  nib-web (an agent's way): skipped in CI, needs a real agent"
    else
      agent_leg
    fi
  else
    echo "  skipped: no browser nib can drive is installed"
  fi
  ctl run quit; wait $pid
  # Nothing of it may outlive the window.
  sleep 1
  ! pgrep -f -- "$TMP_WEB_MARK" >/dev/null || { echo "FAIL: a browser was left running"; exit 1; }
}
# On a failure: the end of a window's own log (its first output line names its run directory),
# which has its requests and every slow operation, and of its daemon's.
window_log() {
  local rd nibd
  rd=$(head -1 "$1" | python3 -c "import json, sys; print(json.load(sys.stdin)['run_dir'])" 2>/dev/null) || return 0
  echo "--- nib.log:"; tail -40 "$rd/nib.log" 2>/dev/null
  nibd=$(head -1 "$1" | python3 -c "import json, sys; print(json.load(sys.stdin)['logs']['nibd'])" 2>/dev/null) || return 0
  echo "--- nibd log:"; tail -20 "$nibd" 2>/dev/null
}
# On a forward failure: what listens on port $1 (and which process), what it answers, the
# window's forwards, the fake ssh's log and the window's own log.
forward_diag() {
  echo "--- listening on $1:"; lsof -nP -iTCP:"$1" -sTCP:LISTEN 2>/dev/null || echo "(nothing, or no lsof)"
  echo "--- curl http://127.0.0.1:$1/:"; curl -s -m 5 -D - -o /dev/null "http://127.0.0.1:$1/" || echo "(curl exit $?)"
  echo "--- forward list:"; "$NIB" ctl -s "$SOCK" forward list 2>&1 | head -20
  echo "--- fake ssh log:"; tail -30 "$TMP/fssh/log" 2>/dev/null
  window_log "$TMP/f.out"
}
if want web; then
echo "e2e: web"
# "Another machine": NIB_E2E_OUTSIDE (a name or address that reaches this machine but is not
# loopback by name, e.g. outside.test in /etc/hosts), else this machine's network address.
OUTSIDE=${NIB_E2E_OUTSIDE:-}
[ -n "$OUTSIDE" ] || OUTSIDE=$(python3 -c "import socket; s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.connect(('192.0.2.1', 9)); print(s.getsockname()[0])" 2>/dev/null || true)
if [ -z "$OUTSIDE" ] || [ "${OUTSIDE#127.}" != "$OUTSIDE" ]; then
  echo "  skipped: this machine has no network address to stand in for another machine"
else
  WEB_PORT=${NIB_E2E_WEB_PORT:-8971}
  export XDG_CACHE_HOME="$TMP/cache"; TMP_WEB_MARK="$TMP/cache/nib/web"
  python3 scripts/test/e2e-web/site.py inside "$WEB_PORT" "http://$OUTSIDE:$((WEB_PORT + 1))" & SITE1=$!
  python3 scripts/test/e2e-web/site.py outside "$OUTSIDE" "$((WEB_PORT + 1))" "$TMP/outside.log" & SITE2=$!
  KIDS="$KIDS $SITE1 $SITE2"
  sleep 0.5
  # The outside site must answer this machine at its address, or a page whose image never comes
  # never finishes loading. Refused or no answer yet (macOS leaves a connection to a port that is
  # bound but not listening yet unanswered): try again, for up to 30 s.
  if ! python3 - "$OUTSIDE" "$((WEB_PORT + 1))" <<'PY'
import socket, sys, time
until, last = time.time() + 30, None
while time.time() < until:
    try:
        socket.create_connection((sys.argv[1], int(sys.argv[2])), 2).close()
        sys.exit(0)
    except OSError as e:
        last = e
        time.sleep(0.5)
sys.exit(f"  {sys.argv[1]}:{sys.argv[2]}: no answer in 30 s ({last!r})")
PY
  then
    # Asked for by name: the web leg's checks (the local-only rule) must run, not be skipped.
    if [ -n "${NIB_E2E_OUTSIDE:-}" ]; then
      op=$((WEB_PORT + 1))
      echo "--- $OUTSIDE resolves to:"; python3 -c "import socket, sys; print(sorted({a[4][0] for a in socket.getaddrinfo(sys.argv[1], int(sys.argv[2]), proto=socket.IPPROTO_TCP)}))" "$OUTSIDE" "$op" 2>&1
      echo "--- every process on $op:"; sudo -n lsof -nP -iTCP:"$op" 2>&1 || lsof -nP -iTCP:"$op" 2>&1 || echo "(none)"
      echo "--- sockets on $op:"; netstat -anv -p tcp 2>/dev/null | grep -E "Proto|[.:]$op " || echo "(none)"
      echo "--- the test sites:"; ps -o pid,stat,etime,command -p "$SITE1,$SITE2" 2>&1
      for u in "http://127.0.0.1:$op/" "http://$OUTSIDE:$op/"; do curl -s -m 5 -o /dev/null -w "$u  %{http_code}  %{remote_ip}  %{time_total}s\n" "$u" || echo "$u  failed (curl exit $?)"; done
      echo "FAIL: NIB_E2E_OUTSIDE=$OUTSIDE does not answer this machine"; exit 1
    fi
    echo "  skipped: this machine's network address ($OUTSIDE) does not answer it, so it cannot stand in for another machine"
  elif [ -n "${NIB_E2E_BROWSERS:-}" ]; then
    IFS=: read -ra BROWSERS <<< "$NIB_E2E_BROWSERS"
    for b in "${BROWSERS[@]}"; do echo "  with $b"; NIB_BROWSER="$b" web_leg; done
  else
    web_leg
  fi
  kill $SITE1 $SITE2 2>/dev/null
  unset XDG_CACHE_HOME
fi

fi  # web
# Another machine's ports: a window "on another machine" (scripts/test/e2e-web/fake_ssh.py stands in
# for ssh, so the other machine is this one) forwards a port of it, shows a page through the
# forward, forwards a ready server's port, and leaves nothing behind. What ssh itself does is
# not checked here (NIB_E2E_SSH_HOST below does that, where there is a host).
if want forward; then
echo "e2e: forward"
mkdir -p "$TMP/far" "$TMP/fbin" "$TMP/fssh"; echo '<title>Far</title><h1>hello from afar</h1>' > "$TMP/far/index.html"
printf '#!/bin/sh\nexec python3 "%s/scripts/test/e2e-web/fake_ssh.py" "$@"\n' "$PWD" > "$TMP/fbin/ssh"; chmod +x "$TMP/fbin/ssh"
NIBD_BIN="$(cd "$(dirname "$NIB")" && pwd)/nibd"
printf '[profiles.far]\nssh = "far.test"\nroot = "%s"\nnibd = "%s"\n[profiles.gpu]\nssh = "gpu.test"\nroot = "~"\n' "$TMP/far" "$NIBD_BIN" > "$TMP/config.toml"
printf '%s\n' "$AI_CONFIG" >> "$TMP/config.toml"
# Two free ports far apart: the other machine is this one, so F1 is taken here and its forwards
# land on the next free ports after it (F1 + 1, F1 + 2). F2 must not be one of them, or asking for
# "the other machine's F2, where nothing listens" would find one of those forwards answering.
read -r F1 F2 <<< "$(python3 -c "
import socket
def free():
    s = socket.socket(); s.bind(('127.0.0.1', 0)); return s, s.getsockname()[1]
a, f1 = free()
while True:
    b, f2 = free()
    if abs(f2 - f1) > 50:
        print(f1, f2); break")"
python3 $SERVE "$F1" --bind 127.0.0.1 --directory "$TMP/far" >/dev/null 2>&1 & FAR=$!
KIDS="$KIDS $FAR"
for _ in $(seq 50); do answers "$F1" && break; sleep 0.1; done
SOCK="$TMP/f.sock"
FAKE_SSH_DIR="$TMP/fssh" FAKE_SSH_NO_MUX=gpu.test PATH="$TMP/fbin:$PATH" NIB_CONTROL="$SOCK" "$NIB" --headless @far > "$TMP/f.out" &
FPID=$!
KIDS="$KIDS $FPID"
for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
ctl wait 30 || { echo "FAIL: forward: never connected: $(tail -3 "$TMP/fssh/log" 2>/dev/null)"; exit 1; }
fwd() { "$NIB" ctl -s "$SOCK" "$@" | python3 -c "
import json, sys
v = json.load(sys.stdin)
if not v.get('ok'): sys.exit(v.get('error', 'failed'))
print(' '.join('%s:%s=%s' % (f['host'], f['remote'], f['local']) for f in v.get('forwards', [])), end='')"; }
get() { python3 -c "import sys, urllib.request; sys.exit(0 if b'hello from afar' in urllib.request.urlopen('http://127.0.0.1:' + sys.argv[1] + '/', timeout=5).read() else 1)" "$1" 2>/dev/null; }
[ -z "$(fwd forward list)" ] || { echo "FAIL: forwards before any was asked for"; exit 1; }
# The port is taken here (the other machine is this one): the next free one, and a warning.
GOT=$(fwd forward add "$F1"); L1=${GOT##*=}
[ "$GOT" = "@far:$F1=$L1" ] && [ "$L1" != "$F1" ] || { echo "FAIL: forward add: $GOT"; exit 1; }
get "$L1" || { echo "FAIL: the forwarded port does not answer with the far page"; exit 1; }
grep -qF -- "-O forward -L 127.0.0.1:$L1:localhost:$F1" "$TMP/fssh/log" || { echo "FAIL: not on the shared connection"; exit 1; }
"$NIB" ctl -s "$SOCK" state | grep -qF "port $F1 is taken on this machine" || { echo "FAIL: no warning that another port was used"; exit 1; }
[ "$(fwd forward add "$F1")" = "@far:$F1=$L1" ] || { echo "FAIL: the same port forwarded twice"; exit 1; }
[ -z "$(fwd forward rm "$L1")" ] && closed "$L1" || { echo "FAIL: forward rm"; exit 1; }
! ctl forward rm "$L1" 2>/dev/null || { echo "FAIL: removing a forward that is not there succeeded"; exit 1; }
! ctl forward add "nobody:$F1" 2>/dev/null || { echo "FAIL: a forward to a connection nobody saved"; exit 1; }
# A saved connection's port: an ssh of its own (it has no shared connection), killed to remove.
GOT=$(fwd forward add "gpu:$F1"); L2=${GOT##*=}
[ "$GOT" = "@gpu:$F1=$L2" ] && get "$L2" || { echo "FAIL: forward add PROFILE:PORT: $GOT"; exit 1; }
grep -qF -- "-N -L 127.0.0.1:$L2:localhost:$F1" "$TMP/fssh/log" || { echo "FAIL: no ssh of its own for another connection"; exit 1; }
[ -z "$(fwd forward rm "$L2")" ] && closed "$L2" || { echo "FAIL: forward rm (ssh of its own)"; exit 1; }
# A ready server's port comes here; stopped, it goes.
approved_apps "$SOCK" "{ \"far\": { \"page\": { \"server\": \"web\", \"path\": \"/\" }, \"servers\": { \"web\": { \"cmd\": \"python3 $SERVE $F2 --bind 127.0.0.1\", \"port\": $F2 } } } }"
ctl app up
for _ in $(seq 150); do GOT=$(fwd forward list); [ -n "$GOT" ] && break; sleep 0.1; done
L3=${GOT##*=}
[ "$GOT" = "@far:$F2=$L3" ] || { echo "FAIL: a ready server's port was not forwarded: $GOT"; exit 1; }
ctl app down
for _ in $(seq 50); do [ -z "$(fwd forward list)" ] && break; sleep 0.1; done
[ -z "$(fwd forward list)" ] && closed "$L3" || { echo "FAIL: a stopped server's forward stayed"; exit 1; }
# The server itself is gone too before its port is asked for again below.
closed "$F2" || { echo "FAIL: a stopped server still listens on $F2"; exit 1; }
# A page of the other machine, shown here through a forward made for it and gone with its tab.
if "$NIB" ctl -s "$SOCK" state | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["web"]["browser"] else 1)'; then
  export XDG_CACHE_HOME="$TMP/cache"
  L4=$("$NIB" ctl -s "$SOCK" web open "$F1" | python3 -c "
import json, sys
v = json.load(sys.stdin)
f = v.get('forwarded') or {}
ok = v.get('ok') and f.get('remote') == int(sys.argv[1]) and f.get('host') == '@far' and (':%d/' % f['local']) in v['url'] and v.get('title') == 'Far'
print(f['local'] if ok else 'FAIL %s' % v)" "$F1")
  [ "${L4#FAIL}" = "$L4" ] || { echo "FAIL: web open of the other machine's port: $L4"; exit 1; }
  "$NIB" ctl -s "$SOCK" web read | grep -qF 'hello from afar' || { echo "FAIL: the far page was not read"; exit 1; }
  [ "$(fwd forward list)" = "@far:$F1=$L4" ] || { echo "FAIL: the page's forward is not listed"; exit 1; }
  ctl web close
  for _ in $(seq 50); do [ -z "$(fwd forward list)" ] && break; sleep 0.1; done
  [ -z "$(fwd forward list)" ] || { echo "FAIL: a closed tab's forward stayed"; exit 1; }
  # An agent there (`nib-web`, run on the daemon's machine by a real agent, so not in CI) means
  # its own machine's port too; its files are not where the browser is.
  if [ -z "${CI:-}" ]; then
    { echo "$AGENT_PRELUDE"; echo "port, page = \"$F1\", \"$TMP/far/index.html\""; cat <<'PY'; } > "$TMP/far/agent_far.py"
code, v = web("open", port)
check("open on another machine is forwarded", code == 0 and (v.get("forwarded") or {}).get("host") == "@far", v)
code, v = web("read")
check("read on another machine", code == 0 and "hello from afar" in v.get("text", ""), v)
code, v = web("upload", "1", page)
check("upload from another machine is refused", "not supported yet" in json.dumps(v), v)
code, v = web("open", "far:" + port)
check("its own machine by its saved name is its own", code == 0 and (v.get("forwarded") or {}).get("host") == "@far", v)
code, v = web("open", "gpu:" + port)
check("another saved connection is refused", code == 1 and "saved connections" in v.get("error", ""), v)
web("close"); web("close")
done()
PY
    as_agent "$SOCK" "$TMP/far/agent_far.py" "nib-web on another machine: an agent's requests"
    for _ in $(seq 50); do [ -z "$(fwd forward list)" ] && break; sleep 0.1; done
    [ -z "$(fwd forward list)" ] || { echo "FAIL: the forward of a tab an agent closed stayed"; exit 1; }
  fi
  # By its saved name, with a path; and a port nothing listens on there says whose it is.
  "$NIB" ctl -s "$SOCK" web open "gpu:$F1/index.html" | grep -qF '"host": "@gpu"' || { echo "FAIL: web open PROFILE:PORT"; exit 1; }
  "$NIB" ctl -s "$SOCK" web read | grep -qF 'hello from afar' || { echo "FAIL: the page of a saved connection was not read"; exit 1; }
  "$NIB" ctl -s "$SOCK" web open "$F2" > "$TMP/f.unanswered" 2>&1 || true; grep -qF "@far's port $F2" "$TMP/f.unanswered" || { cat "$TMP/f.unanswered"; forward_diag "$F2"; echo "FAIL: an unanswered far port does not say whose it is"; exit 1; }
  [ "$(fwd forward list | tr ' ' '\n' | grep -c .)" = 1 ] || { echo "FAIL: a forward for a page that never came was kept: $(fwd forward list)"; exit 1; }
  unset XDG_CACHE_HOME
  echo "  ok"
else
  GOT=$(fwd forward add "gpu:$F1")
  echo "  ok (no browser nib can drive: the web tab's part skipped)"
fi
# Quitting takes every forward with it.
ctl run quit; wait $FPID
sleep 0.5
! pgrep -f -- "fake_ssh.py.*localhost:($F1|$F2)" >/dev/null || { echo "FAIL: a forward outlived the window: $(pgrep -fl fake_ssh.py | cut -c1-8)"; grep -v "exec sh" "$TMP/fssh/log" | tail -30; tail -5 "$TMP/f.out"; exit 1; }
kill $FAR 2>/dev/null; wait $FAR 2>/dev/null || true
printf '' > "$TMP/config.toml"

fi  # forward
if want bench; then
echo "e2e: bench"
python3 - "$TMP/big.py" <<'PY'
import sys; open(sys.argv[1], "w").write(("def f(x: int) -> str:\n    return str(x + 1)  # c\n") * 5000)
PY
"$NIB" --bench "$TMP/big.py"

fi  # bench
if [ -n "${NIB_E2E_SSH_HOST:-}" ]; then
  echo "e2e: ssh $NIB_E2E_SSH_HOST"
  mkdir -p "$TMP/remote" "$TMP/daemons/x86_64-unknown-linux-musl"
  cp "$(dirname "$NIB")/nibd" "$TMP/daemons/x86_64-unknown-linux-musl/nibd"   # same-machine stand-in
  printf '[profiles.t]\nssh = "%s"\nroot = "%s"\n' "$NIB_E2E_SSH_HOST" "$TMP/remote" > "$TMP/config.toml"
  NIB_DAEMONS="$TMP/daemons" TEXT="print('remote')" run_edit @t new.py
  [ "$(cat "$TMP/remote/new.py")" = "print('remote')" ] || { echo "FAIL: ssh content"; exit 1; }
  echo "  ok"
fi
echo "e2e: all passed"
