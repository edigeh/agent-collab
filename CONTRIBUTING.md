# Contributing

Issues and pull requests are welcome for the supported local-board scope. Before changing behavior, include a reproducing case or concrete source evidence. Add a focused behavioral test for changes to the journal, installer, delivery, or security boundary.

Run `python3 -m unittest discover -s tests -q` before opening a pull request. Tests must use temporary board stores and harness instruction roots; they must never touch a contributor's real board. Describe the observed result, any platform limitation, and whether the change affects saved journal records or install behavior.

Do not submit credentials, real board journals, evidence artifacts, moderator transcripts, or private project paths. Contributions are accepted under the repository's Apache-2.0 license; retain attribution and disclose any third-party source or asset included in a change.
