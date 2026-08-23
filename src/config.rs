//! Config engine — reads and validates config.toml.
//!
//! This is the first engine that runs at startup.
//! It produces a VelaConfig and registers all services in the state store.

use crate::error::VelaError;
use crate::models::VelaConfig;
use crate::state::VelaState;
use std::path::Path;
use tokio::fs;

/// Loads and validates the configuration file at the given path.
/// Returns a fully validated VelaConfig on success.
pub async fn load_config(path: &Path) -> Result<VelaConfig, VelaError> {
    let contents = fs::read_to_string(path).await.map_err(|e| {
        VelaError::ConfigParse(format!(
            "Cannot read config file at '{}': {}",
            path.display(),
            e
        ))
    })?;

    let config: VelaConfig = toml::from_str(&contents)?;
    validate_config(&config)?;
    Ok(config)
}

/// Validates business rules that TOML deserialization cannot enforce.
fn validate_config(config: &VelaConfig) -> Result<(), VelaError> {
    if config.global.api_key.trim().is_empty() {
        return Err(VelaError::ConfigValidation(
            "global.api_key must not be empty".to_string(),
        ));
    }

    if config.services.is_empty() {
        return Err(VelaError::ConfigValidation(
            "At least one [[services]] block is required".to_string(),
        ));
    }

    let mut seen_ids = std::collections::HashSet::new();
    for svc in &config.services {
        // Validate service ID format
        if !svc.id.chars().all(|c| c.is_alphanumeric() || c == '-') {
            return Err(VelaError::ConfigValidation(format!(
                "Service id '{}' must contain only lowercase letters, numbers, and hyphens",
                svc.id
            )));
        }

        // Validate uniqueness of service IDs
        if !seen_ids.insert(svc.id.clone()) {
            return Err(VelaError::ConfigValidation(format!(
                "Duplicate service id '{}' found in config",
                svc.id
            )));
        }

        // Validate HTTP health checks have a path
        if svc.health_check.kind == crate::models::HealthCheckKind::Http
            && svc.health_check.http_path.is_none()
        {
            return Err(VelaError::ConfigValidation(format!(
                "Service '{}' uses HTTP health check but has no http_path set",
                svc.id
            )));
        }

        // Validate port is in a usable range
        if svc.port == 0 {
            return Err(VelaError::ConfigValidation(format!(
                "Service '{}' has port 0, which is not valid",
                svc.id
            )));
        }
    }

    Ok(())
}

/// Registers all configured services in the shared state store.
/// Must be called after load_config succeeds and before any engine starts.
pub async fn register_services(config: &VelaConfig, state: &VelaState) -> Result<(), VelaError> {
    for svc in &config.services {
        state.register_service(svc.id.clone()).await?;
        tracing::info!(
            "Registered service '{}' ({}:{})",
            svc.id,
            svc.host,
            svc.port
        );
    }
    Ok(())
}
