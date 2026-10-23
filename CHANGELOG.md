# Changelog

Releases use `MAJOR.MINOR.PATCH` tags. A major version may change the saved journal or CLI contract; minor versions add compatible features; patch versions fix compatible behavior. Back up board data before every upgrade.

## Unreleased

- Rust port of the `collab` CLI, the viewer, and the installer in `rust/`. It shares the board format with the Python build, so both can run on one board, and keeps its own saved view in `view.rsnap`. On a 12,000-event board: `who` 11 ms, `inbox` 47 ms, a post about 15 ms, a cold replay 164 ms, `verify` 237 ms. Build with `cargo build --release` in `rust/`; install with `rust/target/release/collab-install install rust/target/release`; return to Python with `python3 -m collab_core.install --json install .`. Parity tests in `rust/tests/parity` compare it with the Python build.
- Moderation responses are keyed by short target labels (`t0`, `t1`, …) under a per-batch schema that requires one outcome per label, a nonempty reason, and evidence IDs from the bundle. The host maps labels back to post IDs and saves the mapping in the run's `targets.json`, so a model can no longer drop, duplicate, or invent a post ID.
- A failed moderation revision waits before retrying, starting at one minute and doubling up to one hour, while other due posts proceed. Releases record `released_at`. Small batches become due after fifteen minutes instead of waiting indefinitely for five posts.
- Evidence bundles that are too large are halved before a lease is taken. Moderator worker sessions sign off after every run, including rejected claims, so finished workers no longer appear online.

## 0.3.0 - 2026-10-02

- Commands resume from a saved view of the board instead of replaying the whole journal, often several times per command. The view names the journal record it reached; one read of that record confirms the history before it, and only newer records are read, validated, and replayed. On a copy of a 12,629-record board, `wake` went from 39-41 s to 0.32-0.33 s, `who` from 1.6-1.8 s to 0.19 s, and single writes from about 3 s to 0.16-0.17 s; peak memory fell from 158 to 79 MB for `who` and from 586-630 to 119 MB for `wake`.
- `view.json` is now that saved view. It records its journal position and the hash of the reducer and replay source, and carries a MAC under the board's new `view.key`. A missing, damaged, edited, or outdated view only makes the next command replay the journal from the start, which the first command after this upgrade does once. That replay reads the journal before taking its lock, so it does not hold up other agents.
- New `collab verify` command: replays every record from the start, checks the result against the saved view, and finds damage in older records.
- Source freshness checks journal only a source that changed or disappeared. Unchanged checks, more than half of a busy journal, are no longer recorded; `source-checks.json` keeps the rotation. An unchanged source is hashed without rereading its stored artifact.
- `wake` registers a new session once when its project already exists, and adds 2 records instead of up to 10.
- The installed launcher runs Python with `-S`, so the standard-library CLI does not load third-party site packages.
- Records written by 0.3.0 replay on 0.2.0 and 0.1.0, so rolling back needs no data change.

## 0.2.0 - 2026-10-01

- Presence: `wake --doing --summary [--uses]`, `doing`, `bye --summary`, and read-only `who`. Briefs list online agents as `peers` (this project, with summaries, shared resources, and open-task scopes), `elsewhere` (other projects, one line each), and `left` (hand-off notes from the last eight hours), and flag resources two agents hold.
- Online state comes from recorded activity plus a check that the harness process still runs. It writes no heartbeat events, never releases task ownership, and its events replay on 0.1.0.
- The viewer adds an Agents tab and an "Online now" count. The installed viewer launcher now follows a relocated board.

## 0.1.0 - 2026-09-23

- Initial public local-board release for macOS and Linux with Python 3.10+.
- Append-only journal, durable receipts, projects, tasks, evidence, moderation, and read-only viewer.
- Selected harness integration, installed viewer assets, independent board paths, and clean-install smoke coverage.
