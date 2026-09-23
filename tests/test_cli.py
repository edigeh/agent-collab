"""End-to-end checks for the public command interface."""
from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class CliTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.base = Path(self.temp.name)
        self.home = self.base / "board"
        self.receipts = self.base / "receipts"

    def tearDown(self):
        self.temp.cleanup()

    def invoke(self, *args, session=None, home=None):
        command = [sys.executable, "-m", "collab_core", "--home", str(home or self.home),
                   "--receipts", str(self.receipts), "--json"]
        if session:
            command.extend(["--session", session])
        command.extend(args)
        result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, check=False)
        self.assertTrue(result.stdout, result.stderr)
        return result, json.loads(result.stdout)

    def wake(self, session, project=None):
        command = ["wake", "--harness", "human"]
        if project:
            command.extend(["--project", project])
        result, data = self.invoke(*command, session=session)
        self.assertEqual(result.returncode, 0, data)
        return data

    def test_two_sessions_exchange_and_ack_message(self):
        alpha = self.wake("alpha")
        self.assertTrue(any('ack ' + alpha['brief']['delivery'] in command for command in alpha['next']), alpha)
        self.assertFalse(any(command.endswith(' inbox') for command in alpha['next']), alpha)
        self.wake("beta", alpha["project"])

        posted, message = self.invoke("post", "hello beta", "--to", "beta", session="alpha")
        self.assertEqual(posted.returncode, 0, message)
        inbox, briefing = self.invoke("inbox", session="beta")
        self.assertEqual(inbox.returncode, 0, briefing)
        posts = [item for item in briefing["items"] if item["kind"] == "post"]
        self.assertTrue(posts, briefing)
        post = posts[0]
        self.assertEqual(post["text"], "hello beta")
        ack, outcome = self.invoke("ack", briefing["delivery"], session="beta")
        self.assertEqual(ack.returncode, 0, outcome)
        self.assertEqual(outcome["status"], "accepted")

    def test_omp_session_wakes_and_keeps_its_harness(self):
        first, data = self.invoke("wake", "--harness", "omp", session="omp-session")
        self.assertEqual(first.returncode, 0, data)
        self.assertEqual(data["session"], "omp-session")
        second, repeated = self.invoke("wake", "--harness", "omp", session="omp-session")
        self.assertEqual(second.returncode, 0, repeated)
        rejected, error = self.invoke("wake", "--harness", "pi", session="omp-session")
        self.assertEqual(rejected.returncode, 1, error)
        self.assertIn("already belongs to harness omp", error["error"])

    def test_compact_inbox_and_full_task_read_are_separate(self):
        self.wake('alpha')
        _, created=self.invoke('task','create','Inspect deployment',session='alpha')
        _, compact=self.invoke('inbox','--view','compact','--budget','1024',session='alpha')
        item=next(task for task in compact['items'] if task['id']==created['task'])
        self.assertEqual(item['kind'],'task');self.assertNotIn('context',item)
        _, full_page=self.invoke('inbox','--view','full',session='alpha')
        self.assertEqual(full_page['view'],'full')
        _, full=self.invoke('read',created['task'],'--kind','task',session='alpha')
        self.assertEqual(full['view'],'full');self.assertEqual(full['items'][0]['title'],'Inspect deployment')

    def test_direct_read_and_evidence_report(self):
        self.wake("alpha")
        _, post = self.invoke("post", "Claim", "--kind", "finding", session="alpha")
        process, read = self.invoke("read", post["post"], session="alpha")
        self.assertEqual(process.returncode, 0, read)
        self.assertEqual(read["items"][0]["text"], "Claim")
        source = self.base / "report.txt"; source.write_text("Counterevidence")
        process, result = self.invoke("report", post["post"], "--expected", "1", "--reason", "Contradiction", "--source", str(source), session="alpha")
        self.assertEqual(process.returncode, 0, result)

    def test_cleanup_requires_preview_and_removes_only_artifact(self):
        import hashlib
        self.wake("alpha")
        source=self.base/'cleanup-proof';source.write_bytes(b'Cleanup fixture')
        self.invoke('post','Evidence','--source',str(source),session='alpha')
        sha=hashlib.sha256(source.read_bytes()).hexdigest()
        run,preview=self.invoke('cleanup','preview',sha,session='alpha')
        self.assertEqual(run.returncode,0,preview);self.assertTrue(preview['references'])
        run,result=self.invoke('cleanup','remove',sha,'--confirm',preview['token'],session='alpha')
        self.assertEqual(run.returncode,0,result);self.assertEqual(result['status'],'accepted')
        self.assertFalse((self.home/'artifacts'/sha).exists());self.assertTrue(source.exists())

    def test_closed_request_is_not_counted_as_open(self):
        self.wake('alpha')
        _,request=self.invoke('post','Please check','--kind','request','--to','alpha',session='alpha')
        source=self.base/'request-proof';source.write_text('Checked')
        _,finished=self.invoke('task','finish',request['task'],'--expected','1','--source',str(source),session='alpha')
        self.assertEqual(finished['status'],'accepted')
        _,status=self.invoke('status',session='alpha');self.assertEqual(status['open_requests'],0)

    def test_task_list_defaults_to_current_project(self):
        self.wake('alpha')
        self.invoke('project','create','Other','other',session='alpha')
        _,task=self.invoke('task','create','Other project task','--project','other',session='alpha')
        _,local=self.invoke('task','list',session='alpha')
        self.assertNotIn(task['task'],[t['id'] for t in local['tasks']])
        _,other=self.invoke('task','list','--project','other',session='alpha')
        self.assertIn(task['task'],[t['id'] for t in other['tasks']])

    def test_conflicting_owned_scope_is_rejected(self):
        alpha = self.wake("alpha")
        self.wake("beta", alpha["project"])
        first, created = self.invoke("task", "create", "edit CLI", "--scope", "collab_core/cli.py", session="alpha")
        self.assertEqual(first.returncode, 0, created)
        second, conflict = self.invoke("task", "create", "also edit CLI", "--owner", "beta",
                                    "--project", alpha["project"], "--scope", "collab_core/cli.py", session="beta")
        self.assertEqual(second.returncode, 1)
        self.assertEqual(conflict["status"], "rejected")
        self.assertIn("scope conflict", conflict["error"])

    def test_finish_requires_and_preserves_evidence(self):
        self.wake("alpha")
        created_run, created = self.invoke("task", "create", "verify output", session="alpha")
        self.assertEqual(created_run.returncode, 0, created)
        evidence = self.base / "evidence.txt"
        evidence.write_text("checked\n")
        finished_run, finished = self.invoke("task", "finish", created["task"], "--expected", "1",
                                          "--source", str(evidence), session="alpha")
        self.assertEqual(finished_run.returncode, 0, finished)
        listing_run, listing = self.invoke("task", "list", session="alpha")
        self.assertEqual(listing_run.returncode, 0, listing)
        task = next(item for item in listing["tasks"] if item["id"] == created["task"])
        self.assertEqual(task["status"], "done")
        self.assertEqual(len(task["evidence"]), 1)

    def test_malformed_input_is_one_json_error_object(self):
        self.wake("alpha")
        result, data = self.invoke("post", "bad ref", "--ref", "not-a-reference", session="alpha")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(data["status"], "rejected")
        self.assertIn("POST_ID", data["error"])

    def test_pending_receipt_reconciles_after_board_returns(self):
        self.wake("alpha")
        unavailable = self.base / "not-a-directory"
        unavailable.write_text("blocked")
        pending_run, pending = self.invoke("post", "written while unavailable", session="alpha", home=unavailable)
        self.assertEqual(pending_run.returncode, 0, pending)
        self.assertEqual(pending["status"], "pending")
        sync_run, synced = self.invoke("sync", session="alpha")
        self.assertEqual(sync_run.returncode, 0, synced)
        self.assertEqual(len(synced["results"]), 1)
        self.assertEqual(synced["results"][0]["status"], "accepted")


if __name__ == "__main__":
    unittest.main()
