import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from collab_core.board import Board
from collab_core.moderator import run_moderation, parse_result, build_bundle, response_schema, ModerationUnavailable, MODELS, model_for

class ModeratorTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.path=Path(self.tmp.name);self.b=Board(self.path/'board')
        self.b.submit(self.b.request('a','session.register',{'harness':'codex'}))
        f=self.path/'truth.txt';f.write_text('The retry limit is 5.\n')
        self.proof=self.b.capture(f)
        self.post=self.b.submit(self.b.request('a','post.create',{'kind':'finding','text':'The retry limit is 10.','evidence':[self.proof]}))['post']
    def response(self, status='corrected', proof=['e0']):
        return {'outcomes':{'t0':{'status':status,'reason':'The supplied source says 5.',
                'text':'The retry limit is 5.','evidence':proof,'source_fixes':[]}}}

    def test_local_target_mapping_is_exact_and_order_independent(self):
        other=self.b.submit(self.b.request('a','post.create',{'text':'Another claim','evidence':[self.proof]}))['post']
        refs=[{'id':self.post,'revision':1},{'id':other,'revision':1}]
        bundle,proof=build_bundle(self.b,refs)
        self.assertEqual([t['post'] for t in bundle['targets']],['t0','t1'])
        self.assertNotIn(self.post,json.dumps(bundle))
        schema=response_schema(bundle)['properties']['outcomes']
        self.assertEqual(schema['required'],['t0','t1'])
        self.assertFalse(schema['additionalProperties'])
        self.assertEqual(schema['properties']['t0']['properties']['reason'],
                         {'type':'string','minLength':1,'maxLength':8000})
        raw=self.response(); raw['outcomes']['t1']={**raw['outcomes']['t0'],'text':'Second correction'}
        raw['outcomes']=dict(reversed(list(raw['outcomes'].items())))
        outcomes=parse_result(raw,refs,proof,bundle)
        self.assertEqual([o['post'] for o in outcomes],[self.post,other])
        self.assertEqual(outcomes[1]['text'],'Second correction')
        for malformed in ({'outcomes':{'t0':raw['outcomes']['t0']}},
                          {'outcomes':{'t0':raw['outcomes']['t0'],'invented':raw['outcomes']['t1']}}):
            with self.assertRaises(ModerationUnavailable):
                parse_result(malformed,refs,proof,bundle)
    def test_bounded_runner_applies_supported_correction(self):
        def runner(harness,prompt,run_dir):
            self.assertEqual(harness,'codex');self.assertIn('Use no tools',prompt)
            self.assertIn('The retry limit is 5.',prompt)
            return self.response()
        result=run_moderation(self.b,'a',force=True,runner=runner)
        self.assertEqual(result['status'],'accepted',result)
        state=self.b.read_state();self.assertEqual(state['posts'][self.post]['text'],'The retry limit is 5.')
        children=[s for s in state['sessions'].values() if s['parent']=='a'];self.assertEqual(len(children),1)
        self.assertTrue(children[0]['left'])
    def test_omp_session_services_moderation_with_its_own_model(self):
        board=Board(self.path/'omp-board')
        board.submit(board.request('o','session.register',{'harness':'omp'}))
        source=self.path/'omp-truth.txt';source.write_text('The retry limit is 5.\n')
        proof=board.capture(source)
        post=board.submit(board.request('o','post.create',{'kind':'finding','text':'The retry limit is 10.','evidence':[proof]}))['post']
        seen=[]
        def runner(harness,prompt,run_dir):
            seen.append(harness)
            return {'outcomes':{'t0':{'status':'corrected','reason':'The supplied source says 5.',
                    'text':'The retry limit is 5.','evidence':['e0'],'source_fixes':[]}}}
        result=run_moderation(board,'o',force=True,runner=runner)
        self.assertEqual(result['status'],'accepted',result)
        self.assertEqual(seen,['omp'])
        self.assertEqual(result['model'],MODELS['omp'])
        self.assertEqual(board.read_state()['posts'][post]['text'],'The retry limit is 5.')

    def test_fabricated_evidence_never_applied(self):
        result=run_moderation(self.b,'a',force=True,runner=lambda *args:self.response(proof=['invented']))
        self.assertEqual(result['status'],'pending')
        state=self.b.read_state();self.assertEqual(state['posts'][self.post]['revision'],1)
        self.assertTrue(state['posts'][self.post]['due'])
        self.assertTrue(all(l['status']=='released' for l in state['leases'].values()))
        self.assertTrue(all(s.get('left') for s in state['sessions'].values() if s['parent']=='a'))
    def test_model_failure_leaves_due_and_no_fallback(self):
        calls=[]
        def runner(*args):calls.append(args[0]);raise ModerationUnavailable('not available')
        result=run_moderation(self.b,'a',force=True,runner=runner)
        self.assertEqual(result['status'],'pending');self.assertEqual(calls,['codex'])
        self.assertTrue(self.b.read_state()['posts'][self.post]['due'])
        self.assertFalse(any('haiku' in m for m in MODELS.values()))

    def test_blank_reason_remains_pending_and_releases_worker(self):
        raw=self.response(status='supported');raw['outcomes']['t0']['reason']=''
        result=run_moderation(self.b,'a',force=True,runner=lambda *args:raw)
        self.assertEqual(result['status'],'pending')
        self.assertEqual(result['error'],'invalid moderation reason')
        state=self.b.read_state()
        self.assertTrue(state['posts'][self.post]['due'])
        self.assertTrue(all(l['status']=='released' for l in state['leases'].values()))
        self.assertTrue(all(s.get('left') for s in state['sessions'].values() if s['parent']=='a'))

    def test_model_override_is_explicit_and_invalid_value_stays_pending(self):
        with patch.dict('os.environ', {'COLLAB_MODEL_CODEX': 'gpt-6-luna'}):
            self.assertEqual(model_for('codex'), 'gpt-6-luna')
            result = run_moderation(self.b, 'a', force=True, runner=lambda *args: self.response())
            self.assertEqual(result['model'], 'gpt-6-luna')
        with patch.dict('os.environ', {'COLLAB_MODEL_PI': 'bare-model'}):
            with self.assertRaises(ModerationUnavailable):
                model_for('pi')
    def test_no_evidence_cannot_be_supported(self):
        result=run_moderation(self.b,'a',force=True,runner=lambda *args:self.response(status='supported',proof=[]))
        self.assertEqual(result['status'],'pending')
        self.assertIn('supporting evidence',result['error'])
    def test_report_evidence_is_in_model_bundle(self):
        source=self.path/'new-truth.txt';source.write_text('Independent report evidence')
        proof=self.b.capture(source)
        self.b.submit(self.b.request('a','post.report',{'post':self.post,'expected':1,'reason':'Check this','evidence':[proof]}))
        def runner(harness,prompt,run_dir):
            self.assertIn('Independent report evidence',prompt)
            return self.response()
        self.assertEqual(run_moderation(self.b,'a',force=True,runner=runner)['status'],'accepted')

    def test_correction_can_route_to_another_explicitly_affected_project(self):
        self.b.submit(self.b.request('a','project.create',{'id':'other','name':'Affected source project','roots':[str(self.path.resolve())]}))
        raw=self.response()
        raw['outcomes']['t0']['source_fixes']=[{'project':'other','source':str(Path(self.proof['source'])),'reason':'Review affected source'}]
        result=run_moderation(self.b,'a',force=True,runner=lambda *args:raw)
        self.assertEqual(result['status'],'accepted',result)
        fixes=[t for t in self.b.read_state()['tasks'].values() if t['correction']]
        self.assertEqual(len(fixes),1);self.assertEqual(fixes[0]['project'],'other')
        self.assertEqual(fixes[0]['priority'],'urgent')

    def test_failed_batch_cools_down_without_blocking_other_posts(self):
        for n in range(9):
            self.b.submit(self.b.request('a','post.create',{'text':f'Claim {n}'}))
        original={p['id'] for p in self.b.due('global')}
        result=run_moderation(self.b,'a',runner=lambda *args: {'outcomes':{}})
        self.assertEqual(result['status'],'pending')
        automatic={p['id'] for p in self.b.due('global')}
        self.assertTrue(automatic)
        self.assertFalse(original & automatic)
        forced={p['id'] for p in self.b.due('global',force=True)}
        self.assertEqual(forced,original)
        with patch('collab_core.board.utcnow',return_value='2100-01-01T00:00:00+00:00'):
            all_ready={p['id'] for p in self.b.due('global',limit=100)}
        self.assertTrue(original <= all_ready)

    def test_old_small_batch_is_serviced_without_force(self):
        with patch('collab_core.board.utcnow',return_value='2000-01-01T00:00:00+00:00'):
            self.b.submit(self.b.request('a','post.create',{'text':'Old unattended claim'}))
        self.assertTrue(self.b.due('global'))

    def test_oversized_batch_is_split_before_claiming(self):
        self.b.submit(self.b.request('a','project.attach',{'project':'global','expected':1,'root':str(self.path)}))
        for n in range(4):
            proofs=[]
            for x in range(4):
                path=self.path/f'proof-{n}-{x}';path.write_text(f'{n}-{x} '+('evidence '*600))
                proofs.append(self.b.capture(path))
            self.b.submit(self.b.request('a','post.create',{'text':f'Large {n}','evidence':proofs}))
        seen=[]
        def runner(harness,prompt,run_dir):
            bundle=json.loads(prompt.split('EVIDENCE_BUNDLE:\n',1)[1]);seen.append(bundle)
            return {'outcomes':{t['post']:{'status':'unsupported','reason':'Unrelated evidence',
                    'text':'','evidence':[],'source_fixes':[]} for t in bundle['targets']}}
        result=run_moderation(self.b,'a',force=True,runner=runner)
        self.assertEqual(result['status'],'accepted',result)
        self.assertLess(len(seen[0]['targets']),4)
        self.assertLessEqual(len(json.dumps(seen[0]).encode()),64000)

if __name__=='__main__':unittest.main()
