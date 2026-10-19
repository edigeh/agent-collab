"""collab-install keeps the Python installer's prefix layout, so either can run over the other."""
import json, os, subprocess, sys, tempfile, unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
BIN = Path(os.environ.get('COLLAB_RUST_BIN', ROOT / 'rust' / 'target' / 'release'))
MARKER = b"<!-- agent-collab:begin v1 -->"


class RustInstall(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(); self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.home = self.root / 'home'; self.home.mkdir()
        self.prefix = self.root / 'install'
        self.env = {**os.environ, 'HOME': str(self.home)}
        self.env.pop('PYTHONPATH', None)
        codex = self.home / '.codex' / 'AGENTS.md'; codex.parent.mkdir()
        codex.write_bytes(b'User instruction without final newline'); codex.chmod(0o640)
        self.codex = codex

    def run_json(self, argv, code=0, **kw):
        done = subprocess.run(argv, env=kw.pop('env', self.env), capture_output=True, text=True, timeout=60, **kw)
        self.assertEqual(done.returncode, code, done.stdout + done.stderr)
        return json.loads(done.stdout)

    def rust(self, *args, code=0):
        return self.run_json([str(BIN / 'collab-install'), '--json', '--prefix', str(self.prefix), *args], code=code)

    def python(self, *args, code=0):
        return self.run_json([sys.executable, '-m', 'collab_core.install', '--json', '--prefix', str(self.prefix), *args], code=code, cwd=ROOT)

    def test_install_use_doctor_rollback_and_uninstall(self):
        first = self.rust('install', str(BIN), '--harness', 'codex')
        self.assertEqual(first['harnesses'], ['codex'])
        self.assertTrue(self.rust('doctor')['ok'])
        self.assertTrue(self.python('doctor')['ok'])
        block = self.codex.read_bytes()
        self.assertTrue(block.startswith(b'User instruction without final newline\n' + MARKER))
        self.assertIn(str(self.prefix).encode() + b'/collab', block)
        self.assertEqual(self.codex.stat().st_mode & 0o777, 0o640)
        self.assertEqual(self.rust('install', str(BIN))['instruction_changes'], [])

        board = {**self.env, 'COLLAB_HOME': str(self.root / 'board'), 'COLLAB_RECEIPTS': str(self.root / 'receipts')}
        woke = self.run_json([str(self.prefix / 'collab'), '--json', 'wake', '--harness', 'codex'], env=board, cwd=self.root)
        self.assertEqual(woke['status'], 'accepted')
        helped = subprocess.run([str(self.prefix / 'viewer'), '--help'], env=board, capture_output=True, text=True)
        self.assertEqual(helped.returncode, 0, helped.stderr)
        self.assertIn('collab-viewer', helped.stdout)

        # Rolling back is the Python installer over the same prefix, and forward again.
        self.python('install', str(ROOT))
        self.assertTrue(self.python('doctor')['ok'])
        posted = self.run_json([str(self.prefix / 'collab'), '--json', '--session', woke['session'], 'post', 'from python'], env=board, cwd=self.root)
        self.assertEqual(posted['status'], 'accepted')
        self.rust('install', str(BIN))
        self.assertTrue(self.rust('doctor')['ok'])
        verified = self.run_json([str(self.prefix / 'collab'), '--json', 'verify'], env=board, cwd=self.root)
        self.assertTrue(verified['match'], verified)

        preview = self.rust('uninstall', '--dry-run')
        self.assertTrue(preview['remove_launcher']); self.assertTrue(preview['remove_viewer'])
        self.rust('uninstall')
        self.assertFalse((self.prefix / 'collab').exists()); self.assertFalse((self.prefix / 'viewer').exists())
        self.assertEqual(self.codex.read_bytes(), b'User instruction without final newline')
        self.assertTrue((self.root / 'board' / 'events.jsonl').exists())

    def test_relocated_board_survives_upgrade(self):
        self.rust('install', str(BIN), '--no-harnesses')
        launcher = self.prefix / 'collab'
        raw = launcher.read_text().replace(f'COLLAB_HOME:-{self.prefix}', 'COLLAB_HOME:-/durable/board')
        launcher.write_text(raw)
        self.rust('install', str(BIN))
        self.assertIn('COLLAB_HOME:-/durable/board', launcher.read_text())
        self.assertIn('COLLAB_HOME:-/durable/board', (self.prefix / 'viewer').read_text())

    def test_malformed_markers_reject_before_mutation(self):
        self.codex.write_bytes(MARKER + b'\nbody\n')
        done = subprocess.run([str(BIN / 'collab-install'), '--prefix', str(self.prefix), 'install', str(BIN), '--harness', 'codex'],
                              env=self.env, capture_output=True, text=True)
        self.assertEqual(done.returncode, 1)
        self.assertIn('duplicate or malformed managed markers', done.stderr)
        self.assertFalse(self.prefix.exists())

    def test_doctor_detects_tampering_and_argument_errors(self):
        self.rust('install', str(BIN), '--no-harnesses')
        (self.prefix / 'current' / 'templates' / 'codex.md').chmod(0o644)
        (self.prefix / 'current' / 'templates' / 'codex.md').write_text('tampered')
        self.assertFalse(self.rust('doctor', code=1)['ok'])
        done = subprocess.run([str(BIN / 'collab-install'), 'install', str(BIN), '--harness', 'codex', '--no-harnesses'],
                              env=self.env, capture_output=True, text=True)
        self.assertEqual(done.returncode, 2)
        self.assertIn('collab-install install: error: argument --no-harnesses: not allowed with argument --harness', done.stderr)


if __name__ == '__main__':
    unittest.main()
