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

## release

Semver: minor for user-facing features, patch for chores/fixes. The version-bump commit must
include the refreshed `Cargo.lock` — a release commit that leaves the lock stale is incomplete.
Release images are published to GHCR (`ghcr.io/dmarchevsky/ollamamq`) by the tag-triggered
workflow; `release-done`'s artifact probe is the proof — never claim "published" before it
passes. Production runs from the deploy
checkout: `docker compose pull && docker compose up -d`, because compose tracks
`ghcr.io/dmarchevsky/ollamamq:latest`, which CI publishes from the release tag.

```flow
versionFile = Cargo.toml
versionTag = v{version}
```
