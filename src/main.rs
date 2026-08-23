//! Vela — single-binary service health orchestrator.
//!
//! Entry point. Boots all engines in order:
//! 1. Config engine  — parses and validates config.toml
//! 2. State store    — initializes shared runtime state
//! 3. Health engine  — starts async health check loops (Phase 2)
//! 4. Recovery engine — starts failure response loop (Phase 3)
//! 5. Alert engine   — starts event subscriber loop (Phase 4)
//! 6. Proxy engine   — starts reverse proxy listener (Phase 5)
//! 7. API engine     — starts REST API server (Phase 6)

use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

mod alert;
mod config;
mod error;
mod health;
mod models;
mod recovery;
mod state;
// Uncomment as each phase is implemented:
// mod proxy;
// mod api;

#[tokio::main]
async fn main() {
    // Initialize structured logging.
    // Set RUST_LOG=debug for verbose output. Default: info.
    // "vela=info" is only a fallback for when RUST_LOG is unset — using
    // add_directive() unconditionally would always win ties against an
    // equally-specific "vela=..." directive from RUST_LOG, silently
    // overriding the user's requested verbosity.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("vela=info")),
        )
        .init();

    tracing::info!("Vela starting up...");

    // Resolve config path from CLI argument or default
    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    // Phase 1: Load and validate configuration
    let vela_config = match config::load_config(&config_path).await {
        Ok(cfg) => {
            tracing::info!(
                "Config loaded: {} service(s) configured",
                cfg.services.len()
            );
            cfg
        }
        Err(e) => {
            tracing::error!("Failed to load config: {}", e);
            std::process::exit(1);
        }
    };

    // Phase 2: Initialize shared state store
    let (state, event_rx) = state::VelaState::new();
    // event_rx is passed directly to the alert engine below.
    // It must NOT be dropped before the alert engine starts.

    // Phase 3: Register all services in state store
    if let Err(e) = config::register_services(&vela_config, &state).await {
        tracing::error!("Failed to register services: {}", e);
        std::process::exit(1);
    }

    tracing::info!(
        "State store initialized. Monitoring {} service(s).",
        vela_config.services.len()
    );

    // Start the health engine — one async task per configured service.
    let health_handle = match health::run(state.clone(), vela_config.services.clone()).await {
        Ok(handle) => {
            tracing::info!("Health engine started successfully.");
            handle
        }
        Err(e) => {
            tracing::error!("Failed to start health engine: {}", e);
            std::process::exit(1);
        }
    };

    // Start the recovery engine — polls for Failed services and restarts them.
    let recovery_handle = match recovery::run(state.clone(), vela_config.services.clone()).await {
        Ok(handle) => {
            tracing::info!("Recovery engine started successfully.");
            handle
        }
        Err(e) => {
            tracing::error!("Failed to start recovery engine: {}", e);
            std::process::exit(1);
        }
    };

    // Start the alert engine — delivers notifications on status transitions.
    let alert_handle = match alert::run(state.clone(), event_rx, vela_config.services.clone()).await
    {
        Ok(handle) => {
            tracing::info!("Alert engine started successfully.");
            handle
        }
        Err(e) => {
            tracing::error!("Failed to start alert engine: {}", e);
            std::process::exit(1);
        }
    };

    // PLACEHOLDER: proxy engine starts here in Phase 5
    // PLACEHOLDER: API engine starts here in Phase 6

    tracing::info!("Vela is running. Press Ctrl+C to stop.");
    tracing::info!("Set RUST_LOG=debug for per-check verbose output.");

    // Wait for shutdown signal
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for ctrl-c");
    tracing::info!("Shutdown signal received. Stopping engines gracefully...");

    // Shutdown in reverse startup order:
    // Alert engine first — no point delivering alerts for engines that are stopping.
    alert_handle.shutdown().await;
    recovery_handle.shutdown().await;
    health_handle.shutdown().await;

    tracing::info!("Vela shut down cleanly. Goodbye.");
}
