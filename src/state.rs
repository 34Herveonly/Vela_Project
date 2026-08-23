//! Shared in-memory state store for the Vela project.
//!
//! All engines read and write state through this module.
//! The state is protected by an Arc<RwLock<>> so multiple async
//! tasks can read concurrently but writes are exclusive.
//!
//! RULE: Never hold a write lock across an await point.
//! Acquire the lock, perform the write, drop the lock, then await.

use crate::error::VelaError;
use crate::models::{HealthRecord, RestartRecord, ServiceState, ServiceStatus, StatusChangeEvent};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

/// Maximum number of health records kept per service.
/// Older records are dropped when this limit is reached.
// Only read once the health engine (Phase 2) calls record_health_check.
#[allow(dead_code)]
const MAX_HEALTH_RECORDS: usize = 100;

/// Maximum number of restart records kept per service.
// Only read once the recovery engine (Phase 3) calls record_restart.
#[allow(dead_code)]
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
}

/// The shared state store, safe to clone and send across async tasks.
#[derive(Clone, Debug)]
pub struct VelaState {
    inner: Arc<RwLock<StateInner>>,
    /// Broadcast channel sender for status change events.
    /// The alert engine subscribes to this.
    // Only read once the alert engine (Phase 4) subscribes.
    #[allow(dead_code)]
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
        inner.restart_records.insert(service_id, Vec::new());
        Ok(())
    }

    /// Records a health check result and updates the service's runtime state.
    /// Broadcasts a StatusChangeEvent if the service status changes.
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
    // Called starting with the recovery engine (Phase 3).
    #[allow(dead_code)]
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

    /// Returns a snapshot of all service states (for the API engine).
    // Called starting with the API engine (Phase 6).
    #[allow(dead_code)]
    pub async fn snapshot_services(&self) -> HashMap<String, ServiceState> {
        self.inner.read().await.services.clone()
    }

    /// Returns recent health records for a single service (for the API engine).
    // Only exercised by tests until the API engine (Phase 6) calls it.
    #[allow(dead_code)]
    pub async fn get_health_records(&self, service_id: &str) -> Vec<HealthRecord> {
        self.inner
            .read()
            .await
            .health_records
            .get(service_id)
            .cloned()
            .unwrap_or_default()
    }
}
