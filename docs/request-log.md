# Request log

[← Back to README](../README.md)

The log is organised **by conversation**, and it stores what a conversation *added*, not what it repeated. An agent that sends sixty turns resends its whole history sixty times, and a streamed reply arrives as hundreds of chunks that each repeat the same envelope — written as captured, one 64-turn session was a 20 MB record holding well under 1 MB of distinct text, and 50 × 10 MB of log covered 37 minutes. So **every conversation has a file of its own**, and each line in it carries the conversation's header and **only the turns that changed** since they were last written; a turn's request keeps **only the messages it added** to an earlier turn's, and a streamed reply is stored as **the one completion it amounts to**. On the archive above that is 787 MiB → 3.5 MiB (0.4 MiB compressed), with every request rebuilding byte-for-byte. The files live next to `settings.request_log_path` (default `ollamamq-requests.jsonl`, whose folder is the log folder):

```text
logs/
  sessions/2026-11-03/alice__a3f19c.jsonl.zst   sealed lines (zstd frames; zstdcat reads them)
  sessions/2026-11-03/alice__a3f19c.jsonl       open tail, lines being appended
  sessions/2026-11-03/alice__a3f19c.meta.json   summary, so a restart lists it without reading it
  sessions/2026-11-03/alice__oneshot.jsonl.zst  that client's requests with no session tag, that day
  sessions/deleted.json                         conversations an operator deleted
  ollamamq-requests.jsonl*                      a shared chain an older build wrote: read-only
```

The folder is the day of the conversation's first turn; the name is the client and its session tag (made file-name safe — the real key is inside every line). A tail is **sealed** after an hour without traffic: compressed as one more frame of the `.zst`, so a conversation that resumes days later continues the same file. A conversation past 256 MiB continues in `….p2`. A line looks like this:

| field | meaning |
| --- | --- |
| `v` | record format version — `4` (lines written by older builds, `2`/`3`, are whole snapshots and are still read) |
| `key` | the store key: `client#session` for conversation traffic, `client·rid` for a request with no conversation tag (one such request is one entry) |
| `user`, `model`, `backend`, `priority` | who and what, as of the newest turn |
| `session`, `session_label` | the sticky-routing conversation tag and the label the client sent (`X-Session-Name`) |
| `start_ms`, `end_ms` | the first turn's arrival and the newest turn's last event, unix ms |
| `redacted` | `true` when any turn had body capture switched off |
| `dropped_turns` | turns trimmed from the front after a conversation hit the 500-turn cap |
| `turns[]` | the turns this line writes or updates, oldest first — below |

Each turn is one proxied request and its response:

| field | meaning |
| --- | --- |
| `rid` | correlation id, shared by a request and its response |
| `at_ms` | when the request arrived, unix ms |
| `tl` | `queue`, `dispatch`, `first_token`, `end` — unix ms, stamped at the moment each happens. Stages a turn never reached are absent, so a gap in `tl` is a fact and not a guess |
| `method`, `path`, `status`, `outcome`, `model`, `backend`, `priority` | the request line, its answer, and how it ended (`ok`, `timeout`, `client_gone`, `dropped`, …) |
| `req`, `resp` | `{bytes, chars, truncated}` per direction — sizes, never text |
| `prompt_tokens`, `completion_tokens`, `waste_secs` | usage as reported, plus prefill seconds lost to a KV-cache eviction |
| `kind`, `link` | `"compaction"` on a request that summarizes a conversation (or `"post-compact"` on the first turn after one), plus how it was attached to the conversation it belongs to (`header`, `recency`, `content`) — both absent on ordinary traffic |
| `msgs[]` | one entry per request message — `role`, `chars`, and `name`/`calls` when it carries a tool name or issues tool calls. Roles and sizes, never text. With `msgs_base`/`msgs_keep` it continues the first `msgs_keep` entries of turn `msgs_base`'s list |
| `redacted` | body capture was switched off for this turn |
| `content` | the captured text, cut to `log_content_limit` with a `...[truncated: N bytes total]` marker; absent when nothing was stored (or when an earlier line of the same file already holds it). `req` is the whole request body, **or** `req_d` = `{base, keep, add, tmpl}`: the first `keep` messages of turn `base`'s request, then the messages in `add`, inside `tmpl` (the request object with `"messages":null`; absent = same as `base`'s). `resp` is the reply — for a successful streamed turn a `chat.completion` object folded from the stream (content, thinking, tool calls, finish reason, usage, `sse_chunks`); an errored turn keeps its stream as captured |

**When a line is written, and how to read it back.** A conversation is appended once it has been quiet for `request_log_flush_secs` (default 5 s — a crash should cost the last few seconds of a conversation, not the last minute; nothing is `fsync`-ed, so a hard power loss can still take what the operating system had not written). The line carries the turns that changed since the last one; a turn is written again only when something about it changes (its response lands, an audit verdict arrives), and then without the text the file already holds. **A conversation is all the lines of its `key`, folded oldest first**: header fields from the newest line, turns merged by `rid` (a later record of a turn replaces its fields, text it leaves out carries over), `msgs` and `req_d` resolved against the turn they name. Every such reference stays inside the conversation's own file (a new part starts over with whole bodies). Lines from older builds (`v` 2/3) are whole snapshots and fold by the same rule; a legacy shared chain is still listed and opened, and a conversation present in both reads its legacy lines first. You do not have to implement any of this: **`cargo run --release --example convlog_tool -- export /srv/logs`** prints one line per conversation with every request rebuilt whole — the shape `jq` and DuckDB want — reading compressed files and hiding deleted conversations; `convlog_tool compact` rewrites an old chain in the new form, and `convlog_tool check` proves on your own logs that nothing is lost. The server hands out the same record at `GET /ui/api/conv/export?key=…`. The viewer reads turn text in batches from `GET /ui/api/conv/contents?key=…&rids=12,15` (`{turns: [{rid, req, resp}]}`, every turn when `rids` is left out), which folds the conversation once for all of them. A conversation that resumes after a restart simply appends to its history. The in-memory log keeps the 200 most recent conversations and at most 64 MiB of captured text.

**Where the file goes, and what happens if it cannot be opened.** A relative `request_log_path` (the default bare name included) resolves from the folder holding `appconf.yaml` — where the proxy was launched no longer matters, like every other state file. An older install whose log already lives in the working directory keeps using that file (the startup log says so) until it is moved next to `appconf.yaml`. Missing directories inside `request_log_path` are created at startup. A path that still cannot be opened — a container path such as `/data/…` on a host without one — falls back to `ollamamq-requests.jsonl` next to `appconf.yaml`, the store folder the other state files live in, and the log says so (`conversation log: <configured> is unusable here, writing to <actual> instead`) instead of starting with capture off and leaving every log row unopenable. Capture applies from boot: changing the path in Settings takes effect on restart. The Logs page shows the path being written to, and when nothing could be opened it says so above the table and badges the rows `not captured`.

**Log capture.** Every client, every API key and every backend carries a `redact_log` switch (web UI checkbox **Redact log — do not save request/response text**, `redact_log: true` in `clients.yaml` / `appconf.yaml`). When it is on, the request is served exactly as before and the turn is still written — timeline, sizes, message roles, status, token counts — with no body text, so `content` is absent and `redacted` is `true`. Nothing about the traffic changes: queues, priority, metrics and the response the client sees are untouched. Only *new* requests are affected; lines already in the file keep what they stored. The web viewer renders those conversations in full — turn blocks, gaps and timeline — and says *text redacted* where the text would be, instead of pretending the structure is missing.

On top of the configured switches, any single request can opt itself out by sending the **`X-Redact-Log`** header: its presence is the flag (the value is ignored), it works with or without a client key, and it hides only that request's text. Like the config switches it is one-way — a request can never force text to be stored that a client, key or backend has switched off.

- A **client** rule covers its requests in both directions and every key of that client.
- A **key** rule covers that key's requests in both directions. Redaction is an opt-out: a key whose client is marked stays covered whatever the key says.
- A **backend** rule covers the **response** text of requests it served. A request's own text is logged before a backend is chosen, so that half is the client/key's job.
- The **`X-Redact-Log` header** covers the single request that carries it — request and response text alike, keyless requests included.

**Deleting a conversation:** the viewer's **Delete** button (click twice to confirm) or `DELETE /ui/api/conv?key=…` removes it everywhere it is kept — memory, its own files, and (hidden by `deleted.json`, since those files are shared) any lines it left in a client's one-shot file or a legacy chain, until those files go too. The Logs rows stay (they come from the Activity journal) and turn inert. A deleted conversation that keeps talking starts a new history from there.

**Retention keeps or drops whole conversations.** Every ten minutes, conversations whose last turn is older than `request_log_max_days` (default 30) are removed, then the oldest until everything fits `request_log_max_total_bytes` (default 1 GiB, bytes on disk, a legacy chain included — its files go first, being oldest). A conversation still in memory or with an open tail is never touched, so a review, a download or an audit replay always sees a conversation from its first turn. Measured on heavy agent traffic the log grows well under 1 MB per hour compressed, so 1 GiB is months; the age limit is what usually decides. `request_log_max_bytes` / `request_log_max_files` are retired: the boot log names the new keys when they are still set. Everything retained is *visible*, not just stored: at startup the index reads the summaries (no text held) and the viewer reads a conversation's file when you open it, rebuilding requests from their deltas — so the Conversations view covers every day the files keep, even one the hot memory long forgot.

**Web UI:** the **Logs** page shows the newest live log events (one line per request and per response, from the in-memory ring); clicking a row opens the conversation viewer. It is independent of the file format above.

> **Note:** request/response headers are deliberately **not** logged — this keeps API keys out of the file.
