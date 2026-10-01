import http.client
import json
from pathlib import Path
import tempfile
import threading
import unittest
from collab_core.board import Board
from collab_core.viewer import make_server, snapshot

class ViewerTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.addCleanup(self.tmp.cleanup)
        self.board = Board(Path(self.tmp.name) / 'board')
        self.call('session.register', harness='codex', name='Test agent')
    def call(self, op, **data):
        result = self.board.submit(self.board.request('test', op, data))
        self.assertEqual(result['status'], 'accepted', result)
        return result
    def start(self):
        self.server = make_server(self.board.root, 0)
        thread = threading.Thread(target=self.server.serve_forever, daemon=True); thread.start()
        self.addCleanup(self.server.server_close); self.addCleanup(self.server.shutdown)
    def get(self, path, method='GET', headers=None):
        conn = http.client.HTTPConnection('127.0.0.1', self.server.server_port)
        self.addCleanup(conn.close); conn.request(method, path, headers=headers or {})
        response = conn.getresponse(); return response.status, dict(response.getheaders()), response.read()
    def test_snapshot_preserves_history_and_does_not_post_or_ack(self):
        p = self.call('post.create', text='Original claim')['post']
        self.call('post.create', text='A linked discussion', refs=[{'id':p,'revision':1}])
        self.call('post.report', post=p, expected=1, reason='Check this claim')
        before = self.board.journal.path.read_bytes()
        result = snapshot(self.board)
        self.assertEqual(self.board.journal.path.read_bytes(), before)
        self.assertEqual(result['posts'][0]['status'], 'disputed')
        self.assertEqual([x['operation'] for x in result['history'][p]], ['post.create','post.report'])
        self.assertEqual(result['history'][p][0]['detail']['text'], 'Original claim')

    def test_snapshot_lists_online_agents_with_status(self):
        self.call('session.touch', status={'doing': 'Viewer tab', 'summary': 'Agents tab', 'uses': ['port:8765']})
        for view in ('full', 'compact'):
            agents = snapshot(self.board, view=view)['presence']
            self.assertEqual([(a['id'], a['presence'], a['doing'], a['project']) for a in agents],
                             [('test', 'active', 'Viewer tab', 'global')])
            self.assertEqual(agents[0]['uses'], ['port:8765'])

    def test_compact_snapshot_omits_history_bodies(self):
        self.call('post.create', text='Compact discovery')
        result = snapshot(self.board, view='compact')
        self.assertEqual(result['view'], 'compact')
        self.assertEqual(result['posts'][0]['text'], 'Compact discovery')
        self.assertIn('history_counts', result)
        self.assertNotIn('history', result)

    def test_invalid_snapshot_view_is_rejected(self):
        with self.assertRaises(ValueError):
            snapshot(self.board, view='unknown')
    def test_http_assets_api_and_security_boundaries(self):
        self.call('post.create', text='<script>alert(1)</script>')
        self.start()
        status, headers, body = self.get('/api/board')
        self.assertEqual(status, 200); self.assertIn('<script>', json.loads(body)['posts'][0]['text'])
        status, _, body = self.get('/api/board?view=compact')
        self.assertEqual(status, 200); self.assertEqual(json.loads(body)['view'], 'compact')
        self.assertEqual(self.get('/api/board?view=unknown')[0], 400)
        self.assertNotIn('Access-Control-Allow-Origin', headers)
        self.assertIn("frame-ancestors 'none'", headers['Content-Security-Policy'])
        for path in ['/', '/app.js', '/style.css']:
            self.assertEqual(self.get(path)[0], 200)
        for path in ['/events.jsonl', '/../viewer.py', '/artifacts/anything']:
            self.assertEqual(self.get(path)[0], 404)
        self.assertEqual(self.get('/api/board', headers={'Host':'evil.example'})[0], 403)
        self.assertEqual(self.get('/api/board', headers={'Origin':'https://evil.example'})[0], 403)
        self.assertEqual(self.get('/api/board', method='POST')[0], 501)
    def test_fresh_data_and_corruption_error(self):
        self.start(); self.assertEqual(len(json.loads(self.get('/api/board')[2])['posts']),0)
        self.call('post.create', text='New post')
        self.assertEqual(len(json.loads(self.get('/api/board')[2])['posts']),1)
        with self.board.journal.path.open('ab') as stream: stream.write(b'broken\n')
        status, _, body = self.get('/api/board')
        self.assertEqual(status,503); self.assertIn('Board unavailable',json.loads(body)['error'])
