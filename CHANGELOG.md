# Changelog

All notable changes to this fork of [ollamaMQ](https://github.com/Chleba/ollamaMQ)
since upstream `v0.5.0`.

## v0.23.0 — 2026-10-28

### Added

- **The Activity history and the Logs page now survive a restart.** Until now the
  Overview's Activity remembered only what the current process had served — every
  finished request journaled to `ollamamq-activity.jsonl` (next to the request log,
  size-rotated like it) comes back at boot, so a restart no longer blanks the
  Overview or the Activity curve, and a conversation can be opened from the log file
  even when its text long ago left memory: the conversation log is now indexed over
  the whole rotation chain (each conversation's place in the files, body text read
  lazily), not just the last seeded tail. The **Logs** page opens with its history
  too — restored rows carry the recorded outcome (`← ok`, `← timeout`), count as
  requests in the grouped activity line, and open their conversation straight from
  disk like any other row; refused traffic without a conversation stays listed but
  inert. Restored rows also land on the right request after a restart: the log
  correlation ids restart at 1 with every boot, so the counter now starts above
  every id the journal and the conversation log still carry.
- `history_hours` (default 24) sets how far the Activity history reaches: the
  longest Overview window, the age journaled records replay at, and the ceiling of
  the filter modal's window picker — at `48` the modal grows `24 h` and `48 h`
  presets and the custom window follows. `history_max_records` (default 50 000)
  caps how many finished requests are kept whatever their age; retention is sized
  by the request log's rotation (`request_log_max_files`), and both are live-editable
  in Settings.

### Changed

- The conversation viewer's *Expand all* / *Collapse all* are now the same `⊞`/`⊟`
  icon buttons as in the Activity list.

## v0.22.0 — 2026-10-27

### Added

- **Compaction is visible now, instead of looking like a mystery conversation.** When a coding agent
  runs out of context it asks the model to summarize what it has and carries on from that summary —
  and on the proxy that call arrived as a brand-new conversation with an enormous prompt and a cold
  cache on a warm backend. The proxy now recognizes those requests (pi / pi-web / OpenClaw,
  opencode, Claude Code, Cline, Roo Code — matched on the whole marker sentence **opening a
  message** in the first 32 KiB of the body, never on a keyword and never on a quoted mention, so a
  turn that merely talks about summarizing stays ordinary; `X-Compaction: 1` is the explicit path), attaches each one to the conversation it summarizes (same client, key and
  model, active within 120 s; `X-Compaction-Of: <tag>` names the parent exactly, and a tie stays
  explicitly **unlinked** rather than being guessed), and hands it the parent's affinity, priority
  and conversation row. In Activity the summary is one striped graphite block on its parent's row,
  the row is marked `⤓`, Details reads `compaction 71k tok → 1.1k tok · recency`, the window counter
  says `⤓ Compact N` per conversation that compacted, the Logs page names the request
  (`body=285694B · compaction`) and the conversation viewer marks the turn — text redacted or not,
  because the marker is read from the body in memory and nothing new is stored. The summarization
  call is no longer charged as cache waste: its cold prefill is the client rewriting its own
  history, not the backend losing it.
  [README](README.md#-context-compaction)

## v0.21.0 — 2026-10-26

### Changed

- **The conversation viewer reads like a log now, not like a dump** (pure frontend). Requests
  between two final answers wrap as one **run** (`#1–#3 · 2m 30s · ×6`), folding per run as well
  as per request; failures tint every row they touch (`client gone` stays neutral). Message blocks
  fold on their label — system prompts start folded — and thinking renders as a ruled appendix box.
  The jump tree speaks the transcript's vocabulary: answers are green `ASST` rows (opening words +
  duration, no re-sent echoes), clicks land on the block's first line, and the tree follows the
  wheel. `Incremental` and a quiet 5 s `Auto-refresh` (keeps scroll, open blocks and folds) join
  the header; `⟳` keeps your place now too.

### Fixed

- A `[data-action]` checkbox click no longer has its toggle cancelled by the global click handler —
  static header checkboxes toggle on the first click.

## v0.20.0 — 2026-10-26

### Added

- **Per-request log redaction — the `X-Redact-Log` header.** Any request can keep its own body
  text out of the conversation log by carrying the header: its *presence* is the flag, the value
  is ignored (even empty), and no client key is required — keyless requests honor it too. The
  entry is still written with status, size, timing and message roles; only the text is left out,
  exactly as with the per-client/key/backend `redact_log` rules. The flag is one-way: a request
  can hide its own text, but can never force text to be stored that a client, key or backend has
  switched off. Like its sibling headers it reaches the backend forwarded unchanged
  (see README [Request logging](README.md#request-logging)).

### Changed

- **The UI says `redacted` where it said `no text`.** The badge in the Logs list (both views),
  the conversation viewer and the client/key/backend rows, the jump-rail stamps and the
  downloaded transcript all carry the new word, dialog checkboxes now read
  **Redact log — do not save …**, and tooltips and help lines name the sources of redaction —
  including the new header — so the mechanism is discoverable from the UI. Stored log format and
  the admin API are untouched (`redacted` was already the field name); old log lines render with
  the new word.

## v0.19.0 — 2026-10-24

### Breaking

- **The terminal dashboard is gone; `/ui` is the dashboard.** `src/tui.rs` (1695 lines) and the three
  crates only it used (`ratatui`, `crossterm`, `tracing-appender`) are deleted, and with them the two
  startup paths the dashboard needed. Logs now always go to **stderr at `debug`** — what `--no-tui`
  used to select — instead of an `ollamamq.log` file at `info`, and there is one server start path.
  `--no-tui` is still accepted and ignored (one `WARN` at startup), so existing Docker, systemd and CI
  command lines keep starting and can drop the flag at leisure. Everything the dashboard's keys did is
  already a control in the web UI: model **Load**/**Unload**, **Reload config**, VIP/Boost/Block/Block
  IP/Unblock, session pins, and the live log with its conversation viewer.
- **One dashboard key had no web equivalent and is removed with it:** `P` on a client raised the level
  of every request that client had waiting *and* pinned the conversations behind them, so their next
  turn kept it (`AppState::pin_user_sessions`). VIP and Boost change standing priority but leave what
  is already queued where it is, and the web UI pins one conversation at a time, so "lift this
  client's whole backlog right now" is no longer a single action. It was deleted rather than carried
  as dead code; bringing it back means a button and an API op, not a hidden function.

### Removed

- The dashboard's own demo recordings (`demo.gif`, `demo-linkedin.gif` and their `.tape` scripts) and
  the README hero image pointing at them — both showed the terminal UI. The Logging section now
  describes one sink instead of two modes.

## v0.18.0 — 2026-10-24

### Breaking

- **`/admin/*` fails closed.** Model control (`GET /admin/models`, `POST /admin/models/load`,
  `POST /admin/models/unload`) used to follow the proxy's *optional* key: with `OLLAMA_MQ_API_KEY`
  unset — the usual single-machine setup, and the one the dashboard is built for — anyone who could
  reach the port could read the fleet and load or unload models on it. Those routes now answer to
  the operator: the proxy key when one is set, else the dashboard's own credential (a session
  cookie or `curl -u user:pass`), and only when neither exists, an enabled client key from
  `keys.toml`. Anything else gets `403` naming what to configure, and the startup log now says
  which door is open. A script that called `/admin/*` with no credential needs one now.

### Fixed

- **The queue ceiling no longer outgrows its container.** `docker-compose.yml` shipped `mem_limit: 256m`
  against a default `max_queued_bytes` of 512 MiB: the flood defence was an OOM restart instead of a
  `503`. The limit is now 1 GiB, and the proxy reads its own cgroup limit (`memory.max`, or
  `memory/memory.limit_in_bytes` under v1) and halves a queue ceiling above half of it, logging that
  it did — in the shipped container and in any other. The clamp also applies when the setting is
  changed live, so raising it in the UI cannot reopen the hole.
- **A failed log rotation stopped rotating, forever.** `rotate()` swallowed every error and the writer
  zeroed its running size regardless, so when a rename failed (a read-only mount) the conversation log
  kept appending past `request_log_max_bytes` and the accounting said zero from then on. The writer now
  logs the failure once per attempt and re-reads the file's real size, so the ceiling keeps biting.
- **A crash cost a minute of conversations, not five seconds.** `request_log_flush_secs` defaulted to
  60: an open conversation was written out only after a minute of quiet, which is exactly the
  conversation that was in flight. The default is 5 s.
- **The model list stopped re-probing the fleet on every poll.** `GET /v1/models` asked every online
  backend for its models on each call, so a dashboard left open on the Models page kept hitting all of
  them several times a second. Results are cached for 2 s (and dropped when the backend set changes),
  and the probes go out 8 at a time instead of all at once.
- **Idle cost, twice over.** The per-second snapshot cloned the whole 20 k-request history to sum
  prefill/waste seconds per backend, and the 10 fps change-detection diff compared every captured
  request and response body (up to 1 MiB each) even though the view shows a 30-character preview. The
  waste totals are now counters accumulated as requests finish, and the snapshot carries a preview and
  a "has text" flag instead of the text; opening a row fetches that row's text by `rid`. A proxy with
  nothing happening stopped copying megabytes per second to prove nothing had changed.
- **A forged `X-User-ID` could squat in the identity column.** The header arrived verbatim and was
  displayed and labelled as-is, so a client could send an id that looked like a key holder's namespace
  or a 5 KB blob. It is now clipped to 64 printable ASCII characters, control characters and
  whitespace collapse to `-`, `:` is dropped, and the header still reaches backends unchanged.
- A deprecated `fetch_update` call site moved to `try_update`.

### Added

- **CI.** `.github/workflows/test.yml` runs on every push and PR: clippy with `-D warnings`,
  `cargo test`, `node --check` on the three dashboard scripts, and the end-to-end suite against the
  mock backends. Until now the only workflow ran on a release tag, so the first person to
  see a broken build or a failing test was whoever tagged it.
- **The tree is clippy-clean**, so that gate means something: 23 findings went away — let-chains over
  nested `if`s, `sort_by_key`, two long tuples given names, a derived `Default`, and a test that held a
  client-store lock across an `await`. Two `too_many_arguments` stay, allowed where they are, because
  those argument lists *are* the rows the functions draw.
- **`.gitignore` covers what the tooling drops in the tree**: config and log archives.

### Removed

- `test_dispatcher.sh`, its `demo-test.gif` and the README section pointing at them: a 149-line
  script that hammered a running proxy with 50 fake users, was not a test anyone could run unattended,
  and had already drifted from the API it exercised. The README's Development section now names the two
  suites that do run.
- A dead design note (`docs/plan-system-one-backends.md`, the decisions it describes shipped long ago)
  and a test helper nothing called.

## v0.17.0 — 2026-10-23

### Changed

- **A streamed response is finally readable in Formatted.** The viewer rebuilds the stream into the
  answer it produced (text, thinking, tool calls, finish reason, usage) instead of printing the wire, but
  it only recognised a stream whose very first line was `data: {…}`. llama.cpp opens with a
  `: keepalive` comment and keeps the socket warm with empty deltas, so real streams fell through to a
  raw dump — a 156 KB wall of `data: {"id":…}` for one answer. Detection now looks at the first line
  that carries payload, and the keep-alives, empty deltas and `[DONE]` are dropped from the view (`Raw`
  still shows everything as captured).
- **`+` now means one thing in the timeline: a moment.** Response and idle rows are durations, but they
  wore the same `+` as the request stamps, so a column mixing `19:03:59`, `+52.5s`, `+45.2s` and `44.6s`
  asked you to guess which were times and which were lengths. Request rows are moments (`+52.5s` after
  the conversation began), response/idle/message rows are lengths (`45.2s`), and each tooltip says which
  of the two it is.
- **The `≣ View` chips are one column now.** The stage rows had no right inset, so their chips sat 9 px
  outside the header chips; they share the inset now and the pills line up edge to edge.
- **Conversation timestamps count from its start, not from the turn before.** Each request used to be
  stamped with the gap since the previous one, so three turns spaced a steady 7 s apart read
  `+7.0s | +7.0s` — a number that says nothing and repeats itself. Now the first request keeps the
  absolute clock and every stamp after it counts continuously from the conversation's start
  (`+7.0s`, `+14.0s`, `+21.0s`), in the log and the jump tree alike. Things that are lengths rather than
  moments keep their length: a response's duration, an idle gap, the window that produced a message.
- **The jump tree is resizable.** Drag its left edge — it widens to show tool names and message text, or
  narrows to a glance beside the log — with `←`/`→` for the keyboard and double-click (or `Home`) to
  reset to 292 px. The width is remembered, and on a phone, where the tree is a sheet across the top,
  there is nothing to drag.
- **The timeline reads like `dmesg -H` all the way down.** A conversation's first request keeps the
  absolute clock (`17:30:14`); every request and node after it is a delta — `+18s` between two requests,
  `+6.2s` for an idle gap, `+18.4s` for how long a response took — with the absolute time on hover. Idle
  gaps are named from 5 s (was 30 s), so the seconds an agent spends running a tool show up in both the
  log and the jump tree instead of looking like an instant.
- **No `Held` row in the jump tree.** The tree lists only what each request added and says
  `3 new · 7 held` on the request row; opening the held history stays a body action, where the messages
  actually appear.
- **Fonts on the Overview are consistent.** The `≣ View` chip inherited monospace from the stage row it
  sits in, so the header's chip and the row's chip looked different; the chip now names the interface face
  itself and is identical in both. The `By conversation / By request` switch on the Logs page also sits
  on the toolbar line with the filter, the select and Pause instead of riding 7 px above them.
- **A request in the viewer shows what it added, not the whole conversation again.** Every turn after the
  first re-sends the entire history, so the viewer used to repeat it: turn 4 reprinted the messages turns
  1–3 had already drawn. Each turn now collapses what it merely re-sent into one line —
  `▸ 7 messages held from request 1 · system 1 · user 2 · assistant 2 · tool 2 · ≈799 B — show them here
  too` — and clicking it (in the body or in the jump tree, which share the fold) puts those messages inside
  that request too. The request line keeps both halves: `10 messages · 3 new this turn`.
- **The jump tree and the log body are now one fold.** The caret on a request row in the tree and the caret
  on the request's header in the body are the same control: folding either folds both, so the tree no
  longer claims a request is open while the body has it shut. The tree's request row also says
  `3 new · 7 held` now instead of a bare message count.
- **Conversation headers and stage lines carry one and the same `≣ View` chip** (it said
  `≣ view conversation` on the header), at the same right edge of the row.
- **The Overview has one list now — Details — and it says how fast you are being served.**
  The **List / Details** switch is gone (with it the phone's card layout and the wide twelve-column
  table): the Activity panel always threads conversations, one line per stage event. A conversation
  header now carries the rates an active session is getting instead of just the word `decode` or
  `prefill`: `decode 88.7 t/s` from the live stream and `prefill 349 tok/s` measured on the last
  finished request in that conversation — a request cannot time its own prefill, so while its first
  token is still out the header shows `prefill 4.2s` and puts the last measured rate in its tooltip.
  The `≣ view conversation` chip moved to the right edge of the header, where the stage lines below
  already keep their `≣ View` chips, and both are now `cursor: pointer` (they used to show a text
  cursor over their own label).
- **The conversation viewer stops repeating itself, and the jump tree grew real timings.**
  A turn no longer shows the messages the previous request already sent — those are on screen in
  the turns above — so the `7 earlier messages, already sent in the previous request — expand`
  fold is gone and the request line says `7 messages · 3 new this turn` instead; the downloaded
  transcript does the same. In the `≡ Jump` tree each request carries one absolute clock (when it
  arrived) and everything after it is a delta from that, dmesg `-H` style: the response row is
  `+18.4s`, and a message row carries the measured window that produced it — a `TOOL` result is the
  stretch between the previous answer ending and this request arriving, an `ASSIST` row the answer
  that wrote it. System and user rows stay blank, because a message that arrives inside a request has
  no instant of its own, and the tooltip says so instead of inventing a time. The tree is also
  indented far tighter (34 px → 18 px per level), leaving the snippet the room it needs.
  Along the way: three `//` comments in `app.css` — not CSS comments — were swallowing the rules for
  `.capnote`, `.logs .sess` and `.lv-node` whole, so the capture banner, the session chip and the
  tree layout were silently unstyled.
- **The Logs page is one row per conversation by default.** A chat session makes twenty calls, and
  twenty rows that each repeat part of the previous one hid what the session was doing. The table now
  switches between **By conversation** — one row per client and session, naming the session as its
  client labelled it and summing what it sent and got (`→ 4 req · 1.3 MB ← 200 · 92 KB`, with `waiting`
  or `2 failed` when either is true) — and **By request**, the old one row per round, which is what a
  single call that misbehaved needs. Control actions and traffic that sent no session token stand alone
  in either view, and the choice is remembered across sessions. `GET /ui/api/logs` gained
  `session_label`, so a row reads `#cache-warmup` instead of the six-hex tag. Clicking a grouped row
  opens that conversation on its newest request, so the turns are walked without going back to the list.
- **The request log is now one line per conversation (`v: 2`), and the log viewer follows it.**
  A chat client resends its whole history every turn, so one line per request stored the same
  messages over and over: `src/reqlog.rs` became `src/convlog.rs`, a conversation is one JSON
  object that grows and is appended again once it has been quiet for `request_log_flush_secs`
  (default 60 s) — the last line per `key` is the complete one, and startup re-seeds from the file's
  tail. Every stage (queue, dispatch, first token, end) is now stamped with the unix time it happened,
  so the Overview prints measured times instead of back-computing them from durations, and the idle
  stretch between two turns can name the tools that ran in it. The viewer opens a **conversation**
  (from a Logs row, or from any `≣ View` chip in Details), shows every turn as a block on one
  timeline, folds the messages a turn resent from the previous one, walks request by request with
  `⏮ ‹ › ⏭`, reloads, and downloads the conversation as a markdown transcript or as the log line;
  `≡ Jump` is an expandable request tree. The `Whole / New only` switch is gone — the resent history
  is folded per turn instead. Conversations whose capture was switched off render their full structure
  and timeline with *capture turned off* where the text would be. The TUI's Requests panel and the
  in-memory event ring are unchanged. **Breaking for log readers:** the JSONL format is `v: 2`, one
  line per conversation; `v: 1` files are still rotated and never rewritten.
- **Capture failure is now loud instead of silent.** Missing directories inside `request_log_path`
  are created at startup, and a path that still cannot be opened (a container path like `/data/…` on
  a host without one) falls back to `ollamamq-requests.jsonl` next to `appconf.yaml` with a warning
  naming both paths. Before, that config booted with the log disabled and every Logs row failed to
  open with the same message an aged-out conversation gives. `GET /ui/api/capture` reports whether
  capture runs and where it writes; the Logs page shows the path, badges rows `not captured` and
  explains when nothing is captured.
- **Developer playbook only, no product change.** Three stale lines of the dev-flow playbook were
  replaced with what actually works. Dark-mode screenshots were lying: the recipe rewrote
  `prefers-color-scheme: dark` to `all`, which is not a valid media query, so the dark block never
  applied and every "dark" render was a copy of the light one — the recipe now uses `min-width: 0`
  and carries a probe that proves the emulation took before a shot is trusted. The log-viewer UI
  assertion driver (`tests/web/demo_check.driver.js`) is documented at last — how to splice it into a
  copy of the web bundle, the flag that surfaces its `ASSERT PASS/FAIL` lines, and why it has to open
  a round that stored text. And the test gate now runs `node --check` over the three embedded web
  bundles before `cargo test`, so a syntax error in `src/web/*.js` can no longer pass while the UI is
  broken.

### Fixed

- **A downloaded transcript wrote every request twice.** Each request section opened with two
  `**Request**` lines — the plain one and the one carrying the message count — so the file a reviewer
  got held a duplicated heading per request. The viewer never showed it (the duplicate lived only in
  the markdown writer), and nothing looked at that file; the transcript is checked heading by heading
  now.
- **The jump tree folded everything else whenever you opened one request.** Body blocks and tree
  nodes shared one open-set, and focusing a request replaced it (`LV.open = new Set([rid])`), so
  expanding a node collapsed the rest — and because every node of the focused request was marked as
  current, an expanded request painted a solid wall of highlight. Tree nodes now have their own
  state, each request opens and closes independently, and only the row in view is marked. Clicking a
  message row (`USER` / `ASSIST` / `TOOL`) now focuses *that message* — opening the turn, unfolding
  the resent-history block if the message lives in it, scrolling it into view and flashing it —
  instead of only scrolling to the request header. Only the request row and the response row carry a
  clock time now (arrival and finish); the `req` row and the message rows dropped the request's
  second repeated on every line, which read as a measurement that was never taken.
- **`≡ View conversation` in Overview → Details failed for keyed traffic.** The chip sent the display
  name (which becomes `client:key` when the identity filter is by key) where the store matches on the
  client, so the viewer answered *no such conversation in the log*. It sends the client identity now.

## v0.16.0 — 2026-10-22

### Added

- **Log capture switch.** A client, one of its API keys or a backend can be marked
  `redact_log` (`redact_log: true` in `clients.yaml` / `appconf.yaml`, or the
  checkbox on its row in the web UI). The request is served exactly as before and
  its log entry is still written — user, model, backend, status, byte counts,
  session, timing — but no body text is captured, so the line carries
  `"redacted":true` and a null `content`. Such a round is marked `no text` on the
  Clients, Backends and Logs pages, and the log viewer says the text was switched
  off instead of showing an empty body. A key can only opt out: its client's
  setting always wins. A backend's rule covers its **response** text — a request's
  own text is logged before a backend is chosen, so that half belongs to the
  client/key.

### Changed

- **The Clients page is one list with two levels.** A client row carries the
  controls that used to sit in the side panel (VIP, boost, Block IP, + Key,
  Edit/Disable/Delete) and opens its keys as rows underneath; Expand all / Collapse
  all works over the list. On phones the controls share one line (VIP and boost
  collapse to `★` / `⚡`) and each key becomes one nested card.
- A key is **edited or deleted, never moved**: the Edit dialog changes its label and
  its capture switch, and the key string itself is read-only — moving a key to
  another client means deleting it and handing out a new one.

### Fixed

- The Clients page no longer scrolls sideways below ~360px — the action cell carried a
  `min-width` its row never needed — and VIP/boost keep an `aria-label` on the phones
  where the layout hides the word.

### Breaking

- `PATCH /ui/api/keys/{id}` no longer accepts `client`; it takes `label` and/or
  `redact_log` and answers `400` when neither is present. Scripts that moved a key
  must delete and re-add it.

## v0.15.0 — 2026-10-22

### Added

- **dmesg-style stamps in the log viewer's Jump outline.** The outline now carries
  a time column like `dmesg -H`: the request's section opens with its absolute
  time, and anything within a minute of it shows as a short offset (`+0s`,
  `+18s`) — so a reply that took 18 seconds reads at a glance. A section or
  message whose event is missing (still awaiting a response) carries no stamp.

### Fixed

- **The log viewer's scrollbars make sense.** A modal dialog no longer leaves
  the page behind scrollable — the scrollbar at the screen edge was the Logs
  list's, not the viewer's, and dragging it scrolled content nobody could see.
  And the viewer's own scrollbar, thinned to near-invisibility, is visible again
  where the content actually scrolls.

### Changed

- **Developer playbook only, no product change.** The dev-flow playbook now documents the live
  test-instance recipe — `8081` is the dev-walk port (and collides with the compose mapping),
  how to spot a stale proxy pinning it, and running the walk from a config copy with a writable
  `request_log_path` — and tells the agent to persist findings into `plan/` during long
  implement stages.

## v0.14.0 — 2026-10-21

### Added

- **Human-readable request logs.** Every log event and live-run row now carries a
  correlation id and the session tag, and the web UI uses both: the **Logs** page
  pairs a request with its response into one compact line per round
  (`→ request · ← response`), and clicking a round opens a full-screen viewer —
  the request as a message transcript, a streamed reply reconstructed into one
  assistant message (thinking collapsed, tool calls rebuilt, token footer),
  `Raw ↔ Formatted`, `Whole ↔ New only` (the diff against the conversation's
  previous round), a `≣ Jump` message outline, `↑/↓` round navigation and an
  always-visible scrollbar. Captures truncated by the content limit are salvaged
  — complete messages and intact chunks render, with a note saying what the
  capture kept. The captured-text default rose to **1 MiB** (`log_content_limit`);
  the JSONL file format and the TUI are unchanged.
- **Overview → log jumps.** Details stage lines and conversation headers carry
  permanent `≣ Logs` chips: a stage line opens that request's round in the
  viewer, the header opens the Logs page filtered to the conversation's session.

### Fixed

- A checked preset in the Overview filter modal now wins over leftover custom
  text — picking a preset after typing "last N" used to keep the custom window.
- **Capacity** colours conversations (client + API-key label in label mode)
  exactly like Activity does; before, capacity segments keyed on the bare client.

## v0.13.0 — 2026-10-20

### Added

- **Session labels.** A client sends `X-Session-Name` and its conversation reads
  `#refactor-auth` instead of `#a3f19c` — in Activity (graph lane, List, Details,
  phone cards), in the request log and on the run rows. The label is display
  text only: it stays out of the session key, so renaming a session cannot
  re-key it and lose its sticky backend. Kept to 64 characters, lowercased,
  control characters and edge separators stripped.
- **Three priority levels** (`normal` / `high` / `top`) from `X-Session-Priority`.
  `high` goes first inside its own client's queue; `top` goes first on the whole
  board (ahead of the VIP client), is never held back by the cache hold-back, and
  may overbook the backend its conversation is warm on by exactly one slot.
  Nothing running is ever cancelled to make room.
- `settings.client_priority: off | self | global` (default `global`) caps how far
  a client's own claim reaches. Keyless requests never get a say, and an operator
  pin always wins — in both directions, including holding a session at `normal`.
- **Priority on the operator's three surfaces.** The icon beside a session *is*
  the control: click cycles normal → high → top (`▲` violet = top, `△` berry =
  high, grey `○` = none yet, so every live session carries one). The status line
  counts what sits above normal, the Filters modal can show only those, and
  `POST /ui/api/sessions/<tag>/priority` `{"level":"top"}` does it in one call.
  In the TUI, `P` on a client sets everything that client has waiting and marks it
  `[HGH]` / `[TOP]`.

### Changed

- Activity groups its backends **alphabetically** instead of by most recent
  activity, so the panel stops rearranging itself between polls. Sessions keep
  their existing order inside a group (newest start first).
- An operator priority pin is keyed by the six-hex **session tag** the UI shows,
  not by the conversation behind it. Pinning any visible session succeeds
  immediately — including during its first request, when the conversation is not
  yet resolvable anywhere — and lands on the conversation's next turn. The old
  `404 no live conversation carries that session tag` is gone; a malformed tag is
  a `400`. The Overview now also returns `status.pins`, and the icon beside a
  session shows the pin (a finished row keeps its historical level underneath).

## v0.12.2 — 2026-10-20

### Added

- **Cache-waste metric:** the time lost when a conversation's context was
  evicted from a backend's KV cache and the next turn had to re-prefill it.
  Each turn's prefill time is split between the work even a perfect cache
  could not skip (the previous answer re-entering the prompt, new content)
  and the evicted prefix — the latter is the waste. `GET /metrics` gains
  `ollamamq_cache_waste_seconds_total{client,model,backend}` and
  `ollamamq_cache_waste_events_total{backend}`; the waste share is
  `rate(cache_waste) / rate(prompt_seconds)`. The Web overview shows a Waste
  column in the request Details, the Backends and Clients panels and the
  client detail; the TUI Backends table grows a Waste column; the request log
  records `waste_secs` per served request. Backends without per-request
  prefill telemetry (plain Ollama) read as `—` — absence, never zero.

## v0.12.1 — 2026-10-20

### Fixed

- **Key-label rename actually works:** the 1 s poll used to repaint the Clients
  page and tear the inline editor down ~1 s after opening it — with the editor
  gone, Esc/Enter did nothing either. The editor now stays open while you type;
  Enter saves (or closes if nothing changed), Esc or clicking away cancels.
- The editor's input no longer stretches the Label column, push the action
  buttons out of the panel and spawn a horizontal scrollbar.
- The expand all / collapse all pair now hides in List view (its `.btn-group`
  display was overriding the browser's `[hidden]` rule).

### Changed

- Activity's controls (Active/All, List/Details, expand/collapse) moved from
  the panel header to a right-aligned row **under the graph** — the header
  stays one line on phones.
- **expand all / collapse all** are now icon buttons (**⊞ / ⊟**) with tooltips.

## v0.12.0 — 2026-10-20

### Added

- **Key-label rename.** The Clients key table grew a `Rename` button beside
  Copy/Reassign: the Label cell becomes an input, Enter saves (Esc cancels).
  `PATCH /api/keys/{id}` now accepts `{label}` next to `{client}`.
- **Expand all / collapse all** in Activity's Details view, folding every
  conversation at once (remembered per browser).

### Fixed

- Filters modal: the custom “last N” interval now applies (it was silently
  losing to the preset radio), sits on the presets' baseline, and switching to
  *Clients + label* repopulates the client list immediately.
- The little `all` labels are working **all · none** toggles; *none* empties
  the panels as an explicit empty scope.
- Ticking a single backend (or client in label mode), then reopening the modal
  and pressing Apply no longer resets the filter — the checkbox lists are
  built from the unfiltered state instead of the already-scoped payload.
- The modal's Models list no longer offers hidden models.

## v0.11.0 — 2026-10-19

### Added

- **Filters modal.** The top-bar window selector and client chip row are gone; a
  `Filters` button opens one modal that picks the interval (5/15/30/60 min or
  “last N” minutes/hours, up to 24 h), lists and filters clients, backends and
  models (multi-select), and switches client identity between bare names and
  `client:key` — the API key label, shown everywhere in Activity and the Clients
  table. The selection scopes every Overview panel and is remembered per browser.
- **Session-stable order.** Graph and Details list conversations by when each
  session started (newest start first) — a running session keeps its place
  instead of jumping to the top with every request.
- **Lane hover.** Hovering a swimlane (track or its client label) lights the
  whole row in that conversation's client colour.

### Changed

- **Stage colours reverted:** prefill is yellow again, decode the old teal,
  waiting the hollow blue. Yellow means prefill and nothing else: the live
  highlight moved from yellow to a client-colour wash on List rows and Details
  stage lines, and the graph reserves that wash for hover.
- Overview history is kept for 24 h (was 1 h), so long intervals have data.

## v0.10.1 — 2026-10-19

### Fixed

- **Prefill speed is now honest on cache hits.** The Details prefill line divided the
  whole prompt by the prefill time, so a 100% prefix-cache hit — most of the prompt
  skipped, a tiny measured time — printed rates like `200,121` tok/s. It now divides
  the tokens actually evaluated (the cache-excluded count the Backends table already
  uses), and shows no rate when that telemetry is missing.

## v0.10.0 — 2026-10-19

### Added

- **Activity: one session order, live at a glance.** The graph and Details now list
  conversations by most recent activity — same order everywhere. Running and waiting
  requests carry a single yellow tint instead of the old active/finished divider: live
  rows in the List, live stage lines in Details, and a current-state chip on every
  conversation header (`decode` / `prefill` / `waiting`, none when idle). Details
  headers also align their `req / tok / time` summary in right-aligned columns.
- **Graph click-to-focus.** Clicking a conversation in the graph focuses it wherever you
  are looking: a folded Details block unfolds, a conversation hidden behind "Active" is
  revealed, then the row scrolls into view and flashes.

### Changed

- **Status-consistent colours.** Prefill is accent blue in the graph, the badge and the
  Details square (was amber); decode uses the app's green; the waiting badge is an
  outlined blue matching its graph chip; one new `--live` yellow marks everything
  active. Client identity colours were re-picked (cyan, pink, orange, orchid, lime,
  sand, terracotta, berry/rose) to avoid every status hue — no swatch can be misread as
  a stage any more.
- Activity panel header: title, Active/All and List/Details now share one line on
  phones (wrapping back to two lines only below ~360px).

## v0.9.3 — 2026-10-18

### Added

- **Cache-aware dispatch.** When a backend reports KV-cache telemetry (llama.cpp/halogen
  with `--metrics`) and per-request `prompt_eval` timings, two mechanisms replace the plain
  per-request round-robin that thrashes a cache too small for all active conversations:
  - *Hold-back*: a conversation whose last prefill missed the cache — and whose estimated
    next-turn increment (evaluated + decoded last turn) does not fit the free KV — queues
    behind the conversations the backend served most recently, instead of forcing a context
    switch and a full re-prefill every round. Bounded: 16 yields per request, and a
    conversation is never held on its first two turns.
  - *Slot reservation*: a session that finished a turn keeps queue priority for its own
    follow-up while its adaptive window is open — 1.5x the session's worst recent
    turn-to-turn pause, clamped 2–30 s (client tool time measured 0.5 s median / 4.3 s max
    in the reference logs) — so a whole agent cycle runs on one warm cache. 64 reserved
    wins in a row force one ordinary fair round; nobody ever waits on an idle session.
  Both fail open: no session identity, no telemetry, or an unmeasurable cache means the
  exact scheduling behaviour of `v0.9.2`. New counter
  `ollamamq_cache_scheduling_total{action="held_back"|"reserved_first"}`; `kv % free` on
  the backend card is the pressure signal. Replay of a saturated Brainiac hour (484
  requests, one backend): **−50% prefill, −80% avoidable cache waste (210 → 20 full
  re-prefills), mean queue wait 856 s → 674 s** — simulator and method in `tests/sim/`.

## v0.9.2 — 2026-10-18

### Changed

- **Docker Hub is no longer published to.** GHCR (`ghcr.io/dmarchevsky/ollamamq`) is the only
  registry: the release workflow lost its Docker Hub login, so nothing uses `DOCKERHUB_USERNAME` /
  `DOCKERHUB_TOKEN` any more (delete them from the repo's Actions secrets). The Hub image stays as
  it is, frozen at `0.9.1` — an install that still pulls it has to switch to the GHCR name.
- `release-done` proves a release against GHCR now
  (`docker buildx imagetools inspect ghcr.io/dmarchevsky/ollamamq:{version}`) instead of the
  Docker Hub tag API.

## v0.9.1 — 2026-10-18

### Changed

- **Release images now live on GHCR: `ghcr.io/dmarchevsky/ollamamq`.** Same tags as before
  (`latest`, `<version>`, `<major>.<minor>`), published by the same tag-triggered workflow. GHCR
  needs no registry secrets (the workflow's own `GITHUB_TOKEN` logs in) and public packages there
  carry no anonymous pull limit — Docker Hub caps anonymous pulls at ~100 per 6 h per IP, which
  bites shared homelab and CI addresses. The shipped `docker-compose.yml` pulls from GHCR now.
- **Docker Hub keeps getting every tag** as `dmarchevsky/ollamamq`, so installs created before this
  release keep updating. New installs should use the GHCR name; the Hub mirror goes away once GHCR
  has shipped a release or two.
- Two `docker run` examples pointed at `chlebon/ollamamq`, an upstream namespace that never published
  this fork — they now name the GHCR image. The manual *Publishing to Docker Hub* section is gone:
  publishing happens by pushing a release tag, the workflow is the documentation.

## v0.9.0 — 2026-10-17

### Added

- **Client filter on the Overview.** A chip row under the top bar lists every client seen in the
  selected window; picking one scopes the counters, Capacity, the timeline and both request lists to
  that client alone (`GET /ui/api/overview?client=<id>`). Free slots, backend health and the slots
  themselves stay backend-wide — they belong to the backend, not to the client. The choice is
  remembered per browser.
- **One Activity panel with a per-conversation timeline.** The old Activity swimlanes and the
  Requests panel merged into a single panel. The graph draws one row per conversation, grouped under
  the backend it ran on: each request paints one block per stage — waiting, prefill, decode — placed
  where that stage began and as wide as it ran, so durations are readable as length and not only as
  numbers. Stage colours are fixed (they match the legend); client identity is the swatch by the name.
  Rows step down with the axis by session start, cap at 12 per backend with `+n older sessions
  hidden`, and a stage cut off by a window edge is clipped and marked `‹`.
- **Details view.** Under the graph, `List | Details` replaces the old grouping. Details threads each
  conversation: one line per stage event with its own timestamp and numbers (prompt size, prefill
  time, `cache 48%` for a warm prompt, tokens, seconds, tok/s), a `waiting` line with its own start
  for a queued request, an `idle` line where a conversation sat quiet for over 30s, and one line for
  a request that ended without serving (`client gone`, `blocked`). Clicking a timeline row opens
  Details at that conversation.
- **A readable Overview on a phone.** Request rows become three-line cards — client, `model → backend`,
  counts, timing — the panel header keeps its title and both switches on one line, and a
  client/session-keyed colour scheme keeps conversations distinguishable at 390px.
- **Version in Settings.** The Settings page prints the running ollamaMQ version, so a board and a
  bug report can be matched without shell access.

### Changed

- The top bar no longer shows **Drop** / **LogDrop**; dropped requests stay in the per-client table,
  the TUI and `/metrics`. The window selector's counters are Queue / Running / Done.
- `Active n | All n` carries the counts that used to be a sentence above the list, and the divider
  between running and finished rows is a hairline rather than a label.

### Fixed

- Stage timings on the graph were anchored from the wrong zero point: `ttft_secs`, `gen_secs` and
  `elapsed_secs` are measured from *dispatch*, while `waited_secs` happened before it, so a request
  that had queued was drawn in the wrong place. Stages now rebuild as
  `arrival = finish − waited − prefill − decode`.
## v0.8.1 — 2026-10-13

### Changed

- `docker-compose.yml` runs the published Docker Hub image (tracking `latest`) instead of
  building the checkout locally: `docker compose pull && docker compose up -d` upgrades to
  the newest release; pin a version tag to make upgrades deliberate again.

## v0.8.0 — 2026-10-13

### Changed

- **Overview redesign.** The KPI cards and Recent requests panel are gone. Capacity bars now
  size to each backend's slots with clearly visible free space; waiting rows share the bar's
  token scale so a queue that outgrows a backend is shown as overflow, not a hidden number.
  Activity became a per-backend swimlane of stacked, per-client histograms; buckets are aligned
  to the wall clock and show the busiest instant in the bucket, so a bar never exceeds the slot
  count and closed buckets never re-height.
- The window selector moved to the top bar. There **Done** and **Drop** count the selected window
  (completed / dropped), mirroring the lifetime counters they replace; Queue and Running stay live.
- The Requests table shows prefill as `size · s.s` (one decimal, ≈ while estimated), a new Gen
  time column, first token, and prefix-cache use for finished requests.

### Added

- **Request grouping.** Requests carry a short session tag — the client's `X-Session-Id`, its
  `prompt_cache_key`, or a fingerprint of the conversation — and the Requests table groups by it:
  *Ungrouped* (newest first), *Threads* (a header per session with its turns under a dotted lane),
  or *Chain* (Ungrouped's order with a git-graph-style lane threaded through it).

## v0.7.0 — 2026-10-13

### Added

- **Sticky routing (session affinity)**: a conversation is remembered by the backend that
  served its previous turn and prefers to go back there, so a long multi-turn prompt is not
  re-prefilled on a backend that has no KV cache for it. `settings.sticky_routing` picks the
  key — an explicit `X-Session-Id` / `prompt_cache_key`, a fingerprint of the conversation's first turn
  (default), or the client id; `sticky_ttl`, `sticky_lag` and `sticky_max_entries` bound the
  memory. It is a preference, not a pin: a busy, offline or no-longer-serving backend loses
  the conversation and the mapping follows the move. New counter
  `ollamamq_affinity_decisions_total{result}`; the payoff shows up as lower
  `ollamamq_prompt_eval_tokens_total` per client.

## v0.6.0 — 2026-10-12

First tagged release of the fork. Everything below is on top of upstream `v0.5.0`.

### Breaking (config layout)

- The web UI now edits `appconf.yaml` itself — the `settings.json` overlay and
  `admin.json` are gone. A leftover `settings.json` / `admin.json` next to the
  config is merged into `appconf.yaml` on first start and renamed to `.bak`.
  A UI save rewrites `appconf.yaml`, so file comments do not survive a save;
  the previous version is kept as `appconf.yaml.bak` for "Undo last save".
- The UI-changed password is stored as an argon2id hash at
  `settings.web_ui.password_hash` in `appconf.yaml` (wins over
  `OLLAMA_MQ_UI_PASSWORD` / `settings.web_ui.password`; delete it to recover).
- Client and model-policy stores are now hand-editable YAML next to
  `appconf.yaml`: `clients.yaml`, `models.yaml` (old `clients.json` /
  `models.json` convert automatically on first start and are kept as `.bak`).
  Hand-written files may omit ids/timestamps; keys may be bare strings and
  hide rules plain patterns.
- `settings.settings_file` / `settings.admin_file` no longer exist (ignored if
  present); defaults for `clients_file` / `models_file` are the `.yaml` names.
- Store files resolve from the folder holding `appconf.yaml`, so the whole
  configuration lives in one folder (`config/` in the default compose file);
  request logs stay in the working directory. The old layout in the working
  directory keeps being used and is reported at startup.

### Added

- Web UI at `/ui`: sign-in with session cookies (basic auth still works for
  scripts), overview dashboard with live capacity, activity and per-backend /
  per-client speed, waiting requests on the capacity board, clients and API-key
  management, model filtering page, backends page, settings editor, password
  change, favicon; mobile layout without horizontal scrolling.
- Clients and API keys: per-client keys (`omq-…` or any string), queue identity
  by key, VIP/boost persistence, disable/rename/move-key semantics.
- Model filtering: hide/allow glob rules per backend (`models.yaml`), hidden
  models answered `404`, merged model listings respect the rules.
- Prometheus `/metrics`: per-client and per-model requests, tokens, bytes,
  latency; free slots, queue by model, per-key tokens; time to first token and
  generation speed; `settings.metrics_token`.
- Model aliases (`settings.model_aliases`) and built-in Olla-style URL prefixes.
- Requests in progress with live token counts (`/api/ps`-style "Running now").
- Backend capacity auto-detected from reported slots (`detect_backend_slots`).
- Backend `name` and `disabled` fields; per-backend UI labels.
- Config-folder docker-compose becomes the default deployment: `config/` and
  `logs/` folders; the service runs without the TUI (`docker logs`).

### Fixed

- Metrics: per-request series start at zero (first values no longer lost);
  llama.cpp stream token counts from final-chunk timings.

### Upstream changes merged into this fork

- `kind: systemone` backend pinning and related Laya-style support (present in
  upstream `v0.5.0` history at the merge base).
