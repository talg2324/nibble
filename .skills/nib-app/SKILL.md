---
name: nib-app
description: Register the project's apps with nib and start, stop, restart and read them (a backend and its frontend, TensorBoard, Jupyter…), each server in a terminal the user sees, instead of launching them yourself. Use whenever the user asks you to register (set up) how an app of this project is run, whenever you need an app running or restarted (after changing its code or config), or want to read what a server printed. Never start a long-running server with `cd … && nohup … &` or a background shell.
---

# The project's apps

An **app** is what the user opens from the **Apps** panel on nib's toolbar: the servers it
runs (each one a process, in a terminal of its own in the window) and, if it has one, the page
nib shows in a web tab once its server answers. The app is the one thing that is registered,
opened, stopped and removed; its servers are parts of it. They are **the user's view of the
running app**, so use them, don't start a second copy beside them:

- a server you start by hand (`nohup … &`, a background shell) is invisible to the user, holds
  the port so nib's own cannot start (`port-taken`), and is left behind when the session ends;
- nib's servers end when the window closes. Nothing to clean up.

## Registering an app for the user

The user tells you how they run the app (their instructions follow the request); registering
it is writing that down for nib, once, so that next time they open it with one click.

1. **Read the user's instructions**, and fill the gaps from the project: README, `package.json`
   scripts, `pyproject.toml`, `Makefile`, `docker-compose.yml`, `.env.example`. Ask the user
   only what the project cannot tell you.
2. **Pick fixed ports** (the ones the project already uses, when it has them).
3. **Write the app as JSON**: one entry in `servers` per process; `after` for the order (a
   frontend after its backend); `page` on the server whose page the user looks at.
4. **Register it, start it, look at it**:
   ```
   nib-app set myapp --json - <<'EOF'
   { "title": "My app",
     "page": { "server": "web", "path": "/" },
     "servers": {
       "api": { "cmd": "uv sync && uv run myapp serve --no-open --port 8765", "port": 8765 },
       "web": { "cmd": "npm run dev -- --port 8766 --host 127.0.0.1", "port": 8766,
                "after": ["api"], "cwd": "frontend", "env": { "API_URL": "http://127.0.0.1:8765" } } } }
   EOF
   nib-app open myapp
   nib-web read                  # its page renders? (the nib-web skill)
   ```
   Not ready? `nib-app log myapp api` says why; fix the definition and `set` it again.
5. **Tell the user** it is under **Apps** on the toolbar, and that the first time they open it
   nib shows them its commands to approve.

What a server's command must be:

- **In the foreground**, for as long as the server runs: no `&`, no `--daemon`, no `nohup`. Steps
  that come first and end (installing, building, migrating) go in front of it with `&&`.
- **Listening on 127.0.0.1 at its `port`**, said in the command too (`--port 8765`,
  `--host 127.0.0.1`): the port is how nib knows it is ready and that it is free.
- **Not opening a browser** (`--no-open`, `BROWSER=none`, `--no-browser`): nib shows the page.
- Run by the user's shell, in the project root (`cwd`: a folder inside it).
- `env` may not set what changes which program runs (`PATH`, `LD_*`, `DYLD_*`, `NODE_OPTIONS`,
  `PYTHONPATH`, `PYTHONSTARTUP`…): put what the command needs in the command.

## The user approves what you register

An app you define (or change) **waits for the user's approval**: the Apps panel marks it, and
the first time they open it, nib shows them every command, folder and variable and asks. Only
they approve, with their own click; nothing you can run does it for them. Until then:

- you may start the app **you** registered, to check it works (`nib-app open`);
- the user's own "start everything" skips it, and so does any other agent.

Changing an approved app (any `set`) takes the approval away again. So register what the user
asked for, and nothing they would be surprised to read.

## Commands

Run them as ordinary shell commands (`nib-app` is on your PATH inside an agent nib started):

| | |
|---|---|
| `nib-app list` | every app: its state, approval, page; under it each server's name, state, port, what it comes after, its command |
| `nib-app set APP --json FILE\|-` | register or replace an app, whole (the JSON above; `-`: from stdin). Kept in `$NIBDIR/apps.json`, outside the repo |
| `nib-app open APP…` | start its servers, each after those it comes `after`, and wait until each is `ready` or has failed (at most 60 s; `--timeout SECONDS`, `0`: don't wait), then print the list. Its page shows in the user's web tab, behind what they are looking at |
| `nib-app up [APP…]` | the same; none named: every app you may start |
| `nib-app restart APP [SERVER]` | stop its servers (or one), wait for the ports to be free, start them, wait until ready. The user's web tab of it reloads by itself |
| `nib-app down [APP…]` | stop them (none named: all) |
| `nib-app log APP [SERVER] [--lines N]` | the end of what a server printed (default: the page's server, 40 lines): read this when it is not `ready` |
| `nib-app rm APP` | stop it and forget it |

If `nib-app` is not found you are not inside nib's agents tab; from any shell on the window's
machine `nib ctl app …` does the same (the nib-control skill).

## States

| | |
|---|---|
| `stopped` | registered, not running |
| `waiting` | for the servers it comes `after`: it starts when their ports answer |
| `starting` | running; its port does not answer yet |
| `ready` | running and its port answers on 127.0.0.1 or `[::1]` (a server with no port: as soon as it runs) |
| `exited(CODE)` | it ended by itself. Read `nib-app log APP SERVER`, fix the cause, `nib-app open APP`. Plain `exited`: it was never started, because a server it comes after ended or was stopped (its log says which) |
| `port-taken` | **not started**: something else already listens on its port |

`port-taken` is not a failure of the server. Another project's server, or one somebody started
by hand, has the port. Find it (`lsof -nP -iTCP:PORT -sTCP:LISTEN`; on Windows
`netstat -ano | findstr :PORT`) and tell the user what holds it; stop it only if it is yours or
the user says so. Or give this app another port (the JSON's `port` and the same port inside
`cmd`), `set` it again, then `nib-app open`. Do not work around it by pointing at the other
process: it may be serving another project.

## Habits

- Before starting anything, `nib-app list`: the app you need may be registered, or running.
- Changed code the server does not reload by itself? `nib-app restart APP`, not a second copy.
- Something is wrong with the app? `nib-app log APP` first.
- Leave the apps running when you finish unless the user asked otherwise: they are theirs.
