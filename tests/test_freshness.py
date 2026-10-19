import tempfile, unittest
from pathlib import Path
from collab_core.board import Board
from collab_core.journal import Journal
class Freshness(unittest.TestCase):
    def setUp(self):
        t=tempfile.TemporaryDirectory();self.addCleanup(t.cleanup)
        self.root=Path(t.name).resolve();self.b=Board(self.root/'board')
        self.call('session.register',harness='codex')
        self.call('project.attach',project='global',root=str(self.root),expected=1)
    def call(self,op,**data):
        result=self.b.submit(self.b.request('a',op,data));self.assertEqual(result['status'],'accepted',result);return result
    def observed(self):
        with Journal(self.root/'board'/'events.jsonl').locked() as log:
            return [r for r in log.records if r['payload']['request']['op']=='post.observe']
    def post(self,n=0):
        path=self.root/f'source{n}';path.write_text('Version one')
        proof=self.b.capture(path)
        post=self.call('post.create',text='Version one',kind='finding',evidence=[proof])['post']
        lease=self.call('moderation.claim',posts=[{'id':post,'revision':1}])['lease']
        self.call('moderation.apply',lease=lease,outcomes=[{'post':post,'status':'supported','reason':'Source matches','evidence':[proof]}])
        return path,post
    def test_changed_verified_source_becomes_due_without_model(self):
        path,post=self.post();self.b.brief('a')
        path.write_text('Version two')
        result=self.b.check_sources('a','global')
        p=self.b.read_state()['posts'][post]
        self.assertTrue(p['due']);self.assertTrue(p['urgent']);self.assertEqual(p['status'],'disputed')
        self.assertEqual(p['text'],'Version one')
        self.assertTrue(p['reports'][-1]['evidence'])
        self.assertEqual(result['changed'],1)
        self.assertEqual(len(self.observed()),1)
    def test_unchanged_source_stays_supported_and_is_not_journaled(self):
        _,post=self.post();result=self.b.check_sources('a','global')
        p=self.b.read_state()['posts'][post]
        self.assertFalse(p['due']);self.assertEqual(p['revision'],2)
        self.assertEqual((result['checked'],result['changed'],result['outcomes']),(1,0,[]))
        self.assertEqual(self.observed(),[])
        self.assertIn(post,self.b._source_checks())
    def test_unchanged_check_does_not_read_preserved_artifacts(self):
        from unittest.mock import patch
        _,post=self.post()
        with patch.object(self.b,'artifact',side_effect=AssertionError('artifact read')), \
             patch.object(self.b,'capture',side_effect=AssertionError('artifact captured')):
            self.assertEqual(self.b.check_sources('a','global')['checked'],1)
    def test_missing_source_is_uncertain_and_history_preserved(self):
        path,post=self.post();path.unlink();self.b.check_sources('a','global')
        p=self.b.read_state()['posts'][post]
        self.assertEqual(p['status'],'disputed');self.assertEqual(p['text'],'Version one')
        self.assertEqual(self.b.artifact(p['evidence'][0]['sha256']),b'Version one')
    def test_bounded_checks_rotate_across_posts(self):
        posts=[self.post(n)[1] for n in range(4)]
        self.assertEqual(self.b.check_sources('a','global',limit=2)['checked'],2)
        self.assertEqual(len(self.b._source_checks()),2)
        self.assertEqual(self.b.check_sources('a','global',limit=2)['checked'],2)
        self.assertEqual(set(self.b._source_checks()),set(posts))
        self.assertEqual(self.observed(),[])
    def test_pending_observation_keeps_its_place_in_the_rotation(self):
        from unittest.mock import patch
        from collab_core.journal import JournalBusy
        path,post=self.post();path.write_text('Version two')
        with patch.object(self.b,'_ingest',side_effect=JournalBusy('busy')):
            result=self.b.check_sources('a','global')
        self.assertEqual((result['checked'],result['outcomes'][0]['status']),(0,'pending'))
        self.assertNotIn(post,self.b._source_checks())
    def test_unattached_sources_are_not_read(self):
        t=tempfile.TemporaryDirectory();self.addCleanup(t.cleanup)
        path=Path(t.name)/'outside';path.write_text('Old')
        proof=self.b.capture(path)
        post=self.call('post.create',text='Old',evidence=[proof])['post'];path.write_text('New')
        self.assertEqual(self.b.check_sources('a','global')['checked'],0)
        self.assertTrue(self.b.read_state()['posts'][post]['due'])
        self.assertEqual(self.b.read_state()['posts'][post]['revision'],1)
