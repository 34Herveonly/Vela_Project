//! Health engine — monitors every upstream of every configured service.
//!
//! ## Design
//! One independent tokio task per UPSTREAM (not per service). A service
//! with two upstreams gets two independent check loops. This ensures a
//! slow or timing-out upstream never blocks checks on any other upstream —
//! including a sibling upstream of the same service.
//!
//! ## Docker-aware checks
//! When an upstream's `docker_container` is set and a Docker client is
//! available, the container's running state is checked first — a stopped
//! container is an immediate failure regardless of what TCP/HTTP would say.
//! If the container is running, the existing TCP/HTTP check still runs as
//! confirmation that the service inside it is actually responding. If no
//! Docker client is available, Vela falls back to TCP/HTTP only and logs
//! a warning — Docker is optional, never required.
//!
//! ## Shutdown
//! All tasks hold a clone of a CancellationToken. Calling shutdown() on the
//! HealthEngineHandle cancels the token, which causes every task to exit cleanly
//! on its next iteration. No task is forcefully killed — they drain gracefully.
//!
//! ## Scale
//! Designed and tested for up to 1,000 concurrent upstream checks.
//! Each task is a lightweight tokio green thread — not an OS thread.

use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::docker_engine::{self, DockerClient};
use crate::error::VelaError;
use crate::models::{
    CheckResult, HealthCheckConfig, HealthCheckKind, ServiceConfig, UpstreamConfig,
};
use crate::state::VelaState;

/// Handle returned by `run()`. Gives the caller clean control over the engine.
/// Call `shutdown()` to stop all health check tasks gracefully.
pub struct HealthEngineHandle {
    /// One JoinHandle per monitored upstream.
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

/// Starts the health engine. Spawns one async task per upstream across all
/// configured services. Returns a HealthEngineHandle for lifecycle management.
///
/// # Arguments
/// * `state` — shared state store, cloned per task
/// * `services` — list of service configs to monitor (each with >=1 upstream
///   after config.rs synthesis)
/// * `docker_client` — shared Docker client, or `None` if Docker is
///   unavailable. Docker-mode upstreams fall back to TCP/HTTP-only checks
///   when this is `None` — see the module doc comment.
///
/// # Errors
/// Returns VelaError if no services are provided (programmer error).
pub async fn run(
    state: VelaState,
    services: Vec<ServiceConfig>,
    docker_client: Option<DockerClient>,
) -> Result<HealthEngineHandle, VelaError> {
    if services.is_empty() {
        return Err(VelaError::ConfigValidation(
            "Health engine started with zero services — nothing to monitor".to_string(),
        ));
    }

    let cancellation_token = CancellationToken::new();
    let mut task_handles = Vec::new();

    for service in services {
        let failure_threshold = service.failure_threshold;
        let check_interval_secs = service.check_interval_secs;

        for upstream in &service.upstreams {
            let state_clone = state.clone();
            let token_clone = cancellation_token.clone();
            let service_id = service.id.clone();
            let service_name = service.name.clone();
            let upstream_clone = upstream.clone();
            let health_check = service.health_check.clone();
            let docker_client_clone = docker_client.clone();

            info!(
                "Health engine: starting check loop for '{}' upstream {}:{} every {}s{}",
                service_id,
                upstream.host,
                upstream.port,
                check_interval_secs,
                upstream
                    .docker_container
                    .as_ref()
                    .map(|c| format!(" (docker container '{}')", c))
                    .unwrap_or_default()
            );

            let handle = tokio::spawn(async move {
                run_check_loop(
                    state_clone,
                    service_id,
                    service_name,
                    upstream_clone,
                    health_check,
                    check_interval_secs,
                    failure_threshold,
                    docker_client_clone,
                    token_clone,
                )
                .await;
            });

            task_handles.push(handle);
        }
    }

    info!(
        "Health engine: monitoring {} upstream(s) concurrently",
        task_handles.len()
    );

    Ok(HealthEngineHandle {
        task_handles,
        cancellation_token,
    })
}

/// The check loop for a single upstream. Runs until the cancellation token fires.
/// This function never returns an error — all errors are recorded in state.
/// A panic inside this function would only kill one upstream's check loop,
/// not the engine, and not any sibling upstream of the same service.
#[allow(clippy::too_many_arguments)]
async fn run_check_loop(
    state: VelaState,
    service_id: String,
    service_name: String,
    upstream: UpstreamConfig,
    health_check: HealthCheckConfig,
    check_interval_secs: u64,
    failure_threshold: u32,
    docker_client: Option<DockerClient>,
    token: CancellationToken,
) {
    let check_interval = Duration::from_secs(check_interval_secs);
    let mut ticker = interval(check_interval);

    // Tick immediately fires on first call — this gives us an instant first check
    // rather than waiting a full interval before the first result.
    loop {
        tokio::select! {
            // Cancellation takes priority over the interval tick
            _ = token.cancelled() => {
                debug!(
                    "Health engine: check loop for '{}' upstream {}:{} received shutdown signal",
                    service_id, upstream.host, upstream.port
                );
                return;
            }
            _ = ticker.tick() => {
                let result = execute_check(&upstream, &health_check, &service_id, &docker_client).await;
                record_result(&state, result, &upstream, &service_name, failure_threshold).await;
            }
        }
    }
}

/// Executes a single health check against one upstream.
/// Returns a CheckResult regardless of success or failure — never panics.
///
/// For Docker-backed upstreams: the container's running state is checked
/// first. A stopped container fails immediately, without even attempting
/// the TCP/HTTP check. A running container still must pass the TCP/HTTP
/// check — "the container exists" is not the same claim as "the service
/// inside it is responding."
async fn execute_check(
    upstream: &UpstreamConfig,
    health_check: &HealthCheckConfig,
    service_id: &str,
    docker_client: &Option<DockerClient>,
) -> CheckResult {
    let start = Instant::now();
    let timeout_duration = Duration::from_millis(health_check.timeout_ms);
    let addr = format!("{}:{}", upstream.host, upstream.port);

    if let Some(container) = &upstream.docker_container {
        match docker_client {
            Some(client) => match docker_engine::get_container_status(client, container).await {
                Ok(false) => {
                    let latency_ms = start.elapsed().as_millis() as u64;
                    let reason = format!("Docker container '{}' is not running", container);
                    warn!(
                        "Health check FAILED: service='{}' upstream={} latency={}ms error={}",
                        service_id, addr, latency_ms, reason
                    );
                    return CheckResult {
                        service_id: service_id.to_string(),
                        success: false,
                        latency_ms,
                        error: Some(reason),
                    };
                }
                Err(e) => {
                    let latency_ms = start.elapsed().as_millis() as u64;
                    let reason = format!("Docker status check failed for '{}': {}", container, e);
                    warn!(
                        "Health check FAILED: service='{}' upstream={} latency={}ms error={}",
                        service_id, addr, latency_ms, reason
                    );
                    return CheckResult {
                        service_id: service_id.to_string(),
                        success: false,
                        latency_ms,
                        error: Some(reason),
                    };
                }
                Ok(true) => {
                    // Container is running — fall through to the TCP/HTTP
                    // check below as secondary confirmation.
                }
            },
            None => {
                warn!(
                    "Health engine: upstream {} for '{}' has docker_container set but Docker \
                     is unavailable — falling back to TCP/HTTP only",
                    addr, service_id
                );
            }
        }
    }

    let outcome = match health_check.kind {
        HealthCheckKind::Tcp => check_tcp(&addr, timeout_duration).await,
        HealthCheckKind::Http => {
            let path = health_check.http_path.as_deref().unwrap_or("/health");
            let url = format!("http://{}:{}{}", upstream.host, upstream.port, path);
            check_http(&url, timeout_duration, health_check.timeout_ms).await
        }
    };

    let latency_ms = start.elapsed().as_millis() as u64;

    match outcome {
        Ok(()) => {
            debug!(
                "Health check OK: service='{}' upstream={} latency={}ms",
                service_id, addr, latency_ms
            );
            CheckResult {
                service_id: service_id.to_string(),
                success: true,
                latency_ms,
                error: None,
            }
        }
        Err(e) => {
            warn!(
                "Health check FAILED: service='{}' upstream={} latency={}ms error={}",
                service_id, addr, latency_ms, e
            );
            CheckResult {
                service_id: service_id.to_string(),
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

/// Records one upstream's check outcome in the shared state store.
/// Logs at ERROR (not the usual WARN/DEBUG) because a failure here means
/// the state store itself is broken — a much more serious condition than
/// the upstream simply being down.
async fn record_result(
    state: &VelaState,
    result: CheckResult,
    upstream: &UpstreamConfig,
    service_name: &str,
    failure_threshold: u32,
) {
    if let Err(e) = state
        .record_upstream_health_check(
            &result.service_id,
            &upstream.host,
            upstream.port,
            result.success,
            result.latency_ms,
            result.error,
            failure_threshold,
            service_name.to_string(),
        )
        .await
    {
        error!(
            "CRITICAL: Health engine failed to write check result for '{}' upstream {}:{}: {}",
            result.service_id, upstream.host, upstream.port, e
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
            host: Some("127.0.0.1".to_string()),
            port: Some(port),
            command: None,
            upstreams: vec![UpstreamConfig {
                host: "127.0.0.1".to_string(),
                port,
                docker_container: None,
            }],
            restart: None,
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
        let upstream = &service.upstreams[0];
        let result = execute_check(upstream, &service.health_check, &service.id, &None).await;
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
        let upstream = &service.upstreams[0];
        let result = execute_check(upstream, &service.health_check, &service.id, &None).await;
        assert!(!result.success, "execute_check should fail for closed port");
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn execute_check_falls_back_to_tcp_when_docker_container_set_but_no_client() {
        // docker_container is set but docker_client is None — must not panic,
        // must fall back to the TCP check and still succeed for an open port.
        let (_listener, port) = bind_ephemeral_port();
        let mut service = make_test_service("docker-fallback", port, HealthCheckKind::Tcp);
        service.upstreams[0].docker_container = Some("some-container".to_string());
        let upstream = &service.upstreams[0];
        let result = execute_check(upstream, &service.health_check, &service.id, &None).await;
        assert!(
            result.success,
            "Should fall back to TCP check and succeed when Docker is unavailable"
        );
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
        state
            .register_service_upstreams(
                service.id.clone(),
                service
                    .upstreams
                    .iter()
                    .map(|u| crate::models::UpstreamState::initial(u.host.clone(), u.port))
                    .collect(),
            )
            .await;

        let handle = run(state.clone(), vec![service], None)
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
        state
            .register_service_upstreams(
                service_id.clone(),
                service
                    .upstreams
                    .iter()
                    .map(|u| crate::models::UpstreamState::initial(u.host.clone(), u.port))
                    .collect(),
            )
            .await;

        let handle = run(state.clone(), vec![service], None)
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
        let result = run(state, vec![], None).await;
        assert!(
            result.is_err(),
            "Health engine should return error when started with no services"
        );
    }
}
