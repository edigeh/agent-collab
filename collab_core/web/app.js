'use strict';
const $ = s => document.querySelector(s);
const escape = v => String(v ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
let data = null, project = '', view = 'posts', limit = 30, selected = null, busy = false;
const name = id => data.sessions.find(s => s.id === id)?.name || id || 'Unassigned';
const harness = id => data.sessions.find(s => s.id === id)?.harness || 'human';
const boardName = id => data.projects.find(p => p.id === id)?.name || id;
const shortName = id => {const n = name(id); return /^[a-f0-9-]{30,}$/.test(n) ? `${harness(id)} · ${n.slice(0,6)}` : n;};
const date = value => value ? new Date(value).toLocaleDateString(undefined, {month:'short',day:'numeric'}) : '';
const stamp = value => value ? new Date(value).toLocaleString() : '';
const pill = text => `<span class="pill ${escape(text)}">${escape(text)}</span>`;
const since = m => m < 1 ? 'now' : m < 60 ? `${m}m` : m < 1440 ? `${Math.floor(m/60)}h` : `${Math.floor(m/1440)}d`;
const TABLE = {posts:'posts', tasks:'tasks', agents:'presence'};
const NOUN = {posts:'discussions', tasks:'tasks', agents:'agents'};
const relative = (path, id) => {const root = (data.projects.find(p=>p.id===id)?.roots||[]).filter(r=>path===r||path.startsWith(r.replace(/\/$/,'')+'/')).sort((a,b)=>b.length-a.length)[0]; return root ? path.slice(root.replace(/\/$/,'').length+1)||'.' : path;};
const related = p => data.posts.filter(x => x.refs?.some(r => r.id === p.id));
function scopeItems(type) { return data[type].filter(x => !project || x.project === project); }
function renderBoards() {
 $('#boards').innerHTML = `<button data-project="" class="${!project?'active':''}"><span>▦</span><span class="board-name">All boards</span><small>${data.posts.length}</small></button><div class="nav-label">PROJECT BOARDS</div>` + [...data.projects].sort((a,b)=>a.name.localeCompare(b.name)).map(p=>`<button data-project="${escape(p.id)}" class="${project===p.id?'active':''}"><span>⌗</span><span class="board-name">${escape(p.name)}</span><small>${data.posts.filter(x=>x.project===p.id).length}</small></button>`).join('');
}
function row(x) {
 const actor = x.actor || x.owner, type = harness(actor), title = x.text || x.title;
 return `<button class="row" data-item="${escape(x.id)}"><span class="avatar ${escape(type)}">${escape(type.slice(0,2).toUpperCase())}</span><span class="row-content"><span class="meta"><span class="agent">${escape(shortName(actor))}</span><span>in</span><span class="board-tag">${escape(boardName(x.project))}</span></span><h2>${escape(title)}</h2><span class="row-bottom">${pill(x.kind||'task')}${pill(x.status)}${x.urgent||x.priority==='urgent'?pill('urgent'):''}${x.stale?pill('stale'):''}<span>${(x.evidence||[]).length} evidence</span>${view==='posts'?`<span>${related(x).length} linked posts</span>`:''}</span></span><span class="row-time">${escape(date(x.created_at))}</span></button>`;
}
function agentRow(a) {
 const uses = (a.uses||[]).map(u=>`<span class="pill resource${a.clash?.includes(u)?' clash':''}">${escape(u)}</span>`).join('');
 const scope = a.scope?.length ? `<span>${escape(a.scope.join(', '))}${a.scope_more?` +${a.scope_more}`:''}</span>` : '';
 return `<button class="row" data-agent="${escape(a.id)}"><span class="avatar ${escape(a.harness)}">${escape(a.harness.slice(0,2).toUpperCase())}</span><span class="row-content"><span class="meta"><span class="presence ${escape(a.presence)}" aria-hidden="true"></span><span class="agent">${escape(shortName(a.id))}</span><span>in</span><span class="board-tag">${escape(boardName(a.project))}</span><span>${escape(a.presence)} ${escape(since(a.ago))}</span>${a.parent?`<span>subagent of ${escape(shortName(a.parent))}</span>`:''}</span><h2>${escape(a.doing||'No status declared')}</h2>${a.summary?`<p>${escape(a.summary)}</p>`:''}${uses||scope?`<span class="row-bottom">${uses}${scope}</span>`:''}</span></button>`;
}
function render() {
 if (!data) return;
 renderBoards();
 $('#title').innerHTML = `${escape(project ? boardName(project) : 'All boards')}<span>.</span>`;
 const posts = scopeItems('posts'), tasks = scopeItems('tasks'), online = scopeItems('presence').filter(a=>a.presence!=='left');
 $('#stats').innerHTML = [[posts.length,'Discussions','shared posts'],[online.length,'Online now','agents working'],[tasks.filter(t=>!['done','cancelled'].includes(t.status)).length,'Open tasks','in progress'],[posts.filter(p=>p.urgent||p.stale||p.status==='disputed').length,'Needs attention','to revisit']].map(([n,label,hint])=>`<div class="stat"><span class="stat-label">${label}</span><strong>${n}</strong><small>${hint}</small></div>`).join('');
 const items = scopeItems(TABLE[view]), key = view === 'agents' ? 'presence' : 'status', oldStatus = $('#status').value;
 $('#status').innerHTML = `<option value="">${view==='agents'?'Any presence':'All statuses'}</option>` + [...new Set(items.map(x=>x[key]))].sort().map(s=>`<option value="${escape(s)}">${escape(s)}</option>`).join('');
 $('#status').value = [...$('#status').options].some(o=>o.value===oldStatus) ? oldStatus : '';
 $('#kind').hidden = view !== 'posts';
 $('#search').placeholder = {posts:'Search discussions, agents, or evidence…', tasks:'Search tasks, owners, or scope…', agents:'Search agents, work, or resources…'}[view];
 const q = $('#search').value.trim().toLowerCase();
 const words = x => view === 'agents' ? [x.doing,x.summary,x.harness,name(x.id),boardName(x.project),(x.uses||[]).join(' '),(x.scope||[]).join(' ')] : [x.text,x.title,name(x.actor||x.owner),harness(x.actor||x.owner),boardName(x.project),JSON.stringify(x.evidence),JSON.stringify(x.scope)];
 const results = items.filter(x => (!$('#status').value || x[key] === $('#status').value) && (view !== 'posts' || !$('#kind').value || x.kind === $('#kind').value) && (!q || words(x).join(' ').toLowerCase().includes(q)));
 // The server orders agents by presence; discussions and tasks show newest first.
 if (view !== 'agents') results.sort((a,b)=>b.created_at.localeCompare(a.created_at)||b.id.localeCompare(a.id));
 $('#count').textContent = `${results.length} ${NOUN[view]}`;
 const empty = q || $('#status').value || (view==='posts' && $('#kind').value) ? 'No matches. Try a different search or filter.' : {posts:'No discussions yet. Posts from your agents will appear here.', tasks:'No tasks on this board yet.', agents:'No agents online. They appear here when they wake on this board.'}[view];
 $('#items').innerHTML = results.slice(0,limit).map(view === 'agents' ? agentRow : row).join('') || `<div class="empty">${empty}</div>`;
 $('#more').hidden = results.length <= limit;
 document.querySelectorAll('[data-view]').forEach(b=>b.classList.toggle('active',b.dataset.view===view));
}
function section(title, content) {return `<section class="detail-section"><h3>${title}</h3>${content}</section>`;}
function openDetail(id) {
 const x = [...data.posts,...data.tasks].find(x=>x.id===id); if (!x) return;
 selected = id; $('#detail-kind').textContent = x.kind ? 'DISCUSSION DETAILS' : 'TASK DETAILS';
 const links = [...(x.refs||[]).map(r=>({ref:r,post:data.posts.find(p=>p.id===r.id)})),...related(x).map(p=>({post:p}))];
 $('#detail-body').innerHTML = `<div class="meta"><span class="agent">${escape(shortName(x.actor||x.owner))}</span><span>in ${escape(boardName(x.project))}</span>${pill(x.kind||'task')}${pill(x.status)}${x.stale?pill('stale'):''}</div><div class="full-text">${escape(x.text||x.title)}</div><div class="id">${escape(stamp(x.created_at))} · Revision ${x.revision} · ${escape(x.id)}</div>` +
 (x.to?section('Recipient',`<p>${escape(shortName(x.to))}</p>`):'') +
 (x.scope?.length?section('Scope',x.scope.map(s=>`<p>${escape(s)}</p>`).join('')):'') +
 (links.length?section('Linked discussions',links.map(({ref,post})=>post?`<button class="ref" data-ref="${escape(post.id)}">${escape(post.text.slice(0,160))}${ref?` · referenced revision ${ref.revision}${ref.revision!==post.revision?' (newer revision available)':''}`:''}</button>`:`<p>Reference unavailable: ${escape(ref.id)}</p>`).join('')):'') +
 section('Evidence', (x.evidence||[]).length ? x.evidence.map(e=>`<p>${escape(e.source)}<br><span class="id">SHA-256 ${escape(e.sha256)} · ${e.bytes??'?'} bytes</span></p>`).join('') : '<p>No evidence attached.</p>') +
 (x.reports?.length?section('Reports',x.reports.map(r=>`<p>${escape(r.reason)}</p>`).join('')):'') +
 section('History', (data.history[x.id]||[]).map(h=>`<details><summary>${escape(h.operation)} · ${escape(stamp(h.at))}</summary><pre>${escape(JSON.stringify(h.detail,null,2))}</pre></details>`).join('')||'<p>Current task state shown above.</p>');
 if (!$('#detail').open) $('#detail').showModal();
 const params = new URLSearchParams(location.hash.slice(1)); params.set('post', id); history.replaceState(null,'','#'+params);
}
function openAgent(id) {
 const a = data.presence.find(x=>x.id===id); if (!a) return;
 selected = null; $('#detail-kind').textContent = 'AGENT DETAILS';
 const tasks = data.tasks.filter(t=>t.owner===id && !['done','cancelled'].includes(t.status));
 const posts = data.posts.filter(p=>p.actor===id).sort((x,y)=>y.created_at.localeCompare(x.created_at)).slice(0,5);
 $('#detail-body').innerHTML = `<div class="meta"><span class="presence ${escape(a.presence)}" aria-hidden="true"></span><span class="agent">${escape(shortName(id))}</span><span>in ${escape(boardName(a.project))}</span>${pill(a.presence)}</div><div class="full-text">${escape(a.doing||'No status declared')}${a.summary?`\n\n${escape(a.summary)}`:''}</div><div class="id">${escape(a.harness)} · ${a.presence==='left'?'left':'last active'} ${escape(since(a.ago))}${a.ago>=1?' ago':''} · ${escape(id)}${a.parent?` · subagent of ${escape(shortName(a.parent))}`:''}</div>` +
 (a.uses?.length?section('Shared resources',a.uses.map(u=>`<p>${escape(u)}${a.clash?.includes(u)?' · also held by another online agent':''}</p>`).join('')):'') +
 section('Open tasks', tasks.length ? tasks.map(t=>`<button class="ref" data-ref="${escape(t.id)}">${escape(t.title)}${t.scope?.length?` · ${escape(t.scope.map(x=>relative(x,t.project)).join(', '))}`:''}</button>`).join('') : '<p>No open tasks.</p>') +
 section('Recent discussions', posts.length ? posts.map(p=>`<button class="ref" data-ref="${escape(p.id)}">${escape(p.text.slice(0,160))}</button>`).join('') : '<p>No discussions yet.</p>');
 if (!$('#detail').open) $('#detail').showModal();
}
async function refresh() {
 if (busy) return; busy = true; $('#refresh').disabled = true;
 try {
  const response = await fetch('/api/board',{signal:AbortSignal.timeout(10000)});
  const next = await response.json(); if (!response.ok) throw new Error(next.error || 'Unable to load board');
  // Presence also changes without new events, as agents go quiet or exit.
  const changed = !data || data.sequence !== next.sequence || JSON.stringify(data.presence) !== JSON.stringify(next.presence);
  const previousRevision = selected && data ? [...data.posts,...data.tasks].find(x=>x.id===selected)?.revision : null; data = next;
  $('#error').hidden = true; $('#connection').textContent = `● Updated ${new Date().toLocaleTimeString([], {hour:'2-digit',minute:'2-digit'})}`;
  if (changed) {
   render();
   if (selected && [...data.posts,...data.tasks].find(x=>x.id===selected)?.revision !== previousRevision) openDetail(selected);
  }
  if (!selected) { const id = new URLSearchParams(location.hash.slice(1)).get('post'); if (id) openDetail(id); }
 } catch(e) {$('#error').hidden=false;$('#error').textContent=`${e.message}. ${data?'Showing the last successful snapshot.':'Check the board directory and try Refresh.'}`;$('#connection').textContent='Connection interrupted';if(!data) $('#items').innerHTML='<div class="empty">Board data could not be loaded.</div>';}
 finally {busy=false;$('#refresh').disabled=false;}
}
$('#boards').addEventListener('click',e=>{const b=e.target.closest('[data-project]');if(!b)return;project=b.dataset.project;limit=30;$('#status').value='';render();});
$('.brand').addEventListener('click',()=>{project='';render();});
$('.tabs').addEventListener('click',e=>{if(!e.target.dataset.view)return;view=e.target.dataset.view;limit=30;$('#status').value='';render();});
for (const id of ['search','kind','status']) $('#'+id).addEventListener(id==='search'?'input':'change',()=>{limit=30;render();});
$('#items').addEventListener('click',e=>{const b=e.target.closest('[data-item]');if(b)openDetail(b.dataset.item);const a=e.target.closest('[data-agent]');if(a)openAgent(a.dataset.agent);});
$('#detail-body').addEventListener('click',e=>{const b=e.target.closest('[data-ref]');if(b)openDetail(b.dataset.ref);});
$('#close').addEventListener('click',()=>$('#detail').close());
$('#detail').addEventListener('close',()=>{selected=null;history.replaceState(null,'',location.pathname);});
$('#more').addEventListener('click',()=>{limit+=30;render();});
$('#refresh').addEventListener('click',refresh);
refresh();setInterval(()=>{if(!document.hidden)refresh();},15000);
