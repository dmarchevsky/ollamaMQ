// ollamaMQ web UI — the dashboard: live proxy state plus client/API-key
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
  capture: null,             // { log, path } from api/capture: is body capture running
  logGroup: 'session',       // Logs page: one row per conversation, or per request
  clients: [],
  expanded: new Set(),       // backend URLs with their model list open
  showAll: new Set(),        // backend URLs showing every model
  revealed: new Set(),       // key ids shown unmasked
  openClients: new Set(),    // registered clients whose key rows are shown
  demoClientsSeeded: false,  // demo mode opens them all once, for static renders
  clientFilter: 'all',       // all | registered | unregistered
  modelPolicy: null,         // { mode, rules } from api/models
  modelShow: 'all',          // all | served | hidden
  modelQuery: "",
  overview: null,            // api/overview for the chosen window and filters
  filters: storedFilters(),  // the filter modal's selection (time, identity, lists)
  reqShow: storedReqShow(),  // the Activity list: active rows only, or the window too
  collapsed: storedCollapsed(),// conversation headers the operator folded shut
  pins: {},                // session tag -> operator-pinned priority level
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
  const d = { win: '15m', mode: 'client', clients: [], backends: [], models: [], priority: [] };
  try {
    const j = JSON.parse(localStorage.getItem('omq.filters') || 'null');
    if (j && typeof j === 'object') {
      return { ...d, ...j, clients: [...(j.clients || [])], backends: [...(j.backends || [])], models: [...(j.models || [])], priority: [...(j.priority || [])] };
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
    // Capture state only changes on restart (the log path is opened at boot),
    // so ask once and keep the answer.
    const [state, logs, clients, models, overview, settings, me, capture] = await Promise.all([
      api('GET', 'state'),
      needLogs && !$('#log-pause').checked ? api('GET', `logs?limit=${LOG_LIMIT}`) : null,
      needClients ? api('GET', 'clients') : null,
      needModels ? api('GET', 'models') : null,
      needOverview ? api('GET', overviewPath()) : null,
      needSettings ? api('GET', 'settings') : null,
      needSettings && !S.me ? api('GET', 'me') : null,
      needLogs && !S.capture ? api('GET', 'capture') : null,
    ]);
    S.state = state;
    if (logs) S.logs = logs;
    if (clients) S.clients = clients;
    if (models) S.modelPolicy = models;
    if (overview) {
      S.overview = overview;
      // Operator pins by session tag. The icon shows the pin when there is one,
      // because the level on a finished row is history — the pin is what that
      // conversation runs at next.
      S.pins = Object.fromEntries(overview.status.pins || []);
    }
    if (settings) S.settings = settings;
    if (me) S.me = me;
    if (capture) S.capture = capture;
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
    || f.clients.length > 0 || f.backends.length > 0 || f.models.length > 0
    || f.priority.length > 0;
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
/** A session as a person reads it: the label the client sent, else the tag. */
const sessName = o => o.session_label || o.session || null;
const prioSel = p => !S.filters.priority.length || S.filters.priority.includes(p || 'normal');
/** Priority in the tight places (graph lane, phone card): a glyph in the one
 *  hue that already means "priority", so no new status colour is invented. */
/** The priority mark: one glyph whose colour carries the level, and the thing
 *  you click to change it (`tag` given). The grey `○` — "no priority yet" — is
 *  shown everywhere a control is, because a control that only appears once a
 *  session already has priority is not a control. It is a span with a click
 *  action rather than a button: a Details header is itself a button and cannot
 *  hold one. */
const PRI = {
  normal: { g: '○', tip: 'no priority — click to make it high' },
  high: { g: '△', tip: 'high — first inside its own client — click for top' },
  top: { g: '▲', tip: 'top — first on the whole board — click to clear' },
};
const priLevel = p => (p === 'high' || p === 'top') ? p : 'normal';
/** The level a conversation runs at next: the operator's pin if it has one,
 *  otherwise the level the request being shown carried. */
const priOf = o => (o.session && S.pins[o.session]) || priLevel(o.priority);
/** Read-only glyph, for places with no room or no click target (graph lanes). */
const priMark = o => { const l = priOf(o); const d = PRI[l];
  return `<span class="pri ${l}" title="${l} priority${S.pins[o.session] ? ' — pinned by you' : ''}">${d.g}</span>`; };
/** The same glyph as the control that cycles normal → high → top → normal. */
const priCtl = (o, tag) => {
  const l = priOf(o), d = PRI[l], pinned = !!S.pins[tag];
  return tag
    ? `<span class="pri ${l}" data-action="session-pri" data-tag="${esc(tag)}" data-pri="${l}"
        title="${d.tip}${pinned ? ' — pinned by you, applies to the next turn' : ''}"
        aria-label="${l} priority, click to change">${d.g}</span>`
    : priMark(o);
};

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
  const pr = ['normal', 'high', 'top'];
  setHTML($('#f-priority'), `${flhead('Priority', 'f-pr-list', f.priority, pr.length)}
    <div class="flist" id="f-pr-list">${pr.map(p => fchk(f.priority, p, `${p === 'normal' ? '' : `<span class="pri ${p}">${PRI[p].g}</span>`}${p}`)).join('')}</div>`);
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
 *  parameter and shows nothing. A clicked preset beats whatever text is
 *  still in the custom box (typing there unchecks the presets, so whichever
 *  control was touched last wins). */
function collectFilters() {
  const f = S.filters;
  const win = $('#dlg-filters input[name="fwin"]:checked')?.value;
  const n = ($('#f-win-n').value || '').trim();
  const custom = /^\d+$/.test(n)
    ? Math.min(+n || 1, $('#f-win-u').value === 'h' ? 24 : 1440) + $('#f-win-u').value
    : null;
  f.win = win || custom || '15m';
  f.mode = $('#dlg-filters input[name="fmode"]:checked').value;
  const read = id => {
    const all = $$('#' + id + ' input');
    const on = [...all].filter(i => i.checked).map(i => i.value);
    return on.length === all.length ? [] : on.length ? on : [FNONE];
  };
  f.clients = read('f-cl-list');
  f.backends = read('f-bk-list');
  f.models = read('f-md-list');
  f.priority = read('f-pr-list');
}

/** Dots + tooltip summary on the Filters button. */
function renderFiltersBtn() {
  const on = filtersActive();
  $('#ov-filters').classList.toggle('on', on);
  $('#f-dot').hidden = !on;
  const f = S.filters;
  const bit = (sel, word) => !sel.length ? null : sel[0] === FNONE ? `no ${word}` : `${sel.length} ${word}`;
  const bits = [winLabel(f.win), f.mode === 'key' ? 'client+label' : null,
    bit(f.clients, 'clients'), bit(f.backends, 'backends'), bit(f.models, 'models'), bit(f.priority, 'priority')];
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
  const vis = { ...st, runs: (st.runs || []).filter(r => clientSel(r.client, r.key) && modelSel(r.model) && backendSel(r.backend) && prioSel(priLevel(r.priority))) };
  // Identity colours key on the display name, so client+label pairs get their
  // own hue (the server already names lanes, client rows and segments that way).
  assignColors([...new Set([...clientIds(st), ...ov.clients.map(c => c.id), ...vis.runs.map(fmtWho), ...ov.queue.map(w => fmtWho(w))])]);
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
    s.priority_sessions
      ? `<span class="pri-count" title="${s.priority_sessions} conversation${s.priority_sessions === 1 ? '' : 's'} running or waiting above normal priority">${s.priority_sessions} priority</span>`
      : null,
    ...s.loading.map(l => `${esc(l.verb)} ${esc(l.model)} on ${esc(bkName(l.backend))}`),
  ].filter(Boolean);
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
  const waitSeg = (w, style) => `<i class="seg q ${colorClass(fmtWho(w))}" style="${style}" title="${esc(
    `${fmtWho(w)} · ${w.model}\n~${fmtTok(w.est_prompt_tokens)} tokens · waiting ${fmtDur(w.waited_secs)} · #${w.pos} in its client's queue`
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
    let bar = runs.map(r => `<i class="seg run ${colorClass(fmtWho(r))}" style="flex:0 1 ${pct(r.est_prompt_tokens || avg, scale)}" title="${esc(
      `${fmtWho(r)} · ${r.model}\n~${fmtTok(r.est_prompt_tokens)} tokens · ${r.state === 'generating' ? 'generating' : 'waiting for first token'} · ${fmtDur(r.elapsed_secs)}${r.tokens_per_sec ? ` · ${r.tokens_per_sec.toFixed(1)} tok/s` : ''}`)}"></i>`).join('');
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
  // Legend of the board: the conversations on it right now, not the window
  // history — same identity (client, or client+label) the rows are coloured by.
  const legend = [...new Set([...(st.runs || []).map(fmtWho), ...ov.queue.map(fmtWho)])]
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
    // Backends alphabetically, so the panel does not rearrange itself every
    // time a request lands; the pseudo-group ("no free backend") stays last.
    .sort((a, b) => (a.pseudo - b.pseudo) || a.name.localeCompare(b.name, undefined, { sensitivity: 'base' }))
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
    + `<div class="tl-lab" title="${esc(fmtWho(head.o))}${head.s ? ` · session ${esc(head.s)}` : ''} · ${esc(priOf(head.o))} priority">${swatch(fmtWho(head.o))}<b>${esc(head.o.session_label ? `#${head.o.session_label}` : fmtWho(head.o))}</b>${head.o.session_label || head.s ? priMark(head.o) : ''}</div>`
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
    // The proxy stamped dispatch and first token when they happened, so the
    // live line is anchored on measured instants, not on back-computed ones.
    const dispatch = o.dispatch_ms ?? now - o.elapsed_secs * 1000;
    const w = o.waited_secs || 0;
    const out = [];
    if (w >= .05) out.push(ev('w', dispatch - w * 1000, w, `${fmtDur(w)} · prompt ${fmtTok(o.est_prompt_tokens)} tok`));
    if (o.ttft_secs == null) {
      out.push(ev('p', dispatch, o.elapsed_secs, `≈${fmtTok(o.est_prompt_tokens)} tok · ${fmtDur(o.elapsed_secs)} prefilling`, true));
    } else {
      if (o.ttft_secs >= .05) out.push(ev('p', dispatch, o.ttft_secs, `${fmtTok(o.est_prompt_tokens)} tok · ${fmtSec1(o.ttft_secs)}`));
      out.push(ev('g', o.first_token_ms ?? dispatch + o.ttft_secs * 1000, Math.max(0, o.elapsed_secs - o.ttft_secs),
        `${fmtNum(o.tokens)} tok · ${fmtDur(Math.max(0, o.elapsed_secs - o.ttft_secs))} · `
        + `${o.tokens_per_sec == null ? '…' : o.tokens_per_sec.toFixed(1)} t/s<span class="more"> · <b>now</b></span>`, true));
    }
    return done(out, dispatch - w * 1000, now);
  }
  const finish = row.t;
  const w = o.waited_secs || 0;
  const gen = o.gen_secs || 0;
  const pre = o.prefill_secs ?? o.ttft_secs ?? 0;
  // A finished request carries its stage times as they were measured; the
  // durations only fill in for what the proxy could not observe.
  const sg = o.stage || {};
  const end = sg.end ?? finish;
  const dispatch = sg.dispatch ?? end - (pre + gen) * 1000;
  const at = sg.queue ?? dispatch - w * 1000;
  const firstTok = sg.first_token ?? (pre ? dispatch + pre * 1000 : null);
  // Where both ends of a stage were stamped, its length is the distance between
  // them — the durations in `o` are the fallback, not the number to print.
  const wM = sg.queue != null && sg.dispatch != null ? (sg.dispatch - sg.queue) / 1000 : w;
  const preM = sg.dispatch != null && sg.first_token != null ? (sg.first_token - sg.dispatch) / 1000 : pre;
  const genM = sg.first_token != null && sg.end != null ? (sg.end - sg.first_token) / 1000 : gen;
  if (o.error) {
    const out = [];
    if (wM >= .05) out.push(ev('w', at, wM, `${fmtDur(wM)} · prompt ${fmtTok(o.prompt_tokens)} tok`));
    out.push(ev('e', end, 0, `${outcomePhrase(o)}${o.backend ? '' : '<span class="more"> · never dispatched</span>'}`));
    return done(out, at, end);
  }
  const out = [];
  // A request that left without an error and without serving cleanly (the
  // client hung up, the queue refused it) still has to say so here.
  if (o.outcome && o.outcome !== 'ok') out.push(ev('e', end, 0, outcomePhrase(o)));
  if (wM >= .05) out.push(ev('w', at, wM, `${fmtDur(wM)} · prompt ${fmtTok(o.prompt_tokens)} tok`));
  if (preM >= .05) {
    out.push(ev('p', dispatch, preM, `${fmtTok(o.prompt_tokens)} tok · ${fmtSec1(preM)}`
      + (o.cache_pct != null ? ` · cache ${o.cache_pct}%` : '')
      // Time this turn burned re-reading context the backend had evicted.
      + (o.waste_secs ? `<span class="st-bad"> · waste ${fmtSec1(o.waste_secs)}</span>` : '')
      // The rate divides the tokens actually evaluated (cache-excluded), like
      // the Backends table — dividing the whole prompt by the short time of a
      // cached prefill invented impossible speeds.
      + (o.prefill_secs && o.prefill_tokens ? `<span class="more"> · ${fmtRate(o.prefill_tokens / o.prefill_secs)}</span>` : '')));
  }
  if (genM > 0) {
    out.push(ev('g', firstTok ?? end - genM * 1000, genM,
      `${fmtTok(o.completion_tokens)} tok · ${fmtSec1(genM)} · ${(o.gen_tps ?? 0).toFixed(1)} t/s`));
  }
  return done(out, at, end);
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
    // An active conversation says how it is being served, not only that it is.
    // Decode is the live stream's own rate; prefill is measured on the newest
    // finished request here, because a request cannot time its own prefill — by
    // the time the first token proves anything, it is already decoding.
    const pre = run ? prefillRate(unit) : null;
    const chip = run
      ? (run.o.state === 'generating'
        ? `${pre ? `<span class="badge warn" title="${pre.tip}">prefill ${pre.txt} tok/s</span>` : ''}`
          + `<span class="badge dec" title="tokens generated per second, this request">${run.o.tokens_per_sec != null ? `decode ${fmtRate(run.o.tokens_per_sec)} t/s` : 'decode \u2026'}</span>`
        : `<span class="badge warn" title="the first token has not arrived${pre ? ` \u2014 prefill runs at ${pre.txt} tok/s here (${pre.tip})` : ''}">prefill ${fmtDur(run.o.elapsed_secs)}</span>`)
      : wait ? `<span class="badge q" title="Position in this client's own queue">waiting #${wait.o.pos}</span>`
      : '';
    // The summary is three right-aligned columns so the numbers line up
    // across headers; on phones the columns drop their min-widths and wrap.
    const sum = `<span class="sum"><i>${unit.length} req</i><i>${fmtTok(unit.reduce((a, x) => a + rowTokens(x), 0))} tok</i>`
      + `<i>${fmtTime(asc[0].plan.at)}→${last >= now - 1500 ? 'now' : fmtTime(last)}</i></span>`;
    out.push(`<button class="det-h" style="--c:var(--${c})" data-key="${esc(head.key)}" aria-expanded="${!shut}"
      title="${shut ? 'expand' : 'collapse'} this conversation">
      <span class="chev">${shut ? '▸' : '▾'}</span><span class="who">${esc(fmtWho(head.o))}</span>
      ${head.s || head.o.session_label ? `<span class="who">#${esc(sessName(head.o))}</span>${priCtl(head.o, head.s || head.o.session)}` : ''}
      <span class="sub" title="${esc(head.o.model)}${bk}">${esc(head.o.model)}${bk}</span>${chip}
      ${sum}
      ${head.s ? `<span class="logs-link jump-chip" data-action="conv-open" data-session="${esc(head.s)}" data-user="${esc(head.o.client)}" title="open this conversation in the viewer">\u2263 View</span>` : ''}</button>`);
    if (shut) continue;
    const lines = [];
    for (let i = 0; i < asc.length; i++) {
      if (i > 0) {
        const gap = (asc[i].plan.at - asc[i - 1].plan.end) / 1000;
        // Just before the newer request it separates the two from. The tools
        // the newer request carries are what ran during the quiet stretch.
        if (gap > 30) {
          const tools = (asc[i].o.tools || []).join(', ');
          lines.push({ t: asc[i].plan.at - 1, html: `<div class="det-row dim"><span class="when">${fmtTime(asc[i - 1].plan.end)}</span>`
            + '<span class="st i">idle</span><span class="val">' + fmtDur(gap)
            + (tools ? ` <span class="dim">\u00b7 tool ${esc(tools)}</span>` : '') + '</span></div>' });
        }
      }
      for (const e of asc[i].plan.events) {
        // Live stage lines (the running request's now-still-unfinished ones)
        // wear the client's own colour wash. The line
        // belongs to one request, so it opens the conversation on that request.
        const rid = asc[i].o.rid;
        lines.push({ t: e.at, html: `<div class="det-row${e.live ? ' live' : ''}"${rid ? ` data-action="log-jump" data-rid="${rid}" title="open this conversation at this request"` : ''}><span class="when">${fmtTime(e.at)}</span>`
          + `<span class="st ${e.k}">${STAGE[e.k]}</span><span class="val">${e.line}</span>${rid ? '<span class="jump jump-chip">\u2263 View</span>' : ''}</div>` });
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

/** Prefill speed of one finished request: prompt tokens over the time the
 *  backend spent reading them (dispatch to first token). Null when either half
 *  is missing — a response that never streamed has no first token to measure. */
function prefillOf(o) {
  const secs = o.prefill_secs ?? (o.stage?.first_token != null && o.stage?.dispatch != null
    ? (o.stage.first_token - o.stage.dispatch) / 1000 : null);
  const tok = o.prefill_tokens ?? o.prompt_tokens;
  if (!secs || !tok || secs <= 0) return null;
  return { tps: tok / secs, tok, secs };
}

/** The prefill speed a running conversation is being served at, from the newest
 *  finished request in it whose prefill was measurable. */
function prefillRate(unit) {
  const done = unit.filter(x => x.kind === 'done').sort((a, b) => (b.o.at_ms || 0) - (a.o.at_ms || 0));
  for (const d of done) {
    const p = prefillOf(d.o);
    if (p) return { txt: fmtRate(p.tps), tip: `measured on the last finished request here: ${fmtTok(p.tok)} tok in ${fmtSec1(p.secs)}` };
  }
  return null;
}

/** Generated tokens of a row, however the row is shaped. */
const rowTokens = row => row.kind === 'done' ? row.o.completion_tokens : row.kind === 'run' ? row.o.tokens : 0;

/** Running and waiting rows share one treatment wherever they are listed. */
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
    b.onclick = e => {
      // The priority control and the logs link sit in this header; folding on
      // top of using either would hide the very row the click was about.
      if (e.target.closest('.pri, .logs-link')) return;
      const k = b.dataset.key;
      if (S.collapsed.has(k)) S.collapsed.delete(k); else S.collapsed.add(k);
      saveCollapsed();
      render();
    };
  });
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
    // Waiting and finished rows come from the Overview payload (filtered
    // server-side by client, model and backend), so the priority filter — which
    // the server knows nothing about — has to be applied here too, or filtering
    // by `top` would still list every other session waiting in front of it.
    ...ov.queue.filter(w => prioSel(priOf(w)))
      .map(w => ({ kind: 'wait', t: now - w.waited_secs * 1000, s: w.session || null, o: w, key: keyOf(w, 'queue') })),
  ];
  const activeN = rows.length;
  if (all) for (const x of ov.recent) {
    if (prioSel(priOf(x))) rows.push({ kind: 'done', t: x.at_ms, s: x.session || null, o: x, key: keyOf(x, x.at_ms) });
  }
  return { rows, activeN };
}

/**
 * The list under the timeline: one block per conversation, one line per stage
 * event. Show: Active = running + waiting; All also lists recently finished
 * (live lines carry the client's colour wash). Prefill is size · seconds
 * (≈ while estimated), Gen time the generation seconds (for a running request:
 * since its first token), Cache the prefix-cache share of finished prompts.
 */
function renderRequests(st, ov) {
  const now = ov.now_ms;
  const { rows, activeN } = reqRows(st, ov, S.reqShow === 'all');
  // The counts that used to be a sentence above the list sit in the toggle —
  // the All count is the whole window even while Active is selected.
  segCount('#ov-req-show', 0, activeN);
  segCount('#ov-req-show', 1, activeN + ov.recent.length);
  for (const r of rows) r.plan = stagePlan(r, now);
  setHTML($('#ov-now'), renderDetails(rows));
  wireFold();
}

function renderBackendStats(ov) {
  setHTML($('#ov-backends'), ov.backends.length ? `<table>
    <tr><th>Backend</th><th class="num">Slots</th><th class="num">Requests</th><th class="num">Errors</th><th class="num">First token p50 / p95</th><th class="num">Gen tok/s</th><th class="num">Prefill tok/s</th><th class="num" title="share of prefill time lost re-reading evicted context">Waste</th><th>Output tok/s</th></tr>
    ${ov.backends.map(b => `<tr>
      <td><span class="sdot ${b.online ? 'on' : 'off'}"></span>${bkCell(b.label, true)} ${typeBadge(b.api_type)}</td>
      <td class="num">${b.online ? `${b.running}/${b.max}` : '<span class="st-bad">offline</span>'}</td>
      <td class="num">${fmtNum(b.requests)}</td>
      <td class="num ${b.errors ? 'st-bad' : 'dim'}">${fmtNum(b.errors)}</td>
      <td class="num">${fmtSecs(b.ttft_p50)} / ${fmtSecs(b.ttft_p95)}</td>
      <td class="num">${fmtRate(b.gen_p50)}</td>
      <td class="num">${fmtRate(b.prefill_tps)}</td>
      <td class="num">${wasteCell(b)}</td>
      <td class="sparkcell">${b.online ? spark(b.spark) : '<span class="dim">—</span>'}</td></tr>`).join('')}
    </table>` : '<div class="empty">No backends configured</div>');
}

/** Waste cell for a windowed group (backend/client): share of its prefill
 *  time lost to cache eviction, absolute time beside it; `—` when no
 *  prefill was measurable there (not zero — it is absence, not a clean run). */
const wasteCell = x => x.waste_pct == null ? '<span class="dim">—</span>'
  : `<span class="${x.waste_pct >= 15 ? 'st-bad' : x.waste_pct >= 5 ? 'st-warn' : 'st-ok'} mono">${x.waste_pct}%</span>`
  + (x.waste_secs >= 60 ? ` <span class="dim mono">${Math.round(x.waste_secs / 60)}m</span>` : '');

function renderClientStats(st, ov) {
  const now = c => [c.running ? `${c.running} running` : '', c.queued ? `<span class="st-warn">${c.queued} waiting</span>` : '']
    .filter(Boolean).join(' · ') || '<span class="dim">idle</span>';
  setHTML($('#ov-clients'), ov.clients.length ? `<table>
    <tr><th>Client</th><th>Now</th><th class="num">Requests</th><th>Share of slot time</th><th class="num">Wait p95</th><th class="num">First token p50</th><th class="num">Gen tok/s</th><th class="num">Tokens in / out</th><th class="num" title="share of this client's prefill time lost to cache eviction">Waste</th></tr>
    ${ov.clients.map(c => `<tr>
      <td>${swatch(c.id)}<span class="mono">${esc(c.id)}</span>${idBadges(c.id, { registered: c.registered }, st, true)}</td>
      <td>${now(c)}</td>
      <td class="num">${fmtNum(c.requests)}${c.errors ? ` <span class="st-bad" title="errors">(${c.errors} err)</span>` : ''}</td>
      <td><span class="share"><i class="${colorClass(c.id)}" style="width:${(c.share * 100).toFixed(1)}%"></i></span> ${Math.round(c.share * 100)}%</td>
      <td class="num">${fmtSecs(c.wait_p95)}</td>
      <td class="num">${fmtSecs(c.ttft_p50)}</td>
      <td class="num">${fmtRate(c.gen_p50)}</td>
      <td class="num">${fmtTok(c.prompt_tokens)} / ${fmtTok(c.completion_tokens)}</td>
      <td class="num">${wasteCell(c)}</td></tr>`).join('')}
    </table>` : `<div class="empty">No requests in the last ${winLabel(S.filters.win)}</div>`);
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
    const noText = c?.redact_log
        ? ' ' + redactBadge('No response text is stored for this backend — log entries keep status, size and timing')
        : '';
    const loaded = new Set(b.models_loaded);
    const hiddenHere = new Set(b.models_hidden);
    let html = `<tr class="bk-row ${open ? 'open' : ''}" data-action="toggle-backend" data-url="${esc(b.url)}">
      <td><span class="caret">${open ? '▾' : '▸'}</span><span class="sdot ${b.online ? 'on' : 'off'}"></span>${bkCell(b.url, true)}${b.disabled ? ' <span class="badge">disabled</span>' : ''}${c?.removed ? ' <span class="badge">removed — gone after a restart</span>' : ''}${noText}</td>
      <td>${typeBadge(b.api_type)}</td>
      <td class="num" title="${b.max_detected ? `${b.max} slots reported by the backend` : `${b.max} = settings.max_concurrent_per_backend`}">${b.active}/${b.max}<span class="meter"><i style="width:${pct}%"></i></span></td>
      <td class="num">${fmtNum(b.processed)}</td>
      <td>${b.loaded_known ? `<b>${loaded.size}</b> <span class="dim">loaded ·</span> ` : ''}${b.models_available.length} <span class="dim">available</span></td>
      <td>${opCell(b)}</td>${bkActions(b, c, editable)}</tr>`;
    if (open) {
      // Loaded first, then alphabetical — the order the queue uses.
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
  // Demo mode is screenshotted without a click: show both levels from the start.
  if (DEMO && !S.demoClientsSeeded) {
    S.demoClientsSeeded = true;
    for (const r of rows) if (r.reg) S.openClients.add(r.id);
  }
  const anyOpen = rows.some(r => S.openClients.has(r.id));
  $('#client-expand').textContent = anyOpen ? 'Collapse all' : 'Expand all';
  setHTML($('#client-list'), rows.length ? `<table>
    <tr><th>Client / key</th><th class="num">Queue</th><th class="num">Done</th><th class="num">Drop</th><th>Last used</th><th class="num"></th></tr>
    ${rows.map(r => clientRow(r, st) + keyRows(r)).join('')}
    </table>`
    : `<div class="empty">${S.clientFilter === 'unregistered'
      ? 'No unregistered ids are active.'
      : 'No clients yet.<br>Create one to hand out API keys.'}</div>`);
}

/** Level 1: one row per id, carrying what the old detail panel offered. */
function clientRow(r, st) {
  const c = r.reg, u = r.u, id = esc(r.id);
  const ipBlocked = !!u?.ip && st.blocked.ips.includes(u.ip);
  const open = S.openClients.has(r.id);
  // Windowed waste lives in the overview payload (the Overview poll caches it);
  // without a cached overview there is nothing to say about it.
  const w = (S.overview?.clients || []).find(x => x.id === r.id);
  const parts = c
    ? [c.note, `${c.keys.length} key${c.keys.length === 1 ? '' : 's'}`,
      u?.ip ? `last IP ${esc(u.ip)}` : '',
      w?.waste_pct != null ? `${w.waste_pct}% prefill lost to eviction` : '']
    : r.id.startsWith('unverified:')
      ? [`a request without a key claimed this registered name, so it queues apart from ${esc(r.id.slice('unverified:'.length))}`]
      : [u?.ip ? `last IP ${esc(u.ip)}` : '', 'identified by X-User-ID only, so any request can use this id'];
  const sub = parts.filter(Boolean).join(' · ');
  const prio = `
    <button class="btn sm iconly ${st.vip === r.id ? 'on-vip' : ''}" data-action="user" data-op="vip" data-id="${id}" title="Always served first" aria-label="VIP">★<span class="w"> VIP</span></button>
    <button class="btn sm iconly ${st.boost === r.id ? 'on-boost' : ''}" data-action="user" data-op="boost" data-id="${id}" title="Extra share of turns" aria-label="Boost">⚡<span class="w"> Boost</span></button>`;
  const ipBtn = !u?.ip ? '' : ipBlocked
    ? `<button class="btn sm" data-action="unblock" data-kind="ip" data-value="${esc(u.ip)}">Unblock IP</button>`
    : `<button class="btn sm danger" data-action="user" data-op="block-ip" data-id="${id}" title="Block ${esc(u.ip)}">Block IP</button>`;
  const actions = c
    ? `${prio}
       <button class="btn sm" data-action="add-key" data-name="${id}">+ Key</button>
       <button class="btn sm" data-action="edit-client" data-name="${id}">Edit</button>
       <button class="btn sm ${c.disabled ? '' : 'danger'}" data-action="toggle-client" data-name="${id}">${c.disabled ? 'Enable' : 'Disable'}</button>
       ${ipBtn}
       <button class="btn sm danger" data-action="delete-client" data-name="${id}">Delete</button>`
    : `${prio}
       <button class="btn sm primary" data-action="register" data-id="${id}" ${NAME_RE.test(r.id) ? '' : 'disabled title="Not a valid client name"'}>Register</button>
       ${u?.blocked && !ipBlocked
         ? `<button class="btn sm" data-action="user" data-op="unblock" data-id="${id}">Unblock</button>`
         : `<button class="btn sm danger" data-action="user" data-op="block" data-id="${id}" title="Block this id">Block</button>`}
       ${ipBtn}`;
  return `<tr class="clickable ${open ? 'open' : ''}" data-action="toggle-keys" data-name="${id}">
      <td><span class="caret">${c ? (open ? '▾' : '▸') : ''}</span>
        <span class="client-name"><span class="mono">${id}</span>${idBadges(r.id, rowFlags(r), st, true)}
        ${c?.redact_log ? REDACT_BADGE : ''}</span>
        <div class="client-sub trunc">${sub}</div></td>
      <td class="num">${queueShort(u)}</td>
      <td class="num">${fmtNum(u?.done ?? 0)}</td>
      <td class="num">${fmtNum(u?.dropped ?? 0)}</td>
      <td class="dim nowrap">${c ? fmtAgo(lastUsed(c)) : '<span class="dim">—</span>'}</td>
      <td class="actions"><span class="btn-group">${actions}</span></td></tr>`;
}

/** Says why a record has no body text: switched off, not missing. */
const redactBadge = title => `<span class="badge warn" title="${title}">no text</span>`;
const REDACT_BADGE = redactBadge('No request/response text is stored — the log entry keeps user, model, backend, status, size, session and timing');

/** Level 2: the client's keys, while its own row is open. */
function keyRows(r) {
  if (!r.reg || !S.openClients.has(r.id)) return '';
  const ks = r.reg.keys;
  if (!ks.length) return `<tr class="key-row"><td colspan="6" class="keycell">
    No keys — this client cannot authenticate yet.
    <button class="btn sm" data-action="add-key" data-name="${esc(r.id)}">+ Key</button></td></tr>`;
  return ks.map(k => {
    const shown = S.revealed.has(k.id);
    const kid = esc(k.id);
    const word = k.label ? esc(k.label) : '<span class="dim">no label</span>';
    return `<tr class="key-row">
      <td class="keycell" colspan="5"><span class="keyval">${esc(shown ? k.key : maskKey(k.key))}</span>
        <button class="iconbtn" data-action="reveal" data-id="${kid}" title="${shown ? 'Hide' : 'Show'} the key" aria-label="${shown ? 'Hide' : 'Show'} the key">${shown ? '◉' : '👁'}</button>
        <button class="iconbtn" data-action="copy-key" data-id="${kid}" title="Copy the key" aria-label="Copy the key">⧉</button>
        <b>${word}</b>${k.redact_log ? ' ' + REDACT_BADGE : ''}
        <div class="client-sub">created ${fmtDate(k.created_at_ms)} · last used ${fmtAgo(k.last_used_ms)}</div></td>
      <td class="actions"><span class="btn-group">
        <button class="btn sm" data-action="edit-key" data-id="${kid}">Edit</button>
        <button class="btn sm danger" data-action="revoke-key" data-id="${kid}">Delete</button>
      </span></td></tr>`;
  }).join('');
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

/** One round: the IN and OUT log events that share a rid, or a standalone
 *  event (CTL, refused-before-dispatch OUT). Newest first, as `logs` is. */
function logRounds(logs) {
  const byRid = new Map();
  const rounds = [];
  for (const l of logs) {
    const slot = l.rid && byRid.get(l.rid);
    if (slot && !slot[l.dir === 'IN' ? 'in' : 'out'] && l.dir !== 'CTL') {
      slot[l.dir === 'IN' ? 'in' : 'out'] = l;
      continue;
    }
    const r = { in: l.dir === 'IN' ? l : null, out: l.dir === 'OUT' ? l : null, ctl: l.dir === 'CTL' ? l : null, solo: null };
    if (r.ctl || !l.rid) { r.solo = l; r.in = r.out = null; }
    if (l.rid && !r.ctl) byRid.set(l.rid, r);
    rounds.push(r);
  }
  return rounds;
}

/** The compact IN segment text: `POST /v1/chat/completions · 677 KB`. */
function inSeg(info) {
  const m = /^(\S+) (\S+) body=(\d+)B$/.exec(info);
  return m ? `${m[1]} ${m[2]} · ${fmtBytes(+m[3])}` : info;
}

/** The compact OUT segment text: `200 · 12.3 KB` (else the info note). */
function outSeg(info) {
  const m = /^\S+ \S+ -> (\d{3}) resp=(\d+)B$/.exec(info);
  return m ? `${m[1]} · ${fmtBytes(+m[2])}` : info;
}

function roundSegs(r) {
  if (r.ctl) return `<span class="rl ctl"><span class="ar">◆</span>${esc(fmtInfo(r.ctl.info))}</span>`;
  const segs = [];
  if (r.solo) return `<span class="rl ${r.solo.dir === 'IN' ? 'in' : 'out'}"><span class="ar">${r.solo.dir === 'IN' ? '→' : '←'}</span>${esc((r.solo.dir === 'IN' ? inSeg : outSeg)(r.solo.info))}</span>`;
  if (r.in) segs.push(`<span class="rl in"><span class="ar">→</span>${esc(inSeg(r.in.info))}</span>`);
  if (r.out) {
    // Red only on a failed status — never on a byte count that happens to
    // start with 4xx/5xx digits.
    const st = /-> (\d{3})/.exec(r.out.info);
    const bad = st ? +st[1] >= 400 : /\b[45]\d\d\b|drop|fail|error|timeout|refus/i.test(r.out.info);
    segs.push(`<span class="rl out${bad ? ' err' : ''}"><span class="ar">←</span>${esc(outSeg(r.out.info))}</span>`);
  }
  if (r.in && !r.out) segs.push('<span class="rl out wait"><span class="ar">←</span>awaiting response</span>');
  // One chip for the round: the text was switched off, not lost.
  if (r.in?.redacted || r.out?.redacted || r.solo?.redacted)
    segs.push(REDACT_BADGE);
  return segs.join(' ');
}

function logTable(rounds) {
  if (!rounds.length) return '<div class="empty">No matching log events</div>';
  const off = captureOff();
  const ev = r => r.in || r.out || r.solo;
  return `<table class="logs">
    <tr><th>Time</th><th>Client</th><th>Model</th><th>Backend</th><th>Round</th></tr>
    ${rounds.map(r => { const e = ev(r); const rid = r.in?.rid ?? r.out?.rid;
      return `<tr class="clickable" ${rid ? `data-action="log-round" data-rid="${rid}"` : `data-action="log-detail" data-seq="${e.seq}"`}>
      <td class="time">${fmtTime(e.at_ms)}</td>
      <td class="mono">${esc(e.user)}</td>
      <td class="trunc ${e.model ? '' : 'none'}">${esc(e.model ?? '—')}</td>
      <td class="dim ${e.backend ? '' : 'none'}">${e.backend ? bkCell(e.backend) : '—'}</td>
      <td class="round"><div class="rline">${roundSegs(r)}${off && rid ? NOCAP_BADGE : ''}</div></td></tr>`; }).join('')}
    </table>`;
}

// The other view of the same ring: one row per conversation. A chat session
// makes twenty calls, and twenty rows that each repeat part of the previous one
// hide that; a control action and traffic with no session belong to no
// conversation, so they stand alone in both views.
const storedLogGroup = () => {
  try { return localStorage.getItem('omq.logGroup') === 'request' ? 'request' : 'session'; } catch { return 'session'; }
};

const listSome = set => {
  const v = [...set];
  return v.length ? esc(v[0]) + (v.length > 1 ? ` +${v.length - 1}` : '') : null;
};

/** Rounds grouped into conversations: same client + session tag is one row. */
function logConvs(rounds) {
  const items = [];
  const byKey = new Map();
  const num = (re, s) => { const m = re.exec(s || ''); return m ? +m[1] : 0; };
  for (const r of rounds) {
    const e = r.in || r.out || r.solo;
    if (r.ctl || !e.session) { items.push({ kind: 'solo', at: e.at_ms, r }); continue; }
    const key = `${e.user}#${e.session}`;
    let g = byKey.get(key);
    if (!g) {
      g = { kind: 'conv', key, user: e.user, session: e.session, label: e.session_label || null,
        models: new Set(), backends: new Set(), reqs: 0, inBytes: 0, outBytes: 0, errors: 0,
        first: e.at_ms, at: e.at_ms, rid: e.rid ?? null, lastStatus: null,
        statusAt: 0, redacted: false, waiting: false };
      byKey.set(key, g);
      items.push(g);
    }
    g.first = Math.min(g.first, e.at_ms);
    if (e.at_ms >= g.at) { g.at = e.at_ms; if (e.rid != null) g.rid = e.rid; }
    if (e.model) g.models.add(e.model);
    if (e.backend) g.backends.add(e.backend);
    if (e.redacted) g.redacted = true;
    if (r.in) { g.reqs++; g.inBytes += num(/body=(\d+)B/, r.in.info); }
    if (r.out) {
      g.outBytes += num(/resp=(\d+)B/, r.out.info);
      const st = num(/-> (\d{3})/, r.out.info);
      if (st && r.out.at_ms >= g.statusAt) { g.lastStatus = st; g.statusAt = r.out.at_ms; if (st >= 400) g.errors++; }
    }
    if (r.in && !r.out) g.waiting = true;
  }
  items.sort((a, b) => b.at - a.at);
  return items;
}

/** The Activity cell: what the session sent and got, plus what it is doing. */
function convSegs(g) {
  return `<span class="rl in"><span class="ar">\u2192</span>${g.reqs} req \u00b7 ${fmtBytes(g.inBytes)}</span>`
    + ` <span class="rl out${g.errors ? ' err' : ''}"><span class="ar">\u2190</span>${g.lastStatus ?? '…'} \u00b7 ${fmtBytes(g.outBytes)}</span>`
    + (g.waiting ? ' <span class="badge q">waiting</span>' : '')
    + (g.errors ? ` <span class="badge bad">${g.errors} failed</span>` : '')
    + (g.redacted ? ' ' + REDACT_BADGE : '')
    + (captureOff() ? ' ' + NOCAP_BADGE : '');
}

/** A session as the client named it (`#cache-warmup`), else the tag the store
 *  keys on — the label is what makes one row per conversation recognisable. */
const sessTag = g => `<span class="sess" title="session tag ${esc(g.session)} — this is what the log keys on">#${esc(g.label || g.session)}</span>`;

function convTable(rounds) {
  const items = logConvs(rounds);
  if (!items.length) return '<div class="empty">No matching log events</div>';
  const rows = items.map(it => {
    if (it.kind === 'solo') {
      const e = it.r.in || it.r.out || it.r.solo;
      return `<tr class="clickable" data-action="log-detail" data-seq="${e.seq}">
        <td class="time">${fmtTime(e.at_ms)}</td>
        <td><span class="mono">${esc(e.user)}</span> <span class="dim">\u00b7 no session</span></td>
        <td class="trunc none">\u2014</td><td class="dim none">\u2014</td>
        <td class="round"><div class="rline">${roundSegs(it.r)}</div></td></tr>`;
    }
    const g = it, model = listSome(g.models), bk = listSome(g.backends);
    return `<tr class="clickable" data-action="conv-open" data-session="${esc(g.session)}" data-user="${esc(g.user)}"${g.rid != null ? ` data-rid="${g.rid}"` : ''}
      title="${esc(g.user)} \u00b7 ${fmtTime(g.first)} \u2192 ${fmtTime(g.at)} \u00b7 ${g.reqs} requests">
      <td class="time">${fmtTime(g.at)}</td>
      <td><span class="mono">${esc(g.user)}</span>${sessTag(g)}</td>
      <td class="trunc ${model ? '' : 'none'}">${model ?? '—'}</td>
      <td class="dim ${bk ? '' : 'none'}">${bk ?? '—'}</td>
      <td class="round"><div class="rline">${convSegs(g)}</div></td></tr>`;
  }).join('');
  return `<table class="logs">
    <tr><th>Time</th><th>Conversation</th><th>Model</th><th>Backend</th><th>Activity</th></tr>${rows}</table>`;
}

// When the log file could not be opened at boot nothing is captured, so a row
// has no conversation to open. The list still shows the requests (they come
// from the live ring buffer), and saying so beats a bare “no such conversation”.
const NOCAP_BADGE = '<span class="badge warn" title="body capture is off — no conversation is stored for this request">not captured</span>';
const captureOff = () => S.capture && S.capture.log === false;

function renderLogs() {
  const q = $('#log-filter').value.trim().toLowerCase();
  const dir = $('#log-dir').value;
  const rounds = logRounds(S.logs).filter(r =>
    (dir === '' || (dir === 'awaiting' ? r.in && !r.out : r.ctl)) &&
    (!q || [r.in?.user, r.in?.model, r.in?.backend, r.in?.session, r.in?.session_label, r.out?.user, r.out?.model,
      r.out?.backend, r.out?.session, r.out?.session_label,
      r.in?.info, r.out?.info, r.solo?.user, r.solo?.model, r.solo?.backend, r.solo?.session, r.solo?.info]
      .some(v => v && String(v).toLowerCase().includes(q))));
  const warn = captureOff()
    ? `<div class="capnote">Body capture is <b>off</b> — these requests are listed from memory, but their conversations are not written to disk, so a row has nothing to open. Set a writable <b>Request log path</b> in Settings and restart to capture them${S.capture.path ? ` (current path: <code>${esc(S.capture.path)}</code>)` : ''}.</div>`
    : '';
  setHTML($('#logs'), warn + (S.logGroup === 'request' ? logTable(rounds) : convTable(rounds)));
}

// --- Conversation viewer
//
// Full-screen dialog over one conversation. Every turn is a block on one
// timeline carrying the timestamps the proxy measured (queue, dispatch, first
// token, end), the quiet stretch between two turns is named by the tools that
// ran in it, and Raw/Formatted, download, reload and request navigation
// (⏮ ‹ › ⏭) work over the whole conversation instead of one request round.
// The index arrives without body text — a conversation resends its whole
// history every turn, so sending text with it would refetch hundreds of
// kilobytes per navigation step — and text is fetched per expanded turn.

const storedLv = () => {
  try {
    const v = JSON.parse(localStorage.getItem('omq.lv') || '{}');
    return { fmt: v.fmt === 'raw' ? 'raw' : 'fmt', rail: !!v.rail,
      inc: v.inc !== false, railW: lvRailClamp(v.railW), auto: !!v.auto };
  } catch { /* storage blocked */ }
  return { fmt: 'fmt', rail: false, inc: true, railW: LV_RAIL_DEF, auto: false };
};
const saveLv = () => { try { localStorage.setItem('omq.lv', JSON.stringify(S.lv)); } catch { /* storage blocked */ } };
/* The jump tree is a side column you may want wide (tool names, message text) or
   narrow (a glance beside the log), so its width is yours and remembered. The
   dialog is full-viewport, so the ceiling keeps a strip of log readable. */
const LV_RAIL_DEF = 292, LV_RAIL_MIN = 200;
const lvRailMax = () => Math.max(LV_RAIL_MIN + 80, Math.round(window.innerWidth * 0.62));
const lvRailClamp = w => {
  const n = Math.round(+w);
  return Number.isFinite(n) && n >= LV_RAIL_MIN && n <= 900 ? Math.min(n, lvRailMax()) : 0;
};
// What is stored is what a previous session wrote: a value that is not a sane
// width falls back to the default instead of poisoning the CSS variable.
const lvRailW = () => lvRailClamp(S.lv && S.lv.railW) || LV_RAIL_DEF;
function lvApplyRail() {
  const shell = $('#dlg-log .lv-shell');
  if (shell) shell.style.setProperty('--lv-w', lvRailW() + 'px');
  const grip = $('#lv-grip');
  if (grip) {
    grip.hidden = !(S.lv.rail && !isPhone());
    grip.setAttribute('aria-valuenow', String(lvRailW()));
    grip.setAttribute('aria-valuetext', `${lvRailW()} pixels wide`);
  }
}
function lvSetRail(w) {
  const next = lvRailClamp(w);
  if (!next || next === lvRailW()) return false;
  S.lv.railW = next;
  lvApplyRail();
  return true;
}
S.lv = storedLv();

/** Viewer state. `open` is the one fold for a request: the tree caret and the
 *  turn header fold the same block, in either direction. `closedRuns` holds the
 *  run ordinals folded shut; whether a request shows only its increment or the
 *  whole re-sent prompt is the one global `S.lv.inc` switch, not per-request. */
const LV = { conv: null, rid: null, open: new Set(), closedRuns: new Set(), focusMsg: null, content: new Map(),
  msgFold: new Map() };
let lvToken = 0;

const tryJson = s => { try { return JSON.parse(s); } catch { return undefined; } };

const lvTurns = () => (LV.conv && LV.conv.turns) || [];
const lvIdxOf = rid => lvTurns().findIndex(t => t.rid === rid);
/** How many messages a turn's request carried (0 when the capture was cut
 *  before them — then the viewer cannot tell which messages are new). */
const lvMsgCount = t => (t && t.msgs ? t.msgs.length : 0);

/** How many of turn i's messages an earlier request already sent — the prefix a
 *  turn does not repeat in its body. 0 when there is nothing comparable: the
 *  first turn, or a request whose message count did not grow. */
function lvHidden(i) {
  const turns = lvTurns();
  const prev = i > 0 ? lvMsgCount(turns[i - 1]) : 0;
  return prev > 0 && prev < lvMsgCount(turns[i]) ? prev : 0;
}

/** Open the viewer on a conversation: by request id (a row) or by the client
 *  and conversation tag (a conversation header). Either way the server picks
 *  the conversation, because it owns the keys. */
async function openConv({ rid, session, user } = {}) {
  const q = new URLSearchParams();
  if (rid) q.set('rid', rid);
  if (session) q.set('session', session);
  if (user) q.set('user', user);
  let conv;
  try { conv = await api('GET', 'conv?' + q.toString()); }
  catch (e) { return toast(e.message, false); }
  const resuming = LV.conv && LV.conv.key === conv.key;
  LV.conv = conv;
  LV.rid = rid || (resuming && LV.rid) || conv.turns.at(-1)?.rid || null;
  LV.open = new Set(LV.rid != null ? [LV.rid] : []);
  LV.closedRuns = new Set();
  LV.focusMsg = null;
  LV.content = new Map();
  LV.msgFold = new Map();
  $('#dlg-log [data-title]').textContent =
    `${conv.model || 'traffic'} · ${conv.user}${conv.backend ? ` → ${bkName(conv.backend)}` : ''}`;
  $('#dlg-log [data-sub]').textContent = lvSubline(conv);
  $$('input[name="lv-fmt"]').forEach(r => { r.checked = S.lv.fmt === r.value; });
  $('#lv-inc').checked = S.lv.inc;
  $('#lv-auto').checked = S.lv.auto;
  const dlg = $('#dlg-log');
  if (!dlg.open) dlg.showModal();
  await lvDraw({ scroll: true });
  lvPrefetch();
  lvAutoStart();
}

/** The viewer header's second line: conversation id, its name when it has
 *  one, request count and span — rebuilt on refresh so a growing conversation
 *  says so without a reload. */
function lvSubline(conv) {
  const sub = [`#${conv.session || conv.key}`];
  if (conv.session_label) sub.push(`#${conv.session_label}`);
  sub.push(`${conv.turns.length} ${conv.turns.length === 1 ? 'request' : 'requests'}`);
  if (conv.turns.length) sub.push(`${fmtTime(conv.start_ms)} → ${fmtTime(conv.end_ms)}`);
  if (conv.redacted) sub.push('no text');
  return sub.join(' · ');
}

/** The collapsed lines want a word from every turn's text (and the run lines
 *  want every finish), so all turn texts are fetched in the background after
 *  the first paint — newest first, a few in flight, repaints throttled. A
 *  redacted turn has no text to fetch. */
let lvPrefetchKey = null;
async function lvPrefetch() {
  if (!LV.conv) return;
  const key = lvPrefetchKey = LV.conv.key;
  const todo = lvTurns().filter(t => !t.redacted && !LV.content.has(t.rid)).reverse();
  if (!todo.length) return;
  let timer = 0;
  const paint = () => {
    if (lvPrefetchKey !== key) return;
    clearTimeout(timer);
    timer = setTimeout(() => { if (lvPrefetchKey === key) lvDraw({ scroll: false }); }, 250);
  };
  const worker = async list => {
    for (const t of list) {
      if (lvPrefetchKey !== key) return;
      await lvLoad(t.rid);
      paint();
    }
  };
  const N = Math.min(6, todo.length);
  await Promise.all(Array.from({ length: N }, (_, w) => worker(todo.filter((_, i) => i % N === w))));
}

/** A log event with no proxied request behind it (a control action): the same
 *  dialog with a single turn, its text taken from the event ring. */
async function openSeq(seq) {
  let ev;
  try { ev = await api('GET', `logs/${seq}`); } catch (e) { return toast(e.message, false); }
  LV.conv = {
    key: 'ctl:' + seq, user: ev.user || '\u2014', model: 'control', session: null,
    start_ms: ev.at_ms, end_ms: ev.at_ms,
    turns: [{ rid: seq, at_ms: ev.at_ms, path: ev.info || 'event', tl: { end: ev.at_ms }, redacted: !!ev.redacted }],
  };
  LV.rid = seq;
  LV.open = new Set([seq]);
  LV.closedRuns = new Set();
  LV.focusMsg = null;
  LV.content = new Map([[seq, { req: null, resp: ev.content ?? null, reqObj: null }]]);
  $('#dlg-log [data-title]').textContent = `control \u00b7 ${ev.user}`;
  $('#dlg-log [data-sub]').textContent = `${fmtTime(ev.at_ms)} \u00b7 ${ev.info}`;
  $$('input[name="lv-fmt"]').forEach(r => { r.checked = S.lv.fmt === r.value; });
  const dlg = $('#dlg-log');
  if (!dlg.open) dlg.showModal();
  await lvDraw({ scroll: true });
}

/** Captured text of one turn, fetched once and kept for the session. */
async function lvLoad(rid) {
  if (LV.content.has(rid)) return LV.content.get(rid);
  const one = async dir => {
    try { return (await api('GET', `conv/content?rid=${rid}&dir=${dir}`)).content ?? null; }
    catch { return null; }
  };
  const c = { req: await one('in'), resp: await one('out') };
  c.reqObj = c.req ? tryJson(c.req) || salvageChat(c.req) : null;
  LV.content.set(rid, c);
  return c;
}

/** The turn's own measured timeline, in the same words Details uses. */
function lvStages(t) {
  const tl = t.tl || {};
  const out = [];
  if (tl.queue != null && tl.dispatch != null) out.push(`queued ${fmtDur((tl.dispatch - tl.queue) / 1000)}`);
  else if (tl.queue != null) out.push(`queued ${fmtDur(((tl.end || nowish()) - tl.queue) / 1000)}`);
  if (tl.dispatch != null) {
    const to = tl.first_token ?? tl.end;
    if (to != null) out.push(`prefill ${fmtSec1((to - tl.dispatch) / 1000)}`);
    if (tl.first_token == null && tl.end == null) out.push('prefilling');
  }
  if (tl.first_token != null && tl.end != null) {
    out.push(`gen ${fmtSec1((tl.end - tl.first_token) / 1000)}`);
    if (t.completion_tokens != null && tl.end > tl.first_token)
      out.push(`${(t.completion_tokens / ((tl.end - tl.first_token) / 1000)).toFixed(1)} t/s`);
  }
  if (t.prompt_tokens != null) out.push(`prompt ${fmtTok(t.prompt_tokens)} tok`);
  if (t.waste_secs) out.push(`<span class="st-bad">waste ${fmtSec1(t.waste_secs)}</span>`);
  if (tl.end == null) out.push('<b>running</b>');
  return out.filter(Boolean).join(' · ');
}
const nowish = () => (S.overview && S.overview.now_ms) || Date.now();

/** A gap this long between one answer ending and the next request arriving is
 *  worth a line of its own: that is a tool (or a human) working, not network
 *  turnaround. The gap is a length, so it is written without the `+` a moment
 *  carries, and named by what ran in it — the turn's own tool results are
 *  exactly what the agent did while the proxy was idle. */
const LV_IDLE_MS = 5_000;

function lvGap(t, prev) {
  const end = prev && prev.tl && prev.tl.end;
  if (!end || t.at_ms - end < LV_IDLE_MS) return '';
  const tools = (t.msgs || []).filter(m => m.name).map(m => m.name);
  const what = tools.length ? `tool ${tools.join(', ')}` : 'agent side';
  return `<div class="lv-gap"><span class="dot">◌</span>idle ${fmtDur((t.at_ms - end) / 1000)} · ${esc(what)} (${fmtTime(end)} → ${fmtTime(t.at_ms)})</div>`;
}

/**
 * The first request of a conversation owns the absolute clock; every one after
 * it counts continuously from the conversation's own start, the way `dmesg -H`
 * counts from the boot it describes — a turn is a point on one line, so turn 3
 * never repeats turn 2's number. Anything that is a length rather than a moment
 * (a response's duration, an idle gap, the window that produced a message)
 * stays a length. The absolute pair of any delta stays one hover away.
 */
function lvSpan(t, first) {
  const end = t.tl?.end;
  if (first == null)
    return { txt: `${fmtTime(t.at_ms)}${end != null ? ` → ${fmtTime(end)}` : ''}`, tip: '' };
  const on = ms => `+${fmtDur((ms - first) / 1000)}`;
  return {
    txt: on(t.at_ms) + (end != null ? ` → ${on(end)}` : ''),
    tip: `arrived ${fmtTime(t.at_ms)}${end != null ? ` · finished ${fmtTime(end)}` : ''}`
      + ` · ${fmtDur((t.at_ms - first) / 1000)} into this conversation`,
  };
}

const lvSec = ({ cls, title, info, tip, id }, html) =>
  `<section class="lv-sec"${id ? ` id="${id}"` : ''}><h3 class="lv-sec-h ${cls}"${tip ? ` title="${esc(tip)}"` : ''}><span class="ar">${cls === 'req' ? '→' : '←'}</span>${esc(title)}${info ? ` · ${esc(info)}` : ''}</h3>${html}</section>`;

// --- Derived turn facts and runs ------------------------------------------
//
// The collapsed line and the run boundaries need what the turn index does not
// carry: what the response finished with, a word of the increment, and whether
// its tool calls came back bad. All of it lives in the captured bodies, so it
// is derived once per turn as its text arrives (prefetched after the first
// paint) and cached on the content entry; without text the tool count still
// comes from the message metadata.

const lvWords = (s, n = 7) => {
  const words = String(s || '').replace(/\s+/g, ' ').trim().split(' ').filter(Boolean);
  return words.length ? words.slice(0, n).join(' ') + (words.length > n ? '…' : '') : '';
};

function lvFacts(i) {
  const t = lvTurns()[i];
  if (!t) return { finish: null, words: '', pvU: '', pvT: '', tCalls: 0, tBad: false };
  const c = LV.content.get(t.rid);
  if (c && c.facts) return c.facts;
  const hid = lvHidden(i);
  const f = { finish: null, words: '', pvU: '', pvT: '', tCalls: 0, tBad: false };
  // A turn that did not grow the message list has no measurable increment
  // (a retry, a no-growth step): guessing from the whole body would invent a
  // preview and a badge. Only the first turn's increment is all of it.
  const grew = i === 0 || hid > 0;
  const fromMeta = () => {
    if (grew) f.tCalls = (t.msgs || []).slice(hid).reduce((n, m) => n + (m.calls ? m.calls.length : 0), 0);
  };
  if (!c) { fromMeta(); return f; }
  if (c.resp != null) {
    const st = streamState(c.resp) || lvPlainResp(c.resp);
    f.finish = (st && st.finish) || null;
    // The response's own opening words: a one-request conversation has no
    // re-sent assistant history, so without this the rail never shows what
    // the model actually answered — only how the stream ended.
    f.words = lvWords((st && st.text) || '');
  }
  const msgs = c.reqObj && c.reqObj.messages;
  if (msgs && grew) {
    for (const m of msgs.slice(hid)) {
      if (m.role === 'user' && !f.pvU) f.pvU = lvWords(msgText(m.content, m.images));
      if (m.role === 'assistant' && !f.pvT) f.pvT = lvWords(m.reasoning_content || m.reasoning || msgText(m.content));
      f.tCalls += (m.tool_calls || []).length;
      if (m.role === 'tool' && /exit code: -?[1-9]\d*/i.test(msgText(m.content))) f.tBad = true;
    }
  } else if (!msgs) fromMeta();
  return (c.facts = f);
}

/** The opening signature of a turn's answer text — enough to recognize the
 *  very same reply when the next request re-sends it as a message. */
function lvAnswerSig(t) {
  const c = t && LV.content.get(t.rid);
  if (!c || c.resp == null) return '';
  if (c.ansSig != null) return c.ansSig;
  const st = streamState(c.resp) || lvPlainResp(c.resp);
  return (c.ansSig = lvWords((st && st.text) || ''));
}

/** Runs in index ranges: the requests from the previous final answer up to the
 *  next `finish: stop`. A turn whose finish is unknown cannot close a run — it
 *  merges forward — and a conversation with nothing to split renders flat,
 *  exactly as before runs existed. */
function lvRuns() {
  // The incremental view is about one request's moment: the run grouping is
  // the whole session's story and waits for the whole-prompt view. The answer
  // to the request stays visible — hide it and a single-request conversation
  // shows no assistant words anywhere.
  if (S.lv.inc) return [];
  const turns = lvTurns(); const runs = []; let s = 0;
  for (let i = 0; i < turns.length; i++)
    if (lvFacts(i).finish === 'stop') { runs.push({ a: s, b: i }); s = i + 1; }
  if (s < turns.length) runs.push({ a: s, b: turns.length - 1 });
  // A run of one request groups nothing and reads as a second numbering
  // system competing with #N — those requests stand on their own, and only
  // spans of two or more get a group line.
  const kept = runs.filter(r => r.b > r.a);
  return kept.length ? kept : [];
}
const lvRunOf = (runs, i) => runs.findIndex(r => i >= r.a && i <= r.b);

function lvRunInfo(runs, k) {
  const turns = lvTurns(); const { a, b } = runs[k];
  const first = turns[a], last = turns[b];
  const end = last.tl?.end ?? LV.conv?.end_ms ?? last.at_ms;
  const info = { a: a + 1, b: b + 1, n: b - a + 1,
    dur: end > first.at_ms ? fmtDur((end - first.at_ms) / 1000) : null, pv: null, calls: 0 };
  let think = null;
  for (let i = a; i <= b; i++) {
    const f = lvFacts(i); info.calls += f.tCalls;
    if (!info.pv && f.pvU) info.pv = { cls: 'u', txt: f.pvU };
    if (!think && f.pvT) think = { cls: 't', txt: f.pvT };
  }
  if (!info.pv) info.pv = think;
  return info;
}

/** Capture switched off is a decision, not a gap: say so and show what was kept. */
const lvOff = () => '<div class="lv-off">capture turned off — no request or response text is stored for this client, key or backend. Its timeline, sizes and message roles are still recorded.</div>';

/** What a turn re-sent that an earlier request already delivered, as one line:
 *  how many, where they first appeared, and what they were (roles, size). The
 *  conversation is one view, so by default they show only on the turn that first
 *  sent them; the Show-incremental switch decides everywhere whether a later
 *  turn names them (bright strip only on the increment) or shows them dimmed. */
function lvCarry(i) {
  const turns = lvTurns();
  const hidden = lvHidden(i);
  if (!hidden) return null;
  const held = ((turns[i] || {}).msgs || []).slice(0, hidden);
  const counts = {};
  let chars = 0;
  for (const m of held) { counts[m.role] = (counts[m.role] || 0) + 1; chars += m.chars || 0; }
  const roles = ['system', 'user', 'assistant', 'tool'].concat(Object.keys(counts))
    .filter((r, k, all) => counts[r] && all.indexOf(r) === k).map(r => `${r} ${counts[r]}`).join(' · ');
  // The request that first carried them: the first one whose body reached that
  // many messages.
  let from = i;
  for (let k = 0; k <= i; k++) if (lvMsgCount(turns[k]) >= hidden) { from = k; break; }
  return { hidden, from, roles, bytes: chars ? `≈${fmtBytes(chars)}` : null };
}

const lvCarryTip = c => c
  ? `${c.hidden} ${c.hidden === 1 ? 'message' : 'messages'} held from request ${c.from + 1}`
    + [c.roles, c.bytes].filter(Boolean).map(x => ` · ${x}`).join('')
  : '';

function lvMsgsHtml(obj, i, rid) {
  const msgs = (obj && obj.messages) || [];
  const toolNames = new Map();
  for (const m of msgs) for (const tc of m.tool_calls || []) if (tc.id) toolNames.set(tc.id, tc.function?.name ?? tc.name);
  // A turn carrying exactly as many messages as the previous one added nothing
  // comparable — show them all rather than hide the whole block.
  const carry = lvCarry(i);
  // Incremental mode renders the re-sent prefix not at all; full mode renders
  // it dimmed. The strip says which rows the request actually added.
  const from = carry && S.lv.inc ? carry.hidden : 0;
  const mid = (k) => (rid != null ? `lv-m-${rid}-${k}` : undefined);
  const cls = k => !carry ? '' : (from + k < carry.hidden ? ' m-held' : ' m-new');
  const body = msgs.slice(from).map((m, k) => msgBlock(m, toolNames, mid(from + k), cls(k))).join('');
  return body || (from ? '' : '<div class="empty">No messages in this request.</div>');
}

function lvReqHtml(t, c, i) {
  if (c.req == null) return (t.redacted ? lvOff() : '<div class="lv-note">No request text was stored for this request.</div>') + lvMsgChips(t);
  if (S.lv.fmt === 'raw')
    return (c.req.includes('[truncated:') ? '<div class="lv-note">Captured text was truncated by the content limit — Raw shows what was kept.</div>' : '') + rawView(c.req);
  if (!c.reqObj) return rawView(c.req);
  // No meta line: the section header carries method, path and size, and its
  // tooltip carries the parameters and the new/held split.
  return lvMsgsHtml(c.reqObj, i, t.rid);
}

function lvRespHtml(t, c) {
  if (c.resp == null)
    return (t.redacted ? lvOff() : `<div class="lv-note">${t.tl?.end != null ? 'No response text was stored for this request.' : 'No response recorded yet — still queued, running, or disconnected.'}</div>`)
      + lvRespChips(t);
  return S.lv.fmt === 'raw' ? rawView(c.resp) : formatContent(c.resp);
}

/** Without text the turn still lists what it held: one chip per message. */
function lvMsgChips(t) {
  const chips = (t.msgs || []).map(m =>
    `<div class="msg-slot ${ROLE_CLS[m.role] || 'user'}"><span class="role">${ROLE_LABEL[m.role] || 'MSG'}</span>`
    + `<span>${esc(m.name || (m.calls || []).join(', ') || '—')}</span><span>${fmtBytes(m.chars)}</span></div>`).join('');
  return chips;
}

function lvRespChips(t) {
  const bits = [];
  if (t.completion_tokens != null) bits.push(`${fmtTok(t.completion_tokens)} tok`);
  if (t.tl?.first_token != null && t.tl?.end != null)
    bits.push(`${fmtSec1((t.tl.end - t.tl.first_token) / 1000)} of generation`);
  // The header already carries the status; repeat it only where the chip is
  // the whole trace that remains.
  if (t.status != null && !t.redacted) bits.push(`HTTP ${t.status}`);
  if (!bits.length) return '';
  // "ASST" would be a guess on a request that never carried a chat body.
  if (!/\/chat\//.test(t.path || '')) return `<div class="lv-note">${esc(bits.join(' · '))}</div>`;
  return `<div class="msg-slot asst"><span class="role">${ROLE_LABEL.assistant}</span><span>${esc(bits.join(' · '))}</span></div>`;
}

async function lvTurnInner(t) {
  const i = lvIdxOf(t.rid);
  const c = await lvLoad(t.rid);
  const reqSize = t.req?.bytes != null ? fmtBytes(t.req.bytes) : null;
  const respSize = t.resp?.bytes != null ? fmtBytes(t.resp.bytes) : null;
  const carry = lvCarry(i);
  const reqTip = [c.reqObj ? reqMeta(c.reqObj) : null,
    carry ? `${(c.reqObj?.messages || []).length - carry.hidden} new · ${lvCarryTip(carry)}` : null
  ].filter(Boolean).join(' · ');
  return lvSec({ cls: 'req', title: 'Request', info: [t.method, t.path, reqSize].filter(Boolean).join(' '),
    tip: reqTip || null, id: `lv-q-${t.rid}` }, lvReqHtml(t, c, i))
    + lvSec({ cls: 'out', title: 'Response', info: [t.status ?? '—', respSize].filter(Boolean).join(' '),
      id: `lv-a-${t.rid}` }, lvRespHtml(t, c));
}

/** Render the whole conversation. `scroll` lands on the focused request. */
async function lvDraw({ scroll = false } = {}) {
  const tk = ++lvToken;
  const turns = lvTurns();
  const runs = lvRuns();
  for (const k of [...LV.closedRuns]) if (k >= runs.length) LV.closedRuns.delete(k);
  const parts = [];
  let prevRun = null, openRun = false;
  for (let i = 0; i < turns.length; i++) {
    const t = turns[i];
    const rk = lvRunOf(runs, i); // -1: a request that belongs to no group
    const gap = lvGap(t, turns[i - 1]);
    if (rk !== prevRun) { // a group starts or ends here
      parts.push(openRun ? gap + '</div>' : gap);
      openRun = rk !== -1;
      if (openRun) parts.push(lvRunHead(runs, rk), `<div class="run" data-run="${rk}">`);
      prevRun = rk;
    } else parts.push(gap);
    if (rk !== -1 && LV.closedRuns.has(rk)) continue; // folded run: its header says it all
    let sec = lvTurnHead(t, i);
    if (LV.open.has(t.rid)) sec += await lvTurnInner(t);
    parts.push(sec + '</section>');
  }
  if (openRun) parts.push('</div>');
  if (!turns.length) parts.push('<div class="empty">This conversation has no requests.</div>');
  const html = parts.join('');
  if (tk !== lvToken) return; // a newer click beat us
  setHTML($('#dlg-log [data-content]'), html);
  lvDrawRail();
  $('#lv-jump').classList.toggle('active', S.lv.rail);
  const i = lvIdxOf(LV.rid);
  $('#lv-first').disabled = $('#lv-prev').disabled = i <= 0;
  $('#lv-last').disabled = $('#lv-next').disabled = i < 0 || i >= turns.length - 1;
  if (scroll) lvScroll();
}

/** A problem worth the red tint: a failure the operator should notice. A
 *  client that walked away is not one — that row just carries no status. */
const lvProb = t => (t.status != null && t.status >= 400)
  || t.outcome === 'timeout' || t.outcome === 'backend_error';

/** One collapsed run: the range it covers, how long it ran, its first words,
 *  and how many tools it fired. A run is named by that range — `#1–#3`, the
 *  requests between two final answers — never by a second counter. */
function lvRunHead(runs, k) {
  const ri = lvRunInfo(runs, k);
  const shut = LV.closedRuns.has(k);
  return `<h3 class="run-h" data-action="lv-run" data-run="${k}" role="button" tabindex="0"
    title="a run: the requests between two final answers — #${ri.a}–#${ri.b}${ri.dur ? ` · ${ri.dur}` : ''}; click to ${shut ? 'open' : 'fold'} it">
    <span class="chev">${shut ? '▸' : '▾'}</span><span>#${ri.a}–#${ri.b}${ri.dur ? ` · ${ri.dur}` : ''}</span>
    <span class="pv">${shut && ri.pv ? `<span class="${ri.pv.cls}">${esc(ri.pv.txt)}</span>` : ''}</span>
    ${ri.calls ? `<span class="tb" title="${ri.calls} tool calls across this run">×${ri.calls}</span>` : ''}</h3>`;
}

/** The request line: `#N`, its measured moments, a word of its increment
 *  (blue user, green thinking), a tool badge (red when a call came back on a
 *  non-zero exit), and the sizes kept quiet on the right. A healthy status
 *  shows nothing; a problem tints the whole line through `.prob`. */
function lvTurnHead(t, i) {
  const f = lvFacts(i);
  const sp = lvSpan(t, i === 0 ? null : lvTurns()[0].at_ms);
  // The preview is the tease for a line you cannot see: once the request is
  // open, its own message lines spell out the same words right below, and a
  // header repeating the first of them just reads as a duplicate entry.
  const pv = !LV.open.has(t.rid) && (f.pvU || f.pvT)
    ? `<span class="pv"><span class="${f.pvU ? 'u' : 't'}">${esc(f.pvU || f.pvT)}</span></span>`
    : '<span class="pv"></span>';
  const tb = f.tCalls
    ? `<span class="tb${f.tBad ? ' bad' : ''}" title="${f.tCalls} ${f.tCalls === 1 ? 'tool call' : 'tool calls'} in this request${f.tBad ? ' — a result reported a non-zero exit code' : ''}">×${f.tCalls}</span>`
    : '';
  const szBits = [t.req?.bytes != null ? [`→ ${fmtBytes(t.req.bytes)}`, `request ${fmtBytes(t.req.bytes)}`] : null,
    t.resp?.bytes != null ? [`← ${fmtBytes(t.resp.bytes)}`, `response ${fmtBytes(t.resp.bytes)}`] : null].filter(Boolean);
  const szTxt = szBits.map(b => b[0]).join(' ');
  const tail = t.status == null ? '<span class="tail"><span class="dim">running</span></span>'
    : szTxt ? `<span class="tail sz" title="${szBits.map(b => b[1]).join(' · ')}">${szTxt}</span>`
      : '<span class="tail"></span>';
  return `<section class="turn${t.rid === LV.rid ? ' active' : ''}${lvProb(t) ? ' prob' : ''}" id="lv-t-${t.rid}" data-rid="${t.rid}">
    <h3 class="turn-h" data-action="lv-toggle" data-rid="${t.rid}" title="expand or collapse this request${t.status != null ? ` · ${t.status}${t.outcome && t.outcome !== 'ok' ? ` ${t.outcome}` : ''}` : ''}">
      <span class="chev">${LV.open.has(t.rid) ? '▾' : '▸'}</span>
      <span class="n">#${i + 1}</span>
      <span class="ts"${sp.tip ? ` title="${esc(sp.tip)}"` : ''}>${sp.txt}</span>
      <span>${lvStages(t)}</span>
      ${pv}${tb}
      ${t.redacted ? '<span class="badge warn" title="no body text is stored for this client, key or backend">no text</span>' : ''}
      ${tail}</h3>`;
}

function lvScroll() {
  const el = document.getElementById('lv-t-' + LV.rid);
  if (el) el.scrollIntoView({ block: 'start' });
}

/** Focus one request: expand it (leaving the others as they were), scroll to it. */
async function lvFocus(rid) {
  if (rid == null || lvIdxOf(rid) < 0) return;
  lvTrackLock = Date.now() + 700; // its own scroll lands the highlight it drew
  LV.rid = rid;
  LV.focusMsg = null;
  LV.open.add(rid);
  LV.closedRuns.delete(lvRunOf(lvRuns(), lvIdxOf(rid)));
  await lvDraw({ scroll: true });
}

/** A deep target inside a turn (the rail's RESP row): focus the turn, then
 *  land on that section and flash it — a plain jump lands on the turn header,
 *  which for a tall turn reads as "the top of the page". */
async function lvFocusSec(rid, side) {
  if (rid == null || lvIdxOf(rid) < 0) return;
  lvTrackLock = Date.now() + 700;
  LV.rid = rid;
  LV.focusMsg = null;
  LV.open.add(rid);
  LV.closedRuns.delete(lvRunOf(lvRuns(), lvIdxOf(rid)));
  await lvDraw({ scroll: false });
  const el = document.getElementById(`lv-${side}-${rid}`);
  if (!el) { lvScroll(); lvDrawRail(); return; }
  el.scrollIntoView({ block: 'center' });
  el.classList.add('flash');
  setTimeout(() => el.classList.remove('flash'), 1300);
  lvDrawRail();
}

/** The rail is where you are, not just what you click: whatever block sits at
 *  the top of the log lights its row, and the rail scrolls it into view — so a
 *  hand on the wheel never loses its place in a long session. Jump handlers
 *  light the target themselves, so their own scroll settles under a short
 *  lock instead of fighting the highlight it just placed. */
let lvTrackLock = 0;
function lvTrackRail() {
  if (!S.lv.rail || !LV || Date.now() < lvTrackLock) return;
  const body = document.querySelector('#dlg-log .lv-body');
  const rail = $('#lv-rail');
  if (!body || !rail || rail.hidden) return;
  const top = body.getBoundingClientRect().top;
  // Scrolled to the end means reading the last block — the tail rows are
  // short and can never reach the top edge, so the crossed rule alone would
  // strand the mark one (or several) messages before the end.
  const atEnd = body.scrollHeight - (body.scrollTop + body.clientHeight) <= 24;
  let node = null, last = null;
  // Document order is visual order; the lit row is the last block whose top
  // has crossed the container's top edge — at the end, the last block there
  // is at all.
  for (const el of document.querySelectorAll('#dlg-log .run-h, #dlg-log section.turn, #dlg-log .msg[id], #dlg-log .lv-sec[id^="lv-a-"]')) {
    let sel = null;
    if (el.classList.contains('run-h')) sel = `.lv-node.run[data-run="${el.dataset.run}"]`;
    else if (el.matches('section.turn')) sel = `.lv-node.turn[data-rid="${el.dataset.rid}"]`;
    else if (el.classList.contains('msg')) {
      const m = /^lv-m-(\d+)-(\d+)$/.exec(el.id);
      if (m) sel = `.lv-node.msg.sub[data-rid="${m[1]}"][data-idx="${m[2]}"]`;
    } else sel = `.lv-node.resp.sub[data-rid="${el.id.slice(5)}"]`;
    const found = sel && rail.querySelector(sel);
    if (!found) continue;
    last = found;
    if (el.getBoundingClientRect().top - top <= 8) node = found;
  }
  if (atEnd && last) node = last;
  const nodes = rail.querySelectorAll('.lv-node.on');
  if (!node) { nodes.forEach(n => n.classList.remove('on')); return; }
  nodes.forEach(n => { if (n !== node) n.classList.remove('on'); });
  if (node && !node.classList.contains('on')) {
    node.classList.add('on');
    const rr = node.getBoundingClientRect(), pr = rail.getBoundingClientRect();
    if (rr.top < pr.top) rail.scrollTop += rr.top - pr.top - 4;
    else if (rr.bottom > pr.bottom) rail.scrollTop += rr.bottom - pr.bottom + 4;
  }
}

/** A tree message node (the `USER` / `ASSIST` / `TOOL` line under a request):
 *  focus that message, not just its request — open the turn, unfold the history
 *  block if the message lives in it, scroll it into view and flash it. */
async function lvFocusMsg(rid, idx) {
  const turns = lvTurns();
  const i = turns.findIndex(t => t.rid === rid);
  if (i < 0) return;
  lvTrackLock = Date.now() + 700;
  // A message this request only resent is drawn in the turn that first sent it,
  // so that is the one to open and flash — unless this turn has its re-sent
  // history unfolded, in which case the copy on screen is the one to land on.
  let k = i, tgt = { rid: turns[i].rid, idx };
  while (k > 0 && S.lv.inc) {
    const hid = lvHidden(k);
    if (!(hid && tgt.idx < hid)) break;
    k--;
    tgt = { rid: turns[k].rid, idx: Math.min(tgt.idx, Math.max(lvMsgCount(turns[k]) - 1, 0)) };
  }
  const needed = !LV.open.has(tgt.rid) || !LV.content.has(tgt.rid);
  // a jump to a message opens a folded block — the row was clicked for its text
  LV.msgFold.set(`lv-m-${tgt.rid}-${tgt.idx}`, false);
  LV.rid = tgt.rid;
  LV.open.add(tgt.rid);
  LV.focusMsg = [tgt.rid, tgt.idx];
  if (needed) await lvDraw({ scroll: false });
  const el = document.getElementById(`lv-m-${tgt.rid}-${tgt.idx}`);
  if (!el) { lvScroll(); lvDrawRail(); return; }
  for (const d of [el.closest('details'), ...el.querySelectorAll('details')]) if (d) d.open = true;
  el.scrollIntoView({ block: 'center' });
  el.classList.add('flash');
  setTimeout(() => el.classList.remove('flash'), 1300);
  lvDrawRail();
}

async function lvStep(delta) {
  const i = lvIdxOf(LV.rid) + delta;
  if (i >= 0 && i < lvTurns().length) await lvFocus(lvTurns()[i].rid);
}

async function lvToggle(rid) {
  // Each request opens and closes on its own: opening one never folds the others.
  // One set drives both views, so folding from the tree folds the block and
  // folding the block folds the tree. Opening also opens its run.
  if (lvIdxOf(rid) < 0) return;
  if (LV.open.has(rid)) LV.open.delete(rid);
  else {
    LV.open.add(rid);
    LV.closedRuns.delete(lvRunOf(lvRuns(), lvIdxOf(rid)));
  }
  LV.rid = rid;
  LV.focusMsg = null;
  await lvDraw({ scroll: false });
}

async function lvReload({ quiet = false } = {}) {
  if (!LV.conv) return;
  const key = LV.conv.key;
  let conv;
  try { conv = await api('GET', 'conv?key=' + encodeURIComponent(key)); }
  catch (e) { if (!quiet) toast(e.message, false); return; }
  // Turn text is refetched too: a finished response may have arrived since.
  const keep = r => conv.turns.some(t => t.rid === r);
  const open = [...LV.open], shut = [...LV.closedRuns];
  // A refresh keeps the view: same open turns, same block folds, same scroll
  // position — a poll that yanks you to the focused request is a poll off.
  const b = document.querySelector('#dlg-log .lv-body');
  const top = b ? b.scrollTop : 0;
  LV.conv = conv;
  LV.content = new Map();
  LV.open = new Set(open.filter(keep));
  LV.closedRuns = new Set(shut);
  $('#dlg-log [data-sub]').textContent = lvSubline(conv);
  await lvDraw({ scroll: false });
  if (b) b.scrollTop = top;
  lvPrefetch();
  LV.reloads = (LV.reloads || 0) + 1;
  if (!quiet) toast('reloaded', true);
}

// Auto-refresh: a quiet lvReload on a timer, stopped the moment the viewer
// closes so nothing polls behind a shut dialog.
const LV_AUTO_MS = 5000;
let lvAutoTimer = null;
function lvAutoStart() {
  lvAutoStop();
  if (!S.lv.auto) return;
  lvAutoTimer = setInterval(() => lvReload({ quiet: true }), LV_AUTO_MS);
}
function lvAutoStop() {
  if (lvAutoTimer) { clearInterval(lvAutoTimer); lvAutoTimer = null; }
}

/** Refetch everything and hand the browser a file: raw is the log line as the
 *  file holds it, formatted is the conversation as a transcript. */
async function lvDownload() {
  if (!LV.conv) return;
  toast(`preparing ${S.lv.fmt === 'raw' ? 'raw log' : 'transcript'}…`, true);
  const turns = lvTurns();
  for (const t of turns) await lvLoad(t.rid);
  const stamp = new Date().toISOString().slice(0, 16).replace(/[-:T]/g, '').slice(0, 11);
  const base = `ollamamq-${(LV.conv.user || 'client').replace(/[^\w.-]/g, '_')}-${(LV.conv.session || 'one-shot')}-${stamp}`;
  if (S.lv.fmt === 'raw') {
    const line = JSON.stringify({ ...LV.conv, turns: turns.map(t => ({ ...t, content: { req: LV.content.get(t.rid)?.req ?? null, resp: LV.content.get(t.rid)?.resp ?? null } })) });
    lvSave(line + '\n', base + '.jsonl', 'application/x-ndjson');
    return;
  }
  lvSave(lvMarkdown(LV.conv, LV.content, turns), base + '.md', 'text/markdown');
}

/**
 * The conversation as a transcript: what the viewer shows, in a file. One
 * section per request with its measured timeline, the messages the request
 * actually added (the history it resent is named, not repeated), and the
 * response reconstructed to text — thinking, answer, tool calls, usage.
 */
function lvMarkdown(conv, content, turns) {
  const out = [`# ${conv.user}${conv.session ? ` · #${sessName(conv)}` : ''}`, ''];
  if (conv.session_label) out.push(conv.session_label, '');
  if (conv.redacted) out.push('_Body capture was switched off for part of this conversation; the timeline, sizes and roles below are complete._', '');
  out.push(`_${turns.length} ${turns.length === 1 ? 'request' : 'requests'} · ${fmtTime(conv.start_ms)} → ${fmtTime(conv.end_ms)}${conv.model ? ` · ${conv.model}` : ''}${conv.backend ? ` → ${conv.backend}` : ''}_`, '');
  turns.forEach((t, i) => {
    const c = content.get(t.rid) || {};
    out.push(`## Request ${i + 1} — ${fmtTime(t.at_ms)}${t.tl?.end != null ? ` → ${fmtTime(t.tl.end)}` : ' (running)'}`, '',
      `> ${lvStages(t).replace(/<[^>]*>/g, '')}${t.status != null ? ` · HTTP ${t.status}` : ''}${t.outcome && t.outcome !== 'ok' ? ` · ${t.outcome}` : ''}`, '');
    const prev = i > 0 ? lvMsgCount(turns[i - 1]) : 0;
    const msgs = c.reqObj?.messages || [];
    // What an earlier turn already sent is not repeated here; the count says so.
    const cut = prev > 0 && prev < msgs.length ? prev : 0;
    out.push(`**Request** ${[t.method, t.path, t.req?.bytes != null ? fmtBytes(t.req.bytes) : null,
      msgs.length ? `${msgs.length} messages${cut ? `, ${msgs.length - cut} new` : ''}` : null]
      .filter(Boolean).join(' ')}`, '');
    if (c.req == null) out.push(t.redacted ? '_capture turned off — no text is stored for this client, key or backend_' : '_no request text stored_', '');
    else if (msgs.length) for (const m of msgs.slice(cut)) out.push(...lvMsgMd(m));
    else out.push('```', c.req, '```', '');
    out.push(`**Response**${t.resp?.bytes != null ? ` · ${fmtBytes(t.resp.bytes)}` : ''}`, '');
    if (c.resp == null) out.push(t.redacted ? '_capture turned off_' : '_no response text stored_', '');
    else {
      const st = streamState(c.resp) || lvPlainResp(c.resp);
      if (!st) out.push('```', c.resp, '```', '');
      else {
        if (st.think) out.push('_thinking_', '', `> ${st.think.replace(/\n/g, '\n> ')}`, '');
        if (st.text) out.push(st.text.trimEnd(), '');
        for (const tc of st.tools || [])
          out.push(`_tool call_ **${tc.name || '?'}**`, '', '```json', lvArgsText(tc.args), '```', '');
        if (st.usage?.c != null)
          out.push(`_${fmtNum(st.usage.c)} tokens generated${st.finish ? `, finished \`${st.finish}\`` : ''}${st.cutTail ? ', capture truncated' : ''}_`, '');
      }
    }
  });
  return out.join('\n');
}

/** One request message as markdown: role, text, then the calls it issues. */
function lvMsgMd(m) {
  const role = String(m.role || 'message').toLowerCase();
  const label = ROLE_LABEL[role] || role.toUpperCase();
  const who = role === 'tool' ? (m.name || m.tool_name || m.tool_call_id || '') : '';
  const think = m.reasoning_content || m.reasoning || '';
  const text = msgText(m.content, m.images);
  const out = [`**${label}${who ? ` · ${who}` : ''}**`, ''];
  if (think) out.push('_thinking_', `> ${String(think).replace(/\n/g, '\n> ')}`, '');
  if (text) out.push(String(text).trimEnd(), '');
  for (const tc of m.tool_calls || [])
    out.push(`_tool call_ **${tc.function?.name ?? tc.name ?? '?'}**`, '', '```json', lvArgsText(tc.function?.arguments), '```', '');
  return out;
}

/** A non-stream response has no SSE to rebuild, but reads out the same way. */
function lvPlainResp(text) {
  const o = tryJson(text);
  const m = o?.choices?.[0]?.message || (o?.choices?.[0]?.text != null ? { content: o.choices[0].text } : null);
  if (!m) return null;
  return {
    text: m.content || '', think: m.reasoning_content || m.reasoning || '',
    tools: (m.tool_calls || []).map(tc => ({ name: tc.function?.name ?? tc.name, args: tc.function?.arguments })),
    usage: o.usage ? { p: o.usage.prompt_tokens, c: o.usage.completion_tokens } : null,
    finish: o.choices?.[0]?.finish_reason,
  };
}

/** Tool-call arguments arrive as a JSON string (sometimes mid-stream, so
 *  sometimes not yet valid); pretty-print what parses, pass through the rest. */
function lvArgsText(args) {
  if (args == null) return '';
  if (typeof args === 'string') {
    const o = tryJson(args);
    return o === undefined ? args : JSON.stringify(o, null, 2);
  }
  return JSON.stringify(args, null, 2);
}

function lvSave(text, name, type) {
  const a = document.createElement('a');
  a.href = URL.createObjectURL(new Blob([text], { type }));
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 5000);
  toast(`saved ${name}`, true);
}

// --- Jump tree ---
//
// The rail mirrors the conversation as a tree: request → request/response →
// message → tool call. Each node is expandable on its own and every node
// carries the timestamp it was measured at, so the outline reads as the
// timeline it is.

function lvNode({ level, rid, idx, when, tip, tag, snip, pv, tb, caret, cls = '', action = 'lv-jump', on = false, run }) {
  return `<div class="lv-node ${cls} l${level}${on ? ' on' : ''}" data-action="${action}" data-rid="${rid ?? ''}"${idx != null ? ` data-idx="${idx}"` : ''}${run != null ? ` data-run="${run}"` : ''}
    ${tip ? `title="${esc(tip)}"` : ''}>
    <span class="car"${caret ? ` data-action="${action === 'lv-run' ? 'lv-run' : 'lv-tree'}"` : ''} data-rid="${rid}">${caret ?? ''}</span>
    <span class="when">${when ?? ''}</span><span class="tag">${esc(tag)}</span>
    <span class="snip">${pv || esc(snip ?? '')}</span>${tb ?? ''}</div>`;
}

/** A message has no clock instant of its own: it arrives inside one request, and
 *  the capture never timed it separately. What IS measured is the window that
 *  produced it — a tool result is the stretch since the previous answer ended,
 *  an assistant message is that answer's own generation. The tree names those
 *  windows the way dmesg -H names a delta, and leaves the rows that truly have
 *  no time of their own (system, user) blank, with the reason in the tooltip.
 *  A message an earlier request already sent resolves back to the turn that
 *  first carried it, so one message never claims two different windows. */
function lvMsgWhen(i, idx) {
  const turns = lvTurns();
  let k = i;
  while (k > 0) {
    const hid = lvHidden(k);
    if (!(hid && idx < hid)) break;
    k--;
  }
  if (idx < lvHidden(k)) return null;          // older than the log: no window to name
  const t = turns[k], before = turns[k - 1];
  const role = (t.msgs?.[idx] || {}).role;
  if (before && role === 'tool' && before.tl?.end != null)
    return { when: fmtDur((t.at_ms - before.tl.end) / 1000),
      tip: `tool result sent with request ${k + 1} — produced between ${fmtTime(before.tl.end)} and ${fmtTime(t.at_ms)}` };
  // A streamed answer is timed from its first token; a single-shot one has no
  // first token, so its whole model-side window (dispatch to end) is what the
  // message took to be produced.
  if (before && role === 'assistant' && (before.tl?.first_token ?? before.tl?.dispatch) != null && before.tl?.end != null) {
    const from = before.tl.first_token ?? before.tl.dispatch;
    return { when: fmtDur((before.tl.end - from) / 1000),
      tip: `the answer to request ${k} — generated ${fmtTime(from)} → ${fmtTime(before.tl.end)}` };
  }
  return { when: '', tip: `sent inside request ${k + 1} at ${fmtTime(t.at_ms)} — a message has no instant of its own` };
}

const lvFinish = t => `${t.status ?? '…'}${t.outcome && t.outcome !== 'ok' ? ` ${t.outcome}` : ''}`;

function lvDrawRail() {
  const rail = $('#lv-rail');
  $('#dlg-log .lv-shell').classList.toggle('railed', S.lv.rail);
  rail.hidden = !S.lv.rail;
  lvApplyRail();
  if (!S.lv.rail) return;
  const turns = lvTurns();
  const runs = lvRuns();
  const items = [`<div class="lv-rail-h">
    <button class="btn sm" data-action="lv-expand">Expand all</button>
    <button class="btn sm" data-action="lv-collapse">Collapse all</button></div>`];
  let prevRun = -1;
  turns.forEach((t, i) => {
    const prev = turns[i - 1];
    const rk = lvRunOf(runs, i);
    // Indent by membership: a request inside a group sits one level under its
    // range line; a lone request sits at the top, with its messages under it.
    const subLevel = rk !== -1 ? 3 : 2;
    const turnLevel = rk !== -1 ? 2 : 1;
    const gapNode = prev && prev.tl?.end != null && t.at_ms - prev.tl.end >= LV_IDLE_MS
      ? lvNode({ level: turnLevel, rid: t.rid, when: fmtDur((t.at_ms - prev.tl.end) / 1000), tag: 'idle',
        tip: `how long it sat quiet: ${fmtDur((t.at_ms - prev.tl.end) / 1000)} (${fmtTime(prev.tl.end)} → ${fmtTime(t.at_ms)})`,
        snip: ((t.msgs || []).filter(m => m.name).map(m => m.name).join(', ') || 'agent side'), cls: 'gapnode', caret: '' })
      : '';
    items.push(gapNode);
    if (runs.length && rk !== prevRun) {
      if (rk !== -1) {
        const ri = lvRunInfo(runs, rk);
        const shut = LV.closedRuns.has(rk);
        items.push(lvNode({ level: 1, rid: '', run: rk, tag: `#${ri.a}–#${ri.b}`,
          when: rk === 0 ? fmtTime(turns[0].at_ms) : `+${fmtDur((turns[ri.a - 1].at_ms - turns[0].at_ms) / 1000)}`,
          pv: `${ri.dur ? esc(ri.dur) : ''}${shut && ri.pv ? ` · <span class="${ri.pv.cls}">${esc(ri.pv.txt)}</span>` : ''}`,
          tb: ri.calls ? `<span class="tb" title="${ri.calls} tool calls across this run">×${ri.calls}</span>` : '',
          tip: `a run: the requests between two final answers — ${ri.a}–${ri.b}${ri.dur ? ` · ${ri.dur}` : ''}; click to ${shut ? 'open' : 'fold'} it`,
          caret: shut ? '▸' : '▾', cls: 'run', action: 'lv-run' }));
      }
      prevRun = rk;
    }
    if (rk !== -1 && LV.closedRuns.has(rk)) return; // folded run: the row above is all of it
    const f = lvFacts(i);
    items.push(lvNode({
      level: turnLevel, rid: t.rid, tag: `#${i + 1}`,
      when: i === 0 ? fmtTime(t.at_ms) : `+${fmtDur((t.at_ms - turns[0].at_ms) / 1000)}`,
      pv: !LV.open.has(t.rid) && (f.pvU || f.pvT) ? `<span class="${f.pvU ? 'u' : 't'}">${esc(f.pvU || f.pvT)}</span>` : '',
      snip: `→ ${t.req?.bytes != null ? fmtBytes(t.req.bytes) : '…'} · ← ${lvFinish(t)}`,
      tb: f.tCalls ? `<span class="tb${f.tBad ? ' bad' : ''}" title="${f.tCalls} ${f.tCalls === 1 ? 'tool call' : 'tool calls'} in this request${f.tBad ? ' — a result reported a non-zero exit code' : ''}">×${f.tCalls}</span>` : '',
      cls: 'turn', caret: LV.open.has(t.rid) ? '▾' : '▸',
      tip: `arrived ${fmtTime(t.at_ms)}${i ? ` · a moment: ${fmtDur((t.at_ms - turns[0].at_ms) / 1000)} after the conversation began` : ''}`
        + `${prev ? ` · ${fmtDur((t.at_ms - prev.at_ms) / 1000)} after request ${i}` : ''}`
        + `${t.status != null ? ` · ${t.status}${t.outcome && t.outcome !== 'ok' ? ` ${t.outcome}` : ''}` : ''}`,
      on: t.rid === LV.rid && !LV.focusMsg }));
    if (!LV.open.has(t.rid)) return;
    const c = LV.content.get(t.rid);
    // The messages this request re-sent stay out of the tree while incremental
    // mode is on — they are drawn under the request that first sent them.
    const hid = lvHidden(i);
    // The answer this turn gave re-enters the next request as a message —
    // the answer row under its own turn already speaks it, so the increment
    // below does not repeat it (it read as a duplicated assistant).
    const prevAns = i ? lvAnswerSig(turns[i - 1]) : '';
    for (const [mi, m] of ((c?.reqObj && c.reqObj.messages) || []).entries()) {
      if (S.lv.inc && mi < hid) continue;
      if (S.lv.inc && m.role === 'assistant' && prevAns
        && lvWords(msgText(m.content, m.images)) === prevAns) continue;
      const w = lvMsgWhen(lvIdxOf(t.rid), mi)
        || { when: '', tip: 'older than this log \u2014 the request that produced it was never captured' };
      items.push(lvNode({ level: subLevel, rid: t.rid, idx: mi, when: w.when, tip: w.tip,
        tag: ROLE_LABEL[m.role] || 'MSG', snip: lvSnip(m),
        cls: `msg sub ${ROLE_CLS[m.role] || 'user'}`,
        action: 'lv-msg', on: !!LV.focusMsg && LV.focusMsg[0] === t.rid && LV.focusMsg[1] === mi }));
    }
    // The assistant's own answer row. It carries the reply's opening words
    // (the operator reads the rail as SYS / USER / ASST messages — a meta
    // "resp" row did not read as the assistant), and clicking it lands on the
    // Response section in the body. The duration is a length, not a moment.
    const dur = t.tl?.end != null ? t.tl.end - t.at_ms : null;
    const fin = f.finish;
    items.push(lvNode({ level: subLevel, rid: t.rid, cls: 'resp sub asst', tag: 'ASST', action: 'lv-resp',
      when: dur == null ? '' : fmtDur(dur / 1000),
      tip: t.tl?.end != null
        ? `the assistant's answer — how long it took: ${fmtDur(dur / 1000)} (arrived ${fmtTime(t.at_ms)}, finished ${fmtTime(t.tl.end)})${fin ? ` — ${fin}` : ''} — click to jump to it`
        : 'no response recorded yet',
      snip: (f.words || fin || lvFinish(t)) + (c && c.resp == null ? ' · no text' : ''), caret: '' }));
  });
  if (!turns.length) items.push('<div class="lv-foot">Nothing to jump to in this view.</div>');
  setHTML(rail, items.join(''));
}

const lvSnip = m => {
  const first = String(typeof m.content === 'string' ? m.content
    : Array.isArray(m.content) ? (m.content.find(p => p.text)?.text || '') : '')
    .split('\n')[0].trim();
  const calls = (m.tool_calls || []).map(tc => tc.function?.name ?? tc.name).filter(Boolean).join(', ');
  return (first || calls || m.name || '').slice(0, 48);
};

// --- formatters: every one returns the raw view if it cannot parse ---

const rawView = t => `<pre class="raw">${esc(t)}</pre>`;

/** Chat request truncated by the content limit: recover the scalar params
 *  from the prefix and as many complete `messages` elements as the capture
 *  reached. Undefined when this is not a truncated chat request. */
function salvageChat(text) {
  const cut = text.indexOf('\n...[truncated:');
  if (cut < 0) return undefined;
  const head = text.slice(0, cut);
  const mi = head.indexOf('"messages"');
  const open = mi < 0 ? -1 : head.indexOf('[', mi);
  if (open < 0) return undefined;
  // Walk the array string-aware and note where each complete element ends
  // (depth back to 0 on `}`); the capture may end mid-element.
  const ends = [];
  let depth = 0, inStr = false, es = false;
  for (let i = open + 1; i < head.length; i++) {
    const c = head[i];
    if (inStr) { if (es) es = false; else if (c === '\\') es = true; else if (c === '"') inStr = false; continue; }
    if (c === '"') inStr = true;
    else if (c === '{' || c === '[') depth++;
    else if (c === '}' || c === ']') { depth--; if (depth === 0 && c === '}') ends.push(i); }
  }
  if (!ends.length) return undefined;
  const arr = tryJson(head.slice(open, ends.at(-1) + 1) + ']');
  if (!Array.isArray(arr)) return undefined;
  const obj = { messages: arr };
  const ms = /"model"\s*:\s*"([^"]*)"/.exec(head);
  if (ms) obj.model = ms[1];
  if (/"stream"\s*:\s*true/.test(head)) obj.stream = true;
  for (const k of ['temperature', 'top_p', 'max_tokens', 'max_completion_tokens', 'num_predict']) {
    const m = new RegExp(`"${k}"\\s*:\\s*([\\d.]+)`).exec(head);
    if (m) obj[k] = +m[1];
  }
  return obj;
}

/** Chat request body → meta line + transcript blocks; chat response →
 *  reconstructed message + footer; any other JSON → collapsible tree. */
function formatContent(text) {
  const trunc = text.includes('[truncated:')
    ? '<div class="lv-note">Body was truncated by the content limit — showing everything the capture kept.</div>' : '';
  const o = tryJson(text);
  if (o && typeof o === 'object' && !Array.isArray(o)) {
    if (Array.isArray(o.messages)) return transcriptView(o);
    if (o.choices || o.message) return responseView(o) ?? jsonView(o, text);
    return jsonView(o, text);
  }
  const salv = salvageChat(text);
  if (salv) return trunc + transcriptView(salv);
  const gs = groupStream(text);
  if (gs) return gs + trunc;
  return rawView(text);
}

const ROLE_CLS = { system: 'sys', developer: 'sys', user: 'user', assistant: 'asst', tool: 'tool' };
// The role chip locates a message in a long transcript; it is not a sentence.
// Four letters, like every other role chip, so the column stays narrow.
const ROLE_LABEL = { system: 'SYS', developer: 'DEV', user: 'USER', assistant: 'ASST', tool: 'TOOL' };

/** A message `content`: string, or parts array (images as placeholders). */
function msgText(c, images) {
  let t = typeof c === 'string' ? c : '';
  if (Array.isArray(c))
    t = c.map(p => p.type === 'text' ? (p.text || '') : (p.type === 'image_url' || p.type === 'image') ? '[image]' : '')
      .filter(Boolean).join('\n');
  if (Array.isArray(images) && images.length) t += (t ? '\n' : '') + `[${images.length} image${images.length > 1 ? 's' : ''}]`;
  return t;
}

const thinkBlock = r => r
  ? `<details><summary>thinking · ${r.length.toLocaleString()} chars</summary><div class="mtext">${esc(r)}</div></details>` : '';

function toolCallBlock(name, args) {
  const v = typeof args === 'string' ? tryJson(args) : args;
  const body = v !== undefined ? JSON.stringify(v, null, 2) : String(args ?? '');
  return `<div class="tc"><div class="tc-h"><span class="fn">${esc(name || 'tool call')}</span></div><pre>${esc(body)}</pre></div>`;
}

function msgBlock(m, toolNames, id, cls = '') {
  const role = String(m.role || 'message');
  const label = role === 'tool'
    ? `TOOL · ${esc(m.tool_name || m.name || (toolNames && toolNames.get(m.tool_call_id)) || m.tool_call_id || '?')}`
    : (ROLE_LABEL[role] || esc(role));
  // Fold state: an explicit click wins; system/developer blocks default to one
  // line so the transcript opens on what the conversation said, not on the
  // harness prompt every request repeats.
  const shut = id ? (LV.msgFold.has(id) ? LV.msgFold.get(id) : role === 'system' || role === 'developer') : false;
  let html = `<div class="msg ${ROLE_CLS[role] || 'user'}${cls}${shut ? ' fold' : ''}"${id ? ` id="${id}"` : ''}><span class="role"${id ? ` data-action="msg-fold" data-key="${id}" title="click to ${shut ? 'open' : 'fold'} this message"` : ''}>${shut ? '▸' : '▾'} ${label}`;
  if (shut) return html + `<span class="mf-pv">${esc(lvSnip(m))}</span></span></div>`;
  html += '</span>';
  html += thinkBlock(m.reasoning_content || m.reasoning || '');
  const t = msgText(m.content, m.images);
  if (t) html += `<div class="mtext">${esc(t)}</div>`;
  for (const tc of m.tool_calls || [])
    html += toolCallBlock(tc.function?.name ?? tc.name, tc.function?.arguments ?? tc.arguments);
  return html + '</div>';
}

/** Parameters worth a glance above the transcript. */
function reqMeta(body) {
  const bits = [`${(body.messages || []).length} messages`];
  if (body.stream) bits.push('stream');
  for (const k of ['temperature', 'top_p', 'max_tokens', 'max_completion_tokens', 'num_predict'])
    if (body[k] != null) bits.push(`${k} ${body[k]}`);
  if (Array.isArray(body.tools) && body.tools.length) {
    const names = body.tools.map(t => t.function?.name ?? t.name).filter(Boolean);
    bits.push(`tools: ${names.slice(0, 3).join(', ')}${names.length > 3 ? ` +${names.length - 3} more` : ''}`);
  }
  return bits.join(' · ');
}

function transcriptView(body) {
  const toolNames = new Map();
  for (const m of body.messages || [])
    for (const tc of m.tool_calls || []) if (tc.id) toolNames.set(tc.id, tc.function?.name ?? tc.name);
  return `<div class="lv-meta">${esc(reqMeta(body))}</div>`
    + (body.messages || []).map(m => msgBlock(m, toolNames)).join('');
}

/** Parse a streamed capture into its parts — reconstructed text, thinking,
 *  tool calls, finish reason, usage — or null when it is not a stream. OpenAI
 *  SSE (`data: {…}` lines + `[DONE]`) or Ollama-native line-delimited JSON.
 *  The viewer renders this to HTML; the markdown download writes it as text. */
function streamState(text) {
  const lines = text.split('\n').map(l => l.trim()).filter(Boolean);
  if (!lines.length) return null;
  // A stream may open with an SSE comment (`: keepalive`) or an `event:` line —
  // llama.cpp does exactly that — so the first line that carries payload is
  // what decides the shape, not line 0.
  const head = lines.find(l => /^data:/.test(l) || /^\{/.test(l));
  if (!head) return null;
  const sse = /^data:/.test(head);
  const trunc = text.includes('[truncated:');
  const st = { text: '', think: '', tools: [], finish: null, usage: null, chunks: 0, errs: 0, cutTail: false };
  for (const line of lines) {
    if (sse && !/^data:/.test(line)) continue; // keep-alives, event:, marker lines
    const payload = sse ? line.slice(5).trim() : line;
    if (payload === '[DONE]') continue;
    const o = tryJson(payload);
    // A capture truncated by the content limit ends mid-chunk: keep what
    // parsed instead of dropping the whole view.
    if (!o || typeof o !== 'object') { if (trunc) { st.cutTail = true; continue; } return null; }
    st.chunks++;
    // A single whole response object (`chat.completion`, not a chunk) is not a
    // stream: let the caller fall back to the plain-response reader.
    if (!sse && st.chunks === 1 && o.object !== 'chat.completion.chunk'
        && !o.choices?.[0]?.delta && typeof o.response !== 'string') return null;
    if (o.error) { st.errs++; st.text += `\n[error: ${o.error.message ?? JSON.stringify(o.error)}]`; continue; }
    if (o.choices?.[0]?.finish_reason) st.finish = o.choices[0].finish_reason;
    if (o.done_reason) st.finish = o.done_reason;
    if (o.usage) st.usage = { p: o.usage.prompt_tokens ?? o.usage.p, c: o.usage.completion_tokens ?? o.usage.c };
    if (o.prompt_eval_count != null || o.eval_count != null)
      st.usage = { p: o.prompt_eval_count ?? st.usage?.p, c: o.eval_count ?? st.usage?.c };
    const d = o.choices?.[0]?.delta || o.message || (typeof o.response === 'string' ? { content: o.response } : null);
    if (!d) continue;
    if (d.content) st.text += d.content;
    const th = d.reasoning_content || d.reasoning;
    if (th) st.think += th;
    for (const t of d.tool_calls || []) {
      const f = t.function || {};
      if (f.arguments != null && typeof f.arguments === 'object') { st.tools.push({ name: f.name, args: f.arguments }); continue; }
      const i = t.index ?? 0;
      (st.tools[i] ??= { name: '', args: '' });
      st.tools[i].name += f.name || '';
      st.tools[i].args += typeof f.arguments === 'string' ? f.arguments : '';
    }
  }
  if (!st.chunks) return null;
  st.tools = st.tools.filter(Boolean);
  return st;
}

/** Render that reconstruction as one assistant message. */
function groupStream(text) {
  const st = streamState(text);
  if (!st) return null;
  const foot = [`finish: ${st.finish || '?'}`];
  if (st.usage) foot.push(`prompt ${fmtTok(st.usage.p || 0)} → completion ${fmtTok(st.usage.c || 0)} tok`);
  foot.push(`${fmtNum(st.chunks)} chunks grouped${st.errs ? ` · ${st.errs} error payload(s)` : ''}${st.cutTail ? ' · tail truncated' : ''}`);
  return `<div class="msg asst"><span class="role">assistant</span>${thinkBlock(st.think)}`
    + (st.text ? `<div class="mtext">${esc(st.text)}</div>` : '')
    + st.tools.map(t => toolCallBlock(t.name, t.args)).join('')
    + `</div><div class="lv-foot">${esc(foot.join(' · '))}</div>`;
}

/** Non-streamed chat response (OpenAI object or Ollama /api/chat object). */
function responseView(o) {
  const m = o.choices?.[0]?.message || o.message;
  if (!m) return null;
  const usage = o.usage || (o.prompt_eval_count != null ? { p: o.prompt_eval_count, c: o.eval_count } : null);
  const foot = [`finish: ${o.choices?.[0]?.finish_reason || o.done_reason || '?'}`];
  if (usage) foot.push(`prompt ${fmtTok(usage.prompt_tokens ?? usage.p ?? 0)} → completion ${fmtTok(usage.completion_tokens ?? usage.c ?? 0)} tok`);
  return `<div class="msg asst"><span class="role">assistant</span>${msgBlock(m, null)}</div>`
    + `<div class="lv-foot">${esc(foot.join(' · '))}</div>`;
}

function jsonView(o, text) {
  if (text.length > 400_000) return rawView(text);
  return `<div class="jt">${jsonTree(o, null, 0)}</div>`;
}

function jsonTree(v, key, depth) {
  const label = key !== null ? `<b>${esc(key)}</b> ` : '';
  if (v === null || typeof v !== 'object')
    return `<div class="jl" style="margin-left:${depth * 14}px">${label}${esc(typeof v === 'string' ? v : String(v))}</div>`;
  const isArr = Array.isArray(v);
  const es = isArr ? v.map((x, i) => [i, x]) : Object.entries(v);
  if (!es.length) return `<div class="jl" style="margin-left:${depth * 14}px">${label}${isArr ? '[]' : '{}'}</div>`;
  const inner = es.map(([k, v2]) => jsonTree(v2, k, depth + 1)).join('');
  return `<details${depth < 1 ? ' open' : ''}><summary style="margin-left:${depth * 14}px">${label}${isArr ? `[${es.length}]` : `{${es.length}}`}</summary>${inner}</details>`;
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
  client_priority: { label: 'How far a client’s own priority claim reaches', kind: 'select', options: [['off', 'Off — priority is set here only'], ['self', 'Within its own client (high)'], ['global', 'Anywhere (high and top)']], help: 'A client can ask for its session to jump the queue with an X-Session-Priority header. Off ignores it; “within its own client” caps the ask at first place among that client’s own sessions; “anywhere” also allows top, which goes first on the whole board. Keyless requests never get a say, and a pin set here always wins.' },
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
    form.redact_log.checked = !!existing.redact_log;
  }
  const fd = await ask(dlg);
  if (!fd) return;
  const body = { url: fd.get('url').trim(), name: fd.get('name').trim() || null, kind: fd.get('kind') || null };
  body.redact_log = !!fd.get('redact_log');
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
    form.redact_log.checked = !!existing.redact_log;
  }
  const fd = await ask(dlg);
  if (!fd) return;
  const body = { name: fd.get('name').trim(), note: fd.get('note').trim() };
  if (existing) {
    body.disabled = !!fd.get('disabled');
    body.redact_log = !!fd.get('redact_log');
    const r = await act('PATCH', `clients/${encodeURIComponent(existing.name)}`, body, 'Client saved');
    if (r) S.openClients.add(body.name);
  } else {
    const r = await act('POST', 'clients', body, `Client ${body.name} created`);
    if (r) {
      S.openClients.add(body.name);
      await poll();
    }
  }
}

/** Edit one key: its label and its capture switch. The key string never moves. */
async function keyEditDialog(id) {
  const f = findKey(id);
  if (!f) return;
  const dlg = $('#dlg-key-edit');
  const form = $('form', dlg);
  form.reset();
  $('[data-title]', dlg).textContent = `Edit key · ${f.client.name}`;
  form.key.value = f.key.key;
  form.label.value = f.key.label || '';
  form.redact_log.checked = !!f.key.redact_log;
  const fd = await ask(dlg);
  if (!fd) return;
  await act('PATCH', `keys/${encodeURIComponent(id)}`,
    { label: fd.get('label').trim(), redact_log: !!fd.get('redact_log') }, 'Key saved');
}

/** Copy whatever the key dialog is showing (the full string is in the field). */
const copyShownKey = () => copyText($('#dlg-key-edit [name="key"]').value);

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
  const body = { label: fd.get('label').trim(), redact_log: !!fd.get('redact_log') };
  if (fd.get('mode') === 'manual') body.key = fd.get('key').trim();
  const r = await act('POST', `clients/${encodeURIComponent(client)}/keys`, body);
  if (r) {
    S.openClients.add(client);
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
  'session-pri': async el => {
    const next = { normal: 'high', high: 'top', top: 'normal' }[el.dataset.pri] || 'high';
    // act() re-polls, so the new badge shows up on its own.
    await act('POST', `sessions/${encodeURIComponent(el.dataset.tag)}/priority`,
      { level: next }, `Session priority set to ${next}`);
  },
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
      S.openClients.add(name);
      S.clients = await api('GET', 'clients');
      render();
      addKeyDialog(name);
    }
  },
  'toggle-keys': el => {
    // A keyless id has no keys to show: its row says why in its own sub-line.
    const name = el.dataset.name;
    S.openClients.has(name) ? S.openClients.delete(name) : S.openClients.add(name);
    render();
  },
  'expand-clients': () => {
    const all = identities().filter(r => r.reg);
    if (all.every(r => S.openClients.has(r.id))) S.openClients.clear();
    else for (const r of all) S.openClients.add(r.id);
    render();
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
  'copy-shown': () => copyShownKey(),
  'edit-key': el => keyEditDialog(el.dataset.id),
  'revoke-key': async el => {
    const f = findKey(el.dataset.id);
    if (!await confirmDlg('Delete key?', `<code>${esc(maskKey(f.key.key))}</code> (${esc(f.key.label || 'no label')}) stops identifying <b>${esc(f.client.name)}</b> immediately. A key cannot be moved to another client — delete it and hand out a new one.`, 'Delete')) return;
    return act('DELETE', `keys/${encodeURIComponent(f.key.id)}`, undefined, 'Key deleted');
  },
  'fold-all': () => { S.collapsed.clear(); saveCollapsed(); render(); },
  'fold-none': () => { for (const b of $$('#ov-now .det-h')) S.collapsed.add(b.dataset.key); saveCollapsed(); render(); },

  'log-detail': el => openSeq(+el.dataset.seq),
  'log-round': el => openConv({ rid: +el.dataset.rid }),
  'log-close': () => $('#dlg-log').close(),
  // A log row -> the conversation that request belongs to, opened on it.
  'log-jump': el => { const rid = +el.dataset.rid; if (rid) openConv({ rid }).catch(e => toast(e.message, false)); },
  // A conversation row in the grouped list, or an Overview header chip: open the
  // whole conversation, focused on the newest request when the row names one.
  'conv-open': el => openConv({ rid: +el.dataset.rid || null, session: el.dataset.session, user: el.dataset.user })
    .catch(e => toast(e.message, false)),
  // A turn header or a tree request row: focus that request.
  'lv-toggle': el => lvToggle(+el.dataset.rid),
  'lv-jump': el => lvFocus(+el.dataset.rid),
  // The tree's caret is the turn header's caret: one fold, and it moves the
  // same way whichever side you click it on.
  'lv-tree': el => lvToggle(+el.dataset.rid),
  // A run header (body or tree) folds the whole run to its one line.
  'lv-run': el => {
    const k = +el.dataset.run;
    if (LV.closedRuns.has(k)) LV.closedRuns.delete(k);
    else LV.closedRuns.add(k);
    return lvDraw({ scroll: false });
  },
  // The tree's RESP row: land on the Response section, not the turn top.
  'lv-resp': el => lvFocusSec(+el.dataset.rid, 'a'),
  // One switch decides increment-vs-full-prompt for every request at once.
  'lv-inc': async el => {
    S.lv.inc = el.checked;
    saveLv();
    await lvDraw({ scroll: false });
  },
  'lv-auto': el => {
    S.lv.auto = el.checked;
    saveLv();
    if (el.checked) lvAutoStart(); else lvAutoStop();
  },
  // A message block folds on its label — the system prompt defaults to one
  // line, every block is the user's to open or shut.
  'msg-fold': el => {
    const shutNow = el.closest('.msg').classList.contains('fold');
    LV.msgFold.set(el.dataset.key, !shutNow);
    lvDraw();
  },
  // A message row inside a request: jump to that message in the body.
  'lv-msg': el => lvFocusMsg(+el.dataset.rid, +el.dataset.idx),
  'lv-expand': async () => {
    LV.open = new Set(lvTurns().map(t => t.rid));
    LV.closedRuns.clear();
    await lvDraw();
  },
  'lv-collapse': async () => {
    // All means all — the request you arrived on folds with the rest; a row
    // that refuses to close is what made Collapse all read as broken.
    LV.open = new Set();
    LV.closedRuns = new Set(lvRuns().map((_, k) => k));
    await lvDraw();
  },
};

document.addEventListener('click', e => {
  const el = e.target.closest('[data-action]');
  if (!el || el.disabled) return;
  const fn = actions[el.dataset.action];
  if (!fn) return;
  // A checkbox's new state IS its click activation — cancelling it leaves the
  // box stuck on the old value (a redraw used to paper over that while the
  // Show-incremental switch lived inside the rail).
  if (!(el instanceof HTMLInputElement && el.type === 'checkbox')) e.preventDefault();
  fn(el);
});

// Following a nav link closes the drawer, even for the page already shown
// (which fires no hashchange).
$$('.nav nav a').forEach(a => a.addEventListener('click', () => setNav(false)));

document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && document.body.classList.contains('nav-open')) setNav(false);
});

// Click on a dialog's backdrop closes it.
$$('dialog').forEach(d => d.addEventListener('click', e => { if (e.target === d) d.close('cancel'); }));
// A closed viewer must leave no poller behind.
$('#dlg-log').addEventListener('close', lvAutoStop);

['#log-filter', '#log-dir'].forEach(s => $(s).addEventListener('input', () => renderLogs()));
// Grouped or per-request: which view of the same ring, remembered like the
// other view choices.
S.logGroup = storedLogGroup();
$$('#log-group input[name="log-group"]').forEach(r => {
  r.checked = S.logGroup === r.value;
  r.addEventListener('change', () => {
    S.logGroup = r.value;
    try { localStorage.setItem('omq.logGroup', r.value); } catch { /* storage blocked */ }
    renderLogs();
  });
});
// Round-viewer controls: choices persist like the other view preferences.
$('#lv-jump').addEventListener('click', () => { S.lv.rail = !S.lv.rail; saveLv(); lvDrawRail(); $('#lv-jump').classList.toggle('active', S.lv.rail); });
/* One reading position: as the log is scrolled, the rail row of the block at
   its top lights up and the rail scrolls it into view, so the hand on the
   wheel always sees where it is. Scroll does not bubble, but a capture-phase
   listener on the dialog sees it and re-checks which element it came from. */
let lvTrackPending = false;
$('#dlg-log').addEventListener('scroll', e => {
  if (!e.target.classList?.contains('lv-body')) return;
  if (lvTrackPending) return;
  lvTrackPending = true;
  setTimeout(() => { lvTrackPending = false; lvTrackRail(); }, 50);
}, true);
// Drag the tree wider or narrower; the width is remembered, double-click (or
// Home) puts it back, arrow keys nudge it for anyone not using a mouse.
(() => {
  const grip = $('#lv-grip');
  const edge = () => $('#dlg-log .lv-shell').getBoundingClientRect().right;
  let dragging = false;
  grip.addEventListener('pointerdown', e => {
    dragging = true;
    document.body.classList.add('lv-resizing');
    try { grip.setPointerCapture(e.pointerId); } catch { /* synthetic events */ }
    e.preventDefault();
  });
  grip.addEventListener('pointermove', e => { if (dragging) lvSetRail(edge() - e.clientX); });
  const end = () => {
    if (!dragging) return;
    dragging = false;
    document.body.classList.remove('lv-resizing');
    saveLv();
  };
  grip.addEventListener('pointerup', end);
  grip.addEventListener('pointercancel', end);
  grip.addEventListener('dblclick', () => { lvSetRail(LV_RAIL_DEF); saveLv(); });
  grip.addEventListener('keydown', e => {
    const step = e.shiftKey ? 64 : 24;
    if (e.key === 'ArrowLeft') lvSetRail(lvRailW() + step);
    else if (e.key === 'ArrowRight') lvSetRail(lvRailW() - step);
    else if (e.key === 'Home') lvSetRail(LV_RAIL_DEF);
    else return;
    e.preventDefault();
    saveLv();
  });
})();
$('#lv-first').addEventListener('click', () => lvFocus(lvTurns()[0]?.rid));
$('#lv-prev').addEventListener('click', () => lvStep(-1));
$('#lv-next').addEventListener('click', () => lvStep(1));
$('#lv-last').addEventListener('click', () => lvFocus(lvTurns().at(-1)?.rid));
$('#lv-reload').addEventListener('click', () => lvReload().catch(e => toast(e.message, false)));
$('#lv-dl').addEventListener('click', () => lvDownload().catch(e => toast(e.message, false)));
$$('input[name="lv-fmt"]').forEach(r => r.addEventListener('change', () => { S.lv.fmt = r.value; saveLv(); lvDraw({ scroll: true }); }));
$('#client-filter').addEventListener('change', e => { S.clientFilter = e.target.value; render(); });
$('#model-search').addEventListener('input', e => { S.modelQuery = e.target.value.trim().toLowerCase(); render(); });
$('#model-show').addEventListener('change', e => { S.modelShow = e.target.value; render(); });
$('#model-mode').addEventListener('change', e => setModelMode(e.target.value));
$(`#ov-req-show input[value="${S.reqShow}"]`).checked = true;
['input', 'change'].forEach(t => $('#set-form').addEventListener(t, onSettingsInput));
// Leaving with unsaved settings: the browser asks first.
window.addEventListener('beforeunload', e => { if (S.draft.size) e.preventDefault(); });
$('#ov-req-show').addEventListener('change', e => {
  S.reqShow = e.target.value;
  try { localStorage.setItem('omq.reqShow', S.reqShow); } catch { /* storage blocked */ }
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
