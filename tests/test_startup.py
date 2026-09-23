import json, subprocess, sys, tempfile, unittest
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
class Startup(unittest.TestCase):
    def setUp(self):
        t=tempfile.TemporaryDirectory();self.addCleanup(t.cleanup)
        self.root=Path(t.name);self.home=self.root/'board';self.receipts=self.root/'receipts'
    def cli(self,*args,cwd=None):
        r=subprocess.run([sys.executable,str(ROOT/'collab'),'--home',str(self.home),'--receipts',str(self.receipts),'--json',*args],cwd=cwd or self.root,capture_output=True,text=True)
        return r.returncode,json.loads(r.stdout)
    def test_first_wake_offline_can_backfill_work(self):
        self.home.write_text('unavailable')
        code,wake=self.cli('--session','new','wake','--harness','pi')
        self.assertEqual(code,0,wake);self.assertEqual(wake['status'],'pending')
        code,post=self.cli('--session','new','post','Completed while offline','--kind','work')
        self.assertEqual(post['status'],'pending')
        self.home.unlink()
        code,result=self.cli('--session','new','sync')
        self.assertEqual([x['status'] for x in result['results']],['accepted','accepted'])
        _,result=self.cli('--session','new','inbox')
        self.assertTrue(any(x.get('text')=='Completed while offline' for x in result['items']))
    def test_repository_worktree_uses_same_project_with_real_roots(self):
        repo=self.root/'repo';repo.mkdir()
        def git(*args):subprocess.run(['git','-C',str(repo),*args],check=True,capture_output=True)
        git('init');git('-c','user.name=Test','-c','user.email=test@example.invalid','commit','--allow-empty','-m','fixture')
        wt=self.root/'wt';git('worktree','add','--detach',str(wt))
        _,first=self.cli('--session','a','wake','--harness','codex',cwd=repo)
        _,second=self.cli('--session','b','wake','--harness','claude',cwd=wt)
        self.assertEqual(first['project'],second['project'])
        _,listing=self.cli('project','list')
        project=next(p for p in listing['projects'] if p['id']==first['project'])
        self.assertEqual(set(project['roots']),{str(repo.resolve()),str(wt.resolve())})

    def test_work_receipt_preserves_checkout_context_and_absolute_scope(self):
        checkout=self.root/'checkout';checkout.mkdir()
        self.cli('--session','a','wake','--harness','codex',cwd=checkout)
        _,post=self.cli('--session','a','post','Actual work','--kind','work','--scope','src/file.py',cwd=checkout)
        _,read=self.cli('--session','a','read',post['post'],cwd=checkout)
        item=read['items'][0]
        self.assertEqual(item['context']['worktree'],str(checkout.resolve()))
        self.assertEqual(item['scope'],[str((checkout/'src/file.py').resolve())])
        _,task=self.cli('--session','a','task','create','Shared resource','--scope','resource:release',cwd=checkout)
        _,listing=self.cli('--session','a','task','list',cwd=checkout)
        value=next(t for t in listing['tasks'] if t['id']==task['task'])
        self.assertEqual(value['scope'],['resource:release'])
        self.assertEqual(value['context']['cwd'],str(checkout.resolve()))
