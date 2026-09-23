import json
import tempfile,unittest
from pathlib import Path
from collab_core.board import Board
from collab_core.state import Rejected
class Pagination(unittest.TestCase):
    def setUp(self):
        t=tempfile.TemporaryDirectory();self.addCleanup(t.cleanup)
        self.b=Board(Path(t.name)/'board');self.call('session.register',harness='human')
    def call(self,op,**data):
        r=self.b.submit(self.b.request('a',op,data));self.assertEqual(r['status'],'accepted',r);return r
    def test_ack_each_page_does_not_skip_posts_or_repeat_open_tasks(self):
        task=self.call('task.create',title='Still open')['task']
        posts=[self.call('post.create',text=f'Message {n}')['post'] for n in range(5)]
        seen=[];cursor=None
        for _ in range(10):
            page=self.b.brief('a',limit=2,cursor=cursor)
            seen.extend(x['id'] for x in page['items'])
            self.call('delivery.ack',delivery=page['delivery'])
            cursor=page['next_cursor']
            if cursor is None:break
        self.assertEqual(set(seen),{task,*posts});self.assertEqual(len(seen),6)
        self.assertIn(task,[x['id'] for x in self.b.brief('a')['items']])
    def test_limits_and_cursor_are_validated(self):
        for limit in (0,-1,51):
            with self.assertRaises(Rejected):self.b.brief('a',limit=limit)
        with self.assertRaises(Rejected):self.b.brief('a',offset=-1)
        with self.assertRaises(Rejected):self.b.brief('a',cursor='not-a-cursor')

    def test_brief_omits_bulk_evidence_but_read_retains_it(self):
        path=self.b.root.parent/'proof';path.write_text('Evidence')
        proof=self.b.capture(path)
        post=self.call('post.create',text='Claim',evidence=[proof])['post']
        brief=self.b.brief('a')['items'][0]
        self.assertEqual(brief['evidence_count'],1);self.assertNotIn('evidence',brief)
        full=self.b.brief('a',post_id=post)['items'][0]
        self.assertEqual(full['evidence'][0]['sha256'],proof['sha256'])

    def test_compact_tasks_omit_context_and_support_on_demand_read(self):
        task=self.call('task.create', title='Inspect the deployment', scope=['src/app.py'],
                       context={'large':'x' * 5000})['task']
        compact=next(item for item in self.b.brief('a')['items'] if item['id'] == task)
        self.assertEqual(compact['kind'],'task')
        self.assertNotIn('context',compact);self.assertNotIn('dependencies',compact)
        self.assertTrue(compact['detail_available'])
        full=self.b.brief('a', item_id=task, item_kind='task', view='full')['items'][0]
        self.assertEqual(full['context']['large'], 'x' * 5000)

    def test_budget_continuation_does_not_ack_hidden_items(self):
        posts=[self.call('post.create',text=f'Message {n}: ' + ('x' * 400))['post'] for n in range(5)]
        seen=[];cursor=None
        for _ in range(10):
            page=self.b.brief('a', limit=5, budget=1100, cursor=cursor)
            seen.extend(item['id'] for item in page['items'])
            self.assertLessEqual(len(json.dumps(page['items'], ensure_ascii=False).encode()),1100)
            self.call('delivery.ack',delivery=page['delivery'])
            cursor=page['next_cursor']
            if cursor is None: break
        self.assertEqual(set(seen),set(posts));self.assertEqual(len(seen),len(posts))

    def test_urgent_posts_are_before_normal_posts(self):
        normal=self.call('post.create',text='Normal')['post']
        urgent=self.call('post.create',text='Urgent')['post']
        self.call('post.report',post=urgent,expected=1,reason='Needs immediate review')
        page=self.b.brief('a')
        post_ids=[item['id'] for item in page['items'] if item['kind']=='post']
        self.assertEqual(post_ids[:2],[urgent,normal])
