//! All data models for the Vela project.
//!
//! RULE: This is the single source of truth for data types.
//! Never define a struct or enum in an engine file.
//! If you need a new type, add it here first, then use it in the engine.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
// api_port and log_dir are read once the API engine (Phase 6) exists.
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
// Most fields are read once the health/recovery/proxy/alert engines (Phase 2+) exist.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    /// Unique identifier for this service. Used in logs, alerts, and the API.
    /// Must be a lowercase alphanumeric string with hyphens only (e.g. "auth-service").
    pub id: String,

    /// Human-readable name shown in alerts and the dashboard.
    pub name: String,

    /// Hostname or IP of the service.
    pub host: String,

    /// Port the service listens on.
    pub port: u16,

    /// Shell command to restart the service if it fails.
    /// Optional — if absent, Vela monitors but cannot restart.
    /// Example: "systemctl restart auth-service"
    pub command: Option<String>,

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
// listen_port is read once the proxy engine (Phase 5) exists.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ProxyConfig {
    /// Port Vela listens on to receive traffic for this service.
    pub listen_port: u16,
}

/// Configuration for a single alert destination.
// Fields are read once the alert engine (Phase 4) exists.
#[allow(dead_code)]
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

/// Event emitted to the alert engine when a service changes status.
/// Sent over a tokio broadcast channel.
// Fields are constructed by record_health_check but only read once the alert
// engine (Phase 4) subscribes and reads events off the channel.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct StatusChangeEvent {
    pub service_id: String,
    pub service_name: String,
    pub previous_status: ServiceStatus,
    pub new_status: ServiceStatus,
    pub timestamp: DateTime<Utc>,
    pub message: String,
}
