"""Deterministic board transitions. Only recorded inputs influence replay."""
from __future__ import annotations
import copy
import datetime as dt
import fnmatch
import hashlib
import json
import re

TABLES = ('projects', 'sessions', 'posts', 'tasks', 'deliveries', 'notices', 'leases', 'artifact_removals')
# Participating harnesses: the agents that run the CLI, plus the user.
HARNESSES = ('codex', 'claude', 'pi', 'omp', 'human')
ID = re.compile(r'^[A-Za-z0-9_.:-]{1,160}$')
HASH = re.compile(r'^[a-f0-9]{64}$')

class Rejected(ValueError):
    """A command cannot be accepted against the observed board state."""

def empty():
    state = {name: {} for name in TABLES}
    state['projects']['global'] = {'id': 'global', 'name': 'Shared system', 'roots': [], 'revision': 1}
    return state

def require(test, message):
    if not test:
        raise Rejected(message)

def identifier(value):
    require(isinstance(value, str) and ID.fullmatch(value), 'invalid identifier')
    return value

def text(value, field='text', limit=32000):
    require(isinstance(value, str) and value.strip() and len(value.encode('utf-8')) <= limit,
            f'{field} must be nonempty text of at most {limit} bytes')
    return value

def timestamp(value):
    try:
        parsed = dt.datetime.fromisoformat(value.replace('Z', '+00:00'))
        require(parsed.tzinfo is not None, 'timestamp requires timezone')
        return parsed
    except (AttributeError, ValueError) as exc:
        raise Rejected('invalid timestamp') from exc

def entity(state, table, key):
    require(isinstance(key, str), f'{table} identifier must be text')
    require(key in state[table], f'unknown {table}: {key}')
    return state[table][key]

def expected(item, data):
    require(type(data.get('expected')) is int and item['revision'] == data['expected'],
            f'revision conflict: current revision is {item["revision"]}')

def evidence(value):
    require(isinstance(value, list) and 0 < len(value) <= 32, 'captured evidence is required')
    for ref in value:
        require(isinstance(ref, dict) and HASH.fullmatch(str(ref.get('sha256', ''))) and
                isinstance(ref.get('source'), str), 'evidence requires source and SHA-256')
    return copy.deepcopy(value)

def artifact_hashes(value):
    if isinstance(value, dict):
        found = {value['sha256']} if isinstance(value.get('sha256'), str) else set()
        return found | set().union(*(artifact_hashes(v) for v in value.values()))
    if isinstance(value, list):
        return set().union(*(artifact_hashes(v) for v in value))
    return set()

def overlaps(left, right):
    for a in left:
        for b in right:
            if a == b or fnmatch.fnmatchcase(a, b) or fnmatch.fnmatchcase(b, a):
                return True
            if a.rstrip('/').startswith(b.rstrip('/') + '/') or b.rstrip('/').startswith(a.rstrip('/') + '/'):
                return True
    return False

def scope(value):
    require(isinstance(value, list) and len(value) <= 64, 'scope must be a list of at most 64 paths/resources')
    return sorted(set(text(x, 'scope', 1024) for x in value))

def may_manage(state, actor, owner):
    while owner:
        if actor == owner:
            return True
        owner = state['sessions'].get(owner, {}).get('parent')
    return False

def notice(state, nid, recipient, project, kind, target, message):
    nid = 'n-' + hashlib.sha256(json.dumps([nid, recipient, project, kind, target]).encode()).hexdigest()
    require(nid not in state['notices'], 'derived notice ID collision')
    state['notices'][nid] = {'id': nid, 'recipient': recipient, 'project': project,
                            'kind': kind, 'target': target, 'text': message, 'acked': False, 'revision': 1}

def recipients(state, post_ids):
    return {d['actor'] for d in state['deliveries'].values()
            if any(i['kind'] == 'post' and i['id'] in post_ids for i in d['items'])}

def invalidate(state, changed, event_id):
    affected = set(changed)
    while True:
        extra = {p['id'] for p in state['posts'].values() if p['kind'] == 'summary' and
                 any(ref['id'] in affected for ref in p.get('refs', []))} - affected
        if not extra:
            break
        affected |= extra
    for pid in affected - set(changed):
        state['posts'][pid]['stale'] = True
        state['posts'][pid]['revision'] += 1
    for pid in sorted(affected):
        p = state['posts'][pid]
        for actor in sorted(recipients(state, {pid})):
            notice(state, f'{event_id}:{pid}:{actor}', actor, p['project'], 'correction', pid,
                   f'Information changed: {pid}; read its current revision before relying on it.')
    return affected

def validate_request(request):
    require(isinstance(request, dict), 'request must be an object')
    identifier(request.get('id')); identifier(request.get('actor'))
    require(isinstance(request.get('op'), str), 'operation must be text')
    d = request.get('data'); require(isinstance(d, dict), 'data must be an object')
    if request['op'] == 'artifact.remove.plan':
        require(isinstance(d.get('sha256'), str) and HASH.fullmatch(d['sha256']), 'invalid artifact hash')
        require(isinstance(d.get('token'), str) and HASH.fullmatch(d['token']), 'cleanup requires a preview token')
        require(isinstance(d.get('preview'), dict), 'cleanup requires a preview object')
    elif request['op'] == 'artifact.remove.finish':
        identifier(d.get('removal'))
    if 'observed_at' in request:
        timestamp(request['observed_at'])
    for field in ('items', 'refs', 'posts', 'outcomes', 'evidence', 'source_fixes'):
        if field in d:
            require(isinstance(d[field], list), f'{field} must be a list')
            require(all(isinstance(x, dict) for x in d[field]), f'{field} entries must be objects')
    for field in ('roots', 'scope', 'dependencies'):
        if field in d:
            require(isinstance(d[field], list) and all(isinstance(x, str) for x in d[field]), f'{field} must contain strings')
    for outcome in d.get('outcomes', []):
        identifier(outcome.get('post'))
        for field in ('evidence', 'source_fixes'):
            if field in outcome:
                require(isinstance(outcome[field], list) and all(isinstance(x, dict) for x in outcome[field]), f'outcome {field} must contain objects')
    for ref in d.get('posts', []) + d.get('refs', []) + d.get('items', []):
        require(isinstance(ref.get('id'), str) and type(ref.get('revision')) is int and ref['revision'] > 0, 'references need id and positive revision')
    if 'name' in d:
        text(d['name'], 'name', 200)


def record_rejection(state, request, reason, recorded_at):
    project = state['sessions'].get(request['actor'], {}).get('project', 'global')
    notice(state, 'rejection:' + request['id'], request['actor'], project, 'rejection', request['id'],
           f'Request {request["id"]} was rejected: {reason}. Reconcile the intended work explicitly.')


def apply(state, request, recorded_at, *, copy_state=True):
    """Return (new_state, result); rejected requests never mutate the input state."""
    validate_request(request)
    s = copy.deepcopy(state) if copy_state else state
    identifier(request.get('id')); actor = identifier(request.get('actor'))
    op = request.get('op'); d = request.get('data')
    require(isinstance(d, dict), 'data must be an object')
    now = timestamp(recorded_at)
    rid = request['id']
    if op != 'session.register':
        entity(s, 'sessions', actor)
    result = {'id': rid}
    if not op.startswith('artifact.'):
        removing = {item['sha256'] for item in s['artifact_removals'].values() if item['status'] == 'planned'}
        require(not (artifact_hashes(d) & removing), 'artifact removal is pending; evidence cannot be newly referenced')

    if op == 'session.register':
        require(d.get('harness') in HARNESSES, 'unknown harness')
        prior = s['sessions'].get(actor, {})
        project = d.get('project', prior.get('project', 'global')); entity(s, 'projects', project)
        parent = d.get('parent', prior.get('parent'))
        if parent:
            entity(s, 'sessions', parent); require(parent != actor, 'session cannot parent itself')
        if actor in s['sessions']:
            item = s['sessions'][actor]
            require(item['harness'] == d['harness'] and item['parent'] == parent,
                    'session identity already registered with a different harness/parent')
            item['seen_at'] = recorded_at; item['project'] = project
        else:
            s['sessions'][actor] = {'id': actor, 'harness': d['harness'], 'parent': parent,
                'project': project, 'name': d.get('name', actor), 'seen_at': recorded_at,
                'started_at': recorded_at, 'revision': 1}
        result['session'] = actor
    elif op == 'session.touch':
        s['sessions'][actor]['seen_at'] = recorded_at
        if 'project' in d:
            entity(s, 'projects', d['project']); s['sessions'][actor]['project'] = d['project']
        result['session'] = actor
    elif op == 'artifact.remove.plan':
        sha = d.get('sha256'); require(isinstance(sha, str) and HASH.fullmatch(sha), 'invalid artifact hash')
        require(not any(r['sha256'] == sha and r['status'] == 'planned' for r in s['artifact_removals'].values()), 'artifact removal already pending')
        s['artifact_removals'][rid] = {'id':rid, 'actor':actor, 'sha256':sha, 'status':'planned',
            'preview':copy.deepcopy(d['preview']), 'created_at':recorded_at}
        changed = set()
        for p in s['posts'].values():
            if sha in artifact_hashes(p):
                p['due'] = True; p['urgent'] = True; p['status'] = 'disputed'; p['revision'] += 1
                p['reports'].append({'id':rid, 'actor':actor, 'reason':'Preserved evidence explicitly scheduled for removal: ' + sha,
                    'evidence':[], 'observed_at':recorded_at, 'ingested_at':recorded_at})
                changed.add(p['id'])
        invalidate(s, changed, rid)
        result['removal'] = rid
    elif op == 'artifact.remove.finish':
        removal = entity(s, 'artifact_removals', d.get('removal'))
        require(may_manage(s, actor, removal['actor']) or s['sessions'][actor]['harness'] == 'human', 'only responsible agent or user can resume removal')
        removal['status'] = 'removed'; removal['finished_at'] = recorded_at
        result['removal'] = removal['id']
    elif op == 'project.create':
        pid = identifier(d.get('id', rid)); require(pid not in s['projects'], 'project already exists')
        roots = d.get('roots', []); require(isinstance(roots, list), 'roots must be a list')
        for root in roots:
            text(root, 'root'); require(root.startswith('/'), 'repository roots must be absolute')
            require(not any(root in p['roots'] for p in s['projects'].values()), 'repository already attached')
        s['projects'][pid] = {'id': pid, 'name': text(d.get('name'), 'name', 200),
                             'roots': sorted(set(roots)), 'revision': 1}
        result['project'] = pid
    elif op == 'project.attach':
        item = entity(s, 'projects', d.get('project')); expected(item, d)
        root = text(d.get('root'), 'root'); require(root.startswith('/'), 'root must be absolute')
        require(not any(root in p['roots'] and p['id'] != item['id'] for p in s['projects'].values()),
                'repository already attached elsewhere')
        item['roots'] = sorted(set(item['roots'] + [root])); item['revision'] += 1
        result['project'] = item['id']
    elif op == 'post.create':
        project = d.get('project', s['sessions'][actor]['project']); entity(s, 'projects', project)
        kind = d.get('kind', 'message'); require(kind in ('message', 'finding', 'request', 'summary', 'work'), 'invalid post kind')
        recipient = d.get('to')
        if recipient:
            entity(s, 'sessions', recipient)
        refs = d.get('refs', [])
        require(isinstance(refs, list), 'refs must be a list')
        for ref in refs:
            source = entity(s, 'posts', ref.get('id'))
            require(source['revision'] == ref.get('revision') and not source.get('stale'), 'summary source is stale')
        if kind == 'summary':
            require(refs, 'summary requires source revisions')
        require(rid not in s['posts'], 'post already exists')
        proof = evidence(d['evidence']) if d.get('evidence') else []
        s['posts'][rid] = {'id': rid, 'project': project, 'actor': actor, 'kind': kind,
            'text': text(d.get('text')), 'to': recipient, 'revision': 1, 'status': 'unverified',
            'refs': copy.deepcopy(refs), 'evidence': proof, 'created_at': recorded_at,
            'observed_at': request.get('observed_at', recorded_at), 'ingested_at': recorded_at, 'reports': [],
            'due': True, 'urgent': False, 'stale': False,
            'moderation_generation': 0, 'context': copy.deepcopy(d.get('context', {}))}
        if kind == 'request':
            tid = rid + ':task'
            require(tid not in s['tasks'], 'derived request task ID collision')
            s['tasks'][tid] = {'id': tid, 'project': project, 'title': d['text'][:200],
                'owner': recipient, 'creator': actor, 'scope': [], 'status': 'open', 'revision': 1,
                'priority': 'normal', 'evidence': [], 'source_post': rid, 'correction': False,
                'created_at': recorded_at, 'dependencies': [], 'context':copy.deepcopy(d.get('context', {}))}
            result['task'] = tid
        if kind == 'work':
            worked = scope(d.get('scope', [])); s['posts'][rid]['scope'] = worked
            for task in s['tasks'].values():
                if task['project'] == project and task['status'] not in ('done', 'cancelled') and task['owner'] != actor and overlaps(worked, task['scope']):
                    for recipient in sorted({actor, task['owner']} - {None}):
                        notice(s, f'{rid}:{task["id"]}:{recipient}', recipient, project, 'overlap', task['id'],
                               f'Reconcile work {rid} with assignment {task["id"]}; no prior coordination is implied.')
        result['post'] = rid
    elif op == 'post.observe':
        p = entity(s, 'posts', d.get('post')); expected(p, d)
        observations = d.get('observations')
        require(isinstance(observations, dict) and 0 < len(observations) <= 32, 'source observations must be a bounded object')
        proof = evidence(d['evidence']) if d.get('evidence') else []
        baseline = {ref['source']:ref['sha256'] for ref in (p.get('moderation', {}).get('evidence') or p.get('evidence', []))}
        prior = {**baseline, **p.get('source_observations', {})}
        for source, sha in observations.items():
            require(isinstance(source, str) and source in baseline, 'observation must reference a known source')
            require(sha is None or isinstance(sha, str) and HASH.fullmatch(sha), 'invalid observed hash')
            require(sha is None or any(ref['source'] == source and ref['sha256'] == sha for ref in proof), 'observed bytes require captured evidence')
        changes = sorted(source for source, sha in observations.items() if prior.get(source) != sha)
        p['source_checked_at'] = recorded_at
        p['source_observations'] = {**p.get('source_observations', {}), **observations}
        if changes:
            p['due'] = True; p['urgent'] = True; p['revision'] += 1; p['status'] = 'disputed'
            p['reports'].append({'id':rid, 'actor':actor,
                'reason':'Source changed or became unavailable; recheck claim: ' + ', '.join(changes),
                'evidence':proof, 'observed_at':request.get('observed_at', recorded_at), 'ingested_at':recorded_at})
            invalidate(s, {p['id']}, rid)
        result.update(post=p['id'], changed=bool(changes))
    elif op == 'post.report':
        p = entity(s, 'posts', d.get('post')); expected(p, d)
        p['due'] = True; p['urgent'] = True; p['revision'] += 1
        p['reports'].append({'id': rid, 'actor': actor, 'reason': text(d.get('reason')),
            'evidence': evidence(d['evidence']) if d.get('evidence') else [],
            'observed_at': request.get('observed_at', recorded_at), 'ingested_at': recorded_at})
        p['status'] = 'disputed'
        invalidate(s, {p['id']}, rid); result['post'] = p['id']
    elif op == 'task.create':
        project = d.get('project', s['sessions'][actor]['project']); entity(s, 'projects', project)
        owner = d.get('owner', actor)
        if owner:
            entity(s, 'sessions', owner)
        sc = scope(d.get('scope', []))
        dependencies = d.get('dependencies', [])
        require(isinstance(dependencies, list), 'dependencies must be a list')
        for dep in dependencies:
            entity(s, 'tasks', dep)
        conflicts = [t['id'] for t in s['tasks'].values() if t['project'] == project and
            t['status'] not in ('done', 'cancelled') and t['owner'] and owner and overlaps(sc, t['scope'])]
        require(not conflicts, 'scope conflict with ' + ', '.join(conflicts))
        require(rid not in s['tasks'], 'task already exists')
        priority = d.get('priority', 'normal'); require(priority in ('normal', 'urgent'), 'invalid priority')
        s['tasks'][rid] = {'id': rid, 'project': project, 'title': text(d.get('title'), 'title', 300),
            'owner': owner, 'creator': actor, 'scope': sc, 'status': 'open', 'revision': 1,
            'priority': priority, 'evidence': [], 'correction': False, 'created_at': recorded_at,
            'dependencies': dependencies, 'context':copy.deepcopy(d.get('context', {}))}
        result['task'] = rid
    elif op in ('task.claim', 'task.finish', 'task.block', 'task.cancel'):
        task = entity(s, 'tasks', d.get('task')); expected(task, d)
        require(task['status'] not in ('done', 'cancelled'), 'task already closed')
        if op == 'task.claim':
            old = task['owner']
            if old and old != actor:
                require(d.get('takeover') is True, 'owned task requires explicit takeover')
                task['takeover_evidence'] = evidence(d.get('evidence'))
                notice(s, f'{rid}:takeover', old, task['project'], 'ownership', task['id'],
                       f'{actor} took over {task["id"]}; reconcile before resuming.')
            for other in s['tasks'].values():
                if other['id'] != task['id'] and other['project'] == task['project'] and other['owner'] and other['status'] not in ('done','cancelled'):
                    require(not overlaps(task['scope'], other['scope']), f'scope conflict with {other["id"]}')
            task['owner'] = actor; task['status'] = 'open'
        else:
            require(may_manage(s, actor, task['owner']) or (not task['owner'] and task['creator'] == actor),
                    'only owner or parent can change this task')
            if op == 'task.finish':
                require(all(s['tasks'][dep]['status'] == 'done' for dep in task['dependencies']), 'unfinished dependencies')
                task['evidence'] = evidence(d.get('evidence'))
                if task['correction']:
                    require(d.get('resolution') in ('corrected', 'mistaken'), 'correction requires corrected/mistaken resolution')
                    task['resolution'] = d['resolution']
                task['status'] = 'done'
            elif op == 'task.cancel':
                require(not task['correction'], 'urgent correction requires evidence-backed resolution')
                task['status'] = 'cancelled'; task['reason'] = text(d.get('reason'))
            else:
                task['status'] = 'blocked'; task['reason'] = text(d.get('reason'))
        task['revision'] += 1; result['task'] = task['id']
    elif op == 'delivery.prepare':
        items = d.get('items'); require(isinstance(items, list) and len(items) <= 50, 'invalid delivery size')
        for ref in items:
            require(ref.get('kind') in ('post', 'task', 'notice'), 'invalid delivery reference')
            item = entity(s, ref['kind'] + ('s' if ref['kind'] != 'notice' else 's'), ref.get('id'))
            require(item['revision'] == ref.get('revision'), 'delivery source changed')
            if ref['kind'] == 'notice':
                require(item['recipient'] == actor, 'notice belongs to another session')
        s['deliveries'][rid] = {'id': rid, 'actor': actor, 'items': copy.deepcopy(items), 'acked': False,
                               'created_at': recorded_at, 'revision': 1}
        result['delivery'] = rid
    elif op == 'delivery.ack':
        item = entity(s, 'deliveries', d.get('delivery')); require(item['actor'] == actor, 'delivery belongs to another session')
        item['acked'] = True
        for ref in item['items']:
            if ref['kind'] == 'notice':
                s['notices'][ref['id']]['acked'] = True
        result['delivery'] = item['id']
    elif op == 'moderation.claim':
        refs = d.get('posts'); require(isinstance(refs, list) and 0 < len(refs) <= 8, 'moderation batch must contain 1..8 posts')
        require(len({r['id'] for r in refs}) == len(refs), 'duplicate moderation target')
        seconds = d.get('seconds', 600); require(type(seconds) is int and 30 <= seconds <= 1800, 'invalid lease duration')
        generations = {}
        for ref in refs:
            p = entity(s, 'posts', ref.get('id')); require(p['revision'] == ref.get('revision'), 'moderation target changed')
            require(p['due'], 'post not due for moderation')
            for lease in s['leases'].values():
                if lease['status'] == 'active' and timestamp(lease['expires']) > now:
                    require(p['id'] not in lease['generations'], 'moderation already assigned')
            p['moderation_generation'] += 1; generations[p['id']] = p['moderation_generation']
        s['leases'][rid] = {'id': rid, 'actor': actor, 'posts': copy.deepcopy(refs), 'generations': generations,
            'expires': (now + dt.timedelta(seconds=seconds)).isoformat(), 'status': 'active', 'revision': 1}
        result['lease'] = rid
    elif op in ('moderation.apply', 'moderation.release'):
        lease = entity(s, 'leases', d.get('lease')); require(lease['actor'] == actor and lease['status'] == 'active', 'invalid moderation owner/state')
        if op == 'moderation.release':
            lease['status'] = 'released'; lease['reason'] = text(d.get('reason'))
        else:
            require(timestamp(lease['expires']) > now, 'moderation lease expired')
            outcomes = d.get('outcomes'); require(isinstance(outcomes, list), 'outcomes must be a list')
            wanted = {r['id']: r['revision'] for r in lease['posts']}
            require(len(outcomes) == len(wanted) and {r.get('post') for r in outcomes} == set(wanted), 'outcomes must cover batch exactly')
            changed = set()
            for out in outcomes:
                p = entity(s, 'posts', out['post'])
                require(p['revision'] == wanted[p['id']] and p['moderation_generation'] == lease['generations'][p['id']], 'stale moderation result')
                status = out.get('status'); require(status in ('supported','unsupported','disputed','corrected','superseded'), 'invalid moderation classification')
                proof = evidence(out.get('evidence')) if status in ('supported','corrected','superseded') else (evidence(out['evidence']) if out.get('evidence') else [])
                reason = text(out.get('reason'))
                if status in ('corrected', 'superseded'):
                    p['text'] = text(out.get('text'), 'corrected text')
                p['status'] = status; p['revision'] += 1; p['due'] = False; p['urgent'] = False
                p['moderation'] = {'actor': actor, 'reason': reason, 'evidence': proof, 'event': rid, 'time': recorded_at}
                changed.add(p['id'])
                fixes = out.get('source_fixes', []); require(isinstance(fixes, list), 'source_fixes must be a list')
                require(not fixes or status in ('corrected', 'superseded'), 'source fixes require an evidence-backed correction')
                for n, fix in enumerate(fixes):
                    project = fix.get('project', p['project']); entity(s, 'projects', project)
                    source = text(fix.get('source'), 'source')
                    require(any(ref['source'] == source for ref in proof), 'source fix requires evidence for that source')
                    justification = text(fix.get('reason'))
                    key = source + ':' + '|'.join(sorted(x['sha256'] for x in proof))
                    existing = next((t for t in s['tasks'].values() if t.get('correction_key') == key and t['project'] == project and t['status'] != 'done'), None)
                    if existing:
                        existing['related_posts'] = sorted(set(existing['related_posts'] + [p['id']]))
                        existing['revision'] += 1
                    else:
                        tid = f'{rid}:fix:{p["id"]}:{n}'
                        require(tid not in s['tasks'], 'derived correction task ID collision')
                        s['tasks'][tid] = {'id': tid, 'project': project, 'title': justification[:300],
                            'owner': None, 'creator': actor, 'scope': [source], 'status': 'open',
                            'priority': 'urgent', 'correction': True, 'correction_key': key,
                            'related_posts': [p['id']], 'evidence': proof, 'revision': 1,
                            'created_at': recorded_at, 'dependencies': [], 'source': source}
            invalidate(s, changed, rid)
            lease['status'] = 'applied'
        result['lease'] = lease['id']
    else:
        raise Rejected(f'unknown operation: {op}')
    return s, result
