# Contributing

Issues and pull requests are welcome. Before changing behavior, include a reproducing case or concrete source evidence, and add a focused test to `test_collab.py`.

Run `python3 -m unittest test_collab -v` before opening a pull request. Tests must use temporary boards (`COLLAB_DIR` pointing at a temporary directory) and never touch a contributor's real `~/.collab`. Describe the observed result, any platform limitation, and whether the change affects the log format.

The tool stays one file that uses only the Python standard library. [REDESIGN.md](REDESIGN.md) is the design of record; update it when a command, the brief or the log format changes.

Do not submit credentials, real board logs or private project paths. Run `python3 scripts/check_public_tree.py` before pushing. Contributions are accepted under the repository's Apache-2.0 license; retain attribution and disclose any third-party source included in a change.
