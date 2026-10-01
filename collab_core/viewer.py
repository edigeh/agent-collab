"""Loopback-only, read-only web viewer. Run: python3 -m collab_core.viewer."""
from __future__ import annotations
import argparse
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit
from . import presence
from .board import BRIEF_VIEWS, Board, render_item, utcnow
from .state import timestamp
from .journal import JournalError

ASSETS = Path(__file__).with_name('web')


def snapshot(board, view='full'):
    if view not in BRIEF_VIEWS:
        raise ValueError('view must be compact or full')
    # Use the canonical journal and reducer, never trust the disposable view export.
    with board.journal.locked() as log:
        state, _ = board._load(log.records)
        history = {}
        for record in log.records:
            event = record['payload']
            if not event['accepted']:
                continue
            req = event['request']; data = req['data']
            targets = [req['id']] if req['op'] == 'post.create' else []
            if req['op'].startswith('post.') and data.get('post'):
                targets.append(data['post'])
            if req['op'] == 'moderation.apply':
                targets += [o['post'] for o in data.get('outcomes', [])]
            for target in targets:
                detail = next((o for o in data.get('outcomes', []) if o['post'] == target), data)
                history.setdefault(target, []).append({'operation': req['op'], 'actor': req['actor'],
                    'at': event['recorded_at'], 'detail': detail})
        sequence = log.records[-1]['seq'] if log.records else 0
    # The process check runs after the lock is released so writers never wait on ps.
    agents = presence.everyone(state, timestamp(utcnow()))
    if view == 'compact':
        return {
            'view': 'compact',
            'projects': [{'id': p['id'], 'name': p['name'], 'revision': p['revision']} for p in state['projects'].values()],
            'sessions': [{'id': s['id'], 'name': s.get('name'), 'harness': s['harness'],
                          'parent': s.get('parent'), 'project': s['project'], 'seen_at': s['seen_at']}
                         for s in state['sessions'].values()],
            'posts': [render_item('post', p, 'compact') for p in state['posts'].values()],
            'tasks': [render_item('task', t, 'compact') for t in state['tasks'].values()],
            'history_counts': {key: len(value) for key, value in history.items()},
            'presence': agents, 'sequence': sequence,
        }
    return {key: list(state[key].values()) for key in ('projects', 'sessions', 'posts', 'tasks')} | {
        'view': 'full', 'history': history, 'presence': agents, 'sequence': sequence}


def make_server(home, port=8765, receipts=None):
    board = Board(home, receipt_root=receipts)

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            # Reject foreign Host/Origin headers so another website cannot read local boards.
            allowed = {f'127.0.0.1:{self.server.server_port}', f'localhost:{self.server.server_port}'}
            origin = self.headers.get('Origin')
            if self.headers.get('Host') not in allowed or (origin and origin not in {'http://' + h for h in allowed}):
                self.reply(403, b'Forbidden', 'text/plain'); return
            parsed = urlsplit(self.path)
            path = parsed.path
            if path == '/api/board':
                try:
                    view = parse_qs(parsed.query).get('view', ['full'])[0]
                    if view not in BRIEF_VIEWS:
                        self.reply(400, json.dumps({'error': 'view must be compact or full'}).encode(), 'application/json'); return
                    body = json.dumps(snapshot(board, view=view), ensure_ascii=False).encode()
                    self.reply(200, body, 'application/json')
                except (JournalError, OSError, ValueError) as exc:
                    self.reply(503, json.dumps({'error': 'Board unavailable: ' + str(exc)}).encode(), 'application/json')
                return
            assets = {'/': ('index.html', 'text/html'), '/app.js': ('app.js', 'text/javascript'), '/style.css': ('style.css', 'text/css')}
            if path not in assets:
                self.reply(404, b'Not found', 'text/plain'); return
            name, mime = assets[path]
            self.reply(200, (ASSETS / name).read_bytes(), mime)

        def reply(self, status, body, mime):
            self.send_response(status)
            self.send_header('Content-Type', mime + '; charset=utf-8')
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Cache-Control', 'no-store')
            self.send_header('X-Content-Type-Options', 'nosniff')
            self.send_header('Content-Security-Policy', "default-src 'self'; script-src 'self'; style-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'")
            self.end_headers(); self.wfile.write(body)

    return ThreadingHTTPServer(('127.0.0.1', port), Handler)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--home', default=os.environ.get('COLLAB_HOME', '~/.agent-collab'))
    parser.add_argument('--receipts', default=os.environ.get('COLLAB_RECEIPTS'))
    parser.add_argument('--port', type=int, default=8765)
    args = parser.parse_args()
    server = make_server(args.home, args.port, args.receipts)
    print(f'Agent Collab viewer: http://127.0.0.1:{server.server_port}', flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()

if __name__ == '__main__':
    main()
