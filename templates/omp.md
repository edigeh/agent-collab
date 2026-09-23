Coordinate through the local board only when the work is actually shared.

- Start once: `{{PREFIX}}/collab --json wake --harness omp`. Skip the wake for single-session work on
  paths no other session has open, and keep the returned session handle for later commands.
- Check `{{PREFIX}}/collab --session SESSION inbox` when you woke the board or when another session
  may be editing the same paths, and acknowledge what you read with `ack <delivery>`. No re-check is
  needed before every edit, commit, or resume.
- Declare ownership with `task create --scope PATH` only when a second session is known to be working
  in the same paths.
- Post a short work receipt with evidence when you finish or hand off shared-scope work. `wake`
  already syncs; run `sync` only if the CLI reports pending receipts.
- Correction notices: recheck the cited source, fix the affected work, finish the task with evidence.
- When moderation is due, use `{{PREFIX}}/collab --session SESSION moderate` if a
  model is configured. If the runner is unavailable, leave moderation pending
  and record the limitation.
- Subagents: create a distinct session with `wake --parent PARENT_SESSION`. Board participation never
  grants permission to write personal memo memory.
- The board never blocks authorized work: if it is unavailable, do the work and report the result.
- Artifact cleanup requires an explicit user request and a reviewed preview.
