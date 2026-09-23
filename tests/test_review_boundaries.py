import tempfile
from pathlib import Path
import unittest
from collab_core.board import Board
from collab_core.moderator import build_bundle, run_moderation

class Boundaries(unittest.TestCase):
    def setUp(self):
        t=tempfile.TemporaryDirectory(); self.addCleanup(t.cleanup)
        self.root=Path(t.name); self.b=Board(self.root/'board')
        self.b.submit(self.b.request('a','session.register',{'harness':'codex'}))
        self.source=self.root/'source';self.source.write_text('Original evidence')
        self.proof=self.b.capture(self.source)
        self.post=self.b.submit(self.b.request('a','post.create',{'kind':'finding','text':'Claim','evidence':[self.proof]}))['post']
    def response(self):
        return {'outcomes':[{'post':self.post,'status':'corrected','reason':'Observed','text':'Fixed','evidence':['e0'],'source_fixes':[]}]}
    def test_malformed_outcome_receipt_does_not_poison_sync(self):
        r=self.b.submit(self.b.request('a','moderation.apply',{'lease':'x','outcomes':[{'post':[]}]}))
        self.assertEqual(r['status'],'rejected')
        self.assertEqual(self.b.submit(self.b.request('a','post.create',{'text':'Following'}))['status'],'accepted')
    def test_accepted_retry_survives_artifact_corruption(self):
        t=self.b.submit(self.b.request('a','task.create',{'title':'Check'}))['task']
        req=self.b.request('a','task.finish',{'task':t,'expected':1,'evidence':[self.proof]})
        result=self.b.submit(req);self.assertEqual(result['status'],'accepted')
        (self.b.root/'artifacts'/self.proof['sha256']).write_text('corrupt')
        self.assertEqual(self.b.submit(req),result)
        fresh=self.b.submit(self.b.request('a','post.create',{'text':'New','evidence':[self.proof]}))
        self.assertEqual(fresh['status'],'rejected')
    def test_malformed_model_releases_lease(self):
        for field in ('post','source','project'):
            raw=self.response()
            if field=='post':raw['outcomes'][0]['post']=[]
            else:raw['outcomes'][0]['source_fixes']=[{'source':str(self.source),'project':'global','reason':'why',field:[]}]
            result=run_moderation(self.b,'a',force=True,runner=lambda *args:raw)
            self.assertEqual(result['status'],'pending')
            self.assertTrue(self.b.due('global',force=True))
    def test_unattached_path_is_not_refreshed(self):
        self.source.write_text('Later private bytes')
        bundle,proof=build_bundle(self.b,[{'id':self.post,'revision':1}])
        self.assertEqual(bundle['sources'][0]['content'],'Original evidence')
        self.assertNotIn('Later private bytes',str(bundle))
        self.assertFalse(proof['e0']['current'])
    def test_attached_source_keeps_both_versions(self):
        self.b.submit(self.b.request('a','project.attach',{'project':'global','expected':1,'root':str(self.root)}))
        self.source.write_text('Current version')
        bundle,_=build_bundle(self.b,[{'id':self.post,'revision':1}])
        self.assertEqual([x['content'] for x in bundle['sources']],['Original evidence','Current version'])
    def test_source_fix_must_cite_its_source(self):
        other=self.root/'other';other.write_text('Other proof')
        proof=self.b.capture(other)
        self.b.submit(self.b.request('a','post.report',{'post':self.post,'expected':1,'reason':'check','evidence':[proof]}))
        raw=self.response();raw['outcomes'][0]['source_fixes']=[{'source':str(other),'project':'global','reason':'Change it'}]
        result=run_moderation(self.b,'a',force=True,runner=lambda *args:raw)
        self.assertEqual(result['status'],'pending')
        self.assertEqual(self.b.read_state()['tasks'],{})
