"""The Rust viewer serves the same routes, headers and JSON as the Python one."""
import http.client, json, os, subprocess, sys, tempfile, unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT))
from collab_core.board import Board
from collab_core.viewer import snapshot

BIN = Path(os.environ.get('COLLAB_RUST_BIN', ROOT / 'rust' / 'target' / 'release'))


def normalized(value):
    # Presence ages depend on when each side looked.
    for agent in value.get('presence', []):
        agent.pop('ago', None)
    return value


class RustViewer(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(); self.addCleanup(tmp.cleanup)
        self.board = Board(Path(tmp.name) / 'board')
        self.call('session.register', harness='codex', name='Test agent')

    def call(self, op, actor='test', **data):
        result = self.board.submit(self.board.request(actor, op, data))
        self.assertEqual(result['status'], 'accepted', result)
        return result

    def start(self):
        self.proc = subprocess.Popen([str(BIN / 'collab-viewer'), '--home', str(self.board.root), '--port', '0'],
                                     stdout=subprocess.PIPE, text=True)
        self.addCleanup(self.proc.stdout.close); self.addCleanup(self.proc.wait); self.addCleanup(self.proc.kill)
        line = self.proc.stdout.readline()
        self.assertTrue(line.startswith('Agent Collab viewer: http://127.0.0.1:'), line)
        self.port = int(line.rsplit(':', 1)[1])

    def get(self, path, method='GET', headers=None):
        conn = http.client.HTTPConnection('127.0.0.1', self.port)
        self.addCleanup(conn.close); conn.request(method, path, headers=headers or {})
        response = conn.getresponse(); return response.status, dict(response.getheaders()), response.read()

    def test_snapshot_matches_python(self):
        p = self.call('post.create', text='Original claim')['post']
        self.call('post.create', text='A linked discussion', refs=[{'id': p, 'revision': 1}])
        self.call('post.report', post=p, expected=1, reason='Check this claim')
        self.call('task.create', title='Do the thing', owner='test', scope=['resource:x'])
        self.call('session.touch', status={'doing': 'Viewer tab', 'summary': 'Agents tab', 'uses': ['port:8765']})
        self.start()
        for view in ('full', 'compact'):
            status, _, body = self.get('/api/board?view=' + view)
            self.assertEqual(status, 200)
            self.assertEqual(normalized(json.loads(body)), normalized(snapshot(self.board, view=view)))

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
        web = ROOT / 'collab_core' / 'web'
        for path, name in [('/', 'index.html'), ('/app.js', 'app.js'), ('/style.css', 'style.css')]:
            status, _, body = self.get(path)
            self.assertEqual(status, 200); self.assertEqual(body, (web / name).read_bytes())
        for path in ['/events.jsonl', '/../viewer.py', '/artifacts/anything']:
            self.assertEqual(self.get(path)[0], 404)
        self.assertEqual(self.get('/api/board', headers={'Host': 'evil.example'})[0], 403)
        self.assertEqual(self.get('/api/board', headers={'Origin': 'https://evil.example'})[0], 403)
        self.assertEqual(self.get('/api/board', method='POST')[0], 501)

    def test_fresh_data_and_corruption_error(self):
        self.start(); self.assertEqual(len(json.loads(self.get('/api/board')[2])['posts']), 0)
        self.call('post.create', text='New post')
        self.assertEqual(len(json.loads(self.get('/api/board')[2])['posts']), 1)
        with self.board.journal.path.open('ab') as stream: stream.write(b'broken\n')
        status, _, body = self.get('/api/board')
        self.assertEqual(status, 503); self.assertIn('Board unavailable', json.loads(body)['error'])

    def test_help_and_argument_errors(self):
        done = subprocess.run([str(BIN / 'collab-viewer'), '--help'], capture_output=True, text=True)
        self.assertEqual(done.returncode, 0); self.assertIn('usage: collab-viewer', done.stdout)
        done = subprocess.run([str(BIN / 'collab-viewer'), '--port', 'x'], capture_output=True, text=True)
        self.assertEqual(done.returncode, 2)
        self.assertIn("collab-viewer: error: argument --port: invalid int value: 'x'", done.stderr)


if __name__ == '__main__':
    unittest.main()
