# ollamaMQ — dev-flow playbook

<!-- Operator-owned and tracked: a change rides a docs flow and a PR. -->

## global

```flow
defaultBranch = master
```

```flow-guard
protect config :: live backend credentials (untracked, real keys) — never echo or edit them without the operator
```

## plan

- [ ] (ui) rendered mockups attached for every changed screen

## implement

```flow
test = cargo clippy --all-targets --locked -- -D warnings && node --check src/web/app.js && node --check src/web/demo.js && node --check src/web/login.js && cargo test
build = cargo build --release
run = cargo run
healthWait = 0
```

```flow-check health
cargo build --release -q || exit 1
cfg="$(git rev-parse --path-format=absolute --git-common-dir)/../config/appconf.yaml"
target/release/ollamaMQ --no-tui -c "$cfg" --port 8099 >/tmp/devflow-health.log 2>&1 & pid=$!
for i in $(seq 1 15); do curl -fs http://127.0.0.1:8099/health >/dev/null && break; sleep 1; done
curl -fs http://127.0.0.1:8099/health; rc=$?
kill $pid 2>/dev/null; for i in 1 2 3 4 5; do kill -0 $pid 2>/dev/null || break; sleep 1; done
kill -9 $pid 2>/dev/null; wait $pid 2>/dev/null
exit $rc
```

The health probe builds first (`flow check` runs no build, and a fresh worktree has no binary), reads the
main checkout's untracked `config/` through `git rev-parse --git-common-dir` (worktrees carry no config),
waits up to 15 s for readiness, and stops its instance with TERM, then KILL — a bare `kill` once left a
zombie holding :8099 that answered a later probe for the wrong build. It retries itself, hence
`healthWait = 0`.

The repo is not rustfmt-clean under the pinned toolchain (a `cargo fmt` drive-by reformats ~17 files):
hand-format new code to the surrounding style and never run a `cargo fmt` sweep inside a feature flow.

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
stamps) and must open a turn that has stored text — a redacted turn legitimately has nothing to scroll,
so the body-scrollable check goes silently wrong once the demo carries a redacted turn first; the driver
skips rows marked `redacted` for exactly that reason. The shell's screenshot can still capture a stale frame
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
(`exe -> …/ollamaMQ (deleted)`) and silently serves the wrong build. With two flows live in parallel,
:8081 may be held by the *other* flow's worktree binary — then walk on another free port
(`--port 8092`) and kill yours before finish. Start it only after
implementation completes, kill it before `flow finish`. Run the worktree binary against the
repo's own config by absolute path: `-c "$(git rev-parse --path-format=absolute --git-common-dir)/../config/appconf.yaml"` — real
backends, real credentials, sign-in-able with the operator's saved login; no config copy, no
test password, no `.dev-flow/<slug>/live/` dir (retired). `config/` is untracked, so worktrees
carry no config of their own — the `flow-check health` probe resolves it the same way.

**Log placement (since v0.23.1):** `request_log_path` resolves from the folder holding
appconf.yaml, so a local run starts from anywhere — including the main folder, as plain
`ollamaMQ -c config/appconf.yaml`; the old `cd logs && …` launch convention is retired. The
ONE log folder is repo-root `logs/` for both worlds, reached by construction: appconf.yaml
(gitignored — set it once on this machine and on the production deploy) carries
`request_log_path: ../logs/ollamamq-requests.jsonl`, and the container maps the same folder
via `./logs:/logs`. An install whose log still lives in an old cwd keeps using that file via
the legacy fallback (startup says `request log: writing … in the working directory`) until it
is moved. Confirm with `GET /ui/api/capture` that capture runs and where it writes.
Gotchas: never run container and native simultaneously (shared rotation chain; port 11435
collides anyway); `config/`, `logs/`, `logs*/` and `ollamamq-requests.jsonl` are gitignored —
never commit runtime log state.

Persist as you go: draft findings, calibration constants and mid-stage decisions into
`plan/plan.md` (or a scratch file under the feature dir) as they happen, not only at stage
boundaries — long implement stages otherwise lose them to context compaction.

## pre-release

```flow
versionFile = Cargo.toml
versionTag = v{version}
```

Semver: minor for user-facing features, patch for chores/fixes. The version-bump commit must
include the refreshed `Cargo.lock` — a release commit that leaves the lock stale is incomplete.
The changelog has no `## Unreleased` section, so every release is versioned; a `--no-version` flow
needs that heading added first.

## release

```flow
merge = squash
artifact = docker buildx imagetools inspect ghcr.io/dmarchevsky/ollamamq:{version}
preflight = docker build -q -t ollamamq:preflight .
```

`preflight` builds the release image from the worktree before anything ships: the tag-triggered
`docker-release` build is otherwise the only check of the Cargo layout in the Dockerfile's cache stage,
and v0.27.0 broke it twice after tagging (#47, #48).

**No tag on red CI:** the local `test` command runs the same `cargo clippy --all-targets --locked
-- -D warnings` as the hosted `test` workflow, and `rust-toolchain.toml` pins the toolchain both
sides use — so lint fallout must surface at implement, never in CI. Still, the merge commit is the
first commit the hosted workflow actually runs on, and `docker-release` publishes from the tag
regardless of the `test` verdict: v0.22.0–v0.24.0 all shipped on red CI. So after the merge lands and
before tagging, wait for the `test` run on the merge commit and require green:
`concl=$(gh run list --workflow=test --branch master --commit "$(git rev-parse origin/master)" --json conclusion -q '.[0].conclusion // "no run"'); [ "$concl" = success ] || { echo "test workflow on the merge commit: $concl"; exit 1; }`
A red or missing run means fix-forward with a new PR (or abort the tag), never tag anyway.

- [ ] `test` workflow green on the merge commit before the tag (`gh run list --workflow=test --branch master --commit <merge-sha>`)

Release images are published to GHCR (`ghcr.io/dmarchevsky/ollamamq`) by the tag-triggered
workflow; `release-done`'s artifact probe is the proof — never claim "published" before it
passes. Production runs from the deploy
checkout: `docker compose pull && docker compose up -d`, because compose tracks
`ghcr.io/dmarchevsky/ollamamq:latest`, which CI publishes from the release tag.
