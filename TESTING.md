# Testing Vela

Vela version: v0.1.0
Last updated: 2026-08-28

This document covers how to test Vela before merging any change. Tests
are split into two categories: **automated** (`cargo test` — fast, no
external dependencies) and **functional** (live system tests against
real services, real Docker containers, and the real API/CLI/dashboard).
Both categories must pass before a PR is merged. Functional tests
require the environment and setup described below — read that first.

## Table of contents

1. [Environment Requirements](#1-environment-requirements)
2. [Pre-Test Setup](#2-pre-test-setup--do-this-before-running-any-tests)
3. [Automated Tests](#3-automated-tests-cargo-test)
4. [Functional Tests](#4-functional-tests--live-system-verification)
5. [Post-Test Cleanup](#5-post-test-cleanup)
6. [Complete Pass Criteria](#6-definition-of-a-complete-pass)
7. [Troubleshooting](#7-troubleshooting)

---

## 1. Environment Requirements

**Operating system:** Ubuntu 22.04+ or any modern Linux distribution.
Functional tests rely on Linux process signals and the Docker Unix
socket. `cargo test` runs fine on Windows and macOS, but the functional
rounds in Section 4 assume Linux — run those there.

**Required tools and minimum versions:**

| Tool | Minimum | Verify with |
|---|---|---|
| Rust | 1.85+ | `rustc --version` |
| Cargo | 1.85+ | `cargo --version` |
| Docker | 24.0+ | `docker --version` |
| Git | 2.34+ | `git --version` |
| curl | any recent | `curl --version` |
| python3 | any recent (JSON pretty-printing) | `python3 --version` |

Rust 1.85+ specifically: a transitive dependency requires edition-2024
support. `cargo build` fails on older toolchains with an edition2024
error — see [Troubleshooting](#7-troubleshooting) if you hit this.

**Docker socket access:** the user running tests must be able to reach
`/var/run/docker.sock`.

```bash
# Verify — must return a (possibly empty) container table, no permission error
docker ps

# If you get "permission denied":
sudo usermod -aG docker $USER
newgrp docker

# Quick fix for a throwaway dev VM only (not for shared/production machines):
sudo chmod 666 /var/run/docker.sock
```

**Network requirements:** the functional tests include one check against
a real external HTTP service (Round 3's `gcp-backend`, used to prove
Vela can monitor a remote service it doesn't control). That round needs
an internet connection. Every other round runs entirely on localhost.

---

## 2. Pre-Test Setup — Do This Before Running Any Tests

### Step 1 — Clone and build

```bash
git clone https://github.com/34Herveonly/Vela_Project.git
cd Vela_Project
cargo build --release
```

Expected: both binaries appear in `target/release/vela` and
`target/release/vela-ctl`. The first build takes 5-15 minutes
(downloading and compiling dependencies). Subsequent builds take
under 30 seconds thanks to incremental compilation.

### Step 2 — Verify both binaries exist

```bash
ls -lh target/release/vela target/release/vela-ctl
```

Expected: two executable files — `vela` roughly 5-6MB, `vela-ctl`
roughly 3-4MB.

### Step 3 — Create the test config file

Create `config.test.toml` in the project root with the content below.
**This file is gitignored and must be created manually on each machine**
— it contains a throwaway API key and a real external IP, neither of
which belongs in version control.

It defines exactly four services, each exercising a different part of
Vela: a real external monitor-only service, a deliberately-dead service
for failure/recovery testing, a single Docker container under Vela's
control, and a two-upstream Docker pair for proxy failover testing.

```toml
[global]
api_port = 7700
api_key = "vela-dev-key-2026"
log_dir = "/var/log/vela-test"

# Service 1 — a real external service, monitor-only (no restart possible)
[[services]]
id = "gcp-backend"
name = "GWPS Backend"
host = "34.35.143.220"
port = 8080
check_interval_secs = 10
failure_threshold = 3
max_restarts = 3

[services.health_check]
kind = "http"
http_path = "/health"
timeout_ms = 5000

[services.restart]
mode = "none"

[[services.alerts]]
kind = "log"
enabled = true

# Service 2 — nothing listens on this port; fails immediately, drives
# the health/recovery/max_restarts test rounds
[[services]]
id = "fake-service"
name = "Fake Dead Service"
host = "127.0.0.1"
port = 19999
check_interval_secs = 5
failure_threshold = 2
max_restarts = 2

[services.health_check]
kind = "tcp"
timeout_ms = 2000

[services.restart]
mode = "manual"
command = "echo 'Recovery engine fired for fake-service'"

[[services.alerts]]
kind = "log"
enabled = true

# Service 3 — a real Docker container, single upstream, Docker-mode restart
[[services]]
id = "nginx-docker"
name = "Nginx Docker Test"
check_interval_secs = 5
failure_threshold = 2
max_restarts = 3

[services.health_check]
kind = "tcp"
timeout_ms = 2000

[services.restart]
mode = "docker"

[services.restart.docker]
container = "vela-nginx-test"

[services.proxy]
listen_port = 18081

[[services.upstreams]]
host = "127.0.0.1"
port = 18080
docker_container = "vela-nginx-test"

[[services.alerts]]
kind = "log"
enabled = true

# Service 4 — two upstreams (one shared with Service 3's container, one
# its own), for multi-upstream proxy failover testing
[[services]]
id = "nginx-replica"
name = "Nginx Replica"
check_interval_secs = 5
failure_threshold = 2
max_restarts = 3

[services.health_check]
kind = "tcp"
timeout_ms = 2000

[services.restart]
mode = "docker"

[services.restart.docker]
container = "vela-nginx-replica"

[services.proxy]
listen_port = 18083

[[services.upstreams]]
host = "127.0.0.1"
port = 18080
docker_container = "vela-nginx-test"

[[services.upstreams]]
host = "127.0.0.1"
port = 18082
docker_container = "vela-nginx-replica"

[[services.alerts]]
kind = "log"
enabled = true
```

This exact config has been verified end-to-end against a live Docker
daemon — every log line and API response shown in Section 4 was
captured from a real run of it, not written from assumption.

### Step 4 — Start the Docker test containers

```bash
docker run -d --name vela-nginx-test -p 18080:80 nginx:latest
docker run -d --name vela-nginx-replica -p 18082:80 nginx:latest

# Verify both are running
docker ps

# Verify each responds
curl http://localhost:18080
curl http://localhost:18082
```

### Step 5 — Set the test API key environment variable

```bash
export VELA_API_KEY="vela-dev-key-2026"
```

This must be set in every terminal session used for testing (`vela-ctl`
reads it, and it's used throughout Section 4). Add it to `~/.bashrc` or
`~/.zshrc` to make it permanent across sessions.

### Step 6 — Pre-test checklist

- [ ] `cargo build --release` succeeded with zero errors
- [ ] Both binaries exist in `target/release/`
- [ ] `config.test.toml` created with all four services
- [ ] `docker ps` shows both nginx containers running
- [ ] `curl http://localhost:18080` returns the nginx welcome page
- [ ] `curl http://localhost:18082` returns the nginx welcome page
- [ ] `VELA_API_KEY` environment variable is set

---

## 3. Automated Tests (`cargo test`)

The automated suite covers **92 tests** across all engines. These run
without any external services or Docker — they use mock TCP listeners,
in-memory state, and controlled timing to verify engine behavior in
isolation. Run these first, before any functional testing.

```bash
# Run all tests
cargo test

# Run tests for a specific module
cargo test health
cargo test recovery
cargo test proxy
cargo test alert
cargo test api
cargo test docker_engine
cargo test models
cargo test config

# Run with output visible (useful for debugging a failing test)
cargo test -- --nocapture

# Must be clean before any PR
cargo clippy --all-targets
cargo fmt -- --check
```

Expected results, counted directly from `cargo test -- --list`:

| Module | Tests | What it covers |
|---|---|---|
| `alert` | 22 | Suppression logic, delivery, webhook errors, engine lifecycle |
| `api` | 17 | Auth, security headers, all endpoints, engine lifecycle |
| `proxy` | 15 | Port validation, binding, health-aware routing, round-robin, byte forwarding |
| `recovery` | 12 | Backoff math, all three restart modes, per-service isolation, engine lifecycle |
| `health` | 9 | TCP/HTTP/Docker checks, engine lifecycle, state recording |
| `config` | 8 | Backward-compat synthesis, validation, legacy-field handling |
| `models` | 6 | `RestartConfig` validation, `UpstreamState` initialization |
| `docker_engine` | 3 | Restart outcome types, Docker config validation |
| **Total** | **92** | |

Pass criteria: 0 failed, 0 errors, `cargo clippy --all-targets` clean,
`cargo fmt -- --check` clean (no output). Any failure here must be
fixed before moving on to functional testing.

---

## 4. Functional Tests — Live System Verification

These rounds run Vela against the real services from Section 2 and
verify behavior end-to-end. Complete the pre-test setup first. Run the
rounds in order — several build on state left by the previous one.

### Round 1 — Binary verification and startup

**Proves:** both binaries exist and every engine starts correctly.

```bash
ls -lh target/release/vela target/release/vela-ctl
RUST_LOG=info ./target/release/vela config.test.toml
```

Expected startup log, in this order (captured from a real run — Docker
support and 5 upstreams are what this exact config produces):

```
INFO vela: Vela starting up...
INFO vela: Config loaded: 4 service(s) configured
INFO vela::config: Registered service 'gcp-backend' (1 upstream(s): 34.35.143.220:8080)
INFO vela::config: Registered service 'fake-service' (1 upstream(s): 127.0.0.1:19999)
INFO vela::config: Registered service 'nginx-docker' (1 upstream(s): 127.0.0.1:18080)
INFO vela::config: Registered service 'nginx-replica' (2 upstream(s): 127.0.0.1:18080, 127.0.0.1:18082)
INFO vela: State store initialized. Monitoring 4 service(s).
INFO vela: Docker engine: connected to Docker daemon.
INFO vela::health: Health engine: monitoring 5 upstream(s) concurrently
INFO vela: Health engine started successfully.
INFO vela: Recovery engine started successfully.
INFO vela: Alert engine started successfully.
INFO vela::proxy: Proxy engine: 2 listener(s) active
INFO vela: Proxy engine started successfully.
INFO vela::api: API engine: listening on 127.0.0.1:7700 (API v1)
INFO vela: API engine started on port 7700 (API v1).
INFO vela: Vela is running.
INFO vela: Docker support: enabled
INFO vela: Monitoring 4 service(s) across 5 upstream(s)
(alerts fire for each service transitioning Unknown→Healthy or Unknown→Degraded)
```

**PASS:** all engines report "started successfully", no `ERROR` lines
at startup, log shows "Docker support: enabled".
**FAIL:** any engine fails to start, any panic, any port-conflict error.

### Round 2 — API authentication and security

**Proves:** API auth works, security headers are present, wrong/missing
keys are rejected identically (no information leak).

Leave Vela running from Round 1. Open a second terminal.

```bash
curl http://localhost:7700/health
```
→ `{"ok":true,"service":"vela"}`

```bash
curl -s -H "Authorization: Bearer vela-dev-key-2026" \
  http://localhost:7700/api/v1/status | python3 -m json.tool
```
→ Valid JSON with `"ok":true`, `healthy_count` and `total_services` fields present.

```bash
curl -s -H "Authorization: Bearer wrong-key" \
  http://localhost:7700/api/v1/status
```
→ `{"ok":false,"version":"1","error":"Authentication required"}`

```bash
curl -s http://localhost:7700/api/v1/status
```
→ `{"ok":false,"version":"1","error":"Authentication required"}` (identical to the wrong-key case — Vela never reveals *why* auth failed, to prevent enumeration)

```bash
curl -sI -H "Authorization: Bearer vela-dev-key-2026" \
  http://localhost:7700/api/v1/status | grep -iE "x-content|x-frame|cache-control"
```
→ All three present:
```
x-content-type-options: nosniff
x-frame-options: DENY
cache-control: no-store
```

**PASS:** all five commands return exactly the responses shown above.
**FAIL:** any unexpected status code, missing security header, or a
different error message between the wrong-key and no-key cases.

### Round 3 — Health engine failure detection

**Proves:** Vela detects service failure, transitions through
Degraded→Failed at the configured `failure_threshold`, and leaves
unrelated services unaffected.

`fake-service` points at port 19999, which nothing listens on, so it
fails immediately. `failure_threshold = 2`.

Watch the Round 1 terminal. Within ~10 seconds of startup:

```
WARN health: Health check FAILED: service='fake-service' upstream=127.0.0.1:19999 ...
INFO alert: Alert engine: dispatching 1 alert(s) for 'fake-service' (Unknown→Degraded)
WARN health: Health check FAILED: service='fake-service' upstream=127.0.0.1:19999 ...
```

After the 2nd consecutive failure the service's aggregate status moves
to `Failed` (visible via `vela-ctl status` or the API — the recovery
engine picks it up from there, see Round 4).

Confirm the other three services keep checking independently and are
unaffected — `gcp-backend`, `nginx-docker`, and `nginx-replica` should
all show `Healthy` throughout.

**PASS:** Degraded fires after the 1st failure, Failed is reached after
the 2nd (matching `failure_threshold = 2`), other services unaffected.
**FAIL:** wrong failure count before transition, no alert fired, or
other services affected.

### Round 4 — Recovery engine with manual restart

**Proves:** the recovery engine fires the configured restart command,
respects `max_restarts`, stops attempting after the limit, and never
touches any other service (per-service isolation).

`fake-service` has `restart.mode = "manual"`,
`command = "echo 'Recovery engine fired for fake-service'"`,
`max_restarts = 2`. Expected log sequence (captured from a real run —
happens within ~30 seconds of startup):

```
INFO recovery: Recovery engine: attempting manual restart of 'fake-service' (attempt 1/2)
Recovery engine fired for fake-service
INFO recovery: Recovery engine: manual restart of 'fake-service' completed successfully (attempt 1)
(health checks continue failing — the echo command never opened port 19999)
INFO recovery: Recovery engine: attempting manual restart of 'fake-service' (attempt 2/2)
Recovery engine fired for fake-service
INFO recovery: Recovery engine: manual restart of 'fake-service' completed successfully (attempt 2)
ERROR recovery: Recovery engine: 'fake-service' has exceeded max_restarts (2) — giving up. Manual intervention required.
(no further restart attempts for fake-service)
```

`gcp-backend`, `nginx-docker`, and `nginx-replica` must show no restart
activity at all throughout this round — confirm via
`vela-ctl services <id> restarts` on each.

**PASS:** exactly 2 restart attempts, "giving up" logged once, no
further attempts afterward, no other service was restarted.
**FAIL:** wrong attempt count, "giving up" not logged, or another
service's restart was triggered.

### Round 5 — Docker engine connection and container health detection

**Proves:** Vela connects to the Docker daemon and detects container
status via the Docker API — not just TCP — reporting `Healthy` for a
running container.

`nginx-docker`'s upstream declares `docker_container = "vela-nginx-test"`.
Expected in the startup logs (already visible from Round 1):

```
INFO vela: Docker engine: connected to Docker daemon.
INFO vela: Docker support: enabled
INFO vela::health: Health engine: starting check loop for 'nginx-docker' upstream 127.0.0.1:18080 every 5s (docker container 'vela-nginx-test')
```

Verify from a second terminal:

```bash
curl -s -H "Authorization: Bearer vela-dev-key-2026" \
  http://localhost:7700/api/v1/services/nginx-docker | python3 -m json.tool
```
→ `"status"` field shows `"Healthy"`.

**PASS:** Docker connected at startup, `nginx-docker` shows `Healthy`
in the API response.
**FAIL:** Docker connection warning at startup, or `nginx-docker` stays
`Unknown` or shows `Failed` while the container is actually running.

### Round 6 — Docker auto-restart

**Proves:** when a Docker container stops, Vela detects it via the
Docker API, restarts it automatically, confirms recovery, and resets
its recovery state.

With Vela running, in a second terminal:

```bash
docker stop vela-nginx-test
```

Watch the first terminal for this sequence (captured from a real run):

```
WARN health: Health check FAILED: service='nginx-docker' upstream=127.0.0.1:18080 ... error=Docker container 'vela-nginx-test' is not running
INFO alert: Alert engine: dispatching 1 alert(s) for 'nginx-docker' (Healthy→Degraded)
(one check_interval later, 2nd consecutive failure — aggregate status reaches Failed)
INFO recovery: Recovery engine: attempting Docker restart of 'nginx-docker' (attempt 1/3)
INFO recovery: Recovery engine: Docker restart of 'nginx-docker' completed successfully (attempt 1)
INFO alert: Alert engine: dispatching 1 alert(s) for 'nginx-docker' (Failed→Healthy)
INFO recovery: Recovery engine: 'nginx-docker' returned to Healthy after 1 attempt(s) — resetting recovery state
```

Verify the container is actually running again — not just that Vela
*thinks* it is:

```bash
docker ps | grep vela-nginx-test
```
→ shows `Up <a few seconds>` — a fresh uptime, confirming a real restart
happened (not a stale `Up` from before the stop).

Full cycle time from `docker stop` to confirmed `Healthy`: roughly
10-15 seconds in practice (driven by `check_interval_secs = 5` ×
`failure_threshold = 2`, plus the restart itself).

**PASS:** container detected as stopped, restarted automatically,
confirmed `Healthy`, recovery state reset.
**FAIL:** container not detected, restart not attempted, or container
stays stopped.

### Round 7 — Proxy engine and multi-upstream failover

**Proves:** the proxy routes traffic to healthy upstreams, automatically
removes a failed upstream from rotation, and resumes normal routing on
recovery — with no dropped requests once the health engine has caught up.

`nginx-replica` has two upstreams (127.0.0.1:18080 and 127.0.0.1:18082)
behind a single proxy on `listen_port = 18083`.

First verify the proxy is routing:

```bash
curl http://localhost:18083
```
→ nginx welcome page (forwarded to whichever upstream is healthy).

Then take one upstream down:

```bash
docker stop vela-nginx-test
```

Watch Vela's logs for the upstream being marked unhealthy — because
`nginx-replica` has a second, still-healthy upstream, its **aggregate**
status only reaches `Degraded`, not `Failed` (Vela's recovery engine
only acts on `Failed` — this is why `nginx-replica`'s own Docker
container is never touched by this round; only `nginx-docker`'s restart
from Round 6 fires):

```
WARN health: Health check FAILED: service='nginx-replica' upstream=127.0.0.1:18080 ... error=Docker container 'vela-nginx-test' is not running
INFO alert: Alert engine: dispatching 1 alert(s) for 'nginx-replica' (Healthy→Degraded)
```

Within a few seconds, verify traffic still works:

```bash
curl http://localhost:18083
```
→ nginx welcome page (now routed to 18082 only) — no 502 errors.

Start the container again:

```bash
docker start vela-nginx-test
```

Within one `check_interval_secs` (5s) plus the next health check, the
upstream is marked `Healthy` again and the proxy adds it back to
rotation automatically — no restart, no manual step.

**PASS:** traffic works with both upstreams, works with one upstream
down, the failed upstream rejoins rotation on its own once healthy
again, and no 502s occur during the transition.
**FAIL:** a 502 during failover, traffic stops entirely when one
upstream fails, or the recovered upstream never rejoins rotation.

### Round 8 — CLI tool (`vela-ctl`) verification

**Proves:** the CLI connects to the API, displays correct output for
every command, and handles error cases cleanly with exit code 1.

With Vela running and `VELA_API_KEY` exported:

```bash
./target/release/vela-ctl status
```
→ Header line: `3 healthy  0 degraded  1 failed  0 unknown  (4 total)`
(exact counts depend on which round you're on)
→ Table with columns `SERVICE | STATUS | FAILURES | RESTARTS | LAST CHECK`
→ Status column shows colored indicators: `●` Healthy (green), `◐`
Degraded (yellow), `✗` Failed (red), `○` Unknown (grey)

```bash
./target/release/vela-ctl services gcp-backend
```
→ Service name, colored status, consecutive failures, restart count,
last-checked timestamp, last-success timestamp.

```bash
./target/release/vela-ctl services gcp-backend checks
```
→ Table: `TIMESTAMP | RESULT | LATENCY | ERROR` (✓ Success in green /
✗ Failed in red) with recent check rows.

```bash
./target/release/vela-ctl services gcp-backend alerts
```
→ Table: `TIMESTAMP | TRANSITION | KIND | STATUS` — at least one row
showing `Unknown→Healthy`.

```bash
./target/release/vela-ctl services gcp-backend restarts
```
→ `No restart records for 'gcp-backend'.` (`gcp-backend` has
`mode = "none"`, so it's never eligible for a restart).

```bash
./target/release/vela-ctl --key wrong-key status
echo $?
```
→ `Error: Authentication failed. Check your API key.`, exit code `1`.

```bash
./target/release/vela-ctl services does-not-exist
echo $?
```
→ `Error: Service 'does-not-exist' not found.`, exit code `1`.

**PASS:** all seven commands produce the output shown above, both
error cases exit with code 1.
**FAIL:** any command crashes, shows wrong data, or an error case
exits with code 0.

### Round 9 — Web dashboard

**Proves:** the dashboard loads in a browser, all four pages render,
all service states display correctly, auto-refresh works, and the API
key flow behaves correctly.

Open a browser and navigate to:

```
http://localhost:7700
```

**Login:**
- On first load (no key in `sessionStorage` yet), the auth overlay
  appears: "Enter your Vela API key to access the dashboard."
- An incorrect key shows the error: "Invalid API key. Please try again."
- The correct key (`vela-dev-key-2026`) loads the dashboard.

**Dashboard page ("Service status"):**
- A card for each of the 4 configured services.
- `gcp-backend`, `nginx-docker`, `nginx-replica` show a green Healthy badge.
- `fake-service` shows a red Failed badge (once Round 3/4 have run).
- "Last refresh: HH:MM:SS" visible in the header, updating automatically.
- A "Live" indicator with a pulsing dot, and an "Auto-refreshing" badge.

**Service detail page:**
- Click any service card.
- Detail loads with the service name and current status badge.
- "Recent Alerts" table shows the `Unknown→Healthy` (or later) transitions.
- "Recent Restarts" table — empty for `gcp-backend`, populated for
  `fake-service` and `nginx-docker` once their rounds have run.
- A way back to the Dashboard page.

**Alerts page:**
- Stats row: total alerts, delivered count, failed count.
- Alerts table with rows across all services.
- Service filter dropdown populated with all 4 service names.

**Settings page:**
- General section: config file path, refresh interval (`10s`), Vela version (`v0.1.0`).
- "Monitored Services" table lists all 4 services with `SERVICE`,
  `HOST:PORT`, `CHECK INTERVAL`, `FAILURE THRESHOLD`, `STATE` columns.
- "🔒 Read-only" badge visible — the dashboard never writes anything back.

**Auto-refresh:**
- Watch "Last refresh" for ~15 seconds — it updates on its own (the
  dashboard refreshes every 10 seconds) with no user action.

**Session behavior:**
- Close and reopen the tab (or open a fresh private window) — the
  login overlay appears again, since the key lived only in
  `sessionStorage` and is gone with the old session.

**PASS:** all items above verified visually.
**FAIL:** any page fails to load, data is missing, auto-refresh
doesn't happen, or the login overlay doesn't reappear in a fresh session.

### Round 10 — Graceful shutdown

**Proves:** Ctrl+C triggers a clean shutdown in the correct engine
order, and every engine stops cleanly with no hanging tasks.

With Vela running (from any previous round), press **Ctrl+C** in the
terminal where it's running. Expected shutdown log, in this exact
order (captured from a real run — text is verbatim from the source):

```
INFO vela: Shutdown signal received. Stopping engines gracefully...
INFO vela::proxy: Proxy engine: initiating graceful drain — no new connections accepted
INFO vela::proxy: Proxy engine: all listeners stopped and connections drained
INFO vela::alert: Alert engine: initiating graceful shutdown
INFO vela::alert: Alert engine: stopped cleanly
INFO vela::recovery: Recovery engine: initiating graceful shutdown
INFO vela::recovery: Recovery engine: stopped cleanly
INFO vela::health: Health engine: initiating graceful shutdown of N tasks
INFO vela::health: Health engine: all tasks stopped cleanly
INFO vela::api: API engine: initiating graceful shutdown
INFO vela::api: API engine: stopped cleanly
INFO vela: Vela shut down cleanly. Goodbye.
```

(`N` in the health engine line is the number of upstreams being
monitored — 5 for this config.)

**PASS:** all five engines appear in this order, "Goodbye" is logged,
the terminal prompt returns within a few seconds — no hanging process.
**FAIL:** any engine missing from the shutdown log, wrong order, or the
process hangs after Ctrl+C.

---

## 5. Post-Test Cleanup

```bash
# Stop and remove test Docker containers
docker stop vela-nginx-test vela-nginx-replica 2>/dev/null
docker rm vela-nginx-test vela-nginx-replica 2>/dev/null

# Verify they're gone
docker ps

# Remove the test config — never commit this file, it contains a real IP
rm config.test.toml

# Kill any background Vela process still running
pkill -f "vela config" 2>/dev/null
```

---

## 6. Definition of a Complete Pass

All items below must be checked before a contributor can claim the
test suite passes. Include the completed checklist in the PR description.

**Automated tests:**
- [ ] `cargo test`: 92 tests, 0 failed, 0 errors
- [ ] `cargo clippy --all-targets`: 0 warnings
- [ ] `cargo fmt -- --check`: clean (no output)

**Functional tests:**
- [ ] Round 1: all engines start, no `ERROR` lines
- [ ] Round 2: API auth, security headers, identical error for bad/missing keys
- [ ] Round 3: failure detection, correct threshold, alert timing
- [ ] Round 4: recovery fires, `max_restarts` respected, isolation confirmed
- [ ] Round 5: Docker connected, container health via Docker API
- [ ] Round 6: Docker auto-restart within ~15 seconds, confirmed via fresh uptime
- [ ] Round 7: proxy failover, no 502s, failed upstream rejoins automatically
- [ ] Round 8: all `vela-ctl` commands correct, error cases exit 1
- [ ] Round 9: dashboard loads, all pages, all states, auto-refresh works
- [ ] Round 10: graceful shutdown in correct order, no hang

A pull request must not be merged unless every item above is checked.

---

## 7. Troubleshooting

| Issue | Cause | Fix |
|---|---|---|
| Docker permission denied when starting Vela | User not in `docker` group | `sudo usermod -aG docker $USER` then `newgrp docker`. Quick fix for dev VMs only: `sudo chmod 666 /var/run/docker.sock` |
| Port already in use when starting Vela | Previous Vela process still running | `pkill -f "vela config"` then retry |
| `cargo build` fails with an edition2024 error | Rust toolchain too old (need 1.85+) | `rustup update stable` |
| Docker containers not found during Round 5/6 | Pre-test setup Step 4 was skipped | Run the `docker run` commands from Section 2 Step 4 |
| `vela-ctl` shows "Cannot reach Vela at ..." | Vela isn't running, or `VELA_API_KEY` isn't exported | Start Vela first, then `export VELA_API_KEY="vela-dev-key-2026"` |
| Dashboard shows the login overlay even after entering the correct key | API not reachable from the browser | Verify Vela is running and reachable at `http://localhost:7700/health` |
| Build takes far longer than expected | Low VM resources, or background processes competing for CPU | Check `top`/`htop` for runaway processes; retry on a quieter machine |
| Round 7 proxy returns a connection error immediately after startup | Health checks haven't completed yet — upstreams still `Unknown` | Wait ~5-10 seconds after startup for the first health check round, then retry |
