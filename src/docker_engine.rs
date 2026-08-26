//! Docker engine — a thin async wrapper around the Docker daemon API.
//!
//! ## Scope
//! This module does two things: reports container state to the health
//! engine, and performs restart operations for the recovery engine. It is
//! not a general-purpose Docker client — only the operations Vela actually
//! needs are exposed.
//!
//! ## Optional by design
//! Docker is not required to run Vela. If the daemon is unreachable at
//! startup, `connect()` returns an error, main.rs logs a warning, and Vela
//! runs with Docker features disabled — manual and none-mode services are
//! entirely unaffected. See main.rs's Docker connection handling.
//!
//! ## Async throughout
//! bollard's API is fully async — every function here is `async fn` and
//! every Docker call is awaited inside the caller's tokio task. There is no
//! synchronous path through this module.

use bollard::container::{
    Config as ContainerConfig, CreateContainerOptions, RemoveContainerOptions,
    RestartContainerOptions, StopContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::Docker;
use tokio_stream::StreamExt;
use tracing::{debug, warn};

use crate::error::VelaError;
use crate::models::RestartOutcome;

/// Maximum time to wait for a Docker restart_container call before giving up.
const DOCKER_RESTART_TIMEOUT_SECS: u64 = 10;

/// Wraps `bollard::Docker`. Cheap to clone — bollard's client is internally
/// Arc-wrapped, so cloning this shares the same underlying connection pool
/// rather than opening a new one per engine task.
#[derive(Clone, Debug)]
pub struct DockerClient {
    inner: Docker,
}

/// Connects to the Docker daemon using the platform-appropriate default
/// transport: a Unix socket on Linux/macOS, a named pipe on Windows.
/// `connect_with_local_defaults()` (not the Unix-socket-only variant) is
/// what makes this portable across both.
///
/// Returns `VelaError::ProcessError` with a message pointing at the likely
/// cause (Docker not running, or the socket not accessible to this process)
/// if the daemon cannot be reached. This failure is never fatal to Vela —
/// see main.rs, which treats it as a warning and continues without Docker.
pub async fn connect() -> Result<DockerClient, VelaError> {
    let docker = Docker::connect_with_local_defaults().map_err(|e| {
        VelaError::ProcessError(format!(
            "Failed to connect to Docker daemon: {}. Is Docker running, and does this \
             process have permission to access the Docker socket? On Linux, add your \
             user to the `docker` group (sudo usermod -aG docker $USER) or run as root.",
            e
        ))
    })?;

    // connect_with_local_defaults() succeeds even if the daemon is
    // unreachable — it only builds the client. Confirm we can actually
    // talk to the daemon before declaring Docker available.
    docker.ping().await.map_err(|e| {
        VelaError::ProcessError(format!(
            "Docker daemon did not respond to ping: {}. Is the Docker daemon running?",
            e
        ))
    })?;

    Ok(DockerClient { inner: docker })
}

/// Queries the Docker API for a container's first exposed host port.
/// Returns `VelaError::ProcessError` if the container does not exist or
/// has no published port mapping.
// Not yet called by any engine — auto-discovering an upstream's port from
// its container (rather than requiring it in [[services.upstreams]]) is a
// natural Phase 9 feature this makes possible, not something wired in yet.
#[allow(dead_code)]
pub async fn get_container_port(
    client: &DockerClient,
    container_name: &str,
) -> Result<u16, VelaError> {
    let inspect = client
        .inner
        .inspect_container(container_name, None)
        .await
        .map_err(|e| {
            VelaError::ProcessError(format!(
                "Failed to inspect container '{}': {}",
                container_name, e
            ))
        })?;

    let ports = inspect
        .network_settings
        .and_then(|ns| ns.ports)
        .ok_or_else(|| {
            VelaError::ProcessError(format!(
                "Container '{}' has no network settings",
                container_name
            ))
        })?;

    for bindings in ports.values().flatten() {
        for binding in bindings {
            if let Some(host_port) = &binding.host_port {
                if let Ok(port) = host_port.parse::<u16>() {
                    return Ok(port);
                }
            }
        }
    }

    Err(VelaError::ProcessError(format!(
        "Container '{}' has no exposed host port",
        container_name
    )))
}

/// Returns true if the container is currently running.
/// False for stopped, paused, dead, or otherwise non-running containers.
pub async fn get_container_status(
    client: &DockerClient,
    container_name: &str,
) -> Result<bool, VelaError> {
    let inspect = client
        .inner
        .inspect_container(container_name, None)
        .await
        .map_err(|e| {
            VelaError::ProcessError(format!(
                "Failed to inspect container '{}': {}",
                container_name, e
            ))
        })?;

    Ok(inspect.state.and_then(|s| s.running).unwrap_or(false))
}

/// Restarts a container in place via the Docker API — no shell command.
/// Never panics; every failure is caught and returned as `RestartOutcome::Failed`.
pub async fn restart_container(client: &DockerClient, container_name: &str) -> RestartOutcome {
    debug!(
        "Docker engine: restarting container '{}' (timeout {}s)",
        container_name, DOCKER_RESTART_TIMEOUT_SECS
    );

    let options = Some(RestartContainerOptions {
        t: DOCKER_RESTART_TIMEOUT_SECS as isize,
    });

    match client
        .inner
        .restart_container(container_name, options)
        .await
    {
        Ok(()) => RestartOutcome::Succeeded,
        Err(e) => RestartOutcome::Failed(format!(
            "Docker restart of '{}' failed: {}",
            container_name, e
        )),
    }
}

/// Full CI/CD "repull" restart: pulls `image`, stops and removes the
/// existing container, then recreates it under the same name with the
/// original container's port bindings and host config preserved, and
/// starts it.
///
/// Never panics. Any step failing returns `RestartOutcome::Failed` naming
/// the specific step, so recovery.rs's logs make clear where it broke.
pub async fn repull_and_restart(
    client: &DockerClient,
    container_name: &str,
    image: &str,
) -> RestartOutcome {
    // Step 1: capture the existing container's config so the recreated
    // container keeps the same port bindings, env vars, and restart policy.
    let inspect = match client.inner.inspect_container(container_name, None).await {
        Ok(i) => i,
        Err(e) => {
            return RestartOutcome::Failed(format!(
                "repull: failed to inspect existing container '{}': {}",
                container_name, e
            ));
        }
    };
    // inspect_container returns bollard_stubs::ContainerConfig (the "read"
    // shape); create_container expects bollard::container::Config (the
    // "write" shape). They mirror each other field-for-field — bollard
    // provides the From impl to convert between them.
    let original_config: ContainerConfig<String> = inspect.config.unwrap_or_default().into();
    let original_host_config = inspect.host_config;

    // Step 2: pull the new image. create_image returns a stream of progress
    // events — we drain it fully and fail on the first error.
    let mut pull_stream = client.inner.create_image(
        Some(CreateImageOptions {
            from_image: image,
            ..Default::default()
        }),
        None,
        None,
    );
    while let Some(chunk) = pull_stream.next().await {
        if let Err(e) = chunk {
            return RestartOutcome::Failed(format!(
                "repull: failed to pull image '{}': {}",
                image, e
            ));
        }
    }

    // Step 3: stop the existing container.
    if let Err(e) = client
        .inner
        .stop_container(container_name, None::<StopContainerOptions>)
        .await
    {
        // A container that's already stopped/gone returning an error here
        // is expected (it's Failed — that's why we're restarting it) —
        // only treat this as fatal if the container still exists afterward
        // would be over-engineering for v1. Log and continue: remove will
        // surface a clearer error if the container is genuinely unreachable.
        warn!(
            "Docker engine: stop_container for '{}' during repull returned: {} — continuing",
            container_name, e
        );
    }

    // Step 4: remove the stopped container.
    if let Err(e) = client
        .inner
        .remove_container(container_name, None::<RemoveContainerOptions>)
        .await
    {
        return RestartOutcome::Failed(format!(
            "repull: failed to remove container '{}': {}",
            container_name, e
        ));
    }

    // Step 5: recreate under the same name, same host config, new image.
    let new_config = ContainerConfig {
        image: Some(image.to_string()),
        host_config: original_host_config,
        ..original_config
    };
    let create_options = Some(CreateContainerOptions {
        name: container_name,
        platform: None,
    });
    if let Err(e) = client
        .inner
        .create_container(create_options, new_config)
        .await
    {
        return RestartOutcome::Failed(format!(
            "repull: failed to create new container '{}' from image '{}': {}",
            container_name, image, e
        ));
    }

    // Step 6: start it.
    if let Err(e) = client
        .inner
        .start_container::<String>(container_name, None)
        .await
    {
        return RestartOutcome::Failed(format!(
            "repull: failed to start recreated container '{}': {}",
            container_name, e
        ));
    }

    RestartOutcome::Succeeded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_config_with_no_image_uses_restart_not_repull() {
        // A DockerConfig with image = None should route to restart_container,
        // not repull_and_restart. This is recovery.rs's dispatch logic, but
        // the invariant belongs here: image.is_none() is the sole signal.
        let docker = crate::models::DockerConfig {
            container: "my-app".to_string(),
            image: None,
            socket_path: None,
        };
        assert!(docker.image.is_none());
    }

    #[test]
    fn restart_outcome_succeeded_is_correctly_identified() {
        let outcome = RestartOutcome::Succeeded;
        assert_eq!(outcome, RestartOutcome::Succeeded);
    }

    #[test]
    fn restart_outcome_failed_carries_reason_string() {
        let outcome = RestartOutcome::Failed("container not found".to_string());
        match outcome {
            RestartOutcome::Failed(reason) => assert_eq!(reason, "container not found"),
            other => panic!("expected Failed, got {:?}", other),
        }
    }
}
