# Vela

![CI](https://github.com/34Herveonly/Vela_Project/actions/workflows/ci.yml/badge.svg)

> **Status:** v0.1.0 — production ready. Deployed automatically on merge to main.

A single-binary service health orchestrator for lean Linux deployments.

Drop one binary on any Linux server. Vela watches your services,
detects failures, reroutes traffic, restarts crashed processes,
and sends alerts — with no cloud account, no Kubernetes, no agents.

## What Vela solves

Teams running microservices on cheap VPS (DigitalOcean, Hetzner, bare metal)
have no lightweight tool that gives them health monitoring, automatic recovery,
and traffic routing without the operational overhead of Kubernetes.
Vela fills that gap as a single self-contained binary.

## Architecture

Vela contains seven internal engines:

| Engine | Responsibility |
|---|---|
| Config engine | Parses, validates, and backward-compat-synthesizes `config.toml` |
| Docker engine | Optional async client for container status, restart, and repull |
| Health engine | Runs TCP/HTTP/Docker checks per upstream on a per-service interval |
| Recovery engine | Restarts failed services (manual command, Docker, or none) with backoff |
| Alert engine | Fires webhook/log notifications on status changes |
| Proxy engine | Health-aware reverse proxy with round-robin failover across upstreams |
| API engine | Exposes REST endpoints for status and control |

All engines share a single in-memory state store — no database required.
A `vela-ctl` CLI and an embedded web dashboard (served at `GET /` on the
API port) are included for operating a running instance.

## Quick start (native Rust)

**Prerequisites:** Rust 1.85+ installed via [rustup](https://rustup.rs)
(a transitive dependency requires edition-2024 support; see the `Dockerfile`
builder stage for the pinned toolchain used in CI/image builds)

```bash
# Clone the repository
git clone https://github.com/34Herveonly/Vela_Project.git
cd Vela_Project

# Copy and edit the example config
cp config.example.toml config.toml
# Edit config.toml — set your api_key and add your services

# Build in release mode
cargo build --release

# Run Vela
./target/release/vela config.toml

# Or run in development mode with auto-reload
RUST_LOG=debug cargo watch -x 'run -- config.toml'
```

## Quick start (Docker)

```bash
# Build the Docker image
docker build -t vela:latest .

# Run with your config file mounted.
# Mount the Docker socket too if any service uses restart.mode = "docker" —
# this lets Vela's Docker engine inspect and restart containers on the host.
docker run -d \
  --name vela \
  --network host \
  -v $(pwd)/config.toml:/etc/vela/config.toml:ro \
  -v /var/log/vela:/var/log/vela \
  -v /var/run/docker.sock:/var/run/docker.sock \
  vela:latest

# Check logs
docker logs -f vela

# Stop
docker stop vela
```

See [Docker setup](#docker-setup) below for socket permissions and what
happens if the daemon isn't reachable.

## Configuration

Copy `config.example.toml` to `config.toml` and edit it.
Full reference:

```toml
[global]
api_port = 7700                          # Vela REST API port
api_key  = "your-strong-secret-here"    # Required. Protect this.
log_dir  = "/var/log/vela"

[[services]]
id                  = "auth-service"    # Unique ID, alphanumeric + hyphens
name                = "Auth Service"    # Display name in alerts
check_interval_secs = 10
failure_threshold   = 3
max_restarts        = 5

[services.health_check]
kind      = "http"       # "tcp" or "http"
http_path = "/health"    # Required when kind = "http"
timeout_ms = 3000

[services.restart]
mode = "manual"          # "manual", "docker", or "none" — see below
command = "systemctl restart auth-service"   # required when mode = "manual"

[services.proxy]
listen_port = 8001       # Vela proxies traffic from this port to the upstream(s)

[[services.upstreams]]
host = "127.0.0.1"
port = 3001

[[services.alerts]]
kind     = "webhook"
endpoint = "https://hooks.example.com/vela"
enabled  = true
```

### Service configuration modes

Every service picks one **restart mode**, set under `[services.restart]`:

| Mode | Behavior | Required fields |
|---|---|---|
| `manual` | Runs a shell command (systemd, a custom script, anything) | `command` |
| `docker` | Restarts a container via the Docker API directly — no shell command | `[services.restart.docker]` block (`container`, optional `image`) |
| `none` | Monitor and alert only — Vela never attempts recovery | none |

`none` is for services Vela cannot control: a managed cloud backend, a
third-party API. Health checks and alerts still run normally; the recovery
engine just skips the restart step.

Setting `[services.restart.docker].image` enables **repull-and-restart**:
instead of a plain restart, Vela pulls the image, then stops, removes, and
recreates the container under the same name with the same port bindings and
host config. Omit `image` for a plain restart of the existing container.

Every service also declares one or more `[[services.upstreams]]` — the
actual `host:port` pairs the proxy forwards to and the health engine checks.

- **Monolith** — one service, one upstream, optionally one Docker container.
  This is the common case (see Example 1/2 in `config.example.toml`).
- **Microservices with failover** — one service, multiple `[[services.upstreams]]`
  entries (e.g. several replicas). The proxy round-robins across whichever
  upstreams are currently `Healthy`; a failed replica is removed from
  rotation automatically and rejoins once it recovers (see Example 3).

**Backward compatibility:** the old `host` / `port` / `command` fields at
the top level of `[[services]]` still work exactly as before. If you have
an existing `config.toml` from an earlier version of Vela, you don't need
to change anything — at load time Vela synthesizes `upstreams` (from
`host`/`port`) and `restart` (from `command`, or `mode = "none"` if there's
no `command`) automatically.

### Docker setup

Docker support is entirely optional and never fatal. If Vela can't reach
the Docker daemon at startup, it logs a warning, disables Docker-backed
health/restart features, and keeps running normally — `manual` and `none`
mode services are completely unaffected.

To enable it:

- **Native install:** run Vela as a user in the `docker` group (or root),
  so it can reach the default Docker socket.
- **Docker container:** mount the host socket into the Vela container —
  `-v /var/run/docker.sock:/var/run/docker.sock` (see the Quick start above).
  The image runs as a non-root `vela` user for security, and the socket is
  normally owned by `root:root` or `root:docker` on the host, so the
  container also needs group access to it. Grant that with `--group-add`,
  passing the socket's actual group ID from the host:
  ```bash
  docker run -d \
    --name vela \
    --group-add $(stat -c '%g' /var/run/docker.sock) \
    -v /var/run/docker.sock:/var/run/docker.sock \
    ... \
    vela:latest
  ```
  Without this, the container starts normally and everything else works —
  Vela just logs "cannot connect to Docker daemon" and disables
  Docker-backed features, per the non-fatal design above.

Each service using `restart.mode = "docker"` is restarted using only its
own `[services.restart.docker]` config — one service's container failing
to restart can never affect another service's restart attempt.

## API reference

All endpoints except `/health` require `Authorization: Bearer <api_key>` header.
The key is set in `config.toml` under `global.api_key`.

| Method | Path | Auth | Description |
|---|---|---|---|
| GET | `/health` | None | Liveness probe — for Docker/K8s health checks |
| GET | `/api/v1/status` | Required | Aggregated status + counts for all services |
| GET | `/api/v1/services` | Required | List of all service summaries |
| GET | `/api/v1/services/:id` | Required | Detailed state for one service |
| GET | `/api/v1/services/:id/checks` | Required | Recent health check records |
| GET | `/api/v1/services/:id/alerts` | Required | Recent alert records |
| GET | `/api/v1/services/:id/restarts` | Required | Recent restart attempt records |

### Response format

All responses use a standard envelope:
```json
{
  "ok": true,
  "version": "1",
  "data": { ... }
}
```

Error responses:
```json
{
  "ok": false,
  "version": "1",
  "error": "Human-readable message"
}
```

### Example requests

```bash
# Liveness check (no auth)
curl http://localhost:7700/health

# Full status
curl -H "Authorization: Bearer your-api-key" \
  http://localhost:7700/api/v1/status

# Single service detail
curl -H "Authorization: Bearer your-api-key" \
  http://localhost:7700/api/v1/services/auth-service

# Recent health checks
curl -H "Authorization: Bearer your-api-key" \
  http://localhost:7700/api/v1/services/auth-service/checks
```

## Operating a running instance

Vela ships two ways to look at a running instance without scripting `curl`
yourself: a CLI (`vela-ctl`) and an embedded web dashboard. Both are
read-only clients of the REST API above — neither one carries any state
of its own.

### CLI (`vela-ctl`)

Built alongside the main binary (`cargo build --release` produces both
`target/release/vela` and `target/release/vela-ctl`).

```bash
# Point it at your instance — via flags or environment variables
export VELA_URL="http://localhost:7700"        # default: http://127.0.0.1:7700
export VELA_API_KEY="your-api-key"

# Aggregated status of every service (also the default for `services` with no ID)
vela-ctl status

# Detail for one service
vela-ctl services auth-service

# Recent history for one service — checks | alerts | restarts
vela-ctl services auth-service checks
vela-ctl services auth-service alerts
vela-ctl services auth-service restarts

# --url and --key flags override the environment variables
vela-ctl --url http://prod-host:7700 --key "prod-key" status
```

Output is colorized and column-aligned in a terminal (`●` healthy,
`◐` degraded, `✗` failed, `○` unknown); a missing `VELA_API_KEY`/`--key`
fails fast with a message showing how to set it.

### Web dashboard

Every Vela instance serves a single-page dashboard at the API root —
unauthenticated to load (`GET /` on `api_port`), but every data request
it makes is a normal authenticated call to the `/api/v1/*` endpoints
above. On first visit it prompts once for your API key and keeps it only
in the browser's `sessionStorage` (cleared when the tab closes) — Vela
itself never stores it.

```
http://localhost:7700/
```

It shows the same aggregated status, per-service detail, and recent
checks/alerts/restarts history as `vela-ctl` and the API, in a browser.

## Running tests

```bash
# Full test suite — unit tests across all modules
cargo test

# Watch mode during development
cargo watch -x test
```

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before making any changes.
The rules there are non-negotiable and exist to keep the codebase
coherent as multiple contributors (and AI agents) work on it.

## Build status

| Feature | Status |
|---|---|
| Phase 1 — Config engine + models | ✅ Complete |
| Phase 2 — Health engine | ✅ Complete |
| Phase 3 — Recovery engine | ✅ Complete |
| Phase 4 — Alert engine | ✅ Complete |
| Phase 5 — Proxy engine | ✅ Complete |
| Phase 6 — API engine | ✅ Complete |
| Phase 7 — CLI + Dashboard | ✅ Complete |
| Phase 8 — Docker + Multi-upstream | ✅ Complete |
| CI Pipeline | ✅ Complete |
| CD Pipeline | ✅ Complete |
| Terraform (GCP) | 🔄 Planned |
| Ansible (server config) | 🔄 Planned |

v0.1.0 — all seven engines, the CLI, and the dashboard implemented and wired
together, with CI/CD automated end to end. See [ROADMAP.md](ROADMAP.md) for
what's planned beyond v0.1.0.

## License

MIT
