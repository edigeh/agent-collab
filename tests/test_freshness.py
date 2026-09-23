import tempfile, unittest
from pathlib import Path
from collab_core.board import Board
class Freshness(unittest.TestCase):
    def setUp(self):
        t=tempfile.TemporaryDirectory();self.addCleanup(t.cleanup)
        self.root=Path(t.name).resolve();self.b=Board(self.root/'board')
        self.call('session.register',harness='codex')
        self.call('project.attach',project='global',root=str(self.root),expected=1)
    def call(self,op,**data):
        result=self.b.submit(self.b.request('a',op,data));self.assertEqual(result['status'],'accepted',result);return result
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
    def test_unchanged_source_stays_supported(self):
        _,post=self.post();self.b.check_sources('a','global')
        p=self.b.read_state()['posts'][post]
        self.assertFalse(p['due']);self.assertEqual(p['revision'],2)
        self.assertIn('source_checked_at',p)
    def test_missing_source_is_uncertain_and_history_preserved(self):
        path,post=self.post();path.unlink();self.b.check_sources('a','global')
        p=self.b.read_state()['posts'][post]
        self.assertEqual(p['status'],'disputed');self.assertEqual(p['text'],'Version one')
        self.assertEqual(self.b.artifact(p['evidence'][0]['sha256']),b'Version one')
    def test_bounded_checks_rotate_across_posts(self):
        posts=[self.post(n)[1] for n in range(4)]
        self.assertEqual(self.b.check_sources('a','global',limit=2)['checked'],2)
        self.assertEqual(self.b.check_sources('a','global',limit=2)['checked'],2)
        self.assertTrue(all('source_checked_at' in self.b.read_state()['posts'][p] for p in posts))
    def test_unattached_sources_are_not_read(self):
        t=tempfile.TemporaryDirectory();self.addCleanup(t.cleanup)
        path=Path(t.name)/'outside';path.write_text('Old')
        proof=self.b.capture(path)
        post=self.call('post.create',text='Old',evidence=[proof])['post'];path.write_text('New')
        self.assertEqual(self.b.check_sources('a','global')['checked'],0)
        self.assertTrue(self.b.read_state()['posts'][post]['due'])
        self.assertEqual(self.b.read_state()['posts'][post]['revision'],1)
