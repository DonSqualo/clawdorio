use axum::http::header;
use axum::http::HeaderValue;
use axum::{
    middleware,
    response::{Html, IntoResponse},
    routing::delete,
    routing::get,
    routing::post,
    Json, Router,
};
use clawdorio_engine::{Engine, Entity, Quest};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
}

pub fn build_router(state: AppState) -> Router {
    let sprites_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("rts-sprites");
    let sprites = ServiceBuilder::new()
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        ))
        .service(ServeDir::new(sprites_dir));

    Router::new()
        .route("/", get(dashboard))
        .route("/health", get(health))
        .route("/api/state", get(api_state))
        .route("/api/buildings", get(api_buildings))
        .route(
            "/api/entities",
            get(api_entities_list).post(api_entities_create),
        )
        .route("/api/entities/{id}", delete(api_entities_delete))
        .route("/api/quests", get(api_quests_list).post(api_quests_upsert))
        .route("/api/quests/{id}", delete(api_quests_delete))
        .route("/api/runs", get(api_runs_list))
        .route("/api/feature/build", post(api_feature_build))
        .nest_service("/rts-sprites", sprites)
        .with_state(Arc::new(state))
        // Local security: allow only loopback + Tailscale by default.
        .layer(middleware::from_fn(ip_allowlist))
        // This service is expected to be local-only and may control a local agent swarm.
        // Never use `Access-Control-Allow-Origin: *` here; it makes it easier for a random
        // website in your browser to probe/exfiltrate local state.
        .layer(local_only_cors())
}

async fn health() -> &'static str {
    "ok"
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

#[derive(Debug, Clone, Serialize)]
struct ApiState {
    rev: i64,
    working_agents: i64,
    entities: Vec<Entity>,
    quests: Vec<Quest>,
}

async fn api_state(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Result<Json<ApiState>, (axum::http::StatusCode, String)> {
    let rev = state
        .engine
        .get_rev()
        .map_err(internal_error("engine.get_rev"))?;
    let working_agents = state
        .engine
        .count_working_agents()
        .map_err(internal_error("engine.count_working_agents"))?;
    let entities = state
        .engine
        .list_entities()
        .map_err(internal_error("engine.list_entities"))?;
    let quests = state
        .engine
        .list_quests()
        .map_err(internal_error("engine.list_quests"))?;
    Ok(Json(ApiState {
        rev,
        working_agents,
        entities,
        quests,
    }))
}

#[derive(Debug, Clone, Serialize)]
struct BuildingSpec {
    kind: String,
    title: String,
    hotkey: String,
    copy: String,
    preview: String,
    sprite: String,
    w: i64,
    h: i64,
}

async fn api_buildings() -> Json<Vec<BuildingSpec>> {
    Json(building_specs())
}

async fn api_entities_list(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Result<Json<Vec<Entity>>, (axum::http::StatusCode, String)> {
    let entities = state
        .engine
        .list_entities()
        .map_err(internal_error("engine.list_entities"))?;
    Ok(Json(entities))
}

#[derive(Debug, Deserialize)]
struct CreateEntityInput {
    kind: String,
    x: i64,
    y: i64,
}

async fn api_entities_create(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(input): Json<CreateEntityInput>,
) -> Result<Json<Entity>, (axum::http::StatusCode, String)> {
    let specs = building_specs();
    let Some(spec) = specs.iter().find(|b| b.kind == input.kind) else {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "unknown building kind".to_string(),
        ));
    };
    let ent = state
        .engine
        .create_entity(&input.kind, input.x, input.y, spec.w, spec.h)
        .map_err(internal_error("engine.create_entity"))?;
    Ok(Json(ent))
}

async fn api_entities_delete(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let deleted = state
        .engine
        .delete_entity(&id)
        .map_err(internal_error("engine.delete_entity"))?;
    Ok(Json(serde_json::json!({ "ok": true, "deleted": deleted })))
}

async fn api_quests_list(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Result<Json<Vec<Quest>>, (axum::http::StatusCode, String)> {
    let quests = state
        .engine
        .list_quests()
        .map_err(internal_error("engine.list_quests"))?;
    Ok(Json(quests))
}

#[derive(Debug, Deserialize)]
struct UpsertQuestInput {
    #[serde(default)]
    id: Option<String>,
    title: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

async fn api_quests_upsert(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(input): Json<UpsertQuestInput>,
) -> Result<Json<Quest>, (axum::http::StatusCode, String)> {
    let title = input.title.trim();
    if title.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "title is required".to_string(),
        ));
    }
    let kind = input.kind.as_deref().unwrap_or("human");
    let st = input.state.as_deref().unwrap_or("open");
    let body = input.body.as_deref().unwrap_or("");
    let quest = state
        .engine
        .upsert_quest(input.id.as_deref(), title, kind, st, body)
        .map_err(internal_error("engine.upsert_quest"))?;
    Ok(Json(quest))
}

async fn api_quests_delete(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let deleted = state
        .engine
        .delete_quest(&id)
        .map_err(internal_error("engine.delete_quest"))?;
    Ok(Json(serde_json::json!({ "ok": true, "deleted": deleted })))
}

#[derive(Debug, Deserialize)]
struct RunsQuery {
    #[serde(default)]
    entity_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct RunRow {
    id: String,
    status: String,
    task: String,
    created_at: String,
}

async fn api_runs_list(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<RunsQuery>,
) -> Result<Json<Vec<RunRow>>, (axum::http::StatusCode, String)> {
    let Some(entity_id) = q.entity_id else {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "entity_id is required".to_string(),
        ));
    };
    let conn = state.engine.open().map_err(internal_error("engine.open"))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, status, task, created_at
             FROM runs
             WHERE entity_id = ?1
             ORDER BY created_at DESC
             LIMIT 50",
        )
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("db.prepare_runs: {e}"),
            )
        })?;
    let rows = stmt
        .query_map([entity_id], |row| {
            Ok(RunRow {
                id: row.get(0)?,
                status: row.get(1)?,
                task: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("db.query_runs: {e}"),
            )
        })?;
    Ok(Json(rows.filter_map(Result::ok).collect()))
}

#[derive(Debug, Deserialize)]
struct FeatureBuildInput {
    entity_id: String,
    prompt: String,
}

async fn api_feature_build(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(input): Json<FeatureBuildInput>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    if input.prompt.trim().is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "prompt is required".to_string(),
        ));
    }

    let mut conn = state.engine.open().map_err(internal_error("engine.open"))?;
    let tx = conn.transaction().map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db.transaction: {e}"),
        )
    })?;

    let now = time::OffsetDateTime::now_utc();
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "now".to_string());
    let run_id = format!("run-{}", now.unix_timestamp_nanos());
    let task = input.prompt.trim().to_string();
    let ctx = serde_json::json!({
        "entity_id": input.entity_id,
        "prompt": task,
    })
    .to_string();

    tx.execute(
        "INSERT INTO runs (id, workflow_id, task, status, entity_id, context_json, created_at, updated_at)
         VALUES (?1, 'feature', ?2, 'running', ?3, ?4, ?5, ?5)",
        (&run_id, &task, &input.entity_id, &ctx, &ts),
    )
    .map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db.insert_run: {e}"),
        )
    })?;

    let step_row_id = format!("step-{}", now.unix_timestamp_nanos());
    tx.execute(
        "INSERT INTO steps (id, run_id, step_id, agent_id, step_index, status, input_json, output_text, created_at, updated_at)
         VALUES (?1, ?2, 'feature.lead', 'lead', 0, 'pending', ?3, NULL, ?4, ?4)",
        (&step_row_id, &run_id, serde_json::json!({ "prompt": task }).to_string(), &ts),
    )
    .map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db.insert_step: {e}"),
        )
    })?;

    tx.commit().map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db.commit: {e}"),
        )
    })?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "run_id": run_id,
    })))
}

fn internal_error(
    ctx: &'static str,
) -> impl FnOnce(anyhow::Error) -> (axum::http::StatusCode, String) {
    move |e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("{ctx}: {e}"),
        )
    }
}

fn building_specs() -> Vec<BuildingSpec> {
    vec![
        BuildingSpec {
            kind: "base".to_string(),
            title: "Base Core".to_string(),
            hotkey: "B".to_string(),
            copy: "Main command hub. Place first to anchor routing and runtime links.".to_string(),
            preview: "/rts-sprites/thumb-base.webp".to_string(),
            sprite: "/rts-sprites/base_sprite-20260212a.webp".to_string(),
            w: 4,
            h: 4,
        },
        BuildingSpec {
            kind: "feature".to_string(),
            title: "Feature Forge".to_string(),
            hotkey: "F".to_string(),
            copy: "Creates feature runs. Link a base repo, draft stories, and launch agents."
                .to_string(),
            preview: "/rts-sprites/thumb-feature.webp".to_string(),
            sprite: "/rts-sprites/feature_factory_sprite.webp".to_string(),
            w: 3,
            h: 4,
        },
        BuildingSpec {
            kind: "research".to_string(),
            title: "Research Lab".to_string(),
            hotkey: "L".to_string(),
            copy: "Scans repos and generates plan cards. Drag plans to seed feature drafts."
                .to_string(),
            preview: "/rts-sprites/thumb-research.webp".to_string(),
            sprite: "/rts-sprites/research_lab_sprite.webp".to_string(),
            w: 3,
            h: 4,
        },
        BuildingSpec {
            kind: "warehouse".to_string(),
            title: "Warehouse".to_string(),
            hotkey: "W".to_string(),
            copy: "Stores completed artifacts and links them back to base logistics.".to_string(),
            preview: "/rts-sprites/thumb-warehouse.webp".to_string(),
            sprite: "/rts-sprites/warehouse_sprite.webp".to_string(),
            w: 3,
            h: 4,
        },
        BuildingSpec {
            kind: "university".to_string(),
            title: "University".to_string(),
            hotkey: "U".to_string(),
            copy: "Advanced planning campus. Uses Research Lab mechanics with a distinct skin."
                .to_string(),
            preview: "/rts-sprites/thumb-university.webp".to_string(),
            sprite: "/rts-sprites/university_sprite-20260212a.webp".to_string(),
            w: 3,
            h: 4,
        },
        BuildingSpec {
            kind: "library".to_string(),
            title: "Library".to_string(),
            hotkey: "Y".to_string(),
            copy: "Knowledge vault. Uses Warehouse mechanics with a distinct skin.".to_string(),
            preview: "/rts-sprites/thumb-library.webp".to_string(),
            sprite: "/rts-sprites/library_sprite-20260212a.webp".to_string(),
            w: 3,
            h: 4,
        },
        BuildingSpec {
            kind: "power".to_string(),
            title: "Power Plant".to_string(),
            hotkey: "P".to_string(),
            copy: "Cron station. Uses Library placement and shows active jobs.".to_string(),
            preview: "/rts-sprites/thumb-power.webp".to_string(),
            sprite: "/rts-sprites/power_sprite-20260212a.webp".to_string(),
            w: 3,
            h: 4,
        },
    ]
}

pub async fn serve(addr: SocketAddr, db_path: PathBuf) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(listener, db_path, async {
        std::future::pending::<()>().await
    })
    .await?;
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
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    Ok(addr)
}

async fn ip_allowlist(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let ip = peer.ip();
    if is_allowed_peer_ip(ip) {
        return next.run(req).await;
    }
    (axum::http::StatusCode::FORBIDDEN, "forbidden").into_response()
}

fn is_allowed_peer_ip(ip: IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }

    // Tailscale CGNAT range (100.64.0.0/10).
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // 100.64.0.0 - 100.127.255.255
            o[0] == 100 && (64..=127).contains(&o[1])
        }
        IpAddr::V6(_v6) => false,
    }
}

fn local_only_cors() -> CorsLayer {
    use axum::http::header;
    use axum::http::HeaderValue;
    use axum::http::Method;

    CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE])
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _req| {
            is_allowed_local_origin(origin)
        }))
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

const DASHBOARD_HTML: &str = r###"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <meta name="theme-color" content="#081427" />
  <title>Clawdorio Command Grid</title>
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=Orbitron:wght@500;700&family=Inter:wght@400;500;600;700&family=Geist+Mono&display=swap" rel="stylesheet">
  <style>
    :root{
      --bg-a:#050913;
      --bg-b:#081325;
      --bg-c:#0a1a31;
      --ice:#e6fbff;
      --teal:#6ff8ff;
      --blue:#68c7ff;
      --line:#7fcbff5a;
      --panel:#0b1a2dcc;
      --panel-edge:#73c7ff55;
      --muted:#8aa3be;
      --ok:#4df5bf;
      --warn:#ffd06b;
      --bad:#ff7198;
      --dock-w:min(340px, 26vw);
      --top-h:36px;
      --command-h:200px;
      --screen-pad:12px;
    }
    /* No rounded corners anywhere (explicit style rule). */
    *{box-sizing:border-box;margin:0;padding:0;border-radius:0 !important}
    html,body{width:100%;height:100%;overflow:hidden}
    body{
      font-family:Inter,system-ui,sans-serif;
      color:var(--ice);
      background:
        radial-gradient(circle at 12% 12%, #1e3258 0%, #0b1528 24%, transparent 52%),
        radial-gradient(circle at 88% 18%, #163755 0%, #0a1426 25%, transparent 58%),
        linear-gradient(165deg,var(--bg-c) 0%,var(--bg-b) 45%,var(--bg-a) 100%);
    }
    .layout{position:relative;width:100vw;height:100vh}
    .topbar{
      position:absolute;left:var(--screen-pad);right:var(--screen-pad);top:var(--screen-pad);
      height:var(--top-h);display:flex;gap:12px;align-items:center;justify-content:flex-start;
      padding:6px 10px;border:1px solid var(--panel-edge);border-radius:0;
      background:linear-gradient(160deg,#0c223b 0%, #081427 100%);
      box-shadow:0 12px 30px #020c1888;
      z-index:50;
    }
    .resources{display:flex;align-items:baseline;gap:10px}
    .reslabel{font-family:Geist Mono, ui-monospace, SFMono-Regular, Menlo, monospace;font-size:11px;color:#cfefff;letter-spacing:.6px}
    .resvalue{font-family:Orbitron,system-ui,sans-serif;font-size:14px;color:var(--ice);letter-spacing:1px}
    .btn{
      border:1px solid #4f799f;background:#0b1b30;color:var(--ice);
      border-radius:0;padding:8px 10px;font-weight:600;cursor:pointer;
    }
    .btn:hover{border-color:#8de7ff;box-shadow:0 0 0 1px #95e6ff44 inset}

    .dock{
      position:absolute;top:calc(var(--screen-pad) + var(--top-h) + 12px);bottom:calc(var(--screen-pad) + var(--command-h));
      width:var(--dock-w);padding:10px;border:1px solid var(--panel-edge);border-radius:0;
      background:var(--panel);backdrop-filter:blur(10px);
      box-shadow:0 14px 40px #0008;
      overflow:hidden;
      z-index:40;
    }
    .dock.right{right:var(--screen-pad)}
    .dock h2{font-family:Orbitron,system-ui,sans-serif;font-size:13px;letter-spacing:.6px;margin-bottom:10px}
    .dock .scroll{height:100%;overflow:auto;padding-right:6px}
    .card{
      border:1px solid #5fa5d655;border-radius:0;
      background:linear-gradient(160deg,#0c223bdd 0%, #081427dd 100%);
      padding:10px;margin-bottom:10px;
    }
    .card .k{font-size:11px;color:var(--muted);margin-bottom:6px}
    .card .v{font-size:13px}
    .list{display:flex;flex-direction:column;gap:8px}
    .item{
      display:flex;align-items:center;justify-content:space-between;gap:10px;
      padding:10px;border-radius:0;border:1px solid #4f799f55;background:#061325aa;
      cursor:pointer;
    }
    .item:hover{border-color:#8de7ff}
    .item strong{font-size:13px}
    .item span{font-size:11px;color:var(--muted)}

    .commandbar{
      position:absolute;left:var(--screen-pad);right:var(--screen-pad);
      bottom:var(--screen-pad);height:var(--command-h);
      border:1px solid var(--panel-edge);border-radius:0;
      background:linear-gradient(160deg,#0c223bcc 0%, #081427cc 100%);
      backdrop-filter:blur(10px);
      padding:12px;
      box-shadow:0 18px 48px #0009;
      z-index:45;
      display:grid;
      grid-template-columns: 1fr 460px;
      gap:12px;
    }
    .palette-wrap{
      display:flex;
      flex-direction:column;
      gap:10px;
      min-width:0;
    }
    .palette{
      display:flex;
      gap:10px;
      overflow:auto;
      padding:2px;
      border-radius:0;
      border:1px solid #4f799f55;
      background:#061325aa;
    }
    .palette-card{
      width:66px;
      height:66px;
      flex:0 0 auto;
      border-radius:0;
      border:1px solid #4f799f55;
      background:
        radial-gradient(circle at 18% 22%, rgba(255,255,255,0.08) 0%, transparent 56%),
        var(--palette-bg, linear-gradient(160deg,#0d3155 0%, #0a233d 100%));
      background-size:cover;
      background-position:center;
      box-shadow:0 10px 26px #02101f66;
      position:relative;
      cursor:grab;
      outline:none;
    }
    .palette-card:hover{border-color:#8de7ff}
    .palette-card:active{cursor:grabbing}
    .palette-card.active{border-color:#6ff8ff; box-shadow:0 0 0 1px #6ff8ff55 inset, 0 12px 28px #02101f88;}
    .palette-card .hotkey{
      position:absolute;right:7px;bottom:7px;
      font-family:Geist Mono, ui-monospace, SFMono-Regular, Menlo, monospace;
      font-size:11px;
      color:#dff6ff;
      padding:2px 6px;
      border-radius:0;
      border:1px solid #73c7ff55;
      background:#081427cc;
    }
    .palette-card .palette-tooltip{
      position:absolute;
      left:0;
      bottom:calc(100% + 10px);
      width:260px;
      padding:10px 10px 9px;
      border-radius:0;
      border:1px solid #5fa5d655;
      background:#081427f0;
      box-shadow:0 18px 46px #000b;
      pointer-events:none;
      opacity:0;
      transform:translateY(4px);
      transition:opacity .15s ease, transform .15s ease;
      z-index:999;
    }
    .palette-card:hover .palette-tooltip,
    .palette-card:focus-visible .palette-tooltip{
      opacity:1;
      transform:translateY(0);
    }
    .palette-tooltip .tooltip-title{
      font-family:Orbitron,system-ui,sans-serif;
      font-size:12px;
      letter-spacing:.6px;
      display:block;
      margin-bottom:4px;
      color:#dff6ff;
    }
    .palette-tooltip .tooltip-copy{
      font-size:11px;
      color:var(--muted);
      display:block;
      line-height:1.35;
    }
    .bottompanel{
      border-radius:0;border:1px solid #4f799f55;background:#061325aa;
      padding:10px;
      overflow:auto;
    }
    .bottompanel .row{display:flex;align-items:center;justify-content:space-between;font-size:12px;color:var(--muted)}
    .bottompanel h3{
      font-family:Orbitron,system-ui,sans-serif;
      font-size:12px;
      letter-spacing:.6px;
      margin-bottom:8px;
    }
    .bottompanel .sub{font-size:11px;color:var(--muted);margin-bottom:10px}
    .unitrow{display:flex;gap:10px;flex-wrap:wrap}
    .unit{
      display:flex;align-items:center;gap:10px;
      border:1px solid #4f799f55;
      background:#081427cc;
      border-radius:0;
      padding:8px 10px;
      min-width:210px;
    }
    .unit .icon{
      width:44px;height:44px;border-radius:0;
      border:1px solid #73c7ff55;
      background:#0b1b30 center/contain no-repeat;
    }
    .unit strong{font-size:12px}
    .unit span{font-size:11px;color:var(--muted)}
    .kanban{display:grid;grid-template-columns:repeat(3,1fr);gap:10px}
    .col{border:1px solid #4f799f55;border-radius:0;background:#081427cc;padding:10px;min-height:110px}
    .col h4{font-size:11px;color:#cfefff;margin-bottom:8px;font-family:Geist Mono,monospace}
    .chip{border:1px solid #73c7ff55;border-radius:0;padding:8px 10px;background:#061325aa;color:var(--muted);font-size:11px;margin-bottom:8px}

    .viewport{
      position:absolute;
      left:var(--screen-pad);
      right:calc(var(--screen-pad) + var(--dock-w) + 12px);
      top:calc(var(--screen-pad) + var(--top-h) + 12px);
      bottom:calc(var(--screen-pad) + var(--command-h));
      border-radius:0;border:1px solid var(--panel-edge);
      background:radial-gradient(circle at 50% 40%, #0d2a4a 0%, #061325 55%, #04070f 100%);
      box-shadow: inset 0 0 0 1px #0007, 0 22px 70px #0008;
      overflow:hidden;
      z-index:10;
    }
    #rtsCanvas{width:100%;height:100%;display:block}
    .hint{
      position:absolute;left:14px;bottom:14px;z-index:20;
      padding:8px 10px;border-radius:0;border:1px solid #5fa5d655;
      background:#081427bb;color:var(--muted);font-size:11px;
      pointer-events:none;
    }

    /* Small screens: collapse to single column */
    @media (max-width: 980px){
      :root{--dock-w: min(320px, 92vw);}
      .dock.right{display:none}
      .viewport{left:var(--screen-pad);right:var(--screen-pad)}
      .commandbar{grid-template-columns:1fr}
    }
  </style>
</head>
<body>
  <div class="layout">
    <header class="topbar">
      <div class="resources" aria-label="Resources">
        <span class="reslabel">AGENTS</span>
        <span id="agentsCount" class="resvalue">0</span>
      </div>
    </header>

    <main class="viewport">
      <canvas id="rtsCanvas"></canvas>
    </main>

    <aside class="dock right" aria-label="Questbook">
      <h2>Questbook</h2>
      <div class="scroll">
        <div id="questList" class="quest-list" aria-label="Quest list"></div>
        <div class="quest-editor" aria-label="Quest editor">
          <input id="questTitle" type="text" />
          <textarea id="questBody" rows="6"></textarea>
          <div class="quest-actions">
            <select id="questKind" aria-label="Quest kind">
              <option value="human">human</option>
              <option value="system">system</option>
            </select>
            <select id="questState" aria-label="Quest state">
              <option value="open">open</option>
              <option value="done">done</option>
            </select>
            <button id="questSave" class="btn" type="button">Save</button>
            <button id="questNew" class="btn" type="button">New</button>
            <button id="questDelete" class="btn" type="button">Delete</button>
          </div>
        </div>
      </div>
    </aside>

    <footer class="commandbar">
      <section class="palette-wrap">
        <div class="palette" id="palette" aria-label="Building palette"></div>
      </section>
      <section class="bottompanel" id="panel.bottom.bar" aria-label="Selection bottom panel">
      </section>
    </footer>
  </div>

  <script>
  (async function(){
    const $ = (id) => document.getElementById(id);

    const agentsCountEl = $("agentsCount");
    const questListEl = $("questList");
    const questTitleEl = $("questTitle");
    const questBodyEl = $("questBody");
    const questKindEl = $("questKind");
    const questStateEl = $("questState");
    const questSaveEl = $("questSave");
    const questNewEl = $("questNew");
    const questDeleteEl = $("questDelete");
    const paletteEl = $("palette");
    const bottomPanel = $("panel.bottom.bar");

    // Pulled from Antfarm RTS palette/specs via the Rust API, so UI never diverges.
    let BUILDINGS = [];
    let draftKind = "base";
    let selected = null;
    let lastRev = 0;
    const featureDraft = new Map();
    let quests = [];
    let selectedQuestId = null;
    let questDirty = false;

    async function loadBuildings(){
      const r = await fetch("/api/buildings", { cache: "no-store" });
      if (!r.ok) throw new Error("buildings_fetch_failed");
      BUILDINGS = await r.json();
      if (Array.isArray(BUILDINGS) && BUILDINGS.length){
        draftKind = BUILDINGS[0].kind || "base";
      }
    }

    function renderPalette(){
      paletteEl.innerHTML = "";
      for (const b of BUILDINGS){
        const btn = document.createElement("button");
        btn.className = "palette-card";
        btn.type = "button";
        btn.draggable = true;
        btn.title = `${b.title} (${b.hotkey})`;
        btn.style.setProperty("--palette-bg", `url('${b.preview}')`);
        btn.innerHTML = `
          <span class="palette-tooltip" role="tooltip">
            <span class="tooltip-title">${esc(b.title)}</span>
          </span>
          <span class="hotkey">${esc(b.hotkey)}</span>
        `;
        btn.addEventListener("click", () => {
          draftKind = b.kind;
          selected = null;
          updatePaletteActive();
          renderBottomPanel();
          requestDraw();
        });
        btn.addEventListener("dragstart", (e) => {
          draftKind = b.kind;
          updatePaletteActive();
          if (e.dataTransfer){
            e.dataTransfer.setData("text/plain", b.kind);
            e.dataTransfer.effectAllowed = "copy";
          }
        });
        paletteEl.appendChild(btn);
      }
      updatePaletteActive();
    }

    function updatePaletteActive(){
      paletteEl.querySelectorAll(".palette-card").forEach((el, idx) => {
        const b = BUILDINGS[idx];
        if (!b) return;
        el.classList.toggle("active", b.kind === draftKind);
      });
    }

    function esc(s){
      return String(s).replace(/[&<>"]/g, (c) => ({ "&":"&amp;", "<":"&lt;", ">":"&gt;", "\"":"&quot;" }[c]));
    }

    function questById(id){
      return quests.find((q) => String(q.id) === String(id)) || null;
    }

    function wantsBang(q){
      if (!q) return false;
      if (String(q.kind || "") === "human") return true;
      if (String(q.state || "") === "done") return true;
      return false;
    }

    function renderQuestList(){
      if (!questListEl) return;
      questListEl.innerHTML = "";
      for (const q of quests){
        const el = document.createElement("div");
        el.className = "quest-item";
        if (selectedQuestId && String(q.id) === String(selectedQuestId)) el.classList.add("active");
        const bang = wantsBang(q) ? "!" : "";
        el.innerHTML = `<div class="t">${esc(q.title || "")}</div><div class="bang">${esc(bang)}</div>`;
        el.addEventListener("click", () => {
          selectedQuestId = String(q.id);
          questDirty = false;
          syncQuestEditor();
          renderQuestList();
        });
        questListEl.appendChild(el);
      }
      if (!quests.length){
        const empty = document.createElement("div");
        empty.className = "card";
        empty.innerHTML = `<div class="k">No quests</div>`;
        questListEl.appendChild(empty);
      }
    }

    function syncQuestEditor(){
      if (!questTitleEl || !questBodyEl || !questKindEl || !questStateEl) return;
      if (questDirty) return;
      const q = selectedQuestId ? questById(selectedQuestId) : null;
      if (!q){
        questTitleEl.value = "";
        questBodyEl.value = "";
        questKindEl.value = "human";
        questStateEl.value = "open";
        return;
      }
      questTitleEl.value = String(q.title || "");
      questBodyEl.value = String(q.body || "");
      questKindEl.value = String(q.kind || "human");
      questStateEl.value = String(q.state || "open");
    }

    function wireQuestEditor(){
      const markDirty = () => { questDirty = true; };
      if (questTitleEl) questTitleEl.addEventListener("input", markDirty);
      if (questBodyEl) questBodyEl.addEventListener("input", markDirty);
      if (questKindEl) questKindEl.addEventListener("change", markDirty);
      if (questStateEl) questStateEl.addEventListener("change", markDirty);

      if (questNewEl) questNewEl.addEventListener("click", () => {
        selectedQuestId = null;
        questDirty = false;
        syncQuestEditor();
        renderQuestList();
      });

      if (questSaveEl) questSaveEl.addEventListener("click", async () => {
        if (!questTitleEl || !questBodyEl || !questKindEl || !questStateEl) return;
        const title = questTitleEl.value.trim();
        if (!title) return;
        const body = questBodyEl.value || "";
        const kind = questKindEl.value || "human";
        const st = questStateEl.value || "open";
        const payload = { id: selectedQuestId, title, kind, state: st, body };
        try{
          const q = await fetchJson("/api/quests", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(payload),
          });
          selectedQuestId = String(q.id);
          questDirty = false;
          // Force refresh ASAP.
          const st2 = await fetchJson("/api/state");
          quests = Array.isArray(st2.quests) ? st2.quests : [];
          renderQuestList();
          syncQuestEditor();
        }catch(_e){}
      });

      if (questDeleteEl) questDeleteEl.addEventListener("click", async () => {
        if (!selectedQuestId) return;
        try{
          await fetchJson(`/api/quests/${encodeURIComponent(selectedQuestId)}`, { method: "DELETE" });
          selectedQuestId = null;
          questDirty = false;
          const st2 = await fetchJson("/api/state");
          quests = Array.isArray(st2.quests) ? st2.quests : [];
          renderQuestList();
          syncQuestEditor();
        }catch(_e){}
      });
    }

    async function stateLoop(){
      for(;;){
        try{
          const st = await fetchJson("/api/state");
          if (agentsCountEl) agentsCountEl.textContent = String(st.working_agents || 0);
          quests = Array.isArray(st.quests) ? st.quests : [];
          renderQuestList();
          syncQuestEditor();
          const rev = Number(st.rev || 0);
          if (rev !== lastRev){
            applyState(st);
            renderBottomPanel();
            requestDraw();
          }
        }catch(_e){
          // keep last known state
        }
        await new Promise(res => setTimeout(res, 700));
      }
    }

    // RTS-ish canvas: isometric grid, draft placement, camera persist.
    const canvas = $("rtsCanvas");
    const ctx = canvas.getContext("2d");
    let w = 0, h = 0, dpr = 1;

    const CAMERA_KEY = "clawdorio.camera.v1";
    const cam = { x: 0, y: 0, z: 1.0 };
    const grid = { tile: 38, cols: 64, rows: 64 };
    let placed = [];
    const spriteCache = new Map();
    let bgPattern = null;
    let raf = 0;

    const state = {
      isPanning: false,
      panStart: { x: 0, y: 0, camx: 0, camy: 0 },
      mouse: { x: 0, y: 0 },
      hover: null,
    };

    function requestDraw(){
      if (raf) return;
      raf = requestAnimationFrame(() => {
        raf = 0;
        draw();
      });
    }

    function buildingSpec(kind){
      return BUILDINGS.find((b) => b.kind === kind) || null;
    }

    function footprintFor(kind){
      const spec = buildingSpec(kind);
      return {
        w: Number(spec && spec.w ? spec.w : 3),
        h: Number(spec && spec.h ? spec.h : 4),
      };
    }

    function entityCoversCell(ent, cx, cy){
      if (!ent) return false;
      const w = Number(ent.w || 1);
      const h = Number(ent.h || 1);
      return cx >= ent.x && cy >= ent.y && cx < (ent.x + w) && cy < (ent.y + h);
    }

    function hitTestCell(cx, cy){
      // Top-most by updated order isn't available client-side; use last in array.
      for (let i = placed.length - 1; i >= 0; i--){
        const ent = placed[i];
        if (entityCoversCell(ent, cx, cy)) return ent;
      }
      return null;
    }

    function canPlace(kind, x, y){
      const fp = footprintFor(kind);
      for (let dy = 0; dy < fp.h; dy++){
        for (let dx = 0; dx < fp.w; dx++){
          const cx = x + dx;
          const cy = y + dy;
          if (hitTestCell(cx, cy)) return false;
        }
      }
      return true;
    }

    function loadImage(src){
      if (spriteCache.has(src)) return spriteCache.get(src);
      const img = new Image();
      img.decoding = "async";
      img.loading = "eager";
      img.src = src;
      const p = img.decode ? img.decode().catch(() => {}) : Promise.resolve();
      const entry = { img, ready: p };
      spriteCache.set(src, entry);
      p.then(() => requestDraw());
      return entry;
    }

    async function fetchJson(url, opts){
      const r = await fetch(url, Object.assign({ cache: "no-store" }, opts || {}));
      if (!r.ok){
        const t = await r.text().catch(() => "");
        throw new Error(`${url} ${r.status} ${t}`.trim());
      }
      return await r.json();
    }

    function applyState(st){
      if (!st || typeof st !== "object") return;
      lastRev = Number(st.rev || 0);
      const ents = Array.isArray(st.entities) ? st.entities : [];
      placed = ents.map((e) => ({
        id: String(e.id),
        kind: String(e.kind),
        x: Number(e.x),
        y: Number(e.y),
        w: Number(e.w || 1),
        h: Number(e.h || 1),
      }));
      if (selected){
        selected = placed.find((p) => p.id === selected.id) || null;
      }
    }

    async function createEntity(kind, x, y){
      const ent = await fetchJson("/api/entities", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ kind, x, y }),
      });
      placed = placed.filter((p) => !(p.x === Number(ent.x) && p.y === Number(ent.y)));
      placed.push({
        id: String(ent.id),
        kind: String(ent.kind),
        x: Number(ent.x),
        y: Number(ent.y),
        w: Number(ent.w || 1),
        h: Number(ent.h || 1),
      });
      selected = placed.find((p) => p.id === String(ent.id)) || null;
      renderBottomPanel();
      requestDraw();
    }

    async function deleteEntityById(id){
      await fetchJson(`/api/entities/${encodeURIComponent(String(id))}`, { method: "DELETE" });
      const st = await fetchJson("/api/state");
      if (agentsCountEl) agentsCountEl.textContent = String(st.working_agents || 0);
      quests = Array.isArray(st.quests) ? st.quests : [];
      renderQuestList();
      syncQuestEditor();
      applyState(st);
      selected = null;
      renderBottomPanel();
      requestDraw();
    }

    function resize(){
      const r = canvas.getBoundingClientRect();
      dpr = Math.max(1, Math.min(2, window.devicePixelRatio || 1));
      w = Math.floor(r.width * dpr);
      h = Math.floor(r.height * dpr);
      canvas.width = w;
      canvas.height = h;
    }

    function loadCamera(){
      try{
        const raw = localStorage.getItem(CAMERA_KEY);
        if (!raw) return;
        const j = JSON.parse(raw);
        if (typeof j.x === "number") cam.x = j.x;
        if (typeof j.y === "number") cam.y = j.y;
        if (typeof j.z === "number") cam.z = clamp(j.z, 0.5, 2.2);
      }catch(_e){}
    }
    let lastSave = 0;
    function saveCameraThrottled(){
      const now = performance.now();
      if (now - lastSave < 250) return;
      lastSave = now;
      try{ localStorage.setItem(CAMERA_KEY, JSON.stringify(cam)); }catch(_e){}
    }

    function clamp(v, a, b){ return Math.max(a, Math.min(b, v)); }

    function worldToScreen(wx, wy){
      // 2:1 isometric projection with camera.
      const s = grid.tile * cam.z;
      const isoX = (wx - wy) * (s * 0.5);
      const isoY = (wx + wy) * (s * 0.25);
      return {
        x: (w * 0.5) + isoX - cam.x,
        y: (h * 0.42) + isoY - cam.y,
      };
    }

    function screenToWorld(sx, sy){
      const s = grid.tile * cam.z;
      const x = (sx + cam.x - w*0.5) / (s*0.5);
      const y = (sy + cam.y - h*0.42) / (s*0.25);
      // Solve:
      // x = wx - wy
      // y = wx + wy
      const wx = (x + y) * 0.5;
      const wy = (y - x) * 0.5;
      return { wx, wy };
    }

    function snapCell(wx, wy){
      const gx = Math.floor(wx + 0.5);
      const gy = Math.floor(wy + 0.5);
      return { gx, gy };
    }

    function draw(){
      ctx.clearRect(0,0,w,h);

      // Background: star tile pattern (Antfarm RTS asset) if available.
      if (!bgPattern){
        const e = loadImage("/rts-sprites/bg-space-tile-20260212b.webp");
        if (e.img && e.img.complete && e.img.naturalWidth > 0){
          try{
            bgPattern = ctx.createPattern(e.img, "repeat");
          }catch(_e){}
        }
      }
      if (bgPattern){
        ctx.fillStyle = bgPattern;
        ctx.fillRect(0,0,w,h);
        ctx.fillStyle = "rgba(5,9,19,0.55)";
        ctx.fillRect(0,0,w,h);
      }else{
        ctx.fillStyle = "#050913";
        ctx.fillRect(0,0,w,h);
      }

      // Grid.
      const maxCells = 22;
      const center = screenToWorld(w*0.5, h*0.42);
      const cx = Math.floor(center.wx);
      const cy = Math.floor(center.wy);

      for (let y = cy - maxCells; y <= cy + maxCells; y++){
        for (let x = cx - maxCells; x <= cx + maxCells; x++){
          const p = worldToScreen(x, y);
          const s = grid.tile * cam.z;
          const half = s*0.5;
          const quarter = s*0.25;

          // Diamond
          ctx.beginPath();
          ctx.moveTo(p.x, p.y - quarter);
          ctx.lineTo(p.x + half, p.y);
          ctx.lineTo(p.x, p.y + quarter);
          ctx.lineTo(p.x - half, p.y);
          ctx.closePath();

          ctx.strokeStyle = "rgba(127,203,255,0.18)";
          ctx.lineWidth = 1 * dpr;
          ctx.stroke();
        }
      }

      // Placed buildings: footprint + sprite (cached).
      for (const b of placed){
        const p = worldToScreen(b.x, b.y);
        const s = grid.tile * cam.z;
        const half = s*0.5;
        const quarter = s*0.25;

        const isSel = selected && selected.id === b.id;
        const bw = Math.max(1, Number(b.w || 1));
        const bh = Math.max(1, Number(b.h || 1));

        for (let dy = 0; dy < bh; dy++){
          for (let dx = 0; dx < bw; dx++){
            const pc = worldToScreen(b.x + dx, b.y + dy);
            ctx.beginPath();
            ctx.moveTo(pc.x, pc.y - quarter);
            ctx.lineTo(pc.x + half, pc.y);
            ctx.lineTo(pc.x, pc.y + quarter);
            ctx.lineTo(pc.x - half, pc.y);
            ctx.closePath();
            ctx.fillStyle = isSel ? "rgba(111,248,255,0.16)" : "rgba(111,248,255,0.08)";
            ctx.fill();
            ctx.strokeStyle = isSel ? "rgba(111,248,255,0.85)" : "rgba(111,248,255,0.35)";
            ctx.stroke();
          }
        }

        const spec = buildingSpec(b.kind);
        if (spec){
          const e = loadImage(spec.sprite);
          if (e.img && e.img.complete && e.img.naturalWidth > 0){
            const targetW = Math.max(90, 140 * cam.z);
            const scale = targetW / e.img.naturalWidth;
            const dw = e.img.naturalWidth * scale;
            const dh = e.img.naturalHeight * scale;
            // Anchor: bottom center of sprite on tile center.
            ctx.drawImage(e.img, p.x - dw/2, p.y - dh + quarter*0.25, dw, dh);
          }else{
            ctx.fillStyle = "rgba(230,251,255,0.85)";
            ctx.font = `${Math.max(10, 11*cam.z)}px Inter, system-ui, sans-serif`;
            ctx.fillText(spec.title, p.x - half + 6, p.y - quarter - 6);
          }
        }
      }

      // Hover/draft ghost.
      if (state.hover){
        const kind = state.hover.kind || draftKind;
        const fp = footprintFor(kind);
        const valid = !!state.hover.valid;
        const s = grid.tile * cam.z;
        const half = s*0.5;
        const quarter = s*0.25;

        // Footprint cells.
        for (let dy = 0; dy < fp.h; dy++){
          for (let dx = 0; dx < fp.w; dx++){
            const p = worldToScreen(state.hover.x + dx, state.hover.y + dy);
            ctx.beginPath();
            ctx.moveTo(p.x, p.y - quarter);
            ctx.lineTo(p.x + half, p.y);
            ctx.lineTo(p.x, p.y + quarter);
            ctx.lineTo(p.x - half, p.y);
            ctx.closePath();
            ctx.fillStyle = valid ? "rgba(255,208,107,0.07)" : "rgba(255,113,152,0.06)";
            ctx.fill();
            ctx.strokeStyle = valid ? "rgba(255,208,107,0.65)" : "rgba(255,113,152,0.65)";
            ctx.stroke();
          }
        }

        // Draft sprite (transparent).
        const spec = buildingSpec(kind);
        if (spec){
          const e = loadImage(spec.sprite);
          if (e.img && e.img.complete && e.img.naturalWidth > 0){
            const p0 = worldToScreen(state.hover.x, state.hover.y);
            const targetW = Math.max(90, 140 * cam.z);
            const scale = targetW / e.img.naturalWidth;
            const dw = e.img.naturalWidth * scale;
            const dh = e.img.naturalHeight * scale;
            ctx.save();
            ctx.globalAlpha = valid ? 0.45 : 0.20;
            ctx.drawImage(e.img, p0.x - dw/2, p0.y - dh + quarter*0.25, dw, dh);
            ctx.restore();
          }
        }
      }

    }

    function updateHover(clientX, clientY){
      const r = canvas.getBoundingClientRect();
      const sx = (clientX - r.left) * dpr;
      const sy = (clientY - r.top) * dpr;
      const { wx, wy } = screenToWorld(sx, sy);
      const { gx, gy } = snapCell(wx, wy);
      const fp = footprintFor(draftKind);
      const valid = canPlace(draftKind, gx, gy);
      state.hover = { x: gx, y: gy, kind: draftKind, w: fp.w, h: fp.h, valid };
    }

    canvas.addEventListener("mousemove", (e) => {
      state.mouse.x = e.clientX;
      state.mouse.y = e.clientY;
      if (state.isPanning){
        const dx = (e.clientX - state.panStart.x) * dpr;
        const dy = (e.clientY - state.panStart.y) * dpr;
        cam.x = state.panStart.camx - dx;
        cam.y = state.panStart.camy - dy;
        saveCameraThrottled();
        requestDraw();
        return;
      }
      updateHover(e.clientX, e.clientY);
      requestDraw();
    });

    canvas.addEventListener("mousedown", (e) => {
      if (e.button !== 0) return;
      state.isPanning = true;
      state.panStart.x = e.clientX;
      state.panStart.y = e.clientY;
      state.panStart.camx = cam.x;
      state.panStart.camy = cam.y;
    });
    window.addEventListener("mouseup", () => { state.isPanning = false; });

    canvas.addEventListener("dblclick", () => {
      cam.x = 0; cam.y = 0;
      saveCameraThrottled();
      requestDraw();
    });

    canvas.addEventListener("wheel", (e) => {
      e.preventDefault();
      const dz = (e.deltaY > 0) ? -0.08 : 0.08;
      cam.z = clamp(cam.z + dz, 0.5, 2.2);
      saveCameraThrottled();
      requestDraw();
    }, { passive: false });

    canvas.addEventListener("click", (e) => {
      if (!state.hover) return;
      const hit = hitTestCell(state.hover.x, state.hover.y);
      if (hit){
        selected = hit;
        renderBottomPanel();
        requestDraw();
        return;
      }
      // Place a building (draft) via the API (DB is source of truth).
      if (canPlace(draftKind, state.hover.x, state.hover.y)){
        createEntity(draftKind, state.hover.x, state.hover.y).catch(() => {});
      }
    });

    canvas.addEventListener("dragover", (e) => {
      e.preventDefault();
      updateHover(e.clientX, e.clientY);
      requestDraw();
    });
    canvas.addEventListener("drop", (e) => {
      e.preventDefault();
      if (!state.hover) return;
      const kind = (e.dataTransfer && e.dataTransfer.getData("text/plain")) ? e.dataTransfer.getData("text/plain") : draftKind;
      if (!buildingSpec(kind)) return;
      draftKind = kind;
      updatePaletteActive();
      if (!canPlace(kind, state.hover.x, state.hover.y)) return;
      createEntity(kind, state.hover.x, state.hover.y).catch(() => {});
    });

    window.addEventListener("resize", () => resize());
    window.addEventListener("beforeunload", () => { try{ localStorage.setItem(CAMERA_KEY, JSON.stringify(cam)); }catch(_e){} });

    function isTypingTarget(el){
      if (!el) return false;
      const tag = String(el.tagName || "").toLowerCase();
      if (tag === "input" || tag === "textarea" || tag === "select") return true;
      if (el.isContentEditable) return true;
      return false;
    }

    window.addEventListener("keydown", (e) => {
      if (isTypingTarget(e.target)) return;
      const key = String(e.key || "");
      const up = key.length === 1 ? key.toUpperCase() : key;

      if (up === "Escape"){
        selected = null;
        renderBottomPanel();
        requestDraw();
        return;
      }
      if (up === "Delete"){
        if (selected && selected.id){
          deleteEntityById(selected.id).catch(() => {});
        }
        return;
      }

      // Building hotkeys.
      const b = BUILDINGS.find((x) => String(x.hotkey || "").toUpperCase() === up) || null;
      if (!b) return;
      draftKind = b.kind;
      updatePaletteActive();
      if (state.mouse && state.mouse.x && state.mouse.y){
        updateHover(state.mouse.x, state.mouse.y);
      }
      requestDraw();
    });

    function renderBottomPanel(){
      if (!bottomPanel) return;
      if (!selected){
        bottomPanel.innerHTML = "";
        return;
      }

      const spec = buildingSpec(selected.kind);
      const title = spec ? spec.title : selected.kind;

      bottomPanel.innerHTML = `
        <div style="display:flex; align-items:center; justify-content:space-between; gap:10px; margin-bottom:10px;">
          <h3>${esc(title)}</h3>
          <button id="entityDeleteBtn" class="btn" type="button">Delete</button>
        </div>
        <div class="row"><span>ID</span><span>${esc(selected.id || "")}</span></div>
        <div class="row"><span>POS</span><span>${esc(selected.x)},${esc(selected.y)}</span></div>
        <div class="row"><span>SIZE</span><span>${esc(selected.w || 1)}x${esc(selected.h || 1)}</span></div>
        <div id="entityPanelBody" style="margin-top:10px;"></div>
      `;

      const delBtn = bottomPanel.querySelector("#entityDeleteBtn");
      if (delBtn){
        delBtn.addEventListener("click", () => {
          if (selected && selected.id) deleteEntityById(selected.id).catch(() => {});
        });
      }

      const body = bottomPanel.querySelector("#entityPanelBody");
      if (!body) return;

      if (selected.kind !== "feature"){
        body.innerHTML = "";
        return;
      }

      const key = String(selected.id || "");
      const prev = featureDraft.get(key) || "";
      body.innerHTML = `
        <div style="display:flex; gap:10px; align-items:flex-start; margin-bottom:10px;">
          <textarea id="featurePrompt" rows="4" style="flex:1; width:100%; resize:vertical; border:1px solid #4f799f; background:#0b1b30; color:var(--ice); padding:8px 10px; font-family:Geist Mono, ui-monospace, SFMono-Regular, Menlo, monospace; font-size:12px;">${esc(prev)}</textarea>
          <button id="featureBuildBtn" class="btn" type="button" style="white-space:nowrap;">Build</button>
        </div>
        <div id="featureBuildResult" class="sub"></div>
        <div id="featureRuns" style="margin-top:10px;"></div>
      `;

      const ta = bottomPanel.querySelector("#featurePrompt");
      const btn = bottomPanel.querySelector("#featureBuildBtn");
      const out = bottomPanel.querySelector("#featureBuildResult");
      const runsEl = bottomPanel.querySelector("#featureRuns");

      async function refreshRuns(){
        if (!runsEl) return;
        try{
          const runs = await fetchJson(`/api/runs?entity_id=${encodeURIComponent(key)}`);
          if (!Array.isArray(runs) || !runs.length){
            runsEl.innerHTML = "";
            return;
          }
          runsEl.innerHTML = runs.map((r) => (
            `<div class="chip">${esc(r.status)} ${esc(r.id)} ${esc(r.task)}</div>`
          )).join("");
        }catch(_e){
          runsEl.innerHTML = "";
        }
      }

      refreshRuns();

      if (ta){
        ta.addEventListener("input", () => featureDraft.set(key, ta.value));
      }
      if (btn){
        btn.addEventListener("click", async () => {
          const prompt = ta ? ta.value : "";
          if (!prompt.trim()) return;
          if (out) out.textContent = "building";
          try{
            const res = await fetchJson("/api/feature/build", {
              method: "POST",
              headers: { "content-type": "application/json" },
              body: JSON.stringify({ entity_id: key, prompt }),
            });
            if (out) out.textContent = String(res.run_id || "");
            refreshRuns();
          }catch(_e){
            if (out) out.textContent = "";
          }
        });
      }
    }

    try{
      await loadBuildings();
      renderPalette();
      // Warm caches for instant hover/placement.
      for (const b of BUILDINGS){
        if (b && b.preview) loadImage(b.preview);
        if (b && b.sprite) loadImage(b.sprite);
      }
      // Initial DB-backed world state.
      const st = await fetchJson("/api/state");
      if (agentsCountEl) agentsCountEl.textContent = String(st.working_agents || 0);
      quests = Array.isArray(st.quests) ? st.quests : [];
      renderQuestList();
      syncQuestEditor();
      applyState(st);
    }catch(_e){
      // stay usable offline (palette might be empty)
    }
    loadCamera();
    resize();
    renderBottomPanel();
    requestDraw();
    wireQuestEditor();
    stateLoop();
  })();
  </script>
</body>
</html>
"###;
