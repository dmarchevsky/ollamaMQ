// ollamaMQ web UI — the TUI's functions in a browser, plus client/API-key
// management. No framework, no build step: the server embeds this file.
//
// Every second the page polls `api/state` (plus logs/clients when the open
// page needs them) and re-renders only regions whose HTML actually changed.
// Inputs live in dialogs or static toolbars, so re-renders never eat a
// half-typed value.
'use strict';

const POLL_MS = 1000;
const MODEL_LIMIT = 12;      // models shown per expanded backend before "show all"
const OVERVIEW_LOGS = 8;
const LOG_LIMIT = 300;

const S = {
  page: 'overview',
  state: null,
  logs: [],
  clients: [],
  expanded: new Set(),       // backend URLs with their model list open
  showAll: new Set(),        // backend URLs showing every model
  revealed: new Set(),       // key ids shown unmasked
  selectedClient: null,
  online: true,
};

// ---------------------------------------------------------------- utilities

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

function esc(v) {
  return String(v ?? '').replace(/[&<>"']/g, c => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  })[c]);
}

const fmtNum = n => (n ?? 0).toLocaleString('en-US');
const hostOf = url => String(url).replace(/^https?:\/\//, '');

function fmtAgo(ms) {
  if (!ms) return 'never';
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 5) return 'just now';
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function fmtTime(ms) {
  const d = new Date(ms);
  const p = n => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

const fmtDate = ms => new Date(ms).toLocaleDateString('en-CA');

function maskKey(k) {
  if (k.length <= 12) return '•'.repeat(k.length);
  return `${k.slice(0, 4)}••••••••${k.slice(-4)}`;
}

/** Set innerHTML only when it changed, so idle polls don't touch the DOM. */
function setHTML(el, html) {
  if (!el) return;
  if (el._html !== html) {
    el.innerHTML = html;
    el._html = html;
    labelCells(el);
  }
}

/**
 * Copy each table's header text onto its cells as `data-label`. On phones
 * the CSS turns rows into stacked cards and shows these as the field names.
 */
function labelCells(root) {
  for (const table of root.querySelectorAll('table')) {
    const head = table.querySelector('tr:has(> th)');
    if (!head) continue;
    const names = [...head.children].map(th => th.textContent.trim());
    for (const tr of table.querySelectorAll('tr')) {
      if (tr === head) continue;
      // The first cell is the card's title and action cells stand alone,
      // so neither gets a label.
      [...tr.children].forEach((td, i) => {
        if (i > 0 && names[i] && !td.hasAttribute('colspan') && !td.classList.contains('actions')) {
          td.dataset.label = names[i];
        }
      });
    }
  }
}

const isPhone = () => matchMedia('(max-width: 720px)').matches;

function setNav(open) {
  document.body.classList.toggle('nav-open', open);
  $('#nav-toggle').setAttribute('aria-expanded', String(open));
}

function toast(text, ok = true) {
  const t = document.createElement('div');
  t.className = 'toast' + (ok ? '' : ' bad');
  t.textContent = text;
  $('#toasts').appendChild(t);
  setTimeout(() => t.remove(), 5000);
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Clipboard API needs a secure context; plain-http LAN access falls back.
    const ta = document.createElement('textarea');
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand('copy');
    ta.remove();
  }
  toast('Copied to clipboard');
}

// ---------------------------------------------------------------------- API

const DEMO = new URLSearchParams(location.search).has('demo');

async function api(method, path, body) {
  if (DEMO) return window.DemoApi.handle(method, path, body);
  const opts = { method, headers: { 'X-OllamaMQ-UI': '1' } };
  if (body !== undefined) {
    opts.headers['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  const res = await fetch('api/' + path, opts);
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = text; }
  if (!res.ok) {
    throw new Error((data && data.error) || text || `${res.status} ${res.statusText}`);
  }
  return data;
}

/** Run a mutating call, toast the outcome, refresh right away. */
async function act(method, path, body, okText) {
  try {
    const r = await api(method, path, body);
    if (okText) toast(typeof okText === 'function' ? okText(r) : okText);
    await poll();
    return r;
  } catch (e) {
    toast(e.message, false);
    return undefined;
  }
}

// ------------------------------------------------------------------ polling

async function poll() {
  try {
    const needLogs = S.page === 'logs' || S.page === 'overview';
    const needClients = S.page === 'clients' || !S.clients.length;
    const [state, logs, clients] = await Promise.all([
      api('GET', 'state'),
      needLogs && !$('#log-pause').checked ? api('GET', `logs?limit=${LOG_LIMIT}`) : null,
      needClients ? api('GET', 'clients') : null,
    ]);
    S.state = state;
    if (logs) S.logs = logs;
    if (clients) S.clients = clients;
    S.online = true;
  } catch (e) {
    S.online = false;
  }
  render();
}

// ------------------------------------------------------------------- render

function render() {
  $('#live').classList.toggle('down', !S.online);
  $('#live').lastChild.textContent = S.online ? 'live' : 'disconnected';
  const st = S.state;
  if (!st) return;
  $('#version').textContent = st.version ? 'v' + st.version : '';
  renderStats(st);
  renderNav(st);
  ({
    overview: renderOverview,
    backends: renderBackends,
    users: renderUsers,
    clients: renderClients,
    blocked: renderBlocked,
    logs: renderLogs,
  })[S.page]?.(st);
}

function renderStats(st) {
  const s = st.stats;
  setHTML($('#stats'), `
    <span class="stat q">Queue <b>${fmtNum(s.queued + s.processing)}</b></span>
    <span class="stat">Running <b>${fmtNum(s.processing)}</b></span>
    <span class="stat ok">Done <b>${fmtNum(s.done)}</b></span>
    <span class="stat bad">Drop <b>${fmtNum(s.dropped)}</b></span>
    <span class="stat bad">LogDrop <b>${fmtNum(s.log_dropped)}</b></span>
    <span class="stat vip">VIP <b>${esc(st.vip ?? '—')}</b></span>
    <span class="stat q">Boost <b>${esc(st.boost ?? '—')}</b></span>`);
}

function renderNav(st) {
  $('#nav-users').textContent = st.users.length || '';
  $('#nav-clients').textContent = S.clients.length || '';
  const nb = st.blocked.users.length + st.blocked.ips.length;
  $('#nav-blocked').textContent = nb || '';
}

// --- Overview

function renderOverview(st) {
  const s = st.stats;
  const online = st.backends.filter(b => b.online).length;
  setHTML($('#ov-cards'), `
    <div class="card q"><div class="k">Queued</div><div class="v">${fmtNum(s.queued)}</div><div class="sub">${fmtNum(s.processing)} running</div></div>
    <div class="card ok"><div class="k">Done</div><div class="v">${fmtNum(s.done)}</div><div class="sub">since start</div></div>
    <div class="card bad"><div class="k">Dropped</div><div class="v">${fmtNum(s.dropped)}</div><div class="sub">${fmtNum(s.log_dropped)} log drops</div></div>
    <div class="card"><div class="k">Backends</div><div class="v">${online}<span class="dim" style="font-size:16px"> / ${st.backends.length}</span></div><div class="sub">online</div></div>
    <div class="card vip"><div class="k">VIP</div><div class="v small">${esc(st.vip ?? '—')}</div><div class="sub">always first</div></div>
    <div class="card q"><div class="k">Boost</div><div class="v small">${esc(st.boost ?? '—')}</div><div class="sub">extra share</div></div>`);

  setHTML($('#ov-backends'), st.backends.length ? `<table>
    <tr><th>Backend</th><th>Type</th><th class="num">Active</th><th>Serving</th></tr>
    ${st.backends.map(b => `<tr>
      <td><span class="sdot ${b.online ? 'on' : 'off'}"></span><span class="bk-url">${esc(hostOf(b.url))}</span></td>
      <td>${typeBadge(b.api_type)}</td>
      <td class="num">${b.active}/${b.max}</td>
      <td class="trunc dim">${esc(b.current_model ?? (b.models_loaded[0] ?? '—'))}</td></tr>`).join('')}
    </table>` : '<div class="empty">No backends configured</div>');

  const active = st.users.filter(u => u.queued + u.processing > 0 || u.done > 0).slice(0, 6);
  setHTML($('#ov-users'), active.length ? `<table>
    <tr><th>User</th><th class="num">Queue</th><th class="num">Done</th><th class="num">Drop</th></tr>
    ${active.map(u => `<tr><td>${userCell(u, st)}</td>
      <td class="num">${queueCell(u)}</td><td class="num">${fmtNum(u.done)}</td><td class="num">${fmtNum(u.dropped)}</td></tr>`).join('')}
    </table>` : '<div class="empty">No traffic yet</div>');

  setHTML($('#ov-logs'), logTable(S.logs.slice(0, OVERVIEW_LOGS), false));
}

// --- Backends

function typeBadge(t) {
  const cls = { Ollama: 'ok', OpenAI: 'accent', Both: 'accent', 'System One': 'vip', '???': 'warn' }[t] || '';
  return `<span class="badge ${cls}">${esc(t)}</span>`;
}

function opCell(b) {
  if (b.active_op) {
    return `<span class="op st-warn"><span class="spin"></span>${esc(b.active_op.verb)} ${esc(b.active_op.model)} · ${b.active_op.elapsed_secs}s</span>`;
  }
  const r = b.results?.[0];
  if (r) {
    return r.ok
      ? `<span class="st-ok">✓ ${esc(r.verb.replace(/ing$/, 'ed'))} ${esc(r.model)}</span>`
      : `<span class="st-bad" title="${esc(r.error)}">✕ ${esc(r.verb)} ${esc(r.model)} failed</span>`;
  }
  return '';
}

function renderBackends(st) {
  if (!st.backends.length) {
    setHTML($('#backends'), '<div class="empty">No backends configured</div>');
    return;
  }
  const rows = st.backends.map(b => {
    const open = S.expanded.has(b.url);
    const pct = b.max ? Math.min(100, (b.active / b.max) * 100) : 0;
    const loaded = new Set(b.models_loaded);
    let html = `<tr class="bk-row ${open ? 'open' : ''}" data-action="toggle-backend" data-url="${esc(b.url)}">
      <td><span class="caret">${open ? '▾' : '▸'}</span><span class="sdot ${b.online ? 'on' : 'off'}"></span><span class="bk-url">${esc(hostOf(b.url))}</span></td>
      <td>${typeBadge(b.api_type)}</td>
      <td class="num">${b.active}/${b.max}<span class="meter"><i style="width:${pct}%"></i></span></td>
      <td class="num">${fmtNum(b.processed)}</td>
      <td>${b.loaded_known ? `<b>${loaded.size}</b> <span class="dim">loaded ·</span> ` : ''}${b.models_available.length} <span class="dim">available</span></td>
      <td>${opCell(b)}</td></tr>`;
    if (open) {
      // Loaded first, then alphabetical — the TUI's order.
      const models = [...b.models_available].sort((x, y) =>
        (loaded.has(y) - loaded.has(x)) || x.localeCompare(y));
      const all = S.showAll.has(b.url);
      const shown = all ? models : models.slice(0, MODEL_LIMIT);
      const busy = !!b.active_op;
      html += `<tr class="bk-detail"><td colspan="6">
        <div class="bk-detail-h">
          <h2>Models</h2>
          ${b.can_control ? '' : '<span class="badge">no load/unload API — models are always ready</span>'}
          <span class="sp"></span>
          ${b.can_control ? `<button class="btn sm" data-action="load-other" data-idx="${b.idx}" ${busy ? 'disabled' : ''}>Load model…</button>` : ''}
        </div>
        ${shown.length ? `<div class="models">${shown.map(m => {
          const isLoaded = loaded.has(m);
          const ctx = b.loaded_ctx?.[m];
          return `<div class="model ${isLoaded ? 'loaded' : ''}">
            <span class="name" title="${esc(m)}">${esc(m)}</span>
            ${isLoaded ? `<span class="badge ok">loaded${ctx ? ` · ${Math.round(ctx / 1024)}k ctx` : ''}</span>` : ''}
            ${b.can_control ? (isLoaded
              ? `<button class="btn sm" data-action="model" data-op="unload" data-idx="${b.idx}" data-model="${esc(m)}" ${busy ? 'disabled' : ''}>Unload</button>`
              : `<button class="btn sm" data-action="model" data-op="load" data-idx="${b.idx}" data-model="${esc(m)}" ${busy ? 'disabled' : ''}>Load</button>`) : ''}
          </div>`;
        }).join('')}</div>` : '<div class="dim">This backend lists no models.</div>'}
        ${models.length > MODEL_LIMIT ? `<p style="margin:10px 0 0"><button class="linkbtn" data-action="show-all" data-url="${esc(b.url)}">${all ? 'Show fewer' : `Show all ${models.length} models`}</button></p>` : ''}
      </td></tr>`;
    }
    return html;
  }).join('');
  setHTML($('#backends'), `<table>
    <tr><th>Backend</th><th>Type</th><th class="num">Active</th><th class="num">Done</th><th>Models</th><th>Control</th></tr>
    ${rows}</table>`);
}

// --- Users

function userCell(u, st) {
  let h = `<span class="mono">${esc(u.id)}</span>`;
  if (st.vip === u.id) h += ' <span class="badge vip">★ VIP</span>';
  if (st.boost === u.id) h += ' <span class="badge warn">⚡ Boost</span>';
  if (u.client) h += ' <span class="badge accent" title="Authenticated with a client key">⚿ client</span>';
  if (u.blocked) h += ' <span class="badge bad">blocked</span>';
  return h;
}

function queueCell(u) {
  if (!u.queued && !u.processing) return '<span class="dim">0</span>';
  return `<b class="st-warn">${u.queued + u.processing}</b>${u.processing ? ` <span class="dim">(${u.processing} running)</span>` : ''}`;
}

function renderUsers(st) {
  if (!st.users.length) {
    setHTML($('#users'), '<div class="empty">No users have sent requests yet</div>');
    return;
  }
  setHTML($('#users'), `<table>
    <tr><th>User</th><th>Last IP</th><th class="num">Queue</th><th class="num">Done</th><th class="num">Drop</th><th class="num">Actions</th></tr>
    ${st.users.map(u => {
      const id = esc(u.id);
      return `<tr>
      <td>${userCell(u, st)}</td>
      <td class="mono dim">${esc(u.ip ?? '—')}</td>
      <td class="num">${queueCell(u)}</td>
      <td class="num">${fmtNum(u.done)}</td>
      <td class="num">${fmtNum(u.dropped)}</td>
      <td class="actions"><span class="btn-group">
        <button class="btn sm ${st.vip === u.id ? 'on-vip' : ''}" data-action="user" data-op="vip" data-id="${id}" title="Toggle VIP (TUI: p)">★ VIP</button>
        <button class="btn sm ${st.boost === u.id ? 'on-boost' : ''}" data-action="user" data-op="boost" data-id="${id}" title="Toggle boost (TUI: b)">⚡ Boost</button>
        ${u.blocked
          ? `<button class="btn sm" data-action="user" data-op="unblock" data-id="${id}" title="Unblock user and IP (TUI: u)">Unblock</button>`
          : `<button class="btn sm danger" data-action="user" data-op="block" data-id="${id}" title="Block user (TUI: x)">Block</button>
             <button class="btn sm danger" data-action="user" data-op="block-ip" data-id="${id}" ${u.ip ? '' : 'disabled'} title="Block last IP (TUI: X)">Block IP</button>`}
      </span></td></tr>`;
    }).join('')}</table>`);
}

// --- Clients & keys

function clientStats(c) {
  const u = S.state?.users.find(x => x.id === c.name);
  return { queue: u ? u.queued + u.processing : 0, done: u?.done ?? 0, dropped: u?.dropped ?? 0 };
}

function lastUsed(c) {
  return Math.max(0, ...c.keys.map(k => k.last_used_ms || 0));
}

function renderClients() {
  const cs = S.clients;
  if (S.selectedClient && !cs.some(c => c.name === S.selectedClient)) S.selectedClient = null;
  if (!S.selectedClient && cs.length) S.selectedClient = cs[0].name;

  setHTML($('#client-list'), cs.length ? `<table>
    <tr><th>Client</th><th class="num">Keys</th><th class="num">Queue</th><th>Last used</th></tr>
    ${cs.map(c => {
      const s = clientStats(c);
      return `<tr class="clickable ${c.name === S.selectedClient ? 'selected' : ''}" data-action="select-client" data-name="${esc(c.name)}">
        <td><div class="client-name">${esc(c.name)} ${c.disabled ? '<span class="badge bad">disabled</span>' : ''}</div>
            <div class="client-sub trunc">${esc(c.note || '—')}</div></td>
        <td class="num">${c.keys.length}</td>
        <td class="num">${s.queue ? `<b class="st-warn">${s.queue}</b>` : '<span class="dim">0</span>'}</td>
        <td class="dim nowrap">${fmtAgo(lastUsed(c))}</td></tr>`;
    }).join('')}</table>`
    : '<div class="empty">No clients yet.<br>Create one to hand out API keys.</div>');

  const c = cs.find(x => x.name === S.selectedClient);
  if (!c) {
    setHTML($('#client-detail'), '<div class="empty">Select a client</div>');
    return;
  }
  const s = clientStats(c);
  const n = esc(c.name);
  setHTML($('#client-detail'), `
    <div class="detail-h">
      <div><h2>${n} ${c.disabled ? '<span class="badge bad">disabled</span>' : '<span class="badge ok">active</span>'}</h2>
        <div class="dim">${esc(c.note || 'No note')}</div></div>
      <span class="btn-group">
        <button class="btn sm" data-action="edit-client" data-name="${n}">Edit</button>
        <button class="btn sm" data-action="toggle-client" data-name="${n}">${c.disabled ? 'Enable' : 'Disable'}</button>
        <button class="btn sm danger" data-action="delete-client" data-name="${n}">Delete</button>
      </span>
    </div>
    <div class="detail-meta">
      <div><b>${s.queue}</b>in queue</div>
      <div><b>${fmtNum(s.done)}</b>requests done</div>
      <div><b>${fmtNum(s.dropped)}</b>dropped</div>
      <div><b>${fmtAgo(lastUsed(c))}</b>last request</div>
      <div><b>${fmtDate(c.created_at_ms)}</b>created</div>
    </div>
    <div class="section-h"><h2>API keys</h2><button class="btn sm primary" data-action="add-key" data-name="${n}">+ Add key</button></div>
    ${c.keys.length ? `<div class="tscroll"><table>
      <tr><th>Key</th><th>Label</th><th>Created</th><th>Last used</th><th class="num"></th></tr>
      ${c.keys.map(k => {
        const shown = S.revealed.has(k.id);
        return `<tr>
          <td class="keycell"><span class="keyval">${esc(shown ? k.key : maskKey(k.key))}</span>
            <button class="linkbtn" style="margin-left:6px" data-action="reveal" data-id="${esc(k.id)}">${shown ? 'hide' : 'show'}</button></td>
          <td>${esc(k.label || '—')}</td>
          <td class="dim nowrap">${fmtDate(k.created_at_ms)}</td>
          <td class="dim nowrap">${fmtAgo(k.last_used_ms)}</td>
          <td class="actions"><span class="btn-group">
            <button class="btn sm" data-action="copy-key" data-id="${esc(k.id)}">Copy</button>
            <button class="btn sm" data-action="reassign-key" data-id="${esc(k.id)}" ${cs.length < 2 ? 'disabled' : ''}>Reassign</button>
            <button class="btn sm danger" data-action="revoke-key" data-id="${esc(k.id)}">Revoke</button>
          </span></td></tr>`;
      }).join('')}</table></div>`
      : '<div class="empty">No keys — this client cannot authenticate yet.</div>'}
    <p class="field-hint" style="margin-top:14px !important">Clients send a key as <code>Authorization: Bearer &lt;key&gt;</code> or <code>X-Api-Key: &lt;key&gt;</code>. Unknown keys fall back to <code>X-User-ID</code>.</p>`);
}

function findKey(id) {
  for (const c of S.clients) {
    const k = c.keys.find(x => x.id === id);
    if (k) return { client: c, key: k };
  }
  return null;
}

// --- Blocked

function renderBlocked(st) {
  const tbl = (items, kind) => items.length ? `<table>
    ${[...items].sort().map(v => `<tr><td class="mono">${esc(v)}</td>
      <td class="actions"><button class="btn sm" data-action="unblock" data-kind="${kind}" data-value="${esc(v)}">Unblock</button></td></tr>`).join('')}
    </table>` : `<div class="empty">No blocked ${kind === 'ip' ? 'IP addresses' : 'users'}</div>`;
  setHTML($('#blocked-users'), tbl(st.blocked.users, 'user'));
  setHTML($('#blocked-ips'), tbl(st.blocked.ips, 'ip'));
}

// --- Logs

function infoClass(info) {
  if (/^2\d\d/.test(info) || /^ok\b/.test(info)) return 'st-ok';
  if (/^[45]\d\d/.test(info) || /drop|fail|error|timeout/i.test(info)) return 'st-bad';
  if (/queued|wait/i.test(info)) return 'st-warn';
  return '';
}

function logTable(logs, clickable = true) {
  if (!logs.length) return '<div class="empty">No matching log events</div>';
  return `<table class="logs">
    <tr><th>Time</th><th></th><th>User</th><th>Model</th><th>Backend</th><th>Info</th></tr>
    ${logs.map(l => `<tr class="${clickable ? 'clickable' : ''}" ${clickable ? `data-action="log-detail" data-seq="${l.seq}"` : ''}>
      <td class="time">${fmtTime(l.at_ms)}</td>
      <td><span class="dir ${esc(l.dir)}">${esc(l.dir)}</span></td>
      <td class="mono">${esc(l.user)}</td>
      <td class="trunc ${l.model ? '' : 'none'}">${esc(l.model ?? '—')}</td>
      <td class="mono dim ${l.backend ? '' : 'none'}">${esc(l.backend ? hostOf(l.backend) : '—')}</td>
      <td class="${infoClass(l.info)}">${esc(l.info)}${l.has_content ? ' <span class="dim">· ⌕</span>' : ''}</td></tr>`).join('')}
    </table>`;
}

function renderLogs() {
  const q = $('#log-filter').value.trim().toLowerCase();
  const dir = $('#log-dir').value;
  const logs = S.logs.filter(l =>
    (!dir || l.dir === dir) &&
    (!q || [l.user, l.model, l.backend, l.info].some(v => v && String(v).toLowerCase().includes(q))));
  setHTML($('#logs'), logTable(logs));
}

// ------------------------------------------------------------------ dialogs

/** Open a <dialog>, resolve with its FormData on "ok", null otherwise. */
function ask(dlg) {
  return new Promise(resolve => {
    const form = $('form', dlg);
    const onClose = () => {
      dlg.removeEventListener('close', onClose);
      resolve(dlg.returnValue === 'ok' ? new FormData(form) : null);
    };
    dlg.returnValue = '';
    dlg.addEventListener('close', onClose);
    dlg.showModal();
  });
}

async function confirmDlg(title, body, okLabel = 'Confirm') {
  const dlg = $('#dlg-confirm');
  $('[data-title]', dlg).textContent = title;
  $('[data-body]', dlg).innerHTML = body;
  $('[data-submit]', dlg).textContent = okLabel;
  return (await ask(dlg)) !== null;
}

function fillClientSelect(sel, selected, exclude) {
  sel.innerHTML = S.clients
    .filter(c => c.name !== exclude)
    .map(c => `<option ${c.name === selected ? 'selected' : ''}>${esc(c.name)}</option>`).join('');
}

async function clientDialog(existing) {
  const dlg = $('#dlg-client');
  const form = $('form', dlg);
  form.reset();
  $('[data-title]', dlg).textContent = existing ? `Edit ${existing.name}` : 'New client';
  $('[data-submit]', dlg).textContent = existing ? 'Save' : 'Create';
  $('[data-only="edit"]', dlg).style.display = existing ? '' : 'none';
  if (existing) {
    form.name.value = existing.name;
    form.note.value = existing.note || '';
    form.disabled.checked = existing.disabled;
  }
  const fd = await ask(dlg);
  if (!fd) return;
  const body = { name: fd.get('name').trim(), note: fd.get('note').trim() };
  if (existing) {
    body.disabled = !!fd.get('disabled');
    const r = await act('PATCH', `clients/${encodeURIComponent(existing.name)}`, body, 'Client saved');
    if (r) S.selectedClient = body.name;
  } else {
    const r = await act('POST', 'clients', body, `Client ${body.name} created`);
    if (r) {
      S.selectedClient = body.name;
      await poll();
    }
  }
}

async function addKeyDialog(clientName) {
  const dlg = $('#dlg-key');
  const form = $('form', dlg);
  form.reset();
  fillClientSelect(form.client, clientName);
  const syncMode = () => {
    const manual = form.mode.value === 'manual';
    $('[data-only="manual"]', dlg).style.display = manual ? '' : 'none';
    form.key.required = manual;
  };
  form.onchange = syncMode;
  syncMode();
  const fd = await ask(dlg);
  if (!fd) return;
  const client = fd.get('client');
  const body = { label: fd.get('label').trim() };
  if (fd.get('mode') === 'manual') body.key = fd.get('key').trim();
  const r = await act('POST', `clients/${encodeURIComponent(client)}/keys`, body);
  if (r) {
    S.selectedClient = client;
    S.clients = await api('GET', 'clients');
    render();
    const done = $('#dlg-key-created');
    $('[data-client]', done).textContent = client;
    $('[data-key]', done).textContent = r.key;
    done.showModal();
  }
}

async function loadDialog(idx) {
  const b = S.state.backends.find(x => x.idx === idx);
  if (!b) return;
  const dlg = $('#dlg-load');
  const form = $('form', dlg);
  form.reset();
  $('[data-backend]', dlg).textContent = `on ${hostOf(b.url)} — pick a listed model or type any name the backend can resolve.`;
  $('#dl-models').innerHTML = b.models_available.map(m => `<option value="${esc(m)}">`).join('');
  const fd = await ask(dlg);
  if (!fd) return;
  const model = fd.get('model').trim();
  await act('POST', `backends/${idx}/load`, { model }, r => `loading ${r?.model ?? model} started`);
}

// ------------------------------------------------------------------ actions

const actions = {
  'reload-config': () => act('POST', 'config/reload', {}, r => `model config reloaded: ${r.models} model(s), applying`),

  'toggle-backend': el => {
    const u = el.dataset.url;
    S.expanded.has(u) ? S.expanded.delete(u) : S.expanded.add(u);
    render();
  },
  'show-all': el => {
    const u = el.dataset.url;
    S.showAll.has(u) ? S.showAll.delete(u) : S.showAll.add(u);
    render();
  },
  'model': el => {
    const { op, idx, model } = el.dataset;
    return act('POST', `backends/${idx}/${op}`, { model }, r => `${op}ing ${r?.model ?? model} started`);
  },
  'load-other': el => loadDialog(Number(el.dataset.idx)),

  'user': async el => {
    const { op, id } = el.dataset;
    const labels = { vip: 'VIP toggled', boost: 'Boost toggled', block: `Blocked ${id}`, 'block-ip': `Blocked IP of ${id}`, unblock: `Unblocked ${id}` };
    if (op === 'block' || op === 'block-ip') {
      const what = op === 'block' ? `user <b>${esc(id)}</b>` : `the last IP of <b>${esc(id)}</b>`;
      if (!await confirmDlg('Block?', `New requests from ${what} will be rejected with 403. Queued requests are not cancelled.`, 'Block')) return;
    }
    return act('POST', `users/${encodeURIComponent(id)}/${op}`, {}, labels[op]);
  },
  'unblock': el => act('POST', 'blocked/unblock', { kind: el.dataset.kind, value: el.dataset.value }, `Unblocked ${el.dataset.value}`),

  'new-client': () => clientDialog(null),
  'select-client': el => {
    S.selectedClient = el.dataset.name;
    render();
    // The detail panel sits below the list on phones: bring it into view.
    if (isPhone()) $('#client-detail').scrollIntoView({ behavior: 'smooth', block: 'start' });
  },
  'nav-open': () => setNav(true),
  'nav-close': () => setNav(false),
  'edit-client': el => clientDialog(S.clients.find(c => c.name === el.dataset.name)),
  'toggle-client': el => {
    const c = S.clients.find(x => x.name === el.dataset.name);
    return act('PATCH', `clients/${encodeURIComponent(c.name)}`, { disabled: !c.disabled },
      c.disabled ? `${c.name} enabled` : `${c.name} disabled — its keys now get 403`);
  },
  'delete-client': async el => {
    const name = el.dataset.name;
    if (!await confirmDlg(`Delete ${esc(name)}?`, `The client and all its keys are removed. Requests with those keys fall back to <code>X-User-ID</code>.`, 'Delete')) return;
    return act('DELETE', `clients/${encodeURIComponent(name)}`, undefined, `Client ${name} deleted`);
  },
  'add-key': el => addKeyDialog(el.dataset.name),
  'reveal': el => {
    const id = el.dataset.id;
    S.revealed.has(id) ? S.revealed.delete(id) : S.revealed.add(id);
    render();
  },
  'copy-key': el => copyText(findKey(el.dataset.id).key.key),
  'copy-created': () => copyText($('#dlg-key-created [data-key]').textContent),
  'reassign-key': async el => {
    const f = findKey(el.dataset.id);
    const dlg = $('#dlg-reassign');
    const form = $('form', dlg);
    $('[data-key]', dlg).textContent = maskKey(f.key.key);
    fillClientSelect(form.client, null, f.client.name);
    const fd = await ask(dlg);
    if (!fd) return;
    const to = fd.get('client');
    const r = await act('PATCH', `keys/${encodeURIComponent(f.key.id)}`, { client: to }, `Key moved to ${to}`);
    if (r) { S.selectedClient = to; render(); }
  },
  'revoke-key': async el => {
    const f = findKey(el.dataset.id);
    if (!await confirmDlg('Revoke key?', `<code>${esc(maskKey(f.key.key))}</code> (${esc(f.key.label || 'no label')}) stops identifying <b>${esc(f.client.name)}</b> immediately.`, 'Revoke')) return;
    return act('DELETE', `keys/${encodeURIComponent(f.key.id)}`, undefined, 'Key revoked');
  },

  'log-detail': async el => {
    const seq = el.dataset.seq;
    let ev;
    try { ev = await api('GET', `logs/${seq}`); } catch (e) { return toast(e.message, false); }
    const dlg = $('#dlg-log');
    $('[data-title]', dlg).textContent = `Log detail — ${ev.dir}`;
    $('[data-meta]', dlg).innerHTML = [
      ['Time', new Date(ev.at_ms).toLocaleString()], ['User', ev.user], ['Model', ev.model ?? '—'],
      ['Backend', ev.backend ?? '—'], ['Info', ev.info],
    ].map(([k, v]) => `<dt>${k}</dt><dd>${esc(v)}</dd>`).join('');
    let content = ev.content ?? '';
    try { content = JSON.stringify(JSON.parse(content), null, 2); } catch { /* not JSON */ }
    $('[data-content]', dlg).textContent = content || 'No content captured for this event.';
    dlg.showModal();
  },
};

document.addEventListener('click', e => {
  const el = e.target.closest('[data-action]');
  if (!el || el.disabled) return;
  const fn = actions[el.dataset.action];
  if (fn) { e.preventDefault(); fn(el); }
});

// Following a nav link closes the drawer, even for the page already shown
// (which fires no hashchange).
$$('.nav nav a').forEach(a => a.addEventListener('click', () => setNav(false)));

document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && document.body.classList.contains('nav-open')) setNav(false);
});

// Click on a dialog's backdrop closes it.
$$('dialog').forEach(d => d.addEventListener('click', e => { if (e.target === d) d.close('cancel'); }));

['#log-filter', '#log-dir'].forEach(s => $(s).addEventListener('input', () => renderLogs()));

// ------------------------------------------------------------------- routing

function route() {
  const page = (location.hash.slice(1) || 'overview').split('?')[0];
  S.page = $(`#page-${page}`) ? page : 'overview';
  $$('.page').forEach(p => p.classList.toggle('active', p.id === `page-${S.page}`));
  $$('.nav nav a').forEach(a => a.classList.toggle('active', a.dataset.page === S.page));
  // The link's own text, without its icon and counter spans.
  $('#top-title').textContent = [...$(`.nav nav a[data-page="${S.page}"]`).childNodes]
    .filter(n => n.nodeType === Node.TEXT_NODE).map(n => n.textContent).join('').trim();
  setNav(false);
  poll();
}
window.addEventListener('hashchange', route);

function start() {
  route();
  setInterval(poll, POLL_MS);
}

if (DEMO) {
  const s = document.createElement('script');
  s.src = 'demo.js';
  s.onload = start;
  document.head.appendChild(s);
} else {
  start();
}
