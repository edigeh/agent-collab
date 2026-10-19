"""Native repository discovery against Python's `git rev-parse`, layout by layout.

Each case wakes a session and posts from a directory, once with the Python
CLI and once with the Rust CLI, and compares the automatic project and the
post's recorded repository and worktree.
"""
import json, os, subprocess, sys, tempfile, unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
BIN = Path(os.environ.get('COLLAB_RUST_BIN', ROOT / 'rust' / 'target' / 'release'))


def git(cwd, *args):
    subprocess.run(['git', '-C', str(cwd), *args], check=True, capture_output=True)


class Discovery(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(); self.addCleanup(tmp.cleanup)
        self.base = Path(tmp.name).resolve()
        repo = self.base / 'repo'
        (repo / 'sub' / 'deeper').mkdir(parents=True)
        git(repo, 'init', '-q', '-b', 'main')
        git(repo, '-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-q', '--allow-empty', '-m', 'x')
        git(repo, 'worktree', 'add', '-q', str(self.base / 'linked'), '-b', 'side')
        (self.base / 'linked' / 'inner').mkdir()
        (self.base / 'plain' / 'dir').mkdir(parents=True)
        git(self.base, 'init', '-q', '--bare', str(self.base / 'bare.git'))
        self.n = 0

    def run_in(self, build, cwd, extra_env=None):
        self.n += 1
        home = self.base / f'board{self.n}'
        env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
        env.update({'COLLAB_HOME': str(home), 'COLLAB_RECEIPTS': str(home) + '-r', 'PYTHONPATH': str(ROOT),
                    'COLLAB_HOST_PID': str(os.getpid())}, **(extra_env or {}))
        cmd = [sys.executable, '-m', 'collab_core'] if build == 'py' else [str(BIN / 'collab')]

        def cli(*args):
            done = subprocess.run(cmd + ['--json', *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=60)
            self.assertTrue(done.stdout, done.stderr)
            return json.loads(done.stdout)
        woke = cli('wake', '--harness', 'claude')
        self.assertEqual(woke.get('status', 'accepted'), 'accepted', woke)
        session = woke['session']
        posted = cli('--session', session, 'post', 'hello', '--kind', 'work')
        post_id = posted['post']
        read = cli('--session', session, 'read', post_id, '--kind', 'post')
        context = read['items'][0]['context']
        self.assertTrue(context['repository'] and context['worktree'], read)
        return woke.get('project'), context

    def test_layouts(self):
        cases = ['repo', 'repo/sub', 'repo/sub/deeper', 'repo/.git', 'linked', 'linked/inner', 'plain', 'plain/dir', 'bare.git']
        for case in cases:
            with self.subTest(case=case):
                cwd = self.base / case
                expected = self.run_in('py', cwd)
                self.assertEqual(self.run_in('rs', cwd), expected)
                self.assertEqual(self.run_in('rs', cwd, {'COLLAB_NATIVE_GIT': '0'}), expected)

    def test_harmless_env_config(self):
        extra = {'GIT_CONFIG_COUNT': '1', 'GIT_CONFIG_KEY_0': 'url.https://example.invalid/.insteadOf', 'GIT_CONFIG_VALUE_0': 'x:'}
        for case in ['repo/sub', 'linked/inner', 'plain']:
            with self.subTest(case=case):
                cwd = self.base / case
                self.assertEqual(self.run_in('rs', cwd, extra), self.run_in('py', cwd, extra))


if __name__ == '__main__':
    unittest.main()
