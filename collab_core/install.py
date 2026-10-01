"""Idempotent local installation of agent-collab and managed harness notes."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import shlex
import shutil
import stat
import sys
import tempfile

from .board import atomic_file, canonical, digest


MARKER_BEGIN = b"<!-- agent-collab:begin v1 -->"
MARKER_END = b"<!-- agent-collab:end -->"
LAUNCHER_MARKER = "# agent-collab managed launcher v1"
VIEWER_MARKER = "# agent-collab managed viewer v1"
SOURCE_FILES = ("collab",)
TEMPLATE_NAMES = ("codex", "claude", "pi", "omp")
INTEGRATION_FILE = "integration.json"


class InstallError(ValueError):
    pass


def default_entries(home=None):
    home = Path(home or "~").expanduser()
    return {
        "codex": home / ".codex" / "AGENTS.md",
        "claude": home / ".claude" / "CLAUDE.md",
        "pi": home / ".pi" / "agent" / "AGENTS.md",
        "omp": home / ".omp" / "agent" / "AGENTS.md",
    }


def _source_root(source):
    root = Path(source).expanduser().resolve(strict=True)
    if not (root / "collab_core").is_dir() or not (root / "collab").is_file():
        raise InstallError("source must contain collab_core/ and collab")
    return root


def _entries(entries):
    values = default_entries() if entries is None else entries
    if not isinstance(values, dict) or not set(values).issubset(TEMPLATE_NAMES):
        raise InstallError("entries must use only " + ", ".join(TEMPLATE_NAMES))
    return {name: Path(values[name]).expanduser().absolute() for name in TEMPLATE_NAMES if name in values}


def _saved_entries(prefix):
    path = prefix / INTEGRATION_FILE
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        if data.get("version") != 1:
            raise ValueError("unsupported integration version")
        return _entries(data["entries"])
    except (OSError, ValueError, KeyError, TypeError) as exc:
        raise InstallError(f"invalid integration manifest: {path}") from exc


def _select_entries(prefix, entries=None, harnesses=None):
    saved = _saved_entries(prefix)
    if entries is not None and harnesses is not None:
        raise InstallError("use entries or harnesses, not both")
    if entries is not None:
        chosen = _entries(entries)
    elif harnesses is not None:
        unknown = set(harnesses) - set(TEMPLATE_NAMES)
        if unknown:
            raise InstallError("unknown harness: " + ", ".join(sorted(unknown)))
        defaults = default_entries()
        chosen = _entries({name: defaults[name] for name in harnesses})
    elif saved is not None:
        chosen = saved
    elif (prefix / "current").exists():
        # Existing pre-selection installs managed all four templates.
        chosen = _entries(None)
    else:
        defaults = default_entries()
        chosen = _entries({name: defaults[name] for name in TEMPLATE_NAMES if shutil.which(name)})
    if saved is not None and chosen != saved:
        raise InstallError("installed harness selection differs; uninstall before changing it")
    if saved is None and (prefix / "current").exists() and chosen != _entries(None):
        raise InstallError("legacy installation manages all harnesses; uninstall before changing the selection")
    return chosen


def _template(name, prefix):
    path = Path(__file__).resolve().parents[1] / "templates" / (name + ".md")
    body = path.read_text(encoding="utf-8").replace("{{PREFIX}}", str(prefix))
    if "{{PREFIX}}" in body:
        raise InstallError("unexpanded template placeholder")
    return b"\n" + MARKER_BEGIN + b"\n" + body.encode("utf-8") + b"\n" + MARKER_END + b"\n"


def _managed_span(raw, path):
    begins = raw.count(MARKER_BEGIN); ends = raw.count(MARKER_END)
    if begins == 0 and ends == 0:
        return None
    if begins != 1 or ends != 1:
        raise InstallError(f"duplicate or malformed managed markers: {path}")
    begin = raw.index(MARKER_BEGIN); end = raw.index(MARKER_END)
    if end < begin:
        raise InstallError(f"malformed managed marker order: {path}")
    end += len(MARKER_END)
    if end < len(raw) and raw[end:end + 1] == b"\n":
        end += 1
    # Our block owns its preceding separator.  This lets uninstall restore a
    # file that originally lacked a final newline without touching user bytes.
    start = begin - 1 if begin and raw[begin - 1:begin] == b"\n" else begin
    return start, end


def _read_instruction(path):
    if path.exists():
        if not path.is_file() or path.is_symlink():
            raise InstallError(f"instruction path is not a regular file: {path}")
        return path.read_bytes(), stat.S_IMODE(path.stat().st_mode)
    return b"", 0o600


def _atomic_text(path, raw, mode):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, temporary = tempfile.mkstemp(prefix="." + path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as output:
            output.write(raw); output.flush(); os.fsync(output.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _tree(source):
    files = []
    for path in sorted((source / "collab_core").rglob("*.py")):
        if "__pycache__" not in path.parts:
            files.append(path)
    files.extend(sorted(path for path in (source / "collab_core" / "web").iterdir() if path.is_file()))
    files.extend(source / name for name in SOURCE_FILES)
    files.extend(sorted((source / "templates").glob("*.md")))
    records = []
    for path in files:
        relative = path.relative_to(source).as_posix()
        records.append({"path": relative, "sha256": digest(path.read_bytes()),
                        "mode": stat.S_IMODE(path.stat().st_mode)})
    return records


def _release_id(records):
    return hashlib.sha256(canonical(records)).hexdigest()[:24]


def _write_release(source, prefix, records, release_id):
    releases = prefix / "releases"; target = releases / release_id
    if target.exists():
        return target
    releases.mkdir(parents=True, exist_ok=True, mode=0o700)
    staging = Path(tempfile.mkdtemp(prefix=".release-", dir=releases))
    try:
        for record in records:
            origin = source / record["path"]; destination = staging / record["path"]
            destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            shutil.copyfile(origin, destination)
            os.chmod(destination, record["mode"])
        manifest = {"version": 1, "release": release_id, "files": records}
        atomic_file(staging / "manifest.json", canonical(manifest) + b"\n")
        try:
            os.replace(staging, target)
        except FileExistsError:
            shutil.rmtree(staging)
        return target
    finally:
        if staging.exists():
            shutil.rmtree(staging)


def _set_current(prefix, release):
    current = prefix / "current"
    try:
        if current.is_symlink() and current.resolve() == release.resolve():
            return
    except OSError:
        pass
    temporary = prefix / ".current-new"
    try:
        temporary.unlink()
    except FileNotFoundError:
        pass
    os.symlink(os.path.relpath(release, prefix), temporary)
    os.replace(temporary, current)


def _managed_launcher_defaults(prefix):
    """Preserve deliberate board-data relocation across source upgrades."""
    launcher = Path(prefix) / "collab"
    try:
        raw = launcher.read_text(encoding="utf-8")
    except (OSError, UnicodeError):
        return None, None
    if LAUNCHER_MARKER not in raw:
        return None, None
    values = {}
    for name in ("COLLAB_HOME", "COLLAB_RECEIPTS"):
        match = re.search(rf"export {name}=\$\{{{name}:-([^}}\n]+)\}}", raw)
        if match:
            values[name] = match.group(1).strip().strip("'\"")
    return values.get("COLLAB_HOME"), values.get("COLLAB_RECEIPTS")


def _launcher(prefix, home_default=None, receipts_default=None):
    quoted_prefix = shlex.quote(str(prefix))
    quoted_python = shlex.quote(sys.executable)
    home_default = home_default or str(prefix)
    receipts_default = receipts_default or str(prefix) + "-receipts"
    quoted_home = shlex.quote(str(home_default))
    quoted_receipts = shlex.quote(str(receipts_default))
    return ("#!/bin/sh\n" + LAUNCHER_MARKER + "\n" +
            f"export COLLAB_HOME=${{COLLAB_HOME:-{quoted_home}}}\n" +
            f"export COLLAB_RECEIPTS=${{COLLAB_RECEIPTS:-{quoted_receipts}}}\n" +
            f"exec {quoted_python} {quoted_prefix}/current/collab \"$@\"\n").encode("utf-8")


def _viewer_launcher(prefix, home_default=None, receipts_default=None):
    # The viewer reads the same board as the collab launcher, including a relocated one.
    quoted_python = shlex.quote(sys.executable)
    quoted_current = shlex.quote(str(prefix / "current"))
    quoted_home = shlex.quote(str(home_default or prefix))
    quoted_receipts = shlex.quote(str(receipts_default or str(prefix) + "-receipts"))
    return ("#!/bin/sh\n" + VIEWER_MARKER + "\n" +
            f"export COLLAB_HOME=${{COLLAB_HOME:-{quoted_home}}}\n" +
            f"export COLLAB_RECEIPTS=${{COLLAB_RECEIPTS:-{quoted_receipts}}}\n" +
            f"export PYTHONPATH={quoted_current}${{PYTHONPATH:+:$PYTHONPATH}}\n" +
            f"exec {quoted_python} -m collab_core.viewer \"$@\"\n").encode("utf-8")


def _backup(prefix, name, raw, mode):
    key = hashlib.sha256(raw).hexdigest()[:24]
    destination = prefix / "backups" / name / (key + ".md")
    if not destination.exists():
        _atomic_text(destination, raw, mode)


def _instruction_plan(prefix, entries):
    plan = []
    for name, path in entries.items():
        raw, mode = _read_instruction(path)
        span = _managed_span(raw, path)
        block = _template(name, prefix)
        updated = raw + block if span is None else raw[:span[0]] + block + raw[span[1]:]
        plan.append((name, path, raw, updated, mode, span))
    return plan


def install(source, prefix="~/.agent-collab", entries=None, dry_run=False, harnesses=None):
    """Install an immutable source release and managed harness instructions."""
    source = _source_root(source); prefix = Path(prefix).expanduser().absolute()
    entries = _select_entries(prefix, entries, harnesses)
    plan = _instruction_plan(prefix, entries)  # validates every target before mutation
    records = _tree(source); release_id = _release_id(records)
    existing_home, existing_receipts = _managed_launcher_defaults(prefix)
    changes = [str(path) for _, path, before, after, _, _ in plan if before != after]
    result = {"prefix": str(prefix), "release": release_id, "launcher": str(prefix / "collab"),
              "instruction_changes": changes, "harnesses": list(entries), "dry_run": dry_run}
    if dry_run:
        return result
    prefix.mkdir(parents=True, exist_ok=True, mode=0o700)
    release = _write_release(source, prefix, records, release_id)
    _set_current(prefix, release)
    launcher = prefix / "collab"; script = _launcher(prefix, existing_home, existing_receipts)
    if not launcher.exists() or launcher.read_bytes() != script:
        _atomic_text(launcher, script, 0o700)
    viewer_launcher = prefix / "viewer"; viewer_script = _viewer_launcher(prefix, existing_home, existing_receipts)
    if not viewer_launcher.exists() or viewer_launcher.read_bytes() != viewer_script:
        _atomic_text(viewer_launcher, viewer_script, 0o700)
    atomic_file(prefix / INTEGRATION_FILE, canonical({"version": 1,
        "entries": {name: str(path) for name, path in entries.items()}}) + b"\n")
    for name, path, before, after, mode, _ in plan:
        if before != after:
            _backup(prefix, name, before, mode)
            _atomic_text(path, after, mode)
    return result


def doctor(prefix="~/.agent-collab", entries=None):
    prefix = Path(prefix).expanduser().absolute()
    saved = _saved_entries(prefix) if entries is None else None
    entries = _entries(entries) if entries is not None else (saved if saved is not None else _entries(None))
    checks = []
    current = prefix / "current"; manifest = current / "manifest.json"
    try:
        data = json.loads(manifest.read_text(encoding="utf-8"))
        for record in data["files"]:
            path = current / record["path"]
            checks.append({"name": "source:" + record["path"], "ok": path.is_file() and digest(path.read_bytes()) == record["sha256"]})
    except (OSError, ValueError, KeyError, TypeError):
        checks.append({"name": "source-manifest", "ok": False})
    launcher = prefix / "collab"
    checks.append({"name": "launcher", "ok": launcher.is_file() and LAUNCHER_MARKER.encode() in launcher.read_bytes()})
    viewer_launcher = prefix / "viewer"
    checks.append({"name": "viewer-launcher", "ok": viewer_launcher.is_file() and VIEWER_MARKER.encode() in viewer_launcher.read_bytes()})
    for name, path in entries.items():
        try:
            raw, _ = _read_instruction(path)
            checks.append({"name": "instruction:" + name, "ok": _managed_span(raw, path) is not None})
        except InstallError:
            checks.append({"name": "instruction:" + name, "ok": False})
    return {"prefix": str(prefix), "ok": all(item["ok"] for item in checks),
            "checks": checks, "harnesses": {name: shutil.which(name) is not None for name in entries}}


def uninstall(prefix="~/.agent-collab", entries=None, dry_run=False):
    """Remove only managed instruction blocks and a recognizably managed launcher."""
    prefix = Path(prefix).expanduser().absolute()
    saved = _saved_entries(prefix) if entries is None else None
    entries = _entries(entries) if entries is not None else (saved if saved is not None else _entries(None))
    plan = []
    for name, path in entries.items():
        raw, mode = _read_instruction(path); span = _managed_span(raw, path)
        plan.append((name, path, raw, raw if span is None else raw[:span[0]] + raw[span[1]:], mode))
    launcher = prefix / "collab"; remove_launcher = launcher.is_file() and LAUNCHER_MARKER.encode() in launcher.read_bytes()
    viewer_launcher = prefix / "viewer"
    remove_viewer = viewer_launcher.is_file() and VIEWER_MARKER.encode() in viewer_launcher.read_bytes()
    result = {"prefix": str(prefix), "instruction_changes": [str(path) for _, path, before, after, _ in plan if before != after],
              "remove_launcher": remove_launcher, "remove_viewer": remove_viewer, "dry_run": dry_run}
    if dry_run:
        return result
    for name, path, before, after, mode in plan:
        if before != after:
            _backup(prefix, name, before, mode)
            _atomic_text(path, after, mode)
    if remove_launcher:
        launcher.unlink()
    if remove_viewer:
        viewer_launcher.unlink()
    (prefix / INTEGRATION_FILE).unlink(missing_ok=True)
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(prog="python -m collab_core.install")
    parser.add_argument("--prefix", default="~/.agent-collab")
    parser.add_argument("--json", action="store_true")
    sub = parser.add_subparsers(dest="command", required=True)
    put = sub.add_parser("install"); put.add_argument("source"); put.add_argument("--dry-run", action="store_true")
    selection = put.add_mutually_exclusive_group()
    selection.add_argument("--harness", action="append", choices=TEMPLATE_NAMES)
    selection.add_argument("--no-harnesses", action="store_true")
    sub.add_parser("doctor")
    remove = sub.add_parser("uninstall"); remove.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)
    if args.command == "install":
        harnesses = [] if args.no_harnesses else args.harness
        result = install(args.source, args.prefix, dry_run=args.dry_run, harnesses=harnesses)
    elif args.command == "doctor": result = doctor(args.prefix)
    else: result = uninstall(args.prefix, dry_run=args.dry_run)
    print(json.dumps(result, sort_keys=True) if args.json else result)
    return 0 if result.get("ok", True) else 1


if __name__ == "__main__":
    raise SystemExit(main())
