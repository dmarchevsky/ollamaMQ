# Deployment

[← Back to README](../README.md)

ollamaMQ is a single static binary. Run it from the Docker image, as a systemd service, or straight from a terminal. It does not need Ollama on the same machine — it only needs to reach your backends over HTTP.

## Install from source

Requires [Rust](https://rustup.rs/) (the toolchain is pinned in `rust-toolchain.toml`).

```bash
cargo install --git https://github.com/dmarchevsky/ollamaMQ
# or
git clone https://github.com/dmarchevsky/ollamaMQ.git && cd ollamaMQ && cargo install --path .
```

Then run it against your backends:

```bash
ollamaMQ --backend-urls http://localhost:11434,http://gpu-box:11434
# or with a config file
ollamaMQ -c /etc/ollamamq/appconf.yaml
```

## Docker

Release images are published to GHCR as
[`ghcr.io/dmarchevsky/ollamamq`](https://github.com/dmarchevsky/ollamaMQ/pkgs/container/ollamamq)
(tags: `latest`, `<version>`, `<major>.<minor>`, pushed automatically on every release tag).
The old Docker Hub image (`dmarchevsky/ollamamq`) stopped receiving builds at `0.9.1` — installs
that still pull it need to switch to the GHCR name.

### Docker Compose

The included [`docker-compose.yml`](../docker-compose.yml) keeps everything in one host folder, with relative paths only:

```
ollamamq/
├── docker-compose.yml
├── config/   → /config   appconf.yaml, .env, clients.yaml, models.yaml
└── logs/     → /logs     ollamamq-requests.jsonl* (request log; appconf.yaml sets request_log_path: ../logs/ollamamq-requests.jsonl)
```

1. Copy `docker-compose.yml` into an empty folder (or run it in this checkout).
2. Create `config/appconf.yaml` and `config/.env` (`OLLAMA_MQ_UI_PASSWORD=…`). Set `request_log_path: ../logs/ollamamq-requests.jsonl` in `appconf.yaml` so the conversation log lands in `logs/`.
3. Run `docker compose up -d`, then open `http://<host>:8081/ui` (the compose file maps host port **8081** to the proxy's 11435; change `ports:` to taste).

Upgrading is `docker compose pull && docker compose up -d`. To run your own build instead of the published image, replace `image:` with a `build:` section whose `context` is a checkout of this repository.

**Backends on the Docker host:** use `http://host.docker.internal:<port>` in `appconf.yaml`. On Linux, the server must listen on `0.0.0.0` (for Ollama: `OLLAMA_HOST=0.0.0.0`), and the firewall must let the Docker bridge reach that port.

**Moving to another server:** stop the service and copy the folder. The files in `config/` are written by the container as root with mode 600, so copy them with `sudo` (or `docker run --rm -v "$PWD":/s alpine tar …`).

**Where store files go:** relative paths for `clients_file`, `models_file`, and `request_log_path` are resolved from the folder that holds `appconf.yaml`, so by default they sit next to it. A conversation log whose configured path cannot be opened also lands in the `appconf.yaml` folder (see [Request logging](request-log.md)).

Older setups also had a `settings.json` (the web UI's overlay) and an `admin.json` (the password changed in the UI). At startup their values are merged into `appconf.yaml` once and both files retire to `.bak`.

Earlier versions kept the store files (and the request log) in the working directory. If such a file exists there and not next to `appconf.yaml`, it keeps being used, and the startup log says so. To switch, move it next to `appconf.yaml`.

### Dockerfile

The Dockerfile uses a multi-stage build:

- **Build stage**: Uses `rust:1.97-alpine` (pinned) to compile the release binary
- **Runtime stage**: Uses `alpine:3.20` with only `ca-certificates` for a minimal footprint (~10MB)

### Docker CLI

```bash
docker run -d --name ollamamq -p 11435:11435 --restart unless-stopped \
  -v "$PWD/config:/config" \
  -e OLLAMA_MQ_UI_PASSWORD=change-me \
  --entrypoint /app/ollamaMQ \
  ghcr.io/dmarchevsky/ollamamq:latest -c /config/appconf.yaml --host 0.0.0.0
```

Without a config file, the image entrypoint builds the command line from environment variables instead (`BACKEND_URLS`, `PORT`, `HOST`, `TIMEOUT` — see [Configuration](configuration.md#environment-variables)):

```bash
docker run -d --name ollamamq -p 11435:11435 \
  -e BACKEND_URLS=http://host.docker.internal:11434 \
  --add-host host.docker.internal:host-gateway \
  ghcr.io/dmarchevsky/ollamamq:latest
```

To listen on another port, set `PORT` (or `settings.port`) and map it: `-p 8080:8080 -e PORT=8080`.

## Running as a systemd service

To run `ollamaMQ` as a background service with full log visibility in the journal:

1. Copy the provided service file to your system's unit directory:

   ```bash
   sudo cp ollamamq.service /etc/systemd/system/
   ```

2. Edit the file and set the correct `ExecStart` path to your installed binary (e.g. `~/.cargo/bin/ollamaMQ`) and your backend URLs:

   ```ini
   ExecStart=/usr/local/bin/ollamaMQ --port 11435 --backend-urls http://localhost:11434
   ```

   > **Note:** If you installed `ollamaMQ` with `cargo install`, the binary is at `~/.cargo/bin/ollamaMQ`, **not** `/usr/local/bin/ollamaMQ`. Point `ExecStart` at the real path, or create a symlink so the default path works:
   >
   > ```bash
   > # Find the real path
   > which ollamaMQ
   >
   > # Option A: edit ExecStart to use the real path
   > # Option B: symlink it to the expected location
   > sudo ln -s "$HOME/.cargo/bin/ollamaMQ" /usr/local/bin/ollamaMQ
   > ```

3. Reload systemd, then enable and start the service:

   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable --now ollamamq
   ```

4. Follow the logs in real time via the journal:

   ```bash
   journalctl -u ollamamq -f
   ```

   Or show the most recent 100 lines:

   ```bash
   journalctl -u ollamamq -n 100
   ```

Because logs default to `debug` on stderr, **all** dispatcher events (backend health, request routing, errors) are captured in the journal — no separate log file needed.

## Logging

`ollamaMQ` uses [`tracing`](https://docs.rs/tracing). Logs go to **stderr** at a default level of `debug`, so you see everything — backend health checks, per-request routing, backend detection, and errors. That makes the systemd journal, the Docker log, or a piped file the only log you need; there is no separate log file.

Override the level at any time with the standard `RUST_LOG` environment variable:

```bash
# Quieter console (info + errors only)
RUST_LOG=info ollamaMQ

# Everything, including per-request dispatcher detail
RUST_LOG=debug ollamaMQ
```
