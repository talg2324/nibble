---
name: nib-ai
description: Run local models and coding-assistant sub-agents through nib — `nib ai` makes one schema-constrained model call (Ollama), runs one OpenCode sub-agent with only the commands and files it may touch, or runs a resumable batch of thousands of them, on the machine and the GPUs you choose (this one, or an SSH profile), with provenance recorded. Also how to install OpenCode + Ollama, download / delete Ollama models and see what each takes on disk, warm/cool local models, and drive the AI panel of a nib window. Use instead of writing Python around ollama / opencode, whenever you orchestrate local LLM calls or sub-agents (labeling, extraction, audits, swarms), or work with nib's AI features.
---

# Local models and sub-agents with `nib ai`

`nib ai` talks to nibd directly — no window. Jobs run **where nibd runs**: on this machine, or
with `nib ai @PROFILE …` on that SSH profile's machine, next to its GPUs and its data. Nothing
is copied: every path in a job is a path **on that machine**, relative to the root (`--root`,
the profile's root, or the current folder). Output is JSON on stdout. Exit code: 0 ok, 1 the
job (or a batch job) failed, 2 usage or connection error.

`nib` is the nib binary: on `PATH`, next to this `.skills` folder in a release, or
`target/debug/nib` / `target/fast/nib` in a nibble checkout.

## Pick the right tool

| You want | Use |
|---|---|
| a model's answer to a prompt, maybe as JSON matching a schema (extraction, classification, labeling) | `generate` — one call straight to Ollama; greedy and seeded by default; no tools |
| a sub-agent that reads files, runs a command or two, writes its result to a file | `run` — one OpenCode run; may run only the `allow`ed commands and write only the `write` patterns |
| many of either, resumable, with retries and the raw answer kept on failure | `batch` — a JSONL file of jobs |

## Look first

```sh
nib ai models        # assistants, their models (local ones: "local": true, "warm": true/false), GPUs,
                     # and "ollama": {installed, disk, models: [{name, id, bytes, size, warm}]}
nib ai gpus          # [{index, name, memory_mb, kind}] — what --gpu indices mean
```

OpenCode lists every model Ollama has downloaded as `ollama/<name>`, with no OpenCode config
needed: nib adds an `ollama` provider for them on each run (merged with the user's own).

`generate` takes an **Ollama** model name (`qwen3.8:27b-mlx`); `run` takes an **OpenCode**
model id (`ollama/qwen3.8:27b-mlx` — provider/model, as `models` lists it).

## One model call: `generate`

```sh
nib ai generate qwen3.8:27b-mlx --prompt-file prompts/extract.txt --schema schema.json \
    --no-think --ctx 8192 --max-tokens 2048 --var report_id=17
```

Result: `ok`, `text`, `json` (when a schema was given: parsed; a non-JSON answer is a
failure), `thinking`, and provenance — `model`, `digest` (pins the exact weights), `ollama`
(server version), `gpu`, `options` (temperature 0 and seed 0 unless set), `prompt_tokens`,
`new_tokens`, `done_reason`, `prompt_seconds`, `eval_seconds`, `load_seconds`, `seconds`.

- Keep `--ctx` the same across a run: a different context size makes Ollama load the model again.
- Put what jobs share **first** in the prompt (the report, then the per-algorithm question):
  Ollama reuses the work on a shared beginning, so jobs that share one run much faster in a row.

## One sub-agent: `run`

```sh
nib ai run ollama/qwen3.8:27b-mlx --cwd runs/corpus-gate-01/manual \
    --prompt-file sub.md --var i=17 --var RUN=swarm/2026-11-20-a \
    --allow 'uv run python show_row.py *' --write 'swarm/2026-11-20-a/row_*.json'
```

The agent may read inside `--cwd`; run only commands matching an `--allow` pattern (`*` is a
wildcard); create or change only files matching a `--write` pattern (relative to `--cwd`); no
web, no sub-agents of its own, nothing outside `--cwd`. Anything else is refused and shows up in
`tools` with `status: "error"` (counted in `tool_errors`).

Result: `ok`, `reply` (its final text — ask for ONE line), `files` (the `--write` files it
created or changed), `tools` (every tool call: tool, status, input, error), `tokens`, `model`,
`gpu`, `seconds`. Its OpenCode session is deleted afterwards (`--keep-session` keeps it).

Have each sub-agent write its full result to its own file and reply with one line; read the
files with a script, not into your own context.

A run stops after 15 minutes (`--timeout SECS` to change it, `0` for no limit; `generate`:
30 minutes): a sub-agent still going by then is almost always going in circles. Give a long job
a bigger limit rather than none.

**It shows in your chat.** Started from inside an agent nib runs, a run appears in that agent's
chat as it happens: one step for the run (named by `--id`, else the prompt's first line), each of
the sub-agent's tool calls nested in it, and its reply and changed files when it ends. The user
sees what it did and that it is yours; its edits count as yours (Follow, file clashes with other
agents: announce its `--write` files like your own). In a `batch`, only each job's step shows.

A run stops after 15 minutes (`--timeout SECS` to change it, `0` for no limit; `generate`:
30 minutes): a sub-agent still going by then is almost always going in circles. Give a long job
a bigger limit rather than none.

**It shows in your chat.** Started from inside an agent nib runs, a run appears in that agent's
chat as it happens: one step for the run (named by `--id`, else the prompt's first line), each of
the sub-agent's tool calls nested in it, and its reply and changed files when it ends. The user
sees what it did and that it is yours; its edits count as yours (Follow, file clashes with other
agents: announce its `--write` files like your own). In a `batch`, only each job's step shows.

## Many: `batch`

One job per line, the same keys as the flags (`prompt_file`, `schema_file`, `max_tokens`, …);
`id` is required and names the result files. Lines starting with `//` are skipped.

```jsonl
{"id": "row_000", "kind": "run", "model": "ollama/qwen3.8:27b-mlx", "cwd": "manual", "prompt_file": "sub.md", "vars": {"i": 0}, "allow": ["uv run python show_row.py *"], "write": ["swarm/a/row_*.json"]}
{"id": "s17_lvef", "kind": "generate", "model": "qwen3.8:27b-mlx", "prompt_file": "p/s17_lvef.txt", "schema_file": "schemas/extract.json", "think": false, "ctx": 8192}
```

```sh
nib ai batch jobs.jsonl --out runs/x --concurrency 2 --retries 1
```

- Prints one JSON line per finished job (`id`, `ok`, `attempts`, `seconds`, `error`), then
  `{"summary": {jobs, done, failed, resumed, out, manifest}}`.
- Writes under `--out`: `<id>.json` (the result), `<id>_error.json` (the last failure, with
  `raw` — what the model said), `progress.jsonl` (appended), `manifest.json` (jobs file, counts,
  every model with its digest, GPU and Ollama version).
- **Resumable**: run the same command again and finished jobs (`<id>.json` with `ok: true`) are
  skipped; failed ones are tried again. If the connection drops, just run it again.
- `--concurrency` above Ollama's `OLLAMA_NUM_PARALLEL` (1 by default) only queues: on one GPU, 1–2
  for a big model. Several GPUs: split the jobs by `gpu` instead (below).

## Choosing GPUs

Every job and `warm`/`cold` take `gpu`: `all` (default — the user's own Ollama, every GPU),
`cpu`, or indices from `nib ai gpus`: `"1"`, `"0,2"`. Ollama picks its GPUs when its server
starts, so nib runs one more Ollama server per selection (its own port, only those GPUs
visible: `CUDA_VISIBLE_DEVICES` / `ROCR_VISIBLE_DEVICES`), starts it on first use and leaves it
running for the next job. `run` jobs are pointed at that server too. Apple Silicon has one GPU
and no CPU-only mode.

To use two GPUs for two models (or two halves of a corpus), give the jobs different `gpu`s.

## Install, download, delete

```sh
nib ai install                 # OpenCode and Ollama, whichever is missing (their own installers)
nib ai pull qwen3:8b           # download an Ollama model: Ollama's progress lines, then {"ok": true}
nib ai rm qwen3:8b             # delete it from disk
```

- They come together: `install` (no name, or `opencode`) sets up both. It prints the installers'
  output as `{"line": …}`. Ollama's Linux installer needs `sudo`: without a terminal that fails
  (`{"ok": false, "error": …}`, exit 1). Ask the user to press **Install** in the AI panel
  instead, where the password prompt is a terminal, or `nib ctl ai install OpenCode`.
- `pull`/`rm` take Ollama names (`qwen3:8b`; `ollama/qwen3:8b` works too). Names come from
  ollama.com/library. A pull is a few GB: check `nib ai models` (`ollama.disk`, each `size`)
  and ask before pulling something big, and before deleting a model you didn't pull yourself.
- A model's `size` is all its files; deleting frees that less any file another model shares
  (`ollama.disk` counts shared files once).
- Starts Ollama if it isn't running. With `@PROFILE`, all of this happens on that machine.

## Warm and cold

```sh
nib ai warm ollama/qwen3.8:27b-mlx [--gpu 1]   # load it, keep it loaded, prime it for OpenCode
nib ai cold ollama/qwen3.8:27b-mlx [--gpu 1]   # unload it
```

Warm loads the model and has it read OpenCode's own instructions (~7.6k tokens) twice, so an
OpenCode agent's first reply starts in seconds instead of ~45 s for a 27B model. It answers
when that's done (a minute or two). `generate` doesn't need priming.

## Rules

- Patient or other sensitive data stays on its machine: run with `@PROFILE` there instead of
  copying files here. Don't paste job results containing it into your own context; summarize
  with a script.
- Don't raise concurrency to "speed up" one GPU; check `nib ai gpus` and split by GPU.
- A job that fails every try is in `<id>_error.json` with `raw`; read that before changing prompts.

## The agents tab of a window (`nib ctl`)

A window's AI lives in its **agents tab**, in the editor area: several agents at once, each a
conversation with one assistant. Claude Code works as an agent there (it reads, edits and runs
commands in the project, within its mode); Codex and OpenCode too, one run per message (their
steps show when each ends); Grok only reads for now.

| | |
|---|---|
| `run toggle_ai` | show / hide the tab (agents keep working while it is hidden) |
| `run new_agent` | another agent: a blank one with the assistant / model dropdown open; then `ai use NAME` |
| `run next_agent` / `prev_agent` | which agent is shown (`state.ai.current`); `ai …` and `type` act on that one |
| `run kill_agent` | end the shown agent's conversation (a local model no other agent uses is unloaded) |
| `run panic` | pause every working agent (their processes are frozen) and open the dialog; `key space` unmarks the row the keyboard is on (`key down` / `up` move), `key enter` releases: marked agents stop, the rest carry on. `state.ai.panic` lists the rows. Only when the user asked |
| `run fold_agents` | hide / show the agent list |
| `ai mode manual\|accept-edits\|plan\|auto` | how much the shown agent may do unasked (`state.ai.mode`) |
| `ai allow` / `ai deny [WORDS]` / `ai answer A…` | answer what the shown agent asked leave for (`state.ai.ask`: `tool`, `what`, `questions`); deny's words say what to do instead. Its questions: `ai answer` with one answer per question, each an option's number or label, or words (`ai deny` skips them). Only when the user asked |
| `ai accept` / `ai keep` | the shown agent said its job is done (`nib-mail done`; `state.ai.chats[].finished`, `state.ai.accepting` while the dialog is up): accept, which ends the agent, or keep it. Only when the user asked |
| `ai image FILE` | attach a picture to the message being written, as pasting one does (`state.ai.images`) |
| `ai follow` / `ai unfollow` | the editor follows the shown agent: its file comes forward, its edit is selected, the tab closes when it moves on (`state.ai.follow`) |
| `ai skills` / `ai terminal` / `ai system` / `ai noside` | what the pane beside the steps shows (`state.ai.side`); `noside` hides it and gives the chat the width |
| `ai wpf [on\|off]` | worktree-per-feature, the WPF switch in the agents header (`state.ai.worktree_per_feature`; no argument flips it; remembered in config.toml). On: agents change files only on a branch of their own (`nib-work`) that lands as one commit. Only when the user asked |
| `ai effort LEVEL` | the shown agent's thinking level: `default` or one of `state.ai.efforts` (they depend on the assistant) |
| `ai system-prompt TEXT` | replace what the shown agent adds to its assistant's system prompt (`state.ai.prompt`; empty: none). Both are taken when its session starts; before the first message a change restarts it |
| `run agent_history` | the history tab: this project's past conversations, latest first (`state.ai.history.items`: `title`, `name`, `agent`, `steps`, `files`, `hit`). `type TEXT` there searches them |
| `ai reopen N` / `ai replay N` | reopen item N of that list as an agent (a message then continues it), or reopen it and walk the editor through its steps |
| `ai forget N` | delete item N for good. Only when the user asked for that conversation to go |

`state.ai` describes the shown agent as before, plus: `chats` (every agent: assistant, model,
`state` such as `thinking`, `in its terminal`, `editing a.rs`, `paused`, `idle`; `busy`, `frozen`),
`steps` (what the shown agent did, in order: `kind` = `command` / `read` / `edit` / `write` /
`skill` / `search` / `agent` / `other`, `what`, `running`, `ok`, `output`, `seconds`; a command
that waits has `what.wait` = `{seconds, timeout, counts_down}` and a `clock`), `mailbox` (what the
agents said to each other: see the **nib-agent-mailbox** skill). A message typed
while Claude Code works steers it: it reads it after its current step.

The rest of `ai` (see the nib-control skill for `nib ctl`):

| | |
|---|---|
| `ai use NAME [--model ID]` | continue the current conversation and work item with that assistant (`Claude Code`, `Codex`, `Grok`, `OpenCode`) and model, as picking it in the dropdown does; then `type` the message. Stop an active reply before switching. Across assistants, nib supplies recorded context without calling the outgoing model; within one assistant it resumes the native session. Use `run new_agent` for a separate conversation. `state.ai.messages` has the conversation (`from`: `user`, `assistant`, or `nib` for notes like a compaction), `busy` / `error` how the turn went. The message can start with a command (`/clear`, `/compact [focus]`, `/model [name]`) or a skill (`/name …` sends its SKILL.md first; `state.ai.skills` lists them with their group), and attach files (contents) or folders (the names in them) with `@path`; `state.ai.menu` is what the `/` / `@` menu offers for the text typed so far |
| `ai pick [NAME]` | open the assistant / model dropdown, on NAME's tab if given (to `snapshot` it; `state.ai.picking`) |
| `ai warm MODEL` / `ai cold MODEL` | the dropdown's ❄️ / 🔥 switch for a local OpenCode model. `state.ai.agents[].warm_models` lists the warm ones, `warming` those under way, `gpus` the GPUs |
| `ai update NAME` | update that assistant to its latest version in a terminal, as the Update button does |
| `ai install NAME` | the Install / Install Ollama button: installs it (OpenCode: with Ollama) in a terminal, where the user answers any password prompt |
| `ai pull MODEL` | the OpenCode tab's Pull field; `state.ai.pulling` = `{model, status, completed, total}` while it runs. `state.ai.agents[].ollama` has each model's `bytes` and the `disk_bytes` total |
| `ai rm MODEL` | a model's 🗑, without the window's confirm click. Only when the user asked for that model to go |
