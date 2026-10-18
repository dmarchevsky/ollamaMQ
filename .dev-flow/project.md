## global

```flow
defaultBranch = master
release.artifact = curl -fs "https://hub.docker.com/v2/repositories/dmarchevsky/ollamamq/tags/{version}/" -o /dev/null
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

## release

Semver: minor for user-facing features, patch for chores/fixes. The version-bump commit must
include the refreshed `Cargo.lock` — a release commit that leaves the lock stale is incomplete.
Docker Hub images are published by the tag-triggered workflow; `release-done`'s artifact probe
is the proof — never claim "published" before it passes. Production runs from the deploy
checkout: `docker compose pull && docker compose up -d`, because compose tracks
`dmarchevsky/ollamamq:latest`, which CI publishes from the release tag.

```flow
versionFile = Cargo.toml
versionTag = v{version}
```
