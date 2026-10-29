"""Tests for the `collab` tool. Standard library only:

    python3 -m unittest test_collab -v

Every test runs against its own board in a temp dir (COLLAB_DIR); none
touches ~/.collab.
"""

import importlib.machinery
import importlib.util
import multiprocessing
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

TOOL = os.path.join(os.path.dirname(os.path.abspath(__file__)), "collab")


def load():
    loader = importlib.machinery.SourceFileLoader("collab_tool", TOOL)
    spec = importlib.util.spec_from_loader("collab_tool", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


C = load()


def writer(d, w, n):
    """One of the parallel writers: n appends, each naming its writer."""
    os.environ["COLLAB_DIR"] = d
    mod = load()
    for k in range(n):
        mod.append(d, "w%d" % w, "p", "say", "", "w%d-%d" % (w, k))


def spinner(d):
    os.environ["COLLAB_DIR"] = d
    mod = load()
    while True:
        mod.append(d, "spin", "p", "say", "", "x" * 200)


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.d = os.path.join(self.tmp, "board")
        self.work = os.path.join(self.tmp, "work")
        os.makedirs(self.work)
        self.run_cli("init")
        C.READ_BYTES = 0

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_cli(self, *args, agent="alice-0001", ok=True):
        env = dict(os.environ, COLLAB_DIR=self.d, COLLAB_AGENT=agent)
        p = subprocess.run([TOOL] + list(args), cwd=self.work, env=env,
                           capture_output=True, text=True, timeout=60)
        if ok:
            self.assertEqual(p.returncode, 0, p.stderr)
        return p

    def out(self, *args, **kw):
        return self.run_cli(*args, **kw).stdout

    def lines(self):
        with open(os.path.join(self.d, "LOG.txt"), "rb") as f:
            data = f.read()
        self.assertEqual(len(data) % C.REC, 0)
        return C.records(data, 0)

    def write_log(self, rows):
        """rows = [(when, agent, project, kind, target, text)], as raw bytes."""
        with open(os.path.join(self.d, "LOG.txt"), "ab") as f:
            base = os.path.getsize(f.name) // C.REC
            for k, r in enumerate(rows):
                f.write(C.encode(base + k, *r))


class Store(Base):
    def test_round_trip_multibyte_at_limit(self):
        text = "é" * 119 + "ab"  # 240 bytes, 121 characters
        self.assertEqual(len(text.encode()), 240)
        i = C.append(self.d, "bob-1", "proj", "say", "", text)
        r = C.log_get(self.d, i)
        self.assertEqual((r.id, r.agent, r.project, r.kind, r.target, r.text),
                         (i, "bob-1", "proj", "say", "", text))
        with open(os.path.join(self.d, "LOG.txt"), "rb") as f:
            raw = f.read()
        self.assertEqual(len(raw), C.REC)
        self.assertTrue(raw.endswith(b"\n"))
        # a path target with spaces survives the field encoding
        j = C.append(self.d, "bob-1", "proj", "claim", "/a b/100%", "why not")
        r = C.log_get(self.d, j)
        self.assertEqual((r.target, r.text), ("/a b/100%", "why not"))

    def test_refusals_say_how_to_fix(self):
        p = self.run_cli("say", "é" * 121, ok=False)
        self.assertEqual(p.returncode, 1)
        self.assertIn("Too long: 242 bytes, limit 240", p.stderr)
        self.assertIn("file", p.stderr)
        p = self.run_cli("say", "one\ntwo", ok=False)
        self.assertIn("2 lines", p.stderr)
        p = self.run_cli("say", "x", agent="bad name", ok=False)
        self.assertIn("is not a name", p.stderr)
        p = self.run_cli("reply", "#99", "x", ok=False)
        self.assertIn("beyond the board", p.stderr)
        self.assertFalse(os.path.getsize(os.path.join(self.d, "LOG.txt")))

    def test_partial_trailing_record_is_repaired(self):
        C.append(self.d, "a-1", "p", "say", "", "one")
        with open(os.path.join(self.d, "LOG.txt"), "ab") as f:
            f.write(b"#1 2026-10-28T00:00Z half a reco")  # a writer died here
        self.out("say", "two", "--project", "p")
        got = self.lines()
        self.assertEqual([(r.id, r.text) for r in got], [(0, "one"), (1, "two")])

    def test_parallel_writers_lose_nothing(self):
        ctx = multiprocessing.get_context("spawn")
        procs = [ctx.Process(target=writer, args=(self.d, w, 1000))
                 for w in range(8)]
        for p in procs:
            p.start()
        for p in procs:
            p.join(120)
            self.assertEqual(p.exitcode, 0)
        got = self.lines()
        self.assertEqual(len(got), 8000)
        self.assertEqual([r.id for r in got], list(range(8000)))
        self.assertEqual(len({r.text for r in got}), 8000)
        for w in range(8):  # each writer's lines landed in its own order
            mine = [int(r.text.split("-")[1]) for r in got if r.agent == "w%d" % w]
            self.assertEqual(mine, list(range(1000)))

    def test_killed_writer_leaves_a_repairable_log(self):
        ctx = multiprocessing.get_context("spawn")
        p = ctx.Process(target=spinner, args=(self.d,))
        p.start()
        time.sleep(1.5)
        os.kill(p.pid, signal.SIGKILL)
        p.join()
        with open(os.path.join(self.d, "LOG.txt"), "ab") as f:
            f.write(b"#9999 torn")  # and the torn record a kill can leave
        self.out("say", "after the crash", "--project", "p")
        got = self.lines()
        self.assertGreater(len(got), 1)
        self.assertEqual([r.id for r in got], list(range(len(got))))
        self.assertEqual(got[-1].text, "after the crash")

    def test_tail_reads_only_the_window(self):
        old = "2026-01-01T00:00Z"
        with open(os.path.join(self.d, "LOG.txt"), "ab") as f:
            blob = b"".join(C.encode(i, old, "a-1", "p", "say", "", "old")
                            for i in range(1000))
            for k in range(60):  # 60,000 old lines, 19 MB
                f.write(blob)
        n = C.log_len(self.d)
        now = C.stamp()
        self.write_log([(now, "a-1", "p", "say", "", "new %d" % k)
                        for k in range(10)])
        C.READ_BYTES = 0
        got = C.tail(self.d, 7)
        self.assertEqual([r.text for r in got], ["new %d" % k for k in range(10)])
        self.assertEqual(got[0].id, n)
        self.assertLessEqual(C.READ_BYTES, 2 * C.CHUNK * C.REC)
        self.assertLess(C.READ_BYTES, n * C.REC / 50)

    def test_lock_timeout_fails_cleanly(self):
        import fcntl
        lock = open(os.path.join(self.d, ".lock"), "a")
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            t = time.time()
            p = self.run_cli("say", "x", ok=False)
            self.assertLess(time.time() - t, 15)
            self.assertIn("busy", p.stderr)
            self.assertIn("Carry on", p.stderr)
        finally:
            lock.close()


class Brief(Base):
    def test_empty_board_prints_two_lines(self):
        out = self.out("wake", "--project", "p").splitlines()
        self.assertEqual(len(out), 2)
        self.assertTrue(out[0].startswith("You are alice-0001 on p ("))
        self.assertIn("doing", out[1])

    def test_wake_twice_same_output_one_line(self):
        a = self.out("wake", "--project", "p")
        n = len(self.lines())
        b = self.out("wake", "--project", "p")
        self.assertEqual(a, b)
        self.assertEqual(len(self.lines()), n)
        self.assertEqual(n, 1)

    def synthetic(self):
        """Two months, ~7000 lines, 10 projects, many open asks, long text."""
        now, rows = time.time(), []
        long = "x" * 200 + " é"
        for k in range(7000):
            t = now - (60 - k * 60 / 7000.0) * 86400
            p, a = "p%d" % (k % 10), "agent-%04x" % (k % 300)
            kind = ("say", "ask", "doing", "wake", "reply", "fix")[k % 6]
            target = {"ask": "p0" if k % 4 else "@alice-0001",
                      "reply": "#%d" % max(0, k - 7), "fix": "#%d" % max(0, k - 2),
                      "wake": "1:1"}.get(kind, "")
            rows.append((C.stamp(t), a, p, kind, target, long))
        self.write_log(rows)

    def test_budget_with_two_months_of_history(self):
        self.synthetic()
        out = self.out("wake", "--project", "p0", agent="newbie-0001")
        lines = out.splitlines()
        self.assertLessEqual(len(lines), 20, out)
        self.assertLess(len(out.encode()), 2500, out)
        self.assertIn("For you:", out)
        self.assertIn("Recent here (48h):", out)
        self.assertRegex(out, r"\d+ more open asks for you or this project\. "
                              r"Run: .*collab log --open")

    def test_fresh_session_sees_what_an_old_one_sees(self):
        self.synthetic()
        old = self.out("wake", "--project", "p0", agent="old-0001")
        new = self.out("wake", "--project", "p0", agent="new-0002")
        old = self.out("wake", "--project", "p0", agent="old-0001")
        strip = lambda s: [l for l in s.splitlines()[1:]
                           if "new-0002" not in l and "old-0001" not in l
                           and not l.startswith(("Here:", "     "))]
        self.assertEqual(strip(old), strip(new))

    def test_reply_closes_ask(self):
        self.out("ask", "p", "Is the table final?", "--project", "q", agent="bob-0002")
        out = self.out("wake", "--project", "p")
        self.assertIn("For you:   #0 bob-0002 ask p: Is the table final?", out)
        self.assertIn("ask #0 is closed", self.out("reply", "#0", "yes", "--project", "p"))
        self.assertNotIn("For you", self.out("wake", "--project", "p"))
        self.assertIn("No match", self.out("log", "--open", "--project", "p"))
        thread = self.out("log", "#0")
        self.assertIn("reply #0  yes", thread)

    def test_old_asks_collapse_into_a_count(self):
        old = C.stamp(time.time() - 10 * 86400)
        self.write_log([(old, "bob-0002", "p", "ask", "p", "ancient")])
        for k in range(7):
            self.out("ask", "@alice-0001", "q%d" % k, "--project", "p", agent="bob-0002")
        out = self.out("wake", "--project", "p")
        self.assertNotIn("ancient", out)
        self.assertEqual(out.count(" bob-0002 ask"), 5 + 2)  # 5 For you, 2 recent
        self.assertIn("3 more open asks for you or this project.", out)
        self.assertIn("log --open", out)
        listed = self.out("log", "--open", "--project", "p")
        self.assertIn("ancient", listed)
        self.assertIn("8 matches.", listed)

    def test_fix_is_shown_with_its_target(self):
        self.out("say", "install is at cc29feb6", "--project", "p", agent="bob-0002")
        self.out("fix", "#0", "it is eb363ce0", "--project", "p", agent="carl-0003")
        out = self.out("wake", "--project", "p")
        self.assertIn("  #0 bob-0002 say: install is at cc29feb6\n"
                      "    fix #1 carl-0003: it is eb363ce0", out)
        self.assertIn("└ #1", self.out("board", "--plain", "--project", "p"))

    def test_fix_of_a_shown_ask_is_not_repeated(self):
        self.out("ask", "p", "who owns cookies?", "--project", "p", agent="bob-0002")
        self.out("fix", "#0", "session cookies only", "--project", "p", agent="bob-0002")
        out = self.out("wake", "--project", "p")
        self.assertEqual(out.count("session cookies only"), 1, out)

    def test_claim_overlap_and_bye_release(self):
        self.out("doing", "porting the parser", "--project", "p", agent="bob-0002")
        target = os.path.join(self.work, "rust")
        os.makedirs(os.path.join(target, "src"))
        self.out("claim", target, "--why", "parser port", "--project", "p",
                 agent="bob-0002")
        out = self.out("claim", os.path.join(target, "src"), "--project", "p")
        self.assertIn("bob-0002 also holds", out)
        self.assertIn('doing "porting the parser"', out)
        self.assertIn("ask @bob-0002", out)
        self.assertEqual(self.lines()[-1].kind, "claim")  # recorded anyway
        self.assertIn("claims rust/", self.out("wake", "--project", "p"))
        self.assertIn("Released 1 claim", self.out("bye", "done", "--project", "p",
                                                    agent="bob-0002"))
        plain = self.out("board", "--plain", "--project", "p")
        claims = plain.split("CLAIMS")[1].split("STREAM")[0]
        self.assertNotIn("bob-0002", claims)
        self.assertIn("alice-0001", claims)
        self.assertNotIn("Released", self.out("release", "--project", "p",
                                              agent="carl-0003"))
        self.assertIn("Released", self.out("release", "--project", "p"))

    def test_control_characters_never_reach_the_terminal(self):
        self.out("say", "red \x1b[31mtext\x1b]0;title\x07 \u202eevil", "--project", "p")
        self.write_log([(C.stamp(), "x\x1b[2J", "p", "say", "", "raw \x1b[H")])
        for args in (("wake", "--project", "p"), ("log", "--project", "p"),
                     ("board", "--plain", "--project", "p")):
            out = self.out(*args)
            self.assertNotIn("\x1b", out, args)
            self.assertNotIn("\x07", out, args)
            self.assertNotIn("\u202e", out, args)
        self.assertIn("red [31mtext", self.out("log", "--project", "p"))

    def test_board_plain_panes(self):
        self.out("wake", "--project", "p", agent="bob-0002")
        self.out("doing", "rust parity", "--project", "p", agent="bob-0002")
        self.out("ask", "@alice-0001", "ready?", "--project", "p", agent="bob-0002")
        self.out("reply", "#2", "draft in REDESIGN.md", "--project", "p")
        self.out("ask", "p", "still open?", "--project", "p", agent="bob-0002")
        out = self.out("board", "--plain", "--project", "p").splitlines()
        self.assertEqual(out[0], " p ─ 2 live ─ 1 open ask ─ 0 claims")
        self.assertEqual(out[1:4], ["AGENTS", " ● alice-0001   (active)",
                                      " ● bob-0002     rust parity"])
        self.assertIn(" ? #4 → p 0m  still open?", out)
        i = out.index("STREAM (newest first)")
        self.assertRegex(out[i + 1], r"^ #4 \d\d:\d\d bob-0002 ask p  still open\?")
        self.assertTrue(any(l.startswith("   └ #3 ") for l in out))


class Identity(Base):
    def test_override(self):
        out = self.out("wake", "--project", "p", agent="claude-7a3f/review")
        self.assertTrue(out.startswith("You are claude-7a3f/review on p"))
        self.assertEqual(self.lines()[0].agent, "claude-7a3f/review")

    def test_walk(self):
        table = {10: (9, 100, "python3"), 9: (8, 90, "zsh"),
                 8: (7, 80, "claude"), 7: (1, 70, "login")}
        self.assertEqual(C.walk(table, 9), ("claude", 8, 80))
        table[8] = (7, 80, "zsh")
        self.assertEqual(C.walk(table, 9), ("user", 7, 70))
        self.assertEqual(C.script("node /opt/lib/codex/bin/codex.js --x"), "codex")
        self.assertEqual(C.script("-/bin/zsh"), "zsh")
        self.assertEqual(C.harness_of("claude.exe"), "claude")
        self.assertEqual(C.harness_of("codex-aarch64-apple-darwin"), "codex")
        self.assertIsNone(C.harness_of("pip"))

    def test_same_session_same_name(self):
        env = dict(os.environ, COLLAB_DIR=self.d)
        env.pop("COLLAB_AGENT", None)
        names = [subprocess.run([TOOL, "wake", "--project", "p"], env=env,
                                capture_output=True, text=True).stdout.split()[2]
                 for _ in range(2)]
        self.assertEqual(names[0], names[1])
        self.assertRegex(names[0], r"^[a-z]+-[0-9a-f]{8}$")

    def test_pid_reuse_and_dead_pids_are_gone(self):
        C._PS = None
        me = C.ps()[os.getpid()]
        dead = subprocess.Popen(["true"])
        dead.wait()
        now = C.stamp()
        recs = [C.R(0, now, "live", "p", "wake", "%d:%d" % (os.getpid(), me[1]), ""),
                C.R(1, now, "reused", "p", "wake", "%d:%d" % (os.getpid(), me[1] + 5), ""),
                C.R(2, now, "dead", "p", "wake", "%d:%d" % (dead.pid, me[1]), ""),
                C.R(3, now, "unknown", "p", "say", "", "no wake in the window"),
                C.R(4, now, "left", "p", "wake", "%d:%d" % (os.getpid(), me[1]), ""),
                C.R(5, now, "left", "p", "bye", "", "done")]
        s = C.state(recs)
        status = {n: a["status"] for n, a in s["agents"].items()}
        self.assertEqual(status, {"live": "active", "reused": "gone",
                                  "dead": "gone", "unknown": "active",
                                  "left": "gone"})

    def test_project_is_the_main_repository_in_a_worktree(self):
        repo = os.path.join(self.tmp, "myrepo")
        git = lambda *a, cwd=repo: subprocess.run(
            ["git"] + list(a), cwd=cwd, capture_output=True, check=True)
        os.makedirs(repo)
        git("init", "-q")
        git("-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q",
            "--allow-empty", "-m", "x")
        git("worktree", "add", "-q", os.path.join(self.tmp, "wt-other"))
        for cwd in (repo, os.path.join(self.tmp, "wt-other")):
            p = subprocess.run([TOOL, "say", "hi"], cwd=cwd, capture_output=True,
                               text=True, env=dict(os.environ, COLLAB_DIR=self.d,
                                                   COLLAB_AGENT="a-1"))
            self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual([r.project for r in self.lines()], ["myrepo", "myrepo"])


class Init(Base):
    def test_init_block_and_config(self):
        out = self.out("init")
        self.assertIn("## Agent Collab", out)
        self.assertIn("%s wake" % C.pretty(TOOL), out)
        self.out("config", "WAKE_LINES=12")
        self.assertIn("WAKE_LINES   12", self.out("config"))
        self.assertIn("at most 240",
                      self.run_cli("config", "TEXT_BYTES=300", ok=False).stderr)

    def test_no_board_without_init(self):
        env = dict(os.environ, COLLAB_DIR=os.path.join(self.tmp, "typo"))
        p = subprocess.run([TOOL, "wake"], env=env, capture_output=True, text=True)
        self.assertEqual(p.returncode, 1)
        self.assertIn("init", p.stderr)

SESSION_VARS = ("COLLAB_AGENT", "CODEX_THREAD_ID", "PI_SESSION_ID",
                "CLAUDE_CODE_SESSION_ID", "CLAUDE_PID")


def bare_env(**kw):
    """This process's environment without any harness's session variables:
    the tests run inside one."""
    env = {k: v for k, v in os.environ.items() if k not in SESSION_VARS}
    env.update(kw)
    return env


class Sessions(Base):
    """Identity in-process, with a faked process table and environment."""

    def setUp(self):
        super().setUp()
        self.saved = (dict(os.environ), C._PS, C.args_of)
        for k in SESSION_VARS:
            os.environ.pop(k, None)

    def tearDown(self):
        os.environ.clear()
        os.environ.update(self.saved[0])
        C._PS, C.args_of = self.saved[1], self.saved[2]
        super().tearDown()

    def test_env_names_sessions_without_ps(self):
        C._PS = False  # ps is not allowed, as in the Codex sandbox
        os.environ["CODEX_THREAD_ID"] = "thread-a"
        a1, proc, _ = C.whoami(5)
        a2 = C.whoami(5)[0]
        os.environ["CODEX_THREAD_ID"] = "thread-b"
        b = C.whoami(5)[0]
        self.assertEqual(a1, a2)
        self.assertNotEqual(a1, b)
        self.assertRegex(a1, r"^codex-[0-9a-f]{8}$")
        self.assertEqual(proc, "")  # liveness by time alone

    def test_exported_pid_is_kept_for_liveness(self):
        C._PS = False
        os.environ.update(CLAUDE_CODE_SESSION_ID="s1", CLAUDE_PID="4242")
        name, proc, _ = C.whoami(5)
        self.assertRegex(name, r"^claude-[0-9a-f]{8}$")
        self.assertEqual(proc, "4242:0")  # start unknown: pid alone decides

    def test_no_ps_and_no_session_refuses(self):
        C._PS = False
        name, proc, note = C.whoami(5)
        self.assertEqual(name, "")
        self.assertIn("COLLAB_AGENT=", note)

    def test_walk_picks_the_nearest_harness_over_inherited_env(self):
        # omp started from a Claude Code shell inherits Claude's variables.
        C._PS = {10: (9, 100, "zsh"), 9: (8, 90, "bun"), 8: (7, 80, "zsh"),
                 7: (1, 70, "claude.exe")}
        C.args_of = lambda pid: {9: "bun /opt/bun/bin/omp -p hi"}[pid]
        os.environ["CLAUDE_CODE_SESSION_ID"] = "outer"
        name, proc, _ = C.whoami(10)
        self.assertTrue(name.startswith("omp-"), name)
        self.assertEqual(proc, "9:90")
        name, proc, _ = C.whoami(8)  # the Claude shell itself
        self.assertEqual(name, "claude-" + C.hashlib.sha1(
            b"claude:outer").hexdigest()[:8])
        self.assertEqual(proc, "7:70")

    def test_pi_under_node_uses_its_session_id(self):
        C._PS = {10: (9, 100, "bash"), 9: (1, 90, "node")}
        C.args_of = lambda pid: "node /usr/lib/node_modules/x/bin/pi -c"
        os.environ.update(PI_SESSION_ID="p1", CLAUDE_CODE_SESSION_ID="outer")
        a = C.whoami(10)[0]
        os.environ["PI_SESSION_ID"] = "p2"
        self.assertNotEqual(a, C.whoami(10)[0])
        self.assertTrue(a.startswith("pi-"))

    def test_new_session_under_an_old_name_drops_its_claims(self):
        now = C.stamp()
        recs = [C.R(0, now, "x-1", "p", "wake", "1:1", ""),
                C.R(1, now, "x-1", "p", "claim", "/a", ""),
                C.R(2, now, "bob", "p", "ask", "@x-1", "for the old one"),
                C.R(3, now, "x-1", "p", "wake", "2:2", ""),
                C.R(4, now, "bob", "p", "ask", "@x-1", "for the new one")]
        C._PS = False
        s = C.state(recs)
        self.assertEqual(s["claims"], [])
        mine = [r.text for r in s["asks"] if C.for_me(r, s, "x-1", "q")]
        self.assertEqual(mine, ["for the new one"])  # the old one's ask stays
        here = [r.text for r in s["asks"] if C.for_me(r, s, "y-1", "p")]
        self.assertEqual(here, ["for the old one"])  # ...on its project


class SandboxCLI(Base):
    """The whole CLI where ps is refused, as in Codex's seatbelt sandbox."""

    def setUp(self):
        super().setUp()
        fake = os.path.join(self.tmp, "bin")
        os.makedirs(fake)
        with open(os.path.join(fake, "ps"), "w") as f:
            f.write("#!/bin/sh\necho 'ps: Operation not permitted' >&2\nexit 1\n")
        os.chmod(os.path.join(fake, "ps"), 0o755)
        self.path = fake + os.pathsep + os.environ["PATH"]

    def wake(self, **kw):
        return subprocess.run([TOOL, "wake", "--project", "p"], cwd=self.work,
                              env=bare_env(COLLAB_DIR=self.d, PATH=self.path, **kw),
                              capture_output=True, text=True)

    def test_thread_id_names_the_session(self):
        a, b = self.wake(CODEX_THREAD_ID="t1"), self.wake(CODEX_THREAD_ID="t1")
        c = self.wake(CODEX_THREAD_ID="t2")
        names = [p.stdout.split()[2] for p in (a, b, c)]
        self.assertEqual(names[0], names[1])
        self.assertNotEqual(names[0], names[2])
        self.assertEqual(len(self.lines()), 2)  # one wake per session
        self.assertIn(names[0] + " (active)", c.stdout)

    def test_no_identity_is_refused_not_invented(self):
        p = self.wake()
        self.assertEqual(p.returncode, 1)
        self.assertIn("COLLAB_AGENT=", p.stderr)
        self.assertEqual(self.lines(), [])
        # Reading works without a name.
        p = subprocess.run([TOOL, "board", "--plain", "--project", "p"],
                           cwd=self.work, capture_output=True, text=True,
                           env=bare_env(COLLAB_DIR=self.d, PATH=self.path))
        self.assertEqual(p.returncode, 0, p.stderr)


class Limits(Base):
    def test_overflowing_asks_keep_the_line_budget(self):
        rows = []
        for k in range(5):
            rows.append((C.stamp(), "bob-0002", "p", "ask", "@alice-0001",
                         "question %d" % k))
            rows += [(C.stamp(), "carl-0003", "p", "fix", "#%d" % (6 * k),
                      "fix %d.%d" % (k, j)) for j in range(5)]
        rows += [(C.stamp(), "bob-0002", "p", "say", "", "said %d" % k)
                 for k in range(6)]
        self.write_log(rows)
        out = self.out("wake", "--project", "p")
        self.assertLessEqual(len(out.splitlines()), 20, out)
        self.assertIn("    fix #5 carl-0003: fix 0.4 (+4 fixes)", out)
        self.write_log([(C.stamp(), "bob-0002", "p", "ask", "p", "more %d" % k)
                        for k in range(4)])
        for lines in (20, 8, 4):
            self.out("config", "WAKE_LINES=%d" % lines)
            out = self.out("wake", "--project", "p")
            self.assertLessEqual(len(out.splitlines()), lines, out)
            if lines > 4:
                self.assertRegex(out, r"\d+ more open asks? for you or this "
                                      r"project\. Run: .* log --open")

    def test_long_names_and_paths_stay_under_the_byte_cap(self):
        deep = os.path.join(self.work, *(["d" * 40] * 10))
        os.makedirs(deep)
        me = "a" * 32 + "/" + "b" * 15
        long = "y" * 150
        for k in range(30):
            agent = "%s%02d/%s" % ("n" * 30, k, "c" * 15)
            proj = ("p" * 31 + "%d" % (k % 3)) if k % 2 else "P" * 32
            self.write_log([(C.stamp(), agent, proj, "doing", "", long),
                            (C.stamp(), agent, "P" * 32, "ask", "@" + me, long),
                            (C.stamp(), agent, "P" * 32, "say", "", long)])
        p = subprocess.run([TOOL, "wake", "--project", "P" * 32], cwd=deep,
                           env=dict(os.environ, COLLAB_DIR=self.d,
                                    COLLAB_AGENT=me),
                           capture_output=True, text=True)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertGreater(len(deep), 400)
        self.assertLessEqual(len(p.stdout.splitlines()), 20, p.stdout)
        self.assertLess(len(p.stdout.encode()), 2500, p.stdout)
        self.assertRegex(p.stdout.splitlines()[0], r" \(…d+\)\.$")

    def test_clock_stepping_back_keeps_times_in_order(self):
        future = C.stamp(time.time() + 86400)
        self.write_log([(future, "a-1", "p", "say", "", "from the future")])
        i = C.append(self.d, "a-1", "p", "say", "", "now")
        self.assertEqual(C.log_get(self.d, i).time, future)
        self.assertEqual([r.text for r in C.tail(self.d, 1)],
                         ["from the future", "now"])

    def test_non_utf8_argument_is_a_plain_error(self):
        env = dict(os.environ, COLLAB_DIR=self.d, COLLAB_AGENT="a-1")
        p = subprocess.run([TOOL.encode(), b"say", b"caf\xe9"], cwd=self.work,
                           env=env, capture_output=True)
        self.assertEqual(p.returncode, 1)
        self.assertIn(b"not valid UTF-8", p.stderr)
        self.assertNotIn(b"Traceback", p.stderr)

    def test_invisible_characters_are_dropped(self):
        sneaky = "a\U000E0041b​c﻿d e fg‮h"
        self.out("say", sneaky, "--project", "p")
        with open(os.path.join(self.d, "LOG.txt"), encoding="utf-8") as f:
            raw = f.read()
        self.assertIn(" abcdefgh", raw)
        self.write_log([(C.stamp(), "x-1", "p", "say", "", sneaky)])
        for args in (("log", "--project", "p"), ("wake", "--project", "p")):
            out = self.out(*args)
            self.assertIn("abcdefgh", out)
            for ch in sneaky[1::2]:
                self.assertNotIn(ch, out)

    def test_claim_path_with_control_characters_is_refused(self):
        p = self.run_cli("claim", os.path.join(self.work, "a\x1b[2Jb"), ok=False)
        self.assertEqual(p.returncode, 1)
        self.assertIn("control or invisible", p.stderr)
        self.assertEqual(self.lines(), [])

    def test_names_start_with_a_letter_or_digit(self):
        self.assertIn("neither", self.run_cli("ask", "-x", "q", ok=False).stderr)
        self.assertIn("start with a letter",
                      self.run_cli("say", "x", "--project", ".x", ok=False).stderr)
        p = self.run_cli("ask", "nosuchproject", "anyone?", "--project", "p")
        self.assertIn("no line from project nosuchproject", p.stderr)
        self.assertEqual(self.lines()[-1].target, "nosuchproject")

    def test_double_dash_ends_options(self):
        self.out("say", "--project", "p", "--", "--all is a flag of log")
        self.assertEqual(self.lines()[-1].text, "--all is a flag of log")
        self.assertIn("put -- before it",
                      self.run_cli("say", "--allx", ok=False).stderr)


class Log(Base):
    def test_thread_output_is_paged(self):
        self.out("say", "root", "--project", "p")
        self.write_log([(C.stamp(), "b-1", "p", "reply", "#0",
                         "%03d " % k + "z" * 200) for k in range(200)])
        out = self.out("log", "#0")
        self.assertLess(len(out), C.PART_CHARS + 1000)
        self.assertIn("reply #0  199 ", out)
        m = re.search(r"Newest (\d+) of 200\. .* --before '(#\d+)'", out)
        self.assertTrue(m, out[-300:])
        older = self.out("log", "#0", "--before", m.group(2))
        self.assertIn("reply #0  038 ", older)
        self.assertNotIn("reply #0  199 ", older)

    def test_since_and_before(self):
        old = C.stamp(time.time() - 40 * 86400)
        self.write_log([(old, "a-1", "p", "say", "", "old news"),
                        (C.stamp(), "a-1", "p", "say", "", "fresh news")])
        self.assertNotIn("old news", self.out("log", "news", "--since", "30d",
                                              "--project", "p"))
        self.assertIn("old news", self.out("log", "news", "--project", "p"))
        out = self.out("log", "news", "--before", "#1", "--project", "p")
        self.assertIn("old news", out)
        self.assertNotIn("fresh news", out)
        self.assertIn("12h", self.run_cli("log", "--since", "3w", ok=False).stderr)

    def test_open_before_counts_later_replies(self):
        self.out("ask", "p", "first?", "--project", "p", agent="b-1")
        self.out("ask", "p", "second?", "--project", "p", agent="b-1")
        self.out("reply", "#0", "answered", "--project", "p")
        out = self.out("log", "--open", "--before", "#2", "--project", "p")
        self.assertNotIn("first?", out)
        self.assertIn("second?", out)

    def test_board_plain_caps_each_section(self):
        for k in range(15):
            os.makedirs(os.path.join(self.work, "d%02d" % k))
            self.out("claim", os.path.join(self.work, "d%02d" % k),
                     "--project", "p")
        out = self.out("board", "--plain", "--project", "p")
        claims = out.split("CLAIMS")[1].split("STREAM")[0].strip().splitlines()
        self.assertEqual(len(claims), 11)
        self.assertEqual(claims[-1].strip(), "5 more")

    def test_release_by_path_keeps_the_others(self):
        work = os.path.realpath(self.work)
        a, b = os.path.join(work, "a"), os.path.join(work, "b")
        self.out("claim", a, b, "--project", "p")
        self.assertIn("Released %s." % C.pretty(a),
                      self.out("release", a, "--project", "p"))
        plain = self.out("board", "--plain", "--project", "p")
        claims = plain.split("CLAIMS")[1].split("STREAM")[0]
        self.assertIn(C.pretty(b), claims)
        self.assertNotIn(C.pretty(a) + " ", claims)

    def test_ask_to_a_gone_agent_falls_to_its_project(self):
        self.out("wake", "--project", "p", agent="bob-0002")
        self.out("bye", "--project", "p", agent="bob-0002")
        self.out("ask", "@bob-0002", "where is the doc?", "--project", "p",
                 agent="carl-0003")
        self.assertIn("For you:   #2 carl-0003 ask @bob-0002: where is the doc?",
                      self.out("wake", "--project", "p"))
        self.assertNotIn("where is the doc", self.out("wake", "--project", "q"))


class Config(Base):
    def test_config_errors(self):
        self.assertIn("at least ASK_DAYS",
                      self.run_cli("config", "TAIL_DAYS=3", ok=False).stderr)
        self.assertIn("positive whole number",
                      self.run_cli("config", "ASK_DAYS=x", ok=False).stderr)
        self.out("config", "ASK_DAYS=3", "TAIL_DAYS=3")
        self.assertEqual(sorted(os.listdir(self.d)), ["LOG.txt", "config"])
        with open(os.path.join(self.d, "config"), "a") as f:
            f.write("COLOUR = 3\n")
        p = self.run_cli("wake", ok=False)
        self.assertIn("line 9: COLOUR is not a size", p.stderr)
        with open(os.path.join(self.d, "config"), "w") as f:
            f.write("ASK_DAYS = 20\n")
        self.assertIn("TAIL_DAYS (14) must be at least ASK_DAYS (20)",
                      self.run_cli("wake", ok=False).stderr)


if __name__ == "__main__":
    unittest.main()
