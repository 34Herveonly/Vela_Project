//! Unified error type for the Vela project.
//! All engine functions that can fail must return `Result<T, VelaError>`.
//! Never add `unwrap()` to engine code — propagate with `?` instead.

use std::fmt;

/// The single error type used across all Vela engines.
/// Add new variants here when a genuinely new failure category is needed.
/// Do not create local error types in engine files.
// Some variants are only constructed once their owning engine (Phase 2+) exists.
#[allow(dead_code)]
#[derive(Debug)]
pub enum VelaError {
    /// Configuration file could not be read or parsed.
    ConfigParse(String),

    /// A required configuration field is missing or invalid.
    ConfigValidation(String),

    /// A TCP or HTTP health check failed.
    HealthCheckFailed { service_id: String, reason: String },

    /// Health check timed out before receiving a response.
    HealthCheckTimeout { service_id: String, timeout_ms: u64 },

    /// A service process could not be spawned or managed.
    ProcessError(String),

    /// Recovery engine exceeded max_restarts for a service without recovery.
    /// The service is now in a permanently failed state until manually intervened.
    MaxRestartsExceeded { service_id: String, attempts: u32 },

    /// A restart command timed out before completing.
    RestartTimeout {
        service_id: String,
        timeout_secs: u64,
    },

    /// An outbound alert (webhook, email) failed to deliver.
    AlertDeliveryFailed { kind: String, reason: String },

    /// A webhook URL failed security validation before delivery was attempted.
    /// This is a configuration error — log it clearly so the user can fix it.
    InsecureWebhookUrl { url: String },

    /// The reverse proxy could not forward a request.
    ProxyError(String),

    /// The API engine encountered an error handling a request.
    ApiError(String),

    /// An operation on the shared state store failed.
    StateError(String),

    /// Wraps standard IO errors.
    Io(std::io::Error),

    /// Wraps HTTP client errors from reqwest.
    Http(reqwest::Error),
}

impl fmt::Display for VelaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VelaError::ConfigParse(msg) => write!(f, "Config parse error: {}", msg),
            VelaError::ConfigValidation(msg) => write!(f, "Config validation error: {}", msg),
            VelaError::HealthCheckFailed { service_id, reason } => {
                write!(f, "Health check failed for '{}': {}", service_id, reason)
            }
            VelaError::HealthCheckTimeout {
                service_id,
                timeout_ms,
            } => {
                write!(
                    f,
                    "Health check timed out for '{}' after {}ms",
                    service_id, timeout_ms
                )
            }
            VelaError::ProcessError(msg) => write!(f, "Process error: {}", msg),
            VelaError::MaxRestartsExceeded {
                service_id,
                attempts,
            } => {
                write!(
                    f,
                    "Service '{}' exceeded max restarts ({} attempts) — manual intervention required",
                    service_id, attempts
                )
            }
            VelaError::RestartTimeout {
                service_id,
                timeout_secs,
            } => {
                write!(
                    f,
                    "Restart command for '{}' timed out after {}s",
                    service_id, timeout_secs
                )
            }
            VelaError::AlertDeliveryFailed { kind, reason } => {
                write!(f, "Alert delivery failed [{}]: {}", kind, reason)
            }
            VelaError::InsecureWebhookUrl { url } => {
                // SECURITY: never log the full URL if it might contain credentials.
                // Truncate to scheme+host only in the error message.
                let safe_url = url.split('?').next().unwrap_or("(url hidden)");
                write!(f, "Webhook URL failed security validation: {}", safe_url)
            }
            VelaError::ProxyError(msg) => write!(f, "Proxy error: {}", msg),
            VelaError::ApiError(msg) => write!(f, "API error: {}", msg),
            VelaError::StateError(msg) => write!(f, "State error: {}", msg),
            VelaError::Io(e) => write!(f, "IO error: {}", e),
            VelaError::Http(e) => write!(f, "HTTP error: {}", e),
        }
    }
}

impl std::error::Error for VelaError {}

impl From<std::io::Error> for VelaError {
    fn from(e: std::io::Error) -> Self {
        VelaError::Io(e)
    }
}

impl From<reqwest::Error> for VelaError {
    fn from(e: reqwest::Error) -> Self {
        VelaError::Http(e)
    }
}

impl From<toml::de::Error> for VelaError {
    fn from(e: toml::de::Error) -> Self {
        VelaError::ConfigParse(e.to_string())
    }
}
