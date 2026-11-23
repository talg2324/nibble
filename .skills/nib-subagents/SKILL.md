---
name: nib-subagents
description: What "an agent" or "a subagent" means when nib runs you — your own harness's subagents (invisible in nib's agent list, working in your tree) versus a nib agent (its own row, branch and review). Use whenever the user asks you to spin up, spawn, start or hand work to a subagent or another agent, and before you start a subagent of your own for anything that changes files. Says which one the user means, how to start each, and what to tell them.
---

# Subagents and nib agents are not the same thing

Inside nib there are two things a user may call "an agent" or "a subagent". They look alike
from where you sit and nothing alike from where the user sits.

| | Your own subagent | A nib agent |
|---|---|---|
| What it is | Your harness's helper (Claude Code's Agent / Task tool, and the like), inside your session | Another conversation in the window's **agents tab** |
| What the user sees | One step in **your** chat (`agent`, with its tool calls under it). **No row in the agent list, nothing in the dashboard count** | Its own row, name, state, chat and steps |
| Where it works | Your folder, your branch: its edits are your edits | Its own branch and work tree, once it runs `nib-work start` |
| Who it answers to | You. The user cannot write to it, steer it or stop it by itself | The user. It reports with `nib-mail done`; they review its work in the Review tab and accept it |
| How you talk to it | Your harness | `nib-mail` (the **nib-agent-mailbox** skill) |

## Which one the user means

A user of nib who says "spin up a subagent", "have an agent fix this", "hand this to another
agent" is looking at the agents tab. **Unless they say otherwise, they mean a nib agent** — one
they can see, steer, review and accept — when the work:

- changes files in the project (a fix, a feature, a refactor), or
- is something they will want to watch or review by itself, or
- should not block the conversation they are having with you.

Your own subagents are for what stays yours: searching and reading across the code, checking
your work, a side question whose answer is all that matters. Nobody needs to see those.

If you cannot tell which is meant, ask once, in one sentence, before starting anything:
"As an agent of its own in your agents tab, or as a helper inside my session (you would see it
only as a step in my chat)?"

## If you use your own subagent

Say so in the same reply, in these terms: it runs inside your session, it shows as a step in
your chat and not in the agent list, and its changes land where yours do. Never call it just
"a subagent" and leave the user looking for a row that will not appear.

## Starting a nib agent

The new agent knows nothing of your conversation. Write it a brief that stands alone: the goal,
what you found, the files, what is already ruled out, how to check the result, and that it
should follow the **nib-work** skill (plan, own branch, `nib-mail done`).

Its work tree holds **committed files only**. If the job builds on changes that are not
committed yet (yours or the user's), say so to the user first: they commit them. Only with
worktree-per-feature off (`nib-work status`: `wpf: off`) may the agent be told to work in the
project folder instead.

Then, one of:

- **`nib` is on your PATH and the window is on this machine** (not over SSH): drive the window,
  which the user has just asked for. Find it by its root, never "the newest one":

  ```sh
  nib ctl list                              # the window whose "root" is $NIB_ROOT
  nib ctl -p PID run new_agent              # a blank agent, the dropdown open
  nib ctl -p PID ai use "Claude Code"       # or Codex, OpenCode; --model ID to pick one
  nib ctl -p PID type 'THE BRIEF\n'         # \n sends it
  nib ctl -p PID state                      # ai.chats: the new row and its name
  ```

  This leaves the new agent in front in the user's window. Tell them its name.
- **Otherwise** (no `nib` command, or you run on an SSH host): you cannot start one. Give the
  user the brief in your reply and ask them to press **+ new agent** and paste it.

After that it is the user's agent, not yours:

- Do not do its job as well. If your work touches the same files, agree by mailbox
  (`nib-mail who`, `nib-mail send NAME "…"`).
- It hands its result to the user, not to you. If you need to know when it is done or what it
  found, ask it by mailbox and `nib-mail wait --from NAME`.
- Tell the user what you handed over and what you kept.

## Stopping one of your own

If the user wanted a nib agent and you already started a subagent of your own: say so at once,
and ask whether to let it finish or stop it and start the other. Before starting the nib
agent, account for whatever your subagent already changed (keep it and say so, or undo it) —
two workers must not be left on the same files.
