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
  ].map((b, idx) => ({ idx, ...b }));

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

  function snapshot() {
    const sum = (f) => users.reduce((a, u) => a + u[f], 0);
    return {
      ...state,
      stats: { queued: sum('queued'), processing: sum('processing'), done: sum('done'), dropped: sum('dropped'), log_dropped: 0 },
      backends: backends.map(b => ({ ...b, models_hidden: b.models_available.filter(m => !served(m, b.url)) })),
      users,
    };
  }

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
