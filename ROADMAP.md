# Vela — Future Implementation Roadmap
# Document version: 1.0.0
# Applies to: Vela versions beyond v0.1.0
#
# This document captures planned features that are intentionally out of
# scope for Vela v0.1.0. They are not missing — they are deferred because
# v0.1.0 is designed to solve one problem perfectly before expanding.
#
# Reading this document: each version section describes what changes,
# why it matters, and the engineering decisions that must be made before
# building it. This is a living document — update it as decisions are made.

---

## What v0.1.0 deliberately is

Vela v0.1.0 is a single-server tool. It installs on the same machine
as the services it manages. It monitors those services, restarts them
when they fail, routes traffic across healthy instances, and alerts
the operator. It does all of this without SSH, without cloud accounts,
and without any network credentials — because everything it manages
is local to the machine it runs on.

This constraint is not a limitation. It is a design choice that makes
v0.1.0 simple to install, simple to understand, and simple to trust.
A tool that does one thing correctly is more valuable than a tool that
does many things unreliably.

Everything in this document builds on top of that foundation.

---

## v0.1.1 — Self-hosting and High Availability

### The problem this version solves

Vela v0.1.0 has one operational blind spot: it cannot monitor itself.
If the machine running Vela goes down, there is nobody left to send
the alert. This version solves that with a simple, elegant pattern.

### Feature 1 — systemd integration (vela install command)

Currently, keeping Vela running after a crash or system reboot requires
the operator to manually write a systemd service file. This is well-
documented but still a manual step.

v0.1.1 adds: `vela install`

This command generates and installs a systemd service file for Vela
automatically, then enables and starts it. One command. Vela is now
a proper system service that survives crashes and reboots forever.

What the generated systemd unit must include:
- Restart=always with RestartSec=5
- StartLimitBurst=3 over StartLimitIntervalSec=60 (stops infinite crash loops)
- After=network.target and After=docker.service when Docker mode is configured
- Logging to journald (RUST_LOG=info by default)
- Running as a dedicated non-root vela user with only the permissions it needs

`vela uninstall` removes the service cleanly.

### Feature 2 — Vela watching Vela (the HA pattern)

The recommended high availability architecture for Vela v0.1.1 is:

```
Primary Vela (Server A) — monitors your actual services
Watchdog Vela (Server B) — monitors the Primary Vela health endpoint
```

Watchdog Vela configuration on Server B:

```toml
[[services]]
id = "primary-vela"
name = "Primary Vela Instance"
host = "SERVER_A_IP"
port = 7700
check_interval_secs = 30
failure_threshold = 2
max_restarts = 0  # cannot restart a remote process in v0.1.x

[services.health_check]
kind = "http"
http_path = "/health"
timeout_ms = 5000

[services.restart]
mode = "none"  # monitor only — alerting handles the response

[[services.alerts]]
kind = "webhook"
endpoint = "https://your-pagerduty-or-slack-webhook.com"
enabled = true
```

When Primary Vela goes down, Watchdog Vela fires an alert within 60
seconds (2 failed checks × 30 second interval). The operator receives
the alert and can respond. This requires no new code — it works with
Vela v0.1.0 exactly as built. It just needs to be documented clearly
as the recommended production deployment pattern.

### Engineering decisions to make before building v0.1.1
- systemd service file template location (embedded in binary vs external)
- Permissions model for vela install (requires sudo — how to handle cleanly)
- Whether vela install also configures the watchdog pattern automatically

---

## v1.0.0 — Multi-Server with Ansible and Global Management

### The problem this version solves

A company with 50 servers needs to install and configure Vela on each
one. Doing this manually takes days. v1.0.0 makes it a single command
using Ansible, then provides a Global Vela instance that aggregates
status from all server-level Vela instances into one unified view.

### Feature 1 — Ansible role for Vela

An official Ansible role published to Ansible Galaxy as `vela.vela`.

What the role does in order:
1. Downloads the correct Vela binary for the target architecture
   (x86_64-linux, arm64-linux) from GitHub releases
2. Creates a dedicated vela system user with minimum permissions
3. Installs the binary to /usr/local/bin/vela and /usr/local/bin/vela-ctl
4. Creates /etc/vela/ directory with correct ownership
5. Writes config.toml from an Ansible template (operator provides variables)
6. Runs vela install to create and enable the systemd service
7. Adds vela user to the docker group if Docker mode is configured
8. Verifies the service started correctly and the health endpoint responds

Example Ansible playbook for deploying Vela to 100 servers:

```yaml
- hosts: all_servers
  roles:
    - role: vela.vela
      vars:
        vela_api_key: "{{ vault_vela_api_key }}"
        vela_services:
          - id: my-app
            name: My Application
            restart_mode: docker
            docker_container: my-app-container
            health_check_kind: http
            health_check_path: /health
            proxy_listen_port: 8080
```

This deploys a correctly configured Vela instance to every server in
the inventory simultaneously. What took days of manual work takes minutes.

### Feature 2 — Global Vela (federated monitoring)

Global Vela is a special Vela instance that does not monitor services
directly. Instead it monitors the Vela instances running on each server
by polling their API endpoints.

Architecture:

```
Global Vela (management server)
  ├── monitors Vela on Server 1 via API (SSH tunnel or VPN)
  ├── monitors Vela on Server 2 via API
  ├── monitors Vela on Server 3 via API
  └── ... up to N servers

Each server's Vela instance monitors its own local services independently.
```

Global Vela provides:
- A single unified dashboard showing all servers and all services
- Cross-server alert aggregation (one Slack message covers all servers)
- Global status API: GET /api/v1/federation/status

Global Vela does NOT restart services on remote servers in this version.
It monitors and alerts. Remote restart comes in v1.1.0.

### Feature 3 — SSH private key management for remote operations

Global Vela needs to authenticate to each server's Vela API. Rather
than using SSH directly for command execution (which is complex and
dangerous if done wrong), v1.0.0 uses a simpler and safer pattern:

Each server's Vela API is reachable from Global Vela via:
- A private network / VPN (recommended — no public exposure)
- An SSH tunnel maintained by Global Vela (fallback for servers without VPN)

For the SSH tunnel approach:
- Global Vela holds one SSH private key per server (or a shared key)
- Keys are stored encrypted on disk using the same key derivation as
  the API key (never stored in plaintext)
- Global Vela opens an SSH tunnel to forward the remote API port locally
- All API calls go through the tunnel — no direct SSH command execution

This is deliberately safer than SSH command execution:
- The SSH key only needs tunnel permission, not full shell access
- No arbitrary commands are run over SSH — only port forwarding
- The blast radius of a compromised key is limited to API access

### Feature 4 — Ansible provisions SSH keys automatically

The Ansible role for v1.0.0 handles SSH key distribution:

1. Operator generates a key pair once: `vela-global keygen`
2. Ansible role deploys the public key to each server's authorized_keys
   with a command restriction: `command="false"` (tunnel only, no shell)
3. Global Vela holds the private key for tunnel establishment
4. Each server's Vela API is now reachable from Global Vela via tunnel

This means a company with 100 servers runs one Ansible playbook and
Global Vela can reach all of them. No manual SSH key management.

### Engineering decisions to make before building v1.0.0
- VPN-first vs SSH-tunnel-first for API access
- Whether Global Vela is a separate binary (vela-global) or a mode
  flag on the existing binary (vela --mode=global)
- Key storage encryption approach (system keychain vs encrypted file)
- Whether the Ansible role is in the main repo or a separate repo
- Federation API design — how Global Vela aggregates N server statuses
- Rate limiting on the federation polling (N servers × polling interval)

---

## v1.1.0 — Remote Restart and Full Fleet Control

### The problem this version solves

v1.0.0 Global Vela can see that a service on Server 47 is down but
cannot do anything about it. v1.1.0 adds the ability to trigger restarts
on remote servers from Global Vela, completing the full management loop.

### Feature 1 — Remote restart via Global Vela

Global Vela adds a new API endpoint:
POST /api/v1/federation/servers/:server_id/services/:service_id/restart

This endpoint:
1. Authenticates the caller with Global Vela's API key
2. Forwards a restart command to the target server's Vela API via tunnel
3. The target server's Vela executes the restart locally (already implemented)
4. Returns the restart outcome to the caller

No new SSH command execution. The restart runs locally on the target
server by that server's own Vela instance. Global Vela only forwards
the instruction through the already-established tunnel. This keeps the
security model clean — Global Vela never runs arbitrary commands on
remote servers.

### Feature 2 — Fleet dashboard

The web dashboard gets a new Fleet view showing all servers on one screen:

```
Fleet Overview — 47 servers, 312 services

SERVER          HEALTHY  DEGRADED  FAILED  STATUS
server-01       8        0         0       ● All healthy
server-02       7        1         0       ◐ Degraded
server-47       6        0         2       ✗ Action needed
...
```

Clicking a server opens that server's individual dashboard (proxied
through Global Vela). The operator never needs to know individual
server IP addresses or API keys — Global Vela handles all of that.

### Feature 3 — vela-ctl fleet commands

```bash
# Status across all servers
vela-ctl fleet status

# Restart a specific service on a specific server
vela-ctl fleet restart server-47 payment-service

# Restart a service across all servers simultaneously
vela-ctl fleet restart --all payment-service
```

### Engineering decisions to make before building v1.1.0
- Authorization model for fleet-wide restarts (who can restart what)
- Rate limiting on fleet restart (prevent accidental fleet-wide restart)
- Audit log for all remote restart commands (who triggered what and when)
- Whether vela-ctl fleet commands need a separate config file or extend
  the existing VELA_URL / VELA_API_KEY pattern

---

## v2.0.0 — Kubernetes Native Mode

### The problem this version solves

Teams running Kubernetes do not use config.toml files. Their services
are Kubernetes Deployments, Services, and Pods. v2.0.0 makes Vela
Kubernetes-native — it reads service definitions directly from the
Kubernetes API instead of a config file.

### Feature 1 — Kubernetes service discovery

Instead of config.toml, Vela reads from the Kubernetes API:
- Watches all Deployments and Services in configured namespaces
- Automatically creates a Vela service entry for each Kubernetes Service
- Reads health check configuration from Kubernetes liveness probe annotations
- No manual configuration needed when a new service is deployed

### Feature 2 — Kubernetes-native restart

When a service fails, Vela triggers a Kubernetes rollout restart:
`kubectl rollout restart deployment/payment-service`

This is the correct way to restart a Kubernetes service — it replaces
pods gracefully following the Deployment's rolling update strategy.

### Feature 3 — Kubernetes Gateway API integration

The proxy engine integrates with the Kubernetes Gateway API standard,
allowing Vela to participate in the cluster's ingress infrastructure
rather than operating as a separate proxy.

### Engineering decisions to make before building v2.0.0
- In-cluster vs out-of-cluster Kubernetes client (different auth models)
- Whether v2.0.0 replaces config.toml or adds Kubernetes as an additional
  source alongside it
- Namespace scoping — monitoring all namespaces vs configured ones only
- How to handle Kubernetes services with no health endpoint configured

---

## Guiding principles for all future versions

These principles must be maintained across every version:

**Single binary.** Adding a feature must never require the operator to
install additional processes, agents, or dependencies. Everything Vela
does must be doable from the compiled binary.

**Backward compatibility.** A config.toml that works in v0.1.0 must
work in v2.0.0 without modification. Users must never be forced to
update their config to upgrade Vela.

**Fail safe.** If a new feature fails to initialize (no Kubernetes
access, no SSH key, no federation endpoint reachable), Vela must
continue operating with the features that do work. A failed optional
feature never takes down the core monitoring functionality.

**Zero cloud dependency.** Vela must always be fully functional with
no internet connection, no cloud account, and no third-party service.
Cloud integrations are additive, never required.

**Honest scope.** Each version must solve one problem well before
expanding scope. A tool that does eight things poorly serves nobody.
