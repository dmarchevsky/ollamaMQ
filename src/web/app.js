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
  clientFilter: 'all',       // all | registered | unregistered
  modelPolicy: null,         // { mode, rules } from api/models
  modelShow: 'all',          // all | served | hidden
  modelQuery: "",
  overview: null,            // api/overview for the chosen window
  window: storedWindow(),    // 5m | 15m | 1h
  colorOf: new Map(),        // client id -> colour slot 1..4
  activity: null,            // activity chart geometry, for the hover
  online: true,
};

/** The Overview window from the last visit (per browser). */
function storedWindow() {
  try {
    const w = localStorage.getItem('omq.window');
    if (['5m', '15m', '1h'].includes(w)) return w;
  } catch { /* storage blocked */ }
  return '15m';
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

// ------------------------------------------------------------------ polling

async function poll() {
  try {
    const needLogs = S.page === 'logs' || S.page === 'overview';
    const needClients = S.page === 'clients' || !S.clients.length;
    const needModels = S.page === 'models' || !S.modelPolicy;
    const needOverview = S.page === 'overview';
    const [state, logs, clients, models, overview] = await Promise.all([
      api('GET', 'state'),
      needLogs && !$('#log-pause').checked ? api('GET', `logs?limit=${LOG_LIMIT}`) : null,
      needClients ? api('GET', 'clients') : null,
      needModels ? api('GET', 'models') : null,
      needOverview ? api('GET', `overview?window=${S.window}`) : null,
    ]);
    S.state = state;
    if (logs) S.logs = logs;
    if (clients) S.clients = clients;
    if (models) S.modelPolicy = models;
    if (overview) S.overview = overview;
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
    clients: renderClients,
    models: renderModels,
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
 * Colour slots for the four busiest clients. A client keeps its colour while
 * it stays among them, so the board does not repaint when the order shifts.
 */
function assignColors(ids) {
  for (const id of [...S.colorOf.keys()]) if (!ids.includes(id)) S.colorOf.delete(id);
  for (const id of ids) {
    if (S.colorOf.has(id)) continue;
    const used = new Set(S.colorOf.values());
    S.colorOf.set(id, [1, 2, 3, 4].find(n => !used.has(n)));
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

function kpi(label, value, sub, values, cls = '') {
  return `<div class="card kpi ${cls}"><div class="k">${label}</div><div class="v">${value}</div><div class="sub">${sub}</div>${spark(values)}</div>`;
}

/** Seconds between activity samples (`stats::SAMPLE_EVERY`). */
const SAMPLE_SECS = 5;
const WINDOW_LABEL = { '5m': '5 min', '15m': '15 min', '1h': '1 hour' };

function renderOverview(st) {
  const ov = S.overview;
  setHTML($('#ov-logs'), logTable(S.logs.slice(0, OVERVIEW_LOGS), false));
  if (!ov) return;
  assignColors(ov.clients.filter(c => c.busy_secs > 0 || c.running).slice(0, 4).map(c => c.id));
  const s = ov.status, k = ov.kpis;

  // Status line: the one sentence an operator reads first.
  const offline = s.backends_total - s.backends_online;
  const level = !s.backends_online ? 'bad'
    : (offline || (s.oldest_wait_secs ?? 0) > 30 || (k.error_pct ?? 0) >= 5) ? 'warn' : 'ok';
  const word = { ok: 'Healthy', warn: 'Needs attention', bad: 'No backend online' }[level];
  const parts = [
    `${s.backends_online} of ${s.backends_total} backends online`,
    `${s.free} of ${s.capacity} slots free`,
    s.queued ? `${s.queued} waiting (oldest ${fmtDur(s.oldest_wait_secs)})` : 'nothing waiting',
    ...s.loading.map(l => `${esc(l.verb)} ${esc(l.model)} on ${esc(l.backend)}`),
  ];
  if (k.errors) parts.push(`${k.errors} error${k.errors === 1 ? '' : 's'} in the last ${WINDOW_LABEL[S.window]}`);
  setHTML($('#ov-status'), `<span class="lvl ${level}"><span class="sdot"></span>${word}</span>${parts.map(p => `<span>${p}</span>`).join('')}`);

  setHTML($('#ov-kpis'), [
    kpi('Slots in use', `${s.running}<span class="of"> / ${s.capacity}</span>`, `${s.free} free`, k.spark.running),
    kpi('Waiting', fmtNum(s.queued), s.queued ? `oldest ${fmtDur(s.oldest_wait_secs)}` : 'queue empty', k.spark.queued, s.queued ? 'q' : ''),
    kpi('Throughput', `${fmtRate(k.req_per_min)}<span class="of"> req/min</span>`,
      k.error_pct ? `<span class="st-bad">${k.error_pct.toFixed(1)}% errors</span>` : 'no errors', k.spark.req_per_min),
    kpi('Output', `${fmtRate(k.out_tps)}<span class="of"> tok/s</span>`, 'all streams together', k.spark.out_tps),
    kpi('Time to first token', fmtSecs(k.ttft_p50), `median · p95 ${k.ttft_p95 == null ? '—' : fmtSecs(k.ttft_p95)}`, k.spark.ttft),
    kpi('Generation speed', `${fmtRate(k.gen_p50)}<span class="of"> tok/s</span>`, 'median per stream', k.spark.gen_tps),
  ].join(''));

  renderCapacity(st, ov);
  renderActivity(ov);
  renderNow(st, ov);
  renderBackendStats(ov);
  renderClientStats(st, ov);
}

/** One row of slot squares per backend; a busy slot wears its client's colour. */
function renderCapacity(st, ov) {
  const runsAt = new Map();
  for (const r of st.runs || []) (runsAt.get(r.backend) || runsAt.set(r.backend, []).get(r.backend)).push(r);
  $('#ov-cap-sum').textContent = `${ov.status.running} running · ${ov.status.free} free`;
  const rows = ov.backends.map(b => {
    const runs = runsAt.get(b.label) || [];
    let slots = '';
    if (b.online) {
      slots += runs.map(r => `<span class="slot ${colorClass(r.client)}" title="${esc(`${r.client} · ${r.model}\n${r.state === 'generating' ? 'generating' : 'waiting for first token'} · ${fmtDur(r.elapsed_secs)} · ${r.tokens} tok${r.tokens_per_sec ? ` · ${r.tokens_per_sec.toFixed(1)} tok/s` : ''}`)}"></span>`).join('');
      // In-flight requests without a live view (none today, but the counts
      // come from different places).
      slots += '<span class="slot busy" title="busy"></span>'.repeat(Math.max(0, b.running - runs.length));
      const rest = Math.max(0, b.max - Math.max(b.running, runs.length));
      slots += b.loading
        ? `<span class="slot loading" title="${esc(`${b.loading.verb} ${b.loading.model}`)}"></span>`.repeat(rest)
        : '<span class="slot free" title="free"></span>'.repeat(rest);
    }
    const note = !b.online ? '<span class="st-bad">offline</span>'
      : b.loading ? `<span class="st-warn">${esc(b.loading.verb)} ${esc(b.loading.model)}</span>`
      : `${b.free} free`;
    return `<div class="cap-row">
      <div class="cap-name"><span class="sdot ${b.online ? 'on' : 'off'}"></span><span class="bk-url">${esc(b.label)}</span></div>
      <div class="slots">${slots}</div>
      <div class="cap-note">${note}</div></div>`;
  }).join('');
  // What is waiting, by model: the demand the free slots are not meeting.
  const byModel = new Map();
  for (const w of ov.queue) {
    const m = byModel.get(w.model) || { n: 0, oldest: 0 };
    m.n++; m.oldest = Math.max(m.oldest, w.waited_secs);
    byModel.set(w.model, m);
  }
  const waiting = byModel.size ? `<div class="cap-wait"><span class="dim">Waiting:</span> ${[...byModel].map(([m, v]) =>
    `<span class="chip q">${esc(m)} ×${v.n} · ${fmtDur(v.oldest)}</span>`).join(' ')}</div>` : '';
  const legend = [...S.colorOf.keys()].map(id => `<span>${swatch(id)}${esc(id)}</span>`).join('')
    + '<span><i class="sw cother"></i>other</span><span><i class="sw free"></i>free</span><span><i class="sw loading"></i>loading a model</span>';
  setHTML($('#ov-capacity'), ov.backends.length
    ? `<div class="caps">${rows}</div>${waiting}<div class="legend">${legend}</div>`
    : '<div class="empty">No backends configured</div>');
}

/** Running + waiting over the window, stacked, with the capacity line. */
function renderActivity(ov) {
  const el = $('#ov-activity');
  const t = ov.timeline;
  const from = ov.now_ms - ov.window_secs * 1000;
  S.activity = null;
  if (t.length < 2) {
    $('#ov-act-sum').textContent = '';
    setHTML(el, '<div class="empty">Collecting data — the chart fills in every 5 seconds</div>');
    return;
  }
  const W = Math.max(280, el.clientWidth || 800), H = 170;
  const pad = { l: 28, r: 8, t: 10, b: 22 };
  const top = Math.max(1, ...t.map(p => Math.max(p.capacity, p.running + p.queued)));
  const step = top <= 5 ? 1 : top <= 10 ? 2 : top <= 25 ? 5 : top <= 50 ? 10 : Math.ceil(top / 50) * 10;
  const ymax = Math.ceil(top / step) * step;
  const x = ms => pad.l + (ms - from) / (ov.now_ms - from) * (W - pad.l - pad.r);
  const y = v => H - pad.b - v / ymax * (H - pad.t - pad.b);
  const path = (f0, f1) => `M${t.map(p => `${x(p.at_ms).toFixed(1)},${y(f1(p)).toFixed(1)}`).join('L')}`
    + `L${[...t].reverse().map(p => `${x(p.at_ms).toFixed(1)},${y(f0(p)).toFixed(1)}`).join('L')}Z`;
  const cap = `M${t.map(p => `${x(p.at_ms).toFixed(1)},${y(p.capacity).toFixed(1)}`).join('L')}`;
  let grid = '';
  for (let v = 0; v <= ymax; v += step) {
    grid += `<line class="g" x1="${pad.l}" x2="${W - pad.r}" y1="${y(v)}" y2="${y(v)}"/><text class="ax" x="${pad.l - 6}" y="${y(v) + 4}" text-anchor="end">${v}</text>`;
  }
  for (let i = 0; i <= 3; i++) {
    const ms = from + (ov.now_ms - from) * i / 3;
    grid += `<text class="ax" x="${x(ms)}" y="${H - 6}" text-anchor="${i === 0 ? 'start' : i === 3 ? 'end' : 'middle'}">${fmtTime(ms).slice(0, 5)}</text>`;
  }
  const marks = ov.markers.map(m => `<path class="mk" d="M${x(m.at_ms).toFixed(1)},${H - pad.b + 1}l4,7h-8z"><title>${esc(`${fmtTime(m.at_ms)} ${m.status ? `HTTP ${m.status}` : m.outcome.replace('_', ' ')} · ${m.client} · ${m.model}`)}</title></path>`).join('');
  S.activity = { t, x, W, H, pad };
  const peak = Math.max(...t.map(p => p.running + p.queued));
  $('#ov-act-sum').textContent = `last ${WINDOW_LABEL[S.window]} · peak ${peak} in use or waiting`;
  setHTML(el, `<svg class="act" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" role="img" aria-label="Running and waiting requests over time">
    ${grid}
    <path class="run" d="${path(() => 0, p => p.running)}"/>
    <path class="que" d="${path(p => p.running, p => p.running + p.queued)}"/>
    <path class="cap" d="${cap}"/>
    ${marks}
    <line class="cross" x1="0" x2="0" y1="${pad.t}" y2="${H - pad.b}" visibility="hidden"/>
  </svg><div class="tip" hidden></div>
  <div class="legend"><span><i class="sw run"></i>running</span><span><i class="sw que"></i>waiting</span><span><i class="sw capl"></i>slots online</span>${ov.markers.length ? '<span><i class="sw mk"></i>error or timeout</span>' : ''}</div>`);
}

/** Crosshair + tooltip on the activity chart (delegated; survives re-renders). */
function activityHover(e) {
  const a = S.activity, el = $('#ov-activity');
  const svg = $('svg.act', el), tip = $('.tip', el);
  if (!a || !svg || !tip) return;
  const line = $('.cross', svg);
  if (e.type === 'pointerleave') { tip.hidden = true; line.setAttribute('visibility', 'hidden'); return; }
  const box = svg.getBoundingClientRect();
  const px = (e.clientX - box.left) * a.W / box.width;
  let best = a.t[0];
  for (const p of a.t) if (Math.abs(a.x(p.at_ms) - px) < Math.abs(a.x(best.at_ms) - px)) best = p;
  const bx = a.x(best.at_ms);
  line.setAttribute('x1', bx); line.setAttribute('x2', bx); line.setAttribute('visibility', 'visible');
  tip.innerHTML = `<b>${fmtTime(best.at_ms)}</b><br>${best.running} running · ${best.queued} waiting<br>${best.capacity} slots online · ${best.free} free<br>${fmtRate(best.out_tokens / SAMPLE_SECS)} tok/s out`;
  tip.hidden = false;
  const left = bx / a.W * box.width;
  tip.style.left = `${Math.min(Math.max(0, left + 12), box.width - tip.offsetWidth)}px`;
}

/** Running and waiting requests in one table. */
function renderNow(st, ov) {
  const runs = st.runs || [];
  const pill = r => r.state === 'generating'
    ? '<span class="badge ok">generating</span>'
    : `<span class="badge accent" title="Waiting for the first token (prefill, or a response that is not streamed)">prefill ${fmtDur(r.elapsed_secs)}</span>`;
  $('#ov-now-count').textContent = `${runs.length} running · ${ov.status.queued} waiting`;
  const rows = runs.map(r => `<tr>
      <td>${pill(r)}</td>
      <td>${swatch(r.client)}<span class="mono">${esc(r.client)}</span></td>
      <td class="trunc">${esc(r.model)}</td>
      <td class="mono dim">${esc(r.backend)}</td>
      <td class="num">${fmtDur(r.waited_secs ?? 0)}</td>
      <td class="num">${firstToken(r)}</td>
      <td class="num">${fmtNum(r.tokens)}</td>
      <td class="num">${r.tokens_per_sec == null ? '<span class="dim">—</span>' : r.tokens_per_sec.toFixed(1)}</td></tr>`)
    .concat(ov.queue.map(w => `<tr>
      <td><span class="badge warn" title="Position in this client's own queue">waiting #${w.pos}</span></td>
      <td>${swatch(w.client)}<span class="mono">${esc(w.client)}</span></td>
      <td class="trunc">${esc(w.model)}</td>
      <td class="dim ph-hide">—</td>
      <td class="num st-warn">${fmtDur(w.waited_secs)}</td>
      <td class="num dim ph-hide">—</td><td class="num dim ph-hide">—</td><td class="num dim ph-hide">—</td></tr>`));
  setHTML($('#ov-now'), rows.length ? `<table>
    <tr><th>State</th><th>Client</th><th>Model</th><th>Backend</th><th class="num">Waited</th><th class="num">First token</th><th class="num">Tokens</th><th class="num">tok/s</th></tr>
    ${rows.join('')}</table>` : '<div class="empty">Nothing running or waiting</div>');
}

function renderBackendStats(ov) {
  // Flag the slowest backend, when there are several with enough traffic to compare.
  const busy = ov.backends.filter(b => b.requests >= 5);
  const worst = (f, cmp) => busy.length < 2 ? null
    : busy.filter(b => f(b) != null).reduce((a, b) => (a == null || cmp(f(b), f(a))) ? b : a, null);
  const slowTtft = worst(b => b.ttft_p95, (a, b) => a > b);
  const slowGen = worst(b => b.gen_p50, (a, b) => a < b);
  const flag = (on, text) => on ? ` <span class="badge warn">${text}</span>` : '';
  setHTML($('#ov-backends'), ov.backends.length ? `<table>
    <tr><th>Backend</th><th class="num">Slots</th><th class="num">Requests</th><th class="num">Errors</th><th class="num">First token p50 / p95</th><th class="num">Gen tok/s</th><th class="num">Prefill tok/s</th><th>Output tok/s</th></tr>
    ${ov.backends.map(b => `<tr>
      <td><span class="sdot ${b.online ? 'on' : 'off'}"></span><span class="bk-url">${esc(b.label)}</span> ${typeBadge(b.api_type)}</td>
      <td class="num">${b.online ? `${b.running}/${b.max}` : '<span class="st-bad">offline</span>'}</td>
      <td class="num">${fmtNum(b.requests)}</td>
      <td class="num ${b.errors ? 'st-bad' : 'dim'}">${fmtNum(b.errors)}</td>
      <td class="num">${fmtSecs(b.ttft_p50)} / ${fmtSecs(b.ttft_p95)}${flag(b === slowTtft, 'slowest')}</td>
      <td class="num">${fmtRate(b.gen_p50)}${flag(b === slowGen, 'slowest')}</td>
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
    </table>` : `<div class="empty">No requests in the last ${WINDOW_LABEL[S.window]}</div>`);
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
  if (!st.backends.length) {
    setHTML($('#backends'), '<div class="empty">No backends configured</div>');
    return;
  }
  const rows = st.backends.map(b => {
    const open = S.expanded.has(b.url);
    const pct = b.max ? Math.min(100, (b.active / b.max) * 100) : 0;
    const loaded = new Set(b.models_loaded);
    const hiddenHere = new Set(b.models_hidden);
    let html = `<tr class="bk-row ${open ? 'open' : ''}" data-action="toggle-backend" data-url="${esc(b.url)}">
      <td><span class="caret">${open ? '▾' : '▸'}</span><span class="sdot ${b.online ? 'on' : 'off'}"></span><span class="bk-url">${esc(hostOf(b.url))}</span></td>
      <td>${typeBadge(b.api_type)}</td>
      <td class="num" title="${b.max_detected ? `${b.max} slots reported by the backend` : `${b.max} = settings.max_concurrent_per_backend`}">${b.active}/${b.max}<span class="meter"><i style="width:${pct}%"></i></span></td>
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
  setHTML($('#backends'), `<table>
    <tr><th>Backend</th><th>Type</th><th class="num">Active</th><th class="num">Done</th><th>Models</th><th>Control</th></tr>
    ${rows}</table>`);
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
      <td><span class="chips">${r.per.map(p => `<span class="chip ${p.ok ? '' : 'off'}" title="${esc(p.b.url)}${p.ok ? '' : ' — hidden here'}">${esc(hostOf(p.b.url))}</span>`).join('')}</span></td>
      <td>${badge(r)}</td>
      <td class="num"><label class="switch" title="${r.status === 'hidden' ? 'Serve on every backend' : 'Hide on every backend'}">
        <input type="checkbox" data-model="${esc(r.name)}" ${r.status !== 'hidden' ? 'checked' : ''}><span></span></label></td></tr>`).join('')}
    </table>` : `<div class="empty">${rows.length ? 'No models match' : 'No backend lists any models yet'}</div>`);

  const scope = r => r.backends.length
    ? r.backends.map(u => `<span class="chip">${esc(hostOf(u))}</span>`).join('')
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
    S.state.backends.map(b => `<option value="${esc(b.url)}">${esc(hostOf(b.url))}</option>`).join('');
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
$(`#ov-window input[value="${S.window}"]`).checked = true;
$('#ov-window').addEventListener('change', e => {
  S.window = e.target.value;
  try { localStorage.setItem('omq.window', S.window); } catch { /* storage blocked */ }
  S.overview = null;
  poll();
});
['pointermove', 'pointerleave'].forEach(t => $('#ov-activity').addEventListener(t, activityHover));
// The activity chart is drawn to the panel's width.
window.addEventListener('resize', () => { if (S.page === 'overview' && S.overview) renderActivity(S.overview); });
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
