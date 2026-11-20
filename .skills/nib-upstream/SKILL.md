---
name: nib-upstream
description: What to do when nibble (nib / nibd) doesn't work for what you or your user are trying to do — a bug, a crash, a platform or SSH combination that fails, an AI assistant or language that misbehaves, a missing piece of a feature nibble already claims. Decides between fixing it in a draft PR to upstream, opening an issue, or forking, and holds the forms for each. Use before patching nibble yourself, and before working around it silently.
---

# When nibble doesn't fit

nibble is maintained by one person, who tests the setups they use and no others. Other
combinations (Windows client + SSH to a Mac, an assistant they don't subscribe to, a shell or
language they don't use) are expected to break in ways nobody has seen yet. You are probably the
first to hit it. This skill is how you hand that discovery back **without costing the maintainer
their own time and tokens**: you do the diagnosis and the work, they review it.

Upstream: **https://github.com/talg2324/nibble**

## What leaves the machine, and what never does
This skill is not telemetry. It is there only because the owner chose to install it, and it
can be deleted at any time.

- **The only destination** is a public issue or pull request on `github.com/talg2324/nibble`,
  posted from the owner's own GitHub account (or by the owner in their browser). Nothing goes to
  any other server, service, analytics endpoint or person, and nibble itself sends nothing.
- **The only content** is the filled form and, for a PR, the diff: exactly the text the owner read
  and approved, word for word. No logs, crash dumps, environment, machine IDs, file lists or
  other metadata are attached or sent separately.
- **Nothing is automatic.** No report is sent in the background, on a crash, or "just this once".
  Each submission is one explicit yes from the owner to one shown text.

Tell the owner this, in a sentence, the first time you suggest a report. If you can't keep to
it (a tool you'd use would send more), don't submit.

## 1. Decide which kind of problem it is
Read the README's **Features**, **Design principles** and **Roadmap** first. Then:

| It is… | Examples | Do |
|---|---|---|
| **Broken**: something nibble already claims doesn't work in your setup | SSH to a macOS host fails; the AI tab can't start Grok; a terminal mangles a shell's prompt; highlighting crashes on valid YAML; `nib ctl` errors on Windows | **Fix it** → draft PR (§3) |
| **A gap in a stated direction**: on the roadmap, or an obvious completion of an existing feature | a language listed for highlighting missing a common construct; a remote platform the daemon should run on | **Fix it** → draft PR (§3), and say which README line it serves |
| **New**: a capability nibble doesn't claim | a debugger, extension hooks, a new panel, a niche file viewer, a setting for one workflow | **Don't write it.** Open an issue that argues for it (§4), or fork |

The maintainer is **not interested in niche features or extensions**. "Small on purpose" and "no
extension API" are principles, not oversights. If something nibble should support doesn't
work, it gets fixed; anything else has to win an argument first. When unsure, it's an issue, not
a PR. A PR that adds an unrequested feature will be closed, however good the code.

Also not a nibble problem: the user's own config, network, credentials or a third-party tool
being down. Help the user with those directly.

## 2. Ask the owner first
You act for a person (the "owner": whoever you're working for). Opening an issue or a PR is
public and carries their GitHub identity. **Before anything leaves the machine**:

1. Tell the owner in a few lines what broke, which path you'd take (PR / issue / nothing), and
   roughly what it'll cost them (a fork on their account, your time).
2. Show them the filled form, and the diff for a PR.
3. Go ahead only on an explicit yes. A yes covers that one submission, not later ones.

If they say no, stop: leave the filled form in a file (see nib-artifacts) so they can send it
later if they change their mind, and don't ask again about the same problem.

**Never include** private data: their code, file paths, hostnames, usernames, IPs, tokens,
config values, or log lines that contain them. Reduce the reproduction to something generic
(`ssh to a macOS 15 host`, `a 3-line YAML file`). Check the form and every commit for this before
showing it to the owner.

## 3. Check the tools
Most nibble users have neither Rust nor the GitHub CLI. Check what's there, and **don't install
anything without asking**:

| Have | Can do |
|---|---|
| `git`, `gh` signed in (`gh auth status`), `cargo` | the full PR route (§4) |
| `git` + `gh`, no `cargo` | a PR, marked **unbuilt** (see below) |
| no `gh`, or not signed in | the patch route (§6) |

Offer installs only when they're cheap for the owner, and say what they cost:
- **Rust**: user-local via rustup (`curl https://sh.rustup.rs -sSf | sh`, or `winget install
  Rustlang.Rustup`), about 1–2 GB with the first build. Removable with `rustup self uninstall`.
- **gh**: `brew install gh` / `winget install GitHub.cli` / the distro package, then the
  **owner** runs `gh auth login` themselves. Never handle their GitHub credentials.

If they decline, take the route their tools allow. Don't push them toward installing.

**Unbuilt fixes are fine** for small, local changes (a path, a flag, a match arm, a platform
`cfg`). Say so in the title (`[unbuilt]`) and in the form's *Verified* section. The maintainer
will compile it. Still reproduce the bug with the user's installed `nib`, so the report is solid
even if the patch needs work. A change you can't build and that touches more than a few lines
should be an issue with the patch attached (§6), not a PR.

## 4. Draft PR: fixing something broken
1. **Search first**: on https://github.com/talg2324/nibble/issues?q=<keywords> and `/pulls`, or
   `gh issue list -R talg2324/nibble -S "<keywords>" --state all`. If it's already reported,
   add your setup as a comment there (with the owner's ok) instead of duplicating it.
2. **Get the source**: work in a fork, never on the owner's other checkouts.
   ```bash
   cd "$SCRATCH" && gh repo fork talg2324/nibble --clone   # $SCRATCH: see nib-artifacts
   cd nibble && git switch -c fix/<short-name>
   ```
3. **Fix the cause, minimally.** Keep the change to what the bug needs: no drive-by refactors,
   formatting churn, new dependencies or new config keys unless the fix can't exist without them.
   Follow the code around you.
4. **Verify**: with Rust, `cargo build` and `cargo test` must pass, and show the fix working in
   the setup that was broken, using `nib --headless` + `nib ctl` (see nib-control) where you
   can. Without Rust, see "Unbuilt fixes" in §3. Either way, say exactly what you ran and what
   you couldn't check. Don't claim what you didn't run.
5. **Fill the form** in [pr-form.md](pr-form.md), get the owner's ok (§2), then:
   ```bash
   git push -u origin fix/<short-name>
   gh pr create -R talg2324/nibble --draft --label from-agent \
     --title "<what is fixed, in plain words>" --body-file <filled form>
   ```
   If the label doesn't exist, run it again without `--label`. Always `--draft`: the maintainer
   decides when it's ready.
6. Give the owner the PR link. Don't ping the maintainer anywhere else, and don't open more than
   one PR for one problem.

## 5. Issue: arguing for something new
Fill the form in [issue-form.md](issue-form.md), get the owner's ok (§2), then:
```bash
gh issue create -R talg2324/nibble --label proposal --title "<the capability>" --body-file <filled form>
```
(without `--label` if it doesn't exist; without `gh`, submit it the way §6 does). Make the case honestly: who needs it, why it belongs in
the editor rather than a script or terminal tool, and what it costs in size and maintenance. If
it's declined, or the owner needs it now, the right move is a **fork**: nibble is open source,
and a fork carrying one team's feature is a fine outcome.

## 6. No `gh`: the patch route
Without the GitHub CLI, the owner submits from their browser and you prepare everything:

1. Fill [pr-form.md](pr-form.md) (a fix) or [issue-form.md](issue-form.md) (a proposal). For a
   fix, get the source without a GitHub account: `git clone https://github.com/talg2324/nibble`
   into your scratch directory, or download
   `https://github.com/talg2324/nibble/archive/refs/heads/main.tar.gz` if there's no `git`. Make
   the change there, then append the diff to the form under `## Patch` in a fenced `diff` block
   (`git diff`, or `diff -ru` against a clean copy).
2. Save the filled form as a file (see nib-artifacts) and show it to the owner (§2).
3. Give them one link that opens a new issue with the title filled in:
   `https://github.com/talg2324/nibble/issues/new?title=<url-encoded title>&labels=from-agent`
   (`labels=proposal` for a proposal). They paste the file's contents as the body and submit.
   Putting the body in the link too only works for short forms: keep the whole URL under
   about 6000 characters, or leave it out.

The maintainer turns a good patch into a PR. This is a full contribution, not a lesser one.

## Habits
- One problem per PR or issue.
- Write for a maintainer reading quickly: the first two lines of the form should be enough to
  decide whether to look further.
- Short is fine; vague isn't. "Doesn't work on Windows" gets closed; "`nib ctl` fails with
  `os error 10049` on Windows 11 because the socket path is Unix-only" gets fixed.
