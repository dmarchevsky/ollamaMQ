// Demo mode (`?demo=1`): an in-memory stand-in for the server API with
// sample data, used for screenshots and for trying the UI without a proxy.
// Mutations work against the fixture; nothing leaves the page.
'use strict';

(() => {
  const now = Date.now();
  const min = 60_000, hour = 60 * min, day = 24 * hour;

  const backends = [
    {
      url: 'http://192.168.6.11:8731', online: true, api_type: 'OpenAI', active: 1, max: 1, processed: 1873,
      current_model: 'Qwen3.8-Flash-Next', models_available: ['Qwen3.8-Flash-Next'], models_loaded: [],
      loaded_known: false, loaded_ctx: {}, can_control: false, active_op: null, results: [],
    },
    {
      url: 'http://192.168.255.224:8000', online: true, api_type: 'OpenAI', active: 1, max: 1, processed: 942,
      current_model: 'Qwen3.8-27B', models_available: ['Gemma-4-12B', 'Qwen3.8-27B', 'Qwen3.8-27B-Instruct-Q6', 'Devstral-3-24B'],
      models_loaded: [], loaded_known: false, loaded_ctx: {}, can_control: false, active_op: null, results: [],
    },
    {
      url: 'http://192.168.255.224:8001', online: true, api_type: 'OpenAI', active: 0, max: 1, processed: 311,
      current_model: null, models_available: ['Gemma-4-E4B'], models_loaded: [],
      loaded_known: false, loaded_ctx: {}, can_control: false, active_op: null, results: [],
    },
    {
      url: 'http://192.168.255.224:8002', online: false, api_type: 'OpenAI', active: 0, max: 1, processed: 58,
      current_model: null, models_available: ['BGE-M3'], models_loaded: [],
      loaded_known: false, loaded_ctx: {}, can_control: false, active_op: null, results: [],
    },
    {
      url: 'http://gpu-box.lan:11434', online: true, api_type: 'Ollama', active: 0, max: 2, processed: 4120,
      current_model: 'llama3.3:70b',
      models_available: ['llama3.3:70b', 'Qwen3.8-27B', 'qwen3:32b', 'gpt-oss:120b', 'gpt-oss:20b', 'mistral-small3.2:24b',
        'nomic-embed-text:latest', 'deepseek-r1:32b', 'gemma3:27b', 'phi4:14b', 'qwen2.5-coder:32b',
        'llava:13b', 'codellama:34b', 'granite3.3:8b', 'smollm2:1.7b', 'mxbai-embed-large:latest'],
      models_loaded: ['llama3.3:70b', 'nomic-embed-text:latest'], loaded_known: true,
      loaded_ctx: { 'llama3.3:70b': 32768, 'nomic-embed-text:latest': 8192 }, can_control: true,
      active_op: { verb: 'loading', model: 'qwen3:32b', elapsed_secs: 14 }, results: [],
    },
  ].map((b, idx) => ({ idx, disabled: false, ...b }));
  // Names set in the settings; gpu-box is left unnamed to show both forms.
  Object.assign(backends[0], { name: 'brainiac-halogen' });
  Object.assign(backends[1], { name: 'ryzen-router' });
  Object.assign(backends[2], { name: 'ryzen-e4b' });
  Object.assign(backends[3], { name: 'ryzen-bge' });

  const users = [
    { id: 'codex-laptop', ip: '172.22.0.1', queued: 3, processing: 1, done: 812, dropped: 2, registered: true, blocked: false },
    { id: 'openwebui', ip: '172.22.0.5', queued: 1, processing: 1, done: 1440, dropped: 0, registered: true, blocked: false },
    { id: 'n8n-agents', ip: '192.168.6.40', queued: 0, processing: 0, done: 2210, dropped: 11, registered: true, blocked: false },
    { id: 'dm-desktop', ip: '192.168.6.21', queued: 0, processing: 0, done: 351, dropped: 0, registered: true, blocked: false },
    { id: 'grafana-alerts', ip: '192.168.6.30', queued: 1, processing: 0, done: 96, dropped: 0, registered: false, blocked: false },
    { id: 'unverified:openwebui', ip: '192.168.6.88', queued: 0, processing: 0, done: 0, dropped: 3, registered: false, blocked: false },
    { id: 'ip-192.168.6.77', ip: '192.168.6.77', queued: 0, processing: 0, done: 4, dropped: 9, registered: false, blocked: true },
    { id: 'anonymous', ip: '172.22.0.1', queued: 0, processing: 0, done: 17, dropped: 1, registered: false, blocked: false },
  ];

  const state = {
    version: '0.5.0',
    vip: 'openwebui',
    boost: null,
    blocked: { users: ['ip-192.168.6.77'], ips: ['192.168.6.77', '10.8.0.14'], clients: ['old-intern'] },
  };

  let keySeq = 1;
  const key = (k, label, created, used) => ({ id: `k${keySeq++}`, key: k, label, created_at_ms: created, last_used_ms: used });
  const clients = [
    { name: 'codex-laptop', note: 'Codex CLI on the work laptop', disabled: false, created_at_ms: now - 21 * day,
      keys: [key('omq-7f3a9c41d2e8b6057a1c9e3f4b2d8a60', 'laptop', now - 21 * day, now - 4000),
             key('codex-review-2026', 'auto-review', now - 6 * day, now - 3 * min)] },
    { name: 'openwebui', note: 'Open WebUI (docker)', disabled: false, created_at_ms: now - 40 * day,
      keys: [key('omq-1b9e44c0a7f3d25e8c6b0f9a3d7e1c52', 'webui', now - 40 * day, now - 9000)] },
    { name: 'n8n-agents', note: 'Workflow automations', disabled: false, created_at_ms: now - 12 * day,
      keys: [key('omq-c08d2f6a91b3e7d4405f8a2c6e9b13d7', 'prod', now - 12 * day, now - 18 * min),
             key('n8n-staging-key-01', 'staging', now - 3 * day, now - 2 * day)] },
    { name: 'dm-desktop', note: '', disabled: false, created_at_ms: now - 5 * day,
      keys: [key('omq-5e2b7d91c4a8f03e6b1d9c7a2f4e8b06', '', now - 5 * day, now - 47 * min)] },
    { name: 'old-intern', note: 'Summer project, access ended', disabled: true, created_at_ms: now - 120 * day,
      keys: [key('omq-9a4c1e7b3d5f80262e9c4b7a1d3f5e08', 'notebook', now - 120 * day, now - 61 * day)] },
  ];

  const LOG_SAMPLES = [
    ['IN', 'codex-laptop', 'Qwen3.8-Flash-Next', null, 'queued', true],
    ['OUT', 'openwebui', 'Qwen3.8-27B', 'http://192.168.255.224:8000', '200 · 7.9s · 912 tok', true],
    ['IN', 'openwebui', 'Qwen3.8-27B', null, 'queued', true],
    ['OUT', 'codex-laptop', 'Qwen3.8-Flash-Next', 'http://192.168.6.11:8731', '200 · 12.4s · 2.1k tok', true],
    ['CTL', 'admin', 'qwen3:32b', 'http://gpu-box.lan:11434', 'loading started', false],
    ['OUT', 'n8n-agents', 'BGE-M3', 'http://192.168.255.224:8002', 'dropped: backend offline', false],
    ['IN', 'n8n-agents', 'BGE-M3', null, 'queued', true],
    ['OUT', 'dm-desktop', 'Gemma-4-E4B', 'http://192.168.255.224:8001', '200 · 1.2s · 140 tok', true],
    ['OUT', 'ip-192.168.6.77', null, null, '403 user blocked', false],
    ['OUT', 'codex-laptop', 'Qwen3.8-Flash-Next', 'http://192.168.6.11:8731', '200 · 31.0s · 6.4k tok', true],
    ['OUT', 'anonymous', 'codex-auto-review', null, '503 no backend serves this model', false],
    ['OUT', 'openwebui', 'llama3.3:70b', 'http://gpu-box.lan:11434', '200 · 18.3s · 1.5k tok', true],
  ];
  const logs = [];
  for (let i = 0; i < 60; i++) {
    const [dir, user, model, backend, info, has] = LOG_SAMPLES[i % LOG_SAMPLES.length];
    logs.push({ seq: 1000 - i, at_ms: now - i * 23_000 - 1500, dir, user, model, backend, info, has_content: has });
  }

  const SAMPLE_CONTENT = JSON.stringify({
    model: 'Qwen3.8-Flash-Next',
    stream: true,
    messages: [
      { role: 'system', content: 'You are a coding agent. Be concise.' },
      { role: 'user', content: 'Refactor dispatcher::proxy_handler so the API-key lookup happens before the block checks, and add a unit test.' },
    ],
    temperature: 0.2,
    max_tokens: 8192,
  });

  const SAMPLE_RESPONSE = JSON.stringify({
    id: 'chatcmpl-8f2a', object: 'chat.completion', model: 'Qwen3.8-27B',
    choices: [{ index: 0, finish_reason: 'stop', message: { role: 'assistant',
      content: 'Moved the key lookup to the top of proxy_handler so a blocked client is matched by its client name, and added `client_key_sets_queue_user`.' } }],
    usage: { prompt_tokens: 431, completion_tokens: 481, total_tokens: 912 },
  });

  const err = (msg) => { throw new Error(msg); };
  const delay = (v) => new Promise(r => setTimeout(() => r(structuredClone(v)), 60));
  const findClient = (n) => clients.find(c => c.name === n) || err(`no client '${n}'`);
  const allKeys = () => clients.flatMap(c => c.keys.map(k => ({ c, k })));
  const NAME_RE = /^[A-Za-z0-9._-]{1,64}$/;

  const isClient = (id) => clients.some(c => c.name === id);
  const clientList = () => clients.map(c => ({ ...c, vip: state.vip === c.name, boost: state.boost === c.name }));
  // Mirror the server: a client's block state is its disabled flag.
  const syncDisabled = () => {
    state.blocked.clients = clients.filter(c => c.disabled).map(c => c.name);
    users.forEach(u => { if (u.registered) u.blocked = clients.find(c => c.name === u.id)?.disabled ?? false; });
  };

  // Model policy (mirrors src/model_policy.rs: hide rules win; allow rules
  // only count in allow mode).
  const policy = {
    mode: 'deny',
    rules: [
      { id: 'm1', pattern: '*-Instruct-Q6', action: 'hide', backends: [], from_toggle: false },
      { id: 'm2', pattern: 'codellama:34b', action: 'hide', backends: [], from_toggle: true },
      { id: 'm3', pattern: 'smollm*', action: 'hide', backends: [], from_toggle: false },
      { id: 'm4', pattern: 'Qwen3.8-27B', action: 'hide', backends: ['http://gpu-box.lan:11434'], from_toggle: false },
    ],
  };
  let ruleSeq = 10;
  const glob = p => new RegExp('^' + p.replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*').replace(/\?/g, '.') + '$', 'i');
  const hits = (r, m, url) => (!r.backends.length || r.backends.includes(url)) && glob(r.pattern).test(m);
  const served = (m, url) => !policy.rules.some(r => r.action === 'hide' && hits(r, m, url))
    && (policy.mode === 'deny' || policy.rules.some(r => r.action === 'allow' && hits(r, m, url)));
  const policyView = () => ({
    mode: policy.mode,
    rules: policy.rules.map(r => ({
      ...r,
      matches: new Set(backends.flatMap(b => b.models_available.filter(m => hits(r, m, b.url)).map(m => m.toLowerCase()))).size,
    })),
  });

  // Runs in progress: tokens grow with time like a streaming backend.
  const demoRuns = [
    { client: 'codex-laptop', key: 'laptop', model: 'Qwen3.8-Flash-Next', backend: '192.168.6.11:8731', started: now - 94_000, ttfb: 2.4, rate: 38.5, waited: 0.4, est: 21_000, session: 'a3f19c' },
    { client: 'openwebui', key: 'webui', model: 'Qwen3.8-27B', backend: '192.168.255.224:8000', started: now - 21_000, ttfb: 6.8, rate: 11.2, waited: 2.1, est: 44_000, session: '7e04d2' },
    { client: 'n8n-agents', key: 'prod', model: 'llama3.3:70b', backend: 'gpu-box.lan:11434', started: now - 3_000, ttfb: null, rate: 0, waited: 12.6, est: 3_200, session: null },
  ];
  const runView = () => demoRuns.map(r => {
    const elapsed = (Date.now() - r.started) / 1000;
    const gen = r.ttfb == null ? 0 : Math.max(0, elapsed - r.ttfb);
    return { client: r.client, key: r.key, model: r.model, backend: r.backend, elapsed_secs: elapsed,
      state: r.ttfb != null && elapsed >= r.ttfb ? 'generating' : 'prefill', waited_secs: r.waited,
      ttfb_secs: r.ttfb, ttft_secs: r.ttfb, tokens: Math.floor(gen * r.rate), tokens_per_sec: gen >= 1 ? r.rate : null,
      est_prompt_tokens: r.est, session: r.session };
  });

  function snapshot() {
    const sum = (f) => users.reduce((a, u) => a + u[f], 0);
    return {
      ...state,
      stats: { queued: sum('queued'), processing: sum('processing'), done: sum('done'), dropped: sum('dropped'), log_dropped: 0 },
      backends: backends.map(b => ({ ...b, models_hidden: b.models_available.filter(m => !served(m, b.url)) })),
      users,
      runs: runView(),
    };
  }

  // Overview history. Values come from a hash of the sample time, so a
  // sample looks the same on every poll and the chart scrolls steadily.
  const noise = n => { const x = Math.sin(n * 12.9898) * 43758.5453; return x - Math.floor(x); };
  // `backends`: indexes of the backends that can serve the model.
  const demoQueue = [
    { client: 'codex-laptop', model: 'Qwen3.8-Flash-Next', since: now - 38_000, pos: 1, backends: [0], est: 18_000, session: 'a3f19c' },
    { client: 'codex-laptop', model: 'Qwen3.8-Flash-Next', since: now - 21_000, pos: 2, backends: [0], est: 9_500, session: 'a3f19c' },
    { client: 'codex-laptop', model: 'Qwen3.8-Flash-Next', since: now - 4_000, pos: 3, backends: [0], est: 2_400, session: 'a3f19c' },
    { client: 'openwebui', model: 'Qwen3.8-27B', since: now - 9_000, pos: 1, backends: [1], est: 40_000, session: '7e04d2' },
    { client: 'grafana-alerts', model: 'Gemma-4-E4B', since: now - 2_000, pos: 1, backends: [2], est: 800 },
    { client: 'n8n-agents', model: 'BGE-M3', since: now - 17_000, pos: 1, backends: [3], est: 5_000 },
    { client: 'anonymous', model: 'unknown', since: now - 6_000, pos: 1, backends: [], est: 1_200 },
  ];
  // Per backend over 15 minutes: requests, errors, first token p50/p95, gen tok/s, prefill tok/s.
  const BACKEND_STATS = {
    '192.168.6.11:8731': [188, 2, 0.5, 1.1, 64.0, 2950],
    '192.168.255.224:8000': [141, 0, 0.7, 2.4, 38.4, 1610],
    '192.168.255.224:8001': [52, 0, 0.3, 0.6, 92.0, 4100],
    '192.168.255.224:8002': [3, 3, null, null, null, null],
    'gpu-box.lan:11434': [96, 1, 1.2, 6.8, 17.9, 480],
  };
  // Per client: requests, errors, first token p50, wait p95, gen tok/s, tokens in, tokens out, slot seconds.
  const CLIENT_STATS = {
    'codex-laptop': [142, 1, 0.9, 41.0, 51.4, 1_240_000, 96_000, 2100],
    'openwebui': [118, 0, 0.6, 3.1, 29.4, 880_000, 141_000, 1650],
    'n8n-agents': [61, 4, 4.2, 12.0, 17.0, 210_000, 22_000, 850],
    'dm-desktop': [24, 0, 0.4, 0.8, 88.5, 40_000, 9_000, 120],
    'grafana-alerts': [11, 0, 0.3, 2.0, 90.1, 6_000, 1_800, 30],
  };

  function overviewView(path) {
    const w = new URLSearchParams(path.split('?')[1] || '').get('window');
    const secs = { '5m': 300, '1h': 3600 }[w] || 900;
    const scale = secs / 900;
    const t = Date.now(), from = t - secs * 1000;
    const online = backends.filter(b => b.online);
    const capacity = online.reduce((a, b) => a + b.max, 0);
    const loadingOf = b => b.active_op && { backend: hostOf(b.url), verb: b.active_op.verb, model: b.active_op.model };
    // Free KV share per backend (see the capacity panel): one llama.cpp box
    // with --metrics; the rest report nothing.
    const KV = { '192.168.255.224:8000': 42 };
    const queue = demoQueue.map(q => ({
      client: q.client, model: q.model, pos: q.pos, backends: q.backends, session: q.session,
      waited_secs: (t - q.since) / 1000, est_prompt_tokens: q.est,
    }));
    const be = backends.map(b => {
      const label = hostOf(b.url);
      const [requests, errors, ttft_p50, ttft_p95, gen_p50, prefill_tps] = BACKEND_STATS[label] || [0, 0, null, null, null, null];
      return {
        idx: b.idx, label, online: b.online, api_type: b.api_type, running: b.active, max: b.max,
        free: b.online && !b.active_op ? Math.max(0, b.max - b.active) : 0, loading: loadingOf(b),
        kv_free_pct: KV[label] ?? null,
        requests: Math.round(requests * scale), errors: Math.round(errors * scale), ttft_p50, ttft_p95, gen_p50, wait_p95: null,
        prefill_tps, prompt_tokens: 0, completion_tokens: 0, busy_secs: 0,
        spark: b.online ? Array.from({ length: 30 }, (_, i) => (gen_p50 || 0) * (0.6 + 0.6 * noise(i + label.length)) * b.max) : Array(30).fill(0),
      };
    });
    const totalBusy = Object.values(CLIENT_STATS).reduce((a, c) => a + c[7], 0);
    const clientRows = Object.entries(CLIENT_STATS).map(([id, c]) => {
      const u = users.find(x => x.id === id) || { queued: 0, processing: 0, registered: false };
      return {
        id, registered: u.registered, running: demoRuns.filter(r => r.client === id).length,
        queued: queue.filter(q => q.client === id).length,
        requests: Math.round(c[0] * scale), errors: Math.round(c[1] * scale), ttft_p50: c[2], ttft_p95: c[2] * 3,
        wait_p95: c[3], gen_p50: c[4], prefill_tps: null,
        prompt_tokens: Math.round(c[5] * scale), completion_tokens: Math.round(c[6] * scale),
        busy_secs: c[7] * scale, share: c[7] / totalBusy,
      };
    });
    const requests = be.reduce((a, b) => a + b.requests, 0);
    const errors = be.reduce((a, b) => a + b.errors, 0);

    // Swimlanes: running/waiting per client per bucket, from noise waves that
    // keep their shape between polls (noise hashes its input).
    const LANE_W = { 300: 10, 3600: 60 }[secs] ?? 30; // seconds, as in overview.rs
    const LANE_N = Math.round(secs / LANE_W);
    const LANE_CLIENTS = {
      '192.168.6.11:8731': ['codex-laptop', 'openwebui'],
      '192.168.255.224:8000': ['openwebui', 'n8n-agents'],
      '192.168.255.224:8001': ['grafana-alerts'],
      'gpu-box.lan:11434': ['n8n-agents'],
    };
    const lanes = [];
    for (const r of be) {
      const clients = LANE_CLIENTS[r.label] || [];
      if (!r.requests && !queue.some(q => q.backends.length === 1 && q.backends[0] === r.idx)) continue;
      const buckets = Array.from({ length: LANE_N }, (_, i) => {
        const wave = Math.max(0, Math.sin(i / LANE_N * Math.PI * 1.2 - 0.4));
        const segs = [];
        clients.forEach((c, ci) => {
          const run = Math.min(r.max, Math.round(wave * (0.8 + 1.4 * ci) + noise(i * 7 + ci * 31 + r.idx) * 1.1));
          const wt = i > LANE_N - 7 ? Math.round(noise(i + 4 * ci + r.idx)) : 0;
          if (run || wt) segs.push({ client: c, running: run, waiting: wt });
        });
        return segs;
      });
      // What waits right now lands on the last bucket.
      for (const q of queue.filter(q => q.backends.length === 1 && q.backends[0] === r.idx)) {
        const last = buckets.at(-1);
        let s = last.find(s => s.client === q.client);
        if (!s) { s = { client: q.client, running: 0, waiting: 0 }; last.push(s); }
        s.waiting += 1;
      }
      const peak_run = Math.max(0, ...buckets.map(bs => bs.reduce((a, s) => a + s.running, 0)));
      const peak_wait = Math.max(0, ...buckets.map(bs => bs.reduce((a, s) => a + s.waiting, 0)));
      const markers = [];
      if (r.errors > 1) markers.push({ at_ms: from + 0.31 * secs * 1000, outcome: 'backend_error', status: 500, client: clients[0] || 'anon', model: '—' });
      if (r.errors > 3) markers.push({ at_ms: from + 0.62 * secs * 1000, outcome: 'timeout', status: null, client: clients[0] || 'anon', model: '—' });
      lanes.push({ idx: r.idx, label: r.label, online: r.online, free: r.free, max: r.max, requests: r.requests, peak_run, peak_wait, buckets, markers });
    }

    // Finished requests, newest first, spread over the window.
    const recent = Array.from({ length: 9 }, (_, i) => {
      const err = i === 2 || i === 6;
      const prompt = 4000 + Math.round(noise(i + 3) * 26000);
      const cache = err ? null : Math.round(noise(i + 11) * 92);
      const prefillTok = err ? null : Math.round(prompt * (1 - (cache ?? 0) / 100));
      const prefill = err ? null : prefillTok / 2900;
      const completion = err ? 0 : Math.round(40 + noise(i + 5) * 900);
      return {
        at_ms: t - (30 + i * (secs / 10)) * 1000,
        client: ['codex-laptop', 'openwebui', 'n8n-agents', 'dm-desktop'][i % 4],
        model: ['Qwen3.8-Flash-Next', 'Qwen3.8-27B', 'BGE-M3', 'llama3.3:70b'][i % 4],
        backend: ['192.168.6.11:8731', '192.168.255.224:8000', 'gpu-box.lan:11434'][i % 3],
        outcome: err ? 'backend_error' : 'ok', error: err, status: err ? 500 : 200,
        waited_secs: noise(i + 9) * 3,
        session: i % 4 === 0 ? 'a3f19c' : i % 4 === 1 ? '7e04d2' : null,
        prompt_tokens: err ? 0 : prompt, prefill_secs: prefill, prefill_tokens: prefillTok, cache_pct: cache,
        ttft_secs: err ? null : prefill + 0.2,
        completion_tokens: completion, gen_tps: err ? null : 20 + noise(i + 7) * 40,
        gen_secs: err ? null : completion / (20 + noise(i + 7) * 40),
      };
    });

    return {
      window_secs: secs, now_ms: t, lane_from_ms: from, lane_span_ms: secs * 1000,
      status: {
        backends_total: backends.length, backends_online: online.length, capacity,
        running: demoRuns.length, free: be.reduce((a, b) => a + b.free, 0), queued: queue.length,
        oldest_wait_secs: Math.max(...queue.map(q => q.waited_secs)),
        loading: backends.filter(b => b.active_op).map(loadingOf),
        errors, error_pct: requests ? errors * 100 / requests : null,
        done: recent.filter(r => r.outcome === 'ok').length, dropped: recent.filter(r => r.outcome === 'backend_error').length,
      },
      backends: be, clients: clientRows, queue, lanes, recent,
    };
  }

  // Settings page: fields as /ui/api/settings returns them.
  const F = (key, value, source, live = true, fallback = value, fallback_source = source === 'ui' ? 'file' : source) =>
    ({ key, value, source, locked_by: null, live, fallback, fallback_source });
  const MiB = 1 << 20;
  const settings = {
    fields: [
      F('timeout', 900, 'file'), F('stuck_timeout', 60, 'file'), F('max_concurrent_per_backend', 1, 'file'),
      F('detect_backend_slots', true, 'default'), F('max_queued_bytes', 512 * MiB, 'default'),
      F('max_body_bytes', 64 * MiB, 'default', false), F('load_keep_alive', 86400, 'default'),
      F('model_aliases', { 'codex-auto-review': 'Qwen3.8-Flash-Next' }, 'file'), F('ip_identity', true, 'default'),
      F('sticky_routing', 'prompt', 'default'), F('sticky_ttl', 1800, 'default'), F('sticky_lag', 0, 'default'),
      F('sticky_max_entries', 10000, 'default'),
      F('allow_all_routes', false, 'file'), F('metrics_token', null, 'default'), F('log_content_limit', 65536, 'default'),
    ],
    startup: [
      F('host', '0.0.0.0', 'file', false), F('port', 11435, 'file', false),
      F('request_log_path', '/data/ollamamq-requests.jsonl', 'file', false), F('request_log_max_bytes', 10 * MiB, 'default', false),
      F('request_log_max_files', 5, 'default', false), F('clients_file', '/config/clients.yaml', 'default', false),
      F('models_file', '/config/models.yaml', 'default', false),
    ],
    restart_pending: [],
    can_undo: false,
    backends_locked: null,
  };
  const removedUrls = new Set();
  const tokenSet = new Set(['http://192.168.255.224:8000']);
  let lastSaved = null;
  const settingsView = () => ({
    ...settings,
    backends: backends.map(b => ({
      url: b.url, label: hostOf(b.url), name: b.name ?? null, token_set: tokenSet.has(b.url), kind: null,
      disabled: b.disabled || removedUrls.has(b.url), removed: removedUrls.has(b.url), online: b.online && !b.disabled,
      api_type: b.api_type, slots: b.max, slots_detected: b.max > 1,
    })),
  });
  function putSettings(set) {
    lastSaved = structuredClone(settings.fields);
    for (const [k, v] of Object.entries(set)) {
      const f = settings.fields.find(x => x.key === k) || err(`'${k}' cannot be changed here`);
      if (typeof v === 'number' && v < 1 && k !== 'load_keep_alive') err(`${k} must be at least 1`);
      if (v === null) { f.value = f.fallback; f.source = f.fallback_source; }
      else { f.value = k === 'metrics_token' ? (v ? '••••••••' : null) : v; f.source = 'ui'; }
    }
    const body = settings.fields.find(f => f.key === 'max_body_bytes');
    settings.restart_pending = body.value !== 64 * MiB ? ['max_body_bytes'] : [];
    settings.can_undo = true;
  }
  const findBackend = url => backends.find(b => b.url === url) || err(`no backend ${url}`);
  const normUrl = u => { const t = u.trim().replace(/\/+$/, ''); return /^https?:\/\//.test(t) ? t : `http://${t}`; };
  const checkName = (name, url) => {
    if (name && backends.some(b => b.url !== url && (b.name || '').toLowerCase() === name.toLowerCase())) err(`two backends are named '${name}'`);
  };
  let me = { user: 'admin', password_source: 'OLLAMA_MQ_UI_PASSWORD', password_changed_at_ms: null };

  function genKey() {
    const b = new Uint8Array(16);
    crypto.getRandomValues(b);
    return 'omq-' + [...b].map(x => x.toString(16).padStart(2, '0')).join('');
  }

  function handle(method, path, body) {
    try {
      const [p] = path.split('?');
      const seg = p.split('/').map(decodeURIComponent);
      if (method === 'GET' && p === 'state') return delay(snapshot());
      if (method === 'GET' && p === 'overview') return delay(overviewView(path));
      if (method === 'GET' && p === 'me') return delay(me);
      if (method === 'GET' && p === 'settings') return delay(settingsView());
      if (method === 'PUT' && p === 'settings') { putSettings(body.set); return delay(settingsView()); }
      if (method === 'POST' && p === 'settings/undo') {
        if (!lastSaved) err('nothing to undo');
        [settings.fields, lastSaved] = [lastSaved, settings.fields];
        return delay(settingsView());
      }
      if (method === 'POST' && p === 'settings/backends') {
        const url = normUrl(body.url);
        checkName(body.name, url);
        if (removedUrls.delete(url)) { findBackend(url).disabled = false; return delay(settingsView()); }
        if (backends.some(b => b.url === url)) err(`${url} is already a backend`);
        backends.push({ idx: backends.length, url, name: body.name, disabled: false, online: true, api_type: 'OpenAI',
          active: 0, max: 1, processed: 0, current_model: null, models_available: [], models_loaded: [], loaded_known: false,
          loaded_ctx: {}, can_control: false, active_op: null, results: [] });
        if (body.token) tokenSet.add(url);
        return delay(settingsView());
      }
      if (method === 'PUT' && p === 'settings/backends') {
        const b = findBackend(normUrl(body.url));
        if ('name' in body) { checkName(body.name, b.url); b.name = body.name || null; }
        if ('disabled' in body) b.disabled = body.disabled;
        if (body.token) tokenSet.add(b.url); else if ('token' in body) tokenSet.delete(b.url);
        return delay(settingsView());
      }
      if (method === 'POST' && p === 'settings/backends/remove') { removedUrls.add(findBackend(normUrl(body.url)).url); return delay(settingsView()); }
      if (method === 'POST' && p === 'settings/backends/test') {
        return delay({ online: true, api_type: 'OpenAI', slots: 4, model_count: 3, models: ['Gemma-4-12B', 'Qwen3.8-27B', 'Qwen3.8-27B-Instruct-Q6'] });
      }
      if (method === 'POST' && p === 'account/password') {
        if (body.current === 'wrong') err('the current password is wrong');
        if ((body.new || '').length < 12) err('use at least 12 characters');
        me = { ...me, password_source: 'admin.json', password_changed_at_ms: Date.now() };
        return delay({ changed_at_ms: me.password_changed_at_ms });
      }
      if (method === 'GET' && p === 'clients') return delay(clientList());
      if (method === 'GET' && seg[0] === 'logs' && seg.length === 1) return delay(logs);
      if (method === 'GET' && seg[0] === 'logs') {
        const ev = logs.find(l => String(l.seq) === seg[1]) || err('log event expired');
        const content = !ev.has_content ? null : ev.dir === 'OUT' ? SAMPLE_RESPONSE : SAMPLE_CONTENT;
        return delay({ ...ev, content });
      }
      if (method === 'POST' && p === 'config/reload') return delay({ models: 3 });
      if (method === 'GET' && p === 'models') return delay(policyView());
      if (method === 'PUT' && p === 'models/mode') { policy.mode = body.mode; return delay({}); }
      if (method === 'POST' && p === 'models/rules') {
        if (!body.pattern) err('pattern must be 1–200 characters');
        const r = { id: 'm' + ruleSeq++, pattern: body.pattern, action: body.action, backends: body.backends || [], from_toggle: false };
        policy.rules.push(r);
        return delay(r);
      }
      if (method === 'DELETE' && seg[0] === 'models' && seg[1] === 'rules') {
        policy.rules = policy.rules.filter(r => r.id !== seg[2]);
        return delay({});
      }
      if (method === 'POST' && p === 'models/toggle') {
        const exact = r => r.pattern.toLowerCase() === body.model.toLowerCase();
        if (body.served) {
          policy.rules = policy.rules.filter(r => !(r.action === 'hide' && exact(r)));
          const blocker = policy.rules.find(r => r.action === 'hide' && !r.backends.length && glob(r.pattern).test(body.model));
          if (blocker) err(`'${body.model}' is hidden by the rule '${blocker.pattern}' — remove or narrow that rule`);
          if (policy.mode === 'allow' && !policy.rules.some(r => r.action === 'allow' && !r.backends.length && glob(r.pattern).test(body.model))) {
            policy.rules.push({ id: 'm' + ruleSeq++, pattern: body.model, action: 'allow', backends: [], from_toggle: true });
          }
        } else if (!policy.rules.some(r => r.action === 'hide' && !r.backends.length && exact(r))) {
          policy.rules.push({ id: 'm' + ruleSeq++, pattern: body.model, action: 'hide', backends: [], from_toggle: true });
        }
        return delay({});
      }

      if (method === 'POST' && seg[0] === 'users') {
        const [, id, op] = seg;
        const u = users.find(x => x.id === id);
        if (op === 'vip') { state.vip = state.vip === id ? null : id; if (state.boost === id) state.boost = null; }
        if (op === 'boost') { state.boost = state.boost === id ? null : id; if (state.vip === id) state.vip = null; }
        if (op === 'block') {
          if (isClient(id)) { findClient(id).disabled = true; syncDisabled(); }
          else { u.blocked = true; state.blocked.users.push(id); }
        }
        if (op === 'block-ip' && u.ip) state.blocked.ips.push(u.ip);
        if (op === 'unblock') {
          if (isClient(id)) { findClient(id).disabled = false; syncDisabled(); }
          u.blocked = false;
          state.blocked.users = state.blocked.users.filter(x => x !== id);
          state.blocked.ips = state.blocked.ips.filter(x => x !== u.ip);
        }
        return delay({});
      }
      if (method === 'POST' && p === 'blocked/unblock') {
        if (body.kind === 'client') { findClient(body.value).disabled = false; syncDisabled(); return delay({}); }
        const list = body.kind === 'ip' ? 'ips' : 'users';
        state.blocked[list] = state.blocked[list].filter(x => x !== body.value);
        users.forEach(u => { if (u.id === body.value) u.blocked = false; });
        return delay({});
      }
      if (method === 'POST' && seg[0] === 'backends') {
        const b = backends[Number(seg[1])];
        if (b.active_op) err('a model control operation is already in progress on this backend');
        b.active_op = { verb: seg[2] === 'load' ? 'loading' : 'unloading', model: body.model, elapsed_secs: 0 };
        setTimeout(() => {
          if (seg[2] === 'load') b.models_loaded.push(body.model);
          else b.models_loaded = b.models_loaded.filter(m => m !== body.model);
          b.results = [{ ok: true, verb: b.active_op.verb, model: body.model, error: null }];
          b.active_op = null;
        }, 2500);
        return delay({ model: body.model });
      }

      if (method === 'POST' && p === 'clients') {
        if (!NAME_RE.test(body.name)) err('invalid client name');
        if (clients.some(c => c.name === body.name)) err(`client '${body.name}' already exists`);
        clients.push({ name: body.name, note: body.note, disabled: false, created_at_ms: Date.now(), keys: [] });
        users.forEach(u => { if (u.id === body.name) u.registered = true; });
        return delay({});
      }
      if (seg[0] === 'clients' && seg.length === 2) {
        const c = findClient(seg[1]);
        if (method === 'DELETE') {
          clients.splice(clients.indexOf(c), 1);
          users.forEach(u => { if (u.id === c.name) u.registered = false; });
          if (state.vip === c.name) state.vip = null;
          if (state.boost === c.name) state.boost = null;
          syncDisabled();
          return delay({});
        }
        if (method === 'PATCH') {
          if (body.name !== undefined && body.name !== c.name) {
            if (!NAME_RE.test(body.name)) err('invalid client name');
            if (clients.some(x => x.name === body.name)) err(`client '${body.name}' already exists`);
            // The server moves live state to the new name.
            users.forEach(u => { if (u.id === c.name) u.id = body.name; });
            if (state.vip === c.name) state.vip = body.name;
            if (state.boost === c.name) state.boost = body.name;
            c.name = body.name;
          }
          if (body.note !== undefined) c.note = body.note;
          if (body.disabled !== undefined) { c.disabled = body.disabled; syncDisabled(); }
          return delay({});
        }
      }
      if (method === 'POST' && seg[0] === 'clients' && seg[2] === 'keys') {
        const c = findClient(seg[1]);
        const k = body.key ?? genKey();
        if (k.length < 8 || k.length > 256 || /\s/.test(k)) err('key must be 8–256 characters with no whitespace');
        if (allKeys().some(x => x.k.key === k)) err('that key is already assigned to a client');
        const nk = key(k, body.label, Date.now(), 0);
        c.keys.push(nk);
        return delay(nk);
      }
      if (seg[0] === 'keys') {
        const f = allKeys().find(x => x.k.id === seg[1]) || err('no such key');
        f.c.keys.splice(f.c.keys.indexOf(f.k), 1);
        if (method === 'PATCH') findClient(body.client).keys.push(f.k);
        return delay({});
      }
      err(`demo: unhandled ${method} ${path}`);
    } catch (e) {
      return Promise.reject(e);
    }
  }

  // Tick the in-flight load timer like the real proxy would.
  setInterval(() => backends.forEach(b => { if (b.active_op) b.active_op.elapsed_secs++; }), 1000);

  window.DemoApi = { handle };
})();
