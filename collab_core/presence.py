"""Who is online, decided at read time from recorded activity and a process check.

Agents reach the board only at checkpoints and the journal is append-only, so
presence never writes heartbeats. Declarations ride on session.register and
session.touch, which earlier releases already replay.
"""
from __future__ import annotations
from collections import Counter
import os
import subprocess

from .state import timestamp

ACTIVE = 900            # seconds since the last board command
FALLBACK_IDLE = 7200    # idle window when the harness process cannot be checked
LEFT_WINDOW = 8 * 3600  # how long a hand-off note stays visible
LIMIT = 20
SCOPE_LIMIT = 3
RANK = {'active': 0, 'idle': 1, 'left': 2}
# Shells and wrappers that sit between this CLI call and its harness process.
PASSTHROUGH = frozenset({'sh', 'bash', 'zsh', 'dash', 'ksh', 'mksh', 'fish', 'tcsh', 'csh', 'env',
                         'timeout', 'gtimeout', 'nice', 'nohup', 'time', 'xargs', 'sandbox-exec',
                         'script', 'caffeinate'})


def _ps(*args):
    try:
        return subprocess.run(['ps', *args], stdin=subprocess.DEVNULL, capture_output=True, text=True,
                              timeout=5, env={**os.environ, 'LC_ALL': 'C'}).stdout
    except (OSError, subprocess.SubprocessError):
        return None


def _started(fields):
    return ' '.join(fields[:5])  # lstart is five fields in the C locale


def _probe_rows(output):
    rows = {}
    for line in (output or '').splitlines():
        parts = line.split()
        if len(parts) >= 6 and parts[0].isdigit():
            rows[int(parts[0])] = _started(parts[1:6])
    return rows


def detect_host(harness, environ=None, ps=_ps):
    """Identify the long-lived harness process above this CLI call, or None."""
    environ = os.environ if environ is None else environ
    override = environ.get('COLLAB_HOST_PID')
    if override is not None:
        pid = int(override) if override.strip().isdigit() else 0
        started = _probe_rows(ps('-o', 'pid=,lstart=', '-p', str(pid))).get(pid) if pid > 1 else None
        return {'pid': pid, 'started': started} if started else None
    table = {}
    for line in (ps('-A', '-o', 'pid=,ppid=,lstart=,comm=') or '').splitlines():
        parts = line.split()
        if len(parts) >= 8 and parts[0].isdigit() and parts[1].isdigit():
            name = os.path.basename(' '.join(parts[7:])).lstrip('-')
            table[int(parts[0])] = (int(parts[1]), _started(parts[2:7]), name)
    chain, pid = [], os.getppid()
    while pid in table and pid > 1 and len(chain) < 32:
        chain.append(pid); pid = table[pid][0]
    chosen = (next((p for p in chain if table[p][2] == harness), None) or
              next((p for p in chain if table[p][2] not in PASSTHROUGH), None))
    return {'pid': chosen, 'started': table[chosen][1]} if chosen else None


def running(hosts, ps=_ps):
    """Return the (pid, start) pairs still running, or None when ps cannot answer."""
    own = os.getpid()
    pids = sorted({int(h['pid']) for h in hosts} | {own})
    rows = _probe_rows(ps('-o', 'pid=,lstart=', '-p', ','.join(map(str, pids))))
    return set(rows.items()) if own in rows else None


def classify(session, quiet, live, superseded):
    """Return active, idle or left, or None once the session is gone."""
    if 'left' in session:
        return 'left' if quiet < LEFT_WINDOW else None
    if quiet < ACTIVE:
        return 'active'
    if session.get('parent') or superseded:
        return None  # subagents never idle; a quiet session yields to a newer one in its process
    host = session.get('host')
    if host and live is not None:
        return 'idle' if (host['pid'], host['started']) in live else None
    return 'idle' if quiet < FALLBACK_IDLE else None


def _process(session):
    host = session.get('host')
    return (session['harness'], host['pid'], host['started']) if host and not session.get('parent') else None


def survey(state, now, probe=running):
    """Return (session, presence, quiet seconds) for visible sessions, most present first."""
    sessions = list(state['sessions'].values())
    quiet = {s['id']: (now - timestamp(s['seen_at'])).total_seconds() for s in sessions}
    newest = {}
    for s in sessions:
        if _process(s):
            newest[_process(s)] = max(newest.get(_process(s), 0.0), timestamp(s['host']['at']).timestamp())
    yielded = {s['id'] for s in sessions
               if _process(s) and newest[_process(s)] > timestamp(s['seen_at']).timestamp()}
    hosts = [s['host'] for s in sessions if _process(s) and 'left' not in s and
             s['id'] not in yielded and quiet[s['id']] >= ACTIVE]
    live = probe(hosts) if hosts else set()
    seen = []
    for s in sessions:
        presence = classify(s, quiet[s['id']], live, s['id'] in yielded)
        if presence:
            seen.append((s, presence, quiet[s['id']]))
    seen.sort(key=lambda row: (RANK[row[1]], row[2], row[0]['id']))
    return seen


def _relative(path, roots):
    for root in roots:
        base = root.rstrip('/')
        if path == base:
            return '.'
        if path.startswith(base + '/'):
            return path[len(base) + 1:]
    return path


def radar(state, session):
    """Paths of the session's open tasks, relative to its project roots."""
    roots = sorted(state['projects'].get(session['project'], {}).get('roots', []), key=len, reverse=True)
    paths = sorted({path for task in state['tasks'].values() if task['owner'] == session['id'] and
                    task['project'] == session['project'] and task['status'] not in ('done', 'cancelled')
                    for path in task['scope']})
    return [_relative(path, roots) for path in paths]


def entry(state, session, presence, quiet, mine=frozenset(), full=True, project=False):
    """One compact line of presence; empty fields are omitted to save context."""
    status = session.get('status') or {}
    item = {'id': session['id'], 'harness': session['harness'], 'presence': presence,
            'ago': max(0, int(quiet // 60))}
    if session.get('parent'):
        item['parent'] = session['parent']
    if project:
        item['project'] = state['projects'].get(session['project'], {}).get('name', session['project'])
    if status.get('doing'):
        item['doing'] = status['doing']
    summary = (session.get('left') or {}).get('summary') or status.get('summary')
    if full and summary:
        item['summary'] = summary
    if presence != 'left' and status.get('uses'):
        item['uses'] = status['uses']
        clash = sorted(set(status['uses']) & mine)
        if clash:
            item['clash'] = clash
    if full:
        paths = radar(state, session)
        if paths:
            item['scope'] = paths[:SCOPE_LIMIT]
        if len(paths) > SCOPE_LIMIT:
            item['scope_more'] = len(paths) - SCOPE_LIMIT
    return item


def listing(state, now, actor=None, project=None, probe=running, you=False):
    """Group presence for one reader: its project in full, other projects briefly, recent hand-offs."""
    me = state['sessions'].get(actor) or {}
    mine = frozenset((me.get('status') or {}).get('uses', [])) if 'left' not in me else frozenset()
    seen = survey(state, now, probe)
    online = {session['id'] for session, presence, _ in seen if presence != 'left'}
    peers, elsewhere, left, folded = [], [], [], Counter()
    for session, presence, quiet in seen:
        if session['id'] == actor:
            continue
        here = project is None or session['project'] == project
        if presence == 'left':
            if here:
                left.append(entry(state, session, presence, quiet, mine, project=project is None))
        elif here:
            peers.append(entry(state, session, presence, quiet, mine, project=project is None))
        elif session.get('parent') in online:
            folded[session['parent']] += 1  # other projects' subagents count toward their parent
        else:
            elsewhere.append(entry(state, session, presence, quiet, mine, full=False, project=True))
    for item in peers + elsewhere:
        if folded[item['id']]:
            item['subagents'] = folded[item['id']]
    result = {'peers': peers[:LIMIT], 'peer_count': len(peers)}
    if elsewhere:
        result['elsewhere'] = elsewhere[:LIMIT]
    if left:
        result['left'] = left[:LIMIT]
    if you and me:
        own = entry(state, me, 'active', 0)
        result['you'] = {key: value for key, value in own.items() if key not in ('presence', 'ago')}
    return result


def everyone(state, now, probe=running):
    """Every visible session for the local viewer, flagging resources two agents hold."""
    seen = survey(state, now, probe)
    holders = Counter(use for session, presence, _ in seen if presence != 'left'
                      for use in (session.get('status') or {}).get('uses', []))
    shared = frozenset(use for use, count in holders.items() if count > 1)
    return [{**entry(state, session, presence, quiet, shared), 'project': session['project']}
            for session, presence, quiet in seen]
