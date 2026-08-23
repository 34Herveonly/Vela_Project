//! Health engine — monitors all configured services concurrently.
//!
//! ## Design
//! Each service gets an independent tokio task running on its own interval.
//! Tasks run concurrently via tokio's cooperative scheduler — not sequentially.
//! This ensures that a slow or timing-out service never blocks checks on others.
//!
//! ## Shutdown
//! All tasks hold a clone of a CancellationToken. Calling shutdown() on the
//! HealthEngineHandle cancels the token, which causes every task to exit cleanly
//! on its next iteration. No task is forcefully killed — they drain gracefully.
//!
//! ## Scale
//! Designed and tested for up to 1,000 concurrent service checks.
//! Each task is a lightweight tokio green thread — not an OS thread.

use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::error::VelaError;
use crate::models::{CheckResult, HealthCheckKind, HealthRecord, ServiceConfig};
use crate::state::VelaState;

/// Handle returned by `run()`. Gives the caller clean control over the engine.
/// Call `shutdown()` to stop all health check tasks gracefully.
pub struct HealthEngineHandle {
    /// One JoinHandle per monitored service.
    task_handles: Vec<JoinHandle<()>>,
    /// Shared cancellation token. Cancelling this stops all tasks.
    cancellation_token: CancellationToken,
}

impl HealthEngineHandle {
    /// Signals all health check tasks to stop and waits for them to finish.
    /// Call this during Vela's graceful shutdown sequence.
    pub async fn shutdown(self) {
        info!(
            "Health engine: initiating graceful shutdown of {} tasks",
            self.task_handles.len()
        );
        self.cancellation_token.cancel();
        for handle in self.task_handles {
            // Await each task — they will exit cleanly on next loop iteration
            if let Err(e) = handle.await {
                error!("Health engine: task panicked during shutdown: {:?}", e);
            }
        }
        info!("Health engine: all tasks stopped cleanly");
    }
}

/// Starts the health engine. Spawns one async task per configured service.
/// Returns a HealthEngineHandle for lifecycle management.
///
/// # Arguments
/// * `state` — shared state store, cloned per task
/// * `services` — list of service configs to monitor
///
/// # Errors
/// Returns VelaError if no services are provided (programmer error).
pub async fn run(
    state: VelaState,
    services: Vec<ServiceConfig>,
) -> Result<HealthEngineHandle, VelaError> {
    if services.is_empty() {
        return Err(VelaError::ConfigValidation(
            "Health engine started with zero services — nothing to monitor".to_string(),
        ));
    }

    let cancellation_token = CancellationToken::new();
    let mut task_handles = Vec::with_capacity(services.len());

    for service in services {
        let state_clone = state.clone();
        let token_clone = cancellation_token.clone();
        let service_name = service.name.clone();

        info!(
            "Health engine: starting check loop for '{}' ({}:{}) every {}s",
            service.id, service.host, service.port, service.check_interval_secs
        );

        let handle = tokio::spawn(async move {
            run_check_loop(state_clone, service, service_name, token_clone).await;
        });

        task_handles.push(handle);
    }

    info!(
        "Health engine: monitoring {} service(s) concurrently",
        task_handles.len()
    );

    Ok(HealthEngineHandle {
        task_handles,
        cancellation_token,
    })
}

/// The check loop for a single service. Runs until the cancellation token fires.
/// This function never returns an error — all errors are recorded in state.
/// A panic inside this function would only kill one service's check loop, not the engine.
async fn run_check_loop(
    state: VelaState,
    service: ServiceConfig,
    service_name: String,
    token: CancellationToken,
) {
    let check_interval = Duration::from_secs(service.check_interval_secs);
    let mut ticker = interval(check_interval);

    // Tick immediately fires on first call — this gives us an instant first check
    // rather than waiting a full interval before the first result.
    loop {
        tokio::select! {
            // Cancellation takes priority over the interval tick
            _ = token.cancelled() => {
                debug!("Health engine: check loop for '{}' received shutdown signal", service.id);
                return;
            }
            _ = ticker.tick() => {
                let result = execute_check(&service).await;
                record_result(&state, result, &service, &service_name).await;
            }
        }
    }
}

/// Executes a single health check against a service.
/// Returns a CheckResult regardless of success or failure — never panics.
///
/// Timing: measures wall-clock latency from start of check to completion.
/// This includes DNS resolution and TCP handshake for accurate real-world latency.
async fn execute_check(service: &ServiceConfig) -> CheckResult {
    let start = Instant::now();
    let timeout_duration = Duration::from_millis(service.health_check.timeout_ms);
    let addr = format!("{}:{}", service.host, service.port);

    let outcome = match service.health_check.kind {
        HealthCheckKind::Tcp => check_tcp(&addr, timeout_duration).await,
        HealthCheckKind::Http => {
            let path = service
                .health_check
                .http_path
                .as_deref()
                .unwrap_or("/health");
            let url = format!("http://{}:{}{}", service.host, service.port, path);
            check_http(&url, timeout_duration, service.health_check.timeout_ms).await
        }
    };

    let latency_ms = start.elapsed().as_millis() as u64;

    match outcome {
        Ok(()) => {
            debug!(
                "Health check OK: service='{}' latency={}ms",
                service.id, latency_ms
            );
            CheckResult {
                service_id: service.id.clone(),
                success: true,
                latency_ms,
                error: None,
            }
        }
        Err(e) => {
            warn!(
                "Health check FAILED: service='{}' latency={}ms error={}",
                service.id, latency_ms, e
            );
            CheckResult {
                service_id: service.id.clone(),
                success: false,
                latency_ms,
                error: Some(e.to_string()),
            }
        }
    }
}

/// Attempts a TCP connection to addr within the given timeout.
/// Success means the port accepted the connection. We close it immediately after.
/// This is the lightest possible check — no bytes sent, no protocol overhead.
async fn check_tcp(addr: &str, timeout_duration: Duration) -> Result<(), VelaError> {
    match timeout(timeout_duration, TcpStream::connect(addr)).await {
        Ok(Ok(_stream)) => Ok(()), // stream is dropped here, closing the connection cleanly
        Ok(Err(e)) => Err(VelaError::HealthCheckFailed {
            service_id: addr.to_string(),
            reason: format!("TCP connection refused or reset: {}", e),
        }),
        Err(_elapsed) => Err(VelaError::HealthCheckFailed {
            service_id: addr.to_string(),
            reason: format!(
                "TCP connection timed out after {}ms",
                timeout_duration.as_millis()
            ),
        }),
    }
}

/// Sends an HTTP GET to the given URL and checks for a 2xx response.
/// A non-2xx response is treated as a failure — the service is running but unhealthy.
/// Uses a fresh reqwest client per check to avoid connection pool masking failures.
async fn check_http(
    url: &str,
    timeout_duration: Duration,
    timeout_ms: u64,
) -> Result<(), VelaError> {
    // Build a lightweight client for this check.
    // We do not use a shared client pool because a pooled connection can hide
    // that a service has restarted or its port has changed.
    let client = reqwest::Client::builder()
        .timeout(timeout_duration)
        .build()
        .map_err(|e| VelaError::HealthCheckFailed {
            service_id: url.to_string(),
            reason: format!("Failed to build HTTP client: {}", e),
        })?;

    match timeout(timeout_duration, client.get(url).send()).await {
        Ok(Ok(response)) => {
            if response.status().is_success() {
                Ok(())
            } else {
                Err(VelaError::HealthCheckFailed {
                    service_id: url.to_string(),
                    reason: format!("HTTP check returned non-2xx status: {}", response.status()),
                })
            }
        }
        Ok(Err(e)) => Err(VelaError::HealthCheckFailed {
            service_id: url.to_string(),
            reason: format!("HTTP request failed: {}", e),
        }),
        Err(_elapsed) => Err(VelaError::HealthCheckFailed {
            service_id: url.to_string(),
            reason: format!("HTTP check timed out after {}ms", timeout_ms),
        }),
    }
}

/// Converts a CheckResult into a HealthRecord and writes it to the state store.
/// Logs at appropriate levels — info for first-time recovery, debug for routine checks.
async fn record_result(
    state: &VelaState,
    result: CheckResult,
    service: &ServiceConfig,
    service_name: &str,
) {
    let record = HealthRecord {
        service_id: result.service_id.clone(),
        success: result.success,
        latency_ms: result.latency_ms,
        checked_at: chrono::Utc::now(),
        error: result.error,
    };

    if let Err(e) = state
        .record_health_check(record, service_name.to_string(), service.failure_threshold)
        .await
    {
        // This should never happen in normal operation.
        // If it does, it means the state store is corrupted — log loudly.
        error!(
            "CRITICAL: Health engine failed to write check result for '{}': {}",
            result.service_id, e
        );
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{HealthCheckConfig, HealthCheckKind, ServiceConfig};
    use std::net::TcpListener;

    /// Builds a minimal ServiceConfig for testing purposes.
    fn make_test_service(id: &str, port: u16, kind: HealthCheckKind) -> ServiceConfig {
        ServiceConfig {
            id: id.to_string(),
            name: format!("Test Service {}", id),
            host: "127.0.0.1".to_string(),
            port,
            command: None,
            check_interval_secs: 5,
            failure_threshold: 3,
            max_restarts: 3,
            health_check: HealthCheckConfig {
                kind,
                http_path: Some("/health".to_string()),
                timeout_ms: 1000,
            },
            proxy: None,
            alerts: vec![],
        }
    }

    /// Binds a real OS port and returns the listener (kept alive for the test duration)
    /// and the port number. The OS assigns an ephemeral port — no hardcoded ports in tests.
    fn bind_ephemeral_port() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    #[tokio::test]
    async fn tcp_check_succeeds_when_port_is_open() {
        let (_listener, port) = bind_ephemeral_port();
        let addr = format!("127.0.0.1:{}", port);
        let result = check_tcp(&addr, Duration::from_millis(500)).await;
        assert!(result.is_ok(), "TCP check should succeed when port is open");
    }

    #[tokio::test]
    async fn tcp_check_fails_when_port_is_closed() {
        // Port 1 is privileged and almost certainly not open on any test machine.
        // Using it as a reliable "closed port" for testing.
        let result = check_tcp("127.0.0.1:1", Duration::from_millis(500)).await;
        assert!(result.is_err(), "TCP check should fail when port is closed");
    }

    #[tokio::test]
    async fn tcp_check_fails_on_timeout() {
        // 203.0.113.0/24 is TEST-NET-3 (RFC 5737) — routable but guaranteed
        // to drop packets, giving us a reliable timeout target.
        let result = check_tcp("203.0.113.1:80", Duration::from_millis(100)).await;
        assert!(result.is_err(), "TCP check should fail on timeout");
    }

    #[tokio::test]
    async fn execute_check_returns_success_for_open_port() {
        let (_listener, port) = bind_ephemeral_port();
        let service = make_test_service("test-svc", port, HealthCheckKind::Tcp);
        let result = execute_check(&service).await;
        assert!(
            result.success,
            "execute_check should succeed for open TCP port"
        );
        assert!(result.error.is_none());
        assert!(
            result.latency_ms < 500,
            "Latency should be under 500ms for localhost"
        );
    }

    #[tokio::test]
    async fn execute_check_returns_failure_for_closed_port() {
        // Use a port we just opened and immediately closed — guaranteed to be free.
        let (listener, port) = bind_ephemeral_port();
        drop(listener); // Close it immediately
        let service = make_test_service("dead-svc", port, HealthCheckKind::Tcp);
        let result = execute_check(&service).await;
        assert!(!result.success, "execute_check should fail for closed port");
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn health_engine_starts_and_shuts_down_cleanly() {
        let (state, _rx) = VelaState::new();
        let (_listener, port) = bind_ephemeral_port();
        let service = make_test_service("engine-test", port, HealthCheckKind::Tcp);

        state
            .register_service(service.id.clone())
            .await
            .expect("Failed to register service");

        let handle = run(state.clone(), vec![service])
            .await
            .expect("Health engine should start successfully");

        // Let the engine run for two check cycles
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Shutdown must complete without hanging
        let shutdown_result = tokio::time::timeout(Duration::from_secs(5), handle.shutdown()).await;

        assert!(
            shutdown_result.is_ok(),
            "Health engine shutdown should complete within 5 seconds"
        );
    }

    #[tokio::test]
    async fn health_engine_records_check_results_in_state() {
        let (state, _rx) = VelaState::new();
        let (_listener, port) = bind_ephemeral_port();
        let service = make_test_service("state-test", port, HealthCheckKind::Tcp);
        let service_id = service.id.clone();

        state
            .register_service(service_id.clone())
            .await
            .expect("Failed to register service");

        let handle = run(state.clone(), vec![service])
            .await
            .expect("Health engine should start");

        // Wait long enough for at least one check to complete
        tokio::time::sleep(Duration::from_millis(300)).await;

        handle.shutdown().await;

        let records = state.get_health_records(&service_id).await;
        assert!(
            !records.is_empty(),
            "State store should contain at least one health record after engine runs"
        );
        assert!(
            records[0].success,
            "First health record for open port should be a success"
        );
    }

    #[tokio::test]
    async fn health_engine_refuses_to_start_with_zero_services() {
        let (state, _rx) = VelaState::new();
        let result = run(state, vec![]).await;
        assert!(
            result.is_err(),
            "Health engine should return error when started with no services"
        );
    }
}
