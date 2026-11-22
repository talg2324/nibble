---
name: nib-serve
description: Start, stop, restart and read the project's dev servers (backend, frontend, TensorBoard, Jupyter…) through nib, each in a terminal the user sees, instead of launching them yourself. Use whenever you need a server of the project running or restarted (after changing its code or config), want to read what it printed, or the user asks you to set up how the project is served. Never start a long-running server with `cd … && nohup … &` or a background shell.
---

# The project's servers

nib runs the project's servers for the user: each one in a terminal of its own in the window,
headed with its name and state, and a web tab for the one with a page. They are **the user's
view of the running app**, so use them, don't start a second copy beside them:

- a server you start by hand (`nohup … &`, a background shell) is invisible to the user, holds
  the port so nib's own cannot start (`port-taken`), and is left behind when the session ends;
- nib's servers end when the window closes. Nothing to clean up.

## Commands

Run them as ordinary shell commands (`nib-serve` is on your PATH inside an agent nib started):

| | |
|---|---|
| `nib-serve list` | every server: name, state, port, what it comes after, its command |
| `nib-serve up [NAME…]` | start them (none named: all), with the servers they come after. Waits until each is `ready` or has failed (at most 60 s; `--timeout SECONDS`, `0`: don't wait), then prints the list |
| `nib-serve restart NAME` | stop it, wait for its port to be free, start it, wait until it is ready. The user's web tab of it reloads by itself |
| `nib-serve down [NAME…]` | stop them (none named: all) |
| `nib-serve log NAME [--lines N]` | the end of what it printed (default 40 lines): read this when it is not `ready` |
| `nib-serve set NAME --cmd "…" [--port N] [--after A,B] [--view /path] [--cwd DIR] [--title T] [--env K=V]…` | define or redefine a server |
| `nib-serve rm NAME` | stop it and forget its definition |

If `nib-serve` is not found you are not inside nib's agents tab; from any shell on the window's
machine `nib ctl serve …` does the same (the nib-control skill).

## States

| | |
|---|---|
| `stopped` | defined, not running |
| `waiting` | for the servers it comes `after`: it starts when their ports answer |
| `starting` | running; its port does not answer yet |
| `ready` | running and its port answers on 127.0.0.1 or `[::1]` (a server with no port: as soon as it runs) |
| `exited(CODE)` | it ended by itself. Read `nib-serve log NAME`, fix the cause, `nib-serve up NAME`. Plain `exited`: it was never started, because a server it comes after ended or was stopped (its log says which) |
| `port-taken` | **not started**: something else already listens on its port |

`port-taken` is not a failure of the server. Another project's server, or one somebody started
by hand, has the port. Find it (`lsof -nP -iTCP:PORT -sTCP:LISTEN`) and tell the user what holds
it; stop it only if it is yours or the user says so. Or give this project another port
(`nib-serve set` with the new `--port`, and the same port inside `--cmd`), then `nib-serve up`.
Do not work around it by pointing at the other process: it may be serving another project.

## Defining servers

Definitions live in `$NIBDIR/serve.json`, outside the repo, one per project. Write them with
`nib-serve set` (it checks them) rather than editing the file:

```
nib-serve set backend  --cmd "uv run myapp serve --no-open --port 8765" --port 8765
nib-serve set frontend --cmd "uv run myapp frontend --port 8766 --api http://127.0.0.1:8765" \
                       --port 8766 --after backend --view / --title "My app"
nib-serve up
```

- `--cmd` runs in the user's login shell, in the project root (`--cwd DIR`: a folder inside it).
  It must stay in the foreground (no `&`, no `--daemon`), and must not open a browser itself.
- `--port` is how nib knows it is ready, and that the port is free before starting. The command
  must listen on 127.0.0.1 at that same port: say it in the command too.
- `--after A,B`: started only once those servers' ports answer.
- `--view /path`: once ready, the window shows `http://127.0.0.1:PORT/path` in a web tab
  (read and operate it with `nib-web`, the nib-web skill).
- Names are `a-z`, `0-9`, `_`, `-`. `set` replaces the whole definition; a running server keeps
  its old one until `nib-serve restart NAME`.

## Habits

- Before starting anything, `nib-serve list`: the server you need may be defined, or running.
- Changed code the server does not reload by itself? `nib-serve restart NAME`, not a second copy.
- Something is wrong with the app? `nib-serve log NAME` first.
- Leave the servers running when you finish unless the user asked otherwise: they are theirs.
