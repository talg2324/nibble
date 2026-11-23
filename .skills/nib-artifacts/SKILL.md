---
name: nib-artifacts
description: Where to put files you create while working that are not source changes — scratch scripts, intermediate data, logs, output data, plots, images, screenshots, reports. Read before writing any such file.
---

# Where your files go

Sort every file you are about to write into one of three kinds:

| Kind | Examples | Where |
|---|---|---|
| **Scratch**: only you need it | throwaway scripts, intermediate data, your own run logs, test fixtures | **scratch** directory |
| **Artifact**: the user will look at it | plots, images, output CSV/JSON, reports, screenshots | **NIBDIR** |
| **Deliverable**: part of the project | code, docs, data the project tracks | the project, and only when the user asked for it |

## Finding the directories
Inside a nib terminal (`$TERM_PROGRAM` is `nib`), nib provides both, on the machine where the
code lives:

- **`$NIB_SCRATCH`**: this session's scratch directory. Deleted about a day after the session
  ends. An agent nib started gets a folder of its own in it; git work trees made there (a copy
  of the code to compare against) are removed when the agent ends.
- **`$NIBDIR`**: this project's persistent directory, outside the repo. It survives sessions:
  look in it first, since earlier work may already be there.

The user sees both in nib's file tree, bottom left (`scratch`, `nibdir`), and can open what you
put there directly. nib's viewers show images, CSV/TSV, Markdown and HTML.

Outside nib, or when these aren't set:
- scratch → the scratch or temp directory your harness gives this session, else one `mktemp -d`
  per task, created once and reused;
- artifacts → an `artifacts/` folder in that same directory. Tell the user it is temporary, and
  ask where to keep anything that should last.

## Showing a picture in your reply
In nib's agents tab, a Markdown image in your reply is drawn in the chat: write
`![loss by epoch](/abs/path/loss_by_epoch.png)` on its own line. Use the absolute path (a relative
one is read from the project root). PNG, JPEG, GIF, WebP, TIFF, BMP and SVG work; web URLs don't. It is
shrunk to fit the chat, and the user clicks it to open it full size. Show a plot this way rather
than only naming its file.

## Rules
- **Never** write scratch files or artifacts into the repo, the user's home directory, or loose
  in `/tmp` (`/tmp/out.png` collides with other runs, and on Linux `/tmp` is shared with
  other users).
- **Organise NIBDIR.** It is shared by every session on the project. Use a folder per topic or
  task (`NIBDIR/loss-curves/`, `NIBDIR/2026-11-20-profiling/`), not a flat pile.
- **Name files for what they are.** `loss_by_epoch.png`, not `out.png`. Don't overwrite an artifact
  the user may have seen: add `_v2`, or a short suffix saying what changed.
- **Give the full absolute path** whenever you mention a file.
- **Don't copy large data around.** Point to where it already is. Write derived data only when you
  need it.
- **Leave other tools' files where those tools put them.** nib's snapshots and logs stay in its run
  directory (see the `nib-control` skill). Refer to them by path.
- **Promote, don't hoard:** when a scratch file turns out to matter, move it to NIBDIR. Move
  something from NIBDIR into the project only when the user asks.
