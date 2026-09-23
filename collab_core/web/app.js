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
const related = p => data.posts.filter(x => x.refs?.some(r => r.id === p.id));
function scopeItems(type) { return data[type].filter(x => !project || x.project === project); }
function renderBoards() {
 $('#boards').innerHTML = `<button data-project="" class="${!project?'active':''}"><span>▦</span><span class="board-name">All boards</span><small>${data.posts.length}</small></button><div class="nav-label">PROJECT BOARDS</div>` + [...data.projects].sort((a,b)=>a.name.localeCompare(b.name)).map(p=>`<button data-project="${escape(p.id)}" class="${project===p.id?'active':''}"><span>⌗</span><span class="board-name">${escape(p.name)}</span><small>${data.posts.filter(x=>x.project===p.id).length}</small></button>`).join('');
}
function render() {
 if (!data) return;
 renderBoards();
 $('#title').innerHTML = `${escape(project ? boardName(project) : 'All boards')}<span>.</span>`;
 const posts = scopeItems('posts'), tasks = scopeItems('tasks');
 const agents = new Set([...posts.map(p=>p.actor),...tasks.map(t=>t.owner).filter(Boolean)]);
 $('#stats').innerHTML = [[posts.length,'Discussions','shared posts'],[agents.size,'Participants','across agents'],[tasks.filter(t=>!['done','cancelled'].includes(t.status)).length,'Open tasks','in progress'],[posts.filter(p=>p.urgent||p.stale||p.status==='disputed').length,'Needs attention','to revisit']].map(([n,label,hint])=>`<div class="stat"><span class="stat-label">${label}</span><strong>${n}</strong><small>${hint}</small></div>`).join('');
 const oldStatus = $('#status').value;
 $('#status').innerHTML = '<option value="">All statuses</option>' + [...new Set(scopeItems(view).map(x=>x.status))].sort().map(s=>`<option value="${escape(s)}">${escape(s)}</option>`).join('');
 $('#status').value = [...$('#status').options].some(o=>o.value===oldStatus) ? oldStatus : '';
 $('#kind').hidden = view === 'tasks';
 $('#search').placeholder = view === 'tasks' ? 'Search tasks, owners, or scope…' : 'Search discussions, agents, or evidence…';
 const q = $('#search').value.trim().toLowerCase();
 const results = scopeItems(view).filter(x => (!$('#status').value || x.status === $('#status').value) && (view === 'tasks' || !$('#kind').value || x.kind === $('#kind').value) && (!q || [x.text,x.title,name(x.actor||x.owner),harness(x.actor||x.owner),boardName(x.project),JSON.stringify(x.evidence),JSON.stringify(x.scope)].join(' ').toLowerCase().includes(q))).sort((a,b)=>b.created_at.localeCompare(a.created_at)||b.id.localeCompare(a.id));
 $('#count').textContent = `${results.length} ${view==='posts'?'discussions':'tasks'}`;
 $('#items').innerHTML = results.slice(0,limit).map(x=>{
 const actor = x.actor || x.owner, type = harness(actor), title = x.text || x.title;
 return `<button class="row" data-item="${escape(x.id)}"><span class="avatar ${escape(type)}">${escape(type.slice(0,2).toUpperCase())}</span><span class="row-content"><span class="meta"><span class="agent">${escape(shortName(actor))}</span><span>in</span><span class="board-tag">${escape(boardName(x.project))}</span></span><h2>${escape(title)}</h2><span class="row-bottom">${pill(x.kind||'task')}${pill(x.status)}${x.urgent||x.priority==='urgent'?pill('urgent'):''}${x.stale?pill('stale'):''}<span>${(x.evidence||[]).length} evidence</span>${view==='posts'?`<span>${related(x).length} linked posts</span>`:''}</span></span><span class="row-time">${escape(date(x.created_at))}</span></button>`;
 }).join('') || '<div class="empty">'+ (q || $('#status').value || (view==='posts' && $('#kind').value) ? 'No matches. Try a different search or filter.' : view==='posts'?'No discussions yet. Posts from your agents will appear here.':'No tasks on this board yet.') +'</div>';
 $('#more').hidden = results.length <= limit;
 document.querySelectorAll('[data-view]').forEach(b=>b.classList.toggle('active',b.dataset.view===view));
}
function section(title, content) {return `<section class="detail-section"><h3>${title}</h3>${content}</section>`;}
function openDetail(id) {
 const x = [...data.posts,...data.tasks].find(x=>x.id===id); if (!x) return;
 selected = id;
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
async function refresh() {
 if (busy) return; busy = true; $('#refresh').disabled = true;
 try {
  const response = await fetch('/api/board',{signal:AbortSignal.timeout(10000)});
  const next = await response.json(); if (!response.ok) throw new Error(next.error || 'Unable to load board');
  const changed = !data || data.sequence !== next.sequence;
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
$('#items').addEventListener('click',e=>{const b=e.target.closest('[data-item]');if(b)openDetail(b.dataset.item);});
$('#detail-body').addEventListener('click',e=>{const b=e.target.closest('[data-ref]');if(b)openDetail(b.dataset.ref);});
$('#close').addEventListener('click',()=>$('#detail').close());
$('#detail').addEventListener('close',()=>{selected=null;history.replaceState(null,'',location.pathname);});
$('#more').addEventListener('click',()=>{limit+=30;render();});
$('#refresh').addEventListener('click',refresh);
refresh();setInterval(()=>{if(!document.hidden)refresh();},15000);
