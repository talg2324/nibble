---
name: nib-control
description: See and drive nib (the nibble code editor) through its control socket with `nib ctl` — read editor, terminal and panel state as JSON, take a PNG snapshot, run named commands, press keys, type, open files, add / test / switch SSH connections — or run a windowless nib with `nib --headless`. Use whenever you need to look at or operate nib, while debugging nibble or while helping someone who is using it. Never ask for screen or computer control for nib; use this instead.
---

# Driving nib with `nib ctl`

Every nib window listens on a private unix socket (macOS and Linux). `nib ctl` talks to it and
prints JSON: `"ok": true|false`; exit code 0 = ok, 1 = the window refused, 2 = couldn't connect.

`nib` below is the nib binary: on `PATH`, next to this `.skills` folder in a release, or
`target/debug/nib` / `target/fast/nib` in a nibble checkout.

## Which window
- **One you start (preferred for experiments).** Headless needs no screen and touches nothing of
  the user's:
  ```bash
  nib --headless path/to/file.py > "$SCRATCH/nib.out" &   # run in the background
  # first line of nib.out: the session — {"pid": 1234, "run_dir": "...", "socket": "...", "logs": {...}}
  nib ctl -p 1234 wait                              # until the file has loaded
  ...
  nib ctl -p 1234 run quit                          # twice if a buffer is dirty
  ```
  (`$SCRATCH` = your own scratch directory; see "Where files go".)
  Headless runs the same editor as the window (tabs, terminals, LSP, git, the daemon). Only
  `snapshot` is missing. Use a real window (`nib file` instead of `nib --headless file`) when the
  question is how something *looks*; it appears on the user's screen, so quit it when done.
- **The user's own window.** `nib ctl list` shows every open window (pid, title, root). Reading
  it (`state`, `snapshot`) is fine. Drive it (`run`, `key`, `type`, `open`) only when the user
  asked you to act in their editor.

Choosing: `-p PID`, or `-s SOCKET`, or `$NIB_CONTROL`; with none of them `nib ctl` uses the only
open window and refuses if there are several. `NIB_CONTROL=<path>` at launch picks the socket
path (keep it under ~100 bytes); `NIB_CONTROL=off` turns the socket off.

## Where files go
Every window has a run directory (`session.run_dir` in `state`, `hello` and `nib ctl list`;
`<TMPDIR or XDG_RUNTIME_DIR>/nib-<uid>/<pid>/`), private to the user:

| | |
|---|---|
| `ctl.sock` | the socket |
| `meta.json` | pid, label, root, version, socket, log paths, `started` / `ended` (unix seconds) |
| `nib.log` | the window's trace: every control request (yours included), refusals (`ctl!`), flash messages (`info` / `warn`), disconnects (`disc`), `start` / `exit`, and slow operations (`slow  rpc Diff 412.00ms`: requests, `update` steps, `view`, `draw`, `ping`; nibd logs its own as `slow  req …` / `notify …`). Launch with `NIB_TIMING=all` to log every operation as `time  …`; `NIB_SLOW_MS=N` sets the threshold (default 50 ms on the UI thread, 250 ms for the daemon) |
| `snapshots/` | `001.png`, `002.png`, … — where `snapshot` saves by default |

The daemon's stderr (nibd and ssh) is in `session.logs.nibd` (`~/.cache/nib/<target>.log`,
shared by every session on that target; read its tail).

Rules:
1. **Files nib makes stay in the run directory.** Call `snapshot` without a path, and refer to
   snapshots and logs by their path there instead of copying them.
2. **Your own files** (test files you open in nib, scripts, the headless `nib.out`) follow the
   `nib-artifacts` skill.
3. **When something breaks, read before you reproduce:** `nib.log` (what happened, in order),
   then the tail of `logs.nibd` (daemon errors, panics), then `state`. A crashed window's run
   directory survives for a day.

## Commands
| | |
|---|---|
| `state` | `session` (run dir, socket, logs), `roots` (scratch and NIBDIR paths on the code's machine, which one the tree shows), title, focus, panels, tabs, active editor (cursor, selections with their text, visible lines), terminals (full screen text, cursor), `pending` daemon requests, last flash + recent `messages` |
| `state.diff` | the diff tab (`run toggle_diff`; `null` while closed): `scope` (`head` / `unstaged` / `staged` / `everyone` / `agent:N`; `scopes` lists the ones on offer, `diff open SCOPE` shows one), `work` and `plan` (an agent's branch and plan, in its scope), `files` (path, status, adds, dels, folded), `cursor` and `at` (the row the keys act on: file, hunk or line with its numbers and text). Keys: `j` `k` rows, `n` `p` hunks, `]` `[` files, `s` or `1` `2` `3` scope, `space` stage / unstage the file, `enter` open the line. It follows the disk by itself (about a second) |
| `diff comment` … `diff send` | review the agent's work the diff tab shows: `diff comment [--on PATH[:LINE[-END]]] TEXT` (lines of the file as it is now; no `--on`: the whole work), `diff reply ID TEXT`, `diff resolve|reopen|remove ID`, `diff send` (the open comments go to the agent as its next message). `state.diff.review.comments[]` has each comment with the agent's `replies`; `diff open since:N` shows what it changed since the review |
| `ai diff` / `ai accept` | an agent said its job is done (`state.ai.accepting`): `ai diff` shows its work in the diff tab instead of answering; `ai accept` accepts it, which for an agent on its own branch (`state.diff.work.branch`) lands the branch as one commit first (`wait`, then `state.flash` says `landed …` or `not landed: …`) |
| `snapshot [FILE]` | PNG of the window, by default into `snapshots/`; prints the path. Look at it with your image-reading tool. Not headless |
| `commands` | every name `run` accepts (snake_case: `save`, `new_terminal`, `move_doc_end`, `toggle_git`, …) |
| `run NAME` | run a command exactly as its shortcut would |
| `key CHORD` | press a chord: `mod+s`, `ctrl+\``, `enter`, `shift+f6` (`mod` = Cmd on macOS, Ctrl elsewhere) |
| `type TEXT` | type into whatever has focus (editor, terminal, AI box); `\n` = Enter, `\t` = Tab |
| `open PATH` | open a file, relative to the window's root or absolute |
| `search [--project] TEXT` | Find (Cmd+F). Without `--project`: in the file or agents chat with focus (`matches`, `current`; the match is selected). With it: across the project on the daemon (works over SSH): `results.files` (fuzzy name matches) and `results.lines` (`path`, 1-based `line` / `col`, `text`); hidden folders, gitignored and binary files skipped. `search --next` / `--prev` / `--close`, `search --open N` (file names first, then lines). `state.search` shows the box |
| `drop tree DIR PATH…` / `drop chat PATH…` | the same as dragging files from the desktop onto the file tree's folder `DIR` (`.` = root; copied to the daemon's machine, renamed `name 2.ext` when taken) or onto the agents chat (pictures go with the message, text files become `@` mentions) |
| `ai …` | the AI panel (assistants, models, warm/cold, update): see the **nib-ai** skill, which also covers `nib ai` — local models and sub-agents without a window |
| `wait [SECS]` | poll until no requests to the daemon are outstanding (not logged) |
| `raw JSON` | one request as-is: `{"cmd":"state"}` |

## SSH connections
A window works on one machine at a time and **switches in place** (same window, same socket):
saved connections are `[profiles]` in the user's config.toml. `state.connection` says where the
window is, whether it is connecting, and the last error.

| | |
|---|---|
| `ssh list` | saved connections (`current` marks the window's), `home` (where it started), unsaved `~/.ssh/config` hosts |
| `ssh add NAME HOST [--root DIR] [--user U] [--port N] [--identity FILE] [--jump HOST] [--nibd PATH] [--test]` | save one (root defaults to `~`); `--test` also tests it |
| `ssh update NAME [--host H] [...] [--rename NEW] [--unset FIELD]` | change one |
| `ssh remove NAME` | delete one |
| `ssh test NAME` / `ssh test --host H [...]` | reach it, host key, key login, platform, folder. `test.stage`: `ok`, `network`, `host_key_unknown` (with `fingerprints`), `host_key_changed`, `auth` (`password: true` = only a person can log in), `platform`, `root` |
| `ssh connect NAME [--root DIR] [--force]` / `ssh connect --home` | switch the window (default root: the one used last; `roots` in `ssh list`); answers once the new daemon is up. Refuses if unsaved files or terminals would close: ask the user before `--force` |
| `run open_folder` | the SSH panel's folder browser (a person picks the project folder there; agents use `ssh root`) |
| `ssh root PATH` | another folder on the same machine as the project root (absolute, `~/…`, or relative); tabs and terminals stay |
| `ssh cancel` | stop a connect in progress |

Rules: you connect with keys only, and nothing is asked. A `host_key_unknown` host is refused
until **the user** checks the fingerprint and presses "Trust this host key" in the SSH panel;
there is no API for that, and you should not edit known_hosts yourself. Passwords and 2FA
codes are only typed by the user (connect from the panel). Switching closes the user's work on
the current machine: only do it in their window when they asked; otherwise use a headless nib.

## Habits
- Prefer `state` over `snapshot`: exact and cheap. `snapshot` is for layout, colour and
  rendering questions.
- `run` a command by name rather than `key`-ing its shortcut: shortcuts differ per OS and per
  user config, names don't.
- After anything that goes to the daemon (open, save, LSP, git, a new terminal), `wait` before
  reading `state`. Terminal output needs a short sleep after typing, then `terminals[].screen`.
- Inputs take the same path as the keyboard, so what you reproduce through `nib ctl` is real.

## For nibble developers
Code: `crates/nib/src/control.rs` (socket + CLI), `crates/nib/src/app/control.rs` (answers),
`crates/nib/src/headless.rs` (windowless loop). `scripts/e2e.sh` is the reference for scripted
use. New request kinds go in `App::on_control`, and in `ctl()` if they need CLI sugar; then add
a row above. Keep the transport plain (socket + JSON lines; no MCP). Planned: requests for the
localhost viewer (forwarded ports, read the page, act on it).
