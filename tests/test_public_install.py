"""Exercise the public clone-and-install path without touching the real home."""
from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[1]


class PublicInstallTests(unittest.TestCase):
    def test_fresh_install_two_boards_viewer_upgrade_and_uninstall(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            home = root / "home"
            home.mkdir()
            project = root / "project"
            project.mkdir()
            prefix = root / "install"
            env = os.environ.copy()
            env["HOME"] = str(home)
            env.pop("PYTHONPATH", None)

            def command(argv, *, cwd=project, environment=env):
                process = subprocess.run(argv, cwd=cwd, env=environment, text=True,
                                         capture_output=True, timeout=30)
                self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
                return json.loads(process.stdout)

            installer = [sys.executable, "-m", "collab_core.install", "--json", "--prefix", str(prefix)]
            first = command(installer + ["install", str(SOURCE), "--harness", "codex"], cwd=SOURCE)
            self.assertEqual(first["harnesses"], ["codex"])
            self.assertTrue(command(installer + ["doctor"], cwd=SOURCE)["ok"])
            self.assertTrue((home / ".codex" / "AGENTS.md").exists())
            self.assertFalse((home / ".claude" / "CLAUDE.md").exists())
            viewer_help = subprocess.run([str(prefix / "viewer"), "--help"], cwd=project,
                                         env=env, text=True, capture_output=True, timeout=30)
            self.assertEqual(viewer_help.returncode, 0, viewer_help.stdout + viewer_help.stderr)

            launcher = str(prefix / "collab")
            board_a = {**env, "COLLAB_HOME": str(root / "board-a"),
                       "COLLAB_RECEIPTS": str(root / "receipts-a")}
            board_b = {**env, "COLLAB_HOME": str(root / "board-b"),
                       "COLLAB_RECEIPTS": str(root / "receipts-b")}

            def board(argv, environment):
                return command([launcher, "--json", *argv], environment=environment)

            sender = board(["wake", "--harness", "codex"], board_a)
            recipient = board(["wake", "--harness", "codex"], board_a)
            board(["--session", sender["session"], "ack", sender["brief"]["delivery"]], board_a)
            board(["--session", recipient["session"], "ack", recipient["brief"]["delivery"]], board_a)
            message = board(["--session", sender["session"], "post", "public install smoke",
                             "--to", recipient["session"]], board_a)
            self.assertEqual(message["status"], "accepted")
            incoming = board(["--session", recipient["session"], "inbox"], board_a)
            self.assertTrue(any(item.get("text") == "public install smoke"
                                for item in incoming["items"]))
            board(["--session", recipient["session"], "ack", incoming["delivery"]], board_a)
            self.assertEqual(board(["--session", sender["session"], "sync"], board_a)["status"], "accepted")

            separate = board(["wake", "--harness", "codex"], board_b)
            self.assertFalse(any(item.get("text") == "public install smoke"
                                 for item in separate["brief"]["items"]))
            self.assertNotEqual((root / "board-a" / "events.jsonl").read_bytes(),
                                (root / "board-b" / "events.jsonl").read_bytes())

            viewer_script = """import json, sys, threading, urllib.request
from collab_core.viewer import make_server
server = make_server(sys.argv[1], 0)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
base = f'http://127.0.0.1:{server.server_port}'
with urllib.request.urlopen(base + '/') as response:
    html = response.read()
with urllib.request.urlopen(base + '/api/board') as response:
    snapshot = json.load(response)
server.shutdown()
server.server_close()
assert b'html' in html.lower()
assert any(post['text'] == 'public install smoke' for post in snapshot['posts'])
print('installed viewer served board and bundled assets')
"""
            viewed = subprocess.run([sys.executable, "-c", viewer_script, str(root / "board-a")],
                                    cwd=project, env={**env, "PYTHONPATH": str(prefix / "current")},
                                    text=True, capture_output=True, timeout=30)
            self.assertEqual(viewed.returncode, 0, viewed.stdout + viewed.stderr)

            before = (root / "board-a" / "events.jsonl").read_bytes()
            command(installer + ["install", str(SOURCE)], cwd=SOURCE)
            self.assertEqual((root / "board-a" / "events.jsonl").read_bytes(), before)
            preview = command(installer + ["uninstall", "--dry-run"], cwd=SOURCE)
            self.assertTrue(preview["remove_launcher"])
            command(installer + ["uninstall"], cwd=SOURCE)
            self.assertFalse((prefix / "collab").exists())
            self.assertFalse((prefix / "viewer").exists())
            self.assertEqual((root / "board-a" / "events.jsonl").read_bytes(), before)
            self.assertTrue((root / "receipts-a").exists())


if __name__ == "__main__":
    unittest.main()
