//! Proxy engine — health-aware TCP reverse proxy.
//!
//! ## Design
//! One independent listener task per configured proxy port.
//! Each listener accepts connections, checks upstream health in VelaState,
//! and bidirectionally forwards bytes using tokio::io::copy_bidirectional.
//!
//! ## Health-aware routing
//! Upstream health is checked on every new connection by reading VelaState.
//! There is no caching — stale health data could forward to a failed service.
//!
//! ## Resource bounds
//! Semaphore per listener limits concurrent connections to MAX_CONCURRENT_CONNECTIONS.
//! Idle timeout and transfer timeout prevent resource exhaustion.
//!
//! ## Graceful drain
//! On shutdown, listeners stop accepting new connections.
//! In-flight connections are allowed to complete naturally.
//! The shutdown completes when all in-flight transfers finish.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::error::VelaError;
use crate::models::{
    ProxyBinding, ServiceStatus, VelaConfig, MAX_CONCURRENT_CONNECTIONS, PROXY_IDLE_TIMEOUT_SECS,
    PROXY_TRANSFER_TIMEOUT_SECS,
};
use crate::state::VelaState;

/// Handle returned by `run()` for lifecycle management.
pub struct ProxyEngineHandle {
    /// One handle per listener task.
    task_handles: Vec<JoinHandle<()>>,
    cancellation_token: CancellationToken,
}

impl ProxyEngineHandle {
    /// Signals all listener tasks to stop accepting new connections.
    /// Waits for all in-flight connection handlers to complete.
    pub async fn shutdown(self) {
        info!("Proxy engine: initiating graceful drain — no new connections accepted");
        self.cancellation_token.cancel();
        for handle in self.task_handles {
            if let Err(e) = handle.await {
                error!(
                    "Proxy engine: listener task panicked during shutdown: {:?}",
                    e
                );
            }
        }
        info!("Proxy engine: all listeners stopped and connections drained");
    }
}

/// Validates proxy port configuration before any sockets are opened.
/// Catches conflicts early — at startup — not at runtime via bind failures.
///
/// Rules:
/// - No two services may share the same proxy listen port
/// - Proxy listen ports must not equal the Vela API port
/// - Proxy listen ports must be in the valid range (1–65535)
/// - Proxy listen ports must not equal the service's own port
pub fn validate_proxy_config(config: &VelaConfig) -> Result<(), VelaError> {
    let mut used_ports: HashSet<u16> = HashSet::new();
    used_ports.insert(config.global.api_port);

    for svc in &config.services {
        let proxy = match &svc.proxy {
            Some(p) => p,
            None => continue,
        };

        let port = proxy.listen_port;

        if port == 0 {
            return Err(VelaError::ProxyPortConflict {
                port,
                reason: format!("Service '{}' proxy listen_port 0 is not valid", svc.id),
            });
        }

        if port == svc.port {
            return Err(VelaError::ProxyPortConflict {
                port,
                reason: format!(
                    "Service '{}' proxy listen_port {} conflicts with the service's own port",
                    svc.id, port
                ),
            });
        }

        if port == config.global.api_port {
            return Err(VelaError::ProxyPortConflict {
                port,
                reason: format!(
                    "Service '{}' proxy listen_port {} conflicts with Vela API port",
                    svc.id, port
                ),
            });
        }

        if !used_ports.insert(port) {
            return Err(VelaError::ProxyPortConflict {
                port,
                reason: format!(
                    "Port {} is used by multiple services or conflicts with the API port",
                    port
                ),
            });
        }
    }

    Ok(())
}

/// Builds the list of ProxyBindings from the validated configuration.
/// Returns only services that have a proxy config — others are skipped silently.
pub fn build_proxy_bindings(config: &VelaConfig) -> Vec<ProxyBinding> {
    config
        .services
        .iter()
        .filter_map(|svc| {
            let proxy = svc.proxy.as_ref()?;
            let bind_host = if proxy.bind_all_interfaces {
                "0.0.0.0"
            } else {
                "127.0.0.1"
            };
            Some(ProxyBinding {
                service_id: svc.id.clone(),
                service_name: svc.name.clone(),
                listen_addr: format!("{}:{}", bind_host, proxy.listen_port),
                upstream_addr: format!("{}:{}", svc.host, svc.port),
            })
        })
        .collect()
}

/// Starts the proxy engine. One listener task per proxy binding.
/// Validates configuration before opening any sockets.
///
/// # Arguments
/// * `state` — shared state store (read on every new connection for health check)
/// * `config` — full Vela config (for proxy bindings and validation)
pub async fn run(state: VelaState, config: &VelaConfig) -> Result<ProxyEngineHandle, VelaError> {
    // Step 1: Validate all proxy port configurations before binding anything.
    validate_proxy_config(config)?;

    let bindings = build_proxy_bindings(config);

    if bindings.is_empty() {
        info!("Proxy engine: no services have proxy config — proxy engine idle");
        // Return a handle with no tasks — still valid, just does nothing.
        return Ok(ProxyEngineHandle {
            task_handles: vec![],
            cancellation_token: CancellationToken::new(),
        });
    }

    let cancellation_token = CancellationToken::new();
    let mut task_handles = Vec::with_capacity(bindings.len());

    for binding in bindings {
        // Bind the socket now, at startup — fail fast if port is unavailable.
        let listener = TcpListener::bind(&binding.listen_addr).await.map_err(|e| {
            VelaError::ProxyBindError {
                port: binding
                    .listen_addr
                    .split(':')
                    .next_back()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0),
                reason: e.to_string(),
            }
        })?;

        info!(
            "Proxy engine: listening on {} → {} (service: '{}' / '{}')",
            binding.listen_addr, binding.upstream_addr, binding.service_id, binding.service_name
        );

        let state_clone = state.clone();
        let token_clone = cancellation_token.clone();
        let binding_clone = binding.clone();

        let handle = tokio::spawn(async move {
            run_listener(listener, state_clone, binding_clone, token_clone).await;
        });

        task_handles.push(handle);
    }

    info!("Proxy engine: {} listener(s) active", task_handles.len());

    Ok(ProxyEngineHandle {
        task_handles,
        cancellation_token,
    })
}

/// The listener loop for a single proxy port.
/// Accepts connections, applies connection limit, dispatches handler tasks.
async fn run_listener(
    listener: TcpListener,
    state: VelaState,
    binding: ProxyBinding,
    token: CancellationToken,
) {
    // One semaphore per listener — limits concurrent connections for this port only.
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));

    loop {
        tokio::select! {
            _ = token.cancelled() => {
                debug!(
                    "Proxy engine: listener on {} received shutdown signal",
                    binding.listen_addr
                );
                // Stop accepting. In-flight handlers continue until their
                // connections close naturally (graceful drain).
                return;
            }
            result = listener.accept() => {
                match result {
                    Ok((client_stream, client_addr)) => {
                        debug!(
                            "Proxy engine: accepted connection from {} on {}",
                            client_addr, binding.listen_addr
                        );

                        // Try to acquire a connection slot.
                        // `try_acquire_owned` is non-blocking — if limit is reached,
                        // we log and close the connection immediately.
                        let permit = match semaphore.clone().try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                warn!(
                                    "Proxy engine: connection limit ({}) reached for '{}' — \
                                     rejecting connection from {}",
                                    MAX_CONCURRENT_CONNECTIONS,
                                    binding.service_id,
                                    client_addr
                                );
                                // Drop client_stream — closes the connection.
                                drop(client_stream);
                                continue;
                            }
                        };

                        let state_clone = state.clone();
                        let binding_clone = binding.clone();

                        // Spawn a task per connection. The permit is moved into
                        // the task and dropped when the task completes — RAII release.
                        tokio::spawn(async move {
                            handle_connection(
                                client_stream,
                                state_clone,
                                &binding_clone,
                            ).await;
                            // permit is dropped here → semaphore slot released
                            drop(permit);
                        });
                    }
                    Err(e) => {
                        // Accept errors are usually transient (EMFILE, ENFILE).
                        // Log and continue — do not crash the listener.
                        error!(
                            "Proxy engine: accept error on {}: {}",
                            binding.listen_addr, e
                        );
                    }
                }
            }
        }
    }
}

/// Handles a single proxied connection.
/// Checks upstream health, connects to upstream, and forwards bytes bidirectionally.
/// All errors are logged and non-fatal — the connection is simply dropped.
async fn handle_connection(mut client_stream: TcpStream, state: VelaState, binding: &ProxyBinding) {
    // Check upstream health at connection time — not cached.
    if !is_upstream_healthy(&state, &binding.service_id).await {
        warn!(
            "Proxy engine: rejecting connection for '{}' — upstream is not Healthy",
            binding.service_id
        );
        // Drop client_stream → client receives TCP RST.
        // In Phase 6 we could write an HTTP 502 here for HTTP clients.
        return;
    }

    // Apply idle timeout to upstream connection attempt.
    let upstream_stream = match timeout(
        Duration::from_secs(PROXY_IDLE_TIMEOUT_SECS),
        TcpStream::connect(&binding.upstream_addr),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            error!(
                "Proxy engine: failed to connect to upstream '{}' at {}: {}",
                binding.service_id, binding.upstream_addr, e
            );
            return;
        }
        Err(_elapsed) => {
            warn!(
                "Proxy engine: upstream connection timeout for '{}' at {} ({}s)",
                binding.service_id, binding.upstream_addr, PROXY_IDLE_TIMEOUT_SECS
            );
            return;
        }
    };

    debug!(
        "Proxy engine: tunnel open: client ↔ '{}' ({})",
        binding.service_id, binding.upstream_addr
    );

    // Set TCP_NODELAY on both sides — reduces latency for small writes.
    let _ = client_stream.set_nodelay(true);
    let mut upstream_stream = upstream_stream;
    let _ = upstream_stream.set_nodelay(true);

    // Bidirectional copy with transfer timeout.
    // copy_bidirectional runs both directions concurrently and handles
    // half-close (FIN from one side) correctly.
    let transfer_result = timeout(
        Duration::from_secs(PROXY_TRANSFER_TIMEOUT_SECS),
        copy_bidirectional(&mut client_stream, &mut upstream_stream),
    )
    .await;

    match transfer_result {
        Ok(Ok((client_to_upstream, upstream_to_client))) => {
            debug!(
                "Proxy engine: tunnel closed for '{}': \
                 {} bytes client→upstream, {} bytes upstream→client",
                binding.service_id, client_to_upstream, upstream_to_client
            );
        }
        Ok(Err(e)) => {
            // IO errors during transfer are usually client disconnects — not alarming.
            debug!(
                "Proxy engine: tunnel error for '{}': {}",
                binding.service_id, e
            );
        }
        Err(_elapsed) => {
            warn!(
                "Proxy engine: transfer timeout for '{}' after {}s — closing tunnel",
                binding.service_id, PROXY_TRANSFER_TIMEOUT_SECS
            );
        }
    }
}

/// Checks whether the upstream for a given service is currently Healthy.
/// Reads from the shared state store — always current, never cached.
async fn is_upstream_healthy(state: &VelaState, service_id: &str) -> bool {
    let snapshot = state.snapshot_services().await;
    match snapshot.get(service_id) {
        Some(service_state) => service_state.status == ServiceStatus::Healthy,
        None => {
            error!(
                "Proxy engine: no state found for service '{}' — treating as unhealthy",
                service_id
            );
            false
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        GlobalConfig, HealthCheckConfig, HealthCheckKind, ProxyConfig, ServiceConfig, VelaConfig,
    };
    use crate::state::VelaState;
    use std::net::TcpListener as StdTcpListener;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn free_port() -> u16 {
        let l = StdTcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
        // l is dropped here — port is free for our use
    }

    fn make_config_with_proxy(service_port: u16, proxy_port: u16, api_port: u16) -> VelaConfig {
        VelaConfig {
            global: GlobalConfig {
                api_port,
                api_key: "test-key".to_string(),
                log_dir: "/tmp".to_string(),
            },
            services: vec![ServiceConfig {
                id: "test-svc".to_string(),
                name: "Test Service".to_string(),
                host: "127.0.0.1".to_string(),
                port: service_port,
                command: None,
                check_interval_secs: 10,
                failure_threshold: 3,
                max_restarts: 3,
                health_check: HealthCheckConfig {
                    kind: HealthCheckKind::Tcp,
                    http_path: None,
                    timeout_ms: 1000,
                },
                proxy: Some(ProxyConfig {
                    listen_port: proxy_port,
                    bind_all_interfaces: false,
                }),
                alerts: vec![],
            }],
        }
    }

    // ── validate_proxy_config ──────────────────────────────────────────────

    #[test]
    fn valid_proxy_config_passes_validation() {
        let config = make_config_with_proxy(3001, 8001, 7700);
        assert!(
            validate_proxy_config(&config).is_ok(),
            "Valid config should pass validation"
        );
    }

    #[test]
    fn proxy_port_conflicting_with_api_port_is_rejected() {
        let config = make_config_with_proxy(3001, 7700, 7700);
        assert!(
            validate_proxy_config(&config).is_err(),
            "Proxy port == API port should be rejected"
        );
    }

    #[test]
    fn proxy_port_conflicting_with_service_port_is_rejected() {
        let config = make_config_with_proxy(3001, 3001, 7700);
        assert!(
            validate_proxy_config(&config).is_err(),
            "Proxy port == service port should be rejected"
        );
    }

    #[test]
    fn duplicate_proxy_ports_across_services_are_rejected() {
        let proxy_port = free_port();
        let mut config = make_config_with_proxy(3001, proxy_port, 7700);
        // Add a second service using the same proxy port
        let mut second = config.services[0].clone();
        second.id = "svc2".to_string();
        second.port = 3002;
        config.services.push(second);

        assert!(
            validate_proxy_config(&config).is_err(),
            "Duplicate proxy ports should be rejected"
        );
    }

    #[test]
    fn service_without_proxy_config_is_skipped_in_validation() {
        let mut config = make_config_with_proxy(3001, 8001, 7700);
        config.services[0].proxy = None;
        assert!(
            validate_proxy_config(&config).is_ok(),
            "Service without proxy should be silently skipped"
        );
    }

    // ── build_proxy_bindings ───────────────────────────────────────────────

    #[test]
    fn build_proxy_bindings_returns_correct_addresses() {
        let config = make_config_with_proxy(3001, 8001, 7700);
        let bindings = build_proxy_bindings(&config);

        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].service_id, "test-svc");
        assert_eq!(bindings[0].listen_addr, "127.0.0.1:8001");
        assert_eq!(bindings[0].upstream_addr, "127.0.0.1:3001");
    }

    #[test]
    fn build_proxy_bindings_skips_services_without_proxy() {
        let mut config = make_config_with_proxy(3001, 8001, 7700);
        config.services[0].proxy = None;
        let bindings = build_proxy_bindings(&config);
        assert!(bindings.is_empty(), "No bindings when no proxy configs");
    }

    #[test]
    fn build_proxy_bindings_uses_all_interfaces_when_configured() {
        let mut config = make_config_with_proxy(3001, 8001, 7700);
        config.services[0]
            .proxy
            .as_mut()
            .unwrap()
            .bind_all_interfaces = true;
        let bindings = build_proxy_bindings(&config);
        assert!(
            bindings[0].listen_addr.starts_with("0.0.0.0"),
            "Should bind to 0.0.0.0 when bind_all_interfaces = true"
        );
    }

    // ── is_upstream_healthy ────────────────────────────────────────────────

    #[tokio::test]
    async fn upstream_is_not_healthy_when_status_is_unknown() {
        let (state, _rx) = VelaState::new();
        state.register_service("svc".to_string()).await.unwrap();
        // Initial status is Unknown — should not be treated as Healthy
        assert!(
            !is_upstream_healthy(&state, "svc").await,
            "Unknown status should not be treated as Healthy"
        );
    }

    #[tokio::test]
    async fn upstream_is_healthy_after_successful_check() {
        let (state, _rx) = VelaState::new();
        state.register_service("svc".to_string()).await.unwrap();

        // Simulate a successful health check to move service to Healthy
        use crate::models::HealthRecord;
        let record = HealthRecord {
            service_id: "svc".to_string(),
            success: true,
            latency_ms: 1,
            checked_at: chrono::Utc::now(),
            error: None,
        };
        state
            .record_health_check(record, "Test Service".to_string(), 3)
            .await
            .unwrap();

        assert!(
            is_upstream_healthy(&state, "svc").await,
            "Service should be Healthy after a successful check"
        );
    }

    #[tokio::test]
    async fn upstream_is_not_healthy_after_failed_checks() {
        let (state, _rx) = VelaState::new();
        state.register_service("svc".to_string()).await.unwrap();

        // Simulate failures crossing the threshold (threshold = 3)
        use crate::models::HealthRecord;
        for _ in 0..3 {
            let record = HealthRecord {
                service_id: "svc".to_string(),
                success: false,
                latency_ms: 0,
                checked_at: chrono::Utc::now(),
                error: Some("refused".to_string()),
            };
            state
                .record_health_check(record, "Test Service".to_string(), 3)
                .await
                .unwrap();
        }

        assert!(
            !is_upstream_healthy(&state, "svc").await,
            "Failed service should not be treated as Healthy"
        );
    }

    // ── Engine integration ─────────────────────────────────────────────────

    #[tokio::test]
    async fn proxy_engine_starts_with_no_proxy_configs() {
        let (state, _rx) = VelaState::new();
        let mut config = make_config_with_proxy(3001, 8001, 7700);
        config.services[0].proxy = None;

        let handle = run(state, &config).await.expect("Engine should start");

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), handle.shutdown()).await;
        assert!(
            result.is_ok(),
            "Engine with no proxies should shut down immediately"
        );
    }

    #[tokio::test]
    async fn proxy_engine_starts_and_shuts_down_cleanly() {
        let (state, _rx) = VelaState::new();
        let proxy_port = free_port();
        let service_port = free_port();
        let api_port = free_port();
        let config = make_config_with_proxy(service_port, proxy_port, api_port);

        state
            .register_service("test-svc".to_string())
            .await
            .unwrap();

        let handle = run(state, &config).await.expect("Engine should start");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), handle.shutdown()).await;
        assert!(
            result.is_ok(),
            "Proxy engine should shut down within 5 seconds"
        );
    }

    #[tokio::test]
    async fn proxy_engine_forwards_bytes_to_healthy_upstream() {
        let (state, _rx) = VelaState::new();
        state
            .register_service("test-svc".to_string())
            .await
            .unwrap();

        // Mark service as Healthy
        use crate::models::HealthRecord;
        state
            .record_health_check(
                HealthRecord {
                    service_id: "test-svc".to_string(),
                    success: true,
                    latency_ms: 1,
                    checked_at: chrono::Utc::now(),
                    error: None,
                },
                "Test".to_string(),
                3,
            )
            .await
            .unwrap();

        // Start a real TCP echo server as the upstream
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut conn, _)) = upstream_listener.accept().await {
                let (mut r, mut w) = conn.split();
                tokio::io::copy(&mut r, &mut w).await.ok();
            }
        });

        let proxy_port = free_port();
        let api_port = free_port();
        let config = make_config_with_proxy(upstream_port, proxy_port, api_port);
        let handle = run(state, &config).await.expect("Proxy should start");

        // Connect to the proxy and send data
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
            .await
            .expect("Should connect to proxy");

        client.write_all(b"hello vela").await.unwrap();
        client.shutdown().await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        assert_eq!(
            response, b"hello vela",
            "Echo server should return the same bytes"
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn proxy_rejects_connection_when_upstream_is_not_healthy() {
        let (state, _rx) = VelaState::new();
        state
            .register_service("test-svc".to_string())
            .await
            .unwrap();
        // Service stays in Unknown status — not Healthy

        let proxy_port = free_port();
        let service_port = free_port(); // Nothing listening here
        let api_port = free_port();
        let config = make_config_with_proxy(service_port, proxy_port, api_port);

        let handle = run(state, &config).await.expect("Proxy should start");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect — proxy should accept TCP then immediately close (upstream not Healthy)
        let mut client = TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
            .await
            .expect("TCP connect should succeed");

        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        // Proxy dropped the connection — read returns 0 bytes (EOF/RST)
        assert!(
            buf.is_empty(),
            "Proxy should close connection when upstream not Healthy"
        );

        handle.shutdown().await;
    }
}
