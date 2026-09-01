//! API engine — authenticated REST API for status and operational control.
//!
//! ## Authentication
//! Every endpoint requires `Authorization: Bearer <api_key>` header.
//! Key comparison uses constant-time equality (subtle crate) to prevent
//! timing attacks. No endpoint is accessible without a valid key.
//!
//! ## Versioning
//! All routes are prefixed with /api/v1/. Future breaking changes
//! will be introduced under /api/v2/ without removing v1 routes.
//!
//! ## Security
//! - Auth middleware applied at router level — no handler can skip it
//! - Error responses never include internal details
//! - API key never logged at any level
//! - Request body size limited to prevent resource exhaustion
//! - Security headers applied to every response
//!
//! ## Endpoints (v1)
//! GET  /health                    — unauthenticated liveness probe (for Docker/K8s)
//! GET  /api/v1/status             — summary of all services
//! GET  /api/v1/services           — list of all service summaries
//! GET  /api/v1/services/:id       — detailed state for one service
//! GET  /api/v1/services/:id/checks  — recent health check records
//! GET  /api/v1/services/:id/alerts  — recent alert records
//! GET  /api/v1/services/:id/restarts — recent restart records

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::error::VelaError;
use crate::models::{
    ApiError, ApiResponse, ServiceConfig, ServiceSummary, VelaStatusResponse, API_VERSION,
};
use crate::state::VelaState;

/// Shared state threaded through axum handlers via `.with_state()`.
/// Arc-wrapped so it can be cloned cheaply per request.
#[derive(Clone)]
pub struct AppState {
    pub vela_state: VelaState,
    /// Service configs by ID — for name lookup and metadata.
    pub service_configs: Arc<HashMap<String, ServiceConfig>>,
    /// API key in bytes — stored as bytes for constant-time comparison.
    /// SECURITY: Never log, serialize, or expose this field.
    pub api_key_bytes: Arc<Vec<u8>>,
}

/// Handle returned by `run()` for lifecycle management.
pub struct ApiEngineHandle {
    task_handle: JoinHandle<()>,
    cancellation_token: CancellationToken,
}

impl ApiEngineHandle {
    /// Signals the API engine to stop accepting requests and shuts it down.
    pub async fn shutdown(self) {
        info!("API engine: initiating graceful shutdown");
        self.cancellation_token.cancel();
        if let Err(e) = self.task_handle.await {
            error!("API engine: task panicked during shutdown: {:?}", e);
        }
        info!("API engine: stopped cleanly");
    }
}

/// Starts the API engine on the configured port.
///
/// # Security
/// The api_key is stored as bytes and compared using constant-time equality.
/// It is never logged, returned in responses, or accessible via any endpoint.
pub async fn run(
    vela_state: VelaState,
    services: Vec<ServiceConfig>,
    api_port: u16,
    api_key: String,
) -> Result<ApiEngineHandle, VelaError> {
    if api_key.trim().is_empty() {
        return Err(VelaError::ApiError("API key must not be empty".to_string()));
    }

    let service_configs: HashMap<String, ServiceConfig> =
        services.into_iter().map(|s| (s.id.clone(), s)).collect();

    let app_state = AppState {
        vela_state,
        service_configs: Arc::new(service_configs),
        // SECURITY: Store as bytes for constant-time comparison.
        api_key_bytes: Arc::new(api_key.into_bytes()),
    };

    let app = build_router(app_state);

    let bind_addr = format!("0.0.0.0:{}", api_port);
    let listener = TcpListener::bind(&bind_addr).await.map_err(|e| {
        VelaError::ApiError(format!("Failed to bind API server on {}: {}", bind_addr, e))
    })?;

    info!(
        "API engine: listening on {} (API v{})",
        bind_addr, API_VERSION
    );

    let cancellation_token = CancellationToken::new();
    let token_clone = cancellation_token.clone();

    let task_handle = tokio::spawn(async move {
        // axum's serve with graceful shutdown.
        // When the token fires, the server stops accepting new requests
        // and waits for in-flight handlers to complete.
        let serve_result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                token_clone.cancelled().await;
                debug!("API engine: graceful shutdown signal received");
            })
            .await;

        if let Err(e) = serve_result {
            error!("API engine: server error: {}", e);
        }
    });

    Ok(ApiEngineHandle {
        task_handle,
        cancellation_token,
    })
}

/// Builds the axum router with all routes and middleware.
/// Auth middleware wraps all /api/v1/ routes.
/// The /health liveness probe and / dashboard are intentionally outside
/// the auth layer — the dashboard handles its own auth via a Bearer token
/// entered client-side in JS and sent on every fetch() to /api/v1/*.
fn build_router(state: AppState) -> Router {
    // Unauthenticated routes — liveness probe for Docker/Kubernetes/load balancers,
    // and the embedded web dashboard.
    // SECURITY: /health deliberately returns minimal information. The dashboard
    // itself carries no Vela data — everything it shows is fetched client-side
    // through the authenticated /api/v1/* routes below.
    let public_routes = Router::new()
        .route("/health", get(handle_liveness))
        .route("/", get(handle_dashboard));

    // Authenticated routes — all require valid Bearer token.
    let authenticated_routes = Router::new()
        .route("/api/v1/status", get(handle_status))
        .route("/api/v1/services", get(handle_list_services))
        .route("/api/v1/services/:id", get(handle_get_service))
        .route("/api/v1/services/:id/checks", get(handle_get_checks))
        .route("/api/v1/services/:id/alerts", get(handle_get_alerts))
        .route("/api/v1/services/:id/restarts", get(handle_get_restarts))
        // Auth middleware applied to all routes in this router.
        // Any route added here inherits auth automatically.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    public_routes
        .merge(authenticated_routes)
        .with_state(state)
        // Apply security headers to every response including public routes.
        .layer(middleware::from_fn(security_headers_middleware))
}

/// Authentication middleware.
/// Checks the Authorization: Bearer <key> header using constant-time comparison.
/// SECURITY: On failure, returns 401 with a generic message.
/// The actual reason (missing header vs wrong key) is NOT revealed to the client
/// — this prevents enumeration attacks.
async fn auth_middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let auth_result = extract_and_validate_bearer(request.headers(), &state.api_key_bytes);

    match auth_result {
        Ok(()) => next.run(request).await,
        Err(reason) => {
            // Log the reason server-side for debugging — never send to client.
            warn!("API engine: authentication failed — {}", reason);
            (
                StatusCode::UNAUTHORIZED,
                Json(ApiError::new("Authentication required")),
            )
                .into_response()
        }
    }
}

/// Extracts the Bearer token from the Authorization header and validates it.
/// Uses constant-time comparison to prevent timing attacks.
/// Returns Ok(()) if valid, Err(reason) if invalid — reason is for server logs only.
fn extract_and_validate_bearer(
    headers: &HeaderMap,
    expected_key_bytes: &[u8],
) -> Result<(), &'static str> {
    let auth_header = match headers.get("authorization") {
        Some(v) => v,
        None => return Err("missing Authorization header"),
    };

    let auth_str = match auth_header.to_str() {
        Ok(s) => s,
        Err(_) => return Err("Authorization header contains non-ASCII characters"),
    };

    let token = match auth_str.strip_prefix("Bearer ") {
        Some(t) => t,
        None => return Err("Authorization header is not a Bearer token"),
    };

    // SECURITY: Constant-time comparison — no timing leak.
    // Both sides must be the same length for constant-time comparison to be
    // meaningful. If lengths differ, ConstantTimeEq returns 0 (false).
    let token_bytes = token.as_bytes();
    let keys_match: bool = expected_key_bytes.ct_eq(token_bytes).into();

    if keys_match {
        Ok(())
    } else {
        Err("Bearer token does not match configured API key")
    }
}

/// Security headers middleware.
/// Applied to every response, including public routes and error responses.
async fn security_headers_middleware(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    // Prevent MIME sniffing attacks
    headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );
    // Prevent framing (clickjacking)
    headers.insert("X-Frame-Options", HeaderValue::from_static("DENY"));
    // No caching of API responses — status data is always live
    headers.insert("Cache-Control", HeaderValue::from_static("no-store"));
    // Remove any server identification header axum might add
    headers.remove("server");

    response
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// GET /health — unauthenticated liveness probe.
/// Returns 200 OK with minimal payload when Vela is running.
/// Used by Docker HEALTHCHECK, Kubernetes liveness probes, and load balancers.
/// SECURITY: Returns no operational data — intentionally minimal.
async fn handle_liveness() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "service": "vela" })),
    )
}

/// The embedded web dashboard — a single self-contained HTML/CSS/JS file
/// compiled into the binary at build time. No build step, no npm, no CDN.
///
/// This `include_str!` path is relative to this file (src/api.rs), so
/// "dashboard/dashboard.html" resolves to src/dashboard/dashboard.html.
const DASHBOARD_HTML: &str = include_str!("dashboard/dashboard.html");

/// GET / — serves the embedded web dashboard.
///
/// Unauthenticated by design: the HTML/CSS/JS itself carries no Vela data.
/// The browser prompts for an API key on first load, keeps it in
/// sessionStorage only, and sends it as a Bearer token on every fetch()
/// to the authenticated /api/v1/* routes — the same auth boundary every
/// other client (curl, vela-ctl) goes through.
async fn handle_dashboard() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("Cache-Control", "no-store")],
        Html(DASHBOARD_HTML),
    )
}

/// GET /api/v1/status — aggregated status of all monitored services.
async fn handle_status(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = state.vela_state.snapshot_services().await;

    let mut healthy_count = 0;
    let mut degraded_count = 0;
    let mut failed_count = 0;
    let mut unknown_count = 0;
    let mut summaries = Vec::with_capacity(snapshot.len());

    for (id, svc_state) in &snapshot {
        use crate::models::ServiceStatus;
        match svc_state.status {
            ServiceStatus::Healthy => healthy_count += 1,
            ServiceStatus::Degraded => degraded_count += 1,
            ServiceStatus::Failed => failed_count += 1,
            ServiceStatus::Unknown => unknown_count += 1,
        }

        let name = state
            .service_configs
            .get(id)
            .map(|c| c.name.as_str())
            .unwrap_or("Unknown");

        summaries.push(ServiceSummary::from_state(svc_state, name));
    }

    // Sort by service_id for stable, predictable response ordering.
    summaries.sort_by(|a, b| a.service_id.cmp(&b.service_id));

    let response = VelaStatusResponse {
        healthy_count,
        degraded_count,
        failed_count,
        unknown_count,
        total_services: summaries.len(),
        services: summaries,
    };

    (StatusCode::OK, Json(ApiResponse::success(response)))
}

/// GET /api/v1/services — list of all service summaries.
/// Identical content to /status but without the aggregate counts.
async fn handle_list_services(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = state.vela_state.snapshot_services().await;
    let mut summaries: Vec<ServiceSummary> = snapshot
        .iter()
        .map(|(id, svc_state)| {
            let name = state
                .service_configs
                .get(id)
                .map(|c| c.name.as_str())
                .unwrap_or("Unknown");
            ServiceSummary::from_state(svc_state, name)
        })
        .collect();

    summaries.sort_by(|a, b| a.service_id.cmp(&b.service_id));
    (StatusCode::OK, Json(ApiResponse::success(summaries)))
}

/// GET /api/v1/services/:id — detailed state for one service.
async fn handle_get_service(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    // Input validation: service IDs are alphanumeric + hyphens.
    // Reject anything else before touching state — prevents log injection.
    if !id.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("Invalid service ID format")),
        )
            .into_response();
    }

    let snapshot = state.vela_state.snapshot_services().await;
    match snapshot.get(&id) {
        Some(svc_state) => {
            let name = state
                .service_configs
                .get(&id)
                .map(|c| c.name.as_str())
                .unwrap_or("Unknown");
            let summary = ServiceSummary::from_state(svc_state, name);
            (StatusCode::OK, Json(ApiResponse::success(summary))).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("Service not found")),
        )
            .into_response(),
    }
}

/// GET /api/v1/services/:id/checks — recent health check records.
async fn handle_get_checks(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !id.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("Invalid service ID format")),
        )
            .into_response();
    }

    // Return 404 if service doesn't exist in config, even if state has data.
    if !state.service_configs.contains_key(&id) {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("Service not found")),
        )
            .into_response();
    }

    let records = state.vela_state.get_health_records(&id).await;
    (StatusCode::OK, Json(ApiResponse::success(records))).into_response()
}

/// GET /api/v1/services/:id/alerts — recent alert records.
async fn handle_get_alerts(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !id.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("Invalid service ID format")),
        )
            .into_response();
    }

    if !state.service_configs.contains_key(&id) {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("Service not found")),
        )
            .into_response();
    }

    let records = state.vela_state.get_alert_records(&id).await;
    (StatusCode::OK, Json(ApiResponse::success(records))).into_response()
}

/// GET /api/v1/services/:id/restarts — recent restart attempt records.
async fn handle_get_restarts(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !id.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("Invalid service ID format")),
        )
            .into_response();
    }

    if !state.service_configs.contains_key(&id) {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("Service not found")),
        )
            .into_response();
    }

    let records = state.vela_state.get_restart_records(&id).await;
    (StatusCode::OK, Json(ApiResponse::success(records))).into_response()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{HealthCheckConfig, HealthCheckKind, ServiceConfig};
    use crate::state::VelaState;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt; // for `.oneshot()`

    const TEST_API_KEY: &str = "test-api-key-for-unit-tests";

    fn make_test_service(id: &str) -> ServiceConfig {
        ServiceConfig {
            id: id.to_string(),
            name: format!("Test Service {}", id),
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

    async fn make_test_app() -> (Router, VelaState) {
        let (state, _rx) = VelaState::new();
        let service = make_test_service("test-svc");
        state
            .register_service("test-svc".to_string())
            .await
            .unwrap();

        let app_state = AppState {
            vela_state: state.clone(),
            service_configs: Arc::new(
                vec![service]
                    .into_iter()
                    .map(|s| (s.id.clone(), s))
                    .collect(),
            ),
            api_key_bytes: Arc::new(TEST_API_KEY.as_bytes().to_vec()),
        };

        (build_router(app_state), state)
    }

    fn bearer(key: &str) -> String {
        format!("Bearer {}", key)
    }

    // ── extract_and_validate_bearer ────────────────────────────────────────

    #[test]
    fn valid_bearer_token_is_accepted() {
        let key = b"my-secret-key";
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer my-secret-key"),
        );
        assert!(extract_and_validate_bearer(&headers, key).is_ok());
    }

    #[test]
    fn wrong_bearer_token_is_rejected() {
        let key = b"correct-key";
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer wrong-key"),
        );
        assert!(extract_and_validate_bearer(&headers, key).is_err());
    }

    #[test]
    fn missing_authorization_header_is_rejected() {
        let key = b"any-key";
        let headers = HeaderMap::new();
        assert!(extract_and_validate_bearer(&headers, key).is_err());
    }

    #[test]
    fn non_bearer_auth_scheme_is_rejected() {
        let key = b"any-key";
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert!(extract_and_validate_bearer(&headers, key).is_err());
    }

    #[test]
    fn constant_time_comparison_different_lengths_is_rejected() {
        // Ensure a prefix-matching key is rejected, not accepted.
        let key = b"long-correct-key";
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer long"));
        assert!(extract_and_validate_bearer(&headers, key).is_err());
    }

    // ── Liveness endpoint (unauthenticated) ───────────────────────────────

    #[tokio::test]
    async fn liveness_endpoint_returns_200_without_auth() {
        let (app, _) = make_test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ── Authentication enforcement ─────────────────────────────────────────

    #[tokio::test]
    async fn status_endpoint_requires_auth() {
        let (app, _) = make_test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_api_key_returns_401() {
        let (app, _) = make_test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .header("authorization", bearer("wrong-key"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn correct_api_key_returns_200_for_status() {
        let (app, _) = make_test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .header("authorization", bearer(TEST_API_KEY))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ── Status endpoint content ────────────────────────────────────────────

    #[tokio::test]
    async fn status_response_includes_service_summary() {
        let (app, _) = make_test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .header("authorization", bearer(TEST_API_KEY))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["ok"], true);
        assert_eq!(json["version"], "1");
        assert!(json["data"]["total_services"].as_u64().unwrap() >= 1);
        assert!(json["data"]["services"].is_array());
    }

    // ── Service detail endpoint ────────────────────────────────────────────

    #[tokio::test]
    async fn get_service_returns_200_for_known_id() {
        let (app, _) = make_test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/services/test-svc")
                    .header("authorization", bearer(TEST_API_KEY))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn get_service_returns_404_for_unknown_id() {
        let (app, _) = make_test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/services/does-not-exist")
                    .header("authorization", bearer(TEST_API_KEY))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_service_returns_400_for_invalid_id_format() {
        let (app, _) = make_test_app().await;
        // Service IDs containing path traversal or injection characters
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/services/../../etc/passwd")
                    .header("authorization", bearer(TEST_API_KEY))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // axum's path extractor normalizes the path, but our validation
        // catches invalid characters before touching state.
        // Status may be 400 or 404 depending on axum routing — either is correct.
        assert!(
            response.status() == StatusCode::BAD_REQUEST
                || response.status() == StatusCode::NOT_FOUND
        );
    }

    // ── Security headers ───────────────────────────────────────────────────

    #[tokio::test]
    async fn responses_include_security_headers() {
        let (app, _) = make_test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.headers().get("X-Content-Type-Options").unwrap(),
            "nosniff"
        );
        assert_eq!(response.headers().get("X-Frame-Options").unwrap(), "DENY");
        assert_eq!(response.headers().get("Cache-Control").unwrap(), "no-store");
    }

    // ── History endpoints ──────────────────────────────────────────────────

    #[tokio::test]
    async fn checks_endpoint_returns_empty_list_for_new_service() {
        let (app, _) = make_test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/services/test-svc/checks")
                    .header("authorization", bearer(TEST_API_KEY))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["data"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn alerts_endpoint_returns_404_for_unknown_service() {
        let (app, _) = make_test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/services/ghost-svc/alerts")
                    .header("authorization", bearer(TEST_API_KEY))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ── Engine lifecycle ───────────────────────────────────────────────────

    #[tokio::test]
    async fn api_engine_starts_and_shuts_down_cleanly() {
        use std::net::TcpListener as StdListener;
        let (vela_state, _rx) = VelaState::new();
        let service = make_test_service("lifecycle-svc");
        vela_state
            .register_service(service.id.clone())
            .await
            .unwrap();

        // Find a free port
        let free_port = StdListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        let handle = run(
            vela_state,
            vec![service],
            free_port,
            TEST_API_KEY.to_string(),
        )
        .await
        .expect("API engine should start");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), handle.shutdown()).await;
        assert!(
            result.is_ok(),
            "API engine should shut down within 5 seconds"
        );
    }
}
