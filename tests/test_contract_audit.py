import json, subprocess, sys, tempfile, unittest
from pathlib import Path
from unittest.mock import patch
from collab_core.board import Board
class Contracts(unittest.TestCase):
    def setUp(self):
        t=tempfile.TemporaryDirectory();self.addCleanup(t.cleanup)
        self.root=Path(t.name);self.b=Board(self.root/'board')
        for actor in ('a','b'):self.call(actor,'session.register',harness='human')
        source=self.root/'proof';source.write_text('Inspected current work')
        self.proof=self.b.capture(source)
    def call(self,actor,op,**data):
        r=self.b.submit(self.b.request(actor,op,data));self.assertEqual(r['status'],'accepted',r);return r
    def test_takeover_requires_explicit_evidence_and_old_owner_reconciles(self):
        task=self.call('a','task.create',title='Owned',scope=['file'])['task']
        for data in ({'task':task,'expected':1},{'task':task,'expected':1,'takeover':True}):
            self.assertEqual(self.b.submit(self.b.request('b','task.claim',data))['status'],'rejected')
        self.call('b','task.claim',task=task,expected=1,takeover=True,evidence=[self.proof])
        res=self.b.submit(self.b.request('a','task.finish',{'task':task,'expected':2,'evidence':[self.proof]}))
        self.assertEqual(res['status'],'rejected')
        self.assertTrue(any(n['recipient']=='a' and n['kind']=='ownership' for n in self.b.read_state()['notices'].values()))
    def test_silence_does_not_free_ownership(self):
        with patch('collab_core.board.utcnow',return_value='2000-01-01T00:00:00+00:00'):
            self.call('a','session.touch')
        task=self.call('a','task.create',title='Still owned')['task']
        brief=self.b.brief('b')
        self.assertEqual(next(p for p in brief['peers'] if p['id']=='a')['presence'],'uncertain')
        self.assertEqual(self.b.read_state()['tasks'][task]['owner'],'a')
    def test_expired_moderator_cannot_apply(self):
        post=self.call('a','post.create',text='Claim',kind='finding')['post']
        with patch('collab_core.board.utcnow',return_value='2000-01-01T00:00:00+00:00'):
            lease=self.call('a','moderation.claim',posts=[{'id':post,'revision':1}])['lease']
        result=self.b.submit(self.b.request('a','moderation.apply',{'lease':lease,'outcomes':[{'post':post,'status':'unsupported','reason':'No evidence'}]}))
        self.assertEqual(result['status'],'rejected');self.assertEqual(self.b.read_state()['posts'][post]['revision'],1)
    def test_failure_after_delivery_record_still_notifies_possible_reader(self):
        post=self.call('a','post.create',text='Claim',kind='finding')['post']
        submit=self.b.submit
        def failed(req):
            result=submit(req)
            if req['op']=='delivery.prepare':raise OSError('Injected failure before rendering')
            return result
        with patch.object(self.b,'submit',failed):
            with self.assertRaises(OSError):self.b.brief('b')
        self.call('a','post.report',post=post,expected=1,reason='Contradiction')
        self.assertTrue(any(n['recipient']=='b' and n['kind']=='correction' for n in self.b.read_state()['notices'].values()))
    def test_receipt_failure_does_not_claim_or_commit_delivery(self):
        req=self.b.request('a','post.create',{'text':'Cannot preserve receipt'})
        with patch.object(self.b,'_receipt_journal',side_effect=PermissionError('No durable receipt')):
            with self.assertRaises(PermissionError):self.b.submit(req)
        self.assertNotIn(req['id'],self.b.read_state()['posts'])
    def test_two_processes_cannot_both_own_same_scope(self):
        code="from collab_core.board import Board; import sys,json; b=Board(sys.argv[1]); print(json.dumps(b.submit(b.request(sys.argv[2],'task.create',{'title':'Exclusive','scope':['same-file']}))))"
        children=[subprocess.Popen([sys.executable,'-c',code,str(self.b.root),actor],cwd=Path(__file__).resolve().parents[1],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True) for actor in ('a','b')]
        results=[]
        for process in children:
            out,err=process.communicate(timeout=20);self.assertEqual(process.returncode,0,err);results.append(json.loads(out))
        self.assertEqual(sorted(r['status'] for r in results),['accepted','rejected'])
        self.assertEqual(len(self.b.read_state()['tasks']),1)

    def test_ordinary_messages_and_work_reports_enter_moderation_batch(self):
        posts=[self.call('a','post.create',text='Claim '+kind,kind=kind)['post'] for kind in ('message','work','finding','request')]
        posts.append(self.call('a','post.create',text='Summary',kind='summary',refs=[{'id':posts[0],'revision':1}])['post'])
        state=self.b.read_state()
        self.assertTrue(all(state['posts'][p]['due'] for p in posts))
        self.assertEqual(len(self.b.due('global')),4)

    def test_summary_only_recipient_gets_source_correction(self):
        source=self.call('a','post.create',text='Private source claim',kind='finding',to='a')['post']
        summary=self.call('a','post.create',text='Derived summary',kind='summary',to='b',refs=[{'id':source,'revision':1}])['post']
        brief=self.b.brief('b');self.assertNotIn(source,[x['id'] for x in brief['items']])
        self.call('a','post.report',post=source,expected=1,reason='Incorrect claim')
        state=self.b.read_state();self.assertTrue(state['posts'][summary]['stale'])
        self.assertTrue(any(n['recipient']=='b' and n['target']==summary for n in state['notices'].values()))
    def test_moderation_can_restore_earlier_text_with_new_evidence(self):
        post=self.call('a','post.create',text='Original',kind='finding')['post']
        prefix=self.b.journal.path.read_bytes()
        lease=self.call('a','moderation.claim',posts=[{'id':post,'revision':1}])['lease']
        self.call('a','moderation.apply',lease=lease,outcomes=[{'post':post,'status':'corrected','text':'Replacement','reason':'First correction','evidence':[self.proof]}])
        self.call('a','post.report',post=post,expected=2,reason='Recheck earlier correction',evidence=[self.proof])
        lease=self.call('a','moderation.claim',posts=[{'id':post,'revision':3}])['lease']
        self.call('a','moderation.apply',lease=lease,outcomes=[{'post':post,'status':'corrected','text':'Original','reason':'Reverse earlier correction','evidence':[self.proof]}])
        self.assertEqual(self.b.read_state()['posts'][post]['text'],'Original')
        self.assertTrue(self.b.journal.path.read_bytes().startswith(prefix))

    def test_human_brief_preserves_uncertainty_and_due_work(self):
        from collab_core.cli import _human
        post=self.call('a','post.create',text='Questioned fact',evidence=[self.proof])['post']
        self.call('a','post.report',post=post,expected=1,reason='Needs verification')
        brief=self.b.brief('b');rendered=_human({'status':'accepted','brief':brief})
        self.assertIn('[disputed urgent]',rendered)
        self.assertIn('Questioned fact',rendered);self.assertIn('moderation due: 1',rendered)

        full=_human(self.b.brief('b',post_id=post))
        self.assertIn(self.proof['source'],full)
        self.assertIn('latest report: Needs verification',full)
