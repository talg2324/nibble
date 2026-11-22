---
name: nib-agent-mailbox
description: Talk to the other agents working in the same project at the same time — say what you are doing and which files you will change, agree on an order, wait for an answer. Use when nib tells you other agents are at work here, when a message from another agent arrives, before editing a file another agent is changing, and when you finish something others depend on.
---

# The agents' mailbox

You may not be the only agent working in this project right now. With worktree-per-feature on
(`nib-work status` says `wpf: on`, the default), each agent that changes files does it on a
branch of its own (the **nib-work** skill), so you do not overwrite each other; the mailbox is
for agreeing on who does what, and for news others depend on. With it off, several agents,
from different assistants, share one working tree: if two of you change the same file without
talking, one overwrites the other. The mailbox is how you avoid that.

nib tells you when it matters: who else is at work when you start, and when two of you touch
the same file. You never need to check for mail: nib shows it to you. A message written to you
by name comes after your current step, and wakes you if you are idle. News sent to `all` never
interrupts anyone: it waits and comes with your next message.

## Commands

Run them as ordinary shell commands (`nib-mail` is on your PATH):

| | |
|---|---|
| `nib-mail who` | who is at work: name, assistant, working / idle, what each was asked to do, and the files each has changed. You are marked `(you)` |
| `nib-mail send NAME "text"` | tell or ask one agent. `all`: news every other agent sees later, never a question |
| `nib-mail wait --from NAME` | do nothing until NAME writes to you, then print it. Gives up after 100 s (`--timeout SECONDS`); run it again if you still need the answer |
| `nib-mail inbox` | messages you haven't seen, without waiting (rarely needed) |

Names come from each agent's task (`fix-calc-sum`, `write-tests`). Take them from `mail who`,
not from memory. An agent that has not been given a task yet is not at work: it is not listed
and gets no mail.

If `nib-mail` is not found you are not running inside nib's agents tab: there is no mailbox.

## When to send

Write to as few agents as you can. Every message costs the reader a turn.

1. **When you start and will change files**: run `nib-mail who`. Write by name only to the
   agents whose changed files, or whose task, overlap what you expect to change: what you will
   do and which files. If nobody overlaps, write to nobody. Reading, research and answering
   questions need no message at all.
2. **Before changing something another agent announced or is changing**: ask it, and agree who
   goes first. Usually the one already in the file finishes; the other waits
   (`mail wait --from NAME`) and then reads the file again before editing.
3. **When you change something others depend on** (a function's name or signature, a file moved,
   a config key): say exactly what changed: to the agents that use it, or to `all` if you
   can't tell who does.
4. **When you are done** with what you told someone: tell the agents you talked with, so
   anyone waiting can go on. Not `all`.

## How to write

- Short and exact: paths, names, what you will do next. One message, not a conversation.
- Say what you need from the other, if anything: "wait until I say done", "tell me when the
  test passes".
- Answer a question you were asked before going on with your own work. Don't answer news
  ("I'm editing X", "done with X") unless it collides with what you are doing: a reply wakes
  the other agent for nothing.
- Don't use the mailbox to hand your task to someone else, to chat, or to report to the user:
  the user reads your normal replies (and sees every message in the mailbox too).

## Waiting

When you can't go on until another agent has finished, don't loop and don't sleep: run
`nib-mail wait --from NAME`. The user sees that you are waiting and for whom. If it gives up,
check `mail who`: if the other agent is idle or gone, go on and say so in your reply.
