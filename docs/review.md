# Conversation review

[← Back to README](../README.md)

Any conversation in the log can be handed to an LLM for a review: open it in the conversation viewer and press **✎ Review** (it says `✎ Review (2)` once it has been reviewed before, and the dialog lists those reviews). The dialog starts from the default prompt of the **prompt library**; pick another, or edit the text — an edited prompt is sent with that review only and is marked `edited`, the library stays as it is. The model defaults to `settings.review.model` and can be changed per review. The review covers the conversation **as it stands when sent** — its last request is fixed then, so an agent that keeps working does not change what was reviewed — and **Re-run** on a finished review sends the conversation again as it is now, keeping the old answer to compare.

**Reviews are the lowest-priority work the proxy does, and never cost a user anything:**

- A review starts only on a backend with **no user request in flight, none queued that it could serve, and none served for `quiet_secs`** (default 5 min) — measured from when the last user request *finished*, so a review never slips into the seconds between an agent's turns.
- It loads its model only where that model is already resident or nothing is loaded — evicting the model someone was using costs them a reload — unless the backend has been quiet for `swap_idle_secs` (default 10 min).
- **A running review is not counted against the backend's capacity**: a user request that arrives meanwhile is dispatched at once, beside it, never held behind it; the scheduler also prefers, among equally loaded backends, one that runs no review. The review finishes (it is not aborted), and no new one starts there until the backend is quiet again.
- One review per backend at a time, `concurrency` (default 1) in all. Review calls go straight to the backend (non-streamed `/api/chat`, or `/v1/chat/completions` on an OpenAI-only backend, with the backend's token); they are not queued as user traffic and not counted in metrics or the log.

A queued review says why it waits — `waiting for 2:13 more of quiet`, `the backend is serving users`, `another model is loaded — may swap to qwen3:32b in 4:10`, `no backend has qwen3:32b` — on the Reviews page, the nearest reason across every backend that could run it.

**▶ Run now** (on a queued review, or **Send & run now** in the dialog) skips all of that: the review starts at once on any online backend that has its model and is not loading or unloading one — beside users' requests, past the concurrency limit and even with reviews switched off. Without such a backend it stays queued, says why, and starts as soon as one appears.

**What the model reads** is the conversation as a transcript, not the raw log: each turn shows only the messages it added (agents re-send the whole history every turn), then the reply — text, a short reasoning excerpt, tool calls — under a `=== Turn N · +m:ss · model ===` header the review can cite. Each message is cut to `max_message_chars` keeping its head and tail, and the whole transcript to `max_transcript_chars` by dropping turns from the **middle** (a third of the budget for the opening, the rest for the end), with markers where something was cut; the review is flagged `shortened` when that happened. A turn recorded without text (redacted) appears as a placeholder, and a conversation that is redacted throughout cannot be sent. A reasoning model's `<think>` block is stripped from the answer, which is shown as Markdown — headings, nested lists, tables, quotes, rules, code, links (http/https only) and emphasis; the text is escaped first, so a model cannot inject HTML.

**Where it is kept:** each review is one JSON file in `reviews/` next to the conversation log's `sessions/` — outside retention, so a review outlives the conversation it reviewed (re-running one whose conversation is gone fails and says so) — with a copy of the prompt text it was sent with. A review that was running when the proxy stopped is queued again at the next start. The prompt library is `review_prompts.yaml` next to `appconf.yaml`, edited from **Reviews → Prompts…** (the last prompt cannot be deleted; a fresh install starts with one generic review prompt).

```yaml
settings:
  review:
    model: qwen3:32b          # default review model
    quiet_secs: 300           # idle this long before a review starts
    swap_idle_secs: 600       # idle this long before it may load its model
```

The same knobs (and `backend`, `timeout_secs`, `max_transcript_chars`, `enabled`) are on the Settings page under **Conversation review**, and apply at once. API, under `/ui/api` with the web UI's sign-in: `GET /reviews[?conv=<key>]`, `GET /reviews/{id}`, `POST /reviews` `{key, prompt_id?, prompt?, model?, run_now?}`, `POST /reviews/{id}/retry|cancel|run`, `DELETE /reviews/{id}`, `GET|PUT /review-prompts`, `DELETE /review-prompts/{id}`.
