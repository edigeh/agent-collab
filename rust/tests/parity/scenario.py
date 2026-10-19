"""Run one scripted session against a CLI and print every result, normalized.

    python3 scenario.py python|rust OUT.json

IDs, hashes, timestamps and the temporary base path are replaced by
placeholders numbered in order of first appearance, so two runs of the same
script against different builds can be compared byte for byte.
"""
import json, os, re, subprocess, sys, tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
BIN = Path(os.environ.get('COLLAB_RUST_BIN', ROOT / 'rust' / 'target' / 'release'))

TOKENS = re.compile(r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}'
                    r'|\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?(?:[+-]\d\d:\d\d|Z)?'
                    r'|(?<![0-9a-zA-Z])[0-9a-f]{16,64}(?![0-9a-zA-Z])'
                    r'|[A-Za-z0-9_-]{40,}')


def main(which, out_path):
    base = Path(tempfile.mkdtemp(prefix='collab-scenario-'))
    project = base / 'work'; project.mkdir()
    (project / 'notes.txt').write_text('first version\n')
    (project / 'proof.txt').write_bytes('évidence ✓\n'.encode())
    env = {k: v for k, v in os.environ.items() if not k.startswith('COLLAB_')}
    env.update({'COLLAB_HOME': str(base / 'board'), 'COLLAB_RECEIPTS': str(base / 'receipts'),
                'COLLAB_HOST_PID': str(os.getpid()), 'PYTHONPATH': str(ROOT), 'HOME': str(base / 'home'),
                'COLUMNS': '80', 'PATH': '/usr/bin:/bin'})
    cli = [sys.executable, '-m', 'collab_core'] if which == 'python' else [str(BIN / 'collab')]
    seen = {}
    results = []

    def norm(text):
        text = text.replace(str(base), '<BASE>')
        def sub(m):
            key = m.group(0)
            if key not in seen:
                seen[key] = f'<{len(seen)}>'
            return seen[key]
        return TOKENS.sub(sub, text)

    def run(*args, session=None, human=False, cwd=project):
        argv = list(cli) + (['--json'] if not human else []) + (['--session', session] if session else []) + list(args)
        done = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, timeout=60)
        results.append({'args': [norm(a) for a in args], 'session': session, 'human': human, 'code': done.returncode,
                        'stdout': norm(done.stdout), 'stderr_empty': not done.stderr.strip()})
        try:
            return json.loads(done.stdout)
        except ValueError:
            return {}

    alpha = run('wake', '--harness', 'claude', '--doing', 'Port', '--summary', 'Rust port', '--uses', 'port:8765', session='alpha')
    beta = run('wake', '--harness', 'codex', session='beta')
    run('wake', '--harness', 'pi', session='alpha')
    run('doing', 'Review', '--summary', 'Reviewing the port', '--uses', 'PORT:8765', session='beta')
    run('who', session='alpha'); run('who', session='alpha', human=True); run('who')
    p1 = run('post', 'Hello beta', '--to', 'beta', session='alpha')
    p2 = run('post', 'Finding with proof', '--kind', 'finding', '--source', str(project / 'proof.txt'),
             '--scope', 'notes.txt', '--scope', 'resource:db', session='alpha')
    run('post', 'Bad ref', '--ref', 'nope', session='alpha')
    run('post', 'Ref', '--ref', f"{p1.get('post')}:1", session='beta')
    run('post', 'Ref zero', '--ref', f"{p1.get('post')}:0", session='beta')
    inbox = run('inbox', session='beta')
    run('inbox', '--view', 'full', '--limit', '2', session='beta')
    run('inbox', '--budget', '600', session='alpha')
    run('ack', inbox.get('delivery', 'x'), session='beta')
    run('read', p2.get('post', 'x'), session='beta'); run('read', p2.get('post', 'x'), session='beta', human=True)
    run('read', p2.get('post', 'x'), '--view', 'compact', session='beta')
    t1 = run('task', 'create', 'Port the CLI', '--scope', 'src/cli.rs', '--priority', 'urgent', session='alpha')
    run('task', 'create', 'Clash', '--scope', 'src/cli.rs', '--owner', 'beta', session='beta')
    t2 = run('task', 'create', 'Second task', '--owner', 'beta', '--dependency', t1.get('task', 'x'), session='alpha')
    run('task', 'claim', t2.get('task', 'x'), '--expected', '1', session='beta')
    run('task', 'block', t2.get('task', 'x'), '--expected', '2', '--reason', 'waiting', session='beta')
    run('task', 'finish', t1.get('task', 'x'), '--expected', '1', session='alpha')
    run('task', 'finish', t1.get('task', 'x'), '--expected', '1', '--source', str(project / 'notes.txt'), session='alpha')
    run('task', 'cancel', t2.get('task', 'x'), '--expected', '3', '--reason', 'not needed', session='alpha')
    run('task', 'list', session='alpha'); run('task', 'list', '--view', 'compact', session='alpha', human=True)
    run('status', session='alpha'); run('status', '--view', 'compact', session='alpha', human=True)
    run('report', p2.get('post', 'x'), '--expected', '1', '--reason', 'Contradiction', '--source', str(project / 'notes.txt'), session='beta')
    req = run('post', 'Please check', '--kind', 'request', '--to', 'beta', session='alpha')
    run('status', session='beta')
    sha = next((e['sha256'] for e in run('read', p2.get('post', 'x'), session='alpha').get('items', [{}])[0].get('evidence', [])), 'x')
    run('artifact', sha); run('artifact', sha, '--display', human=True); run('artifact', sha, '--display', '--path')
    run('artifact', 'ab' * 32)
    run('project', 'list'); run('project', 'create', 'Other', 'other', '--root', str(base / 'other'), session='alpha')
    run('project', 'attach', 'other', str(base / 'more'), '--expected', '1', session='alpha')
    run('project', 'list', human=True)
    run('wake', '--harness', 'omp', '--project', 'other', '--doing', 'Elsewhere', '--summary', 'Other repo', session='gamma')
    run('who', session='gamma', human=True)
    preview = run('cleanup', 'preview', sha)
    run('cleanup', 'remove', sha, '--confirm', 'wrong', session='alpha')
    run('cleanup', 'remove', sha, '--confirm', preview.get('token', 'x'), session='alpha')
    run('artifact', sha)
    run('moderate', session='alpha'); run('moderate', '--force', session='beta')
    run('sync', session='alpha'); run('verify'); run('verify', human=True)
    run('bye', '--summary', 'Left off at tests', session='alpha')
    run('who', session='beta', human=True)
    # Argument handling and errors.
    for args in (['wake'], ['wake', '--harness', 'nope'], ['wake', '--harness', 'claude', '--doing', 'x'],
                 ['wake', '--harness', 'claude', '--doing', 'x' * 81, '--summary', 's'],
                 ['frobnicate'], [], ['post'], ['post', 'x', '--kind'], ['--sess', 'alpha', 'inbox'], ['--s', 'x', 'who'],
                 ['who', '--json'], ['inbox', '--limit', 'x'], ['inbox', '--limit', '0'], ['inbox', '--limit=3', '--off', '1'],
                 ['task', 'claim', 'x', '--expected', '1', '--takeover'], ['task'], ['task', 'list', '--view', 'nope'],
                 ['project', 'attach', 'p', '/x'], ['cleanup', 'resume', 'nothing'], ['read', 'missing'],
                 ['--', 'who'], ['post', '--', '--dash text'], ['inbox', '--cursor', 'garbage'], ['inbox', '--cursor', 'x', '--offset', '1'],
                 ['bye', '--summary', ' '], ['who', '--project', 'nope'], ['--json=1', 'who'], ['-hx'], ['who', '-h'], ['task', 'create', '--help']):
        run(*args, session='beta')
        run(*args, session='beta', human=True)
    run('inbox')
    run('who', session='nobody')
    run('task', 'list', session='nobody')
    Path(out_path).write_text(json.dumps(results, indent=1, ensure_ascii=False) + '\n')


if __name__ == '__main__':
    main(sys.argv[1], sys.argv[2])
