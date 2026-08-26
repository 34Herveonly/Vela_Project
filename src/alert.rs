//! Alert engine — delivers notifications when service status changes.
//!
//! ## Design
//! Subscribes to the StatusChangeEvent broadcast channel from VelaState.
//! Runs as a single independent tokio task — slow webhook delivery never
//! blocks the health or recovery engines.
//!
//! ## Alert fatigue prevention
//! Per-service rate limiting: after one alert fires for a service,
//! subsequent alerts are suppressed for DEFAULT_ALERT_COOLDOWN_SECS.
//! A new failure episode (service recovers then fails again) resets the cooldown.
//!
//! ## Security properties
//! - Secrets are never logged at any level
//! - Webhook delivery enforces HTTPS for non-loopback URLs
//! - No redirects followed (SSRF prevention)
//! - Every outbound request has a hard timeout
//! - Payloads are bounded in length

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use reqwest::Client;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::error::VelaError;
use crate::models::{
    AlertKind, AlertRecord, AlertTargetConfig, ServiceConfig, ServiceStatus, StatusChangeEvent,
    DEFAULT_ALERT_COOLDOWN_SECS, MAX_ALERT_MESSAGE_LEN, MAX_ALERT_NAME_LEN,
    WEBHOOK_REQUEST_TIMEOUT_SECS,
};
use crate::state::VelaState;

/// Handle returned by `run()` for lifecycle management.
pub struct AlertEngineHandle {
    task_handle: JoinHandle<()>,
    cancellation_token: CancellationToken,
}

impl AlertEngineHandle {
    /// Signals the alert engine to stop and waits for it to finish.
    /// Any in-flight webhook delivery is allowed to complete.
    pub async fn shutdown(self) {
        info!("Alert engine: initiating graceful shutdown");
        self.cancellation_token.cancel();
        if let Err(e) = self.task_handle.await {
            error!("Alert engine: task panicked during shutdown: {:?}", e);
        }
        info!("Alert engine: stopped cleanly");
    }
}

/// Tracks when the last alert was sent for a service.
/// Used for rate limiting — private to the alert engine.
#[derive(Debug)]
struct RateLimitEntry {
    /// Timestamp of the last alert sent for this service.
    last_alert_at: DateTime<Utc>,
    /// Whether the service was in a failed state when last alerted.
    /// Reset when the service recovers — allows immediate re-alert on next failure.
    alerted_for_failure: bool,
}

/// Starts the alert engine.
///
/// # Arguments
/// * `state` — shared state store (for recording alert history)
/// * `event_rx` — receiver end of the StatusChangeEvent broadcast channel
/// * `services` — service configs (for looking up alert targets per service)
///
/// # Security
/// The returned handle gives the caller shutdown control only.
/// No secrets are accessible through the handle.
pub async fn run(
    state: VelaState,
    event_rx: broadcast::Receiver<StatusChangeEvent>,
    services: Vec<ServiceConfig>,
) -> Result<AlertEngineHandle, VelaError> {
    // Build the shared HTTP client for webhook delivery.
    // Configured once at startup — reused across all deliveries.
    // SECURITY: redirect following disabled (SSRF prevention).
    let http_client = Client::builder()
        .timeout(std::time::Duration::from_secs(WEBHOOK_REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .use_rustls_tls()
        .build()
        .map_err(|e| VelaError::AlertDeliveryFailed {
            kind: "init".to_string(),
            reason: format!("Failed to build HTTP client: {}", e),
        })?;

    // Build a lookup map of service configs for fast alert target resolution.
    let service_map: HashMap<String, ServiceConfig> =
        services.into_iter().map(|s| (s.id.clone(), s)).collect();

    let cancellation_token = CancellationToken::new();
    let token_clone = cancellation_token.clone();

    let task_handle = tokio::spawn(async move {
        run_event_loop(state, event_rx, service_map, http_client, token_clone).await;
    });

    info!("Alert engine: started — listening for status change events");

    Ok(AlertEngineHandle {
        task_handle,
        cancellation_token,
    })
}

/// The main event loop. Receives StatusChangeEvents and dispatches alerts.
async fn run_event_loop(
    state: VelaState,
    mut event_rx: broadcast::Receiver<StatusChangeEvent>,
    service_map: HashMap<String, ServiceConfig>,
    http_client: Client,
    token: CancellationToken,
) {
    // Per-service rate limit state — lives only inside this task.
    // No lock needed — this HashMap is only accessed from this single task.
    let mut rate_limits: HashMap<String, RateLimitEntry> = HashMap::new();

    loop {
        tokio::select! {
            _ = token.cancelled() => {
                debug!("Alert engine: event loop received shutdown signal");
                return;
            }
            result = event_rx.recv() => {
                match result {
                    Ok(event) => {
                        handle_event(
                            &state,
                            &event,
                            &service_map,
                            &http_client,
                            &mut rate_limits,
                        ).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // The broadcast channel dropped n events because we fell
                        // behind. This should not happen in normal operation.
                        // Log loudly but continue — do not crash.
                        error!(
                            "Alert engine: missed {} event(s) — channel overflowed. \
                             Consider increasing broadcast channel capacity if this persists.",
                            n
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // The sender was dropped — this means VelaState was dropped,
                        // which only happens during a clean shutdown sequence.
                        info!("Alert engine: event channel closed — shutting down");
                        return;
                    }
                }
            }
        }
    }
}

/// Handles a single StatusChangeEvent: applies rate limiting, then dispatches
/// to all configured alert targets for the affected service.
async fn handle_event(
    state: &VelaState,
    event: &StatusChangeEvent,
    service_map: &HashMap<String, ServiceConfig>,
    http_client: &Client,
    rate_limits: &mut HashMap<String, RateLimitEntry>,
) {
    // Ignore no-op transitions that generate noise without useful signal.
    if should_suppress_event(event) {
        debug!(
            "Alert engine: suppressing no-op event for '{}' ({:?}→{:?})",
            event.service_id, event.previous_status, event.new_status
        );
        return;
    }

    // Apply per-service rate limiting.
    if is_rate_limited(&event.service_id, &event.new_status, rate_limits) {
        debug!(
            "Alert engine: rate-limited alert for '{}' — cooldown active",
            event.service_id
        );
        return;
    }

    // Retrieve alert targets for this service.
    let service_config = match service_map.get(&event.service_id) {
        Some(s) => s,
        None => {
            error!(
                "Alert engine: received event for unknown service '{}'",
                event.service_id
            );
            return;
        }
    };

    let enabled_targets: Vec<&AlertTargetConfig> =
        service_config.alerts.iter().filter(|t| t.enabled).collect();

    if enabled_targets.is_empty() {
        debug!(
            "Alert engine: no enabled alert targets for '{}' — event suppressed",
            event.service_id
        );
        // Still update rate limit state even if no targets — prevents logic gap.
        update_rate_limit(&event.service_id, &event.new_status, rate_limits);
        return;
    }

    info!(
        "Alert engine: dispatching {} alert(s) for '{}' ({:?}→{:?})",
        enabled_targets.len(),
        event.service_id,
        event.previous_status,
        event.new_status
    );

    // Dispatch to each enabled target.
    for target in enabled_targets {
        let delivered = dispatch_alert(event, target, http_client).await;

        // Record the alert attempt in state (for API engine in Phase 6).
        // SECURITY: AlertRecord contains no secrets.
        let record = AlertRecord {
            service_id: event.service_id.clone(),
            kind: target.kind.clone(),
            delivered,
            trigger: format!("{:?}→{:?}", event.previous_status, event.new_status),
            sent_at: Utc::now(),
            error: if delivered {
                None
            } else {
                Some(format!("Delivery failed for {:?} target", target.kind))
            },
        };

        if let Err(e) = state.record_alert(record).await {
            error!("Alert engine: failed to record alert to state: {}", e);
        }
    }

    // Update rate limit state after successful dispatch.
    update_rate_limit(&event.service_id, &event.new_status, rate_limits);
}

/// Returns true if the event should be suppressed without alerting.
/// Prevents noise from transitions that carry no operational significance.
fn should_suppress_event(event: &StatusChangeEvent) -> bool {
    matches!(
        (&event.previous_status, &event.new_status),
        // Service was already healthy, still healthy — no change
        (ServiceStatus::Healthy, ServiceStatus::Healthy) |
        // Service status unknown at startup — not yet meaningful
        (ServiceStatus::Unknown, ServiceStatus::Unknown) |
        // Degraded→Degraded: still struggling but no escalation
        (ServiceStatus::Degraded, ServiceStatus::Degraded)
    )
}

/// Returns true if this service is currently in its rate limit cooldown window.
/// Rate limiting is reset when a service recovers (Healthy status).
fn is_rate_limited(
    service_id: &str,
    new_status: &ServiceStatus,
    rate_limits: &HashMap<String, RateLimitEntry>,
) -> bool {
    let entry = match rate_limits.get(service_id) {
        Some(e) => e,
        None => return false, // No history → not rate limited
    };

    // A recovery alert (→Healthy) always goes through — operators need to know.
    if *new_status == ServiceStatus::Healthy {
        return false;
    }

    // If the last alert we sent was for a recovery (not a failure), this
    // failure starts a new episode — it must not inherit the previous
    // episode's cooldown. This is what makes "a new failure episode resets
    // the cooldown" (see module docs) actually true, rather than just asserted.
    if !entry.alerted_for_failure {
        return false;
    }

    // Check if cooldown window has elapsed.
    let elapsed = (Utc::now() - entry.last_alert_at).num_seconds() as u64;
    elapsed < DEFAULT_ALERT_COOLDOWN_SECS
}

/// Updates the rate limit entry for a service after an alert is dispatched.
fn update_rate_limit(
    service_id: &str,
    new_status: &ServiceStatus,
    rate_limits: &mut HashMap<String, RateLimitEntry>,
) {
    let alerted_for_failure = matches!(new_status, ServiceStatus::Failed | ServiceStatus::Degraded);

    rate_limits.insert(
        service_id.to_string(),
        RateLimitEntry {
            last_alert_at: Utc::now(),
            alerted_for_failure,
        },
    );
}

/// Dispatches a single alert to a single target.
/// Returns true if delivery succeeded, false on any failure.
/// Failures are logged but non-fatal — the engine continues operating.
async fn dispatch_alert(
    event: &StatusChangeEvent,
    target: &AlertTargetConfig,
    http_client: &Client,
) -> bool {
    match target.kind {
        AlertKind::Webhook => deliver_webhook(event, target, http_client).await,
        AlertKind::Log => {
            deliver_log(event);
            true // Log delivery never fails
        }
    }
}

/// Delivers an alert via HTTP POST to the configured webhook endpoint.
///
/// # Security properties enforced here
/// 1. HTTPS required for non-loopback URLs
/// 2. No redirect following
/// 3. Hard timeout (configured at client level)
/// 4. Bounded payload — no unbounded strings
/// 5. URL is validated before request is sent
/// 6. SECURITY: endpoint URL is NEVER logged in full — may contain tokens
async fn deliver_webhook(
    event: &StatusChangeEvent,
    target: &AlertTargetConfig,
    http_client: &Client,
) -> bool {
    let endpoint = match &target.endpoint {
        Some(url) if !url.trim().is_empty() => url.trim(),
        _ => {
            error!(
                "Alert engine: webhook target for '{}' has no endpoint configured",
                event.service_id
            );
            return false;
        }
    };

    // SECURITY: Validate URL before attempting delivery.
    if let Err(reason) = validate_webhook_url(endpoint) {
        // Log the scheme+host only — never the full URL (may contain tokens)
        warn!(
            "Alert engine: webhook URL for '{}' failed security validation: {}",
            event.service_id, reason
        );
        return false;
    }

    // Build a bounded, sanitized payload.
    let payload = build_webhook_payload(event);

    // SECURITY: Log only the service_id and status — not the endpoint URL.
    debug!(
        "Alert engine: delivering webhook alert for '{}' ({:?}→{:?})",
        event.service_id, event.previous_status, event.new_status
    );

    match http_client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("User-Agent", "Vela-Alert-Engine/1.0")
        .json(&payload)
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!(
                    "Alert engine: webhook delivered for '{}' — status {}",
                    event.service_id,
                    response.status()
                );
                true
            } else {
                warn!(
                    "Alert engine: webhook for '{}' returned non-2xx status: {}",
                    event.service_id,
                    response.status()
                );
                false
            }
        }
        Err(e) => {
            // SECURITY: Do not include the endpoint URL in this log line.
            error!(
                "Alert engine: webhook delivery failed for '{}': {}",
                event.service_id, e
            );
            false
        }
    }
}

/// Validates a webhook URL for security compliance.
/// Returns Ok(()) if the URL is safe to use, Err(reason) otherwise.
///
/// Rules:
/// - Must be a valid URL
/// - Must use HTTPS scheme, unless the host is loopback (127.0.0.1 / localhost)
/// - Must not be empty
fn validate_webhook_url(url: &str) -> Result<(), String> {
    let parsed = url
        .parse::<reqwest::Url>()
        .map_err(|e| format!("Invalid URL: {}", e))?;

    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "Unsupported scheme '{}' — only http/https allowed",
            scheme
        ));
    }

    // Require HTTPS for non-loopback endpoints.
    if scheme == "http" {
        let host = parsed.host_str().unwrap_or("");
        let is_loopback = host == "localhost" || host == "127.0.0.1" || host == "::1";

        if !is_loopback {
            return Err(format!(
                "Non-loopback webhook URL must use HTTPS — got HTTP for host '{}'",
                host
            ));
        }
    }

    Ok(())
}

/// Builds the JSON webhook payload for an alert event.
/// All strings are truncated to prevent unbounded payloads.
/// SECURITY: This function must never include secrets, API keys, or credentials.
fn build_webhook_payload(event: &StatusChangeEvent) -> serde_json::Value {
    // Truncate strings to defined maximums.
    let service_name = truncate_str(&event.service_name, MAX_ALERT_NAME_LEN);
    let message = truncate_str(&event.message, MAX_ALERT_MESSAGE_LEN);

    serde_json::json!({
        "source": "vela",
        "version": "1",
        "event": {
            "service_id": event.service_id,
            "service_name": service_name,
            "status_from": format!("{:?}", event.previous_status),
            "status_to": format!("{:?}", event.new_status),
            "message": message,
            "timestamp": event.timestamp.to_rfc3339(),
        }
    })
}

/// Delivers an alert to the structured log output.
/// Used for the `kind = "log"` alert target.
/// This is always successful — logs cannot fail to deliver.
fn deliver_log(event: &StatusChangeEvent) {
    match event.new_status {
        ServiceStatus::Failed => error!(
            "[ALERT] service='{}' name='{}' status={:?}→{:?} msg='{}'",
            event.service_id,
            truncate_str(&event.service_name, MAX_ALERT_NAME_LEN),
            event.previous_status,
            event.new_status,
            truncate_str(&event.message, MAX_ALERT_MESSAGE_LEN),
        ),
        ServiceStatus::Degraded => warn!(
            "[ALERT] service='{}' name='{}' status={:?}→{:?} msg='{}'",
            event.service_id,
            truncate_str(&event.service_name, MAX_ALERT_NAME_LEN),
            event.previous_status,
            event.new_status,
            truncate_str(&event.message, MAX_ALERT_MESSAGE_LEN),
        ),
        _ => info!(
            "[ALERT] service='{}' name='{}' status={:?}→{:?} msg='{}'",
            event.service_id,
            truncate_str(&event.service_name, MAX_ALERT_NAME_LEN),
            event.previous_status,
            event.new_status,
            truncate_str(&event.message, MAX_ALERT_MESSAGE_LEN),
        ),
    }
}

/// Truncates a string to a maximum byte length, appending "…" if truncated.
/// Used to ensure bounded payloads in all outbound alert content.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        // Truncate at a char boundary to avoid invalid UTF-8
        let truncated = &s[..s
            .char_indices()
            .take_while(|(i, _)| *i < max_len - 3)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0)];
        format!("{}…", truncated)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AlertKind, AlertTargetConfig, HealthCheckConfig, HealthCheckKind, ServiceConfig,
        ServiceStatus, StatusChangeEvent,
    };
    use crate::state::VelaState;
    use chrono::Utc;

    fn make_event(service_id: &str, from: ServiceStatus, to: ServiceStatus) -> StatusChangeEvent {
        StatusChangeEvent {
            service_id: service_id.to_string(),
            service_name: format!("Service {}", service_id),
            previous_status: from,
            new_status: to,
            timestamp: Utc::now(),
            message: "Test status change".to_string(),
        }
    }

    fn make_service_with_alerts(id: &str, alerts: Vec<AlertTargetConfig>) -> ServiceConfig {
        ServiceConfig {
            id: id.to_string(),
            name: format!("Service {}", id),
            host: Some("127.0.0.1".to_string()),
            port: Some(9999),
            command: None,
            upstreams: vec![],
            restart: None,
            check_interval_secs: 10,
            failure_threshold: 3,
            max_restarts: 3,
            health_check: HealthCheckConfig {
                kind: HealthCheckKind::Tcp,
                http_path: None,
                timeout_ms: 1000,
            },
            proxy: None,
            alerts,
        }
    }

    // ── should_suppress_event ──────────────────────────────────────────────

    #[test]
    fn suppress_healthy_to_healthy_transition() {
        let event = make_event("svc", ServiceStatus::Healthy, ServiceStatus::Healthy);
        assert!(
            should_suppress_event(&event),
            "Healthy→Healthy should be suppressed"
        );
    }

    #[test]
    fn suppress_unknown_to_unknown_transition() {
        let event = make_event("svc", ServiceStatus::Unknown, ServiceStatus::Unknown);
        assert!(
            should_suppress_event(&event),
            "Unknown→Unknown should be suppressed"
        );
    }

    #[test]
    fn do_not_suppress_healthy_to_failed() {
        let event = make_event("svc", ServiceStatus::Healthy, ServiceStatus::Failed);
        assert!(
            !should_suppress_event(&event),
            "Healthy→Failed should NOT be suppressed"
        );
    }

    #[test]
    fn do_not_suppress_failed_to_healthy() {
        let event = make_event("svc", ServiceStatus::Failed, ServiceStatus::Healthy);
        assert!(
            !should_suppress_event(&event),
            "Failed→Healthy should NOT be suppressed"
        );
    }

    #[test]
    fn do_not_suppress_unknown_to_failed() {
        let event = make_event("svc", ServiceStatus::Unknown, ServiceStatus::Failed);
        assert!(
            !should_suppress_event(&event),
            "Unknown→Failed should NOT be suppressed"
        );
    }

    // ── Rate limiting ──────────────────────────────────────────────────────

    #[test]
    fn new_service_is_not_rate_limited() {
        let rate_limits = HashMap::new();
        assert!(
            !is_rate_limited("new-svc", &ServiceStatus::Failed, &rate_limits),
            "New service with no history should not be rate limited"
        );
    }

    #[test]
    fn service_is_rate_limited_within_cooldown_window() {
        let mut rate_limits = HashMap::new();
        // Simulate an alert that just fired
        rate_limits.insert(
            "svc".to_string(),
            RateLimitEntry {
                last_alert_at: Utc::now(),
                alerted_for_failure: true,
            },
        );
        assert!(
            is_rate_limited("svc", &ServiceStatus::Failed, &rate_limits),
            "Service should be rate limited immediately after an alert"
        );
    }

    #[test]
    fn recovery_alert_bypasses_rate_limit() {
        let mut rate_limits = HashMap::new();
        rate_limits.insert(
            "svc".to_string(),
            RateLimitEntry {
                last_alert_at: Utc::now(),
                alerted_for_failure: true,
            },
        );
        // A Healthy transition must always get through — operators need recovery confirmation
        assert!(
            !is_rate_limited("svc", &ServiceStatus::Healthy, &rate_limits),
            "Recovery (→Healthy) alert must never be rate limited"
        );
    }

    #[test]
    fn new_failure_episode_after_recovery_bypasses_previous_cooldown() {
        let mut rate_limits = HashMap::new();
        // Last alert we sent was a recovery notification (Healthy), not a failure.
        rate_limits.insert(
            "svc".to_string(),
            RateLimitEntry {
                last_alert_at: Utc::now(),
                alerted_for_failure: false,
            },
        );
        // A fresh failure right after recovery must not be swallowed by the
        // recovery alert's own cooldown — it's a new episode.
        assert!(
            !is_rate_limited("svc", &ServiceStatus::Failed, &rate_limits),
            "A new failure episode after recovery should bypass the old cooldown"
        );
    }

    // ── URL validation (security) ──────────────────────────────────────────

    #[test]
    fn https_external_url_is_valid() {
        assert!(
            validate_webhook_url("https://hooks.example.com/vela").is_ok(),
            "HTTPS external URL should pass validation"
        );
    }

    #[test]
    fn http_localhost_is_valid() {
        assert!(
            validate_webhook_url("http://localhost:9000/webhook").is_ok(),
            "HTTP to localhost should be allowed"
        );
    }

    #[test]
    fn http_loopback_ip_is_valid() {
        assert!(
            validate_webhook_url("http://127.0.0.1:9000/webhook").is_ok(),
            "HTTP to 127.0.0.1 should be allowed"
        );
    }

    #[test]
    fn http_external_url_is_rejected() {
        let result = validate_webhook_url("http://hooks.example.com/vela");
        assert!(
            result.is_err(),
            "HTTP to external host should be rejected: {:?}",
            result
        );
    }

    #[test]
    fn invalid_url_is_rejected() {
        let result = validate_webhook_url("not-a-url-at-all");
        assert!(result.is_err(), "Invalid URL should be rejected");
    }

    #[test]
    fn ftp_url_is_rejected() {
        let result = validate_webhook_url("ftp://example.com/webhook");
        assert!(result.is_err(), "Non-HTTP scheme should be rejected");
    }

    // ── Payload construction ───────────────────────────────────────────────

    #[test]
    fn webhook_payload_has_required_fields() {
        let event = make_event("auth-svc", ServiceStatus::Healthy, ServiceStatus::Failed);
        let payload = build_webhook_payload(&event);

        assert_eq!(payload["source"], "vela");
        assert_eq!(payload["version"], "1");
        assert!(payload["event"]["service_id"].is_string());
        assert!(payload["event"]["status_from"].is_string());
        assert!(payload["event"]["status_to"].is_string());
        assert!(payload["event"]["timestamp"].is_string());
    }

    #[test]
    fn webhook_payload_truncates_long_service_name() {
        let long_name = "x".repeat(MAX_ALERT_NAME_LEN + 50);
        let event = StatusChangeEvent {
            service_id: "svc".to_string(),
            service_name: long_name,
            previous_status: ServiceStatus::Healthy,
            new_status: ServiceStatus::Failed,
            timestamp: Utc::now(),
            message: "test".to_string(),
        };
        let payload = build_webhook_payload(&event);
        let name = payload["event"]["service_name"].as_str().unwrap();
        assert!(
            name.len() <= MAX_ALERT_NAME_LEN + 4, // +4 for the "…" suffix
            "Service name should be truncated in payload"
        );
    }

    // ── truncate_str ───────────────────────────────────────────────────────

    #[test]
    fn truncate_str_leaves_short_strings_unchanged() {
        assert_eq!(truncate_str("hello", 100), "hello");
    }

    #[test]
    fn truncate_str_truncates_long_strings() {
        let long = "a".repeat(200);
        let result = truncate_str(&long, 50);
        assert!(
            result.len() <= 54,
            "Truncated string should be at most max_len + ellipsis"
        );
        assert!(
            result.ends_with('…'),
            "Truncated string should end with ellipsis"
        );
    }

    // ── Engine lifecycle ───────────────────────────────────────────────────

    #[tokio::test]
    async fn alert_engine_starts_and_shuts_down_cleanly() {
        let (state, event_rx) = VelaState::new();
        let service = make_service_with_alerts(
            "lifecycle-test",
            vec![AlertTargetConfig {
                kind: AlertKind::Log,
                endpoint: None,
                enabled: true,
            }],
        );
        state.register_service(service.id.clone()).await.unwrap();

        let handle = run(state, event_rx, vec![service])
            .await
            .expect("Alert engine should start");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), handle.shutdown()).await;

        assert!(
            result.is_ok(),
            "Alert engine should shut down within 5 seconds"
        );
    }

    #[tokio::test]
    async fn alert_engine_processes_log_alert_for_failed_event() {
        let (state, event_rx) = VelaState::new();
        let service = make_service_with_alerts(
            "log-alert-test",
            vec![AlertTargetConfig {
                kind: AlertKind::Log,
                endpoint: None,
                enabled: true,
            }],
        );
        state.register_service(service.id.clone()).await.unwrap();

        let handle = run(state.clone(), event_rx, vec![service])
            .await
            .expect("Alert engine should start");

        // Broadcast a Failed event directly on the channel
        let event = make_event(
            "log-alert-test",
            ServiceStatus::Healthy,
            ServiceStatus::Failed,
        );
        let _ = state.event_tx.send(event);

        // Give engine time to process the event
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Verify the alert was recorded in state
        let records = state.get_alert_records("log-alert-test").await;
        assert!(
            !records.is_empty(),
            "Alert record should be persisted to state after processing"
        );
        assert!(
            records[0].delivered,
            "Log alert should always be recorded as delivered"
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn alert_engine_suppresses_healthy_to_healthy_event() {
        let (state, event_rx) = VelaState::new();
        let service = make_service_with_alerts(
            "suppress-test",
            vec![AlertTargetConfig {
                kind: AlertKind::Log,
                endpoint: None,
                enabled: true,
            }],
        );
        state.register_service(service.id.clone()).await.unwrap();

        let handle = run(state.clone(), event_rx, vec![service])
            .await
            .expect("Alert engine should start");

        // Send a Healthy→Healthy event — should be suppressed
        let event = make_event(
            "suppress-test",
            ServiceStatus::Healthy,
            ServiceStatus::Healthy,
        );
        let _ = state.event_tx.send(event);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // No alert record should exist — event was suppressed
        let records = state.get_alert_records("suppress-test").await;
        assert!(
            records.is_empty(),
            "Healthy→Healthy event should produce no alert record"
        );

        handle.shutdown().await;
    }
}
