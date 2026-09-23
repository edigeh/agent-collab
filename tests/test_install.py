"""Installer behavior is exercised only against temporary harness roots."""
from __future__ import annotations

from pathlib import Path
import shutil
import stat
import tempfile
import unittest

from collab_core.install import (
    InstallError,
    MARKER_BEGIN,
    MARKER_END,
    doctor,
    install,
    uninstall,
)


SOURCE = Path(__file__).resolve().parents[1]


class InstallTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.prefix = self.root / "board"
        self.entries = {
            "codex": self.root / "home" / ".codex" / "AGENTS.md",
            "claude": self.root / "home" / ".claude" / "CLAUDE.md",
            "pi": self.root / "home" / ".pi" / "agent" / "AGENTS.md",
            "omp": self.root / "home" / ".omp" / "agent" / "AGENTS.md",
        }
        for path in self.entries.values():
            path.parent.mkdir(parents=True)
            path.write_bytes(b"User instruction without final newline")
            path.chmod(0o640)

    def tearDown(self):
        self.temp.cleanup()

    def test_idempotency_preserves_surrounding_bytes_and_modes(self):
        first = install(SOURCE, self.prefix, self.entries)
        self.assertFalse(first["dry_run"])
        before = {name: path.read_bytes() for name, path in self.entries.items()}
        backup_count = len(list((self.prefix / "backups").rglob("*.md")))
        second = install(SOURCE, self.prefix, self.entries)
        self.assertEqual(second["instruction_changes"], [])
        self.assertEqual(before, {name: path.read_bytes() for name, path in self.entries.items()})
        self.assertEqual(backup_count, len(list((self.prefix / "backups").rglob("*.md"))))
        for path in self.entries.values():
            self.assertTrue(path.read_bytes().startswith(b"User instruction without final newline"))
            self.assertIn(MARKER_BEGIN, path.read_bytes())
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o640)
        self.assertTrue((self.prefix / "collab").is_file())
        self.assertTrue((self.prefix / "current").is_symlink())
        self.assertEqual((self.prefix / "current" / "templates" / "codex.md").read_bytes(), (SOURCE / "templates" / "codex.md").read_bytes())
        self.assertTrue(doctor(self.prefix, self.entries)["ok"])

    def test_omp_entry_receives_omp_instructions(self):
        install(SOURCE, self.prefix, self.entries)
        block = self.entries["omp"].read_bytes()
        self.assertIn(b"--harness omp", block)
        self.assertNotIn(b"--harness pi", block)

    def test_upgrade_keeps_board_history_and_artifacts(self):
        fixture = self.root / "source"
        shutil.copytree(SOURCE, fixture, ignore=shutil.ignore_patterns("__pycache__", ".git", "work"))
        install(fixture, self.prefix, self.entries)
        old_release = (self.prefix / "current").resolve().name
        (self.prefix / "events.jsonl").write_bytes(b"durable history\n")
        artifact = self.prefix / "artifacts" / ("a" * 64)
        artifact.parent.mkdir(); artifact.write_bytes(b"evidence")
        init = fixture / "collab_core" / "__init__.py"
        init.write_text(init.read_text() + "\n# upgrade fixture\n")
        install(fixture, self.prefix, self.entries)
        self.assertNotEqual(old_release, (self.prefix / "current").resolve().name)
        self.assertEqual((self.prefix / "events.jsonl").read_bytes(), b"durable history\n")
        self.assertEqual(artifact.read_bytes(), b"evidence")

    def test_upgrade_preserves_relocated_board_paths(self):
        install(SOURCE, self.prefix, self.entries)
        launcher = self.prefix / 'collab'
        raw = launcher.read_text()
        raw = raw.replace(f'COLLAB_HOME:-{self.prefix}', 'COLLAB_HOME:-/durable/board')
        raw = raw.replace(f'COLLAB_RECEIPTS:-{self.prefix}-receipts', 'COLLAB_RECEIPTS:-/durable/receipts')
        launcher.write_text(raw)
        install(SOURCE, self.prefix, self.entries)
        updated = launcher.read_text()
        self.assertIn('COLLAB_HOME:-/durable/board', updated)
        self.assertIn('COLLAB_RECEIPTS:-/durable/receipts', updated)

    def test_uninstall_removes_only_managed_material_and_keeps_data(self):
        install(SOURCE, self.prefix, self.entries)
        (self.prefix / "events.jsonl").write_bytes(b"history")
        receipt = self.root / "board-receipts" / "agent.jsonl"
        receipt.parent.mkdir(); receipt.write_bytes(b"receipt")
        releases = set((self.prefix / "releases").iterdir())
        result = uninstall(self.prefix, self.entries)
        self.assertTrue(result["remove_launcher"])
        self.assertFalse((self.prefix / "collab").exists())
        self.assertEqual((self.prefix / "events.jsonl").read_bytes(), b"history")
        self.assertEqual(receipt.read_bytes(), b"receipt")
        self.assertEqual(releases, set((self.prefix / "releases").iterdir()))
        for path in self.entries.values():
            self.assertEqual(path.read_bytes(), b"User instruction without final newline")
            self.assertNotIn(MARKER_BEGIN, path.read_bytes())
        self.assertTrue((self.prefix / "backups").exists())

    def test_dry_run_and_doctor_hash_failure(self):
        preview = install(SOURCE, self.prefix, self.entries, dry_run=True)
        self.assertTrue(preview["dry_run"])
        self.assertFalse(self.prefix.exists())
        install(SOURCE, self.prefix, self.entries)
        target = next((self.prefix / "current" / "collab_core").glob("*.py"))
        target.write_text("tampered")
        checked = doctor(self.prefix, self.entries)
        self.assertFalse(checked["ok"])
        self.assertTrue(any(not item["ok"] for item in checked["checks"] if item["name"].startswith("source:")))

    def test_selected_harnesses_persist_and_leave_other_instructions_alone(self):
        untouched = {name: path.read_bytes() for name, path in self.entries.items() if name != "codex"}
        install(SOURCE, self.prefix, {"codex": self.entries["codex"]})
        self.assertTrue(doctor(self.prefix)["ok"])
        for name, before in untouched.items():
            self.assertEqual(self.entries[name].read_bytes(), before)
        install(SOURCE, self.prefix)
        with self.assertRaises(InstallError):
            install(SOURCE, self.prefix, harnesses=[])
        uninstall(self.prefix)
        self.assertEqual(self.entries["codex"].read_bytes(), b"User instruction without final newline")
        for name, before in untouched.items():
            self.assertEqual(self.entries[name].read_bytes(), before)

    def test_release_contains_viewer_assets(self):
        install(SOURCE, self.prefix, {})
        for name in ("index.html", "app.js", "style.css"):
            installed = self.prefix / "current" / "collab_core" / "web" / name
            self.assertEqual(installed.read_bytes(), (SOURCE / "collab_core" / "web" / name).read_bytes())
        self.assertTrue(doctor(self.prefix)["ok"])
        self.assertEqual(doctor(self.prefix)["harnesses"], {})

    def test_duplicate_or_malformed_markers_reject_before_mutation(self):
        self.entries["codex"].write_bytes(MARKER_BEGIN + b"\nbody\n")
        before = {name: path.read_bytes() for name, path in self.entries.items()}
        with self.assertRaises(InstallError):
            install(SOURCE, self.prefix, self.entries)
        self.assertFalse(self.prefix.exists())
        self.assertEqual(before, {name: path.read_bytes() for name, path in self.entries.items()})

        self.entries["codex"].write_bytes(MARKER_BEGIN + b"\n" + MARKER_END + b"\n" + MARKER_BEGIN + b"\n" + MARKER_END)
        with self.assertRaises(InstallError):
            install(SOURCE, self.prefix, self.entries)
        self.assertFalse(self.prefix.exists())


if __name__ == "__main__":
    unittest.main()
