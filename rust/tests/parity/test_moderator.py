"""Exercise both CLIs with a bounded synthetic moderator, never a real board."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT))
from collab_core.board import Board

FAKE = r'''
import json, os, pathlib, sys
args=sys.argv[1:]; prompt=sys.stdin.read()
bundle=json.loads(prompt.split('EVIDENCE_BUNDLE:\n',1)[1])
schema=json.loads(pathlib.Path(args[args.index('--output-schema')+1]).read_text())
labels=[t['post'] for t in bundle['targets']]
assert labels == ['t'+str(n) for n in range(len(labels))]
assert schema['properties']['outcomes']['required'] == labels
assert not schema['properties']['outcomes']['additionalProperties']
assert schema['properties']['outcomes']['properties']['t0']['properties']['reason'] == {'type':'string','minLength':1,'maxLength':8000}
out={label:{'status':'unsupported','reason':'No evidence supplied',
     'text':'','evidence':[],'source_fixes':[]} for label in reversed(labels)}
if os.environ.get('TEST_BAD_MODERATOR'): out.pop(labels[-1])
pathlib.Path(args[args.index('--output-last-message')+1]).write_text(json.dumps({'outcomes':out}))
'''

class ModeratorParity(unittest.TestCase):
    def run_case(self, implementation, bad=False):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp); fake=root/'bin';fake.mkdir()
            binary=fake/'codex';binary.write_text('#!'+sys.executable+'\n'+FAKE);binary.chmod(0o700)
            board=Board(root/'board')
            board.submit(board.request('a','session.register',{'harness':'codex'}))
            posts=[]
            for n in range(10):
                posts.append(board.submit(board.request('a','post.create',{'text':f'Claim {n}'}))['post'])
            env={k:v for k,v in os.environ.items() if not k.startswith('COLLAB_')}
            env.update(COLLAB_HOME=str(board.root),COLLAB_RECEIPTS=str(board.receipt_root),
                       PATH=str(fake)+os.pathsep+os.defpath,PYTHONPATH=str(ROOT))
            if bad:env['TEST_BAD_MODERATOR']='1'
            cli=[sys.executable,'-m','collab_core'] if implementation=='python' else [str(ROOT/'rust/target/release/collab')]
            result=subprocess.run(cli+['--json','--session','a','moderate'],env=env,cwd=root,
                                  text=True,capture_output=True,timeout=20)
            self.assertEqual(result.returncode,0,result.stderr)
            value=json.loads(result.stdout)
            error_file=Path(value.get('run_dir',''))/'stderr.txt'
            self.assertEqual(value['status'],'pending' if bad else 'accepted',
                             (value,error_file.read_text() if error_file.is_file() else ''))
            state=board.read_state()
            self.assertTrue(all(s.get('left') for s in state['sessions'].values() if s['parent']=='a'))
            reviewed=[p for p in state['posts'].values() if not p['due']]
            if bad:
                self.assertEqual(reviewed,[])
                failed=set(posts[:4])
                retry=json.loads(subprocess.check_output(cli+['--json','status'],env=env,cwd=root,text=True))
                self.assertTrue(retry['moderation_due'])
                self.assertFalse(failed & {p['id'] for p in retry['moderation_due']})
                self.assertTrue(all(l['status']=='released' for l in state['leases'].values()))
            else:
                self.assertEqual({p['id'] for p in reviewed},set(posts[:4]))
                self.assertEqual({p['status'] for p in reviewed},{'unsupported'})
                run=Path(value['run_dir'])
                self.assertEqual([r['id'] for r in json.loads((run/'targets.json').read_text())],posts[:4])
            verify=json.loads(subprocess.check_output(cli+['--json','verify'],env=env,cwd=root,text=True))
            self.assertTrue(verify['match'])

    def test_complete_reordered_results_map_to_exact_posts(self):
        for implementation in ['python','rust']:
            with self.subTest(implementation=implementation):self.run_case(implementation)

    def test_missing_target_releases_and_other_work_can_proceed(self):
        for implementation in ['python','rust']:
            with self.subTest(implementation=implementation):self.run_case(implementation,bad=True)

if __name__=='__main__':unittest.main()
