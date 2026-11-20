#!/usr/bin/env bash
# GUI end-to-end smoke test: runs the real `nib`, drives it through its control socket
# (`nib ctl`), and checks the files on disk. macOS: runs in a normal window. Linux: under Xvfb on
# the CPU renderer (apt install xvfb). Optional ssh leg via NIB_E2E_SSH_HOST.
#   cargo build --release && scripts/e2e.sh
set -euo pipefail
cd "$(dirname "$0")/.."
NIB=${NIB:-target/release/nib}   # also built by `scripts/build.sh client` (host target)
TMP=$(mktemp -d)
XPID=
trap '[ -n "$XPID" ] && kill $XPID 2>/dev/null; rm -rf "$TMP"' EXIT
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

echo "e2e: local"
printf 'import os\n' > "$TMP/a.py"
TEXT='x = 1' run_edit "$TMP/a.py"
[ "$(cat "$TMP/a.py")" = $'import os\nx = 1' ] || { echo "FAIL: local content: $(cat "$TMP/a.py")"; exit 1; }
echo "  ok"

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

echo "e2e: bench"
python3 - "$TMP/big.py" <<'PY'
import sys; open(sys.argv[1], "w").write(("def f(x: int) -> str:\n    return str(x + 1)  # c\n") * 5000)
PY
"$NIB" --bench "$TMP/big.py"

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
