use axum::{routing::get, routing::post, Json, Router};
use clawdorio_engine::Engine;
use clawdorio_protocol::{targets, Patch, Swap, UiUpdate};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/ui/demo", post(ui_demo))
        .with_state(Arc::new(state))
        // This service is expected to be local-only and may control a local agent swarm.
        // Never use `Access-Control-Allow-Origin: *` here; it makes it easier for a random
        // website in your browser to probe/exfiltrate local state.
        .layer(local_only_cors())
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize)]
struct DemoInput {
    #[serde(default)]
    pub selected: Option<String>,
}

async fn ui_demo(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(input): Json<DemoInput>,
) -> Json<UiUpdate> {
    // Touch engine so the process fails fast if sqlite is unavailable.
    let _ = state.engine.open();

    let selected = input.selected.unwrap_or_else(|| "none".to_string());
    let patches = vec![
        Patch {
            target: targets::PANEL_BOTTOM_BAR.to_string(),
            swap: Swap::Replace,
            html: Some(format!(
                "<div class=\"card\"><strong>Bottom Bar</strong><div>Selected: {}</div></div>",
                html_escape::encode_text(&selected)
            )),
            payload: None,
            trigger: None,
        },
        Patch {
            target: targets::PANEL_RIGHT.to_string(),
            swap: Swap::Replace,
            html: Some("<div class=\"card\"><strong>Right Panel</strong><div>Demo patch ok.</div></div>".to_string()),
            payload: None,
            trigger: None,
        },
    ];
    Json(UiUpdate::new("ui.demo", patches))
}

pub async fn serve(addr: SocketAddr, db_path: PathBuf) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(listener, db_path, async { std::future::pending::<()>().await }).await?;
    Ok(())
}

pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    db_path: PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<SocketAddr> {
    let state = AppState {
        engine: Engine::new(db_path),
    };
    let app = build_router(state);
    let addr = listener.local_addr()?;
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(addr)
}

fn local_only_cors() -> CorsLayer {
    use axum::http::header;
    use axum::http::HeaderValue;
    use axum::http::Method;

    CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE])
        .allow_origin(AllowOrigin::predicate(
            |origin: &HeaderValue, _req| is_allowed_local_origin(origin),
        ))
}

fn is_allowed_local_origin(origin: &axum::http::HeaderValue) -> bool {
    let Ok(s) = origin.to_str() else {
        return false;
    };

    // Tauri WebView origin (production).
    if s == "tauri://localhost" {
        return true;
    }

    // Dev server and local reverse proxies.
    is_http_origin_for_host(s, "localhost") || is_http_origin_for_host(s, "127.0.0.1")
}

fn is_http_origin_for_host(origin: &str, host: &str) -> bool {
    for scheme in ["http://", "https://"] {
        if let Some(rest) = origin.strip_prefix(scheme) {
            if let Some(after) = rest.strip_prefix(host) {
                // Origin is just scheme://host[:port]
                return after.is_empty() || after.starts_with(':');
            }
        }
    }
    false
}
