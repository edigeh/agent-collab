# agent-collab in Rust

A port of the `collab` CLI, the web viewer and the installer. It reads and writes the same board as the Python build (`events.jsonl`, receipts, artifacts, `source-checks.json`, `view.key`), so both can be used on one board at the same time. The only file of its own is `view.rsnap`, its saved view.

The design is described in the module docs.

## Build

```
cargo build --release
```

Needs a Rust toolchain and a C compiler (mimalloc, BLAKE3 and the SHA-256 assembly are built with `cc`; on macOS the Xcode command line tools provide it). The binaries land in `target/release/`: `collab`, `collab-viewer`, `collab-install`.

## Install

```
target/release/collab-install install target/release
```

Same prefix layout and launchers as the Python installer (default prefix `~/.agent-collab`, with the `collab` and `viewer` launchers in it). To go back, run the Python installer again; the board needs no migration either way.

## Tests

```
cargo test --release                       # unit tests
python3 tests/parity/test_mixed.py         # Python and Rust writing one board
python3 tests/parity/test_viewer.py        # viewer JSON and HTTP checks against Python
python3 tests/parity/test_install.py       # install, doctor, rollback, uninstall
python3 tests/parity/test_git.py           # repository discovery against git rev-parse
python3 tests/parity/scenario.py python A.json; python3 tests/parity/scenario.py rust B.json
```

The parity tests import the Python package from the parent directory and use `target/release` (override with `COLLAB_RUST_BIN`). The Python test suite in `../tests` also runs against the binary when `python3 -m collab_core` is pointed at it.

## Environment

Same variables as the Python CLI (`COLLAB_HOME`, `COLLAB_RECEIPTS`, `COLLAB_SESSION`, `COLLAB_HOST_PID`, `COLLAB_MODEL_*`, `COLLAB_MODERATING`), plus `COLLAB_NATIVE_GIT=0` to always ask `git rev-parse` instead of reading `.git` directly.
