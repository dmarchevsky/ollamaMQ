## global

```flow
defaultBranch = master
release.artifact = docker buildx imagetools inspect ghcr.io/dmarchevsky/ollamamq:{version}
```

## plan

- [ ] (ui) rendered mockups attached for every changed screen

## implement

```flow
test = cargo test
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

Visual-pass recipe (headless shell): screenshotting the *served* app crashes the shell
(zygote socket errors) — render the static bundle instead: `file://<worktree>/src/web/index.html?demo`.
Seed UI state (List/Details, Active/All) via `localStorage` on a `--user-data-dir` profile
(open a tiny `localStorage.setItem('omq.reqView','details');…` page once, then the demo).
Dark mode: copy `app.css` and rewrite `prefers-color-scheme: dark` to `all` —
`--force-dark-mode` does nothing on a page that supports dark itself.
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
implementation completes, kill it before `flow finish`. Run it from a config copy outside
`worktree/` (`.dev-flow/<slug>/live/config/appconf.yaml`) whose `request_log_path` points into
that `live/` dir — the repo `config/appconf.yaml` points at `/data/…`, which does not exist on
this host, so the proxy starts with "request log disabled" and writes no JSONL. `live/`
survives `finish`; prune it with the feature.

Persist as you go: draft findings, calibration constants and mid-stage decisions into
`plan/plan.md` (or a scratch file under the feature dir) as they happen, not only at stage
boundaries — long implement stages otherwise lose them to context compaction.

## release

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
