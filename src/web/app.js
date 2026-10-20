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
  clientFilter: 'all',       // all | registered | unregistered
  modelPolicy: null,         // { mode, rules } from api/models
  modelShow: 'all',          // all | served | hidden
  modelQuery: "",
  overview: null,            // api/overview for the chosen window and filters
  filters: storedFilters(),  // the filter modal's selection (time, identity, lists)
  reqShow: storedReqShow(),  // List under the graph: active | all
  reqView: storedReqView(),  // List under the graph: list | details
  collapsed: storedCollapsed(),// conversation headers the operator folded shut
  colorOf: new Map(),        // client id -> colour slot 1..4
  settings: null,            // api/settings
  draft: new Map(),          // unsaved settings: name -> value (null = reset)
  formShown: false,          // settings form drawn since entering the page
  revealToken: null,         // a generated metrics token, shown until saved
  me: null,                  // api/me: user and where the password comes from
  online: true,
};

/** The filter modal's selection from the last visit (per browser); the old
 *  per-window and per-client storage keys fold into it. */
function storedFilters() {
  const d = { win: '15m', mode: 'client', clients: [], backends: [], models: [] };
  try {
    const j = JSON.parse(localStorage.getItem('omq.filters') || 'null');
    if (j && typeof j === 'object') {
      return { ...d, ...j, clients: [...(j.clients || [])], backends: [...(j.backends || [])], models: [...(j.models || [])] };
    }
    const w = localStorage.getItem('omq.window');
    const c = localStorage.getItem('omq.client');
    return { ...d, win: ['5m', '15m', '1h'].includes(w) ? w : '15m', clients: c ? [c] : [] };
  } catch { /* storage blocked */ }
  return d;
}

/** The Requests toggle from the last visit (per browser). */
function storedReqShow() {
  try {
    if (localStorage.getItem('omq.reqShow') === 'all') return 'all';
  } catch { /* storage blocked */ }
  return 'active';
}

/** The list view from the last visit; a folded Thread becomes Details. */
function storedReqView() {
  try {
    const v = localStorage.getItem('omq.reqView');
    if (v === 'details') return 'details';
    if (v === 'list') return 'list';
    if (localStorage.getItem('omq.reqGroup') === 'threads') return 'details';
  } catch { /* storage blocked */ }
  return 'list';
}

/** Session tags folded shut in Threads, per browser. */
function storedCollapsed() {
  try { return new Set(JSON.parse(localStorage.getItem('omq.collapsed') || '[]')); } catch { /* storage blocked */ }
  return new Set();
}

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

/** The backend with this URL or `host:port` label, from the last poll. */
const bkFind = urlOrLabel => S.state?.backends.find(b => hostOf(b.url) === hostOf(urlOrLabel));

/** A backend's display name: its configured name, else `host:port`. */
const bkName = urlOrLabel => bkFind(urlOrLabel)?.name || hostOf(urlOrLabel);

/**
 * A backend for display. Named: the name, with the address beside it
 * (`addr`) or on hover. Unnamed: the address.
 */
function bkCell(urlOrLabel, addr = false) {
  const label = hostOf(urlOrLabel);
  const name = bkFind(urlOrLabel)?.name;
  if (!name) return `<span class="bk-url">${esc(label)}</span>`;
  return `<span class="bk-name" title="${esc(label)}">${esc(name)}</span>${addr ? `<span class="bk-addr">${esc(label)}</span>` : ''}`;
}

function fmtAgo(ms) {
  if (!ms) return 'never';
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 5) return 'just now';
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

/** Seconds as `12.3s` or `4m 05s`. */
function fmtDur(s) {
  if (s < 60) return `${s.toFixed(1)}s`;
  const m = Math.floor(s / 60);
  return `${m}m ${String(Math.floor(s % 60)).padStart(2, '0')}s`;
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
  if (res.status === 401) {
    // Session expired or signed out elsewhere: sign in, then come back here.
    location.href = 'login?next=' + encodeURIComponent(location.pathname + location.hash);
    throw new Error('sign in required');
  }
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

/** The Overview request: window and filter scope (see the Filters modal). */
function overviewPath() {
  const f = S.filters;
  let p = `overview?window=${encodeURIComponent(f.win)}`;
  if (f.mode === 'key') p += '&mode=key';
  // An FNONE selection sends the empty value — the server reads "no entries"
  // as a scope that matches nothing.
  const q = (name, sel) => !sel.length ? '' : `&${name}=${sel.map(v => v === FNONE ? '' : encodeURIComponent(v)).join(',')}`;
  p += q('clients', f.clients) + q('backends', f.backends) + q('models', f.models);
  return p;
}

// ------------------------------------------------------------------ polling

async function poll() {
  try {
    const needLogs = S.page === 'logs';
    const needClients = S.page === 'clients' || !S.clients.length;
    const needModels = S.page === 'models' || !S.modelPolicy;
    const needOverview = S.page === 'overview';
    const needSettings = S.page === 'settings' || S.page === 'backends';
    const [state, logs, clients, models, overview, settings, me] = await Promise.all([
      api('GET', 'state'),
      needLogs && !$('#log-pause').checked ? api('GET', `logs?limit=${LOG_LIMIT}`) : null,
      needClients ? api('GET', 'clients') : null,
      needModels ? api('GET', 'models') : null,
      needOverview ? api('GET', overviewPath()) : null,
      needSettings ? api('GET', 'settings') : null,
      needSettings && !S.me ? api('GET', 'me') : null,
    ]);
    S.state = state;
    if (logs) S.logs = logs;
    if (clients) S.clients = clients;
    if (models) S.modelPolicy = models;
    if (overview) S.overview = overview;
    if (settings) S.settings = settings;
    if (me) S.me = me;
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
  const ver = st.version ? 'v' + st.version : '';
  $('#version').textContent = ver;
  // The sidebar brand is off-canvas on a phone, so Settings repeats the version.
  $('#set-version').textContent = ver ? `Running ollamaMQ ${ver}` : '';
  renderStats(st);
  renderNav(st);
  renderFiltersBtn();
  ({
    overview: renderOverview,
    backends: renderBackends,
    clients: renderClients,
    models: renderModels,
    blocked: renderBlocked,
    logs: renderLogs,
    settings: renderSettings,
  })[S.page]?.(st);
}

function renderStats(st) {
  const s = st.stats;
  // On the Overview, Done counts only the selected window (which the top-bar
  // selector picks); Queue and Running are live gauges either way. Drops are not
  // shown here: they are on the Clients table and in /metrics.
  const w = S.page === 'overview' ? S.overview?.status : null;
  // With a filter on, Queue/Running come from the scoped Overview too, so the
  // header tells the same story as the page under it.
  const scoped = w && filtersActive();
  setHTML($('#stats'), `
    <span class="stat q">Queue <b>${fmtNum(scoped ? w.queued : s.queued + s.processing)}</b></span>
    <span class="stat">Running <b>${fmtNum(scoped ? w.running : s.processing)}</b></span>
    <span class="stat ok"${w ? ' title="requests that completed in the selected window"' : ''}>Done <b>${fmtNum(w ? w.done : s.done)}</b></span>
    <span class="stat vip">VIP <b>${esc(st.vip ?? '—')}</b></span>
    <span class="stat q">Boost <b>${esc(st.boost ?? '—')}</b></span>`);
}

/** Every client the dashboard knows: the live roster plus registered names. */
function clientIds(st) {
  return [...new Set([...st.users.map(u => u.id), ...S.clients.map(c => c.name)])]
    .sort((a, b) => a.localeCompare(b));
}

// ---------------------------------------------------------------- filters

const FPRESETS = ['5m', '15m', '30m', '1h'];
const saveFilters = () => {
  try { localStorage.setItem('omq.filters', JSON.stringify(S.filters)); } catch { /* storage blocked */ }
};

/** `90m` -> `90 min`, `6h` -> `6 h` — for headers and empty-state labels. */
function winLabel(w) {
  const m = /^(\d+)(m|h)$/.exec(w);
  return m ? `${m[1]} ${m[2] === 'm' ? 'min' : 'h'}` : '15 min';
}

/** Any deviation from the defaults — the Filters button dots itself on it. */
const filtersActive = () => {
  const f = S.filters;
  return f.win !== '15m' || f.mode !== 'client'
    || f.clients.length > 0 || f.backends.length > 0 || f.models.length > 0;
};

/** Mirrors the server's client-scope rule: a bare name matches every key, a
 *  `name\tkey` entry only that pair. */
const clientSel = (c, k) => {
  const s = S.filters.clients;
  return !s.length || s.includes(c) || (!!k && k !== 'none' && s.includes(`${c}\t${k}`));
};
const modelSel = m => !S.filters.models.length || S.filters.models.includes(m);
const backendSel = b => !S.filters.backends.length || S.filters.backends.includes(hostOf(b || ''));
/** Display identity: `client`, or `client:key` in label mode — the server's
 *  own naming for lanes, client stats and capacity segments. */
const fmtWho = o => (S.filters.mode === 'key' && o.key && o.key !== 'none') ? `${o.client}:${o.key}` : o.client;

function openFilters() {
  drawFilters();
  $('#dlg-filters').showModal();
}

/** A checkbox row, checked when the selection is empty (= everything) or holds
 *  the value. `FNONE` is the explicit *nothing ticked* selection — it matches
 *  no real value, so the whole list renders unticked and the server receives
 *  an empty scope parameter that selects nothing. */
const FNONE = '\u0000none';
const fchk = (sel, val, label) => `<label class="check"><input type="checkbox" value="${esc(val)}"${!sel.length || sel.includes(val) ? ' checked' : ''}> ${label}</label>`;
const cntText = (sel, n) => sel.length === 1 && sel[0] === FNONE ? 'none' : !sel.length ? 'all' : `${sel.length} of ${n}`;

/** Refresh a list's `all / none / k of n` counter from its checkbox state. */
function updCnt(id) {
  const all = $$('input', $('#' + id));
  const on = all.filter(i => i.checked).length;
  const el = $(`#dlg-filters [data-cnt="${id}"]`);
  if (el) el.textContent = on === all.length ? 'all' : on ? `${on} of ${all.length}` : 'none';
}

/** List header: title, live count and the working all · none links. */
const flhead = (title, id, sel, n) => `<div class="fhead"><b>${title}</b><span class="hint" data-cnt="${id}">${cntText(sel, n)}</span>
  <span class="ftog"><button type="button" class="linkbtn" data-tog="all" data-l="${id}">all</button> · <button type="button" class="linkbtn" data-tog="none" data-l="${id}">none</button></span></div>`;

function drawFilters() {
  const f = S.filters, st = S.state, ov = S.overview || {};
  const cm = /^(\d+)(m|h)$/.exec(f.win);
  const preset = FPRESETS.includes(f.win);
  setHTML($('#f-time'), `<div class="fhead"><b>Time interval</b><span class="hint">or last N — history is kept for 24 h</span></div>
    <div class="frow"><span class="seg" role="radiogroup" aria-label="Time window">
      ${FPRESETS.map(w => `<label><input type="radio" name="fwin" value="${w}"${preset && f.win === w ? ' checked' : ''}><span>${winLabel(w)}</span></label>`).join('')}
    </span><input type="text" id="f-win-n" inputmode="numeric" placeholder="last" value="${preset ? '' : cm[1]}">
    <select id="f-win-u"><option value="m"${!preset && cm?.[2] === 'm' ? ' selected' : ''}>minutes</option><option value="h"${cm?.[2] === 'h' ? ' selected' : ''}>hours</option></select></div>`);
  setHTML($('#f-mode'), `<div class="fhead"><b>Client identity</b><span class="hint">label mode lists and shows client:key (API key label)</span></div>
    <div class="frow"><span class="seg" role="radiogroup" aria-label="Client identity">
      <label><input type="radio" name="fmode" value="client"${f.mode === 'client' ? ' checked' : ''}><span>Clients</span></label>
      <label><input type="radio" name="fmode" value="key"${f.mode === 'key' ? ' checked' : ''}><span>Clients + label</span></label></span></div>`);
  const cl = clientChoices(st, ov);
  // Universes come from the unfiltered state, never from the scoped overview:
  // a filtered list could shrink to its own selection, and "all boxes ticked"
  // would then compact back to "everything" and silently drop the filter.
  const bk = [...new Set(st ? st.backends.map(b => hostOf(b.url)) : (ov.backends || []).map(b => b.label))];
  const md = modelChoices(st);
  setHTML($('#f-clients'), `${flhead('Clients', 'f-cl-list', f.clients, cl.length)}
    <div class="flist" id="f-cl-list">${cl.map(c => fchk(f.clients, c.id, `${swatch(c.label)}${esc(c.label)}`)).join('')}</div>`);
  setHTML($('#f-backends'), `${flhead('Backends', 'f-bk-list', f.backends, bk.length)}
    <div class="flist" id="f-bk-list">${bk.map(l => fchk(f.backends, l, bkCell(l))).join('')}</div>`);
  setHTML($('#f-models'), `${flhead('Models', 'f-md-list', f.models, md.length)}
    <div class="flist" id="f-md-list">${md.map(m => fchk(f.models, m, esc(m))).join('')}</div>`);
}

/** Which clients (or client+key pairs) the modal offers in each mode. */
function clientChoices(st, ov) {
  if (S.filters.mode !== 'key') return clientIds(st).map(n => ({ id: n, label: n }));
  const pairs = new Map();
  const add = (c, k) => pairs.set(k ? `${c}\t${k}` : c, k ? `${c}:${k}` : c);
  for (const c of S.clients) { pairs.set(c.name, c.name); for (const k of c.keys || []) if (k.label) add(c.name, k.label); }
  for (const r of st?.runs || []) add(r.client, r.key && r.key !== 'none' ? r.key : null);
  for (const w of ov.queue || []) add(w.client, w.key && w.key !== 'none' ? w.key : null);
  for (const x of ov.recent || []) add(x.client, x.key && x.key !== 'none' ? x.key : null);
  return [...pairs].sort((a, b) => a[1].localeCompare(b[1])).map(([id, label]) => ({ id, label }));
}

/** Models the modal offers: everything the Models page considers served or
 *  partially served — hidden ones cannot be requested anyway. */
function modelChoices(st) {
  return (st ? modelCatalog(st) : [])
    .filter(m => m.status !== 'hidden')
    .map(m => m.name)
    .sort((a, b) => a.localeCompare(b));
}

/** Read the modal back into S.filters. A list with every box ticked stores
 *  empty — "everything" travels compactly, matching the server's scope rule;
 *  a list with no box ticked stores `FNONE`, which travels as an empty scope
 *  parameter and shows nothing. The custom interval wins over a preset. */
function collectFilters() {
  const f = S.filters;
  const win = $('#dlg-filters input[name="fwin"]:checked')?.value;
  const n = ($('#f-win-n').value || '').trim();
  const custom = /^\d+$/.test(n)
    ? Math.min(+n || 1, $('#f-win-u').value === 'h' ? 24 : 1440) + $('#f-win-u').value
    : null;
  f.win = custom || win || '15m';
  f.mode = $('#dlg-filters input[name="fmode"]:checked').value;
  const read = id => {
    const all = $$('#' + id + ' input');
    const on = [...all].filter(i => i.checked).map(i => i.value);
    return on.length === all.length ? [] : on.length ? on : [FNONE];
  };
  f.clients = read('f-cl-list');
  f.backends = read('f-bk-list');
  f.models = read('f-md-list');
}

/** Dots + tooltip summary on the Filters button. */
function renderFiltersBtn() {
  const on = filtersActive();
  $('#ov-filters').classList.toggle('on', on);
  $('#f-dot').hidden = !on;
  const f = S.filters;
  const bits = [winLabel(f.win), f.mode === 'key' ? 'client+label' : null,
    f.clients.length ? `${f.clients.length} clients` : null,
    f.backends.length ? `${f.backends.length} backends` : null,
    f.models.length ? `${f.models.length} models` : null];
  $('#ov-filters').title = on ? `Filters: ${bits.filter(Boolean).join(' · ')}` : 'Time, clients, backends and models';
}

function renderNav(st) {
  // Registered clients plus the unregistered ids currently tracked.
  const unregistered = st.users.filter(u => !u.registered).length;
  $('#nav-clients').textContent = (S.clients.length + unregistered) || '';
  const hiddenModels = modelCatalog(st).filter(m => m.status !== 'served').length;
  $('#nav-models').textContent = hiddenModels ? `${hiddenModels} hidden` : '';
  const nb = st.blocked.users.length + st.blocked.ips.length + st.blocked.clients.length;
  $('#nav-blocked').textContent = nb || '';
}

// --- Overview

/** Seconds as `350 ms`, `4.2s` or `3m 05s`; `—` when unknown. */
function fmtSecs(s) {
  if (s == null) return '<span class="dim">—</span>';
  return s < 1 ? `${Math.round(s * 1000)} ms` : fmtDur(s);
}

/** A rate with sensible precision; `—` when unknown. */
function fmtRate(v) {
  if (v == null) return '<span class="dim">—</span>';
  return v >= 100 ? fmtNum(Math.round(v)) : v.toFixed(1);
}

/** Token counts as 812, 96k, 1.2M. */
function fmtTok(n) {
  if (n < 1000) return String(n);
  if (n < 1e6) return `${n < 1e4 ? (n / 1e3).toFixed(1) : Math.round(n / 1e3)}k`;
  return `${(n / 1e6).toFixed(n < 1e7 ? 1 : 0)}M`;
}

/**
 * Colour every client seen in the window, 8 hues; past 8 the palette cycles.
 * Colours are kept while the client stays in the window (not just while it is
 * active) so finished history in the lanes and lists keeps its hue instead of
 * fading to grey the moment the request completes.
 */
const N_COLORS = 8;
function assignColors(ids) {
  for (const id of [...S.colorOf.keys()]) if (!ids.includes(id)) S.colorOf.delete(id);
  for (const id of ids) {
    if (S.colorOf.has(id)) continue;
    const used = new Set(S.colorOf.values());
    // shortcut: >8 active clients share hues; cycle on demand if boards ever need more
    S.colorOf.set(id, [1, 2, 3, 4, 5, 6, 7, 8].find(n => !used.has(n)) ?? S.colorOf.size % N_COLORS + 1);
  }
}
const colorClass = id => S.colorOf.has(id) ? `c${S.colorOf.get(id)}` : 'cother';
const swatch = id => `<i class="sw ${colorClass(id)}" aria-hidden="true"></i>`;

/** Inline sparkline: area + line, breaking at gaps (`null`), dot on the last point. */
function spark(values) {
  const W = 120, H = 28;
  const pts = values.map((v, i) => v == null ? null : [i, v]);
  const max = Math.max(0, ...values.filter(v => v != null));
  if (!pts.some(Boolean)) return '<div class="spark empty"></div>';
  const x = i => (i / Math.max(1, values.length - 1)) * W;
  const y = v => H - 2 - (max ? (v / max) * (H - 4) : 0);
  const runs = [];
  let cur = [];
  for (const p of pts) {
    if (p) cur.push(p); else if (cur.length) { runs.push(cur); cur = []; }
  }
  if (cur.length) runs.push(cur);
  const line = runs.map(r => 'M' + r.map(([i, v]) => `${x(i).toFixed(1)},${y(v).toFixed(1)}`).join('L')).join('');
  const area = runs.filter(r => r.length > 1).map(r =>
    `M${x(r[0][0]).toFixed(1)},${H}` + r.map(([i, v]) => `L${x(i).toFixed(1)},${y(v).toFixed(1)}`).join('') + `L${x(r.at(-1)[0]).toFixed(1)},${H}Z`).join('');
  const [li, lv] = runs.at(-1).at(-1);
  return `<div class="spark"><svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" aria-hidden="true">
    <path class="a" d="${area}"/><path class="l" d="${line}" vector-effect="non-scaling-stroke"/></svg>
    <i class="end" style="left:${(x(li) / W * 100).toFixed(1)}%;top:${(y(lv) / H * 100).toFixed(1)}%"></i></div>`;
}

function renderOverview(st) {
  const ov = S.overview;
  if (!ov) return;
  // The payload is already scoped by the filter query; /state is not, so the
  // live runs are scoped here — Capacity, Requests and their colour all read
  // this copy.
  const vis = { ...st, runs: (st.runs || []).filter(r => clientSel(r.client, r.key) && modelSel(r.model) && backendSel(r.backend)) };
  // Identity colours key on the display name, so client+label pairs get their
  // own hue (the server already names lanes, client rows and segments that way).
  assignColors([...new Set([...clientIds(st), ...ov.clients.map(c => c.id), ...vis.runs.map(fmtWho)])]);
  const s = ov.status;

  // Status line: the one sentence an operator reads first.
  const offline = s.backends_total - s.backends_online;
  const level = !s.backends_online ? 'bad'
    : (offline || (s.oldest_wait_secs ?? 0) > 30 || (s.error_pct ?? 0) >= 5) ? 'warn' : 'ok';
  const word = { ok: 'Healthy', warn: 'Needs attention', bad: 'No backend online' }[level];
  const parts = [
    `${s.backends_online} of ${s.backends_total} backends online`,
    `${s.free} of ${s.capacity} slots free`,
    s.queued ? `${s.queued} waiting (oldest ${fmtDur(s.oldest_wait_secs)})` : 'nothing waiting',
    ...s.loading.map(l => `${esc(l.verb)} ${esc(l.model)} on ${esc(bkName(l.backend))}`),
  ];
  if (s.errors) parts.push(`${s.errors} error${s.errors === 1 ? '' : 's'} in the last ${winLabel(S.filters.win)}`);
  setHTML($('#ov-status'), `<span class="lvl ${level}"><span class="sdot"></span>${word}</span>${parts.map(p => `<span>${p}</span>`).join('')}`);

  renderCapacity(vis, ov);
  renderTimeline(vis, ov);
  renderRequests(vis, ov);
  renderBackendStats(ov);
  renderClientStats(vis, ov);
}

/**
 * One row per backend. The capacity row carries solid client-coloured running
 * segments and visible free segments; widths are proportional to the request
 * size (estimated context tokens). When something waits, a slim waiting row
 * sits under the bar on the SAME token scale, so a backlog visibly passes the
 * end of the capacity. CSS clamps every segment to a readable minimum and
 * lets segments shrink, so one giant request cannot push the rest off screen.
 */
function renderCapacity(st, ov) {
  const runsAt = new Map();
  for (const r of st.runs || []) (runsAt.get(r.backend) || runsAt.set(r.backend, []).get(r.backend)).push(r);
  const waitAt = new Map(), shared = [];
  for (const w of ov.queue) {
    if (w.backends.length === 1) (waitAt.get(w.backends[0]) || waitAt.set(w.backends[0], []).get(w.backends[0])).push(w);
    else shared.push(w);
  }
  const label = new Map(ov.backends.map(b => [b.idx, b.name || b.label]));
  // An unseen-size request is drawn as the average seen one.
  const sizes = [...(st.runs || []), ...ov.queue].map(x => x.est_prompt_tokens || 0).filter(n => n > 0);
  const avg = sizes.length ? sizes.reduce((a, b) => a + b, 0) / sizes.length : 1;
  const pct = (n, scale) => (Math.max(0, n / scale) * 100).toFixed(2) + '%';
  const waitSeg = (w, style) => `<i class="seg q ${colorClass(w.client)}" style="${style}" title="${esc(
    `${w.client} · ${w.model}\n~${fmtTok(w.est_prompt_tokens)} tokens · waiting ${fmtDur(w.waited_secs)} · #${w.pos} in its client's queue`
    + (w.backends.length > 1 ? `\ncan run on ${w.backends.map(i => label.get(i)).join(', ')}` : ''))}"></i>`;
  const row = (nameCell, barHtml, noteHtml) =>
    `<div class="cap-row"><div class="cap-name">${nameCell}</div>${barHtml}<div class="cap-note">${noteHtml}</div></div>`;
  const rows = ov.backends.map(b => {
    const name = `<span class="sdot ${b.online ? 'on' : 'off'}"></span>${bkCell(b.label, true)}`;
    if (!b.online) return row(name, '<div class="q2-bar"><div class="cap-bar"></div></div>', '<span class="st-bad">offline</span>');
    if (b.loading) return row(name,
      `<div class="q2-bar"><div class="cap-bar"><i class="seg load" style="flex:1" title="${esc(`${b.loading.verb} ${b.loading.model}`)}"></i></div></div>`,
      `<span class="st-warn">${esc(b.loading.verb)} ${esc(b.loading.model)}</span>`);
    const runs = runsAt.get(b.label) || [];
    const waits = waitAt.get(b.idx) || [];
    // Token budget of the bar: running's estimated tokens plus the free share.
    // A backend reporting KV usage sizes free from it; otherwise each free
    // slot is worth the average request size seen on the board.
    const runTok = runs.reduce((a, r) => a + (r.est_prompt_tokens || avg), 0);
    let freeTok = Math.max(b.free, 0) * avg;
    if (b.kv_free_pct != null && b.kv_free_pct > 0 && b.kv_free_pct < 100 && runTok > 0)
      freeTok = runTok * b.kv_free_pct / (100 - b.kv_free_pct);
    const freeN = Math.max(b.free, 1);
    const capTok = Math.max(1, runTok + (b.free > 0 ? freeTok : 0));
    const waitTok = waits.reduce((a, w) => a + (w.est_prompt_tokens || avg), 0);
    const scale = Math.max(capTok, waitTok);
    let bar = runs.map(r => `<i class="seg run ${colorClass(r.client)}" style="flex:0 1 ${pct(r.est_prompt_tokens || avg, scale)}" title="${esc(
      `${r.client} · ${r.model}\n~${fmtTok(r.est_prompt_tokens)} tokens · ${r.state === 'generating' ? 'generating' : 'waiting for first token'} · ${fmtDur(r.elapsed_secs)}${r.tokens_per_sec ? ` · ${r.tokens_per_sec.toFixed(1)} tok/s` : ''}`)}"></i>`).join('');
    if (b.free > 0) {
      bar += `${runs.length ? '<span class="bar-gap"></span>' : ''}`
        + Array.from({ length: freeN }, () => `<i class="seg free" style="flex:0 1 ${pct(freeTok / freeN, scale)}" title="${b.kv_free_pct != null ? `free · ${b.kv_free_pct}% of the KV cache free` : 'free'}"></i>`).join('');
    }
    const q = waits.length ? `<div class="q-track">${waits.map(w => waitSeg(w, `flex:0 1 ${pct(w.est_prompt_tokens || avg, scale)}`)).join('')}</div>` : '';
    const kv = b.kv_free_pct != null ? `<span title="KV cache ${100 - b.kv_free_pct}% used — the free segments are sized by it">kv ${b.kv_free_pct}% free</span>` : '';
    return row(name, `<div class="q2-bar"><div class="cap-bar">${bar}</div>${q}</div>`, kv);
  }).join('');
  // Requests more than one backend could take (or none lists their model).
  const sharedRow = shared.length ? row(
    '<span class="cap-shared" title="Several backends can serve these models, or none lists them">any backend</span>',
    `<div class="q2-bar"><div class="q-track">${shared.map(w => waitSeg(w, 'flex:1 1 26px')).join('')}</div></div>`, '') : '';
  // Legend of the board: the clients on it right now, not the window history.
  const legend = [...new Set([...(st.runs || []).map(r => r.client), ...ov.queue.map(w => w.client)])]
    .map(id => `<span>${swatch(id)}${esc(id)}</span>`).join('')
    + '<span><i class="sw free"></i>free</span>'
    + '<span><i class="sw loading"></i>loading a model</span>'
    + '<span class="dim">waiting rows share the bar’s token scale</span>';
  setHTML($('#ov-capacity'), ov.backends.length
    ? `<div class="caps">${rows}${sharedRow}</div><div class="legend">${legend}</div>`
    : '<div class="empty">No backends configured</div>');
}

/**
 * One row per conversation, grouped under the backend it ran on. A request
 * paints one chip per stage — waiting, prefill, decode — as wide as that
 * stage ran, so a 40s decode reads as forty seconds next to a 1s prefill. A
 * stage too short to draw is floored to a few pixels rather than disappearing;
 * the exact numbers are in the chip tooltip and the Details lines below.
 * A backend with nothing in the window takes no row.
 */
const TL_ROWS = 12;          // sessions per backend before the oldest fold away
const STAGE = { w: 'waiting', p: 'prefill', g: 'decode', e: 'error' };
const TL_LEGEND = '<div class="tl-legend"><span><i class="sw w"></i>waiting</span>'
  + '<span><i class="sw p"></i>prefill</span><span><i class="sw g"></i>decode</span>'
  + '<span><i class="sw e"></i>error</span></div>';

function renderTimeline(st, ov) {
  const el = $('#ov-timeline');
  const now = ov.now_ms;
  const span = ov.window_secs * 1000;
  const pctOf = ms => Math.max(0, Math.min(100, (ms - (now - span)) / span * 100));
  pctOf.start = now - span;
  const { rows } = reqRows(st, ov, true);   // the graph is a window: it shows all of it
  if (!rows.length) {
    setHTML(el, `<div class="empty">Nothing in the last ${winLabel(S.filters.win)}</div>`);
    return;
  }
  for (const r of rows) r.plan = stagePlan(r, now);
  const laneOf = new Map((ov.lanes || []).map(l => [l.idx, l]));
  const groups = new Map();
  for (const r of rows) {
    const bk = rowBackend(r, ov);
    if (!groups.has(bk.k)) groups.set(bk.k, { ...bk, last: 0, reqs: 0, run: 0, wait: 0, sess: new Map() });
    const g = groups.get(bk.k);
    g.reqs++;
    g.last = Math.max(g.last, r.plan.end);
    if (r.kind === 'run') g.run++;
    if (r.kind === 'wait') g.wait++;
    if (!g.sess.has(r.key)) g.sess.set(r.key, []);
    g.sess.get(r.key).push(r);
  }
  const tick = f => `<span style="left:${(f * 100).toFixed(1)}%">−${Math.round(ov.window_secs / 60 * (1 - f))}m</span>`;
  const axis = `<div class="tl-axis">${[0, 1 / 3, 2 / 3].map(tick).join('')}<span style="left:100%">now</span></div>`;
  let hidden = 0;
  const body = [...groups.values()]
    .sort((a, b) => (a.pseudo - b.pseudo) || (b.last - a.last))
    .map(g => {
      // Same order as Details: sessions are ordered by when they **started**
      // (newest start first) — a stable position that ongoing traffic inside a
      // session never reshuffles. The cap keeps the freshest starts of the group.
      const sess = [...g.sess.entries()]
        .map(([key, rs]) => ({ key, rs, start: Math.min(...rs.map(x => x.plan.at)) }))
        .sort((a, b) => b.start - a.start);
      const shown = sess.slice(0, TL_ROWS);
      hidden += sess.length - shown.length;
      const lane = g.idx != null ? laneOf.get(g.idx) : null;
      // Nothing but the name stands on the canvas — what the backend is doing
      // now rides on the header's tooltip, next to the time it is read against.
      const tip = [`${shown.length} session${shown.length === 1 ? '' : 's'}`, `${g.reqs} req`]
        .concat(g.run ? [`${g.run} running`] : [], g.wait ? [`${g.wait} waiting`] : [],
          lane ? [`peak ${lane.peak_run}/${lane.max}`] : [],
          g.free != null ? [`${g.free} free`] : []).join(' · ');
      return `<div class="tl-grp" title="${esc(tip)}">`
        + `<span class="sdot ${g.online ? 'on' : 'off'}"></span><span class="bk-name">${esc(g.name)}</span>`
        + (g.online ? '' : '<span class="off-tag">offline</span>') + '</div>'
        + shown.map(s => tlRow(s.rs, pctOf, span)).join('');
    }).join('');
  const foot = hidden ? `+${hidden} older session${hidden === 1 ? '' : 's'} hidden` : '';
  setHTML(el, `<div class="tl">${axis}${body}${TL_LEGEND}${foot ? `<div class="tl-foot">${foot}</div>` : ''}</div>`);
  wireTimeline();
}

/** One session's row: its stages laid along the track at their own times. */
function tlRow(rows, pctOf, span) {
  const head = rows[0];
  // Each stage is placed in the track by itself: left = when it began, width =
  // how long it ran. A stage that runs off either edge is clipped there and the
  // row gets a `‹` so its left edge is not read as the request's beginning.
  let cut = '';
  const segs = rows.flatMap(r => {
    if (r.plan.at < pctOf.start) cut = '<b class="cut"></b>';
    return r.plan.events.map(e => `<i class="sg ${e.k}" style="left:${pctOf(e.at).toFixed(3)}%;`
      + `width:${Math.max(0, e.dur * 1000 / span * 100).toFixed(3)}%" title="${esc(stageTip(e))}"></i>`);
  }).join('');
  return `<div class="tl-row" data-key="${esc(head.key)}" style="--c:var(--${cvar(colorClass(fmtWho(head.o)))})">`
    + `<div class="tl-lab">${swatch(fmtWho(head.o))}<b>${esc(fmtWho(head.o))}</b>${head.s ? ` #${esc(head.s)}` : ''}</div>`
    + `<div class="tl-track">${cut}${segs}<span class="now"></span></div></div>`;
}

/** A chip's own tooltip: when the stage began and what it is worth. */
const stageTip = e => `${fmtTime(e.at)} · ${STAGE[e.k]} ${stripTags(e.line)}`;

/**
 * Which group a row belongs to: its backend, or a named bucket when it has
 * none. Live rows carry a backend index, finished ones a label — both resolve
 * to the same handle so a backend is one group, not two.
 */
function rowBackend(r, ov) {
  // pseudo buckets (no backend to hang the row on) sort after the real ones.
  const of = (k, idx, name, online, free, pseudo = idx != null ? 1 : 2) => ({ k, idx, name, online, free, pseudo });
  if (r.kind === 'wait') {
    const i = r.o.backends.length === 1 ? r.o.backends[0] : null;
    const b = i != null ? ov.backends[i] : null;
    return b ? of('b' + i, i, b.name || b.label, b.online, b.free)
      : of('any', null, 'no free backend', true, null, 3);
  }
  if (!r.o.backend) return of('none', null, 'never dispatched', false, null, 2);
  const i = ov.backends.findIndex(b => b.label === r.o.backend || b.name === r.o.backend);
  return i >= 0 ? of('b' + i, i, ov.backends[i].name || ov.backends[i].label, ov.backends[i].online, ov.backends[i].free)
    : of('n' + r.o.backend, null, r.o.backend, true, null);
}

/**
 * Clicking a graph row focuses that conversation in whichever view is open:
 * a folded Details block unfolds, a conversation hidden behind "Active" is
 * revealed by flipping Show to "All", then the target scrolls into view and
 * flashes. Both list rows and Details headers carry its data-key.
 */
function wireTimeline() {
  const target = k => $$('#ov-now [data-key]').find(e => e.dataset.key === k);
  $$('#ov-timeline .tl-row').forEach(row => {
    row.onclick = () => {
      const key = row.dataset.key;
      let redraw = false;
      if (!target(key) && S.reqShow !== 'all') {
        S.reqShow = 'all';
        try { localStorage.setItem('omq.reqShow', S.reqShow); } catch { /* storage blocked */ }
        $('#ov-req-show input[value="all"]').checked = true;
        redraw = true;
      }
      if (S.collapsed.delete(key)) {
        try { localStorage.setItem('omq.collapsed', JSON.stringify([...S.collapsed])); } catch { /* storage blocked */ }
        redraw = true;
      }
      if (redraw) render();
      const el = target(key);
      if (!el) return;
      el.scrollIntoView({ behavior: 'smooth', block: 'center' });
      el.classList.add('flash');
      setTimeout(() => el.classList.remove('flash'), 1200);
    };
  });
}

/**
 * The stage events of one request: when each began and what it is worth.
 * Every duration in the payload is measured from *dispatch* (`ttft_secs`,
 * `gen_secs`, and `elapsed_secs` for a live run); the queue wait happened
 * before dispatch. So a finished request is rebuilt backwards from the moment
 * it left the proxy — arrival = finish − waited − prefill − generation — and a
 * run grows forward from `now − elapsed`, which is its dispatch. `ttft_secs` is
 * dispatch to first token, i.e. the prefill itself, wherever `prefill_secs` is
 * absent. A stage too short to matter is left out rather than drawn as 0.0s.
 */
function stagePlan(row, now) {
  const o = row.o;
  // dur = how long the stage ran, so its chip can be as long as it was.
  const ev = (k, at, dur, line, live) => ({ k, at, dur, line, live: !!live });
  const done = (events, at, end) => ({
    events, at, end,
    // The tooltip opens with when the request arrived, so a cluster can be
    // checked against the Details lines without leaving the graph.
    tip: `${fmtTime(at)} · ` + events.map(e => `${STAGE[e.k]} ${stripTags(e.line)}`).join(' · '),
  });
  if (row.kind === 'wait') {
    const at = now - o.waited_secs * 1000;
    return done([ev('w', at, o.waited_secs, `${fmtDur(o.waited_secs)} · prompt ${fmtTok(o.est_prompt_tokens)} tok · queue #${o.pos}`, true)], at, now);
  }
  if (row.kind === 'run') {
    const dispatch = now - o.elapsed_secs * 1000;
    const w = o.waited_secs || 0;
    const out = [];
    if (w >= .05) out.push(ev('w', dispatch - w * 1000, w, `${fmtDur(w)} · prompt ${fmtTok(o.est_prompt_tokens)} tok`));
    if (o.ttft_secs == null) {
      out.push(ev('p', dispatch, o.elapsed_secs, `≈${fmtTok(o.est_prompt_tokens)} tok · ${fmtDur(o.elapsed_secs)} prefilling`, true));
    } else {
      if (o.ttft_secs >= .05) out.push(ev('p', dispatch, o.ttft_secs, `${fmtTok(o.est_prompt_tokens)} tok · ${fmtSec1(o.ttft_secs)}`));
      out.push(ev('g', dispatch + o.ttft_secs * 1000, Math.max(0, o.elapsed_secs - o.ttft_secs),
        `${fmtNum(o.tokens)} tok · ${fmtDur(Math.max(0, o.elapsed_secs - o.ttft_secs))} · `
        + `${o.tokens_per_sec == null ? '…' : o.tokens_per_sec.toFixed(1)} t/s<span class="more"> · <b>now</b></span>`, true));
    }
    return done(out, dispatch - w * 1000, now);
  }
  const finish = row.t;
  const w = o.waited_secs || 0;
  const gen = o.gen_secs || 0;
  const pre = o.prefill_secs ?? o.ttft_secs ?? 0;
  const dispatch = finish - (pre + gen) * 1000;
  const at = dispatch - w * 1000;
  if (o.error) {
    const out = [];
    if (w >= .05) out.push(ev('w', at, w, `${fmtDur(w)} · prompt ${fmtTok(o.prompt_tokens)} tok`));
    out.push(ev('e', finish, 0, `${outcomePhrase(o)}${o.backend ? '' : '<span class="more"> · never dispatched</span>'}`));
    return done(out, at, finish);
  }
  const out = [];
  // A request that left without an error and without serving cleanly (the
  // client hung up, the queue refused it) still has to say so here.
  if (o.outcome && o.outcome !== 'ok') out.push(ev('e', finish, 0, outcomePhrase(o)));
  if (w >= .05) out.push(ev('w', at, w, `${fmtDur(w)} · prompt ${fmtTok(o.prompt_tokens)} tok`));
  if (pre >= .05) {
    out.push(ev('p', dispatch, pre, `${fmtTok(o.prompt_tokens)} tok · ${fmtSec1(pre)}`
      + (o.cache_pct != null ? ` · cache ${o.cache_pct}%` : '')
      // The rate divides the tokens actually evaluated (cache-excluded), like
      // the Backends table — dividing the whole prompt by the short time of a
      // cached prefill invented impossible speeds.
      + (o.prefill_secs && o.prefill_tokens ? `<span class="more"> · ${fmtRate(o.prefill_tokens / o.prefill_secs)}</span>` : '')));
  }
  if (gen > 0) {
    out.push(ev('g', finish - gen * 1000, gen,
      `${fmtTok(o.completion_tokens)} tok · ${fmtSec1(gen)} · ${(o.gen_tps ?? 0).toFixed(1)} t/s`));
  }
  return done(out, at, finish);
}

const stripTags = s => s.replace(/<[^>]*>/g, '');

/** `outcome` as an operator reads it, with the HTTP status when there is one. */
const outcomePhrase = o => (o.status && o.status >= 500 ? `HTTP ${o.status} · ` : '')
  + String(o.outcome || 'failed').replace(/_/g, ' ');

/**
 * Details: one collapsible block per conversation, its stage events merged onto
 * one timeline and printed newest first, each line with its own timestamp and
 * numbers. A gap over 30s between two requests becomes its own `idle` line —
 * that quiet stretch is what a conversation looks like.
 */
function renderDetails(rows) {
  const now = S.overview ? S.overview.now_ms : Date.now();
  const out = [];
  for (const unit of threadUnits(rows)) {
    const head = unit[0];
    const c = cvar(colorClass(fmtWho(head.o)));
    const shut = S.collapsed.has(head.key);
    const asc = [...unit].sort((a, b) => a.plan.at - b.plan.at);
    const last = Math.max(...unit.map(x => x.plan.end));
    const bk = head.o.backend ? ` → ${esc(bkName(head.o.backend))}` : '';
    // The conversation's current state: a running request decides (prefill
    // until the first token, decode after), else a queued one; an idle
    // conversation carries no chip at all.
    const run = unit.find(x => x.kind === 'run'), wait = unit.find(x => x.kind === 'wait');
    const chip = run
      ? (run.o.state === 'generating' ? '<span class="badge dec">decode</span>'
        : '<span class="badge warn">prefill</span>')
      : wait ? `<span class="badge q" title="Position in this client's own queue">waiting #${wait.o.pos}</span>`
      : '';
    // The summary is three right-aligned columns so the numbers line up
    // across headers; on phones the columns drop their min-widths and wrap.
    const sum = `<span class="sum"><i>${unit.length} req</i><i>${fmtTok(unit.reduce((a, x) => a + rowTokens(x), 0))} tok</i>`
      + `<i>${fmtTime(asc[0].plan.at)}→${last >= now - 1500 ? 'now' : fmtTime(last)}</i></span>`;
    out.push(`<button class="det-h" style="--c:var(--${c})" data-key="${esc(head.key)}" aria-expanded="${!shut}"
      title="${shut ? 'expand' : 'collapse'} this conversation">
      <span class="chev">${shut ? '▸' : '▾'}</span><span class="who">${esc(fmtWho(head.o))}</span>
      ${head.s ? `<span class="who">#${esc(head.s)}</span>` : ''}${chip}
      <span class="sub" title="${esc(head.o.model)}${bk}">${esc(head.o.model)}${bk}</span>
      ${sum}</button>`);
    if (shut) continue;
    const lines = [];
    for (let i = 0; i < asc.length; i++) {
      if (i > 0) {
        const gap = (asc[i].plan.at - asc[i - 1].plan.end) / 1000;
        // Just before the newer request it separates the two from.
        if (gap > 30) {
          lines.push({ t: asc[i].plan.at - 1, html: `<div class="det-row dim"><span class="when">${fmtTime(asc[i - 1].plan.end)}</span>`
            + '<span class="st i">idle</span><span class="val">' + fmtDur(gap) + '</span></div>' });
        }
      }
      for (const e of asc[i].plan.events) {
        // Live stage lines (the running request's now-still-unfinished ones)
        // wear the client-colour wash the List gives live rows.
        lines.push({ t: e.at, html: `<div class="det-row${e.live ? ' live' : ''}"><span class="when">${fmtTime(e.at)}</span>`
          + `<span class="st ${e.k}">${STAGE[e.k]}</span><span class="val">${e.line}</span></div>` });
      }
    }
    lines.sort((a, b) => b.t - a.t);
    out.push(`<div class="det-body" style="--c:var(--${c})">${lines.map(l => l.html).join('')}</div>`);
  }
  return out.join('') || '<div class="empty">Nothing running or waiting</div>';
}

/** Seconds, always one decimal — the Requests table's time format. */
const fmtSec1 = s => s == null ? null : s.toFixed(1) + 's';

/** Put a count inside a segmented control's label (`Active 3`, `All 12`). */
function segCount(sel, i, n) {
  const span = $$(sel + ' label span')[i];
  if (span) span.innerHTML = `${span.textContent.replace(/\s*[\d.,]+\s*$/, '')} <b>${fmtNum(n)}</b>`;
}

/** `--c1`…`--c8` name of a client's hue slot. */
const cvar = cls => cls === 'cother' ? 'c-other' : cls;

/** Generated tokens of a row, however the row is shaped. */
const rowTokens = row => row.kind === 'done' ? row.o.completion_tokens : row.kind === 'run' ? row.o.tokens : 0;

/** Running and waiting rows share one treatment in both views. */
const liveCls = row => row.kind === 'run' || row.kind === 'wait' ? ' is-live' : '';

/** Conversations ordered by when each **started** — newest start first, so a
 *  session never moves while it runs; a request without a session stands alone. */
function threadUnits(rows) {
  const groups = new Map();
  for (const r of rows) {
    if (!groups.has(r.key)) groups.set(r.key, []);
    groups.get(r.key).push(r);
  }
  return [...groups.values()].sort((a, b) => Math.min(...b.map(x => x.plan.at)) - Math.min(...a.map(x => x.plan.at)));
}

const saveCollapsed = () => {
  try { localStorage.setItem('omq.collapsed', JSON.stringify([...S.collapsed])); } catch { /* storage blocked */ }
};

/** Fold handlers for the Details conversation headers (state lives in
 *  S.collapsed, because the list is rebuilt on every poll). */
function wireFold() {
  $$('#ov-now .det-h').forEach(b => {
    b.onclick = () => {
      const k = b.dataset.key;
      if (S.collapsed.has(k)) S.collapsed.delete(k); else S.collapsed.add(k);
      saveCollapsed();
      render();
    };
  });
}


/**
 * What a phone row says about one request: its status, its own numbers, one
 * timing line. Field names live in the tooltips — the desktop table has columns
 * for them — and Gen time is left out, since tokens and tok/s give it away.
 */
function phoneParts(row, ov) {
  const o = row.o;
  if (row.kind === 'run') {
    return {
      head: o.state === 'generating' ? '<span class="badge dec">generating</span>'
        : `<span class="badge warn" title="Waiting for the first token (prefill, or a response that is not streamed)">prefill ${fmtDur(o.elapsed_secs)}</span>`,
      nums: `${fmtNum(o.tokens)} tok · ${o.tokens_per_sec == null ? '…' : o.tokens_per_sec.toFixed(1)} t/s`,
      timing: `<span title="prompt size, estimated">pre ≈${fmtTok(o.est_prompt_tokens)}</span>`
        + (o.ttft_secs != null ? ` · <span title="time to the first token">ttft ${fmtSec1(o.ttft_secs)}</span>` : ''),
      bk: bkName(o.backend),
    };
  }
  if (row.kind === 'wait') {
    return {
      head: `<span class="badge q" title="Position in this client's own queue">waiting #${o.pos}</span>`,
      nums: `<span class="dim">≈${fmtTok(o.est_prompt_tokens)} tok</span>`,
      timing: `<span title="waiting for a free slot">wait ${fmtDur(o.waited_secs)}</span>`,
      bk: o.backends.length === 1 ? bkName(ov.backends[o.backends[0]]?.label) : 'any',
    };
  }
  if (o.error) {
    return {
      head: `<span class="badge bad" title="${esc(o.outcome)}">${esc(o.status && o.status >= 500 ? `HTTP ${o.status}` : o.outcome.replace(/_/g, ' '))}</span>`,
      nums: `<span class="st-bad">${esc(o.outcome.replace(/_/g, ' '))}</span>`,
      timing: o.backend ? '' : '<span class="dim">never dispatched</span>',
      bk: o.backend ? bkName(o.backend) : '—',
    };
  }
  const timing = [
    o.waited_secs > 0 ? `<span title="waiting for a free slot">wait ${fmtDur(o.waited_secs)}</span>` : '',
    o.prefill_tokens != null || o.prefill_secs != null
      ? `<span title="prompt size · prefill time">pre ${fmtTok(o.prompt_tokens)}${o.prefill_secs != null ? ` · ${fmtSec1(o.prefill_secs)}` : ''}</span>` : '',
    o.cache_pct != null ? `<span title="prefix-cache hit">cache ${o.cache_pct}%</span>` : '',
    o.ttft_secs != null ? `<span title="time to the first token">ttft ${fmtSec1(o.ttft_secs)}</span>` : '',
  ].filter(Boolean).join(' · ');
  return {
    head: o.outcome === 'ok' ? `<span class="t">${fmtTime(row.t)}</span>`
      : `<span class="badge">${esc(o.outcome.replace(/_/g, ' '))}</span>`,
    nums: `${fmtTok(o.completion_tokens)} tok · ${o.gen_tps == null ? '—' : o.gen_tps.toFixed(1)} t/s`,
    timing,
    bk: o.backend ? bkName(o.backend) : '—',
  };
}

/** Phone card: the first line names the conversation *and* where it ran — model
 *  and backend belong together, so the model gives way (ellipsis) before the
 *  backend does. Then the counts, then the timing. */
function cardPhone(row, ov) {
  const p = phoneParts(row, ov);
  return `<div class="rq${liveCls(row)}" data-key="${esc(row.key)}" style="--c:var(--${cvar(colorClass(fmtWho(row.o)))})"><div class="rq-1">${p.head}${swatch(fmtWho(row.o))}`
    + `<span class="who">${esc(fmtWho(row.o))}</span>`
    + `<span class="mb"><span class="mo">${esc(row.o.model)}</span><span class="ar">→</span>${esc(p.bk)}</span></div>`
    + `<div class="rq-2">${p.nums}</div>`
    + (p.timing ? `<div class="rq-m">${p.timing}</div>` : '') + '</div>';
}

/** Phone layout of the List: cards instead of the reflowed table. */
function requestsPhone(rows, ov) {
  if (!rows.length) return '<div class="empty">Nothing running or waiting</div>';
  return rows.map(r => cardPhone(r, ov)).join('');
}


/**
 * The rows the Overview draws: live runs, the queue and — when Show is All — the
 * finished log. `key` names the conversation: the session tag when the request
 * carries one, otherwise the request itself, so a session-less client's requests
 * each get a line of their own. /state is not scoped by ?client=, so its rows are
 * scoped here.
 */
function reqRows(st, ov, all) {
  const now = ov.now_ms;
  const keyOf = (o, fallback) => o.session ? `${fmtWho(o)}#${o.session}` : `${fmtWho(o)}·${o.model}·${fallback}`;
  const rows = [
    ...(st.runs || [])
      .map(r => ({ kind: 'run', t: now, s: r.session || null, o: r, key: keyOf(r, 'live') })),
    ...ov.queue.map(w => ({ kind: 'wait', t: now - w.waited_secs * 1000, s: w.session || null, o: w, key: keyOf(w, 'queue') })),
  ];
  const activeN = rows.length;
  if (all) for (const x of ov.recent) rows.push({ kind: 'done', t: x.at_ms, s: x.session || null, o: x, key: keyOf(x, x.at_ms) });
  return { rows, activeN };
}

/**
 * The List under the timeline. Show: Active = running + waiting; All also lists
 * recently finished (live rows carry a yellow tint, no section divider).
 * View: List = newest first (cards on a
 * phone, a table here); Details = one block per conversation with a line per
 * stage event. Prefill is size · seconds (≈ while estimated), Gen time the
 * generation seconds (for running: since the first token), Cache the
 * prefix-cache share of finished prompts.
 */
function renderRequests(st, ov) {
  const dash = '<span class="dim">—</span>';
  const now = ov.now_ms;
  const { rows, activeN } = reqRows(st, ov, S.reqShow === 'all');
  // The counts that used to be a sentence above the table sit in the toggle —
  // the All count is the whole window even while Active is selected.
  segCount('#ov-req-show', 0, activeN);
  segCount('#ov-req-show', 1, activeN + ov.recent.length);

  const el = $('#ov-now');
  $('#ov-fold').hidden = S.reqView !== 'details';
  if (S.reqView === 'details') {
    for (const r of rows) r.plan = stagePlan(r, now);
    setHTML(el, renderDetails(rows));
    wireFold();
    return;
  }
  const pill = r => r.state === 'generating'
    ? '<span class="badge dec">generating</span>'
    : `<span class="badge warn" title="Waiting for the first token (prefill, or a response that is not streamed)">prefill ${fmtDur(r.elapsed_secs)}</span>`;
  // A phone gets cards, not a reflowed table: three tight lines per request.
  if (isPhone()) { setHTML(el, requestsPhone(rows, ov)); return; }

  const backendCell = row => row.kind === 'run' ? `<td>${bkCell(row.o.backend)}</td>`
    : row.kind === 'wait'
      ? `<td class="dim ph-hide">${row.o.backends.length === 1 ? bkName(ov.backends[row.o.backends[0]]?.label) : 'any'}</td>`
      : row.o.backend ? `<td>${bkCell(row.o.backend)}</td>` : `<td>${dash}</td>`;

  const tds = row => {
    const head = `<td>${row.kind === 'run' ? pill(row.o)
      : row.kind === 'wait'
        ? `<span class="badge q" title="Position in this client's own queue">waiting #${row.o.pos}</span>`
        : row.o.error ? `<span class="badge bad" title="${esc(row.o.outcome)}">${esc(row.o.status && row.o.status >= 500 ? `HTTP ${row.o.status}` : row.o.outcome.replace(/_/g, ' '))}</span>`
          : row.o.outcome === 'ok' ? `<span class="dim mono" title="finished">${fmtTime(row.t)}</span>`
          : `<span class="badge">${esc(row.o.outcome.replace(/_/g, ' '))}</span>`}</td>
      <td>${swatch(fmtWho(row.o))}<span class="mono">${esc(fmtWho(row.o))}</span></td>
      <td class="trunc">${esc(row.o.model)}</td>`;
    if (row.kind === 'run') {
      const r = row.o;
      return head + backendCell(row) + `
      <td class="num">${fmtDur(r.waited_secs ?? 0)}</td>
      <td class="num"><span class="mono">≈${fmtTok(r.est_prompt_tokens)} · ${r.ttft_secs != null ? fmtSec1(r.ttft_secs) : '…'}</span></td>
      <td class="num">${dash}</td>
      <td class="num">${firstToken(r)}</td>
      <td class="num">${fmtNum(r.tokens)}</td>
      <td class="num">${r.ttft_secs != null && r.state === 'generating' ? `<span class="mono">${fmtSec1(Math.max(0, r.elapsed_secs - r.ttft_secs))}</span>` : dash}</td>
      <td class="num">${r.tokens_per_sec == null ? dash : r.tokens_per_sec.toFixed(1)}</td>`;
    }
    if (row.kind === 'wait') {
      const w = row.o;
      return head + backendCell(row) + `
      <td class="num st-warn">${fmtDur(w.waited_secs)}</td>
      <td class="num"><span class="mono">≈${fmtTok(w.est_prompt_tokens)} · —</span></td>
      <td class="num">${dash}</td><td class="num">${dash}</td><td class="num">${dash}</td><td class="num">${dash}</td><td class="num">${dash}</td>`;
    }
    const x = row.o;
    const pre = x.prefill_secs == null && x.prefill_tokens == null ? dash
      : `<span class="mono">${fmtTok(x.prompt_tokens)} · ${x.prefill_secs == null ? '—' : fmtSec1(x.prefill_secs)}</span>`;
    return head + backendCell(row) + `
      <td class="num">${x.waited_secs == null ? dash : fmtDur(x.waited_secs)}</td>
      <td class="num">${pre}</td>
      <td class="num">${x.cache_pct == null ? dash : x.cache_pct + '%'}</td>
      <td class="num">${x.ttft_secs == null ? dash : `<span class="mono">${fmtSec1(x.ttft_secs)}</span>`}</td>
      <td class="num">${fmtTok(x.completion_tokens)}</td>
      <td class="num">${x.gen_secs == null ? dash : `<span class="mono">${fmtSec1(x.gen_secs)}</span>`}</td>
      <td class="num">${x.gen_tps == null ? dash : x.gen_tps.toFixed(1)}</td>`;
  };

  const body = rows.map(r => `<tr${liveCls(r)} data-key="${esc(r.key)}" style="--c:var(--${cvar(colorClass(fmtWho(r.o)))})">${tds(r)}</tr>`).join('');

  const head = `<tr><th>State</th><th>Client</th><th>Model</th><th>Backend</th><th class="num">Waited</th><th class="num">Prefill</th><th class="num">Cache</th><th class="num">First token</th><th class="num">Tokens</th><th class="num">Gen time</th><th class="num">tok/s</th></tr>`;
  setHTML(el, rows.length ? `<table>${head}${body}</table>` : '<div class="empty">Nothing running or waiting</div>');
}


function renderBackendStats(ov) {
  setHTML($('#ov-backends'), ov.backends.length ? `<table>
    <tr><th>Backend</th><th class="num">Slots</th><th class="num">Requests</th><th class="num">Errors</th><th class="num">First token p50 / p95</th><th class="num">Gen tok/s</th><th class="num">Prefill tok/s</th><th>Output tok/s</th></tr>
    ${ov.backends.map(b => `<tr>
      <td><span class="sdot ${b.online ? 'on' : 'off'}"></span>${bkCell(b.label, true)} ${typeBadge(b.api_type)}</td>
      <td class="num">${b.online ? `${b.running}/${b.max}` : '<span class="st-bad">offline</span>'}</td>
      <td class="num">${fmtNum(b.requests)}</td>
      <td class="num ${b.errors ? 'st-bad' : 'dim'}">${fmtNum(b.errors)}</td>
      <td class="num">${fmtSecs(b.ttft_p50)} / ${fmtSecs(b.ttft_p95)}</td>
      <td class="num">${fmtRate(b.gen_p50)}</td>
      <td class="num">${fmtRate(b.prefill_tps)}</td>
      <td class="sparkcell">${b.online ? spark(b.spark) : '<span class="dim">—</span>'}</td></tr>`).join('')}
    </table>` : '<div class="empty">No backends configured</div>');
}

function renderClientStats(st, ov) {
  const now = c => [c.running ? `${c.running} running` : '', c.queued ? `<span class="st-warn">${c.queued} waiting</span>` : '']
    .filter(Boolean).join(' · ') || '<span class="dim">idle</span>';
  setHTML($('#ov-clients'), ov.clients.length ? `<table>
    <tr><th>Client</th><th>Now</th><th class="num">Requests</th><th>Share of slot time</th><th class="num">Wait p95</th><th class="num">First token p50</th><th class="num">Gen tok/s</th><th class="num">Tokens in / out</th></tr>
    ${ov.clients.map(c => `<tr>
      <td>${swatch(c.id)}<span class="mono">${esc(c.id)}</span>${idBadges(c.id, { registered: c.registered }, st, true)}</td>
      <td>${now(c)}</td>
      <td class="num">${fmtNum(c.requests)}${c.errors ? ` <span class="st-bad" title="errors">(${c.errors} err)</span>` : ''}</td>
      <td><span class="share"><i class="${colorClass(c.id)}" style="width:${(c.share * 100).toFixed(1)}%"></i></span> ${Math.round(c.share * 100)}%</td>
      <td class="num">${fmtSecs(c.wait_p95)}</td>
      <td class="num">${fmtSecs(c.ttft_p50)}</td>
      <td class="num">${fmtRate(c.gen_p50)}</td>
      <td class="num">${fmtTok(c.prompt_tokens)} / ${fmtTok(c.completion_tokens)}</td></tr>`).join('')}
    </table>` : `<div class="empty">No requests in the last ${winLabel(S.filters.win)}</div>`);
}

/** Time to first token; until then the run is waiting for the backend or
 *  evaluating the prompt. A non-streamed response never shows a first token. */
function firstToken(r) {
  if (r.ttft_secs != null) return fmtDur(r.ttft_secs);
  if (r.ttfb_secs == null) return '<span class="st-warn">waiting…</span>';
  return r.tokens ? '<span class="dim">—</span>' : '<span class="st-warn">prefill…</span>';
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
  // Backend config editing lives here, not in Settings; the settings view
  // (fetched on this page by poll) carries name/token/kind/removed per URL.
  const locked = S.settings?.backends_locked;
  $('#bk-add-backend').hidden = !!locked;
  const note = locked ? `<p class="note flush-note">The backends are set by <code>${esc(locked)}</code>, which wins over the web UI, so they cannot be changed here.</p>` : '';
  if (!st.backends.length) {
    setHTML($('#backends'), note + '<div class="empty">No backends configured</div>');
    return;
  }
  const cfg = new Map((S.settings?.backends || []).map(b => [b.url, b]));
  const editable = !!S.settings && !locked;
  const cols = editable ? 7 : 6;
  const rows = st.backends.map(b => {
    const c = cfg.get(b.url);
    const open = S.expanded.has(b.url);
    const pct = b.max ? Math.min(100, (b.active / b.max) * 100) : 0;
    const loaded = new Set(b.models_loaded);
    const hiddenHere = new Set(b.models_hidden);
    let html = `<tr class="bk-row ${open ? 'open' : ''}" data-action="toggle-backend" data-url="${esc(b.url)}">
      <td><span class="caret">${open ? '▾' : '▸'}</span><span class="sdot ${b.online ? 'on' : 'off'}"></span>${bkCell(b.url, true)}${b.disabled ? ' <span class="badge">disabled</span>' : ''}${c?.removed ? ' <span class="badge">removed — gone after a restart</span>' : ''}</td>
      <td>${typeBadge(b.api_type)}</td>
      <td class="num" title="${b.max_detected ? `${b.max} slots reported by the backend` : `${b.max} = settings.max_concurrent_per_backend`}">${b.active}/${b.max}<span class="meter"><i style="width:${pct}%"></i></span></td>
      <td class="num">${fmtNum(b.processed)}</td>
      <td>${b.loaded_known ? `<b>${loaded.size}</b> <span class="dim">loaded ·</span> ` : ''}${b.models_available.length} <span class="dim">available</span></td>
      <td>${opCell(b)}</td>${bkActions(b, c, editable)}</tr>`;
    if (open) {
      // Loaded first, then alphabetical — the TUI's order.
      const models = [...b.models_available].sort((x, y) =>
        (loaded.has(y) - loaded.has(x)) || x.localeCompare(y));
      const all = S.showAll.has(b.url);
      const shown = all ? models : models.slice(0, MODEL_LIMIT);
      const busy = !!b.active_op;
      html += `<tr class="bk-detail"><td colspan="${cols}">
        <div class="bk-detail-h">
          <h2>Models</h2>
          ${b.can_control ? '' : '<span class="badge">no load/unload API — models are always ready</span>'}
          <span class="sp"></span>
          ${b.can_control ? `<button class="btn sm" data-action="load-other" data-idx="${b.idx}" ${busy ? 'disabled' : ''}>Load model…</button>` : ''}
        </div>
        ${shown.length ? `<div class="models">${shown.map(m => {
          const isLoaded = loaded.has(m);
          const isHidden = hiddenHere.has(m);
          const ctx = b.loaded_ctx?.[m];
          return `<div class="model ${isLoaded ? 'loaded' : ''} ${isHidden ? 'model-hidden' : ''}">
            <span class="name" title="${esc(m)}${isHidden ? ' — hidden by the model policy' : ''}">${esc(m)}</span>
            ${isHidden ? '<span class="badge bad">hidden</span>' : ''}
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
  setHTML($('#backends'), `${note}<table>
    <tr><th>Backend</th><th>Type</th><th class="num">Active</th><th class="num">Done</th><th>Models</th><th>Control</th>${editable ? '<th></th>' : ''}</tr>
    ${rows}</table>`);
}

/** Config controls for a row: Edit / Disable·Enable / Remove, or Add back for a removed one. */
function bkActions(b, c, editable) {
  if (!editable) return '';
  if (c?.removed) return `<td class="actions"><button class="btn sm" data-action="backend-restore" data-url="${esc(b.url)}" data-name="${esc(c.name ?? '')}">Add back</button></td>`;
  return `<td class="actions"><span class="btn-group">
    <button class="btn sm" data-action="backend-edit" data-url="${esc(b.url)}">Edit</button>
    <button class="btn sm" data-action="backend-toggle" data-url="${esc(b.url)}">${b.disabled ? 'Enable' : 'Disable'}</button>
    <button class="btn sm danger" data-action="backend-remove" data-url="${esc(b.url)}">Remove</button></span></td>`;
}

// --- Clients: registered clients and unregistered queue ids in one list

/** Names a client can be registered under (mirrors the server's rule). */
const NAME_RE = /^[A-Za-z0-9._-]{1,64}$/;

/** Badges after an id: kind, priority, and block state. */
function idBadges(id, { registered, disabled, blocked }, st, compact = false) {
  let h = '';
  if (registered) h += ` <span class="badge accent" title="Registered client: authenticates with its API keys">${compact ? '⚿' : '⚿ registered'}</span>`;
  else if (id.startsWith('unverified:')) h += ' <span class="badge warn" title="A keyless request claimed a registered name">unverified</span>';
  if (st.vip === id) h += ' <span class="badge vip">★ VIP</span>';
  if (st.boost === id) h += ' <span class="badge warn">⚡ Boost</span>';
  if (disabled) h += ' <span class="badge bad">disabled</span>';
  else if (blocked) h += ' <span class="badge bad">blocked</span>';
  return h;
}

/** Queue depth for the narrow list column; the running count is a tooltip. */
function queueShort(u) {
  const n = u ? u.queued + u.processing : 0;
  return n ? `<b class="st-warn" title="${u.processing} running">${n}</b>` : '<span class="dim">0</span>';
}

function lastUsed(c) {
  return Math.max(0, ...c.keys.map(k => k.last_used_ms || 0));
}

/**
 * Every client: registered ones (listed even when idle) and the
 * unregistered ids that currently have live state, busiest first.
 */
function identities() {
  const st = S.state;
  const live = new Map(st.users.map(u => [u.id, u]));
  const rows = S.clients.map(c => ({ id: c.name, reg: c, u: live.get(c.name) }));
  const known = new Set(S.clients.map(c => c.name));
  for (const u of st.users) if (!known.has(u.id)) rows.push({ id: u.id, reg: null, u });
  const out = r => (r.u ? r.u.queued + r.u.processing : 0);
  rows.sort((a, b) => out(b) - out(a)
    || (b.u?.done ?? 0) - (a.u?.done ?? 0)
    || Number(!!b.reg) - Number(!!a.reg)
    || a.id.localeCompare(b.id));
  return rows;
}

function rowFlags(r) {
  return { registered: !!r.reg, disabled: !!r.reg?.disabled, blocked: !!r.u?.blocked };
}

function renderClients(st) {
  const all = identities();
  const rows = all.filter(r => S.clientFilter === 'all'
    || (S.clientFilter === 'registered') === !!r.reg);
  if (!rows.some(r => r.id === S.selectedClient)) S.selectedClient = rows[0]?.id ?? null;

  setHTML($('#client-list'), rows.length ? `<table>
    <tr><th>Client</th><th class="num">Queue</th><th class="num">Done</th><th class="num">Drop</th></tr>
    ${rows.map(r => `<tr class="clickable ${r.id === S.selectedClient ? 'selected' : ''}" data-action="select-client" data-name="${esc(r.id)}">
      <td><div class="client-name"><span class="mono">${esc(r.id)}</span>${idBadges(r.id, rowFlags(r), st, true)}</div>
          <div class="client-sub trunc">${esc(r.reg ? (r.reg.note || `${r.reg.keys.length} key${r.reg.keys.length === 1 ? '' : 's'}`) : (r.u?.ip ? `last IP ${r.u.ip}` : 'unregistered'))}</div></td>
      <td class="num">${queueShort(r.u)}</td>
      <td class="num">${fmtNum(r.u?.done ?? 0)}</td>
      <td class="num">${fmtNum(r.u?.dropped ?? 0)}</td></tr>`).join('')}
    </table>`
    : `<div class="empty">${S.clientFilter === 'unregistered'
      ? 'No unregistered ids are active.'
      : 'No clients yet.<br>Create one to hand out API keys.'}</div>`);

  const r = rows.find(x => x.id === S.selectedClient);
  setHTML($('#client-detail'), r ? clientDetail(r, st) : '<div class="empty">Select a client</div>');
}

function clientDetail(r, st) {
  const c = r.reg, u = r.u, id = esc(r.id);
  const ipBlocked = !!u?.ip && st.blocked.ips.includes(u.ip);
  const status = c
    ? (c.disabled ? '<span class="badge bad">disabled</span>' : '<span class="badge ok">active</span>')
    : (u?.blocked ? '<span class="badge bad">blocked</span>' : '<span class="badge">unregistered</span>');
  const prio = `
    <button class="btn sm ${st.vip === r.id ? 'on-vip' : ''}" data-action="user" data-op="vip" data-id="${id}" title="Always served first (TUI: p)">★ VIP</button>
    <button class="btn sm ${st.boost === r.id ? 'on-boost' : ''}" data-action="user" data-op="boost" data-id="${id}" title="Extra share of turns (TUI: b)">⚡ Boost</button>`;
  const ipBtn = !u?.ip ? '' : ipBlocked
    ? `<button class="btn sm" data-action="unblock" data-kind="ip" data-value="${esc(u.ip)}">Unblock IP</button>`
    : `<button class="btn sm danger" data-action="user" data-op="block-ip" data-id="${id}" title="Block ${esc(u.ip)} (TUI: X)">Block IP</button>`;
  const actions = c
    ? `${prio}
       <button class="btn sm" data-action="edit-client" data-name="${id}">Edit</button>
       <button class="btn sm ${c.disabled ? '' : 'danger'}" data-action="toggle-client" data-name="${id}">${c.disabled ? 'Enable' : 'Disable'}</button>
       ${ipBtn}
       <button class="btn sm danger" data-action="delete-client" data-name="${id}">Delete</button>`
    : `${prio}
       <button class="btn sm primary" data-action="register" data-id="${id}" ${NAME_RE.test(r.id) ? '' : 'disabled title="Not a valid client name"'}>Register</button>
       ${u?.blocked && !ipBlocked
         ? `<button class="btn sm" data-action="user" data-op="unblock" data-id="${id}">Unblock</button>`
         : `<button class="btn sm danger" data-action="user" data-op="block" data-id="${id}" title="Block this id (TUI: x)">Block</button>`}
       ${ipBtn}`;
  const meta = `
    <div><b>${u ? u.queued + u.processing : 0}</b>in queue</div>
    <div><b>${fmtNum(u?.done ?? 0)}</b>requests done</div>
    <div><b>${fmtNum(u?.dropped ?? 0)}</b>dropped</div>
    <div><b class="mono">${esc(u?.ip ?? '—')}</b>last IP</div>
    ${c ? `<div><b>${fmtAgo(lastUsed(c))}</b>last key use</div><div><b>${fmtDate(c.created_at_ms)}</b>created</div>` : ''}`;
  let body;
  if (c) {
    body = `
    <div class="section-h"><h2>API keys</h2><button class="btn sm primary" data-action="add-key" data-name="${id}">+ Add key</button></div>
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
            <button class="btn sm" data-action="reassign-key" data-id="${esc(k.id)}" ${S.clients.length < 2 ? 'disabled' : ''}>Reassign</button>
            <button class="btn sm" data-action="rename-key" data-id="${esc(k.id)}">Rename</button>
            <button class="btn sm danger" data-action="revoke-key" data-id="${esc(k.id)}">Revoke</button>
          </span></td></tr>`;
      }).join('')}</table></div>`
      : '<div class="empty">No keys — this client cannot authenticate yet.</div>'}
    <p class="field-hint" style="margin-top:14px !important">Requests send a key as <code>Authorization: Bearer &lt;key&gt;</code> or <code>X-Api-Key: &lt;key&gt;</code>.
      Only requests with one of these keys can use the name <code>${id}</code>; VIP, boost and disabled are saved with the client.</p>`;
  } else if (r.id.startsWith('unverified:')) {
    const claimed = esc(r.id.slice('unverified:'.length));
    body = `<p class="note">Requests <b>without an API key</b> claimed the registered name <code>${claimed}</code> in <code>X-User-ID</code>.
      They are queued apart under this id, so they get none of <code>${claimed}</code>'s queue position, priority or stats.
      If they are legitimate, give that tool one of <code>${claimed}</code>'s keys.</p>`;
  } else {
    body = `<p class="note">Identified only by <code>X-User-ID</code> (or the IP the front proxy fills in), so any request can use this id.
      VIP and boost on it last until restart. <b>Register</b> it to give it API keys and saved settings —
      after that, only requests with one of its keys queue here.</p>`;
  }
  return `
    <div class="detail-h">
      <div><h2><span class="mono">${id}</span> ${status}</h2>
        <div class="dim">${esc(c ? (c.note || 'No note') : 'Unregistered id')}</div></div>
    </div>
    <div class="btn-group detail-actions">${actions}</div>
    <div class="detail-meta">${meta}</div>
    ${body}`;
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
  const tbl = (items, kind, label, empty) => items.length ? `<table>
    ${[...items].sort().map(v => `<tr><td class="mono">${esc(v)}</td>
      <td class="actions"><button class="btn sm" data-action="unblock" data-kind="${kind}" data-value="${esc(v)}">${label}</button></td></tr>`).join('')}
    </table>` : `<div class="empty">${empty}</div>`;
  setHTML($('#blocked-clients'), tbl(st.blocked.clients, 'client', 'Enable', 'No disabled clients'));
  setHTML($('#blocked-users'), tbl(st.blocked.users, 'user', 'Unblock', 'No blocked ids'));
  setHTML($('#blocked-ips'), tbl(st.blocked.ips, 'ip', 'Unblock', 'No blocked IP addresses'));
}

// --- Models: which models the proxy serves

/** Case-insensitive glob (`*`, `?`) — the server's rule, used for previews. */
function globRe(p) {
  return new RegExp('^' + p.replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*').replace(/\?/g, '.') + '$', 'i');
}

/** Every model any backend lists, grouped case-insensitively, with where it is hidden. */
function modelCatalog(st) {
  const by = new Map();
  for (const b of st.backends) {
    const hidden = new Set(b.models_hidden);
    for (const m of b.models_available) {
      const k = m.toLowerCase();
      if (!by.has(k)) by.set(k, { name: m, per: [] });
      by.get(k).per.push({ b, ok: !hidden.has(m) });
    }
  }
  return [...by.values()]
    .map(m => {
      const n = m.per.filter(p => p.ok).length;
      return { ...m, status: n === m.per.length ? 'served' : n === 0 ? 'hidden' : 'partial' };
    })
    .sort((a, b) => a.name.localeCompare(b.name));
}

function renderModels(st) {
  const pol = S.modelPolicy;
  if (!pol) return;
  const rows = modelCatalog(st);
  $$('#model-mode input').forEach(i => { i.checked = i.value === pol.mode; });

  const shown = rows.filter(r =>
    (S.modelShow === 'all' || (S.modelShow === 'served' ? r.status !== 'hidden' : r.status !== 'served')) &&
    (!S.modelQuery || r.name.toLowerCase().includes(S.modelQuery)));
  const badge = r => r.status === 'served' ? '<span class="badge ok">served</span>'
    : r.status === 'hidden' ? '<span class="badge bad">hidden</span>'
    : `<span class="badge warn">hidden on ${r.per.filter(p => !p.ok).length} of ${r.per.length}</span>`;
  setHTML($('#model-list'), shown.length ? `<table>
    <tr><th>Model</th><th>Backends</th><th>Status</th><th class="num">Serve</th></tr>
    ${shown.map(r => `<tr class="${r.status === 'hidden' ? 'is-hidden' : ''}">
      <td><span class="mono">${esc(r.name)}</span></td>
      <td><span class="chips">${r.per.map(p => `<span class="chip ${p.ok ? '' : 'off'}" title="${esc(p.b.url)}${p.ok ? '' : ' — hidden here'}">${esc(bkName(p.b.url))}</span>`).join('')}</span></td>
      <td>${badge(r)}</td>
      <td class="num"><label class="switch" title="${r.status === 'hidden' ? 'Serve on every backend' : 'Hide on every backend'}">
        <input type="checkbox" data-model="${esc(r.name)}" ${r.status !== 'hidden' ? 'checked' : ''}><span></span></label></td></tr>`).join('')}
    </table>` : `<div class="empty">${rows.length ? 'No models match' : 'No backend lists any models yet'}</div>`);

  const scope = r => r.backends.length
    ? r.backends.map(u => `<span class="chip" title="${esc(hostOf(u))}">${esc(bkName(u))}</span>`).join('')
    : '<span class="dim">all backends</span>';
  const list = rules => `<ul class="rules">${rules.map(r => `<li>
      <div class="rule-top"><span class="keyval">${esc(r.pattern)}</span>
        <button class="btn sm danger" data-action="del-rule" data-id="${esc(r.id)}">Remove</button></div>
      <div class="rule-meta"><span class="chips">${scope(r)}</span>
        <span class="dim">· ${r.matches} match${r.matches === 1 ? '' : 'es'}</span>
        ${r.from_toggle ? '<span class="dim" title="Created by a model\'s switch">· from switch</span>' : ''}</div>
    </li>`).join('')}</ul>`;
  const hide = pol.rules.filter(r => r.action === 'hide');
  const allow = pol.rules.filter(r => r.action === 'allow');
  const allowActive = pol.mode === 'allow';
  setHTML($('#model-rules'), `
    <div class="section-h"><h2>Hide rules</h2><span class="dim">${hide.length}</span></div>
    ${hide.length ? list(hide) : '<div class="empty">No hide rules.</div>'}
    ${allowActive || allow.length ? `
      <div class="section-h rules-h"><h2>Allow rules</h2><span class="dim">${allowActive ? allow.length : 'inactive in this mode'}</span></div>
      <div class="${allowActive ? '' : 'rules-inactive'}">${allow.length ? list(allow)
        : '<div class="empty">No allow rules — nothing is served. Switch models on, or add an allow rule.</div>'}</div>` : ''}
    <p class="field-hint" style="margin-top:14px !important">Hide rules always win. ${allowActive
      ? 'Only models an allow rule matches are served.'
      : 'Allow rules apply only in "Serve only allowed".'}
      Patterns also catch models a backend adds later; a model's switch adds or removes an exact-name rule.</p>`);
}

async function addRuleDialog() {
  const dlg = $('#dlg-rule');
  const form = $('form', dlg);
  form.reset();
  form.action.value = S.modelPolicy?.mode === 'allow' ? 'allow' : 'hide';
  form.scope.innerHTML = '<option value="">All backends</option>' +
    S.state.backends.map(b => `<option value="${esc(b.url)}">${esc(bkName(b.url))}</option>`).join('');
  const catalog = modelCatalog(S.state);
  form.oninput = () => {
    const p = form.pattern.value.trim();
    const scope = form.scope.value;
    const re = p ? globRe(p) : null;
    const hits = re ? catalog.filter(m => m.per.some(x => (!scope || x.b.url === scope) && re.test(m.name))) : [];
    $('[data-preview]', dlg).innerHTML = p
      ? `<b>${hits.length}</b> model${hits.length === 1 ? '' : 's'} match now${hits.length ? ': ' + hits.slice(0, 6).map(m => `<code>${esc(m.name)}</code>`).join(', ') + (hits.length > 6 ? ' …' : '') : ''}`
      : 'Type a pattern to see which models it matches.';
  };
  form.oninput();
  const fd = await ask(dlg);
  if (!fd) return;
  await act('POST', 'models/rules', {
    pattern: fd.get('pattern').trim(),
    action: fd.get('action'),
    backends: fd.get('scope') ? [fd.get('scope')] : [],
  }, 'Rule added');
}

async function setModelMode(mode) {
  const pol = S.modelPolicy;
  if (mode === 'allow' && !pol.rules.some(r => r.action === 'allow')) {
    const ok = await confirmDlg('Serve only allowed models?',
      'There are no allow rules yet, so <b>no model will be served</b> until you switch models on or add an allow rule. Hide rules keep applying.',
      'Switch mode');
    if (!ok) { render(); return; }
  }
  await act('PUT', 'models/mode', { mode },
    mode === 'allow' ? 'Now serving only allowed models' : 'Now serving all models except hidden');
}

// --- Logs

function infoClass(info) {
  if (/^2\d\d/.test(info) || /^ok\b/.test(info)) return 'st-ok';
  if (/^[45]\d\d/.test(info) || /drop|fail|error|timeout/i.test(info)) return 'st-bad';
  if (/queued|wait/i.test(info)) return 'st-warn';
  return '';
}

/** Bytes as 512 B, 12.3 KB, 4.1 MB. */
function fmtBytes(n) {
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB'];
  let v = n / 1024, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/** Log info with its byte counts (`resp=12345B`, `body=0B`, `(123 B waiting)`) made readable. */
const fmtInfo = info => info
  .replace(/\b(resp|body)=(\d+)B\b/g, (_, k, n) => `${k === 'resp' ? 'response' : 'body'} ${fmtBytes(+n)}`)
  .replace(/\((\d+) B waiting\)/, (_, n) => `(${fmtBytes(+n)} waiting)`);

function logTable(logs, clickable = true) {
  if (!logs.length) return '<div class="empty">No matching log events</div>';
  return `<table class="logs">
    <tr><th>Time</th><th></th><th>User</th><th>Model</th><th>Backend</th><th>Info</th></tr>
    ${logs.map(l => `<tr class="${clickable ? 'clickable' : ''}" ${clickable ? `data-action="log-detail" data-seq="${l.seq}"` : ''}>
      <td class="time">${fmtTime(l.at_ms)}</td>
      <td><span class="dir ${esc(l.dir)}">${esc(l.dir)}</span></td>
      <td class="mono">${esc(l.user)}</td>
      <td class="trunc ${l.model ? '' : 'none'}">${esc(l.model ?? '—')}</td>
      <td class="dim ${l.backend ? '' : 'none'}">${l.backend ? bkCell(l.backend) : '—'}</td>
      <td class="${infoClass(l.info)}">${esc(fmtInfo(l.info))}${l.has_content ? ' <span class="dim">· ⌕</span>' : ''}</td></tr>`).join('')}
    </table>`;
}

function renderLogs() {
  const q = $('#log-filter').value.trim().toLowerCase();
  const dir = $('#log-dir').value;
  const logs = S.logs.filter(l =>
    (!dir || l.dir === dir) &&
    (!q || [l.user, l.model, l.backend, l.backend && bkName(l.backend), l.info].some(v => v && String(v).toLowerCase().includes(q))));
  setHTML($('#logs'), logTable(logs));
}

// --- Settings
//
// The form is drawn only on demand (entering the page, save, undo, adding a
// row): polling redraws the backends table and the save bar, never the
// inputs, so a half-typed value survives. Edits collect in `S.draft`
// (setting -> new value; `null` = drop the UI's value) until Save.

const FIELD_META = {
  timeout: { label: 'Request timeout', unit: 's', min: 1, help: 'How long a backend may take for one request before it is cut off.' },
  stuck_timeout: { label: 'Give up when no backend fits', unit: 's', min: 1, help: 'A request that no backend can ever serve gets a 503 after this long.' },
  max_concurrent_per_backend: { label: 'Requests per backend', unit: 'at once', min: 1, help: 'For backends that do not report their slots.' },
  detect_backend_slots: { label: 'Use the slots backends report', kind: 'bool', help: 'llama.cpp and halogen say how many requests they run in parallel.' },
  max_queued_bytes: { label: 'Queue memory cap', unit: 'MiB', scale: 1 << 20, min: 1, help: 'Request bodies waiting in all queues together; past this, new requests get a 503.' },
  max_body_bytes: { label: 'Largest request body', unit: 'MiB', scale: 1 << 20, min: 1, help: 'Bigger requests get a 413.' },
  load_keep_alive: { label: 'Keep loaded models for', unit: 's', min: -1, help: 'Sent with model loads (Ollama); -1 keeps them loaded.' },
  model_aliases: { label: 'Model aliases', kind: 'aliases', help: 'A request for the alias (any letter case) is served by the model.' },
  ip_identity: { label: 'Identify keyless clients by IP', kind: 'bool', help: 'No key and no X-User-ID: queue as ip-&lt;address&gt;. Off: they share the "anonymous" queue.' },
  sticky_routing: { label: 'Keep a conversation on its backend', kind: 'select', options: [['off', 'Off'], ['header', 'Explicit session id only'], ['prompt', 'Session id or prompt'], ['identity', 'One backend per client']], help: 'Sending a turn to the backend that served the one before avoids re-reading the whole context. The remembered backend only has to be free-ish, never the emptiest.' },
  sticky_ttl: { label: 'Remember a conversation for', unit: 's', min: 1, help: 'Idle time after which a conversation may move to another backend.' },
  sticky_lag: { label: 'Slack before a conversation moves', unit: 'requests', min: 0, help: 'How many more requests its backend may run than the emptiest one before the conversation goes elsewhere instead of waiting.' },
  sticky_max_entries: { label: 'Conversations remembered', min: 1, help: 'The idlest are forgotten past this.' },
  allow_all_routes: { label: 'Proxy unknown routes', kind: 'bool', help: 'Forward paths ollamaMQ does not know to the backends.' },
  metrics_token: { label: 'Metrics token', kind: 'secret', help: 'Bearer token that <code>GET /metrics</code> requires.' },
  log_content_limit: { label: 'Captured text per log record', unit: 'KiB', scale: 1024, min: 1, help: 'Request and response text kept for the Logs page.' },
};
const SET_GROUPS = [
  ['Scheduling & limits', ['timeout', 'stuck_timeout', 'max_concurrent_per_backend', 'detect_backend_slots', 'max_queued_bytes', 'max_body_bytes', 'load_keep_alive']],
  ['Routing', ['model_aliases', 'ip_identity', 'sticky_routing', 'sticky_ttl', 'sticky_lag', 'sticky_max_entries', 'allow_all_routes']],
  ['Security', ['@password', 'metrics_token']],
  ['Logging & server', ['log_content_limit', '@startup']],
];
const STARTUP_LABEL = {
  host: 'Listen address', port: 'Port', request_log_path: 'Request log file', request_log_max_bytes: 'Log file size',
  request_log_max_files: 'Log files kept', clients_file: 'Clients file', models_file: 'Model rules file',
};
const SOURCE_LABEL = { default: 'default', file: 'appconf.yaml', cli: 'command line', env: 'environment' };

const setField = key => S.settings?.fields.find(f => f.key === key);
const sameJson = (a, b) => JSON.stringify(a) === JSON.stringify(b);
/** The value the form shows: the draft's, else the saved one. */
function draftValue(f) {
  if (!S.draft.has(f.key)) return f.value;
  const v = S.draft.get(f.key);
  return v === null ? f.fallback : v;
}

function setDraft(key, value) {
  const f = setField(key);
  if (!f) return;
  if (value !== null && sameJson(value, f.value)) S.draft.delete(key);
  else S.draft.set(key, value);
  $(`[data-row="${key}"]`)?.classList.toggle('dirty', S.draft.has(key));
  renderSettingsBar();
}

function sourceChip(f) {
  if (f.locked_by) return `<span class="src locked" title="Set by ${esc(f.locked_by)}; it wins over the web UI">🔒 ${esc(f.locked_by)}</span>`;
  return `<span class="src ${f.source}">${SOURCE_LABEL[f.source] ?? f.source}</span>`;
}

function setControl(f) {
  const m = FIELD_META[f.key];
  const v = draftValue(f);
  const dis = f.locked_by ? 'disabled' : '';
  if (m.kind === 'bool') {
    return `<label class="switch"><input type="checkbox" id="set-${f.key}" data-key="${f.key}" ${v ? 'checked' : ''} ${dis}><span></span></label>`;
  }
  if (m.kind === 'select') {
    return `<select id="set-${f.key}" data-key="${f.key}" ${dis}>${m.options.map(([val, text]) => `<option value="${val}" ${v === val ? 'selected' : ''}>${text}</option>`).join('')}</select>`;
  }
  if (m.kind === 'secret') {
    const shown = S.revealToken && S.draft.get(f.key) === S.revealToken;
    const isSet = v !== null && v !== '';
    return `<div class="secret">
      ${shown ? `<div class="keybox"><code>${esc(S.revealToken)}</code><button type="button" class="btn sm" data-action="copy-token">Copy</button></div>
        <div class="set-help">Save to apply. The token is shown only now.</div>`
        : `<span class="${isSet ? '' : 'st-warn'}">${isSet ? 'Set' : 'Not set: /metrics is open'}</span>`}
      <span class="btn-group">
        <button type="button" class="btn sm" data-action="token-generate" ${dis}>Generate</button>
        ${isSet ? `<button type="button" class="btn sm" data-action="token-clear" ${dis}>Remove</button>` : ''}
      </span></div>`;
  }
  if (m.kind === 'aliases') {
    const rows = Object.entries(v || {}).concat([['', '']]);
    return `<div class="aliases">${rows.map(([a, t], i) => `<div class="alias-row">
        <input data-alias="k" value="${esc(a)}" placeholder="${i === rows.length - 1 ? 'new alias' : ''}" aria-label="Alias" autocomplete="off" spellcheck="false" ${dis}>
        <span class="dim">→</span>
        <input data-alias="v" value="${esc(t)}" list="dl-set-models" placeholder="${i === rows.length - 1 ? 'model' : ''}" aria-label="Model" autocomplete="off" spellcheck="false" ${dis}>
        ${a ? `<button type="button" class="btn sm" data-action="alias-remove" data-alias-key="${esc(a)}" aria-label="Remove alias ${esc(a)}" ${dis}>✕</button>` : '<span></span>'}
      </div>`).join('')}</div>`;
  }
  const shown = v == null ? '' : (m.scale ? +(v / m.scale).toFixed(2) : v);
  return `<span class="num-in"><input type="number" id="set-${f.key}" data-key="${f.key}" value="${esc(shown)}" min="${m.min ?? ''}" step="any" ${dis}><span class="dim">${m.unit}</span></span>`;
}

function setRow(f) {
  const m = FIELD_META[f.key];
  const resettable = !f.locked_by && (f.source === 'file' || S.draft.has(f.key));
  const back = f.source === 'file' && !S.draft.has(f.key) ? 'Use the default value' : 'Undo change';
  return `<div class="set-row ${S.draft.has(f.key) ? 'dirty' : ''}" data-row="${f.key}">
    <div class="set-l"><label for="set-${f.key}">${m.label}</label><div class="set-help">${m.help}</div></div>
    <div class="set-c">${setControl(f)}</div>
    <div class="set-s">${sourceChip(f)}${f.live ? '' : ' <span class="badge warn" title="Takes effect after a restart">restart</span>'}
      ${resettable ? `<button type="button" class="linkbtn" data-action="set-reset" data-key="${f.key}">${back}</button>` : ''}</div>
  </div>`;
}

function fmtStartup(key, v) {
  if (v == null) return '<span class="dim">—</span>';
  if (key === 'request_log_max_bytes') return fmtBytes(v);
  return `<code>${esc(v)}</code>`;
}

function passwordRow() {
  const me = S.me || {};
  const src = me.password_source || '';
  const from = me.password_changed_at_ms
    ? `changed here${me.password_changed_at_ms ? ` on ${fmtDate(me.password_changed_at_ms)}` : ''}`
    : `from ${src === 'appconf.yaml' ? 'appconf.yaml' : `the environment (${esc(src)})`}`;
  return `<div class="set-row" data-row="password">
    <div class="set-l"><label>Admin password</label><div class="set-help">Signed in as <b>${esc(me.user || '')}</b>. Password ${from}.</div></div>
    <div class="set-c"><button type="button" class="btn" data-action="change-password">Change password</button></div>
    <div class="set-s"></div></div>`;
}

function startupRows() {
  const pending = new Set(S.settings.restart_pending);
  return `<div class="set-row startup"><div class="set-l"><label>Read at startup</label>
      <div class="set-help">Set in appconf.yaml or on the command line; shown here for reference.</div></div>
    <dl class="kv startup-kv">${S.settings.startup.map(f => `<dt>${STARTUP_LABEL[f.key] ?? f.key}</dt>
      <dd>${fmtStartup(f.key, f.value)} ${sourceChip(f)}${pending.has(f.key) ? ' <span class="badge warn">restart to apply</span>' : ''}</dd>`).join('')}</dl></div>`;
}

/** The whole form; called on demand only (see above). */
function renderSettingsForm() {
  if (!S.settings) return;
  S.formShown = true;
  const models = new Set((S.state?.backends || []).flatMap(b => b.models_available));
  $('#dl-set-models').innerHTML = [...models].sort().map(m => `<option value="${esc(m)}">`).join('');
  $('#set-form').innerHTML = SET_GROUPS.map(([title, keys]) => `<div class="panel set-panel">
    <div class="panel-h"><h2>${title}</h2></div>
    ${keys.map(k => k === '@password' ? passwordRow() : k === '@startup' ? startupRows() : setRow(setField(k))).join('')}
  </div>`).join('');
  renderSettingsBar();
}

function renderSettingsBar() {
  const n = S.draft.size;
  setHTML($('#set-tools'), `
    ${S.settings?.can_undo ? '<button class="btn" data-action="settings-undo" title="Restore the settings before the last save">Undo last save</button>' : ''}
    ${n ? '<button class="btn" data-action="settings-discard">Discard</button>' : ''}
    <button class="btn primary" data-action="settings-save" ${n ? '' : 'disabled'}>${n ? `Save ${n} change${n === 1 ? '' : 's'}` : 'Saved'}</button>`);
  $('#nav-settings').textContent = n ? '•' : '';
}

function renderSettings() {
  const set = S.settings;
  if (!set) return;
  if (!S.formShown) renderSettingsForm();
  renderSettingsBar();
  const names = Object.fromEntries([...set.fields, ...set.startup].map(f => [f.key, FIELD_META[f.key]?.label ?? STARTUP_LABEL[f.key] ?? f.key]));
  setHTML($('#set-banner'), set.restart_pending.length
    ? `<div class="note warn-note"><b>Restart needed.</b> ${set.restart_pending.map(k => esc(names[k])).join(', ')} take${set.restart_pending.length === 1 ? 's' : ''} effect after ollamaMQ restarts.</div>` : '');
}

/** Apply a settings response and redraw the form from it. */
function settingsChanged(r) {
  if (!r) return;
  S.settings = r;
  renderSettingsForm();
  renderSettings();
}

async function saveSettings() {
  const set = Object.fromEntries(S.draft);
  try {
    const r = await api('PUT', 'settings', { set });
    S.draft.clear();
    S.revealToken = null;
    const restart = r.restart_pending.some(k => k in set);
    toast(restart ? 'Settings saved. Some take effect after a restart.' : 'Settings saved and applied');
    settingsChanged(r);
  } catch (e) {
    toast(e.message, false);
  }
}

function readAliases() {
  const out = {};
  for (const row of $$('#set-form .alias-row')) {
    const k = $('[data-alias="k"]', row).value.trim();
    const v = $('[data-alias="v"]', row).value.trim();
    if (k && v) out[k] = v;
  }
  return out;
}

function onSettingsInput(e) {
  const el = e.target;
  if (el.dataset.alias) { setDraft('model_aliases', readAliases()); return; }
  const key = el.dataset.key;
  const f = key && setField(key);
  if (!f) return;
  const m = FIELD_META[key];
  if (m.kind === 'bool') { setDraft(key, el.checked); return; }
  if (m.kind === 'select') { setDraft(key, el.value); return; }
  if (el.value === '') { S.draft.delete(key); el.closest('.set-row')?.classList.remove('dirty'); renderSettingsBar(); return; }
  const n = Number(el.value);
  if (Number.isFinite(n)) setDraft(key, Math.round(n * (m.scale || 1)));
}

async function backendDialog(existing) {
  const dlg = $('#dlg-backend');
  const form = $('form', dlg);
  form.reset();
  $('[data-test]', dlg).textContent = '';
  $('[data-title]', dlg).textContent = existing ? `Edit ${existing.name || existing.label}` : 'Add backend';
  $('[data-submit]', dlg).textContent = existing ? 'Save' : 'Add backend';
  $('[data-only="edit"]', dlg).style.display = existing ? '' : 'none';
  $('[data-only="token-set"]', dlg).style.display = existing?.token_set ? '' : 'none';
  form.url.readOnly = !!existing;
  form.token.placeholder = existing?.token_set ? 'unchanged — type to replace' : 'optional';
  if (existing) {
    form.url.value = existing.url;
    form.name.value = existing.name || '';
    form.kind.value = existing.kind === 'systemone' ? 'systemone' : '';
  }
  const fd = await ask(dlg);
  if (!fd) return;
  const body = { url: fd.get('url').trim(), name: fd.get('name').trim() || null, kind: fd.get('kind') || null };
  const token = fd.get('token');
  if (token) body.token = token;
  else if (fd.get('clear_token')) body.token = null;
  settingsChanged(await act(existing ? 'PUT' : 'POST', 'settings/backends', body,
    existing ? 'Backend saved' : `Backend ${body.name || body.url} added; probing it`));
}

async function testBackend() {
  const form = $('#dlg-backend form');
  const out = $('[data-test]', form);
  const url = form.url.value.trim();
  if (!url) { out.innerHTML = '<span class="st-bad">Enter a URL first</span>'; return; }
  out.innerHTML = '<span class="dim">Testing…</span>';
  try {
    const body = { url };
    if (form.token.value) body.token = form.token.value;
    const r = await api('POST', 'settings/backends/test', body);
    out.innerHTML = r.online
      ? `<span class="st-ok">✓ ${esc(r.api_type)}</span>${r.slots ? ` · ${r.slots} slots` : ''} · ${r.model_count} model${r.model_count === 1 ? '' : 's'}${r.models.length ? `: ${esc(r.models.slice(0, 4).join(', '))}${r.model_count > 4 ? ', …' : ''}` : ''}`
      : '<span class="st-bad">✕ No answer from that address</span>';
  } catch (e) {
    out.innerHTML = `<span class="st-bad">${esc(e.message)}</span>`;
  }
}

async function passwordDialog() {
  const dlg = $('#dlg-password');
  $('form', dlg).reset();
  const fd = await ask(dlg);
  if (!fd) return;
  if (fd.get('new') !== fd.get('repeat')) { toast('The new passwords do not match', false); return; }
  const r = await act('POST', 'account/password', { current: fd.get('current'), new: fd.get('new'), sign_out_others: !!fd.get('others') },
    fd.get('others') ? 'Password changed. Other sessions are signed out.' : 'Password changed');
  if (r) {
    S.me = await api('GET', 'me').catch(() => S.me);
    renderSettingsForm();
  }
}

/** A random token for /metrics (hex, 32 bytes). */
function newToken() {
  const b = new Uint8Array(32);
  crypto.getRandomValues(b);
  return [...b].map(x => x.toString(16).padStart(2, '0')).join('');
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
  $('[data-backend]', dlg).textContent = `on ${bkName(b.url)} — pick a listed model or type any name the backend can resolve.`;
  $('#dl-models').innerHTML = b.models_available.map(m => `<option value="${esc(m)}">`).join('');
  const fd = await ask(dlg);
  if (!fd) return;
  const model = fd.get('model').trim();
  await act('POST', `backends/${idx}/load`, { model }, r => `loading ${r?.model ?? model} started`);
}

// ------------------------------------------------------------------ actions

const actions = {
  'filters': () => openFilters(),
  'add-backend': () => backendDialog(null),
  'backend-edit': el => backendDialog(S.settings.backends.find(b => b.url === el.dataset.url)),
  'backend-toggle': async el => {
    const b = S.settings.backends.find(x => x.url === el.dataset.url);
    if (!b.disabled && !await confirmDlg(`Disable ${esc(b.name || b.label)}?`,
      'It stops taking requests now; requests already running there finish. It stays in the list and can be enabled again.', 'Disable')) return;
    settingsChanged(await act('PUT', 'settings/backends', { url: b.url, disabled: !b.disabled },
      b.disabled ? 'Backend enabled; probing it' : 'Backend disabled'));
  },
  'backend-remove': async el => {
    const b = S.settings.backends.find(x => x.url === el.dataset.url);
    if (!await confirmDlg(`Remove ${esc(b.name || b.label)}?`,
      'It stops taking requests now and leaves the list after the next restart. Requests already running there finish.', 'Remove')) return;
    settingsChanged(await act('POST', 'settings/backends/remove', { url: b.url }, 'Backend removed'));
  },
  'backend-restore': el => act('POST', 'settings/backends', { url: el.dataset.url, name: el.dataset.name || null }, 'Backend added back').then(settingsChanged),
  'test-backend': () => testBackend(),
  'change-password': () => passwordDialog(),
  'settings-save': () => saveSettings(),
  'settings-discard': () => { S.draft.clear(); S.revealToken = null; renderSettingsForm(); },
  'settings-undo': async () => {
    if (!await confirmDlg('Undo the last save?', 'The settings go back to how they were before the last save, and apply at once.', 'Undo')) return;
    settingsChanged(await act('POST', 'settings/undo', {}, 'Last save undone'));
  },
  'set-reset': el => {
    const f = setField(el.dataset.key);
    if (S.draft.has(f.key)) S.draft.delete(f.key);
    else S.draft.set(f.key, null);
    renderSettingsForm();
  },
  'alias-remove': el => {
    const f = setField('model_aliases');
    const next = { ...draftValue(f) };
    delete next[el.dataset.aliasKey];
    setDraft('model_aliases', next);
    renderSettingsForm();
  },
  'token-generate': () => { S.revealToken = newToken(); setDraft('metrics_token', S.revealToken); renderSettingsForm(); },
  'token-clear': () => { S.revealToken = null; setDraft('metrics_token', ''); renderSettingsForm(); },
  'copy-token': () => copyText(S.revealToken),
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
      const what = op === 'block' ? `<b>${esc(id)}</b>` : `the last IP of <b>${esc(id)}</b>`;
      if (!await confirmDlg('Block?', `New requests from ${what} get 403, and its requests still waiting in the queue are dropped.`, 'Block')) return;
    }
    return act('POST', `users/${encodeURIComponent(id)}/${op}`, {}, labels[op]);
  },
  'unblock': el => act('POST', 'blocked/unblock', { kind: el.dataset.kind, value: el.dataset.value },
    `${el.dataset.kind === 'client' ? 'Enabled' : 'Unblocked'} ${el.dataset.value}`),

  'new-client': () => clientDialog(null),
  // Turn an unregistered id into a client of the same name, then offer to
  // give it a key straight away.
  'register': async el => {
    const name = el.dataset.id;
    const r = await act('POST', 'clients', { name, note: '' }, `${name} registered — add a key so it can authenticate`);
    if (r) {
      S.selectedClient = name;
      S.clients = await api('GET', 'clients');
      render();
      addKeyDialog(name);
    }
  },
  'select-client': el => {
    S.selectedClient = el.dataset.name;
    render();
    // The detail panel sits below the list on phones: bring it into view.
    if (isPhone()) $('#client-detail').scrollIntoView({ behavior: 'smooth', block: 'start' });
  },
  'add-rule': () => addRuleDialog(),
  'del-rule': el => act('DELETE', `models/rules/${encodeURIComponent(el.dataset.id)}`, undefined, 'Rule removed'),
  'logout': async () => {
    if (DEMO) return toast('Demo mode: nothing to sign out of');
    try { await api('POST', 'logout', {}); } catch { /* signed out anyway */ }
    location.href = 'login';
  },
  'nav-open': () => setNav(true),
  'nav-close': () => setNav(false),
  'edit-client': el => clientDialog(S.clients.find(c => c.name === el.dataset.name)),
  'toggle-client': el => {
    const c = S.clients.find(x => x.name === el.dataset.name);
    return act('PATCH', `clients/${encodeURIComponent(c.name)}`, { disabled: !c.disabled },
      c.disabled ? `${c.name} enabled` : `${c.name} disabled — its keys get 403 and its queue is dropped`);
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
  'rename-key': el => {
    const td = el.closest('tr').children[1];
    if ($('input', td)) return;
    td.innerHTML = `<input type="text" maxlength="60" value="${esc(findKey(el.dataset.id).key.label || '')}" placeholder="label" title="Enter saves, Esc cancels">`;
    const inp = $('input', td);
    inp.focus();
    inp.select();
    inp.addEventListener('keydown', async e => {
      if (e.key === 'Escape') render();
      else if (e.key === 'Enter') { e.preventDefault(); if (await act('PATCH', `keys/${encodeURIComponent(el.dataset.id)}`, { label: inp.value.trim() }, 'Label updated')) render(); }
    });
    inp.addEventListener('blur', () => { if (document.contains(inp)) render(); });
  },
  'fold-all': () => { S.collapsed.clear(); saveCollapsed(); render(); },
  'fold-none': () => { for (const b of $$('#ov-now .det-h')) S.collapsed.add(b.dataset.key); saveCollapsed(); render(); },

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
$('#client-filter').addEventListener('change', e => { S.clientFilter = e.target.value; render(); });
$('#model-search').addEventListener('input', e => { S.modelQuery = e.target.value.trim().toLowerCase(); render(); });
$('#model-show').addEventListener('change', e => { S.modelShow = e.target.value; render(); });
$('#model-mode').addEventListener('change', e => setModelMode(e.target.value));
$(`#ov-req-show input[value="${S.reqShow}"]`).checked = true;
$(`#ov-req-view input[value="${S.reqView}"]`).checked = true;
['input', 'change'].forEach(t => $('#set-form').addEventListener(t, onSettingsInput));
// Leaving with unsaved settings: the browser asks first.
window.addEventListener('beforeunload', e => { if (S.draft.size) e.preventDefault(); });
$('#ov-req-show').addEventListener('change', e => {
  S.reqShow = e.target.value;
  try { localStorage.setItem('omq.reqShow', S.reqShow); } catch { /* storage blocked */ }
  render();
});
$('#ov-req-view').addEventListener('change', e => {
  S.reqView = e.target.value;
  try { localStorage.setItem('omq.reqView', S.reqView); } catch { /* storage blocked */ }
  render();
});
// The Filters modal writes the scope on Apply (Esc/Cancel discard the edits).
$('#dlg-filters').addEventListener('close', e => {
  if (e.target.returnValue === 'ok') {
    collectFilters();
    saveFilters();
    S.overview = null;
    poll();
  }
});
// Modal interactions: the custom interval and the presets exclude each other,
// switching identity repopulates the Clients list at once, and the all/none
// links tick whole lists.
{
  const dlg = $('#dlg-filters');
  const clearPresets = () => $$('input[name="fwin"]', dlg).forEach(r => { r.checked = false; });
  dlg.addEventListener('input', e => { if (e.target.id === 'f-win-n') clearPresets(); });
  dlg.addEventListener('change', e => {
    const t = e.target;
    if (t.name === 'fwin') $(dlg).querySelector('#f-win-n').value = '';
    else if (t.id === 'f-win-u') clearPresets();
    else if (t.name === 'fmode') { collectFilters(); drawFilters(); }
    else if (t.type === 'checkbox' && t.closest('.flist')) updCnt(t.closest('.flist').id);
  });
  dlg.addEventListener('click', e => {
    const b = e.target.closest('[data-tog]');
    if (!b) return;
    $$('input', $('#' + b.dataset.l)).forEach(i => { i.checked = b.dataset.tog === 'all'; });
    updCnt(b.dataset.l);
  });
}
$('#f-reset').addEventListener('click', () => {
  S.filters = { win: '15m', mode: 'client', clients: [], backends: [], models: [] };
  drawFilters();
});
// Cards on a phone, table on a desktop: repaint when the viewport crosses over.
matchMedia('(max-width: 720px)').addEventListener('change', () => render());
// A model's switch: serve or hide it on every backend. On failure (e.g. a
// pattern rule still hides it) the re-render puts the switch back.
$('#model-list').addEventListener('change', e => {
  const cb = e.target.closest('input[data-model]');
  if (!cb) return;
  const model = cb.dataset.model;
  act('POST', 'models/toggle', { model, served: cb.checked }, `${model} ${cb.checked ? 'served' : 'hidden'}`)
    .then(r => { if (!r) { $('#model-list')._html = null; render(); } });
});

// ------------------------------------------------------------------- routing

function route() {
  let page = (location.hash.slice(1) || 'overview').split('?')[0];
  if (page === 'users') page = 'clients';  // Users merged into Clients
  const was = S.page;
  S.page = $(`#page-${page}`) ? page : 'overview';
  // Coming back to Settings draws the form again (from the draft, if any).
  if (S.page === 'settings' && was !== 'settings') S.formShown = false;
  $$('.page').forEach(p => p.classList.toggle('active', p.id === `page-${S.page}`));
  $$('.nav nav a').forEach(a => a.classList.toggle('active', a.dataset.page === S.page));
  // The Filters button scopes the whole Overview page; other pages hide it.
  $('#ov-filters').style.display = S.page === 'overview' ? '' : 'none';
  // The link's own text, without its icon and counter spans.
  $('#top-title').textContent = [...$(`.nav nav a[data-page="${S.page}"]`).childNodes]
    .filter(n => n.nodeType === Node.TEXT_NODE).map(n => n.textContent).join('').trim();
  setNav(false);
  poll();
}
window.addEventListener('hashchange', route);

function start() {
  if (!DEMO) {
    api('GET', 'me').then(d => { $('#whoami').textContent = d.user ? `Signed in as ${d.user}` : ''; }).catch(() => {});
  }
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
