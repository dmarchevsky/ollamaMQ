## global

```flow
defaultBranch = master
release.artifact = docker buildx imagetools inspect ghcr.io/dmarchevsky/ollamamq:{version}
```

## plan

- [ ] (ui) rendered mockups attached for every changed screen

## implement

```flow
test = node --check src/web/app.js && node --check src/web/demo.js && node --check src/web/login.js && cargo test
build = cargo build --release
run = cargo run
```

```flow-check health
target/release/ollamaMQ --no-tui -c /home/dima/work/ollamaMQ/config/appconf.yaml --port 8099 >/tmp/devflow-health.log 2>&1 & pid=$!
sleep 2
curl -fs http://127.0.0.1:8099/health; rc=$?
kill $pid; exit $rc
```

- [ ] (ui) visual pass in light **and** dark at 390px and 1280px: no overflows, no stray scrollbars, legible contrast
- [ ] app runs locally against real backends (not `?demo`); primary views walked with live data
- [ ] test instance on :8081 killed before `flow finish` — `pgrep -a ollamaMQ` is empty

Visual-pass recipe (headless shell): screenshotting the *served* app crashes the shell
(zygote socket errors) — render the static bundle instead: `file://<worktree>/src/web/index.html?demo`.
Seed UI state (List/Details, Active/All) via `localStorage` on a `--user-data-dir` profile
(open a tiny `localStorage.setItem('omq.reqView','details');…` page once, then the demo).
Dark mode: copy `app.css` and rewrite `prefers-color-scheme: dark` to `min-width: 0` —
`--force-dark-mode` does nothing on a page that supports dark itself.
Never rewrite to `all`: `@media (all)` is an unknown media feature, the block is dropped, and the dark
shots come out byte-identical to light. Prove the emulation took before trusting a shot — `--dump-dom` a
page that prints `getComputedStyle(document.documentElement).getPropertyValue('--bg')` (the dark block
sets it): `#0f1218` with `color-scheme: dark` is dark, `#f4f5f7`/`light` means the block never matched.

Automated UI pass (`tests/web/demo_check.driver.js`): the repo's only scripted UI assertion harness, and
nothing runs it by itself — `cargo test` never loads it. Copy `src/web/{index.html,app.css,app.js,demo.js,
login.js}` to a scratch dir, splice the driver's body into the copied `index.html` as a `<script>` before
`</body>`, then run it and grep the console: `<headless-shell> --no-sandbox --disable-gpu
--enable-logging=stderr --virtual-time-budget=10000 --window-size=1280,800 --screenshot=out.png
"file://<copy>/index.html?demo=1" 2>&1 | grep ASSERT`. `--enable-logging=stderr` is what surfaces the
`ASSERT PASS/FAIL <name>` lines (the harness is silent without it); the virtual-time budget must exceed the
driver's 3 s timeline. It drives the log viewer (background scroll-lock, body scrollability, `≣` rail
stamps) and must open a round that has stored text — a redacted round legitimately has nothing to scroll,
so the body-scrollable check goes silently wrong once the demo carries a redacted round first; the driver
skips rows marked `no text` for exactly that reason. The shell's screenshot can still capture a stale frame
of the modal; the assertions are trustworthy, the PNG is a coin-flip.
Gotchas: each shot starts at the page top-left (no scroll) — for a lower region hide what is
above with an inline `<style>`; a `--user-data-dir` profile persists between shots, so use a
fresh one per intended UI state; for a transient edit state (e.g. the inline key-label editor)
freeze repaints after opening it (`window.render=function(){}`) or the 1 s poll tears it down
before the shot — stub only after the demo data has loaded, or the page renders empty. For a
single component, render a static strip (tiny HTML linking `app.css`, just the markup) instead
of the whole demo app; embed mockups in plan.md only as `![alt](mockups/x.png)` and re-verify
every tag resolves to a file before Approve?.

Live-walk recipe (test instance): `8081` is the test-instance port — the dev build being
verified must be the one answering there (`--port 8081 --host 0.0.0.0`); `8099` is only the
`flow-check health` probe's ephemeral port. `docker-compose.yml` also maps `8081:11435`, so the
production container and a dev walk contend for the same port: before starting, check
`ss -ltnp | grep :8081` / `pgrep -a ollamaMQ`, and re-check if the UI shows stale behaviour —
a proxy left running pins the port after its worktree is rebuilt or deleted
(`exe -> …/ollamaMQ (deleted)`) and silently serves the wrong build. Start it only after
implementation completes, kill it before `flow finish`. Run the worktree binary against the
repo's own config by absolute path: `-c /home/dima/work/ollamaMQ/config/appconf.yaml` — real
backends, real credentials, sign-in-able with the operator's saved login; no config copy, no
test password, no `.dev-flow/<slug>/live/` dir (retired). `config/` is untracked, so worktrees
carry no config of their own — the `flow-check health` probe resolves it the same way. Its
`request_log_path: /data/…` is the container's log mount and does not exist on this host, but
since v0.17.0 capture falls back to `config/ollamamq-requests.jsonl` next to appconf.yaml and
warns (`conversation log: … writing to … instead`); confirm with `GET /ui/api/capture` that
capture runs and where it writes.

Persist as you go: draft findings, calibration constants and mid-stage decisions into
`plan/plan.md` (or a scratch file under the feature dir) as they happen, not only at stage
boundaries — long implement stages otherwise lose them to context compaction.

## release

Before `flow release`, `git status --porcelain` in the worktree must be empty: `flow check` and
`flow release` do not report uncommitted paths (only `flow verify` prints them), and the squash
ships without them — v0.17.0 released with a review fix to `app.css` left uncommitted.

Semver: minor for user-facing features, patch for chores/fixes. The version-bump commit must
include the refreshed `Cargo.lock` — a release commit that leaves the lock stale is incomplete.
Release images are published to GHCR (`ghcr.io/dmarchevsky/ollamamq`) by the tag-triggered
workflow; `release-done`'s artifact probe is the proof — never claim "published" before it
passes. Production runs from the deploy
checkout: `docker compose pull && docker compose up -d`, because compose tracks
`ghcr.io/dmarchevsky/ollamamq:latest`, which CI publishes from the release tag.

```flow
merge = squash
versionFile = Cargo.toml
versionTag = v{version}
```
