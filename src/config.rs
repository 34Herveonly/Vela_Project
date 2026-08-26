//! Config engine — reads and validates config.toml.
//!
//! This is the first engine that runs at startup.
//! It produces a VelaConfig and registers all services in the state store.
//!
//! ## Backward-compatibility synthesis (Phase 8)
//! Pre-Phase-8 configs use the flat `host`/`port`/`command` fields. Every
//! engine downstream of this module (health, recovery, proxy) now works
//! exclusively in terms of `upstreams` and `restart` — never `host`/`port`/
//! `command` directly. This module is where the translation happens, once,
//! at load time, so an existing config.toml with `command = "..."` keeps
//! behaving exactly as it did before Phase 8, with zero changes required.

use crate::error::VelaError;
use crate::models::{RestartConfig, RestartMode, UpstreamConfig, VelaConfig};
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

    let mut config: VelaConfig = toml::from_str(&contents)?;
    synthesize_backward_compat_fields(&mut config)?;
    validate_config(&config)?;
    Ok(config)
}

/// Fills in `upstreams` and `restart` from the legacy `host`/`port`/`command`
/// fields wherever the new fields were left unset. This is the entire
/// backward-compatibility guarantee for Phase 8 — see the module doc comment.
fn synthesize_backward_compat_fields(config: &mut VelaConfig) -> Result<(), VelaError> {
    for svc in &mut config.services {
        // ── upstreams ────────────────────────────────────────────────────
        if svc.upstreams.is_empty() {
            match (&svc.host, svc.port) {
                (Some(host), Some(port)) => {
                    svc.upstreams.push(UpstreamConfig {
                        host: host.clone(),
                        port,
                        docker_container: None,
                    });
                }
                (None, None) => {
                    return Err(VelaError::ConfigValidation(format!(
                        "Service '{}' has no [[services.upstreams]] and no host/port to \
                         synthesize one from — a service needs at least one address to check",
                        svc.id
                    )));
                }
                _ => {
                    return Err(VelaError::ConfigValidation(format!(
                        "Service '{}' sets only one of host/port — both are required together \
                         (or omit both and use [[services.upstreams]] instead)",
                        svc.id
                    )));
                }
            }
        }

        // ── restart ──────────────────────────────────────────────────────
        if svc.restart.is_none() {
            svc.restart = Some(match &svc.command {
                Some(cmd) => RestartConfig {
                    mode: RestartMode::Manual,
                    command: Some(cmd.clone()),
                    docker: None,
                },
                None => RestartConfig {
                    mode: RestartMode::None,
                    command: None,
                    docker: None,
                },
            });
        }

        // RestartConfig is now always Some — validate whichever one we ended
        // up with, whether the user wrote it or we just synthesized it.
        svc.restart.as_ref().unwrap().validate(&svc.id)?;
    }

    Ok(())
}

/// Validates business rules that TOML deserialization cannot enforce.
/// Runs after synthesis, so `svc.upstreams` is always non-empty here.
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

        // Validate every upstream has a usable port. Synthesis guarantees
        // upstreams is non-empty by this point.
        for upstream in &svc.upstreams {
            if upstream.port == 0 {
                return Err(VelaError::ConfigValidation(format!(
                    "Service '{}' has an upstream ({}) with port 0, which is not valid",
                    svc.id, upstream.host
                )));
            }
        }
    }

    Ok(())
}

/// Registers all configured services — and their upstreams — in the shared
/// state store. Must be called after load_config succeeds and before any
/// engine starts.
pub async fn register_services(config: &VelaConfig, state: &VelaState) -> Result<(), VelaError> {
    for svc in &config.services {
        state.register_service(svc.id.clone()).await?;

        let upstream_states: Vec<crate::models::UpstreamState> = svc
            .upstreams
            .iter()
            .map(|u| crate::models::UpstreamState::initial(u.host.clone(), u.port))
            .collect();
        state
            .register_service_upstreams(svc.id.clone(), upstream_states)
            .await;

        let addrs: Vec<String> = svc
            .upstreams
            .iter()
            .map(|u| format!("{}:{}", u.host, u.port))
            .collect();
        tracing::info!(
            "Registered service '{}' ({} upstream(s): {})",
            svc.id,
            svc.upstreams.len(),
            addrs.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AlertTargetConfig, DockerConfig, GlobalConfig, HealthCheckConfig, HealthCheckKind,
        ServiceConfig,
    };

    fn base_service(id: &str) -> ServiceConfig {
        ServiceConfig {
            id: id.to_string(),
            name: format!("Test {}", id),
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
            alerts: vec![],
        }
    }

    fn base_config(services: Vec<ServiceConfig>) -> VelaConfig {
        VelaConfig {
            global: GlobalConfig {
                api_port: 7700,
                api_key: "test-key".to_string(),
                log_dir: "/tmp".to_string(),
            },
            services,
        }
    }

    #[test]
    fn old_command_field_synthesized_to_manual_restart_config() {
        let mut svc = base_service("legacy");
        svc.command = Some("systemctl restart legacy".to_string());
        let mut config = base_config(vec![svc]);

        synthesize_backward_compat_fields(&mut config).unwrap();

        let restart = config.services[0].restart.as_ref().unwrap();
        assert_eq!(restart.mode, RestartMode::Manual);
        assert_eq!(restart.command.as_deref(), Some("systemctl restart legacy"));
    }

    #[test]
    fn service_with_no_command_and_no_restart_synthesizes_mode_none() {
        let svc = base_service("monitor-only");
        let mut config = base_config(vec![svc]);

        synthesize_backward_compat_fields(&mut config).unwrap();

        let restart = config.services[0].restart.as_ref().unwrap();
        assert_eq!(restart.mode, RestartMode::None);
    }

    #[test]
    fn empty_upstreams_synthesized_from_host_port_fields() {
        let mut svc = base_service("single-host");
        svc.host = Some("10.0.0.5".to_string());
        svc.port = Some(8080);
        let mut config = base_config(vec![svc]);

        synthesize_backward_compat_fields(&mut config).unwrap();

        let upstreams = &config.services[0].upstreams;
        assert_eq!(upstreams.len(), 1);
        assert_eq!(upstreams[0].host, "10.0.0.5");
        assert_eq!(upstreams[0].port, 8080);
    }

    #[test]
    fn explicit_upstreams_are_not_overwritten_by_host_port() {
        let mut svc = base_service("explicit");
        svc.host = None;
        svc.port = None;
        svc.upstreams = vec![UpstreamConfig {
            host: "127.0.0.1".to_string(),
            port: 3001,
            docker_container: Some("my-container".to_string()),
        }];
        let mut config = base_config(vec![svc]);

        synthesize_backward_compat_fields(&mut config).unwrap();

        assert_eq!(config.services[0].upstreams.len(), 1);
        assert_eq!(
            config.services[0].upstreams[0].docker_container.as_deref(),
            Some("my-container")
        );
    }

    #[test]
    fn service_with_no_upstreams_and_no_host_port_fails_synthesis() {
        let mut svc = base_service("broken");
        svc.host = None;
        svc.port = None;
        let mut config = base_config(vec![svc]);

        assert!(synthesize_backward_compat_fields(&mut config).is_err());
    }

    #[test]
    fn docker_mode_without_docker_config_fails_validation() {
        let mut svc = base_service("docker-svc");
        svc.restart = Some(RestartConfig {
            mode: RestartMode::Docker,
            command: None,
            docker: None,
        });
        let mut config = base_config(vec![svc]);

        assert!(synthesize_backward_compat_fields(&mut config).is_err());
    }

    #[test]
    fn docker_mode_with_docker_config_passes_validation() {
        let mut svc = base_service("docker-svc");
        svc.restart = Some(RestartConfig {
            mode: RestartMode::Docker,
            command: None,
            docker: Some(DockerConfig {
                container: "my-container".to_string(),
                image: None,
                socket_path: None,
            }),
        });
        let mut config = base_config(vec![svc]);

        assert!(synthesize_backward_compat_fields(&mut config).is_ok());
    }

    #[test]
    fn full_load_and_validate_accepts_legacy_style_config() {
        let mut svc = base_service("legacy-full");
        svc.command = Some("systemctl restart legacy-full".to_string());
        svc.alerts.push(AlertTargetConfig {
            kind: crate::models::AlertKind::Log,
            endpoint: None,
            enabled: true,
        });
        let mut config = base_config(vec![svc]);

        synthesize_backward_compat_fields(&mut config).unwrap();
        assert!(validate_config(&config).is_ok());
    }
}
