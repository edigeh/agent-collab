import tempfile,unittest
from pathlib import Path
from unittest.mock import patch
from collab_core.board import Board
from collab_core.state import Rejected
class Cleanup(unittest.TestCase):
    def setUp(self):
        t=tempfile.TemporaryDirectory();self.addCleanup(t.cleanup)
        self.b=Board(Path(t.name)/'board');self.b.submit(self.b.request('a','session.register',{'harness':'human'}))
        self.source=Path(t.name)/'source';self.source.write_text('Preserved proof')
        self.proof=self.b.capture(self.source);self.sha=self.proof['sha256']
        self.post=self.b.submit(self.b.request('a','post.create',{'text':'Claim','kind':'finding','evidence':[self.proof]}))['post']
    def test_preview_changes_nothing_and_lists_references(self):
        before=self.b.journal.path.read_bytes();preview=self.b.cleanup_preview(self.sha)
        self.assertEqual(before,self.b.journal.path.read_bytes())
        self.assertTrue(any(r['request_id']==self.post for r in preview['references']))
        self.assertEqual(preview['bytes'],len(b'Preserved proof'))
    def test_remove_preserves_history_and_records_loss(self):
        before=self.b.journal.path.read_bytes();p=self.b.cleanup_preview(self.sha)
        result=self.b.cleanup_remove('a',self.sha,p['token'])
        self.assertEqual(result['status'],'accepted',result)
        self.assertTrue(self.b.journal.path.read_bytes().startswith(before))
        self.assertFalse((self.b.root/'artifacts'/self.sha).exists())
        self.assertTrue(self.source.exists())
        s=self.b.read_state();self.assertEqual(s['posts'][self.post]['text'],'Claim')
        self.assertTrue(s['posts'][self.post]['urgent'])
        self.assertEqual(s['artifact_removals'][result['removal']]['status'],'removed')
    def test_new_reference_invalidates_preview(self):
        p=self.b.cleanup_preview(self.sha)
        self.b.submit(self.b.request('a','post.create',{'text':'New reference','evidence':[self.proof]}))
        result=self.b.cleanup_remove('a',self.sha,p['token'])
        self.assertEqual(result['status'],'rejected')
        self.assertTrue((self.b.root/'artifacts'/self.sha).exists())
    def test_failed_unlink_can_resume_without_rewriting_history(self):
        p=self.b.cleanup_preview(self.sha)
        original=Path.unlink
        def fail(path,*args,**kwargs):
            if path.name==self.sha:raise PermissionError('Injected unlink failure')
            return original(path,*args,**kwargs)
        with patch.object(Path,'unlink',fail):result=self.b.cleanup_remove('a',self.sha,p['token'])
        self.assertEqual(result['status'],'pending')
        resumed=self.b.cleanup_resume('a',result['removal'])
        self.assertEqual(resumed['status'],'accepted',resumed)
        self.assertEqual(self.b.cleanup_resume('a',result['removal'])['status'],'accepted')
    def test_pending_receipts_prevent_removal(self):
        from collab_core.journal import JournalBusy
        with patch.object(self.b,'_ingest',side_effect=JournalBusy('offline')):
            self.b.submit(self.b.request('a','post.create',{'text':'Pending proof','evidence':[self.proof]}))
        p=self.b.cleanup_preview(self.sha)
        self.assertTrue(p['pending_receipts'])
        with self.assertRaises(Rejected):self.b.cleanup_remove('a',self.sha,p['token'])

    def test_reference_checked_before_cleanup_cannot_commit_after_it(self):
        request=self.b.request('a','post.create',{'text':'Racing reference','evidence':[self.proof]})
        preview=self.b.cleanup_preview(self.sha)
        validate=self.b._validate_evidence
        def racing(req):
            validate(req)
            if req['id']==request['id']:
                with patch.object(self.b,'_validate_evidence',validate):
                    self.assertEqual(self.b.cleanup_remove('a',self.sha,preview['token'])['status'],'accepted')
        # Call ingestion directly to represent the already-validated request;
        # its receipt is not present during this deliberately controlled race.
        with patch.object(self.b,'_validate_evidence',racing):result=self.b._ingest(request)
        self.assertEqual(result['status'],'rejected')
        self.assertNotIn(request['id'],self.b.read_state()['posts'])
    def test_symlink_artifact_is_not_followed(self):
        from collab_core.journal import JournalCorrupt
        artifact=self.b.root/'artifacts'/self.sha;artifact.unlink();artifact.symlink_to(self.source)
        with self.assertRaises(JournalCorrupt):self.b.artifact(self.sha)
        with self.assertRaises(JournalCorrupt):self.b.capture(self.source)
