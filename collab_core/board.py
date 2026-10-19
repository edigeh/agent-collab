"""Durable receipts, deterministic replay, artifacts, and bounded board reads."""
from __future__ import annotations
import contextlib
import copy
import base64
import binascii
import datetime as dt
import gc
import hashlib
import hmac
import json
import marshal
import os
from pathlib import Path
import re
import secrets
import tempfile
import threading
import uuid

from .journal import Journal, JournalError, JournalBusy, JournalCorrupt, Position
from . import presence, state as model

# The code that derives state: the reducer and the replay below. A saved view
# counts only for the exact source that produced it, so editing either file
# retires every saved view without anyone having to bump a version number.
REDUCER = hashlib.sha256(Path(model.__file__).read_bytes() + Path(__file__).read_bytes()).hexdigest()
VIEW_FORMAT = 2
# A command that had to replay this many records saves its view for the next
# ones. That keeps any command's replay to a few tens of milliseconds, while
# saves, which rewrite the whole view, stay at a few per busy day.
SAVE_AFTER = 256

def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False, allow_nan=False).encode('utf-8')

def digest(data):
    return hashlib.sha256(data).hexdigest()

def utcnow():
    return dt.datetime.now(dt.timezone.utc).isoformat()


BRIEF_VIEWS = ('compact', 'full')
DEFAULT_BRIEF_BUDGET = 12_000
COMPACT_PREVIEW_LIMIT = 600
COMPACT_SCOPE_LIMIT = 8


def _preview(value, limit=COMPACT_PREVIEW_LIMIT):
    if not isinstance(value, str) or len(value) <= limit:
        return value
    return value[:limit] + '… [read full item]'


def _bounded_values(value, limit=COMPACT_SCOPE_LIMIT):
    values = list(value or [])
    return values[:limit], len(values)


def render_item(kind, item, view='compact', preview_limit=COMPACT_PREVIEW_LIMIT):
    """Render a delivery item without exposing bulky fields by default."""
    model.require(view in BRIEF_VIEWS, 'brief view must be compact or full')
    if view == 'full':
        shown = copy.deepcopy(item)
        if kind == 'post':
            if item.get('stale'):
                shown['text'] = '[STALE SUMMARY: retrieve the current source claims before relying on this summary.]'
            shown['post_kind'] = shown.pop('kind')
        elif kind == 'notice':
            shown['notice_kind'] = shown.pop('kind')
        return {**shown, 'kind': kind}

    if kind == 'post':
        scope, scope_count = _bounded_values(item.get('scope'))
        text = '[STALE SUMMARY: retrieve the current source claims before relying on this summary.]' if item.get('stale') else item.get('text', '')
        rendered = {
            'id': item['id'], 'kind': 'post', 'project': item['project'],
            'actor': item['actor'], 'to': item.get('to'), 'revision': item['revision'],
            'status': item.get('status'), 'urgent': bool(item.get('urgent')),
            'stale': bool(item.get('stale')), 'due': bool(item.get('due')),
            'created_at': item.get('created_at'), 'post_kind': item.get('kind'),
            'text': _preview(text, preview_limit), 'evidence_count': len(item.get('evidence', [])),
            'refs_count': len(item.get('refs', [])), 'reports_count': len(item.get('reports', [])),
            'detail_available': True,
        }
        if item.get('scope') is not None:
            rendered['scope'] = scope
            rendered['scope_count'] = scope_count
            rendered['scope_truncated'] = scope_count > len(scope)
        return rendered

    if kind == 'task':
        scope, scope_count = _bounded_values(item.get('scope'))
        rendered = {
            'id': item['id'], 'kind': 'task', 'project': item['project'],
            'title': _preview(item.get('title', ''), min(preview_limit, 300)), 'owner': item.get('owner'),
            'creator': item.get('creator'), 'revision': item['revision'],
            'status': item.get('status'), 'priority': item.get('priority'),
            'correction': bool(item.get('correction')), 'scope': scope,
            'scope_count': scope_count, 'scope_truncated': scope_count > len(scope),
            'dependencies_count': len(item.get('dependencies', [])),
            'evidence_count': len(item.get('evidence', [])),
            'source_post': item.get('source_post'), 'created_at': item.get('created_at'),
            'detail_available': True,
        }
        if 'reason' in item:
            rendered['reason'] = _preview(item['reason'], preview_limit)
        if 'resolution' in item:
            rendered['resolution'] = item['resolution']
        return rendered

    if kind == 'notice':
        notice_kind = item.get('kind')
        return {
            'id': item['id'], 'kind': 'notice', 'notice_kind': notice_kind,
            'project': item['project'], 'recipient': item['recipient'],
            'target': item['target'], 'revision': item['revision'],
            'urgent': notice_kind in ('correction', 'ownership', 'rejection', 'overlap'),
            'text': _preview(item.get('text', ''), preview_limit), 'detail_available': True,
        }
    raise model.Rejected('invalid delivery item kind')

@contextlib.contextmanager
def _acyclic():
    """Pause the cycle collector while building board data, which holds no cycles.

    Loading a view allocates hundreds of thousands of containers, and each
    collection pass over them would find nothing to free.
    """
    paused = gc.isenabled(); gc.disable()
    try:
        yield
    finally:
        if paused:
            gc.enable()

def clone(value):
    """An exact, independent copy of plain JSON data, about twice as fast as deepcopy."""
    return marshal.loads(marshal.dumps(value))

def _outcome(env):
    event = env['payload']
    return {'status': 'accepted' if event['accepted'] else 'rejected',
            'request_id': event['request']['id'], 'seq': env['seq'], **event['result']}


class View:
    """The board replayed up to one journal position.

    `state` is the reducer state. `requests` maps each recorded request ID to
    where its record sits, [seq, start, end], so its content and outcome are
    read back from the journal itself. A view moves forward only by replaying
    committed records onto it, so it always equals a replay from record 1.
    `shared` marks a state handed to a reader: the board copies it before
    replaying more records, so a reader's state never changes under it.
    """

    def __init__(self, position=None, state=None, requests=None):
        self.position = position
        self.state = model.empty() if state is None else state
        self.requests = {} if requests is None else requests
        self.shared = False

    def replay(self, records, positions):
        """Apply committed records in order; each must reproduce its recorded result."""
        if records and self.shared:
            self.state, self.shared = clone(self.state), False
        for env, where in zip(records, positions):
            event = env['payload']
            if event.get('version') != 1:
                raise JournalCorrupt('unsupported board event version')
            req = event['request']
            if req['id'] in self.requests:
                raise JournalCorrupt('duplicate canonical request ID')
            if event['accepted']:
                try:
                    self.state, result = model.apply(self.state, req, event['recorded_at'], copy_state=False)
                except (model.Rejected, KeyError, TypeError) as exc:
                    raise JournalCorrupt(f'accepted event cannot replay: {exc}') from exc
                if result != event['result']:
                    raise JournalCorrupt('reducer result differs from recorded result')
            else:
                model.record_rejection(self.state, req, event['result']['error'], event['recorded_at'])
            self.requests[req['id']] = [where.seq, where.start, where.end]
        if positions:
            self.position = positions[-1]


def durable_directory(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)

def atomic_file(path, data):
    path = Path(path); path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, name = tempfile.mkstemp(prefix='.' + path.name + '.', dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data); stream.flush(); os.fsync(stream.fileno())
        os.replace(name, path); durable_directory(path.parent)
    finally:
        if os.path.exists(name):
            os.unlink(name)

class Board:
    def __init__(self, root, receipt_root=None, timeout=2.0):
        self.root = Path(root).expanduser().absolute()
        self.receipt_root = Path(receipt_root).expanduser().absolute() if receipt_root else self.root.with_name(self.root.name + '-receipts')
        self.timeout = timeout
        self.journal = Journal(self.root / 'events.jsonl', timeout=timeout)
        self.cache_path = self.root / 'view.json'
        self.key_path = self.root / 'view.key'
        self.checks_path = self.root / 'source-checks.json'
        self.save_after = SAVE_AFTER
        self._view = None
        self._mutex = threading.RLock()

    def capture(self, source):
        path = Path(source).expanduser().resolve(strict=True)
        if not path.is_file():
            raise ValueError(f'evidence is not a regular file: {path}')
        raw = path.read_bytes()
        sha = digest(raw); target = self.root / 'artifacts' / sha
        if target.is_symlink():
            raise JournalCorrupt('artifact path is a symlink')
        if target.exists():
            if digest(target.read_bytes()) != sha:
                raise JournalCorrupt(f'artifact corrupted: {sha}')
        else:
            atomic_file(target, raw)
        return {'source': str(path), 'sha256': sha, 'bytes': len(raw), 'captured_at': utcnow()}

    def artifact(self, sha):
        if not isinstance(sha, str) or not model.HASH.fullmatch(sha):
            raise ValueError('invalid artifact hash')
        path = self.root / 'artifacts' / sha
        if path.is_symlink(): raise JournalCorrupt('artifact path is a symlink')
        raw = path.read_bytes()
        if digest(raw) != sha:
            raise JournalCorrupt(f'artifact corrupted: {sha}')
        return raw

    def _cleanup_preview(self, sha, records):
        raw = self.artifact(sha)
        recorded = {r['payload']['request']['id'] for r in records}
        references = [{'seq':r['seq'], 'request_id':r['payload']['request']['id'],
                       'operation':r['payload']['request']['op']}
                      for r in records if not r['payload']['request']['op'].startswith('artifact.')
                      and sha in model.artifact_hashes(r['payload']['request']['data'])]
        pending = []
        for path in sorted(self.receipt_root.glob('*.jsonl')):
            with Journal(path, timeout=self.timeout).locked() as receipts:
                for record in receipts.records:
                    item = record['payload']; req = item.get('request', {})
                    if item.get('type') == 'pending' and req.get('id') not in recorded and not req.get('op', '').startswith('artifact.') and sha in model.artifact_hashes(req.get('data', {})):
                        pending.append({'session':req['actor'], 'request_id':req['id']})
        preview = {'sha256':sha, 'bytes':len(raw), 'head':records[-1]['sha256'] if records else None,
                   'references':references, 'reference_count':len(references), 'pending_receipts':pending}
        return {**preview, 'token':digest(canonical(preview))}

    def cleanup_preview(self, sha):
        with self.journal.locked() as log:
            return self._cleanup_preview(sha, log.records)

    def cleanup_remove(self, actor, sha, token):
        preview = self.cleanup_preview(sha)
        model.require(not preview['pending_receipts'], 'reconcile pending artifact receipts before cleanup')
        # The locked ingestion check verifies the preview again before recording
        # removal intent. No bytes are deleted on an outdated preview.
        compact = {key:value for key,value in preview.items() if key not in ('references', 'pending_receipts')}
        req = self.request(actor, 'artifact.remove.plan', {'sha256':sha, 'token':token, 'preview':compact})
        result = self.submit(req)
        if result['status'] != 'accepted': return result
        return self.cleanup_resume(actor, result['removal'])

    def cleanup_resume(self, actor, removal_id):
        # Keep deletion under the canonical lock. Planned removal rejects new
        # evidence references until completion; crashed deletion remains visible.
        try:
            with self._current() as (_, view):
                state = view.state
                removal = model.entity(state, 'artifact_removals', removal_id)
                model.entity(state, 'sessions', actor)
                model.require(model.may_manage(state, actor, removal['actor']) or state['sessions'][actor]['harness'] == 'human', 'only responsible agent or user can resume removal')
                if removal['status'] == 'removed':
                    return {'status':'accepted', 'removal':removal_id}
                path = self.root / 'artifacts' / removal['sha256']
                if path.is_symlink(): raise model.Rejected('artifact path is a symlink')
                path.unlink(missing_ok=True); durable_directory(path.parent)
        except (OSError, JournalError) as exc:
            return {'status':'pending', 'removal':removal_id, 'error':str(exc)}
        result = self.submit(self.request(actor, 'artifact.remove.finish', {'removal':removal_id}))
        return {**result, 'removal':removal_id}

    def _validate_evidence(self, request):
        """Verify preserved bytes and require fresh source snapshots for moderation/closure."""
        data = request['data']
        groups = [data.get('evidence', [])]
        if request['op'] == 'moderation.apply':
            groups.extend(o.get('evidence', []) for o in data.get('outcomes', []) if isinstance(o, dict))
        for refs in groups:
            if not isinstance(refs, list):
                raise model.Rejected('evidence must be a list')
            for ref in refs:
                if not isinstance(ref, dict) or not model.HASH.fullmatch(str(ref.get('sha256', ''))):
                    raise model.Rejected('invalid evidence reference')
                try:
                    self.artifact(ref['sha256'])
                    if request['op'] in ('task.finish', 'task.claim') or (request['op'] == 'moderation.apply' and ref.get('current', True)):
                        current = Path(ref['source']).read_bytes()
                        if digest(current) != ref['sha256']:
                            raise model.Rejected('source changed since capture: ' + ref['source'])
                except (OSError, ValueError, KeyError, JournalCorrupt) as exc:
                    raise model.Rejected(f'evidence unavailable: {exc}') from exc

    @contextlib.contextmanager
    def _current(self):
        """Lock the journal and yield it with this board's view at its head.

        The view resumes from this process's last one, else from view.json,
        else from a replay of the committed records read before locking, so a
        full replay never holds up other writers. Under the lock the journal
        confirms the view's position with one read and hands over only the
        records after it; if it cannot, everything replays from record 1. A
        view that needed a long replay is saved for later commands once the
        lock is released.
        """
        with self._mutex:
            replayed = 0
            with _acyclic():
                view = self._view or self._saved_view()
                if view is None:
                    view = View(); records, positions = self.journal.committed()
                    view.replay(records, positions); replayed = len(records)
            try:
                with self.journal.locked(after=view.position) as log:
                    if log.base != view.position:
                        view, replayed = View(), 0
                    replayed += len(log.records)
                    with _acyclic():
                        view.replay(log.records, log.positions)
                    self._view = view
                    yield log, view
            except BaseException:
                self._view = None  # it may be half-advanced; the next call reloads it
                raise
            if replayed >= self.save_after and view.position:
                self._save_view(view)

    def _view_key(self, create=False):
        """The secret that makes view.json something only this board's replay writes."""
        try:
            key = self.key_path.read_bytes()
        except OSError:
            key = b''
        if re.fullmatch(rb'[0-9a-f]{64}', key):
            return key
        if not create:
            return None
        # Two first saves that race may each install a key. A view signed with
        # the replaced one then fails its check once, which costs one replay.
        atomic_file(self.key_path, secrets.token_hex(32).encode())
        return self._view_key()

    def _saved_view(self):
        """The view saved in view.json, or None unless it is one this code wrote.

        The file must be in this format, from this exact reducer source, and
        carry a MAC under view.key over everything else in it. Its position is
        then checked against the journal under the lock. A damaged, edited,
        foreign or outdated file is ignored, and the journal replays instead.
        """
        key = self._view_key()
        if key is None:
            return None
        try:
            with open(self.cache_path, 'rb') as stream:
                head = stream.readline(4096); body = stream.read()
            header = json.loads(head)
            mac = header.pop('mac')
            check = hmac.new(key, canonical(header), 'sha256'); check.update(body)
            if not (isinstance(mac, str) and hmac.compare_digest(mac, check.hexdigest())):
                return None
            if header['format'] != VIEW_FORMAT or header['reducer'] != REDUCER:
                return None
            state, requests = (json.loads(line) for line in body.splitlines())
            return View(Position(**header['position']), state, requests)
        except (OSError, ValueError, KeyError, TypeError, AttributeError):
            return None

    def _save_view(self, view):
        """Save `view` to view.json; a failure only makes later commands replay more."""
        try:
            key = self._view_key(create=True)
            header = {'format': VIEW_FORMAT, 'reducer': REDUCER, 'position': view.position._asdict()}
            body = canonical(view.state) + b'\n' + canonical(view.requests) + b'\n'
            check = hmac.new(key, canonical(header), 'sha256'); check.update(body)
            atomic_file(self.cache_path, canonical({**header, 'mac': check.hexdigest()}) + b'\n' + body)
        except (OSError, TypeError):
            return False
        return True

    def read_state(self):
        """The current state, shared rather than copied: read it, do not modify it.

        The board never changes a state it has handed out, so it stays a
        consistent snapshot while the board moves on.
        """
        with self._current() as (_, view):
            view.shared = True
            return view.state

    def read_history(self):
        """The current state and every committed record, read under one lock."""
        with self._current() as (log, view):
            view.shared = True
            return view.state, log.history()

    def verify(self):
        """Replay every record from the start and compare with the incremental view.

        Commands trust the checksum at their view's position and replay only
        what follows. This rereads and validates every record, so it also finds
        damage inside that prefix. A view that differs is replaced.
        """
        with self._mutex:
            with self.journal.locked() as log, _acyclic():
                full = View(); full.replay(log.records, log.positions)
                view = self._view or self._saved_view()
                at = view.position.seq if view and view.position else 0
                if view is None or (at and (at > len(log.positions) or log.positions[at - 1] != view.position)):
                    view, at = View(), 0
                view.replay(log.records[at:], log.positions[at:])
            expected = digest(canonical([full.state, full.requests]))
            found = digest(canonical([view.state, view.requests]))
            self._view = full
            if found != expected:
                self._save_view(full)
        head = full.position._asdict() if full.position else None
        return {'status': 'accepted' if found == expected else 'rejected', 'records': head['seq'] if head else 0,
                'head': head, 'replay_sha256': expected, 'view_sha256': found, 'match': found == expected}

    def request(self, actor, op, data, request_id=None):
        return {'id': request_id or str(uuid.uuid4()), 'actor': actor, 'op': op,
                'data': data, 'observed_at': utcnow()}

    @staticmethod
    def _recorded(log, view, req, sha):
        """The outcome already recorded for this request ID, read back from its record."""
        where = view.requests.get(req['id'])
        if where is None:
            return None
        env = log.record_at(*where)
        recorded = env['payload']['request']
        if recorded['id'] != req['id']:
            raise JournalCorrupt('request index points at another record')
        if digest(canonical(recorded)) != sha:
            raise model.Rejected('request ID reused with different content')
        return _outcome(env)

    def _ingest(self, req):
        sha = digest(canonical(req))
        # Resolve committed requests before touching external evidence, then
        # recheck after validation to handle concurrent identical submissions.
        with self._current() as (log, view):
            previous = self._recorded(log, view, req, sha)
        if previous is not None:
            return previous
        # Inspect external evidence outside the board lock. This observation is
        # relevant only to new requests, never to a previously recorded outcome.
        evidence_error = None
        try:
            model.validate_request(req)
            self._validate_evidence(req)
        except model.Rejected as exc:
            evidence_error = str(exc)
        with self._current() as (log, view):
            previous = self._recorded(log, view, req, sha)
            if previous is not None:
                return previous
            recorded_at = utcnow()
            try:
                if evidence_error is not None:
                    raise model.Rejected(evidence_error)
                # External snapshots were checked before locking. Recheck the
                # preserved artifact under the deletion lock so accepted refs
                # cannot race an explicit cleanup.
                groups = [req['data'].get('evidence', [])]
                if req['op'] == 'moderation.apply':
                    groups += [out.get('evidence', []) for out in req['data'].get('outcomes', [])]
                for refs in groups:
                    for ref in refs:
                        try: self.artifact(ref['sha256'])
                        except (OSError, ValueError, JournalCorrupt) as exc:
                            raise model.Rejected('preserved evidence unavailable: ' + str(exc)) from exc
                if req['op'] == 'artifact.remove.finish':
                    removal = model.entity(view.state, 'artifact_removals', req['data'].get('removal'))
                    model.require(not (self.root / 'artifacts' / removal['sha256']).exists(), 'artifact still exists; resume physical removal first')
                if req['op'] == 'artifact.remove.plan':
                    preview = self._cleanup_preview(req['data']['sha256'], log.history())
                    model.require(not preview['pending_receipts'], 'reconcile pending artifact receipts before cleanup')
                    compact = {key:value for key,value in preview.items() if key not in ('references', 'pending_receipts')}
                    model.require(preview['token'] == req['data'].get('token') and compact == req['data'].get('preview'), 'cleanup preview changed; request a new preview')
                new_state, result = model.apply(clone(view.state), req, recorded_at, copy_state=False)
                accepted = True
            except model.Rejected as exc:
                new_state = clone(view.state); result = {'error': str(exc)}; accepted = False
                model.record_rejection(new_state, req, str(exc), recorded_at)
            event = {'version': 1, 'request': req, 'recorded_at': recorded_at,
                     'accepted': accepted, 'result': result}
            env = log.append(event)
            view.state, view.shared = new_state, False
            view.requests[req['id']] = [log.head.seq, log.head.start, log.head.end]
            view.position = log.head
            return _outcome(env)

    def _receipt_journal(self, actor):
        model.identifier(actor)
        return Journal(self.receipt_root / (actor + '.jsonl'), timeout=self.timeout)

    def submit(self, request):
        model.identifier(request.get('id')); model.identifier(request.get('actor'))
        if not isinstance(request.get('op'), str) or not isinstance(request.get('data'), dict):
            raise model.Rejected('request needs op and data')
        # Canonical serialization checks all input before any durable receipt is written.
        canonical(request)
        receipts = self._receipt_journal(request['actor'])
        with receipts.locked() as out:
            existing = next((r['payload']['request'] for r in out.records
                             if r['payload'].get('type') == 'pending' and r['payload']['request']['id'] == request['id']), None)
            if existing is not None:
                if canonical(existing) != canonical(request):
                    raise model.Rejected('request ID reused with different content')
            else:
                out.append({'type': 'pending', 'request': request})
        outcomes = self.sync(request['actor'])
        for outcome in outcomes:
            if outcome['request_id'] == request['id']:
                return outcome
        if outcomes and outcomes[-1]['status'] == 'pending':
            return {'status': 'pending', 'request_id': request['id'],
                    'error': 'Earlier receipt is pending; session ordering is preserved.'}
        return self._deliver(request, receipts)

    def _deliver(self, request, receipts):
        try:
            result = self._ingest(request)
        except model.Rejected as exc:
            return {'status': 'pending', 'request_id': request['id'], 'error': str(exc)}
        except (JournalError, OSError) as exc:
            return {'status': 'pending', 'request_id': request['id'], 'error': str(exc)}
        try:
            with receipts.locked() as out:
                if not any(r['payload'].get('type') == 'ack' and r['payload'].get('request_id') == request['id'] for r in out.records):
                    out.append({'type': 'ack', 'request_id': request['id'], 'seq': result['seq']})
        except (JournalError, OSError) as exc:
            result = {**result, 'warning': f'Board recorded outcome; receipt acknowledgment pending: {exc}'}
        return result

    def sync(self, actor):
        receipts = self._receipt_journal(actor)
        with receipts.locked() as out:
            acked = {r['payload']['request_id'] for r in out.records if r['payload'].get('type') == 'ack'}
            pending = [r['payload']['request'] for r in out.records if r['payload'].get('type') == 'pending' and r['payload']['request']['id'] not in acked]
        outcomes = []
        for req in pending:
            outcome = self._deliver(req, receipts)
            outcomes.append(outcome)
            if outcome['status'] == 'pending':
                break
        return outcomes

    def check_sources(self, actor, project, limit=8):
        """Rotate a bounded, model-free freshness check at active checkpoints.

        Only a source that changed or vanished becomes a post.observe record. A
        check that finds every source as recorded changes nothing on the board,
        so it is not journaled. When each post was last looked at is local
        scheduling, kept in source-checks.json; losing that file only restarts
        the rotation.
        """
        model.require(type(limit) is int and 1 <= limit <= 32, 'source scan limit must be 1..32')
        state = self.read_state(); model.entity(state, 'sessions', actor)
        roots = model.entity(state, 'projects', project)['roots']
        looked = self._source_checks()
        candidates = []
        for post in state['posts'].values():
            if post['project'] != project or post['due']:
                continue
            proofs = post.get('moderation', {}).get('evidence') or post.get('evidence', [])
            paths = sorted({ref['source'] for ref in proofs})
            paths = [path for path in paths if any(Path(path).resolve().is_relative_to(Path(root).resolve()) for root in roots)]
            if paths:
                known = {**{ref['source']: ref['sha256'] for ref in proofs}, **post.get('source_observations', {})}
                candidates.append((post, paths, known))
        candidates.sort(key=lambda item: (max(looked.get(item[0]['id'], ''), item[0].get('source_checked_at', '')),
                                          item[0]['created_at'], item[0]['id']))
        checked = 0; changed = 0; outcomes = []
        for post, paths, known in candidates[:limit]:
            observations = {}
            for path in paths[:32]:
                try:
                    source = Path(path)
                    observations[path] = digest(source.read_bytes()) if source.is_file() else None
                except OSError:
                    observations[path] = None
            if all(known.get(path) == sha for path, sha in observations.items()):
                # Hashing the source is the whole check: its recorded artifact is not reread.
                looked[post['id']] = utcnow(); checked += 1
                continue
            proofs = []
            for path in observations:
                try:
                    proof = self.capture(path)
                    observations[path] = proof['sha256']; proofs.append(proof)
                except (OSError, ValueError):
                    observations[path] = None
            outcome = self.submit(self.request(actor, 'post.observe', {
                'post':post['id'], 'expected':post['revision'],
                'observations':observations, 'evidence':proofs}))
            outcomes.append(outcome)
            if outcome['status'] == 'accepted':
                looked[post['id']] = utcnow(); checked += 1; changed += int(outcome['changed'])
        if candidates:
            self._save_source_checks({pid: at for pid, at in looked.items() if pid in state['posts']})
        return {'checked':checked, 'changed':changed, 'outcomes':outcomes}

    def _source_checks(self):
        try:
            value = json.loads(self.checks_path.read_bytes())
        except (OSError, ValueError):
            return {}
        return {pid: at for pid, at in value.items() if isinstance(at, str)} if isinstance(value, dict) else {}

    def _save_source_checks(self, looked):
        try:
            atomic_file(self.checks_path, canonical(looked) + b'\n')
        except OSError:
            pass  # the rotation restarts; nothing on the board depends on it

    def due(self, project=None, limit=4, force=False):
        s = self.read_state(); now = model.timestamp(utcnow())
        busy = {pid for lease in s['leases'].values() if lease['status'] == 'active' and model.timestamp(lease['expires']) > now for pid in lease['generations']}
        posts = [p for p in s['posts'].values() if p['due'] and p['id'] not in busy and (not project or p['project'] == project)]
        posts.sort(key=lambda p: (not p['urgent'], p['created_at'], p['id']))
        if not force and len(posts) < 5 and not any(p['urgent'] for p in posts):
            return []
        return [{'id': p['id'], 'revision': p['revision']} for p in posts[:limit]]

    def brief(self, actor, project=None, limit=12, offset=0, post_id=None, cursor=None,
              view='compact', budget=None, item_id=None, item_kind=None):
        model.require(type(limit) is int and 1 <= limit <= 50, 'brief limit must be 1..50')
        model.require(type(offset) is int and offset >= 0, 'offset must be nonnegative')
        model.require(not cursor or offset == 0, 'use cursor or offset, not both')
        model.require(view in BRIEF_VIEWS, 'brief view must be compact or full')
        if budget is not None:
            model.require(type(budget) is int and 512 <= budget <= 1_000_000, 'brief budget must be 512..1000000 bytes')
            model.require(view == 'compact', 'brief budget is only supported for compact view')
        if post_id is not None:
            model.require(item_id is None and item_kind in (None, 'post'), 'post_id cannot be combined with another item')
            item_id = post_id
            item_kind = 'post'
            # Preserve the historical Board.brief(post_id=...) contract: direct
            # reads expose the complete post unless the new item API is used.
            if view == 'compact':
                view = 'full'
        reconciliation = self.sync(actor)
        s = self.read_state(); session = model.entity(s, 'sessions', actor)
        project = project or session['project']; model.entity(s, 'projects', project)
        if self.check_sources(actor, project)['outcomes']:
            s = self.read_state()
        seen ={(i['kind'], i['id'], i['revision']) for d in s['deliveries'].values() if d['actor'] == actor and d['acked'] for i in d['items']}
        items = []
        if item_id is not None:
            model.require(item_kind in (None, 'post', 'task', 'notice'), 'invalid item kind')
            tables = {'post': 'posts', 'task': 'tasks', 'notice': 'notices'}
            candidates = []
            kinds = [item_kind] if item_kind else ('post', 'task', 'notice')
            for kind in kinds:
                if item_id in s[tables[kind]]:
                    candidates.append((kind, s[tables[kind]][item_id]))
            model.require(len(candidates) == 1, 'unknown or ambiguous item: ' + str(item_id))
            kind, item = candidates[0]
            if kind == 'notice':
                model.require(item['recipient'] == actor, 'notice belongs to another session')
            items.append((kind, item))
        else:
            for n in s['notices'].values():
                if n['recipient'] == actor and not n['acked']:
                    items.append(('notice', n))
            tasks = [t for t in s['tasks'].values() if t['status'] not in ('done', 'cancelled') and
                     (t['owner'] == actor or t['project'] == project)]
            tasks.sort(key=lambda t: (t['priority'] != 'urgent', t['created_at'], t['id']))
            items += [('task', t) for t in tasks]
            posts = [p for p in s['posts'].values() if (p['to'] == actor or (p['project'] == project and not p['to'])) and ('post', p['id'], p['revision']) not in seen]
            posts.sort(key=lambda p: (p['urgent'] is False, p['to'] != actor, p['created_at'], p['id']))
            items += [('post', p) for p in posts]

        def item_key(entry):
            kind, item = entry
            if kind == 'notice':
                return ['0', '0', '', item['id']]
            if kind == 'task':
                return ['1', '0' if item['priority'] == 'urgent' else '1', item['created_at'], item['id']]
            # Keep the four-field cursor shape while giving urgent posts their
            # own lane ahead of ordinary direct and project-wide messages.
            urgency = '0' if item.get('urgent') else '1'
            direct = '0' if item.get('to') == actor else '1'
            return ['2', urgency + direct, item['created_at'], item['id']]

        items.sort(key=item_key)
        if cursor:
            try:
                model.require(isinstance(cursor, str) and len(cursor) <= 2000, 'invalid cursor')
                value = json.loads(base64.urlsafe_b64decode(cursor.encode()))
                model.require(isinstance(value, dict) and value.get('actor') == actor and value.get('project') == project, 'cursor belongs to another session/project')
                after = value.get('after')
                model.require(isinstance(after, list) and len(after) == 4 and all(isinstance(x, str) for x in after), 'invalid cursor anchor')
            except (ValueError, binascii.Error, UnicodeError) as exc:
                raise model.Rejected('invalid briefing cursor') from exc
            items = [item for item in items if item_key(item) > after]

        page = items[offset:offset + limit]
        selected = []
        rendered = []
        for kind, item in page:
            shown = render_item(kind, item, view)
            if budget is not None and len(canonical(rendered + [shown])) > budget:
                if not rendered:
                    shown = render_item(kind, item, view, preview_limit=128)
                    model.require(len(canonical([shown])) <= budget, 'brief budget is too small for one compact item')
                else:
                    break
            selected.append((kind, item)); rendered.append(shown)

        refs = [{'kind': kind, 'id': item['id'], 'revision': item['revision']} for kind, item in selected]
        result = self.submit(self.request(actor, 'delivery.prepare', {'items': refs}))
        if result['status'] != 'accepted':
            return result
        has_more = bool(selected) and offset + len(selected) < len(items)
        next_cursor = base64.urlsafe_b64encode(canonical({'actor':actor, 'project':project,
            'after':item_key(selected[-1])})).decode() if has_more else None
        roster = presence.listing(s, model.timestamp(utcnow()), actor=actor, project=project)
        return {**result, 'project': project, 'items': rendered, 'total': len(items),
                'returned': len(rendered), 'view': view, 'budget': budget,
                'truncated': has_more, 'reconciliation': reconciliation,
                'next_offset': offset + len(selected) if has_more else None, 'next_cursor':next_cursor,
                **roster, 'moderation_due': self.due(project),
                'instruction': 'Acknowledge this delivery after reading the returned items. Use next_cursor for continuation without skipping items; start a new checkpoint without a cursor. Do not repeat inbox or status just to reread this page. Use read ITEM --kind post|task|notice only when full detail is needed. Requests and open commitments remain unresolved until explicitly handled.'}
