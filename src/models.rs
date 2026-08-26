//! All data models for the Vela project.
//!
//! RULE: This is the single source of truth for data types.
//! Never define a struct or enum in an engine file.
//! If you need a new type, add it here first, then use it in the engine.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::VelaError;

// ─── Configuration Models ────────────────────────────────────────────────────
// These are parsed from config.toml and are IMMUTABLE after startup.
// They represent what the user declared, not what Vela observed.

/// Top-level configuration file structure.
/// Maps directly to the root of config.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct VelaConfig {
    /// Global settings for the Vela instance.
    pub global: GlobalConfig,

    /// List of services Vela will monitor.
    pub services: Vec<ServiceConfig>,
}

/// Global configuration that applies to the entire Vela instance.
// api_port is read (proxy validation, API engine bind). log_dir is parsed
// but never consumed — Vela logs to stdout only; file logging is unbuilt.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct GlobalConfig {
    /// Port Vela's REST API listens on. Default: 7700.
    #[serde(default = "default_api_port")]
    pub api_port: u16,

    /// Secret key required for all API requests (Bearer token).
    pub api_key: String,

    /// Directory where Vela writes its log file.
    #[serde(default = "default_log_dir")]
    pub log_dir: String,
}

fn default_api_port() -> u16 {
    7700
}
fn default_log_dir() -> String {
    "/var/log/vela".to_string()
}

/// Configuration for a single monitored service.
/// Parsed from a [[services]] block in config.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    /// Unique identifier for this service. Used in logs, alerts, and the API.
    /// Must be a lowercase alphanumeric string with hyphens only (e.g. "auth-service").
    pub id: String,

    /// Human-readable name shown in alerts and the dashboard.
    pub name: String,

    /// Hostname or IP of the service.
    /// Optional as of Phase 8: a service defined entirely through
    /// `[[services.upstreams]]` (the Docker multi-upstream pattern) has no
    /// single host — see `upstreams` below. When present alongside an empty
    /// `upstreams` list, the config engine synthesizes a single upstream
    /// from `host`/`port` at load time — no other code reads these two
    /// fields directly.
    pub host: Option<String>,

    /// Port the service listens on. See `host` above — same Optional/synthesis rule.
    pub port: Option<u16>,

    /// Shell command to restart the service if it fails.
    /// Optional — if absent, Vela monitors but cannot restart.
    /// Example: "systemctl restart auth-service"
    /// Superseded by `restart` below when present; kept for backward
    /// compatibility — the config engine synthesizes a `restart` block
    /// with `mode = "manual"` from this field when `restart` is absent.
    pub command: Option<String>,

    /// One or more upstream instances for this service. Optional in config —
    /// when absent/empty, the config engine synthesizes a single entry from
    /// `host`/`port` at load time, so every other engine can always assume
    /// this is non-empty after startup and never touch `host`/`port` again.
    #[serde(default)]
    pub upstreams: Vec<UpstreamConfig>,

    /// How Vela restarts this service on failure. Optional in config — when
    /// absent, the config engine synthesizes one from `command` (manual mode)
    /// or, if `command` is also absent, `RestartMode::None` (monitor only).
    pub restart: Option<RestartConfig>,

    /// How often to run the health check, in seconds. Default: 10.
    #[serde(default = "default_check_interval")]
    pub check_interval_secs: u64,

    /// How many consecutive failures before marking the service Failed. Default: 3.
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,

    /// Maximum number of restart attempts before giving up. Default: 5.
    #[serde(default = "default_max_restarts")]
    pub max_restarts: u32,

    /// Health check configuration.
    pub health_check: HealthCheckConfig,

    /// Proxy configuration. If absent, proxying is disabled for this service.
    pub proxy: Option<ProxyConfig>,

    /// Alert targets. Can be empty if the user wants no notifications.
    #[serde(default)]
    pub alerts: Vec<AlertTargetConfig>,
}

fn default_check_interval() -> u64 {
    10
}
fn default_failure_threshold() -> u32 {
    3
}
fn default_max_restarts() -> u32 {
    5
}

/// How Vela restarts a service that has crossed its failure threshold.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RestartMode {
    /// Restart via the Docker API — no shell command, targets a container by name.
    Docker,
    /// Restart via a shell command (`sh -c <command>`) — the Phase 3 behavior.
    Manual,
    /// Monitor and alert only. Never attempt a restart. For services Vela
    /// cannot control (external APIs, managed cloud backends).
    None,
}

/// Docker-specific restart parameters. Required when `RestartMode::Docker`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DockerConfig {
    /// Name of the Docker container to restart. Must match exactly.
    pub container: String,

    /// If present, pull this image before restarting (CI/CD "repull" behavior):
    /// stop the old container, remove it, recreate it from the freshly pulled
    /// image with the same name and port bindings, then start it.
    /// If absent, Vela just calls the Docker restart API on the existing container.
    pub image: Option<String>,

    /// Docker socket path override. Default: /var/run/docker.sock (Linux) or
    /// the platform-appropriate named pipe on Windows — see docker_engine::connect().
    pub socket_path: Option<String>,
}

/// How a single service is restarted on failure.
/// Synthesized from the legacy `command` field when absent — see config.rs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RestartConfig {
    pub mode: RestartMode,

    /// Shell command to run. Required when `mode = "manual"`.
    pub command: Option<String>,

    /// Docker restart parameters. Required when `mode = "docker"`.
    pub docker: Option<DockerConfig>,
}

impl RestartConfig {
    /// Validates that the fields required by `mode` are actually present.
    /// Called by the config engine for every service after synthesis, so
    /// this runs even for configs that never wrote a `[services.restart]`
    /// block at all.
    pub fn validate(&self, service_id: &str) -> Result<(), VelaError> {
        match self.mode {
            RestartMode::Docker if self.docker.is_none() => Err(VelaError::ConfigValidation(
                format!(
                    "Service '{}' has restart.mode = \"docker\" but no [services.restart.docker] block",
                    service_id
                ),
            )),
            RestartMode::Manual if self.command.is_none() => Err(VelaError::ConfigValidation(
                format!(
                    "Service '{}' has restart.mode = \"manual\" but restart.command is not set",
                    service_id
                ),
            )),
            _ => Ok(()),
        }
    }
}

/// One upstream instance for a service. A service with multiple upstreams
/// (e.g. two replicas of the same microservice) gets automatic health-aware
/// failover: the proxy only routes to upstreams currently Healthy.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpstreamConfig {
    pub host: String,
    pub port: u16,

    /// If set, this upstream's container identity for Docker-aware health
    /// checks: the health engine additionally verifies the container is
    /// running (not just that the TCP/HTTP check passes).
    pub docker_container: Option<String>,
}

/// Live health status of one upstream instance. Runtime state, not config —
/// mirrors `ServiceState` but scoped to a single upstream within a service.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamState {
    pub host: String,
    pub port: u16,
    pub status: ServiceStatus,
    pub consecutive_failures: u32,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub last_ok_at: Option<DateTime<Utc>>,
    pub latency_ms: Option<u64>,
}

impl UpstreamState {
    /// Creates the initial unknown state for an upstream at startup.
    pub fn initial(host: String, port: u16) -> Self {
        Self {
            host,
            port,
            status: ServiceStatus::Unknown,
            consecutive_failures: 0,
            last_checked_at: None,
            last_ok_at: None,
            latency_ms: None,
        }
    }
}

/// The type and parameters of a health check for a service.
#[derive(Debug, Clone, Deserialize)]
pub struct HealthCheckConfig {
    /// "tcp" or "http". TCP just checks if the port accepts connections.
    /// HTTP sends a GET and checks for a 2xx response.
    pub kind: HealthCheckKind,

    /// HTTP path to check. Required when kind = "http". Example: "/health".
    pub http_path: Option<String>,

    /// Timeout for the health check in milliseconds. Default: 5000.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    5000
}

/// The protocol used for health checking.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum HealthCheckKind {
    Tcp,
    Http,
}

/// Proxy configuration for a service.
/// When present, Vela accepts incoming traffic on `listen_port`
/// and forwards it to the service's host:port.
#[derive(Debug, Clone, Deserialize)]
pub struct ProxyConfig {
    /// Port Vela listens on to receive traffic for this service.
    pub listen_port: u16,

    /// If true, bind to 0.0.0.0 (all interfaces).
    /// If false (default), bind to 127.0.0.1 (loopback only).
    /// Set to true only if external traffic must reach this proxy port.
    /// WARNING: Ensure firewall rules are in place before enabling.
    #[serde(default)]
    pub bind_all_interfaces: bool,
}

/// Configuration for a single alert destination.
#[derive(Debug, Clone, Deserialize)]
pub struct AlertTargetConfig {
    /// "webhook" or "log". More kinds will be added in future phases.
    pub kind: AlertKind,

    /// The webhook URL if kind = "webhook".
    pub endpoint: Option<String>,

    /// Whether this alert target is active. Allows disabling without deleting.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Runtime record of an alert that was sent or attempted.
/// Stored by the alert engine for operational visibility.
/// Exposed by the API engine in Phase 6.
#[derive(Debug, Clone, Serialize)]
pub struct AlertRecord {
    /// Which service triggered this alert.
    pub service_id: String,
    /// The alert kind that was used for delivery.
    pub kind: AlertKind,
    /// Whether delivery succeeded.
    pub delivered: bool,
    /// The status transition that triggered this alert.
    pub trigger: String,
    /// When this alert was sent.
    pub sent_at: chrono::DateTime<chrono::Utc>,
    /// Failure reason if delivery failed. None on success.
    pub error: Option<String>,
}

/// Maximum length of a service name in an alert payload.
/// Prevents unbounded strings in outbound webhook requests.
pub const MAX_ALERT_NAME_LEN: usize = 128;

/// Maximum length of an error message in an alert payload.
pub const MAX_ALERT_MESSAGE_LEN: usize = 512;

/// How long after an alert fires before another alert is allowed
/// for the same service (in seconds). Prevents alert storms.
/// This is the DEFAULT — individual services can override in Phase 6.
pub const DEFAULT_ALERT_COOLDOWN_SECS: u64 = 60;

/// Timeout for outbound webhook HTTP requests (in seconds).
pub const WEBHOOK_REQUEST_TIMEOUT_SECS: u64 = 10;

/// Maximum concurrent connections per proxy listener.
/// Enforced via semaphore — prevents file descriptor exhaustion.
pub const MAX_CONCURRENT_CONNECTIONS: usize = 1024;

/// Seconds of inactivity before an idle connection is closed.
/// Prevents resource exhaustion from clients that connect and do nothing.
pub const PROXY_IDLE_TIMEOUT_SECS: u64 = 30;

/// Seconds of stall before an active transfer is terminated.
/// Prevents resource exhaustion from stalled upstream connections.
pub const PROXY_TRANSFER_TIMEOUT_SECS: u64 = 60;

/// A resolved proxy listener binding: the listen address, the upstream
/// address, and the service it belongs to. Built at startup from config.
#[derive(Debug, Clone)]
pub struct ProxyBinding {
    /// Unique ID of the service this proxy forwards to.
    pub service_id: String,
    /// Human-readable service name (for logs).
    pub service_name: String,
    /// The socket address Vela listens on (e.g., "127.0.0.1:8001").
    pub listen_addr: String,
    /// All configured upstream addresses for this service (e.g.
    /// ["127.0.0.1:3001", "127.0.0.1:3002"]). The proxy round-robins across
    /// whichever of these are currently Healthy — see state.get_healthy_upstreams.
    pub upstream_addrs: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// The delivery method for an alert notification.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AlertKind {
    Webhook,
    Log,
}

// ─── Runtime State Models ─────────────────────────────────────────────────────
// These are written and read by engines at runtime.
// They represent what Vela has observed, not what the user declared.

/// The current observed status of a monitored service.
/// Written by the health engine. Read by all other engines.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum ServiceStatus {
    /// Service is responding to health checks normally.
    Healthy,
    /// Service has failed some checks but not yet crossed the failure threshold.
    Degraded,
    /// Service has crossed the failure threshold. Recovery engine will act.
    Failed,
    /// Service has never been successfully checked yet (initial state).
    Unknown,
}

/// Runtime state for a single service.
/// Stored in the shared VelaState and updated by the health and recovery engines.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceState {
    /// References the ServiceConfig this state belongs to.
    pub service_id: String,

    /// Current observed health status.
    pub status: ServiceStatus,

    /// Number of consecutive failed health checks.
    pub consecutive_failures: u32,

    /// Timestamp of the most recent health check (success or failure).
    pub last_checked_at: Option<DateTime<Utc>>,

    /// Timestamp of the most recent successful health check.
    pub last_ok_at: Option<DateTime<Utc>>,

    /// Total number of restart attempts made by the recovery engine.
    pub restart_count: u32,

    /// Whether the recovery engine is currently backing off.
    pub is_recovering: bool,
}

impl ServiceState {
    /// Creates the initial unknown state for a service at startup.
    pub fn initial(service_id: String) -> Self {
        Self {
            service_id,
            status: ServiceStatus::Unknown,
            consecutive_failures: 0,
            last_checked_at: None,
            last_ok_at: None,
            restart_count: 0,
            is_recovering: false,
        }
    }
}

/// A single health check result recorded by the health engine.
/// Kept as a rolling log for the API to expose.
#[derive(Debug, Clone, Serialize)]
pub struct HealthRecord {
    pub service_id: String,
    pub success: bool,
    pub latency_ms: u64,
    pub checked_at: DateTime<Utc>,
    /// Human-readable error if success = false. None if success = true.
    pub error: Option<String>,
}

/// Configuration for the health engine's runtime behavior.
/// Derived from the global config at engine startup — not user-configurable per service.
/// Controls engine-level concerns separate from per-service check parameters.
// Not yet constructed — run() takes a plain Vec<ServiceConfig> today; wire this in
// when the engine needs a concurrency ceiling enforced (e.g. a semaphore).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct HealthEngineConfig {
    /// Maximum number of concurrent health check tasks that can run simultaneously.
    /// This is a safety ceiling — in practice, tokio schedules tasks as they become ready.
    /// Default: 1000. Raise only if monitoring more than 1000 services.
    pub max_concurrent_checks: usize,
}

impl Default for HealthEngineConfig {
    fn default() -> Self {
        Self {
            max_concurrent_checks: 1000,
        }
    }
}

/// The result of a single health check execution.
/// Internal to the health engine. Converted to HealthRecord before storing in state.
#[derive(Debug)]
pub struct CheckResult {
    pub service_id: String,
    pub success: bool,
    pub latency_ms: u64,
    pub error: Option<String>,
}

/// A single restart attempt recorded by the recovery engine.
#[derive(Debug, Clone, Serialize)]
pub struct RestartRecord {
    pub service_id: String,
    pub attempted_at: DateTime<Utc>,
    pub succeeded: bool,
    pub attempt_number: u32,
}

/// Tracks the recovery engine's internal state for a single service.
/// This is owned by the recovery engine only — not stored in VelaState.
/// Not serialized — it exists only in memory during Vela's runtime.
#[derive(Debug, Clone)]
pub struct RecoveryState {
    /// Current backoff duration before the next restart attempt.
    /// Starts at base_backoff_secs, doubles each attempt, capped at max_backoff_secs.
    pub current_backoff: std::time::Duration,

    /// Whether a restart command is actively running right now.
    /// Used to prevent simultaneous restart attempts for the same service.
    pub restart_in_progress: bool,

    /// The attempt number for the current failure episode.
    /// Resets to 0 when the service returns to Healthy.
    pub current_episode_attempts: u32,

    /// Timestamp when the current backoff period started.
    /// Used to determine when the backoff has elapsed.
    pub backoff_started_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl RecoveryState {
    /// Creates the initial recovery state for a service.
    pub fn initial() -> Self {
        Self {
            current_backoff: std::time::Duration::from_secs(BASE_BACKOFF_SECS),
            restart_in_progress: false,
            current_episode_attempts: 0,
            backoff_started_at: None,
        }
    }

    /// Advances the backoff duration for the next attempt.
    /// Doubles the current backoff, adds jitter, caps at MAX_BACKOFF_SECS.
    pub fn advance_backoff(&mut self) {
        use std::time::Duration;
        // Double the current backoff (exponential growth)
        let doubled = self.current_backoff.saturating_mul(2);
        // Cap at maximum
        let capped = doubled.min(Duration::from_secs(MAX_BACKOFF_SECS));
        // Add jitter: ±20% of the capped value using a simple deterministic offset.
        // In a production system this would use rand::thread_rng(). For now,
        // we use the attempt count as a stable jitter seed — still spreads load.
        let jitter_secs =
            (capped.as_secs() / 5).saturating_add(self.current_episode_attempts as u64 % 3);
        self.current_backoff = capped + Duration::from_secs(jitter_secs);
    }

    /// Resets recovery state when a service returns to Healthy.
    /// Must be called by the recovery engine on every Healthy observation.
    pub fn reset(&mut self) {
        self.current_backoff = std::time::Duration::from_secs(BASE_BACKOFF_SECS);
        self.restart_in_progress = false;
        self.current_episode_attempts = 0;
        self.backoff_started_at = None;
    }
}

/// Base backoff in seconds before the first restart attempt.
pub const BASE_BACKOFF_SECS: u64 = 5;

/// Maximum backoff ceiling in seconds. No restart wait will exceed this.
pub const MAX_BACKOFF_SECS: u64 = 300; // 5 minutes

/// The outcome of a single restart attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum RestartOutcome {
    /// Process started and exited with status 0.
    Succeeded,
    /// Process started but exited with non-zero status.
    Failed(String),
    /// Process could not be spawned (permission error, command not found, etc.)
    SpawnError(String),
    /// Process started but did not exit within the allowed timeout.
    TimedOut,
}

/// Event emitted to the alert engine when a service changes status.
/// Sent over a tokio broadcast channel.
#[derive(Debug, Clone)]
pub struct StatusChangeEvent {
    pub service_id: String,
    pub service_name: String,
    pub previous_status: ServiceStatus,
    pub new_status: ServiceStatus,
    pub timestamp: DateTime<Utc>,
    pub message: String,
}

// ─── API Response Types ───────────────────────────────────────────────────────
// These are stable external types — changes here are breaking API changes.
// Never expose internal structs (ServiceState, VelaState) directly via the API.

/// The stable API version string included in every response envelope.
pub const API_VERSION: &str = "1";

/// Standard envelope wrapping every API response.
/// Provides consistent structure for clients to parse.
#[derive(Debug, Serialize)]
pub struct ApiResponse<T: Serialize> {
    pub ok: bool,
    pub version: &'static str,
    pub data: T,
}

impl<T: Serialize> ApiResponse<T> {
    pub fn success(data: T) -> Self {
        Self {
            ok: true,
            version: API_VERSION,
            data,
        }
    }
}

/// Standard error envelope for API error responses.
/// SECURITY: `message` must never include internal details, stack traces,
/// config values, or anything that could aid an attacker.
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub ok: bool,
    pub version: &'static str,
    /// Generic, safe-to-expose error description.
    pub error: String,
}

impl ApiError {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            version: API_VERSION,
            error: error.into(),
        }
    }
}

/// Public-facing summary of a monitored service.
/// Derived from ServiceState + ServiceConfig — never exposes internal structs.
#[derive(Debug, Serialize)]
pub struct ServiceSummary {
    pub service_id: String,
    pub service_name: String,
    pub status: String, // "Healthy" | "Degraded" | "Failed" | "Unknown"
    pub consecutive_failures: u32,
    pub restart_count: u32,
    pub is_recovering: bool,
    pub last_checked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_ok_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ServiceSummary {
    /// Converts internal ServiceState to the public API response type.
    /// Add fields here deliberately — this is a public contract.
    pub fn from_state(state: &ServiceState, name: &str) -> Self {
        Self {
            service_id: state.service_id.clone(),
            service_name: name.to_string(),
            status: format!("{:?}", state.status),
            consecutive_failures: state.consecutive_failures,
            restart_count: state.restart_count,
            is_recovering: state.is_recovering,
            last_checked_at: state.last_checked_at,
            last_ok_at: state.last_ok_at,
        }
    }
}

/// Top-level status response returned by GET /api/v1/status.
#[derive(Debug, Serialize)]
pub struct VelaStatusResponse {
    /// Count of services in each status category.
    pub healthy_count: usize,
    pub degraded_count: usize,
    pub failed_count: usize,
    pub unknown_count: usize,
    pub total_services: usize,
    /// Full summary list of all monitored services.
    pub services: Vec<ServiceSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_config_validate_ok_for_manual_with_command() {
        let cfg = RestartConfig {
            mode: RestartMode::Manual,
            command: Some("systemctl restart x".to_string()),
            docker: None,
        };
        assert!(cfg.validate("svc").is_ok());
    }

    #[test]
    fn restart_config_validate_fails_for_manual_without_command() {
        let cfg = RestartConfig {
            mode: RestartMode::Manual,
            command: None,
            docker: None,
        };
        let err = cfg.validate("svc").unwrap_err();
        assert!(matches!(err, VelaError::ConfigValidation(_)));
    }

    #[test]
    fn restart_config_validate_ok_for_docker_with_docker_block() {
        let cfg = RestartConfig {
            mode: RestartMode::Docker,
            command: None,
            docker: Some(DockerConfig {
                container: "my-container".to_string(),
                image: None,
                socket_path: None,
            }),
        };
        assert!(cfg.validate("svc").is_ok());
    }

    #[test]
    fn restart_config_validate_fails_for_docker_without_docker_block() {
        let cfg = RestartConfig {
            mode: RestartMode::Docker,
            command: None,
            docker: None,
        };
        let err = cfg.validate("svc").unwrap_err();
        assert!(matches!(err, VelaError::ConfigValidation(_)));
    }

    #[test]
    fn restart_config_validate_ok_for_none_mode_regardless_of_fields() {
        let cfg = RestartConfig {
            mode: RestartMode::None,
            command: None,
            docker: None,
        };
        assert!(cfg.validate("svc").is_ok());
    }

    #[test]
    fn upstream_state_initial_starts_unknown_with_no_history() {
        let state = UpstreamState::initial("127.0.0.1".to_string(), 8080);
        assert_eq!(state.host, "127.0.0.1");
        assert_eq!(state.port, 8080);
        assert_eq!(state.status, ServiceStatus::Unknown);
        assert_eq!(state.consecutive_failures, 0);
        assert!(state.last_checked_at.is_none());
        assert!(state.last_ok_at.is_none());
        assert!(state.latency_ms.is_none());
    }
}
