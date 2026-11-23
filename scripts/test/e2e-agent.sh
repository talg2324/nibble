#!/usr/bin/env bash
# Live end-to-end check of worktree-per-feature with a real agent: a throwaway git repo
# with a bug, a real nib (headless) driven by `nib ctl`, a real Claude Code agent asked to fix it,
# its "done" accepted as ⌘↵ would. Passes when the fix landed on the repo's branch as one commit
# and nothing is left uncommitted or checked out beside it. Spends model tokens: run by hand.
#   cargo build && scripts/test/e2e-agent.sh            NIB_E2E_RUNS=5 for a rate; NIB_E2E_MODEL=…
#   NIB_E2E_WPF=off checks the other way: the fix is left in the tree, and the summary says so.
set -euo pipefail
cd "$(dirname "$0")/../.."
NIB=${NIB:-target/debug/nib}
RUNS=${NIB_E2E_RUNS:-1}
WPF=${NIB_E2E_WPF:-on}
TIMEOUT=${NIB_E2E_TIMEOUT:-600}
command -v claude >/dev/null || { echo "skip: Claude Code (claude) is not installed"; exit 0; }

pass=0 first_try=0
for run in $(seq "$RUNS"); do
  TMP=$(mktemp -d)
  # Its own config, work trees and caches; Claude Code's sign-in stays the user's.
  export NIB_CONFIG="$TMP/config.toml" XDG_DATA_HOME="$TMP/data" XDG_CACHE_HOME="$TMP/cache"
  printf '[ai]\nagent = "claude"\npick_model = false\n%s' "$([ "$WPF" = off ] && echo 'worktree_per_feature = false')" > "$NIB_CONFIG"
  [ -n "${NIB_E2E_MODEL:-}" ] && printf 'model = "%s"\n' "$NIB_E2E_MODEL" >> "$NIB_CONFIG"
  REPO="$TMP/calc"
  mkdir -p "$REPO"
  git -C "$REPO" init -q -b main
  git -C "$REPO" config user.name e2e; git -C "$REPO" config user.email e2e@example.com; git -C "$REPO" config commit.gpgsign false
  printf 'def add(a, b):\n    return a - b\n' > "$REPO/calc.py"
  printf '__pycache__/\n' > "$REPO/.gitignore"
  git -C "$REPO" add -A; git -C "$REPO" commit -qm "first"

  SOCK="$TMP/ctl.sock"
  NIB_CONTROL="$SOCK" "$NIB" --headless "$REPO/calc.py" > "$TMP/nib.out" 2>&1 &
  PID=$!
  trap 'kill $PID 2>/dev/null || true' EXIT
  for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
  ctl() { "$NIB" ctl -s "$SOCK" "$@"; }
  ctl wait 20 >/dev/null
  ctl run toggle_ai >/dev/null
  ctl type "add() in calc.py subtracts instead of adding. Fix it." >/dev/null
  ctl key enter >/dev/null

  # Until it says it is done (the dialog is up), or time runs out.
  state=
  for _ in $(seq "$TIMEOUT"); do
    state=$(ctl state)
    python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["ai"]["accepting"] else 1)' <<<"$state" && break
    sleep 1
  done
  said=$(python3 -c 'import json,sys; a=json.load(sys.stdin)["ai"]; print(json.dumps({"accepting": a["accepting"], "finished": [c["finished"] for c in a["chats"] if c.get("finished")], "nib": [m["text"][:200] for m in a["messages"] if m["from"] in ("nib","mail") or "worktree-per-feature is on" in m["text"].lower()]}))' <<<"$state")
  echo "run $run: $said"
  if ! grep -q '"accepting": true' <<<"$said"; then
    echo "  FAIL: no done within ${TIMEOUT}s"; kill $PID; rm -rf "$TMP"; continue
  fi
  ctl ai accept >/dev/null
  ctl wait 60 >/dev/null || true
  sleep 2

  fixed=$(grep -c 'a + b' "$REPO/calc.py" || true)
  commits=$(git -C "$REPO" rev-list --count HEAD)
  dirty=$(git -C "$REPO" status --porcelain)
  trees=$(git -C "$REPO" worktree list | wc -l | tr -d ' ')
  branches=$(git -C "$REPO" branch --list 'nib/*' | wc -l | tr -d ' ')
  if [ "$WPF" = on ]; then
    if [ "$fixed" = 1 ] && [ "$commits" = 2 ] && [ -z "$dirty" ] && [ "$trees" = 1 ] && [ "$branches" = 0 ]; then
      pass=$((pass + 1))
      echo "  ok: landed as \"$(git -C "$REPO" log -1 --format=%s)\""
      # Sent back on the way (the hook, nib's letter, a refused done) or right the first time.
      grep -qi 'worktree-per-feature is on' <<<"$said" || first_try=$((first_try + 1))
    else
      echo "  FAIL: fixed=$fixed commits=$commits worktrees=$trees nib-branches=$branches dirty=[$dirty]"
    fi
  else
    if [ "$fixed" = 1 ] && [ "$commits" = 1 ] && [ "$dirty" = " M calc.py" ] && grep -q '"stray": \["calc.py"\]' <<<"$said"; then
      pass=$((pass + 1)); first_try=$((first_try + 1))
      echo "  ok: left in the tree, and the summary said so"
    else
      echo "  FAIL: fixed=$fixed commits=$commits dirty=[$dirty]"
    fi
  fi
  ctl run quit >/dev/null 2>&1 || kill $PID 2>/dev/null || true
  wait $PID 2>/dev/null || true
  trap - EXIT
  rm -rf "$TMP"
done
echo "e2e-agent (wpf $WPF): $pass/$RUNS passed, $first_try/$RUNS without being sent back"
[ "$pass" = "$RUNS" ]
