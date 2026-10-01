"""Presence: declared status, read-time online state, and harness-process checks."""
import datetime as dt
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from collab_core.board import Board
from collab_core import presence

ROOT = Path(__file__).resolve().parents[1]
T0 = dt.datetime(2026, 10, 1, 12, 0, tzinfo=dt.timezone.utc)
STATUS = {'doing': 'Presence feature', 'summary': 'Designing who/doing; tests next'}
STARTED = 'Thu Oct 1 11:00:00 2026'


def at(minutes):
    return (T0 + dt.timedelta(minutes=minutes)).isoformat()


def host(pid):
    return {'pid': pid, 'started': STARTED}


class BoardCase(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(); self.addCleanup(tmp.cleanup)
        self.board = Board(Path(tmp.name) / 'board'); self.n = 0

    def submit(self, actor, op, minute=0, **data):
        self.n += 1
        with patch('collab_core.board.utcnow', return_value=at(minute)):
            return self.board.submit(self.board.request(actor, op, data, f'r{self.n}'))

    def call(self, actor, op, minute=0, **data):
        result = self.submit(actor, op, minute, **data)
        self.assertEqual(result['status'], 'accepted', result)
        return result

    def session(self, actor):
        return self.board.read_state()['sessions'][actor]


class ReducerTests(BoardCase):
    def test_register_stores_normalized_status_and_host(self):
        result = self.call('a', 'session.register', harness='claude', host=host(4242),
                           status={**STATUS, 'uses': ['Sim:iPhone-16', ' port:8765 ', 'sim:iphone-16']})
        # 0.1.0 replays these events too, so the recorded result keeps its shape.
        self.assertEqual({k: v for k, v in result.items() if k not in ('status', 'request_id', 'seq')},
                         {'id': 'r1', 'session': 'a'})
        item = self.session('a')
        self.assertEqual(item['status'], {**STATUS, 'uses': ['port:8765', 'sim:iphone-16'], 'at': at(0)})
        self.assertEqual(item['host'], {**host(4242), 'at': at(0)})

    def test_status_and_host_are_bounded(self):
        self.call('a', 'session.register', harness='claude')
        bad = [{'doing': 'x' * 81, 'summary': 's'}, {'doing': 'd', 'summary': 'x' * 281},
               {'doing': 'd', 'summary': 's', 'uses': ['a', 'b', 'c', 'd', 'e']},
               {'doing': 'd', 'summary': 's', 'uses': ['x' * 49]}, {'doing': 'd'},
               {'doing': 'd', 'summary': 's', 'extra': 1}, 'doing']
        for status in bad:
            self.assertEqual(self.submit('a', 'session.touch', status=status)['status'], 'rejected', status)
        for value in ({'pid': True, 'started': STARTED}, {'pid': '7', 'started': STARTED}, {'pid': 9}):
            self.assertEqual(self.submit('a', 'session.register', harness='claude', host=value)['status'], 'rejected')
        self.assertEqual(self.submit('a', 'session.touch', summary='no leave')['status'], 'rejected')
        self.assertNotIn('status', self.session('a'))

    def test_touch_updates_status_and_leave_keeps_a_note(self):
        self.call('a', 'session.register', harness='codex')
        result = self.call('a', 'session.touch', 5, status=STATUS)
        self.assertEqual(set(result) - {'status', 'request_id', 'seq'}, {'id', 'session'})
        self.assertEqual(self.session('a')['status'], {**STATUS, 'at': at(5)})
        self.call('a', 'session.touch', 9, leave=True, summary='Left off at tests')
        self.assertEqual(self.session('a')['left'], {'at': at(9), 'summary': 'Left off at tests'})
        self.call('a', 'post.create', 12, text='Back again')
        item = self.session('a')
        self.assertNotIn('left', item)
        self.assertEqual(item['seen_at'], at(12))

    def test_every_accepted_command_counts_as_activity(self):
        self.call('a', 'session.register', harness='pi')
        self.call('a', 'post.create', 30, text='Working')
        self.assertEqual(self.session('a')['seen_at'], at(30))
        self.assertEqual(self.submit('a', 'post.create', 40, text='')['status'], 'rejected')
        self.assertEqual(self.session('a')['seen_at'], at(30))


class ClassifyTests(unittest.TestCase):
    def test_online_rules(self):
        live = {(4242, STARTED)}
        root = {'id': 'a', 'parent': None, 'host': {**host(4242), 'at': at(0)}}
        classify = presence.classify
        self.assertEqual(classify(root, 60, live, False), 'active')
        self.assertEqual(classify(root, 3600, live, False), 'idle')
        self.assertIsNone(classify(root, 3600, set(), False))
        self.assertIsNone(classify(root, 3600, live, True))
        self.assertEqual(classify(root, 60, live, True), 'active')
        self.assertEqual(classify(root, 3600, None, False), 'idle')
        child = {**root, 'id': 'c', 'parent': 'a'}
        self.assertEqual(classify(child, 60, live, False), 'active')
        self.assertIsNone(classify(child, 3600, live, False))
        unknown = {'id': 'n', 'parent': None}
        self.assertEqual(classify(unknown, 3600, live, False), 'idle')
        self.assertIsNone(classify(unknown, 3 * 3600, live, False))
        left = {**root, 'left': {'at': at(0)}}
        self.assertEqual(classify(left, 60, live, False), 'left')
        self.assertIsNone(classify(left, 9 * 3600, live, False))


class HostTests(unittest.TestCase):
    def table(self, *rows):
        lines = [f'{pid} {ppid} Thu Oct  1 {clock} 2026 {name}' for pid, ppid, clock, name in rows]
        return lambda *args: '\n'.join(lines) if '-A' in args else None

    def test_detects_named_harness_above_its_shell(self):
        ps = self.table((os.getppid(), 500, '11:00:05', '/bin/zsh'), (500, 400, '11:00:00', 'claude'),
                        (400, 300, '10:59:00', '-/bin/zsh'), (300, 1, '10:58:00', '/usr/bin/login'))
        self.assertEqual(presence.detect_host('claude', environ={}, ps=ps), {'pid': 500, 'started': STARTED})

    def test_falls_back_to_first_process_that_is_not_a_shell_or_wrapper(self):
        ps = self.table((os.getppid(), 600, '11:00:05', 'bash'), (600, 700, '11:00:04', 'timeout'),
                        (700, 800, '11:00:00', '/opt/homebrew/bin/node'), (800, 1, '10:58:00', '-zsh'))
        self.assertEqual(presence.detect_host('pi', environ={}, ps=ps)['pid'], 700)
        named = self.table((os.getppid(), 900, '11:00:05', 'bash'), (900, 901, '11:00:00', 'codex'),
                           (901, 1, '10:58:00', 'node'))
        self.assertEqual(presence.detect_host('codex', environ={}, ps=named)['pid'], 900)

    def test_override_and_unavailable_process_list(self):
        probe = lambda *args: '500 Thu Oct  1 11:00:00 2026' if '-p' in args else None
        self.assertEqual(presence.detect_host('claude', environ={'COLLAB_HOST_PID': '500'}, ps=probe),
                         {'pid': 500, 'started': STARTED})
        self.assertIsNone(presence.detect_host('claude', environ={'COLLAB_HOST_PID': '0'}, ps=probe))
        self.assertIsNone(presence.detect_host('claude', environ={}, ps=lambda *args: None))

    def test_running_matches_pid_and_start_time(self):
        own = f'{os.getpid()} Thu Oct  1 10:00:00 2026'
        ps = lambda *args: own + '\n4242 Thu Oct  1 11:00:00 2026\n4343 Thu Oct  1 09:00:00 2026'
        live = presence.running([host(4242), host(4343)], ps=ps)
        self.assertIn((4242, STARTED), live)
        self.assertNotIn((4343, STARTED), live)
        self.assertIsNone(presence.running([host(4242)], ps=lambda *args: ''))


class ListingTests(BoardCase):
    def setUp(self):
        super().setUp()
        self.call('me', 'session.register', 0, harness='claude')
        self.call('me', 'project.create', 0, id='p', name='Collab', roots=['/repo'])
        self.call('me', 'project.create', 0, id='q', name='Alonga', roots=['/alonga'])
        self.now = T0 + dt.timedelta(minutes=120)
        self.live = {(101, STARTED), (102, STARTED), (104, STARTED)}

    def wake(self, actor, minute, harness='claude', project='p', **data):
        return self.call(actor, 'session.register', minute, harness=harness, project=project, **data)

    def listing(self, actor='me', project='p', live=True):
        state = self.board.read_state()
        return presence.listing(state, self.now, actor=actor, project=project,
                                probe=lambda hosts: self.live if live else None)

    def test_groups_project_peers_other_projects_and_recent_hand_offs(self):
        self.wake('me', 110, status={**STATUS, 'uses': ['sim:iphone-16']})
        self.wake('peer', 115, 'codex', host=host(101), status={'doing': 'Viewer tab', 'summary': 'Agents tab',
                                                              'uses': ['sim:iphone-16', 'port:8765']})
        self.wake('sub', 116, parent='peer', status={'doing': 'Review', 'summary': 'Reading board.py'})
        self.wake('idler', 60, 'pi', host=host(102), status={'doing': 'Docs', 'summary': 'README pass'})
        self.wake('gone', 60, host=host(103), status={'doing': 'Old', 'summary': 'Process ended'})
        self.wake('away', 118, 'codex', project='q', status={'doing': 'LP copy', 'summary': 'Hidden here',
                                                              'uses': ['sim:iphone-16']})
        self.wake('quitter', 80, status={'doing': 'Spec', 'summary': 'Writing spec'})
        self.call('quitter', 'session.touch', 90, leave=True, summary='Left off at tests')
        self.wake('oldquit', -500, status=STATUS)
        self.call('oldquit', 'session.touch', -480, leave=True)
        self.call('peer', 'task.create', 115, title='Viewer', scope=[
            '/repo/collab_core/board.py', '/repo/collab_core/cli.py', '/repo/tests', '/repo/README.md'])
        cancelled = self.call('peer', 'task.create', 115, title='Old', scope=['/repo/old.py'])['task']
        self.call('peer', 'task.cancel', 115, task=cancelled, expected=1, reason='Dropped')

        view = self.listing()
        self.assertEqual([p['id'] for p in view['peers']], ['sub', 'peer', 'idler'])
        self.assertEqual(view['peer_count'], 3)
        self.assertEqual(view['peers'][1], {
            'id': 'peer', 'harness': 'codex', 'presence': 'active', 'ago': 5, 'doing': 'Viewer tab',
            'summary': 'Agents tab', 'uses': ['port:8765', 'sim:iphone-16'], 'clash': ['sim:iphone-16'],
            'scope': ['README.md', 'collab_core/board.py', 'collab_core/cli.py'], 'scope_more': 1})
        self.assertEqual(view['peers'][0]['parent'], 'peer')
        self.assertEqual(view['peers'][2]['presence'], 'idle')
        self.assertEqual(view['elsewhere'], [{'id': 'away', 'harness': 'codex', 'presence': 'active', 'ago': 2,
                                              'project': 'Alonga', 'doing': 'LP copy', 'uses': ['sim:iphone-16'],
                                              'clash': ['sim:iphone-16']}])
        self.assertEqual(view['left'], [{'id': 'quitter', 'harness': 'claude', 'presence': 'left', 'ago': 30,
                                         'doing': 'Spec', 'summary': 'Left off at tests'}])
        self.assertIn('gone', [p['id'] for p in self.listing(live=False)['peers']])

    def test_quiet_session_yields_to_a_newer_one_in_the_same_process(self):
        self.wake('before', 30, host=host(104), status=STATUS)
        self.wake('after', 100, host=host(104), status=STATUS)
        self.assertEqual([p['id'] for p in self.listing()['peers']], ['after'])
        self.call('before', 'post.create', 119, text='Still here')
        self.assertEqual([p['id'] for p in self.listing()['peers']], ['before', 'after'])

    def test_quiet_lists_are_omitted_and_probe_sees_only_candidates(self):
        seen = []
        self.wake('idler', 60, host=host(102), status=STATUS)
        self.wake('fresh', 119, host=host(101), status=STATUS)
        state = self.board.read_state()
        view = presence.listing(state, self.now, actor='me', project='p',
                                probe=lambda hosts: seen.extend(hosts) or self.live)
        self.assertEqual([h['pid'] for h in seen], [102])
        self.assertNotIn('elsewhere', view); self.assertNotIn('left', view)


class CliTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(); self.addCleanup(tmp.cleanup)
        self.base = Path(tmp.name)
        self.env = {**os.environ, 'COLLAB_HOST_PID': str(os.getpid())}

    def cli(self, *args, session=None, json_mode=True):
        command = [sys.executable, '-m', 'collab_core', '--home', str(self.base / 'board'),
                   '--receipts', str(self.base / 'receipts')]
        command += ['--json'] if json_mode else []
        command += ['--session', session] if session else []
        result = subprocess.run(command + list(args), cwd=ROOT, env=self.env, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
        return result.returncode, (json.loads(result.stdout) if json_mode else result.stdout)

    def test_agents_declare_see_each_other_and_hand_off(self):
        code, alpha = self.cli('wake', '--harness', 'claude', '--doing', 'Presence feature', '--summary',
                               'Designing who/doing', '--uses', 'sim:iphone-16', session='alpha')
        self.assertEqual(code, 0, alpha)
        self.assertFalse(any(' doing ' in step for step in alpha['next']), alpha['next'])
        project = alpha['project']

        code, beta = self.cli('wake', '--harness', 'codex', '--project', project, session='beta')
        self.assertEqual(code, 0, beta)
        self.assertIn(' doing ', beta['next'][0])
        seen = {p['id']: p for p in beta['brief']['peers']}
        self.assertEqual(seen['alpha']['doing'], 'Presence feature')
        self.assertEqual(seen['alpha']['summary'], 'Designing who/doing')
        self.assertEqual(seen['alpha']['harness'], 'claude')

        code, declared = self.cli('doing', 'Viewer tab', '--summary', 'Agents tab', '--uses', 'SIM:iphone-16',
                                  session='beta')
        self.assertEqual(code, 0, declared)
        self.assertEqual([c['id'] for c in declared['clashes']], ['alpha'])

        self.cli('project', 'create', 'Other', 'other', session='alpha')
        code, gamma = self.cli('wake', '--harness', 'pi', '--project', 'other', '--doing', 'Elsewhere',
                               '--summary', 'Another repo', session='gamma')
        self.assertEqual(code, 0, gamma)
        elsewhere = {p['id']: p for p in gamma['brief']['elsewhere']}
        self.assertEqual(set(elsewhere), {'alpha', 'beta'})
        self.assertNotIn('summary', elsewhere['alpha'])
        self.assertEqual(elsewhere['alpha']['project'], Path(ROOT).name)

        code, left = self.cli('bye', '--summary', 'Left off at tests', session='alpha')
        self.assertEqual(code, 0, left)
        code, who = self.cli('who', session='beta')
        self.assertEqual(code, 0, who)
        self.assertEqual(who['you']['doing'], 'Viewer tab')
        self.assertNotIn('alpha', [p['id'] for p in who['peers']])
        self.assertEqual(who['left'][0]['summary'], 'Left off at tests')
        code, text = self.cli('who', session='beta', json_mode=False)
        self.assertIn('left: claude alpha', text)
        self.assertIn('elsewhere: pi gamma', text)

    def test_wake_needs_doing_and_summary_together(self):
        code, result = self.cli('wake', '--harness', 'claude', '--doing', 'Only half', session='half')
        self.assertEqual(code, 1)
        self.assertIn('--summary', result['error'])

    def test_oversized_status_is_refused_before_anything_is_written(self):
        code, result = self.cli('wake', '--harness', 'claude', '--doing', 'x' * 81, '--summary', 'ok', session='big')
        self.assertEqual(code, 1)
        self.assertIn('80 bytes', result['error'])
        self.assertFalse((self.base / 'board' / 'events.jsonl').exists())


if __name__ == '__main__':
    unittest.main()
