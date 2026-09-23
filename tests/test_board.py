import copy
import json
from pathlib import Path
import tempfile
import unittest
from collab_core.board import Board
from collab_core import state

class BoardTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name); self.board = Board(self.path / 'board')
        self.counter = 0
        self.call('a', 'session.register', harness='codex')
        self.call('b', 'session.register', harness='pi')
        self.call('a', 'project.create', id='p', name='Example')
        self.call('a', 'session.touch', project='p'); self.call('b', 'session.touch', project='p')
    def call(self, actor, op, **data):
        self.counter += 1
        result = self.board.submit(self.board.request(actor, op, data, f'r{self.counter}'))
        self.assertEqual(result['status'], 'accepted', result)
        return result
    def proof(self, text='Observed result'):
        path = self.path / f'evidence-{self.counter}.txt'; path.write_text(text)
        return self.board.capture(path)
    def test_receipt_duplicate_and_rebuild(self):
        req = self.board.request('a', 'post.create', {'project':'p', 'text':'hello'}, 'same')
        first = self.board.submit(req); again = self.board.submit(req)
        self.assertEqual(first, again)
        before = self.board.read_state(); self.board.cache_path.unlink()
        self.assertEqual(self.board.read_state(), before)
        self.assertEqual(self.board.sync('a'), [])
    def test_conflicting_claim_is_durable_rejection(self):
        t = self.call('a', 'task.create', title='change one', scope=['src/foo.py'])['task']
        req = self.board.request('b', 'task.create', {'title':'change two', 'scope':['src/foo.py']})
        result = self.board.submit(req); self.assertEqual(result['status'], 'rejected')
        self.assertIn(t, result['error']); self.assertEqual(self.board.submit(req), result)
    def test_post_delivery_ack_does_not_close_request(self):
        result = self.call('a', 'post.create', kind='request', text='Please inspect', to='b')
        brief = self.board.brief('b'); self.assertIn(result['task'], [x['id'] for x in brief['items']])
        self.call('b', 'delivery.ack', delivery=brief['delivery'])
        self.assertEqual(self.board.read_state()['tasks'][result['task']]['status'], 'open')
        self.assertIn(result['task'], [x['id'] for x in self.board.brief('b')['items']])
    def test_moderation_invalidates_summary_and_opens_urgent_fix(self):
        proof = self.proof('Actual limit is 5')
        post = self.call('a', 'post.create', kind='finding', text='Limit is 10', evidence=[proof])['post']
        summary = self.call('a', 'post.create', kind='summary', text='Use limit 10', refs=[{'id':post,'revision':1}])['post']
        self.board.brief('b')
        lease = self.call('a', 'moderation.claim', posts=[{'id':post,'revision':1}])['lease']
        self.call('a', 'moderation.apply', lease=lease, outcomes=[{'post':post, 'status':'corrected',
            'text':'Limit is 5', 'reason':'Source states 5', 'evidence':[proof],
            'source_fixes':[{'project':'p','source':proof['source'],'reason':'Correct downstream documentation'}]}])
        s=self.board.read_state()
        self.assertTrue(s['posts'][summary]['stale'])
        self.assertEqual(s['posts'][post]['text'], 'Limit is 5')
        self.assertTrue(any(n['recipient']=='b' for n in s['notices'].values()))
        fixes=[t for t in s['tasks'].values() if t['correction']]
        self.assertEqual(len(fixes),1); self.assertEqual(fixes[0]['priority'],'urgent')
        snapshot=copy.deepcopy(s); self.board.cache_path.unlink(); self.assertEqual(snapshot,self.board.read_state())
    def test_source_changed_after_moderator_snapshot_is_rejected(self):
        proof=self.proof('before'); post=self.call('a','post.create',kind='finding',text='before',evidence=[proof])['post']
        lease=self.call('a','moderation.claim',posts=[{'id':post,'revision':1}])['lease']
        Path(proof['source']).write_text('after')
        result=self.board.submit(self.board.request('a','moderation.apply',{'lease':lease,'outcomes':[
            {'post':post,'status':'supported','reason':'source matches','evidence':[proof]}]}))
        self.assertEqual(result['status'],'rejected', result)
        self.assertTrue(self.board.read_state()['posts'][post]['due'])
    def test_accepted_retry_is_independent_of_later_source_change(self):
        proof=self.proof('before'); task=self.call('a','task.create',title='Check source')['task']
        req=self.board.request('a','task.finish',{'task':task,'expected':1,'evidence':[proof]},'finish')
        first=self.board.submit(req); self.assertEqual(first['status'],'accepted')
        Path(proof['source']).write_text('after')
        self.assertEqual(self.board.submit(req),first)
    def test_stale_moderator_cannot_overwrite_report(self):
        post=self.call('a','post.create',kind='finding',text='claim')['post']
        lease=self.call('a','moderation.claim',posts=[{'id':post,'revision':1}])['lease']
        self.call('b','post.report',post=post,expected=1,reason='Contradictory evidence')
        res=self.board.submit(self.board.request('a','moderation.apply',{'lease':lease,'outcomes':[
            {'post':post,'status':'unsupported','reason':'No evidence'}]}))
        self.assertEqual(res['status'],'rejected')
        self.assertEqual(self.board.read_state()['posts'][post]['status'],'disputed')
    def test_corrupt_cache_is_rebuilt(self):
        before=self.board.read_state(); self.board.cache_path.write_text('{broken')
        self.assertEqual(self.board.read_state(),before)
    def test_offline_receipt_eventually_acknowledged(self):
        from unittest.mock import patch
        from collab_core.journal import JournalBusy
        req=self.board.request('a','post.create',{'text':'Worked outside board','kind':'work','scope':['src/foo.py']})
        with patch.object(self.board,'_ingest',side_effect=JournalBusy('temporarily busy')):
            self.assertEqual(self.board.submit(req)['status'],'pending')
        self.assertEqual(self.board.sync('a')[0]['status'],'accepted')
        self.assertEqual(self.board.sync('a'),[])
        self.assertIn(req['id'],self.board.read_state()['posts'])
    def test_unregistered_receipts_register_before_backfill(self):
        from unittest.mock import patch
        from collab_core.journal import JournalBusy
        register=self.board.request('c','session.register',{'harness':'claude','project':'p'})
        work=self.board.request('c','post.create',{'kind':'work','text':'Recovered work; transcript gap noted'})
        with patch.object(self.board,'_ingest',side_effect=JournalBusy('busy')):
            self.board.submit(register);self.board.submit(work)
        self.assertEqual([r['status'] for r in self.board.sync('c')],['accepted','accepted'])
    def test_new_submit_reconciles_earlier_registration(self):
        from unittest.mock import patch
        from collab_core.journal import JournalBusy
        req=self.board.request('new','session.register',{'harness':'pi','project':'p'})
        with patch.object(self.board,'_ingest',side_effect=JournalBusy('busy')):
            self.board.submit(req)
        work=self.board.request('new','post.create',{'kind':'work','text':'Work after board recovery'})
        self.assertEqual(self.board.submit(work)['status'],'accepted')
        self.assertIn(work['id'],self.board.read_state()['posts'])

    def test_forged_cache_cannot_authorize_new_event(self):
        from collab_core.board import canonical,digest
        task=self.call('a','task.create',title='Owned by a')['task']
        self.board.read_state(); cache=json.loads(self.board.cache_path.read_text())
        cache['state']['tasks'][task]['owner']='b';cache.pop('sha256')
        cache['sha256']=digest(canonical(cache));self.board.cache_path.write_bytes(canonical(cache))
        res=self.board.submit(self.board.request('b','task.block',{'task':task,'expected':1,'reason':'Forged ownership'}))
        self.assertEqual(res['status'],'rejected')
        self.board.cache_path.unlink();self.assertEqual(self.board.read_state()['tasks'][task]['owner'],'a')
    def test_malformed_nested_receipt_does_not_stall_following_work(self):
        bad=self.board.submit(self.board.request('a','delivery.prepare',{'items':['bad']}))
        self.assertEqual(bad['status'],'rejected')
        self.call('a','post.create',text='Later valid message')
        self.assertEqual(self.board.sync('a'),[])
    def test_derived_request_task_cannot_overwrite_assignment(self):
        task=self.board.submit(self.board.request('a','task.create',{'title':'Keep me','scope':['critical']},'req:task'))
        self.assertEqual(task['status'],'accepted')
        result=self.board.submit(self.board.request('b','post.create',{'kind':'request','text':'replace','to':'b'},'req'))
        self.assertEqual(result['status'],'rejected')
        self.assertEqual(self.board.read_state()['tasks']['req:task']['title'],'Keep me')
    def test_rejected_receipt_remains_visible_after_sync(self):
        from unittest.mock import patch
        from collab_core.journal import JournalBusy
        req=self.board.request('a','post.create',{'text':'work','project':'missing'})
        with patch.object(self.board,'_ingest',side_effect=JournalBusy('busy')):self.board.submit(req)
        self.assertEqual(self.board.sync('a')[0]['status'],'rejected')
        briefing=self.board.brief('a')
        self.assertTrue(any(x['kind']=='notice' and x['target']==req['id'] for x in briefing['items']))
    def test_occurrence_time_and_multiple_report_evidence_survive(self):
        req=self.board.request('a','post.create',{'text':'Earlier work','kind':'finding'})
        req['observed_at']='2020-01-01T00:00:00+00:00';self.assertEqual(self.board.submit(req)['status'],'accepted')
        post=self.board.read_state()['posts'][req['id']]
        self.assertEqual(post['observed_at'],req['observed_at']);self.assertNotEqual(post['ingested_at'],post['observed_at'])
        proof=self.proof('conflict')
        self.call('b','post.report',post=req['id'],expected=1,reason='First',evidence=[proof])
        self.call('b','post.report',post=req['id'],expected=2,reason='Second',evidence=[proof])
        reports=self.board.read_state()['posts'][req['id']]['reports'];self.assertEqual(len(reports),2)
        self.assertEqual(reports[0]['evidence'][0]['sha256'],proof['sha256'])

    def test_correction_cannot_close_without_evidence(self):
        s=state.empty()
        s['sessions']['a']={'id':'a','parent':None}
        s['tasks']['t']={'id':'t','owner':'a','creator':'a','revision':1,'status':'open','correction':True,'dependencies':[]}
        req={'id':'close','actor':'a','op':'task.finish','data':{'task':'t','expected':1,'resolution':'corrected'}}
        with self.assertRaises(state.Rejected):state.apply(s,req,'2026-09-05T00:00:00+00:00')
        self.assertEqual(s['tasks']['t']['status'],'open')

if __name__ == '__main__':unittest.main()
