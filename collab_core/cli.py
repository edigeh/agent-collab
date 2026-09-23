"""Command-line interface for the local collaboration board."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import uuid

from .board import BRIEF_VIEWS, Board, DEFAULT_BRIEF_BUDGET, render_item
from .journal import JournalError
from .state import HARNESSES, Rejected


class CliError(ValueError):
    pass


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise CliError(message)


def _default_home():
    return os.environ.get("COLLAB_HOME", "~/.agent-collab")


def _default_receipts():
    return os.environ.get("COLLAB_RECEIPTS")


def _default_session():
    return os.environ.get("COLLAB_SESSION")


def _refs(values):
    refs = []
    for value in values or []:
        post, marker, revision = value.rpartition(":")
        if not marker or not post:
            raise CliError("references must use POST_ID:REVISION")
        try:
            number = int(revision)
        except ValueError as exc:
            raise CliError("reference revision must be an integer") from exc
        if number < 1:
            raise CliError("reference revision must be positive")
        refs.append({"id": post, "revision": number})
    return refs


def _board(args):
    return Board(args.home, receipt_root=args.receipts)


def _session(args):
    if not args.session:
        raise CliError("--session is required (or set COLLAB_SESSION); run wake first")
    return args.session


def _outcome(board, actor, operation, data):
    if operation in ('post.create', 'task.create'):
        cwd = Path.cwd().resolve()
        try:
            checkout = subprocess.check_output(['git','-C',str(cwd),'rev-parse','--show-toplevel'],
                text=True, stderr=subprocess.DEVNULL).strip()
            checkout = str(Path(checkout).resolve())
        except (OSError, subprocess.CalledProcessError):
            checkout = str(cwd)
        data = {**data, 'context':{'cwd':str(cwd), 'repository':str(_git_root(cwd)), 'worktree':checkout}}
        if 'scope' in data:
            data['scope'] = sorted({entry if entry.startswith('resource:') else str(Path(entry).expanduser().resolve()) for entry in data['scope']})
    return board.submit(board.request(actor, operation, data))


def _git_root(cwd):
    """Use the common git directory so worktrees map to one automatic project."""
    try:
        result = subprocess.run(
            ["git", "-C", str(cwd), "rev-parse", "--path-format=absolute", "--git-common-dir"],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, check=True,
        )
        common = Path(result.stdout.strip()).resolve()
        return common.parent if common.name == '.git' else common
    except (OSError, subprocess.CalledProcessError):
        return Path(cwd).resolve()


def _automatic_project(board, actor, cwd):
    root = _git_root(cwd); root_text = str(root)
    try:
        checkout = subprocess.check_output(['git','-C',str(cwd),'rev-parse','--show-toplevel'],
                         text=True, stderr=subprocess.DEVNULL).strip()
        checkout = str(Path(checkout).resolve())
    except (OSError, subprocess.CalledProcessError):
        checkout = str(Path(cwd).resolve())
    state = board.read_state()
    project = next((p for p in state['projects'].values() if root_text in p['roots'] or checkout in p['roots']), None)
    if project is None:
        project_id = 'auto-' + hashlib.sha256(root_text.encode()).hexdigest()[:16]
        if project_id in state['projects']:
            raise CliError('automatic project ID already belongs to different roots')
        created = _outcome(board, actor, 'project.create', {
            'id':project_id, 'name':root.name or root_text, 'roots':sorted({root_text, checkout})})
        state = board.read_state(); project = state['projects'].get(project_id)
        if not project or root_text not in project['roots']:
            raise CliError(created.get('error', 'could not create automatic project'))
    # Registration from a linked checkout attaches that checkout to the same
    # logical project, without granting read authority through its .git file.
    for attempt in range(3):
        if checkout in project['roots']:
            return project['id'], None
        result = _outcome(board, actor, 'project.attach', {
            'project':project['id'], 'root':checkout, 'expected':project['revision']})
        project = board.read_state()['projects'][project['id']]
        if result['status'] == 'accepted':
            return project['id'], result
    raise CliError('project attachment changed concurrently; retry wake')


def _capture(board, sources):
    return [board.capture(source) for source in sources or []]


def _wake(args):
    board = _board(args)
    actor = args.session or str(uuid.uuid4())
    # Pending observations must be reconciled before the new checkpoint.
    replayed = board.sync(actor)
    try:
        state = board.read_state()
    except (JournalError, OSError) as exc:
        # Receipts are outside the board directory and can survive its outage.
        # Omitted parent/project preserve an existing identity during replay.
        data = {'harness':args.harness}
        for field in ('project', 'parent', 'name'):
            value = getattr(args, field)
            if value is not None: data[field] = value
        registered = _outcome(board, actor, 'session.register', data)
        return {**registered, 'session':actor, 'sync':replayed,
                'warning':'Board unavailable; continue authorized work, keep this session ID, and sync later. ' + str(exc)}
    prior = state["sessions"].get(actor)
    if prior:
        if prior["harness"] != args.harness:
            raise CliError("session already belongs to harness " + prior["harness"])
        if args.parent is not None and prior["parent"] != args.parent:
            raise CliError("session already has parent " + str(prior["parent"]))
        project = args.project or prior["project"]
        parent = prior["parent"]
        name = args.name or prior["name"]
    else:
        if args.project and args.project not in state["projects"]:
            raise CliError("unknown project: " + args.project)
        # state.session.register requires an existing project, while project.create
        # requires an existing actor.  Register globally first to establish the
        # participant, then move it to an explicit or automatic project.
        parent = args.parent
        name = args.name or actor
        bootstrap = _outcome(board, actor, "session.register", {
            "harness": args.harness, "project": "global", "parent": parent, "name": name,
        })
        if bootstrap.get("status") != "accepted":
            return {"session": actor, "sync": replayed, "registration": bootstrap}
        project = args.project
        if not project:
            project, _ = _automatic_project(board, actor, Path.cwd())
    registered = _outcome(board, actor, "session.register", {
        "harness": args.harness, "project": project, "parent": parent, "name": name,
    })
    if registered.get("status") != "accepted":
        return {"session": actor, "sync": replayed, "registration": registered}
    brief = board.brief(actor, project=project, limit=args.limit, offset=0,
                        view=args.view, budget=args.budget if args.view == 'compact' else None)
    next_commands = []
    if brief.get('delivery'):
        next_commands.append(f"collab --session {actor} ack {brief['delivery']}")
    if brief.get('next_cursor'):
        next_commands.append(f"collab --session {actor} inbox --cursor {brief['next_cursor']}")
    return {"status": brief.get("status", "accepted"), "session": actor, "project": project,
            "sync": replayed, "registration": registered, "brief": brief,
            "next": next_commands}


def _project(args):
    board = _board(args)
    if args.project_command == "list":
        state = board.read_state()
        return {"status": "accepted", "projects": list(state["projects"].values())}
    actor = _session(args)
    if args.project_command == "create":
        roots = [str(Path(root).expanduser().resolve()) for root in args.root]
        return _outcome(board, actor, "project.create", {"id": args.id, "name": args.name, "roots": roots})
    return _outcome(board, actor, "project.attach", {
        "project": args.project, "root": str(Path(args.root).expanduser().resolve()), "expected": args.expected,
    })


def _post(args):
    board = _board(args); actor = _session(args)
    data = {"text": args.text, "kind": args.kind, "project": args.project,
            "to": args.to, "refs": _refs(args.ref), "scope": args.scope or [],
            "evidence": _capture(board, args.source)}
    return _outcome(board, actor, "post.create", {key: value for key, value in data.items() if value not in (None, [], "")})


def _inbox(args, post_id=None):
    board = _board(args); actor = _session(args)
    return board.brief(actor, project=getattr(args, 'project', None), limit=getattr(args, 'limit', 12),
                       offset=getattr(args, 'offset', 0), item_id=post_id,
                       item_kind=getattr(args, 'kind', None), cursor=getattr(args, 'cursor', None),
                       view=getattr(args, 'view', 'full' if post_id else 'compact'),
                       budget=(getattr(args, 'budget', None)
                               if getattr(args, 'view', 'full' if post_id else 'compact') == 'compact' else None))


def _task(args):
    board = _board(args); actor = _session(args)
    if args.task_command == "list":
        state = board.read_state()
        tasks = list(state["tasks"].values())
        project = args.project or state["sessions"][actor]["project"]
        tasks = [task for task in tasks if task["project"] == project]
        if args.view == 'compact':
            tasks = [render_item('task', task, 'compact') for task in tasks]
        return {"status": "accepted", "tasks": tasks, "view": args.view}
    if args.task_command == "create":
        data = {
            "title": args.title, "owner": args.owner or actor, "project": args.project,
            "scope": args.scope or [], "dependencies": args.dependency or [], "priority": args.priority,
        }
        return _outcome(board, actor, "task.create", {key: value for key, value in data.items() if value is not None})
    data = {"task": args.id, "expected": args.expected}
    if args.task_command == "claim":
        if args.takeover and not args.source:
            raise CliError("takeover requires at least one --source evidence file")
        data.update({"takeover": args.takeover, "evidence": _capture(board, args.source)})
        return _outcome(board, actor, "task.claim", data)
    if args.task_command == "finish":
        if not args.source:
            raise CliError("finish requires at least one --source evidence file")
        data.update({"evidence": _capture(board, args.source)})
        if args.resolution:
            data["resolution"] = args.resolution
        return _outcome(board, actor, "task.finish", data)
    data["reason"] = args.reason
    return _outcome(board, actor, "task." + args.task_command, data)


def _status(args):
    board = _board(args); state = board.read_state()
    project = args.project or (state["sessions"].get(args.session, {}).get("project") if args.session else None)
    tasks = [task for task in state["tasks"].values() if not project or task["project"] == project]
    posts = [post for post in state["posts"].values() if not project or post["project"] == project]
    if args.view == 'compact':
        tasks = [render_item('task', task, 'compact') for task in tasks]
    return {"status": "accepted", "project": project, "view": args.view,
            "sessions": len(state["sessions"]), "active_tasks": sum(task["status"] not in ("done", "cancelled") for task in tasks),
            "open_requests": sum(1 for task in tasks if task.get("source_post") and task["status"] not in ("done", "cancelled")),
            "disputed_posts": sum(post["status"] == "disputed" for post in posts),
            "moderation_due": board.due(project), "tasks": tasks}


def _artifact(args):
    board = _board(args); raw = board.artifact(args.sha)
    result = {"status": "accepted", "sha256": args.sha, "bytes": len(raw),
              "path": str(board.root / "artifacts" / args.sha)}
    if args.display:
        result["content"] = raw.decode("utf-8", errors="replace")
    return result


def _moderate(args):
    board = _board(args); actor = _session(args)
    # Kept lazy so ordinary board use does not require a model runner.
    from .moderator import run_moderation
    return run_moderation(board, actor, args.project, args.force)


def _cleanup(args):
    board = _board(args)
    if args.cleanup_command == 'preview': return board.cleanup_preview(args.sha)
    if args.cleanup_command == 'remove': return board.cleanup_remove(_session(args), args.sha, args.confirm)
    return board.cleanup_resume(_session(args), args.removal)


def _human(value):
    status = value.get("status") if isinstance(value, dict) else None
    lines = [str(status or "accepted")]
    if isinstance(value, dict):
        for key in ("session", "project", "post", "task", "delivery", "next_cursor", "sha256", "path", "bytes", "token", "removal", "sessions", "active_tasks", "open_requests", "disputed_posts", "view", "returned", "budget", "truncated", "error", "warning"):
            if key in value:
                lines.append(f"{key}: {value[key]}")
        for ref in value.get("references", []):
            lines.append(f"affected history: event {ref['seq']} {ref['operation']} {ref['request_id']}")
        for ref in value.get("pending_receipts", []):
            lines.append(f"pending receipt blocks cleanup: {ref['session']} {ref['request_id']}")
        for command in value.get("next", []):
            lines.append("next: " + command)
        brief = value.get("brief")
        if isinstance(brief, dict):
            lines.append(f"brief: {len(brief.get('items', []))} item(s), {brief.get('peer_count', 0)} peer(s)")
            lines.extend(_human(brief).splitlines()[1:])
        if "items" in value:
            for item in value["items"]:
                text = item.get("text", item.get("title", ""))
                label = item.get("status", "notice")
                if item.get('kind') == 'notice' and item.get('notice_kind'):
                    label = 'notice/' + item['notice_kind']
                priority = " urgent" if item.get("urgent") or item.get("priority") == "urgent" else ""
                lines.append(f"{item['kind']} {item['id']} r{item['revision']} [{label}{priority}]: {text}")
                if item['kind'] == 'task':
                    lines.append(f"owner: {item.get('owner')}; scope: {', '.join(item.get('scope', []))}")
                for proof in item.get('evidence', []):
                    lines.append(f"captured source: {proof['source']} sha256={proof['sha256']}")
                moderation = item.get('moderation', {})
                if moderation:
                    lines.append('moderation reason: ' + moderation['reason'])
                    for proof in moderation.get('evidence', []):
                        lines.append(f"moderation source: {proof['source']} sha256={proof['sha256']}")
                for report in item.get('reports', [])[-1:]:
                    lines.append('latest report: ' + report['reason'])
                    for proof in report.get('evidence', []):
                        lines.append(f"report source: {proof['source']} sha256={proof['sha256']}")
        if "tasks" in value:
            for task in value["tasks"]:
                lines.append(f"task {task['id']} r{task['revision']} {task['status']} owner={task.get('owner')}: {task['title']}")
                lines.append("scope: " + ", ".join(task.get("scope", [])))
        if value.get("moderation_due"):
            lines.append(f"moderation due: {len(value['moderation_due'])} target(s); run moderate at this checkpoint")
        if value.get("instruction"):
            lines.append(value["instruction"])
        if "content" in value:
            lines.append(value["content"])
    return "\n".join(lines)


def build_parser():
    parser = Parser(prog="collab", description="Local append-only collaboration board")
    parser.add_argument("--home", default=_default_home(), help="board directory (default: COLLAB_HOME or ~/.agent-collab)")
    parser.add_argument("--receipts", default=_default_receipts(), help="receipt directory (default: COLLAB_RECEIPTS)")
    parser.add_argument("--session", default=_default_session(), help="session ID (default: COLLAB_SESSION)")
    parser.add_argument("--json", action="store_true", help="emit one JSON object")
    sub = parser.add_subparsers(dest="command", required=True)
    wake = sub.add_parser("wake"); wake.add_argument("--harness", required=True, choices=HARNESSES); wake.add_argument("--project"); wake.add_argument("--parent"); wake.add_argument("--name"); wake.add_argument("--limit", type=int, default=12); wake.add_argument("--view", choices=BRIEF_VIEWS, default='compact'); wake.add_argument("--budget", type=int, default=DEFAULT_BRIEF_BUDGET, help="maximum compact item payload in bytes"); wake.set_defaults(handler=_wake)
    sync = sub.add_parser("sync"); sync.set_defaults(handler=lambda a: {"status": "accepted", "results": _board(a).sync(_session(a))})
    project = sub.add_parser("project"); ps = project.add_subparsers(dest="project_command", required=True)
    ps.add_parser("list").set_defaults(handler=_project)
    create = ps.add_parser("create"); create.add_argument("name"); create.add_argument("id"); create.add_argument("--root", action="append", default=[]); create.set_defaults(handler=_project)
    attach = ps.add_parser("attach"); attach.add_argument("project"); attach.add_argument("root"); attach.add_argument("--expected", required=True, type=int); attach.set_defaults(handler=_project)
    post = sub.add_parser("post"); post.add_argument("text"); post.add_argument("--kind", choices=("message", "finding", "request", "summary", "work"), default="message"); post.add_argument("--project"); post.add_argument("--to"); post.add_argument("--source", action="append", default=[]); post.add_argument("--ref", action="append", default=[]); post.add_argument("--scope", action="append", default=[]); post.set_defaults(handler=_post)
    read = sub.add_parser("read"); read.add_argument("item"); read.add_argument("--kind", choices=("post", "task", "notice")); read.add_argument("--view", choices=BRIEF_VIEWS, default='full'); read.set_defaults(handler=lambda a: _inbox(a, a.item))
    inbox = sub.add_parser("inbox"); inbox.add_argument("--project"); inbox.add_argument("--limit", type=int, default=12); inbox.add_argument("--offset", type=int, default=0); inbox.add_argument("--cursor"); inbox.add_argument("--view", choices=BRIEF_VIEWS, default='compact'); inbox.add_argument("--budget", type=int, default=DEFAULT_BRIEF_BUDGET, help="maximum compact item payload in bytes"); inbox.set_defaults(handler=_inbox)
    ack = sub.add_parser("ack"); ack.add_argument("delivery"); ack.set_defaults(handler=lambda a: _outcome(_board(a), _session(a), "delivery.ack", {"delivery": a.delivery}))
    task = sub.add_parser("task"); ts = task.add_subparsers(dest="task_command", required=True)
    listing = ts.add_parser("list"); listing.add_argument("--project"); listing.add_argument("--view", choices=BRIEF_VIEWS, default='full'); listing.set_defaults(handler=_task)
    tc = ts.add_parser("create"); tc.add_argument("title"); tc.add_argument("--owner"); tc.add_argument("--project"); tc.add_argument("--scope", action="append", default=[]); tc.add_argument("--dependency", action="append", default=[]); tc.add_argument("--priority", choices=("normal", "urgent"), default="normal"); tc.set_defaults(handler=_task)
    claim = ts.add_parser("claim"); claim.add_argument("id"); claim.add_argument("--expected", required=True, type=int); claim.add_argument("--takeover", action="store_true"); claim.add_argument("--source", action="append", default=[]); claim.set_defaults(handler=_task)
    finish = ts.add_parser("finish"); finish.add_argument("id"); finish.add_argument("--expected", required=True, type=int); finish.add_argument("--source", action="append", default=[]); finish.add_argument("--resolution", choices=("corrected", "mistaken")); finish.set_defaults(handler=_task)
    for action in ("block", "cancel"):
        p = ts.add_parser(action); p.add_argument("id"); p.add_argument("--expected", required=True, type=int); p.add_argument("--reason", required=True); p.set_defaults(handler=_task)
    report = sub.add_parser("report"); report.add_argument("post"); report.add_argument("--expected", required=True, type=int); report.add_argument("--reason", required=True); report.add_argument("--source", action="append", default=[]); report.set_defaults(handler=lambda a: _outcome(_board(a), _session(a), "post.report", {"post": a.post, "expected": a.expected, "reason": a.reason, "evidence": _capture(_board(a), a.source)}))
    status = sub.add_parser("status"); status.add_argument("--project"); status.add_argument("--view", choices=BRIEF_VIEWS, default='full'); status.set_defaults(handler=_status)
    artifact = sub.add_parser("artifact"); artifact.add_argument("sha"); group = artifact.add_mutually_exclusive_group(); group.add_argument("--display", action="store_true"); group.add_argument("--path", action="store_true"); artifact.set_defaults(handler=_artifact)
    moderate = sub.add_parser("moderate"); moderate.add_argument("--project"); moderate.add_argument("--force", action="store_true"); moderate.set_defaults(handler=_moderate)
    cleanup = sub.add_parser("cleanup"); cs = cleanup.add_subparsers(dest="cleanup_command", required=True)
    preview = cs.add_parser("preview"); preview.add_argument("sha"); preview.set_defaults(handler=_cleanup)
    remove = cs.add_parser("remove"); remove.add_argument("sha"); remove.add_argument("--confirm", required=True); remove.set_defaults(handler=_cleanup)
    resume = cs.add_parser("resume"); resume.add_argument("removal"); resume.set_defaults(handler=_cleanup)
    return parser


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    json_mode = "--json" in argv
    try:
        args = build_parser().parse_args(argv)
        value = args.handler(args)
        status = value.get("status") if isinstance(value, dict) else "accepted"
        code = 1 if status == "rejected" else 0
    except (CliError, Rejected, JournalError, OSError, ValueError) as exc:
        value = {"status": "rejected", "error": str(exc)}
        code = 1
    if json_mode:
        print(json.dumps(value, sort_keys=True, ensure_ascii=False))
    else:
        print(_human(value))
    return code


if __name__ == "__main__":
    raise SystemExit(main())
