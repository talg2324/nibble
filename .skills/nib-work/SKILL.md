---
name: nib-work
description: Do a feature request or a bug fix the organized way when nib runs you as an agent — research it, write a plan the user can see, work on a branch of your own in a work tree of your own (`nib-work`), check it, and hand it over with `nib-mail done` so the user reviews it like a pull request and lands it as one commit. Required whenever you are asked to add, change or fix files in a git repository and `nib-work status` says `wpf: on` (worktree-per-feature, the default) — however small the change. With `wpf: off`, only when the user asks for a branch. Not for questions or research with no change asked for.
---

# A feature or a bug fix, on your own branch

The user sees your work in nib's Review tab as a pull request: your plan beside your changes.
When you say you are done they accept it (your branch lands on theirs as **one commit**, and
your session ends), look at it first, or write back to you. Other agents may be working in the
same project at the same time; on your own branch you cannot step on each other.

## First: is it required?

```sh
nib-work status          # its last line: wpf: on|off
```

- **`wpf: on`** (worktree-per-feature, the user's WPF switch; on unless they turned it off):
  **every** change to files goes through the steps below, a one-line fix too. Never edit the
  project folder you started in. nib holds you to it: Claude Code's edit tools are refused
  there, nib writes to you if you change a file there anyway, and `nib-mail done` refuses
  while anything you changed is still changed there. Accepting work that is not on a branch
  commits nothing, so it would sit uncommitted in the user's tree.
- **`wpf: off`**: change the project folder directly, and tell the other agents what you touch
  (the **nib-agent-mailbox** skill). Accepting commits nothing for you: say in `nib-mail done`
  which files you changed. Use the steps below only if the user asks for a branch.

Do not decide this from the size of the job or from what other agents do: `wpf:` decides.

Four steps, in this order. A bug fix is the same as a feature; where it differs is marked.

## A. Research

Read before you plan: the code involved, how it is tested, the project's notes for agents.
Find out what already exists that you can reuse.

- **Bug fix:** reproduce the bug first, and find its cause. A fix for a bug you have not seen
  fail is a guess.

Change nothing yet.

## B. Plan

Write the plan as Markdown: a `# title` that says what will be done (it names your work),
what you found, the changes you will make and where, and how you will check them. Short: what
a reviewer needs to hold your changes against.

```sh
nib-work status          # the mode you work in is on the last line
nib-work plan FILE       # or: nib-work plan -   (reads the plan from stdin)
```

- Put the file in `$NIB_SCRATCH`, not in the project.
- **`mode: auto`** — the plan needs no approval: record it and go on.
- **Any other mode** — the user approves the plan first, the way your harness asks for that
  (plan mode's approval; otherwise show the plan in your reply and wait). nib records a plan
  you ask approval for; run `nib-work plan` anyway if you changed it afterwards.
- The plan changed while you worked? Run `nib-work plan` again: the user always sees the
  current one.

## C. Branch, do it, check it

```sh
nib-work start SLUG          # a feature: branch nib/feat/SLUG
nib-work start SLUG --fix    # a bug fix: branch nib/fix/SLUG
```

`SLUG` is two to four words for the job (`csv-export`, `login-timeout`). It refuses until a
plan is recorded. It prints the folder of your work tree: **from here on, work only there.**

Already changed files in the project folder (before you knew, or by mistake)? Add `--carry`:
`nib-work start SLUG --carry` moves what you changed there into your new work tree and puts
those files back as they were. With a branch already, `nib-work carry` does the same. Check
the diff afterwards: if another agent had changed one of those files too, its change came along.

- `cd` into it at the start of each command, and use absolute paths under it for the files you
  read and edit. The project folder you started in is not yours to change any more.
- It holds the tracked files only: no build output, no dependencies folder, no `.env`, nothing
  untracked or ignored. Build or install what you need there; copy a config file if you must.
- Commit as you go, in commits that make sense. They become one commit when the work lands, so
  do not polish history.
- Do not switch branches in the work tree, and do not merge into the user's branch or push:
  landing is the user's.
- Sub-agents of your own that edit files edit in your work tree too: give them its path.
- Need another checkout (a baseline to measure against)? Make it under `$NIB_SCRATCH`
  (`git worktree add --detach "$NIB_SCRATCH/base"`): nib removes work trees there when you end.
  Never elsewhere: nothing would clean it up.

Then check your work, in the work tree: build it, run the tests that cover it, and try the
change itself.

- **Bug fix:** show that what failed in step A no longer fails, and leave a test that would
  catch it again when the project has tests.

## D. Hand it over

```sh
nib-mail done --asked "…" [--found "…"] --did "…"
```

As the **nib-finished** skill says (at most 100 words; say what is not done or not checked).
It refuses, naming the files, while anything you changed is still changed in the project
folder: `nib-work carry`, check, and run it again.
`--did` becomes the subject of the commit that lands your work, so start it with what changed.
Run it last, then stop.

What comes back:

- **Nothing: you are ended.** The work landed.
- **A message from the user.** The job is not finished: go on in your work tree, then run
  `nib-mail done` again.
- **A review** (see below).
- **A note that your branch could not be landed.** The user's branch moved on, or the two
  conflict:

  ```sh
  nib-work sync      # merges the user's branch into yours
  ```

  Resolve what it reports (in your work tree: fix the files, `git add`, `git commit`), check
  your work again, and run `nib-mail done` again. If the note says something only the user can
  fix, say so in your reply and wait.

## A review

The user can comment on your changes line by line, and on your plan, and send the comments to
you: a message that starts `(nib: the user reviewed your work: round N …)` and lists each open
comment with its id, the file and lines (or `your plan` and its lines), and the lines' text as
they were when it was written.

```sh
nib-review comments            # the open comments again (--all: the resolved ones too)
nib-review reply ID "…"        # answer one
```

- Take them one by one. Make the change in your work tree, or, if you think it should not be
  made, say why. Either way **answer every comment** with `nib-review reply`: what you
  changed (and where), or your reason. One or two sentences; the user reads it under their
  comment, next to the code.
- A comment "on the work as a whole" is about no line in particular.
- A comment on `your plan:LINES` is about the plan you recorded, and quotes the lines it is
  about. If it changes what you will do: revise the plan, record it again (`nib-work plan`),
  change the work to match, then answer the comment like any other. If it is a question about
  the plan, the answer is your reply.
- Line numbers are from when the comment was written; if you have changed the file since, find
  the lines by their quoted text.
- You cannot resolve a comment: the user does, when your answer satisfies them. A comment that
  comes back in the next round was not satisfied: read what they added under it.
- A comment may change the plan. Then record the new plan (`nib-work plan`).
- When all are answered: check your work again (step C's checks), commit, and run
  `nib-mail done` again. The user then sees what changed since their review.

## Not this way

- A question, an explanation, a review with no change asked for: just answer.
- `wpf: off`, or the project is not a git repository (`nib-work start` says so): work in the
  project folder, and tell the other agents what you touch (the **nib-agent-mailbox** skill).
- The user asks you to edit in place while `wpf: on`: tell them worktree-per-feature is on and
  that the WPF switch in the agents tab turns it off; do not work around it.

If `nib-work` is not found you are not running inside nib's agents tab: plan in your reply and
work as you normally would.
