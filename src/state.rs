//! Shared in-memory state store for the Vela project.
//!
//! All engines read and write state through this module.
//! The state is protected by an Arc<RwLock<>> so multiple async
//! tasks can read concurrently but writes are exclusive.
//!
//! RULE: Never hold a write lock across an await point.
//! Acquire the lock, perform the write, drop the lock, then await.

use crate::error::VelaError;
use crate::models::{
    HealthRecord, RestartRecord, ServiceState, ServiceStatus, StatusChangeEvent, UpstreamState,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

/// Maximum number of health records kept per service.
/// Older records are dropped when this limit is reached.
const MAX_HEALTH_RECORDS: usize = 100;

/// Maximum number of restart records kept per service.
const MAX_RESTART_RECORDS: usize = 50;

/// The inner data of the state store.
/// Wrapped in Arc<RwLock<>> in VelaState.
#[derive(Debug)]
pub struct StateInner {
    /// Current runtime state for each service, keyed by service_id.
    pub services: HashMap<String, ServiceState>,

    /// Rolling log of health check results, keyed by service_id.
    pub health_records: HashMap<String, Vec<HealthRecord>>,

    /// Rolling log of restart attempts, keyed by service_id.
    pub restart_records: HashMap<String, Vec<RestartRecord>>,

    /// Rolling log of sent alerts, keyed by service_id.
    pub alert_records: HashMap<String, Vec<crate::models::AlertRecord>>,

    /// Live per-upstream health, keyed by service_id. Every service has at
    /// least one entry after config synthesis (config.rs) — even a
    /// single-host service is modeled as one upstream internally.
    pub upstream_states: HashMap<String, Vec<UpstreamState>>,
}

/// The shared state store, safe to clone and send across async tasks.
#[derive(Clone, Debug)]
pub struct VelaState {
    inner: Arc<RwLock<StateInner>>,
    /// Broadcast channel sender for status change events.
    /// The alert engine subscribes to this.
    pub event_tx: broadcast::Sender<StatusChangeEvent>,
}

impl VelaState {
    /// Creates a new empty state store and returns it along with an event receiver.
    pub fn new() -> (Self, broadcast::Receiver<StatusChangeEvent>) {
        let (event_tx, event_rx) = broadcast::channel(256);
        let state = Self {
            inner: Arc::new(RwLock::new(StateInner {
                services: HashMap::new(),
                health_records: HashMap::new(),
                restart_records: HashMap::new(),
                alert_records: HashMap::new(),
                upstream_states: HashMap::new(),
            })),
            event_tx,
        };
        (state, event_rx)
    }

    /// Registers a service in the state store with its initial Unknown state.
    /// Called by the config engine at startup.
    pub async fn register_service(&self, service_id: String) -> Result<(), VelaError> {
        let mut inner = self.inner.write().await;
        inner.services.insert(
            service_id.clone(),
            ServiceState::initial(service_id.clone()),
        );
        inner.health_records.insert(service_id.clone(), Vec::new());
        inner.restart_records.insert(service_id.clone(), Vec::new());
        inner.alert_records.insert(service_id, Vec::new());
        Ok(())
    }

    /// Records a health check result and updates the service's runtime state.
    /// Broadcasts a StatusChangeEvent if the service status changes.
    // Superseded in production by record_upstream_health_check as of Phase 8
    // (the health engine now checks per-upstream, not per-service). Kept —
    // and still exercised heavily by tests across several modules — as the
    // simplest way to simulate a service transition without also standing
    // up upstream_states plumbing.
    #[allow(dead_code)]
    pub async fn record_health_check(
        &self,
        record: HealthRecord,
        service_name: String,
        failure_threshold: u32,
    ) -> Result<(), VelaError> {
        let mut inner = self.inner.write().await;

        let service_state = inner.services.get_mut(&record.service_id).ok_or_else(|| {
            VelaError::StateError(format!(
                "Service '{}' not found in state store",
                record.service_id
            ))
        })?;

        let previous_status = service_state.status.clone();
        service_state.last_checked_at = Some(record.checked_at);

        if record.success {
            service_state.last_ok_at = Some(record.checked_at);
            service_state.consecutive_failures = 0;
            service_state.status = ServiceStatus::Healthy;
        } else {
            service_state.consecutive_failures += 1;
            service_state.status = if service_state.consecutive_failures >= failure_threshold {
                ServiceStatus::Failed
            } else {
                ServiceStatus::Degraded
            };
        }

        let new_status = service_state.status.clone();
        let service_id = record.service_id.clone();

        // Append to rolling health record log, trimming if over limit
        let records = inner
            .health_records
            .entry(record.service_id.clone())
            .or_default();
        records.push(record);
        if records.len() > MAX_HEALTH_RECORDS {
            records.remove(0);
        }

        // Drop write lock before broadcasting (never hold lock across await)
        drop(inner);

        // Broadcast status change event only when status actually changes
        if previous_status != new_status {
            let event = StatusChangeEvent {
                service_id: service_id.clone(),
                service_name,
                previous_status: previous_status.clone(),
                new_status: new_status.clone(),
                timestamp: chrono::Utc::now(),
                message: format!(
                    "Service '{}' transitioned from {:?} to {:?}",
                    service_id, previous_status, new_status
                ),
            };
            // Ignore send errors — no subscribers is not a fatal error
            let _ = self.event_tx.send(event);
        }

        Ok(())
    }

    /// Records a restart attempt made by the recovery engine.
    pub async fn record_restart(&self, record: RestartRecord) -> Result<(), VelaError> {
        let mut inner = self.inner.write().await;

        let service_state = inner.services.get_mut(&record.service_id).ok_or_else(|| {
            VelaError::StateError(format!(
                "Service '{}' not found in state store",
                record.service_id
            ))
        })?;

        service_state.restart_count += 1;

        let records = inner
            .restart_records
            .entry(record.service_id.clone())
            .or_default();
        records.push(record);
        if records.len() > MAX_RESTART_RECORDS {
            records.remove(0);
        }

        Ok(())
    }

    /// Records a sent (or attempted) alert in the shared state store.
    /// Called by the alert engine after each delivery attempt.
    ///
    /// SECURITY: AlertRecord must never contain secret keys or credentials.
    /// The alert engine is responsible for ensuring this before calling.
    pub async fn record_alert(&self, record: crate::models::AlertRecord) -> Result<(), VelaError> {
        const MAX_ALERT_RECORDS: usize = 50;
        let mut inner = self.inner.write().await;

        let records = inner
            .alert_records
            .entry(record.service_id.clone())
            .or_default();

        records.push(record);
        if records.len() > MAX_ALERT_RECORDS {
            records.remove(0);
        }
        Ok(())
    }

    /// Returns recent alert records for a single service.
    /// Called by the API engine.
    pub async fn get_alert_records(&self, service_id: &str) -> Vec<crate::models::AlertRecord> {
        self.inner
            .read()
            .await
            .alert_records
            .get(service_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns a snapshot of all service states (also used by the API engine, Phase 6).
    pub async fn snapshot_services(&self) -> HashMap<String, ServiceState> {
        self.inner.read().await.services.clone()
    }

    /// Returns recent health records for a single service (for the API engine).
    pub async fn get_health_records(&self, service_id: &str) -> Vec<HealthRecord> {
        self.inner
            .read()
            .await
            .health_records
            .get(service_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns recent restart attempt records for a single service.
    /// Called by the API engine to expose restart history.
    pub async fn get_restart_records(&self, service_id: &str) -> Vec<RestartRecord> {
        self.inner
            .read()
            .await
            .restart_records
            .get(service_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Registers the initial (Unknown) state for every upstream of a service.
    /// Called once by the config engine at startup, after synthesis has
    /// guaranteed `upstreams` is non-empty for every service.
    pub async fn register_service_upstreams(
        &self,
        service_id: String,
        upstreams: Vec<UpstreamState>,
    ) {
        let mut inner = self.inner.write().await;
        inner.upstream_states.insert(service_id, upstreams);
    }

    /// Records a single upstream's health check result, updates that
    /// upstream's own state, recalculates the parent service's aggregate
    /// status from all of its upstreams, and updates `ServiceState` —
    /// broadcasting a `StatusChangeEvent` if the aggregate status changed.
    ///
    /// `failure_threshold` and `service_name` are not derivable from the
    /// stored upstream state alone, so the health engine passes them in —
    /// the same information `record_health_check` already requires.
    ///
    /// Aggregate rule (total over every possible mix of upstream statuses):
    /// - all Healthy               → Healthy
    /// - all Failed                → Failed
    /// - any Failed (not all)      → Degraded  (partial capacity)
    /// - any Unknown (no Failed)   → Unknown   (still waiting on first checks)
    /// - otherwise (Healthy/Degraded mix, no Failed, no Unknown) → Degraded
    #[allow(clippy::too_many_arguments)]
    pub async fn record_upstream_health_check(
        &self,
        service_id: &str,
        host: &str,
        port: u16,
        success: bool,
        latency_ms: u64,
        error: Option<String>,
        failure_threshold: u32,
        service_name: String,
    ) -> Result<(), VelaError> {
        let mut inner = self.inner.write().await;
        let now = chrono::Utc::now();

        let aggregate_status = {
            let upstreams = inner.upstream_states.get_mut(service_id).ok_or_else(|| {
                VelaError::StateError(format!(
                    "Service '{}' not found in upstream_states",
                    service_id
                ))
            })?;

            let upstream = upstreams
                .iter_mut()
                .find(|u| u.host == host && u.port == port)
                .ok_or_else(|| {
                    VelaError::StateError(format!(
                        "Upstream {}:{} not registered for service '{}'",
                        host, port, service_id
                    ))
                })?;

            upstream.last_checked_at = Some(now);
            upstream.latency_ms = Some(latency_ms);

            if success {
                upstream.last_ok_at = Some(now);
                upstream.consecutive_failures = 0;
                upstream.status = ServiceStatus::Healthy;
            } else {
                upstream.consecutive_failures += 1;
                upstream.status = if upstream.consecutive_failures >= failure_threshold {
                    ServiceStatus::Failed
                } else {
                    ServiceStatus::Degraded
                };
            }

            let all_healthy = upstreams.iter().all(|u| u.status == ServiceStatus::Healthy);
            let all_failed = upstreams.iter().all(|u| u.status == ServiceStatus::Failed);
            let any_failed = upstreams.iter().any(|u| u.status == ServiceStatus::Failed);
            let any_unknown = upstreams.iter().any(|u| u.status == ServiceStatus::Unknown);

            if all_healthy {
                ServiceStatus::Healthy
            } else if all_failed {
                ServiceStatus::Failed
            } else if any_failed {
                ServiceStatus::Degraded
            } else if any_unknown {
                ServiceStatus::Unknown
            } else {
                ServiceStatus::Degraded
            }
        };

        // Aggregate per-upstream numbers up to the service level so the
        // existing API/dashboard contract (ServiceSummary) stays meaningful
        // without needing to know about individual upstreams.
        let (agg_failures, agg_checked_at, agg_ok_at) = {
            let upstreams = &inner.upstream_states[service_id];
            (
                upstreams
                    .iter()
                    .map(|u| u.consecutive_failures)
                    .max()
                    .unwrap_or(0),
                upstreams.iter().filter_map(|u| u.last_checked_at).max(),
                upstreams.iter().filter_map(|u| u.last_ok_at).max(),
            )
        };

        let service_state = inner.services.get_mut(service_id).ok_or_else(|| {
            VelaError::StateError(format!("Service '{}' not found in state store", service_id))
        })?;

        let previous_status = service_state.status.clone();
        service_state.status = aggregate_status.clone();
        service_state.consecutive_failures = agg_failures;
        service_state.last_checked_at = agg_checked_at;
        service_state.last_ok_at = agg_ok_at;

        // Keep the existing /checks history populated per upstream check.
        let record = HealthRecord {
            service_id: service_id.to_string(),
            success,
            latency_ms,
            checked_at: now,
            error,
        };
        let records = inner
            .health_records
            .entry(service_id.to_string())
            .or_default();
        records.push(record);
        if records.len() > MAX_HEALTH_RECORDS {
            records.remove(0);
        }

        drop(inner);

        if previous_status != aggregate_status {
            let event = StatusChangeEvent {
                service_id: service_id.to_string(),
                service_name,
                previous_status: previous_status.clone(),
                new_status: aggregate_status.clone(),
                timestamp: now,
                message: format!(
                    "Service '{}' transitioned from {:?} to {:?}",
                    service_id, previous_status, aggregate_status
                ),
            };
            let _ = self.event_tx.send(event);
        }

        Ok(())
    }

    /// Returns (host, port) pairs for every upstream of a service that is
    /// currently Healthy. Called by the proxy engine on every new
    /// connection — never cached, always current.
    pub async fn get_healthy_upstreams(&self, service_id: &str) -> Vec<(String, u16)> {
        self.inner
            .read()
            .await
            .upstream_states
            .get(service_id)
            .map(|upstreams| {
                upstreams
                    .iter()
                    .filter(|u| u.status == ServiceStatus::Healthy)
                    .map(|u| (u.host.clone(), u.port))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns a snapshot of every service's upstream states.
    /// Exposed for a future API endpoint (Phase 9) — not yet called in
    /// production code.
    #[allow(dead_code)]
    pub async fn snapshot_upstream_states(&self) -> HashMap<String, Vec<UpstreamState>> {
        self.inner.read().await.upstream_states.clone()
    }
}
