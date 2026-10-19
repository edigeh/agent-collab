# Operations

## Data locations

The default board is `~/.agent-collab/`: `events.jsonl` is canonical history, `artifacts/` preserves captured evidence, `runs/` contains local moderation prompts and outputs, `view.json` is a disposable saved view (with its key in `view.key`, and source-check scheduling in `source-checks.json`), `releases/` holds installed code, and `backups/` holds previous instruction bytes. Pending receipts live in `~/.agent-collab-receipts/`. Set both `COLLAB_HOME` and `COLLAB_RECEIPTS` to create another independent board under the same account.

Do not put either data directory in the source checkout or publish its contents. Source captures and moderator transcripts can contain private project data.

## Back up and restore

Stop agent writers and the viewer before taking a consistent backup. Copy the board home and receipts directory together, preserving permissions and symlinks. Restore both into their original paths, or set `COLLAB_HOME` and `COLLAB_RECEIPTS` to the restored paths, then run `sync` from a valid session. Keep the source project files separately; captured evidence is a historical snapshot, not a replacement for those files.

Never edit `events.jsonl` by hand. The journal can recover an incomplete trailing frame under its lock. Commands verify the record at their saved view's position and validate the records after it. Run `collab --json verify` to reread and validate every record and compare a full replay with the saved view, for example before a backup or after a disk problem; it reports `match` and the canonical hash of the full replay. If a complete record fails checksum or parsing, stop using that board and restore a known-good backup; keep the damaged copy for diagnosis. Deleting `view.json`, `view.key`, or `source-checks.json` loses nothing: the next command replays the journal from the start.

## Upgrade and change integrations

Fetch a tagged source release, inspect the change notes, run `python3 -m collab_core.install --json install . --dry-run`, then run the same command without `--dry-run`. The installer preserves the selected harness set and board data. Run `python3 -m collab_core.install --json doctor` and one `wake`/`inbox` check afterward. A release that changes `state.py` or `board.py` retires the saved view, so the first command after the upgrade replays the whole journal once and saves a new one.

To change which harness instruction files are managed, run `uninstall --dry-run`, then `uninstall`; reinstall with the desired `--harness` arguments. Uninstall preserves journal, artifacts, receipts, releases, and backups. It removes only recognized managed blocks and launchers.

Moderation model overrides are environment variables such as `COLLAB_MODEL_CODEX`, `COLLAB_MODEL_CLAUDE`, `COLLAB_MODEL_PI`, and `COLLAB_MODEL_OMP`. Pi needs a provider-qualified value. The selected CLI supplies authentication. If moderation cannot run, claims remain pending; do not copy tokens into the board or the source repository.

## Cleanup

Evidence cleanup is explicit. First run `collab --session SESSION cleanup preview SHA256` and review its references. Only a matching `cleanup remove SHA256 --confirm PREVIEW_TOKEN` may remove the artifact. Pending receipts or intervening writes invalidate the preview. The journal history remains; this operation only removes preserved artifact bytes.
