# Vela

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

Vela contains six internal engines:

| Engine | Responsibility |
|---|---|
| Config engine | Parses and validates `config.toml` |
| Health engine | Runs TCP/HTTP checks on a per-service interval |
| Recovery engine | Restarts failed services with exponential backoff |
| Alert engine | Fires webhook/log notifications on status changes |
| Proxy engine | Routes incoming traffic to healthy upstreams |
| API engine | Exposes REST endpoints for status and control |

All engines share a single in-memory state store — no database required.

## Quick start (native Rust)

**Prerequisites:** Rust 1.78+ installed via [rustup](https://rustup.rs)

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

# Run with your config file mounted
docker run -d \
  --name vela \
  --network host \
  -v $(pwd)/config.toml:/etc/vela/config.toml:ro \
  -v /var/log/vela:/var/log/vela \
  vela:latest

# Check logs
docker logs -f vela

# Stop
docker stop vela
```

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
host                = "127.0.0.1"
port                = 3001
command             = "systemctl restart auth-service"  # Optional restart command
check_interval_secs = 10
failure_threshold   = 3
max_restarts        = 5

[services.health_check]
kind      = "http"       # "tcp" or "http"
http_path = "/health"    # Required when kind = "http"
timeout_ms = 3000

[services.proxy]
listen_port = 8001       # Vela proxies traffic from this port to host:port

[[services.alerts]]
kind     = "webhook"
endpoint = "https://hooks.example.com/vela"
enabled  = true
```

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

## Running tests

```bash
# Unit tests
cargo test

# Integration tests (requires Docker for mock services)
cargo test --test full_stack

# Watch mode during development
cargo watch -x test
```

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before making any changes.
The rules there are non-negotiable and exist to keep the codebase
coherent as multiple contributors (and AI agents) work on it.

## Build status

| Phase | Status |
|---|---|
| Phase 1 — Config engine + models | Done |
| Phase 2 — Health engine | Done |
| Phase 3 — Recovery engine | Done |
| Phase 4 — Alert engine | Done |
| Phase 5 — Proxy engine | Done |
| Phase 6 — API engine | Done |

v0.1.0 — all six engines implemented and wired together.

## License

MIT
