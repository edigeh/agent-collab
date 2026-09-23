"""Check the exact Git index for private runtime data and common credential forms."""
from __future__ import annotations

import re
import subprocess
import sys


FORBIDDEN_DIRECTORIES = {b"work", b".agent-collab", b".agent-collab-receipts",
                         b"artifacts", b"runs", b"backups"}
FORBIDDEN_FILES = {b"events.jsonl", b"view.json", b".DS_Store"}
CONTENT_PATTERNS = {
    "home path": re.compile(rb"/(?:Users|home)/[A-Za-z0-9._-]+/"),
    "private key": re.compile(rb"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
    "GitHub token": re.compile(rb"gh[pousr]_[A-Za-z0-9_]{20,}"),
    "OpenAI-style key": re.compile(rb"sk-[A-Za-z0-9_-]{20,}"),
    "AWS access key": re.compile(rb"AKIA[0-9A-Z]{16}"),
}


def main() -> int:
    names = subprocess.check_output(["git", "ls-files", "-z"]).split(b"\0")
    findings = []
    for name in filter(None, names):
        parts = name.split(b"/")
        if any(part in FORBIDDEN_DIRECTORIES for part in parts[:-1]) or parts[-1] in FORBIDDEN_FILES:
            findings.append((name, "private runtime path"))
            continue
        content = subprocess.check_output(["git", "show", ":" + name.decode()])
        for label, pattern in CONTENT_PATTERNS.items():
            if pattern.search(content):
                findings.append((name, label))
    for name, label in findings:
        print(f"{name.decode(errors='replace')}: {label}")
    if findings:
        return 1
    print(f"Public tree check passed: {len(names) - 1} staged files")
    return 0


if __name__ == "__main__":
    sys.exit(main())
