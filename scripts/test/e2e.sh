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
want() { [ -z "${NIB_E2E_ONLY:-}" ] || [[ ",$NIB_E2E_ONLY," == *",$1,"* ]]; }
answers() { python3 -c "import socket, sys; socket.create_connection(('127.0.0.1', int(sys.argv[1])), 1)" "$1" 2>/dev/null; }
# On a failure: the servers' terminals, so the log says why (a command not found, a slow start).
show_srv() { for s in one two; do echo "--- pair/$s:"; "$NIB" ctl -s "$SOCK" app log pair "$s" --lines 15 2>&1 | tail -15; done; }
srv() { "$NIB" ctl -s "$SOCK" app list | python3 -c "import json, sys; print(' '.join(a['name'] + '/' + s['name'] + '=' + s['state'] for a in json.load(sys.stdin)['apps'] for s in a['servers']))"; }
closed() { for _ in $(seq "${2:-50}"); do answers "$1" || return 0; sleep 0.1; done; return 1; }   # $2: tries, 0.1 s apart
# Apps the user approved, as their own click in the Apps panel leaves them in apps.json (nib ctl
# defines apps, but never approves one): $1 the window's socket, $2 {name: definition}. The
# hash is the daemon's (serve::hash, pinned by its test): SHA-256 of the definition as compact
# JSON, fields in their order, maps by key, nothing empty.
approved_apps() {
  local nibdir
  nibdir=$("$NIB" ctl -s "$1" state | python3 -c "import json, sys; print(json.load(sys.stdin)['roots']['nibdir'])")
  python3 - "$nibdir" "$2" <<'PY'
import hashlib, json, sys
nibdir, apps = sys.argv[1], json.loads(sys.argv[2])
out = {}
for name, d in apps.items():
    servers = {s: {k: v for k, v in (("cmd", x["cmd"]), ("port", x.get("port")), ("after", x.get("after") or None)) if v is not None} for s, x in sorted(d["servers"].items())}
    canon = {k: v for k, v in (("title", d.get("title")), ("page", d.get("page")), ("servers", servers)) if v is not None}
    h = hashlib.sha256(json.dumps(canon, separators=(",", ":"), ensure_ascii=False).encode()).hexdigest()
    out[name] = dict(canon, approved=h)
json.dump({"apps": out}, open(nibdir + "/apps.json", "w"))
PY
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
python3 -m http.server "$P3" --bind 127.0.0.1 >/dev/null 2>&1 & HOLD=$!
KIDS="$KIDS $HOLD"
for _ in $(seq 50); do answers "$P3" && break; sleep 0.1; done
approved_apps "$SOCK" "{
  \"pair\": { \"servers\": { \"one\": { \"cmd\": \"python3 -m http.server $P1 --bind 127.0.0.1\", \"port\": $P1 },
                           \"two\": { \"cmd\": \"python3 -m http.server $P2 --bind 127.0.0.1\", \"port\": $P2, \"after\": [\"one\"] } } },
  \"held\": { \"servers\": { \"held\": { \"cmd\": \"python3 -m http.server $P3 --bind 127.0.0.1\", \"port\": $P3 } } } }"
echo '{ "servers": { "s": { "cmd": "true", "after": ["nobody"] } } }' | { ! "$NIB" ctl -s "$SOCK" app set bad --json - >/dev/null 2>&1; } || { echo "FAIL: a server after one the app does not have was accepted"; exit 1; }
# What nib ctl defines waits for the user: listed, not started, not approvable from here.
echo "{ \"servers\": { \"s\": { \"cmd\": \"python3 -m http.server $P3 --bind 127.0.0.1\", \"port\": $P3 } } }" | ctl app set mine --json -
! "$NIB" ctl -s "$SOCK" app up mine >/dev/null 2>&1 || { echo "FAIL: nib ctl started an app the user did not approve"; exit 1; }
! "$NIB" ctl -s "$SOCK" raw '{"cmd":"app.approve","name":"mine","hash":"x"}' 2>/dev/null | grep -q '"ok": *true' || { echo "FAIL: nib ctl approved an app"; exit 1; }
ctl app rm mine
[ "$(srv)" = "held/held=stopped pair/one=stopped pair/two=stopped" ] || { echo "FAIL: apps defined: $(srv)"; exit 1; }
ctl app up
# Up to a minute: a fresh machine's first login shell and first python3 can be slow.
for _ in $(seq 600); do [ "$(srv)" = "held/held=port-taken pair/one=ready pair/two=ready" ] && break; sleep 0.1; done
[ "$(srv)" = "held/held=port-taken pair/one=ready pair/two=ready" ] || { echo "FAIL: apps up: $(srv)"; show_srv; exit 1; }
answers "$P1" && answers "$P2" || { echo "FAIL: a ready server's port does not answer"; exit 1; }
python3 -c "import sys, urllib.request; sys.exit(0 if b'hello' in urllib.request.urlopen('http://127.0.0.1:$P1/index.html').read() else 1)" || { echo "FAIL: server one does not serve the project"; exit 1; }
for _ in $(seq 300); do "$NIB" ctl -s "$SOCK" app log pair one | grep -qF 'GET /index.html' && break; sleep 0.1; done
"$NIB" ctl -s "$SOCK" app log pair one --lines 5 | grep -qF 'GET /index.html' || { echo "FAIL: app log"; exit 1; }
ctl app restart pair one
for _ in $(seq 600); do [ "$(srv)" = "held/held=port-taken pair/one=ready pair/two=ready" ] && break; sleep 0.1; done
[ "$(srv)" = "held/held=port-taken pair/one=ready pair/two=ready" ] || { echo "FAIL: server restarted: $(srv)"; show_srv; exit 1; }
ctl app down
[ "$(srv)" = "held/held=stopped pair/one=stopped pair/two=stopped" ] || { echo "FAIL: apps down: $(srv)"; exit 1; }
closed "$P1" && closed "$P2" || { echo "FAIL: a stopped server still listens"; exit 1; }
answers "$P3" || { echo "FAIL: stopping the apps stopped what held the taken port"; exit 1; }
# Running again when the window quits: they go with it.
ctl app up pair
for _ in $(seq 600); do [ "$(srv)" = "held/held=stopped pair/one=ready pair/two=ready" ] && break; sleep 0.1; done
[ "$(srv)" = "held/held=stopped pair/one=ready pair/two=ready" ] || { echo "FAIL: apps up again: $(srv)"; show_srv; exit 1; }
ctl run quit; wait $SPID
closed "$P1" && closed "$P2" || { echo "FAIL: a server outlived the window"; exit 1; }
! pgrep -f -- "http.server ($P1|$P2) " >/dev/null || { echo "FAIL: a server process outlived the window"; exit 1; }
kill $HOLD 2>/dev/null; wait $HOLD 2>/dev/null || true
echo "  ok"

fi  # apps
# The web tab's engine and the agents' page requests, against scripts/test/e2e-web/site.py: a small
# local app, and a stand-in for "another machine" on this one's network address, which must
# never be asked for a page. Runs with the browser nib would use; NIB_E2E_BROWSERS (paths,
# separated by ":") runs it once for each of those instead.
# An agent's way to the page: `nib-web` (`nibd web`), with the environment nibd gives an agent,
# through the daemon to the window and back. Less than the socket: no eval, no pointer.
agent_leg() {
  local nibd scratch
  nibd="$(dirname "$NIB")/nibd"
  scratch=$("$NIB" ctl -s "$SOCK" state | python3 -c "import json, sys; print(json.load(sys.stdin)['roots']['scratch'])")
  mkdir -p "$scratch/mailbox/outbox"
  NIB_MAILBOX="$scratch/mailbox" NIB_SCRATCH="$scratch" python3 - "$nibd" "$WEB_PORT" "$scratch" "$TMP" <<'PY' || { echo "FAIL: nib-web"; exit 1; }
import json, os, subprocess, sys
nibd, port, scratch, tmp = sys.argv[1:]
def web(*args):
    r = subprocess.run([nibd, "web", *args], capture_output=True, text=True, timeout=120)
    try:
        return r.returncode, json.loads(r.stdout)
    except ValueError:
        return r.returncode, {"stdout": r.stdout, "stderr": r.stderr}
def check(what, ok, got):
    if not ok:
        sys.exit(f"nib-web: {what}: {got}")
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
code, v = web("upload", "1", os.path.join(tmp, "no-such-file"))
check("upload of a file that is not there", code == 1 and "is not a file" in v.get("error", ""), v)
code, v = web("dance")
check("usage for what is no request", code == 2, v)
PY
  echo "  ok (nib-web: an agent's requests through the daemon)"
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
    python3 -m http.server "$SP" --bind 127.0.0.1 --directory "$TMP/stale" >/dev/null 2>&1 & STALE=$!
    KIDS="$KIDS $STALE"
    for _ in $(seq 300); do answers "$SP" && break; sleep 0.1; done
    st=$("$NIB" ctl -s "$SOCK" web open "$SP" | python3 -c "import json, sys; v = json.load(sys.stdin); print(v.get('tab', '') if v.get('title') == 'Stale' else '')")
    [ -n "$st" ] || { echo "FAIL: the stale-page check's page did not open"; exit 1; }
    "$NIB" ctl -s "$SOCK" web close --tab "$st" >/dev/null
    kill $STALE; closed "$SP" 300 || { echo "FAIL: the stale-page check's server did not stop"; exit 1; }
    "$NIB" ctl -s "$SOCK" web open "$SP" > "$TMP/stale.out" 2>&1 || true
    grep -qF "nothing is listening" "$TMP/stale.out" || { cat "$TMP/stale.out"; echo "FAIL: a page whose server stopped was shown from the browser's cache"; exit 1; }
    echo "  ok (a stopped server's page is not shown from the cache)"
    # nibd answers nib-web only with a token it gave an agent it started (crates/nibd/src/tokens.rs),
    # which a script cannot get yet; so in CI (the release smoke test) the agent's leg is skipped.
    if [ -n "${CI:-}" ]; then
      echo "  nib-web (an agent's way): skipped in CI, needs an agent nib started (\$NIB_TOKEN)"
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
  # The address must answer this machine: some VMs (GitHub's macOS runners) drop connections to
  # their own address, and a page whose image never comes never finishes loading. Refused: the
  # site is not up yet, try again; no answer at all: it never will.
  if ! python3 - "$OUTSIDE" "$((WEB_PORT + 1))" <<'PY'
import socket, sys, time
for _ in range(60):
    try:
        socket.create_connection((sys.argv[1], int(sys.argv[2])), 2).close()
        sys.exit(0)
    except ConnectionRefusedError:
        time.sleep(0.5)
    except OSError:
        sys.exit(1)
sys.exit(1)
PY
  then
    # Asked for by name: the web leg's checks (the local-only rule) must run, not be skipped.
    [ -z "${NIB_E2E_OUTSIDE:-}" ] || { echo "FAIL: NIB_E2E_OUTSIDE=$OUTSIDE does not answer this machine"; exit 1; }
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
# Two free ports in a row, F2 = F1 + 1: F1's first forward lands on F1 + 1 here (F1 is taken: the
# other machine is this one), so a local address that showed one page is later asked for another
# machine's port with nothing behind it. GitHub's runners hand out ports like this by chance.
read -r F1 F2 <<< "$(python3 -c "
import socket
while True:
    a, b = socket.socket(), socket.socket()
    a.bind(('127.0.0.1', 0)); p = a.getsockname()[1]
    try:
        b.bind(('127.0.0.1', p + 1))
    except OSError:
        continue
    print(p, p + 1); break")"
python3 -m http.server "$F1" --bind 127.0.0.1 --directory "$TMP/far" >/dev/null 2>&1 & FAR=$!
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
approved_apps "$SOCK" "{ \"far\": { \"page\": { \"server\": \"web\", \"path\": \"/\" }, \"servers\": { \"web\": { \"cmd\": \"python3 -m http.server $F2 --bind 127.0.0.1\", \"port\": $F2 } } } }"
ctl app up
for _ in $(seq 600); do GOT=$(fwd forward list); [ -n "$GOT" ] && break; sleep 0.1; done   # a slow first start, as above
L3=${GOT##*=}
[ "$GOT" = "@far:$F2=$L3" ] || { echo "FAIL: a ready server's port was not forwarded: $GOT"; exit 1; }
ctl app down
for _ in $(seq 50); do [ -z "$(fwd forward list)" ] && break; sleep 0.1; done
[ -z "$(fwd forward list)" ] && closed "$L3" || { echo "FAIL: a stopped server's forward stayed"; exit 1; }
# The server itself is gone too before its port is asked for again below (slow on a fresh machine).
closed "$F2" 300 || { echo "FAIL: a stopped server still listens on $F2"; exit 1; }
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
  # An agent there (`nib-web`) means its own machine's port: skipped in CI like the web leg's
  # agent part (nibd answers nib-web only with a token it gave an agent it started).
  if [ -z "${CI:-}" ]; then
    # An agent there (`nib-web`, run on the daemon's machine) means its own machine's port too;
    # its files are not where the browser is.
    FSCRATCH=$("$NIB" ctl -s "$SOCK" state | python3 -c "import json, sys; print(json.load(sys.stdin)['roots']['scratch'])")
    mkdir -p "$FSCRATCH/mailbox/outbox"
    NIB_MAILBOX="$FSCRATCH/mailbox" "$NIBD_BIN" web open "$F1" | grep -qF '"host": "@far"' || { echo "FAIL: nib-web open on another machine was not forwarded"; exit 1; }
    NIB_MAILBOX="$FSCRATCH/mailbox" "$NIBD_BIN" web read | grep -qF 'hello from afar' || { echo "FAIL: nib-web read on another machine"; exit 1; }
    { NIB_MAILBOX="$FSCRATCH/mailbox" "$NIBD_BIN" web upload 1 "$TMP/far/index.html" || true; } | grep -qF 'not supported yet' || { echo "FAIL: nib-web upload from another machine was not refused"; exit 1; }
    NIB_MAILBOX="$FSCRATCH/mailbox" "$NIBD_BIN" web close >/dev/null
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
