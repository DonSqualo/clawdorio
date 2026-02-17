use axum::{routing::get, routing::post, Json, Router};
use clawdorio_engine::Engine;
use clawdorio_protocol::{targets, Patch, Swap, UiUpdate};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/ui/demo", post(ui_demo))
        .with_state(Arc::new(state))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
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
