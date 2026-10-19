"""Incremental replay: journal positions and the saved view."""
import contextlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from collab_core import board as board_module
from collab_core.board import Board, View, canonical, digest
from collab_core.journal import Journal, JournalCorrupt, Position

ROOT = Path(__file__).resolve().parents[1]


class JournalPositions(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(); self.addCleanup(tmp.cleanup)
        self.journal = Journal(Path(tmp.name) / 'events.jsonl')
        with self.journal.locked() as log:
            self.records = [log.append({'n': n, 'text': 'record %d' % n}) for n in range(3)]
            self.head = log.head
        with self.journal.locked() as log:
            self.assertEqual(log.head, self.head)
        raw = self.journal.path.read_bytes()
        first_end = raw.index(b'\n') + 1; second_end = raw.index(b'\n', first_end) + 1
        self.second = Position(self.head.stream, 2, self.records[1]['sha256'], first_end, second_end)

    def test_reads_only_records_after_a_verified_position(self):
        with self.journal.locked(after=self.second) as log:
            self.assertEqual(log.base, self.second)
            self.assertEqual(log.records, self.records[2:])
            self.assertEqual(log.head, self.head)
            self.assertEqual(log.history(), self.records)

    def test_changed_or_missing_position_reads_everything(self):
        wrong = Position(self.second.stream, 2, 'f' * 64, self.second.start, self.second.end)
        past_end = Position(self.head.stream, 9, self.head.sha256, self.head.end, self.head.end + 10)
        for position in (wrong, past_end):
            with self.journal.locked(after=position) as log:
                self.assertIsNone(log.base)
                self.assertEqual(log.records, self.records)

    def test_append_reports_the_new_head(self):
        with self.journal.locked(after=self.head) as log:
            self.assertEqual(log.records, [])
            record = log.append({'n': 3})
            head = log.head
        self.assertEqual((head.seq, head.sha256, head.start), (4, record['sha256'], self.head.end))
        with self.journal.locked(after=head) as log:
            self.assertEqual((log.base, log.records), (head, []))

    def test_prefix_damage_is_found_by_history_not_by_the_position_check(self):
        raw = self.journal.path.read_bytes()
        self.journal.path.write_bytes(raw.replace(b'record 0', b'record X', 1))
        with self.journal.locked(after=self.head) as log:
            self.assertEqual(log.base, self.head)
            with self.assertRaises(JournalCorrupt):
                log.history()
        with self.assertRaises(JournalCorrupt):
            with self.journal.locked():
                pass

    def test_incomplete_tail_after_a_position_is_recovered(self):
        with open(self.journal.path, 'ab') as stream:
            stream.write(b'{"partial":')
        with self.journal.locked(after=self.head) as log:
            self.assertEqual((log.base, log.records), (self.head, []))
        self.assertEqual(self.journal.path.stat().st_size, self.head.end)
        self.assertEqual(len(list((self.journal.path.parent / 'recoveries').iterdir())), 1)


class SavedView(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(); self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name) / 'board'
        self.board = Board(self.root); self.board.save_after = 0
        self.call('a', 'session.register', harness='codex')
        self.call('b', 'session.register', harness='pi')
        self.call('a', 'project.create', id='p', name='Example')
        self.call('a', 'session.touch', project='p')
        self.task = self.call('a', 'task.create', title='Owned by a', scope=['src/a.py'])['task']
        rejected = self.board.submit(self.board.request('b', 'task.create', {'title': 'clash', 'scope': ['src/a.py'], 'project': 'p'}))
        self.assertEqual(rejected['status'], 'rejected')
        self.call('a', 'post.create', text='hello', to='b')
        self.board.read_state()  # replays nothing new, saves the view at the head
        self.saved = self.header()['position']['seq']

    def call(self, actor, op, **data):
        result = self.board.submit(self.board.request(actor, op, data))
        self.assertEqual(result['status'], 'accepted', result)
        return result

    def header(self):
        return json.loads(self.board.cache_path.read_bytes().split(b'\n', 1)[0])

    def full_replay(self):
        with self.board.journal.locked() as log:
            view = View(); view.replay(log.records, log.positions)
        return view.state

    def replayed(self, fresh):
        """Read state through `fresh` and return how many records it replayed."""
        counts = []
        original = View.replay
        def counting(view, records, positions):
            counts.append(len(records)); return original(view, records, positions)
        with mock.patch.object(View, 'replay', counting):
            state = fresh.read_state()
        self.assertEqual(digest(canonical(state)), digest(canonical(self.full_replay())))
        return sum(counts)

    def test_new_process_replays_only_records_after_the_saved_view(self):
        writer = Board(self.root); writer.save_after = 10**6
        for n in range(3):
            writer.submit(writer.request('b', 'post.create', {'text': f'later {n}', 'project': 'p'}))
        self.assertEqual(self.header()['position']['seq'], self.saved)
        self.assertEqual(self.replayed(Board(self.root)), 3)

    def test_one_process_sees_other_writers(self):
        self.board.read_state()
        other = Board(self.root)
        post = other.submit(other.request('b', 'post.create', {'text': 'from b', 'project': 'p'}))['post']
        self.assertIn(post, self.board.read_state()['posts'])

    def test_damaged_view_replays_from_the_start(self):
        raw = self.board.cache_path.read_bytes()
        self.board.cache_path.write_bytes(raw.replace(b'Owned by a', b'Owned by b', 1))
        self.assertEqual(self.replayed(Board(self.root)), self.saved)

    def test_forged_view_with_its_own_key_replays_from_the_start(self):
        header, state, requests = self.board.cache_path.read_bytes().splitlines()
        forged = json.loads(state); forged['tasks'][self.task]['owner'] = 'b'
        (self.root / 'view.key').write_bytes(b'0' * 64)
        fake = Board(self.root); fake._view = View(Position(**self.header()['position']), forged, json.loads(requests))
        fake._save_view(fake._view)
        (self.root / 'view.key').write_bytes(b'1' * 64)
        fresh = Board(self.root)
        self.assertEqual(self.replayed(fresh), self.saved)
        self.assertEqual(fresh.read_state()['tasks'][self.task]['owner'], 'a')

    def test_view_from_other_reducer_source_replays_from_the_start(self):
        with mock.patch.object(board_module, 'REDUCER', 'f' * 64):
            self.assertEqual(self.replayed(Board(self.root)), self.saved)

    def test_missing_key_replays_and_next_save_makes_one(self):
        (self.root / 'view.key').unlink()
        fresh = Board(self.root); fresh.save_after = 0
        self.assertEqual(self.replayed(fresh), self.saved)
        self.assertTrue((self.root / 'view.key').exists())
        self.assertEqual(self.replayed(Board(self.root)), 0)

    def test_view_ahead_of_a_restored_journal_replays_from_the_start(self):
        raw = self.board.journal.path.read_bytes()
        kept = b''.join(raw.splitlines(keepends=True)[:4])
        self.board.journal.path.write_bytes(kept)
        self.assertEqual(self.replayed(Board(self.root)), 4)

    def test_verify_matches_and_repairs_a_wrong_view(self):
        self.assertTrue(self.board.verify()['match'])
        self.board._view.state['tasks'][self.task]['owner'] = 'b'
        result = self.board.verify()
        self.assertEqual((result['status'], result['match']), ('rejected', False))
        self.assertEqual(self.board.read_state()['tasks'][self.task]['owner'], 'a')
        self.assertEqual(self.replayed(Board(self.root)), 0)

    def test_full_replay_reads_the_journal_before_taking_the_lock(self):
        self.board.cache_path.unlink()
        seen, original = [], Journal.locked
        @contextlib.contextmanager
        def watching(journal, after=None):
            with original(journal, after) as log:
                seen.append((log.base is not None, len(log.records)))
                yield log
        fresh = Board(self.root)
        with mock.patch.object(Journal, 'locked', watching):
            state = fresh.read_state()
        self.assertEqual(seen, [(True, 0)])
        self.assertEqual(digest(canonical(state)), digest(canonical(self.full_replay())))

    def test_records_appended_after_the_unlocked_read_replay_under_the_lock(self):
        self.board.cache_path.unlink()
        original = Journal.committed
        def earlier(journal):
            records, positions = original(journal)
            return records[:-2], positions[:-2]
        with mock.patch.object(Journal, 'committed', earlier):
            self.assertEqual(self.replayed(Board(self.root)), self.saved)

    def test_rejected_write_records_only_its_notice(self):
        before = self.board.read_state()
        result = self.board.submit(self.board.request('b', 'task.block', {'task': self.task, 'expected': 1, 'reason': 'not owner'}))
        self.assertEqual(result['status'], 'rejected')
        after = self.board.read_state()
        self.assertEqual(after['tasks'], before['tasks'])
        self.assertEqual(digest(canonical(after)), digest(canonical(self.full_replay())))


class WakeWrites(unittest.TestCase):
    def test_wake_in_a_known_project_registers_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp); home = base / 'board'
            def wake(session):
                result = subprocess.run([sys.executable, str(ROOT / 'collab'), '--home', str(home),
                    '--receipts', str(base / 'receipts'), '--session', session, '--json', 'wake', '--harness', 'codex'],
                    cwd=base, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                return json.loads(result.stdout)
            first, second = wake('first'), wake('second')
            self.assertEqual(first['project'], second['project'])
            with Journal(home / 'events.jsonl').locked() as log:
                ops = [(r['payload']['request']['actor'], r['payload']['request']['op']) for r in log.records]
            self.assertEqual(ops.count(('second', 'session.register')), 1)
            self.assertEqual(ops.count(('first', 'session.register')), 2)


if __name__ == '__main__':
    unittest.main()
