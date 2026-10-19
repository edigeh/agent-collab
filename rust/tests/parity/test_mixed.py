"""Python and Rust writing one board, in turn and at the same time."""
import json, os, subprocess, sys, tempfile, unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
BIN = Path(os.environ.get('COLLAB_RUST_BIN', ROOT / 'rust' / 'target' / 'release'))
sys.path.insert(0, str(ROOT))
from collab_core.board import Board, canonical, digest
from collab_core.journal import Journal


class Mixed(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(); self.addCleanup(tmp.cleanup)
        self.base = Path(tmp.name)
        self.env = {**os.environ, 'COLLAB_HOME': str(self.base / 'board'), 'COLLAB_RECEIPTS': str(self.base / 'receipts'),
                    'PYTHONPATH': str(ROOT), 'COLLAB_HOST_PID': str(os.getpid())}
        self.n = 0

    def cli(self, build, *args, session=None):
        cmd = [sys.executable, '-m', 'collab_core'] if build == 'py' else [str(BIN / 'collab')]
        cmd += ['--json'] + (['--session', session] if session else []) + list(args)
        done = subprocess.run(cmd, cwd=self.base, env=self.env, capture_output=True, text=True, timeout=60)
        self.assertTrue(done.stdout, done.stderr)
        return json.loads(done.stdout)

    def alternate(self, *args, session=None):
        self.n += 1
        return self.cli('py' if self.n % 2 else 'rs', *args, session=session)

    def check_verified(self):
        for build in ('py', 'rs'):
            result = self.cli(build, 'verify')
            self.assertEqual(result['status'], 'accepted', (build, result))
            self.assertTrue(result['match'], (build, result))
        board = Board(self.base / 'board', receipt_root=self.base / 'receipts')
        state = board.read_state()
        with Journal(self.base / 'board' / 'events.jsonl').locked() as log:
            return state, list(log.records)

    def test_alternating_writers(self):
        self.alternate('wake', '--harness', 'claude', '--doing', 'One', '--summary', 'first', session='a')
        self.alternate('wake', '--harness', 'codex', session='b')
        for i in range(30):
            posted = self.alternate('post', f'message {i} ✓', '--to', 'b' if i % 2 else 'a', session='a' if i % 3 else 'b')
            self.assertEqual(posted['status'], 'accepted', posted)
            if i % 5 == 0:
                inbox = self.alternate('inbox', session='b')
                self.alternate('ack', inbox['delivery'], session='b')
            if i % 7 == 0:
                task = self.alternate('task', 'create', f'task {i}', session='a')
                self.alternate('task', 'claim', task['task'], '--expected', '1', session='a')
        state, records = self.check_verified()
        self.assertGreater(len(records), 50)

    def test_concurrent_writers(self):
        self.cli('py', 'wake', '--harness', 'claude', session='a')
        self.cli('rs', 'wake', '--harness', 'codex', session='b')
        jobs = [('py' if i % 2 else 'rs', i) for i in range(48)]
        with ThreadPoolExecutor(8) as pool:
            results = list(pool.map(lambda j: self.cli(j[0], 'post', f'parallel {j[1]}', session='ab'[j[1] % 2]), jobs))
        self.assertTrue(all(r['status'] == 'accepted' for r in results), [r for r in results if r['status'] != 'accepted'])
        state, records = self.check_verified()
        texts = {p['text'] for p in state['posts'].values()}
        self.assertTrue(all(f'parallel {i}' in texts for i in range(48)))
        seqs = [r['seq'] for r in records]
        self.assertEqual(seqs, list(range(1, len(seqs) + 1)))


if __name__ == '__main__':
    unittest.main()
