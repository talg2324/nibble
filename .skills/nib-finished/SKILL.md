---
name: nib-finished
description: Tell the user, through nib, that the job you were given is done — a short summary of what was asked, what you found and what you did, which the user accepts (ending your session) or answers with what is left. Use only when you were given a clear job with an end, and you believe it is complete. Not for open-ended research, questions, progress reports, or asking the user to look at something.
---

# Saying the job is done

When nib runs you in its agents tab, you can tell the user that the job they gave you is
finished. Your row in their list turns green, a green summary appears in your chat, and when
they come to you they are asked: accept (your session ends) or keep working (they write to you
again).

```sh
nib-mail done --asked "WHAT YOU WERE ASKED" [--found "WHAT YOU FOUND OUT"] --did "WHAT YOU DID"
```

- **At most 100 words, all three parts together.** Over that, the command refuses: shorten and
  run it again.
- `--asked`: the job in one sentence, as the user meant it, not their whole message.
- `--found`: only when you learned something they need to know (the cause of a bug, a surprise,
  a limit). Leave it out otherwise.
- `--did`: what changed — files, commands, results of the checks you ran. Say plainly what is
  not done or not verified.

Run it as the last thing in your turn, after your normal reply, then stop. If the user writes
to you again, the job is not finished: go on, and run `nib-mail done` again when it is.

## When — and when not

Use it only when **all** of these hold:

1. You were given a **clear job with an end**: fix this bug, add this feature, write this file,
   run this migration.
2. You did it, and checked it as far as you can (it builds, the test passes, you looked at the
   result).
3. Nothing is left for you to do without the user.

Do **not** use it when:

- the work is open-ended — research, exploring, "look into", "what do you think", a review
  with no change asked for;
- you are asking the user something, waiting for a decision, or need them to look at or open
  something (just say so in your reply);
- you are reporting progress partway through, or stopped because you are blocked;
- you were told no job at all (a question, a chat).

If you are unsure whether the job is finished, it isn't: say what is left in your reply instead.

## Example

```sh
nib-mail done \
  --asked "Make the agents chat box scroll to the caret when it is full." \
  --found "The box had a fixed height but no scrollable around it, so new lines went below the edge." \
  --did "Wrapped it in a scrollable that follows the caret (crates/nib/src/app/ai/stage.rs). Built and checked with nib ctl: typing 30 lines keeps the caret in view."
```

If `nib-mail` is not found you are not running inside nib's agents tab: just say in your reply
that you are done.
