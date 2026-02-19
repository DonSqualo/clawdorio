use axum::http::header;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::{
    middleware,
    response::{Html, IntoResponse},
    routing::delete,
    routing::get,
    routing::post,
    Json, Router,
};
use clawdorio_engine::{Belt, Engine, Entity, Quest};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;
use tower::ServiceBuilder;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
}

const DEFAULT_AUTO_REBASE_ENABLED: bool = true;
const DEFAULT_AUTO_REBASE_INTERVAL_SEC: i64 = 900;
const AUTO_REBASE_MAX_RETRIES: i64 = 3;

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
        .route("/api/local-repos", get(api_local_repos))
        .route(
            "/api/entities",
            get(api_entities_list).post(api_entities_create),
        )
        .route(
            "/api/entities/{id}",
            delete(api_entities_delete).patch(api_entities_update_pos),
        )
        .route("/api/entities/{id}/repo", post(api_entities_attach_repo))
        .route("/api/belts", get(api_belts_list).post(api_belts_create))
        .route("/api/belts/{id}", delete(api_belts_delete))
        .route("/api/quests", get(api_quests_list).post(api_quests_upsert))
        .route("/api/quests/{id}", delete(api_quests_delete))
        .route("/api/runs", get(api_runs_list))
        .route("/api/runs/{id}/steps", get(api_run_steps))
        .route("/api/pr-feed", get(api_pr_feed))
        .route("/api/pr-feed/{run_id}/files", get(api_pr_feed_files))
        .route("/api/prs/comment", post(api_pr_comment))
        .route("/api/feature/build", post(api_feature_build))
        .route("/api/workers/reemit", post(api_workers_reemit_global))
        .route("/api/github/webhook", post(api_github_webhook))
        .route(
            "/api/bases/{id}/workers/reemit",
            post(api_workers_reemit_base),
        )
        .route("/api/bases/{id}/sync-now", post(api_bases_sync_now))
        .route(
            "/api/bases/{id}/auto-rebase",
            get(api_base_auto_rebase_get).patch(api_base_auto_rebase_patch),
        )
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
    belts: Vec<Belt>,
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
    let belts = state
        .engine
        .list_belts()
        .map_err(internal_error("engine.list_belts"))?;
    Ok(Json(ApiState {
        rev,
        working_agents,
        entities,
        quests,
        belts,
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

#[derive(Debug, Clone, Serialize)]
struct LocalRepo {
    path: String,
    name: String,
}

async fn api_local_repos() -> Json<Vec<LocalRepo>> {
    Json(discover_local_repos())
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
    #[serde(default)]
    repo_path: Option<String>,
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

    // Authoritative placement rules:
    // - No overlaps
    // - Non-base buildings must be close to a base (and will be linked to that base)
    let entities = state
        .engine
        .list_entities()
        .map_err(internal_error("engine.list_entities"))?;
    let belts = state
        .engine
        .list_belts()
        .map_err(internal_error("engine.list_belts"))?;

    let fp = (spec.w, spec.h);
    if overlaps_any(&entities, input.x, input.y, fp.0, fp.1) {
        return Err((axum::http::StatusCode::CONFLICT, "overlap".to_string()));
    }
    if overlaps_any_belt(&belts, input.x, input.y, fp.0, fp.1) {
        return Err((axum::http::StatusCode::CONFLICT, "overlap_belt".to_string()));
    }

    let mut payload = serde_json::json!({});
    if input.kind == "base" {
        let repo_path = input.repo_path.as_deref().unwrap_or("").trim();
        if repo_path.is_empty() {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "repo_path_required".to_string(),
            ));
        }
        let p = std::path::Path::new(repo_path);
        let git_dir = p.join(".git");
        if !git_dir.exists() {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "not_git_repo".to_string(),
            ));
        }
        payload["repo_path"] = serde_json::Value::String(repo_path.to_string());
        payload["auto_rebase_enabled"] = serde_json::Value::Bool(DEFAULT_AUTO_REBASE_ENABLED);
        payload["auto_rebase_interval_sec"] =
            serde_json::Value::Number(DEFAULT_AUTO_REBASE_INTERVAL_SEC.into());
    } else {
        let Some(base_id) = nearest_base_id(&entities, input.x, input.y, fp.0, fp.1, 12) else {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "requires_base".to_string(),
            ));
        };
        payload["base_id"] = serde_json::Value::String(base_id);
    }

    // University connects only to a library (not directly to base), so disallow it unless a library exists.
    if input.kind == "university" {
        let base_id = payload
            .get("base_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let has_library = entities.iter().any(|e| {
            if e.kind != "library" {
                return false;
            }
            let v: serde_json::Value =
                serde_json::from_str(&e.payload_json).unwrap_or_else(|_| serde_json::json!({}));
            v.get("base_id")
                .and_then(|x| x.as_str())
                .map(|s| s == base_id)
                .unwrap_or(false)
        });
        if !has_library {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "university_requires_library".to_string(),
            ));
        }
    }

    let ent = state
        .engine
        .create_entity_with_payload(
            &input.kind,
            input.x,
            input.y,
            spec.w,
            spec.h,
            &payload.to_string(),
        )
        .map_err(internal_error("engine.create_entity_with_payload"))?;

    // Seed default belts for this entity (Factorio-ish).
    if let Err(_e) = seed_belts_for_entity(&state.engine, &ent) {
        // Best-effort: belts are derivable; never fail placement on belt sync.
    }

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
    if deleted {
        // Best-effort belt cleanup.
        if let Ok(conn) = state.engine.open() {
            let _ = conn.execute("DELETE FROM belts WHERE a_id=?1 OR b_id=?1", [&id]);
        }
    }
    Ok(Json(serde_json::json!({ "ok": true, "deleted": deleted })))
}

#[derive(Debug, Deserialize)]
struct UpdateEntityPosInput {
    x: i64,
    y: i64,
}

async fn api_entities_update_pos(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(input): Json<UpdateEntityPosInput>,
) -> Result<Json<Entity>, (axum::http::StatusCode, String)> {
    // Authoritative move rules: no overlaps; non-base remains near a base.
    let entities = state
        .engine
        .list_entities()
        .map_err(internal_error("engine.list_entities"))?;
    let belts = state
        .engine
        .list_belts()
        .map_err(internal_error("engine.list_belts"))?;
    let Some(cur) = entities.iter().find(|e| e.id == id).cloned() else {
        return Err((axum::http::StatusCode::NOT_FOUND, "not_found".to_string()));
    };
    let fp = (cur.w, cur.h);
    let others: Vec<Entity> = entities.into_iter().filter(|e| e.id != id).collect();
    if overlaps_any(&others, input.x, input.y, fp.0, fp.1) {
        return Err((axum::http::StatusCode::CONFLICT, "overlap".to_string()));
    }
    if overlaps_any_belt(&belts, input.x, input.y, fp.0, fp.1) {
        return Err((axum::http::StatusCode::CONFLICT, "overlap_belt".to_string()));
    }
    if cur.kind != "base" {
        if nearest_base_id(&others, input.x, input.y, fp.0, fp.1, 12).is_none() {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "requires_base".to_string(),
            ));
        }
    }
    let ent = state
        .engine
        .update_entity_position(&id, input.x, input.y)
        .map_err(internal_error("engine.update_entity_position"))?
        .ok_or((axum::http::StatusCode::NOT_FOUND, "not_found".to_string()))?;
    Ok(Json(ent))
}

#[derive(Debug, Deserialize)]
struct AttachRepoInput {
    repo_path: String,
}

async fn api_entities_attach_repo(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(input): Json<AttachRepoInput>,
) -> Result<Json<Entity>, (axum::http::StatusCode, String)> {
    let repo_path = input.repo_path.trim();
    if repo_path.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "repo_path required".to_string(),
        ));
    }
    let p = std::path::Path::new(repo_path);
    let git_dir = p.join(".git");
    if !git_dir.exists() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "not_git_repo".to_string(),
        ));
    }

    // Ensure entity exists and is a base.
    let entities = state
        .engine
        .list_entities()
        .map_err(internal_error("engine.list_entities"))?;
    let Some(ent) = entities.iter().find(|e| e.id == id) else {
        return Err((axum::http::StatusCode::NOT_FOUND, "not_found".to_string()));
    };
    if ent.kind != "base" {
        return Err((axum::http::StatusCode::BAD_REQUEST, "not_base".to_string()));
    }

    let mut payload: serde_json::Value =
        serde_json::from_str(&ent.payload_json).unwrap_or_else(|_| serde_json::json!({}));
    if payload
        .get("repo_path")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "repo_already_set".to_string(),
        ));
    }
    payload["repo_path"] = serde_json::Value::String(repo_path.to_string());
    if payload.get("auto_rebase_enabled").is_none() {
        payload["auto_rebase_enabled"] = serde_json::Value::Bool(DEFAULT_AUTO_REBASE_ENABLED);
    }
    if payload.get("auto_rebase_interval_sec").is_none() {
        payload["auto_rebase_interval_sec"] =
            serde_json::Value::Number(DEFAULT_AUTO_REBASE_INTERVAL_SEC.into());
    }
    let updated = state
        .engine
        .update_entity_payload(&id, &payload.to_string())
        .map_err(internal_error("engine.update_entity_payload"))?
        .ok_or((axum::http::StatusCode::NOT_FOUND, "not_found".to_string()))?;
    Ok(Json(updated))
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

async fn api_belts_list(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Result<Json<Vec<Belt>>, (axum::http::StatusCode, String)> {
    let belts = state
        .engine
        .list_belts()
        .map_err(internal_error("engine.list_belts"))?;
    Ok(Json(belts))
}

#[derive(Debug, Deserialize)]
struct CreateBeltInput {
    a_id: String,
    b_id: String,
    #[serde(default)]
    kind: Option<String>,
}

async fn api_belts_create(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(input): Json<CreateBeltInput>,
) -> Result<Json<Belt>, (axum::http::StatusCode, String)> {
    let a_id = input.a_id.trim();
    let b_id = input.b_id.trim();
    if a_id.is_empty() || b_id.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "a_id and b_id required".to_string(),
        ));
    }
    let kind = input.kind.as_deref().unwrap_or("link");
    // Compute a path so belts actually occupy space.
    let ents = state
        .engine
        .list_entities()
        .map_err(internal_error("engine.list_entities"))?;
    let a = ents.iter().find(|e| e.id == a_id).ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "a_id_not_found".to_string(),
    ))?;
    let b = ents.iter().find(|e| e.id == b_id).ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "b_id_not_found".to_string(),
    ))?;
    let path = belt_path_cells(&ents, a, b);
    let path_json = serde_json::to_string(&path).unwrap_or_else(|_| "[]".to_string());

    let belt = state
        .engine
        .create_belt(a_id, b_id, kind, &path_json)
        .map_err(internal_error("engine.create_belt"))?;
    Ok(Json(belt))
}

async fn api_belts_delete(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let deleted = state
        .engine
        .delete_belt(&id)
        .map_err(internal_error("engine.delete_belt"))?;
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

#[derive(Debug, Serialize)]
struct StepRow {
    id: String,
    step_id: String,
    agent_id: String,
    step_index: i64,
    status: String,
    output_text: Option<String>,
    updated_at: String,
}

async fn api_run_steps(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(run_id): axum::extract::Path<String>,
) -> Result<Json<Vec<StepRow>>, (axum::http::StatusCode, String)> {
    let conn = state.engine.open().map_err(internal_error("engine.open"))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, step_id, agent_id, step_index, status, output_text, updated_at
             FROM steps
             WHERE run_id = ?1
             ORDER BY step_index ASC",
        )
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("db.prepare_steps: {e}"),
            )
        })?;
    let rows = stmt
        .query_map([run_id], |row| {
            Ok(StepRow {
                id: row.get(0)?,
                step_id: row.get(1)?,
                agent_id: row.get(2)?,
                step_index: row.get(3)?,
                status: row.get(4)?,
                output_text: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("db.query_steps: {e}"),
            )
        })?;
    Ok(Json(rows.filter_map(Result::ok).collect()))
}

#[derive(Debug, Deserialize)]
struct PrFeedQuery {
    #[serde(default)]
    base_id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct PrFileView {
    path: String,
    additions: i64,
    deletions: i64,
    snippet: String,
}

#[derive(Debug, Serialize)]
struct PrChangedSummary {
    total_files: usize,
    sample: Vec<String>,
    source: String,
    warning: Option<String>,
}

#[derive(Debug, Serialize)]
struct PrCard {
    run_id: String,
    factory_id: Option<String>,
    base_id: Option<String>,
    repo: Option<String>,
    pr_url: Option<String>,
    pr_number: Option<i64>,
    branch: Option<String>,
    status: String,
    updated_at: String,
    title: String,
    changed_files: PrChangedSummary,
}

#[derive(Debug, Deserialize)]
struct PrFilesQuery {
    #[serde(default)]
    max_patch_chars: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct PrCommentInput {
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    pr_url: Option<String>,
    #[serde(default)]
    pr_number: Option<i64>,
    comment: String,
    #[serde(default)]
    idempotency_key: Option<String>,
}

async fn api_pr_feed(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<PrFeedQuery>,
) -> Result<Json<Vec<PrCard>>, (axum::http::StatusCode, String)> {
    let conn = state.engine.open().map_err(internal_error("engine.open"))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, entity_id, status, task, context_json, updated_at
             FROM runs
             ORDER BY updated_at DESC
             LIMIT 120",
        )
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("db.prepare_pr_feed: {e}"),
            )
        })?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("db.query_pr_feed: {e}"),
            )
        })?;

    let entities = state
        .engine
        .list_entities()
        .map_err(internal_error("engine.list_entities"))?;
    let limit = q.limit.unwrap_or(30).clamp(1, 100);
    let mut cards = Vec::new();

    for (run_id, factory_id, status, task, ctx, updated_at) in rows.filter_map(Result::ok) {
        if cards.len() >= limit {
            break;
        }
        let v: serde_json::Value =
            serde_json::from_str(&ctx).unwrap_or_else(|_| serde_json::json!({}));
        let pr_url = v
            .get("pr_url")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let branch = v
            .get("branch")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if pr_url.is_none() && branch.is_none() {
            continue;
        }

        let base_id = factory_id
            .as_ref()
            .and_then(|id| entities.iter().find(|e| &e.id == id))
            .and_then(payload_base_id);
        if q.base_id.as_deref().is_some() && q.base_id.as_deref() != base_id.as_deref() {
            continue;
        }

        let repo = v
            .get("base_repo_path")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let pr_number = v
            .get("pr_number")
            .and_then(|x| x.as_i64())
            .or_else(|| pr_url.as_deref().and_then(parse_pr_number_from_url));

        let changed_files = if let (Some(repo), Some(num)) = (repo.as_deref(), pr_number) {
            match gh_pr_changed_files_summary(repo, &num.to_string()) {
                Ok(v) => v,
                Err(e) => PrChangedSummary {
                    total_files: 0,
                    sample: vec![],
                    source: "fallback".to_string(),
                    warning: Some(e),
                },
            }
        } else {
            PrChangedSummary {
                total_files: 0,
                sample: vec![],
                source: "fallback".to_string(),
                warning: Some("missing_pr_context".to_string()),
            }
        };

        cards.push(PrCard {
            run_id,
            factory_id,
            base_id,
            repo,
            pr_url,
            pr_number,
            branch,
            status,
            updated_at,
            title: task
                .lines()
                .next()
                .unwrap_or(task.as_str())
                .trim()
                .to_string(),
            changed_files,
        });
    }
    Ok(Json(cards))
}

async fn api_pr_feed_files(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(run_id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<PrFilesQuery>,
) -> Result<Json<Vec<PrFileView>>, (axum::http::StatusCode, String)> {
    let conn = state.engine.open().map_err(internal_error("engine.open"))?;
    let ctx_raw: Option<String> = conn
        .query_row(
            "SELECT context_json FROM runs WHERE id=?1",
            [&run_id],
            |r| r.get(0),
        )
        .ok();
    let Some(ctx_raw) = ctx_raw else {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            "run_not_found".to_string(),
        ));
    };
    let ctx: serde_json::Value =
        serde_json::from_str(&ctx_raw).unwrap_or_else(|_| serde_json::json!({}));
    let repo = ctx.get("base_repo_path").and_then(|x| x.as_str()).ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "base_repo_missing".to_string(),
    ))?;
    let pr_selector = ctx
        .get("pr_number")
        .and_then(|x| x.as_i64())
        .map(|n| n.to_string())
        .or_else(|| {
            ctx.get("pr_url")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .ok_or((
            axum::http::StatusCode::BAD_REQUEST,
            "pr_missing".to_string(),
        ))?;

    let files = gh_pr_file_snippets(
        repo,
        &pr_selector,
        q.max_patch_chars.unwrap_or(1600).clamp(200, 8000),
    )
    .map_err(|e| (axum::http::StatusCode::FAILED_DEPENDENCY, e))?;
    Ok(Json(files))
}

async fn api_pr_comment(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(input): Json<PrCommentInput>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let comment = input.comment.trim();
    if comment.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "comment_required".to_string(),
        ));
    }

    let mut conn = state.engine.open().map_err(internal_error("engine.open"))?;
    let tx = conn.transaction().map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db.transaction: {e}"),
        )
    })?;

    if let Some(key) = input
        .idempotency_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let prev: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM event_log WHERE kind='pr.comment.reemit' AND json_extract(payload_json, '$.idempotency_key')=?1",
                [key],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if prev > 0 {
            tx.commit().ok();
            return Ok(Json(
                serde_json::json!({"ok": true, "idempotent_replay": true}),
            ));
        }
    }

    let run_rows: Vec<(String, Option<String>, String)> = {
        let mut stmt = tx
            .prepare(
                "SELECT id, entity_id, context_json FROM runs ORDER BY updated_at DESC LIMIT 200",
            )
            .map_err(|e| {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("db.prepare_run_lookup: {e}"),
                )
            })?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("db.query_run_lookup: {e}"),
                )
            })?;
        rows.filter_map(Result::ok).collect()
    };

    let mut matched: Option<(String, Option<String>)> = None;
    for (run_id, entity_id, ctx_raw) in run_rows {
        if input.run_id.as_deref() == Some(run_id.as_str()) {
            matched = Some((run_id, entity_id));
            break;
        }
        let v: serde_json::Value =
            serde_json::from_str(&ctx_raw).unwrap_or_else(|_| serde_json::json!({}));
        let pr_url = v.get("pr_url").and_then(|x| x.as_str());
        let pr_number = v
            .get("pr_number")
            .and_then(|x| x.as_i64())
            .or_else(|| pr_url.and_then(parse_pr_number_from_url));
        if input.pr_url.as_deref() == pr_url
            || (input.pr_number.is_some() && input.pr_number == pr_number)
        {
            matched = Some((run_id, entity_id));
            break;
        }
    }

    let Some((run_id, factory_id)) = matched else {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            "no_linked_factory_or_run: pass run_id or PR reference".to_string(),
        ));
    };

    let entities = state
        .engine
        .list_entities()
        .map_err(internal_error("engine.list_entities"))?;
    let base_id = factory_id
        .as_ref()
        .and_then(|id| entities.iter().find(|e| &e.id == id))
        .and_then(payload_base_id);

    if let Some(ref b) = base_id {
        let last_ts: Option<i64> = tx
            .query_row(
                "SELECT MAX(ts_ms) FROM event_log WHERE kind='pr.comment.reemit' AND json_extract(payload_json, '$.base_id')=?1",
                [b],
                |r| r.get(0),
            )
            .ok();
        if let Some(last) = last_ts {
            let elapsed = now_ms_i64() - last;
            if elapsed < 15_000 {
                return Err((
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    format!("rate_limited: retry_after_ms={}", 15_000 - elapsed),
                ));
            }
        }
    }

    tx.execute(
        "INSERT INTO event_log (ts_ms, kind, entity_id, payload_json) VALUES (?1, 'pr.comment.reemit', ?2, ?3)",
        (
            now_ms_i64(),
            &run_id,
            serde_json::json!({
                "run_id": run_id,
                "factory_id": factory_id,
                "base_id": base_id,
                "comment": comment,
                "idempotency_key": input.idempotency_key,
            })
            .to_string(),
        ),
    )
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("db.insert_comment_event: {e}")))?;
    tx.commit().map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db.commit_comment_event: {e}"),
        )
    })?;

    let report = reemit_workers(&state.engine, base_id.as_deref())
        .map_err(internal_error("reemit_workers"))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "run_id": run_id,
        "base_id": base_id,
        "scope": if base_id.is_some() { "base" } else { "global" },
        "report": report,
        "idempotent_replay": false,
    })))
}

fn parse_pr_number_from_url(url: &str) -> Option<i64> {
    let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    if parts.len() < 2 || parts[parts.len() - 2] != "pull" {
        return None;
    }
    parts.last()?.parse::<i64>().ok()
}

fn gh_pr_changed_files_summary(repo: &str, selector: &str) -> Result<PrChangedSummary, String> {
    let out = Command::new("gh")
        .arg("pr")
        .arg("view")
        .arg(selector)
        .arg("--json")
        .arg("files")
        .current_dir(repo)
        .output()
        .map_err(|_| "gh_missing: install gh and run gh auth login".to_string())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if stderr.to_lowercase().contains("not logged")
            || stderr.to_lowercase().contains("authentication")
        {
            return Err(format!("github_auth_required: {stderr}"));
        }
        return Err(format!("gh_pr_view_failed: {stderr}"));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|_| serde_json::json!({}));
    let files = v
        .get("files")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(PrChangedSummary {
        total_files: files.len(),
        sample: files
            .iter()
            .filter_map(|f| {
                f.get("path")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
            })
            .take(5)
            .collect(),
        source: "gh".to_string(),
        warning: None,
    })
}

fn gh_pr_file_snippets(
    repo: &str,
    selector: &str,
    max_patch_chars: usize,
) -> Result<Vec<PrFileView>, String> {
    let out = Command::new("gh")
        .arg("pr")
        .arg("view")
        .arg(selector)
        .arg("--json")
        .arg("files")
        .current_dir(repo)
        .output()
        .map_err(|_| "gh_missing: install gh and run gh auth login".to_string())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if stderr.to_lowercase().contains("not logged")
            || stderr.to_lowercase().contains("authentication")
        {
            return Err(format!("github_auth_required: {stderr}"));
        }
        if stderr.to_lowercase().contains("forbidden")
            || stderr.to_lowercase().contains("resource not accessible")
        {
            return Err(format!("github_permission_required: {stderr}"));
        }
        return Err(format!("gh_pr_view_failed: {stderr}"));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|_| serde_json::json!({}));
    let files = v
        .get("files")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(files
        .into_iter()
        .map(|f| PrFileView {
            path: f
                .get("path")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            additions: f.get("additions").and_then(|x| x.as_i64()).unwrap_or(0),
            deletions: f.get("deletions").and_then(|x| x.as_i64()).unwrap_or(0),
            snippet: f
                .get("patch")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .chars()
                .take(max_patch_chars)
                .collect(),
        })
        .collect())
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

    let now = time::OffsetDateTime::now_utc();
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "now".to_string());
    let run_id = format!("run-{}", now.unix_timestamp_nanos());
    let task = input.prompt.trim().to_string();
    let now_ms: i64 = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let entities = state
        .engine
        .list_entities()
        .map_err(internal_error("engine.list_entities"))?;
    let Some(factory) = entities.iter().find(|e| e.id == input.entity_id) else {
        return Err((axum::http::StatusCode::NOT_FOUND, "not_found".to_string()));
    };
    if factory.kind != "feature" {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "not_a_factory".to_string(),
        ));
    }
    let base_id = payload_base_id(factory).ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "missing_base".to_string(),
    ))?;
    let Some(base) = entities
        .iter()
        .find(|e| e.kind == "base" && e.id == base_id)
    else {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "missing_base".to_string(),
        ));
    };
    let base_payload: serde_json::Value =
        serde_json::from_str(&base.payload_json).unwrap_or_else(|_| serde_json::json!({}));
    let repo_path = base_payload
        .get("repo_path")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or((
            axum::http::StatusCode::BAD_REQUEST,
            "base_repo_missing".to_string(),
        ))?;
    let repo_git = std::path::Path::new(&repo_path).join(".git");
    if !repo_git.exists() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "base_repo_not_git".to_string(),
        ));
    }

    // Create a new worktree for this factory run under ~/.openclaw/workspace.
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let ws_root = std::path::Path::new(&home)
        .join(".openclaw")
        .join("workspace");
    let wt_dir = ws_root.join(format!("clawdorio-{}", run_id));
    let wt_dir_s = wt_dir.to_string_lossy().to_string();
    let branch = format!("clawdorio/{}", run_id);
    if let Some(parent) = wt_dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // If already exists (unlikely), refuse rather than clobber.
    if wt_dir.exists() {
        return Err((
            axum::http::StatusCode::CONFLICT,
            "worktree_path_exists".to_string(),
        ));
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo_path)
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(&branch)
        .arg(&wt_dir)
        .output()
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("git_worktree_spawn: {e}"),
            )
        })?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).to_string();
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("git_worktree_failed: {}", msg.trim()),
        ));
    }

    let ctx = serde_json::json!({
        "entity_id": input.entity_id,
        "base_id": base.id,
        "base_repo_path": repo_path.clone(),
        "worktree_path": wt_dir_s.clone(),
        "branch": branch.clone(),
        "prompt": task,
    })
    .to_string();

    let mut conn = state.engine.open().map_err(internal_error("engine.open"))?;
    let tx = conn.transaction().map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db.transaction: {e}"),
        )
    })?;

    tx.execute(
        "INSERT INTO runs (id, workflow_id, task, status, entity_id, context_json, created_at, updated_at)
         VALUES (?1, 'feature-dev', ?2, 'queued', ?3, ?4, ?5, ?5)",
        (&run_id, &task, &input.entity_id, &ctx, &ts),
    )
    .map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db.insert_run: {e}"),
        )
    })?;

    // Persist worktree row (actual observed machine state).
    let wt_id = format!("wt-{}", now.unix_timestamp_nanos());
    let desired = serde_json::json!({ "kind": "worktree", "base_repo_path": repo_path.clone(), "branch": branch.clone() }).to_string();
    let observed = serde_json::json!({ "path": wt_dir_s.clone(), "branch": branch.clone(), "base_repo_path": repo_path.clone() }).to_string();
    tx.execute(
        "INSERT INTO worktrees (id, repo_path, desired_json, observed_json, observed_at_ms, updated_at_ms, rev)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, 0)",
        (&wt_id, &repo_path, &desired, &observed, now_ms),
    )
    .map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db.insert_worktree: {e}"),
        )
    })?;

    // Seed Antfarm-like 7-agent chain (execution is driven by listeners; DB is the queue).
    let steps = [
        ("plan", "feature-dev/planner"),
        ("setup", "feature-dev/setup"),
        ("implement", "feature-dev/developer"),
        ("verify", "feature-dev/verifier"),
        ("test", "feature-dev/tester"),
        ("pr", "internal/pr"),
        ("review", "feature-dev/reviewer"),
    ];
    for (idx, (step_id, agent_id)) in steps.iter().enumerate() {
        let step_row_id = format!("step-{}-{}", now.unix_timestamp_nanos(), idx);
        tx.execute(
            "INSERT INTO steps (id, run_id, step_id, agent_id, step_index, status, input_json, output_text, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, NULL, ?7, ?7)",
            (
                &step_row_id,
                &run_id,
                *step_id,
                *agent_id,
                idx as i64,
                ctx.clone(),
                &ts,
            ),
        )
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("db.insert_step: {e}"),
            )
        })?;
    }

    if let Err(e) = tx.commit() {
        // Best-effort cleanup: remove created worktree so the DB stays authoritative.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(&wt_dir)
            .output();
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db.commit: {e}"),
        ));
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "run_id": run_id,
        "worktree_path": wt_dir_s,
    })))
}

async fn api_workers_reemit_global(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let report = reemit_workers(&state.engine, None).map_err(internal_error("reemit_workers"))?;
    Ok(Json(
        serde_json::json!({ "ok": true, "scope": "global", "report": report }),
    ))
}

async fn api_workers_reemit_base(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(base_id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let report = reemit_workers(&state.engine, Some(base_id.as_str()))
        .map_err(internal_error("reemit_workers"))?;
    Ok(Json(
        serde_json::json!({ "ok": true, "scope": "base", "base_id": base_id, "report": report }),
    ))
}

#[derive(Debug, Serialize)]
struct AutoRebaseSettingsView {
    auto_rebase_enabled: bool,
    auto_rebase_interval_sec: i64,
}

#[derive(Debug, Deserialize)]
struct AutoRebaseSettingsPatch {
    auto_rebase_enabled: Option<bool>,
    auto_rebase_interval_sec: Option<i64>,
}

async fn api_base_auto_rebase_get(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(base_id): axum::extract::Path<String>,
) -> Result<Json<AutoRebaseSettingsView>, (axum::http::StatusCode, String)> {
    let ent = find_base_entity(&state.engine, &base_id)?;
    let payload = parse_payload(&ent.payload_json);
    Ok(Json(AutoRebaseSettingsView {
        auto_rebase_enabled: payload_auto_rebase_enabled(&payload),
        auto_rebase_interval_sec: payload_auto_rebase_interval_sec(&payload),
    }))
}

async fn api_base_auto_rebase_patch(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(base_id): axum::extract::Path<String>,
    Json(input): Json<AutoRebaseSettingsPatch>,
) -> Result<Json<AutoRebaseSettingsView>, (axum::http::StatusCode, String)> {
    let ent = find_base_entity(&state.engine, &base_id)?;
    let mut payload = parse_payload(&ent.payload_json);
    if let Some(v) = input.auto_rebase_enabled {
        payload["auto_rebase_enabled"] = serde_json::Value::Bool(v);
    }
    if let Some(v) = input.auto_rebase_interval_sec {
        if v < 30 {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "auto_rebase_interval_sec must be >= 30".to_string(),
            ));
        }
        payload["auto_rebase_interval_sec"] = serde_json::Value::Number(v.into());
    }
    state
        .engine
        .update_entity_payload(&base_id, &payload.to_string())
        .map_err(internal_error("engine.update_entity_payload"))?;
    Ok(Json(AutoRebaseSettingsView {
        auto_rebase_enabled: payload_auto_rebase_enabled(&payload),
        auto_rebase_interval_sec: payload_auto_rebase_interval_sec(&payload),
    }))
}

async fn api_bases_sync_now(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(base_id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let queued = queue_base_rebase_sweep(&state.engine, &base_id, "manual.sync_now", None)
        .map_err(internal_error("queue_base_rebase_sweep"))?;
    Ok(Json(
        serde_json::json!({ "ok": true, "base_id": base_id, "queued": queued }),
    ))
}

async fn api_github_webhook(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let event = headers
        .get("x-github-event")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");

    let repo_full = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut queued = 0usize;

    if event == "push" {
        let ref_name = payload.get("ref").and_then(|v| v.as_str()).unwrap_or("");
        let after = payload.get("after").and_then(|v| v.as_str());
        for base in matching_bases_by_repo(&state.engine, repo_full)
            .map_err(internal_error("matching_bases_by_repo"))?
        {
            let default = detect_default_branch(
                &repo_path_from_payload(&parse_payload(&base.payload_json)).unwrap_or_default(),
            )
            .unwrap_or_else(|_| "main".to_string());
            if ref_name == format!("refs/heads/{default}") {
                if queue_base_rebase_sweep(&state.engine, &base.id, "webhook.push", after)
                    .map_err(internal_error("queue_base_rebase_sweep"))?
                {
                    queued += 1;
                }
            }
        }
    }

    if event == "pull_request" {
        let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let merged = payload
            .get("pull_request")
            .and_then(|v| v.get("merged"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let should = matches!(action, "synchronize" | "opened" | "reopened")
            || (action == "closed" && merged);
        if should {
            let upstream_sha = payload
                .get("pull_request")
                .and_then(|v| v.get("base"))
                .and_then(|v| v.get("sha"))
                .and_then(|v| v.as_str());
            for base in matching_bases_by_repo(&state.engine, repo_full)
                .map_err(internal_error("matching_bases_by_repo"))?
            {
                if queue_base_rebase_sweep(
                    &state.engine,
                    &base.id,
                    "webhook.pull_request",
                    upstream_sha,
                )
                .map_err(internal_error("queue_base_rebase_sweep"))?
                {
                    queued += 1;
                }
            }
        }
    }

    Ok(Json(
        serde_json::json!({"ok": true, "event": event, "queued": queued}),
    ))
}

#[derive(Debug, Serialize)]
struct ReemitReport {
    scanned_runs: usize,
    queued_steps: usize,
    reset_running_steps: usize,
    touched_runs: usize,
}

fn reemit_workers(engine: &Engine, base_id: Option<&str>) -> anyhow::Result<ReemitReport> {
    let mut conn = engine.open()?;
    let tx = conn.transaction()?;
    let mut scanned_runs = 0usize;
    let mut queued_steps = 0usize;
    let mut reset_running_steps = 0usize;
    let mut touched_runs = 0usize;

    let run_rows: Vec<(String, Option<String>, String)> = {
        let mut stmt = tx.prepare(
            "SELECT id, entity_id, status FROM runs WHERE status IN ('queued','running','failed') ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.filter_map(Result::ok).collect()
    };

    for (run_id, entity_id, run_status) in run_rows {
        if let Some(scope_base) = base_id {
            let Some(entity_id) = entity_id.as_deref() else {
                continue;
            };
            let payload: Option<String> = tx
                .query_row(
                    "SELECT payload_json FROM entities WHERE id=?1",
                    [entity_id],
                    |r| r.get(0),
                )
                .ok();
            let in_base = payload
                .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok())
                .and_then(|v| {
                    v.get("base_id")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                })
                .map(|b| b == scope_base)
                .unwrap_or(false);
            if !in_base {
                continue;
            }
        }

        scanned_runs += 1;

        let running_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM steps WHERE run_id=?1 AND status='running'",
            [&run_id],
            |r| r.get(0),
        )?;

        if running_count == 0 {
            let c = tx.execute(
                "UPDATE steps SET status='queued', updated_at=?1
                 WHERE run_id=?2 AND status IN ('pending','waiting')",
                (&now_rfc3339(), &run_id),
            )?;
            queued_steps += c as usize;
        } else {
            // stale-running fallback: allow operator to re-emit and recover crashed workers
            let c = tx.execute(
                "UPDATE steps SET status='queued', updated_at=?1
                 WHERE run_id=?2 AND status='running'",
                (&now_rfc3339(), &run_id),
            )?;
            if c > 0 {
                reset_running_steps += c as usize;
                queued_steps += c as usize;
            }
        }

        let has_failed: i64 = tx.query_row(
            "SELECT COUNT(*) FROM steps WHERE run_id=?1 AND status='failed'",
            [&run_id],
            |r| r.get(0),
        )?;
        if has_failed > 0 {
            let c = tx.execute(
                "UPDATE steps SET status='queued', output_text=NULL, updated_at=?1
                 WHERE run_id=?2 AND step_index >= (
                    SELECT COALESCE(MIN(step_index), 0) FROM steps WHERE run_id=?2 AND status='failed'
                 )",
                (&now_rfc3339(), &run_id),
            )?;
            queued_steps += c as usize;
        }

        if run_status != "done" {
            let u = tx.execute(
                "UPDATE runs SET status='queued', updated_at=?1 WHERE id=?2 AND status != 'done'",
                (&now_rfc3339(), &run_id),
            )?;
            if u > 0 {
                touched_runs += 1;
            }
        }
    }

    tx.execute(
        "INSERT INTO event_log (ts_ms, kind, entity_id, payload_json) VALUES (?1, 'workers.reemit', NULL, ?2)",
        (
            now_ms_i64(),
            serde_json::json!({
                "scope": base_id.unwrap_or("global"),
                "scanned_runs": scanned_runs,
                "queued_steps": queued_steps,
                "reset_running_steps": reset_running_steps,
                "touched_runs": touched_runs,
            })
            .to_string(),
        ),
    )?;
    tx.commit()?;

    Ok(ReemitReport {
        scanned_runs,
        queued_steps,
        reset_running_steps,
        touched_runs,
    })
}

fn parse_payload(payload_json: &str) -> serde_json::Value {
    serde_json::from_str(payload_json).unwrap_or_else(|_| serde_json::json!({}))
}

fn repo_path_from_payload(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("repo_path")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn payload_auto_rebase_enabled(payload: &serde_json::Value) -> bool {
    payload
        .get("auto_rebase_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(DEFAULT_AUTO_REBASE_ENABLED)
}

fn payload_auto_rebase_interval_sec(payload: &serde_json::Value) -> i64 {
    payload
        .get("auto_rebase_interval_sec")
        .and_then(|v| v.as_i64())
        .filter(|v| *v >= 30)
        .unwrap_or(DEFAULT_AUTO_REBASE_INTERVAL_SEC)
}

fn find_base_entity(
    engine: &Engine,
    base_id: &str,
) -> Result<Entity, (axum::http::StatusCode, String)> {
    let entities = engine
        .list_entities()
        .map_err(internal_error("engine.list_entities"))?;
    let Some(ent) = entities
        .into_iter()
        .find(|e| e.id == base_id && e.kind == "base")
    else {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            "base_not_found".to_string(),
        ));
    };
    Ok(ent)
}

fn matching_bases_by_repo(engine: &Engine, full_name: &str) -> anyhow::Result<Vec<Entity>> {
    if full_name.trim().is_empty() {
        return Ok(vec![]);
    }
    let entities = engine.list_entities()?;
    let mut out = vec![];
    for base in entities.into_iter().filter(|e| e.kind == "base") {
        let payload = parse_payload(&base.payload_json);
        let Some(repo_path) = repo_path_from_payload(&payload) else {
            continue;
        };
        if let Ok(name) = repo_full_name(&repo_path) {
            if name == full_name {
                out.push(base);
            }
        }
    }
    Ok(out)
}

fn repo_full_name(repo: &str) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("remote")
        .arg("get-url")
        .arg("origin")
        .output()?;
    if !out.status.success() {
        anyhow::bail!("git_remote_missing");
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    parse_github_full_name(&url).ok_or_else(|| anyhow::anyhow!("repo_parse_failed"))
}

fn parse_github_full_name(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches(".git");
    if let Some(rest) = u.strip_prefix("git@github.com:") {
        return Some(rest.to_string());
    }
    if let Some(i) = u.find("github.com/") {
        return Some(u[(i + "github.com/".len())..].to_string());
    }
    None
}

fn queue_base_rebase_sweep(
    engine: &Engine,
    base_id: &str,
    reason: &str,
    upstream_sha: Option<&str>,
) -> anyhow::Result<bool> {
    let ent = find_base_entity(engine, base_id).map_err(|(_, e)| anyhow::anyhow!(e))?;
    let mut payload = parse_payload(&ent.payload_json);
    if !payload_auto_rebase_enabled(&payload) {
        return Ok(false);
    }
    let repo =
        repo_path_from_payload(&payload).ok_or_else(|| anyhow::anyhow!("base_repo_missing"))?;
    let default_branch = detect_default_branch(&repo).unwrap_or_else(|_| "main".to_string());
    let now_ms = now_ms_i64();
    let interval_ms = payload_auto_rebase_interval_sec(&payload) * 1000;

    let conn = engine.open()?;
    let running_or_queued: i64 = conn.query_row(
        "SELECT COUNT(*) FROM runs WHERE workflow_id='auto-rebase' AND entity_id=?1 AND status IN ('queued','running')",
        [base_id],
        |r| r.get(0),
    )?;
    if running_or_queued > 0 {
        return Ok(false);
    }

    let last_enqueued_ms = payload
        .get("auto_rebase_last_enqueued_ms")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if now_ms - last_enqueued_ms < interval_ms / 2 {
        return Ok(false);
    }

    let ts = now_rfc3339();
    let run_id = format!("run-auto-rebase-{}", now_ms);
    let ctx = serde_json::json!({
        "action": "auto_rebase_sweep",
        "base_id": base_id,
        "base_repo_path": repo,
        "default_branch": default_branch,
        "trigger_reason": reason,
        "upstream_sha": upstream_sha.unwrap_or(""),
    })
    .to_string();

    conn.execute(
        "INSERT INTO runs (id, workflow_id, task, status, entity_id, context_json, created_at, updated_at)
         VALUES (?1, 'auto-rebase', ?2, 'queued', ?3, ?4, ?5, ?5)",
        (&run_id, &format!("Auto-rebase sweep for base {base_id}"), &base_id, &ctx, &ts),
    )?;
    conn.execute(
        "INSERT INTO steps (id, run_id, step_id, agent_id, step_index, status, input_json, output_text, created_at, updated_at)
         VALUES (?1, ?2, 'auto-rebase', 'internal/pr', 0, 'queued', ?3, NULL, ?4, ?4)",
        (format!("step-{run_id}"), &run_id, &ctx, &ts),
    )?;
    conn.execute(
        "INSERT INTO event_log (ts_ms, kind, entity_id, payload_json) VALUES (?1, 'auto_rebase.queued', ?2, ?3)",
        (
            now_ms,
            base_id,
            serde_json::json!({"run_id": run_id, "reason": reason, "upstream_sha": upstream_sha.unwrap_or("")}).to_string(),
        ),
    )?;

    payload["auto_rebase_last_enqueued_ms"] = serde_json::Value::Number(now_ms.into());
    let _ = engine.update_entity_payload(base_id, &payload.to_string())?;

    Ok(true)
}

fn detect_default_branch(repo: &str) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("symbolic-ref")
        .arg("refs/remotes/origin/HEAD")
        .output()?;
    if !out.status.success() {
        return Ok("main".to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or("main")
        .to_string())
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
            sprite: "/rts-sprites/base_sprite-20260217f.webp".to_string(),
            w: 9,
            h: 9,
        },
        BuildingSpec {
            kind: "feature".to_string(),
            title: "Feature Forge".to_string(),
            hotkey: "F".to_string(),
            copy: "Creates feature runs. Link a base repo, draft stories, and launch agents."
                .to_string(),
            preview: "/rts-sprites/thumb-feature.webp".to_string(),
            sprite: "/rts-sprites/feature_factory_sprite-20260217f.webp".to_string(),
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
            sprite: "/rts-sprites/research_lab_sprite-20260217f.webp".to_string(),
            w: 3,
            h: 4,
        },
        BuildingSpec {
            kind: "warehouse".to_string(),
            title: "Warehouse".to_string(),
            hotkey: "W".to_string(),
            copy: "Stores completed artifacts and links them back to base logistics.".to_string(),
            preview: "/rts-sprites/thumb-warehouse.webp".to_string(),
            sprite: "/rts-sprites/warehouse_sprite-20260217f.webp".to_string(),
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
            sprite: "/rts-sprites/university_sprite-20260217f.webp".to_string(),
            w: 3,
            h: 4,
        },
        BuildingSpec {
            kind: "library".to_string(),
            title: "Library".to_string(),
            hotkey: "Y".to_string(),
            copy: "Knowledge vault. Uses Warehouse mechanics with a distinct skin.".to_string(),
            preview: "/rts-sprites/thumb-library.webp".to_string(),
            sprite: "/rts-sprites/library_sprite-20260217f.webp".to_string(),
            w: 3,
            h: 4,
        },
        BuildingSpec {
            kind: "power".to_string(),
            title: "Power Plant".to_string(),
            hotkey: "P".to_string(),
            copy: "Cron station. Uses Library placement and shows active jobs.".to_string(),
            preview: "/rts-sprites/thumb-power.webp".to_string(),
            sprite: "/rts-sprites/power_sprite-20260217f.webp".to_string(),
            w: 3,
            h: 4,
        },
    ]
}

fn overlaps_any(ents: &[Entity], x: i64, y: i64, w: i64, h: i64) -> bool {
    for e in ents {
        if rects_overlap(x, y, w, h, e.x, e.y, e.w, e.h) {
            return true;
        }
    }
    false
}

fn overlaps_any_belt(belts: &[Belt], x: i64, y: i64, w: i64, h: i64) -> bool {
    for b in belts {
        let cells: Vec<BeltCell> = serde_json::from_str(&b.path_json).unwrap_or_default();
        for c in cells {
            if rects_overlap(x, y, w, h, c.x, c.y, 1, 1) {
                return true;
            }
        }
    }
    false
}

fn rects_overlap(ax: i64, ay: i64, aw: i64, ah: i64, bx: i64, by: i64, bw: i64, bh: i64) -> bool {
    let a_r = ax + aw;
    let a_b = ay + ah;
    let b_r = bx + bw;
    let b_b = by + bh;
    ax < b_r && a_r > bx && ay < b_b && a_b > by
}

fn nearest_base_id(
    ents: &[Entity],
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    max_dist: i64,
) -> Option<String> {
    let mut best: Option<(i64, String)> = None;
    let a_l = x;
    let a_t = y;
    let a_r = x + w;
    let a_b = y + h;
    for e in ents.iter().filter(|e| e.kind == "base") {
        let b_l = e.x;
        let b_t = e.y;
        let b_r = e.x + e.w;
        let b_b = e.y + e.h;
        let dx = dist_1d(a_l, a_r, b_l, b_r);
        let dy = dist_1d(a_t, a_b, b_t, b_b);
        let d = dx.max(dy);
        if d <= max_dist {
            match &best {
                None => best = Some((d, e.id.clone())),
                Some((bd, _)) if d < *bd => best = Some((d, e.id.clone())),
                _ => {}
            }
        }
    }
    best.map(|(_, id)| id)
}

fn dist_1d(a0: i64, a1: i64, b0: i64, b1: i64) -> i64 {
    if a1 <= b0 {
        b0 - a1
    } else if b1 <= a0 {
        a0 - b1
    } else {
        0
    }
}

fn payload_base_id(ent: &Entity) -> Option<String> {
    let v: serde_json::Value =
        serde_json::from_str(&ent.payload_json).unwrap_or_else(|_| serde_json::json!({}));
    v.get("base_id")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn entity_center(ent: &Entity) -> (f64, f64) {
    let x = ent.x as f64 + (ent.w as f64) * 0.5;
    let y = ent.y as f64 + (ent.h as f64) * 0.5;
    (x, y)
}

fn seed_belts_for_entity(engine: &Engine, ent: &Entity) -> anyhow::Result<()> {
    let entities = engine.list_entities()?;
    let belts = engine.list_belts().unwrap_or_default();
    let mut seen: std::collections::HashSet<(String, String)> =
        belts.into_iter().map(|b| (b.a_id, b.b_id)).collect();

    let add = |seen: &mut std::collections::HashSet<(String, String)>,
               engine: &Engine,
               entities: &[Entity],
               a: &str,
               b: &str,
               kind: &str| {
        if a == b {
            return;
        }
        let key = (a.to_string(), b.to_string());
        if seen.contains(&key) {
            return;
        }
        let Some(ae) = entities.iter().find(|e| e.id == a) else {
            return;
        };
        let Some(be) = entities.iter().find(|e| e.id == b) else {
            return;
        };
        let path = belt_path_cells(entities, ae, be);
        let path_json = serde_json::to_string(&path).unwrap_or_else(|_| "[]".to_string());
        if engine.create_belt(a, b, kind, &path_json).is_ok() {
            seen.insert(key);
        }
    };

    let kind = ent.kind.as_str();
    if kind == "base" {
        // base belts are created when other structures get placed.
        return Ok(());
    }
    let Some(base_id) = payload_base_id(ent) else {
        return Ok(());
    };
    let Some(base) = entities
        .iter()
        .find(|e| e.kind == "base" && e.id == base_id)
    else {
        return Ok(());
    };

    // Default: connect most buildings to base.
    if matches!(kind, "research" | "library" | "power") {
        add(&mut seen, engine, &entities, &base.id, &ent.id, "link");
    }

    if kind == "warehouse" {
        // Warehouses connect to nearest lab (research/university) for the same base.
        let (ex, ey) = entity_center(ent);
        let mut best: Option<(&Entity, f64)> = None;
        for cand in entities
            .iter()
            .filter(|e| matches!(e.kind.as_str(), "research" | "university"))
        {
            if payload_base_id(cand).as_deref() != Some(&base_id) {
                continue;
            }
            let (cx, cy) = entity_center(cand);
            let d = ((cx - ex).powi(2) + (cy - ey).powi(2)).sqrt();
            if best.as_ref().map(|(_, bd)| d < *bd).unwrap_or(true) {
                best = Some((cand, d));
            }
        }
        if let Some((lab, _)) = best {
            add(&mut seen, engine, &entities, &lab.id, &ent.id, "link");
        } else {
            add(&mut seen, engine, &entities, &base.id, &ent.id, "link");
        }
    }

    if kind == "feature" {
        // Factories connect to base and (if present) the nearest warehouse.
        add(&mut seen, engine, &entities, &base.id, &ent.id, "link");
        let (ex, ey) = entity_center(ent);
        let mut best_wh: Option<(&Entity, f64)> = None;
        for wh in entities.iter().filter(|e| e.kind == "warehouse") {
            if payload_base_id(wh).as_deref() != Some(&base_id) {
                continue;
            }
            let (cx, cy) = entity_center(wh);
            let d = ((cx - ex).powi(2) + (cy - ey).powi(2)).sqrt();
            if best_wh.as_ref().map(|(_, bd)| d < *bd).unwrap_or(true) {
                best_wh = Some((wh, d));
            }
        }
        if let Some((wh, _)) = best_wh {
            add(&mut seen, engine, &entities, &wh.id, &ent.id, "link");
        }
    }

    // Universities <-> Libraries.
    if kind == "university" || kind == "library" {
        let (ex, ey) = entity_center(ent);
        if kind == "university" {
            let mut best_lib: Option<(&Entity, f64)> = None;
            for lib in entities.iter().filter(|e| e.kind == "library") {
                if payload_base_id(lib).as_deref() != Some(&base_id) {
                    continue;
                }
                let (cx, cy) = entity_center(lib);
                let d = ((cx - ex).powi(2) + (cy - ey).powi(2)).sqrt();
                if best_lib.as_ref().map(|(_, bd)| d < *bd).unwrap_or(true) {
                    best_lib = Some((lib, d));
                }
            }
            if let Some((lib, _)) = best_lib {
                add(&mut seen, engine, &entities, &ent.id, &lib.id, "link");
            }
        } else {
            let mut best_uni: Option<(&Entity, f64)> = None;
            for uni in entities.iter().filter(|e| e.kind == "university") {
                if payload_base_id(uni).as_deref() != Some(&base_id) {
                    continue;
                }
                let (cx, cy) = entity_center(uni);
                let d = ((cx - ex).powi(2) + (cy - ey).powi(2)).sqrt();
                if best_uni.as_ref().map(|(_, bd)| d < *bd).unwrap_or(true) {
                    best_uni = Some((uni, d));
                }
            }
            if let Some((uni, _)) = best_uni {
                add(&mut seen, engine, &entities, &uni.id, &ent.id, "link");
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BeltCell {
    x: i64,
    y: i64,
}

fn belt_anchor_cell(ent: &Entity) -> (i64, i64) {
    let cx = ent.x + (ent.w / 2);
    let cy = ent.y + ent.h;
    (cx, cy)
}

fn rect_contains(ent: &Entity, x: i64, y: i64) -> bool {
    x >= ent.x && y >= ent.y && x < (ent.x + ent.w) && y < (ent.y + ent.h)
}

fn belt_path_cells(ents: &[Entity], a: &Entity, b: &Entity) -> Vec<BeltCell> {
    let (sx, sy) = belt_anchor_cell(a);
    let (ex, ey) = belt_anchor_cell(b);

    let mut path1: Vec<(i64, i64)> = vec![];
    let mut x = sx;
    let mut y = sy;
    // x then y
    while x != ex {
        path1.push((x, y));
        x += if ex > x { 1 } else { -1 };
    }
    while y != ey {
        path1.push((x, y));
        y += if ey > y { 1 } else { -1 };
    }
    path1.push((ex, ey));

    let mut path2: Vec<(i64, i64)> = vec![];
    let mut x = sx;
    let mut y = sy;
    // y then x
    while y != ey {
        path2.push((x, y));
        y += if ey > y { 1 } else { -1 };
    }
    while x != ex {
        path2.push((x, y));
        x += if ex > x { 1 } else { -1 };
    }
    path2.push((ex, ey));

    let score = |p: &[(i64, i64)]| -> i64 {
        let mut bad = 0;
        for (x, y) in p.iter().copied() {
            // Don't count occupancy inside endpoints.
            if rect_contains(a, x, y) || rect_contains(b, x, y) {
                continue;
            }
            for e in ents {
                if e.id == a.id || e.id == b.id {
                    continue;
                }
                if rect_contains(e, x, y) {
                    bad += 1;
                    break;
                }
            }
        }
        bad
    };
    let s1 = score(&path1);
    let s2 = score(&path2);
    let best = if s1 <= s2 { path1 } else { path2 };

    let mut out: Vec<BeltCell> = vec![];
    let mut seen: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
    for (x, y) in best {
        if rect_contains(a, x, y) || rect_contains(b, x, y) {
            continue;
        }
        if seen.insert((x, y)) {
            out.push(BeltCell { x, y });
        }
    }
    out
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
    // Best-effort DB repair: backfill belt paths so belts can occupy tiles even for older rows.
    if let Err(_e) = repair_belt_paths(&state.engine) {
        // Belts are derivable; never fail startup on this.
    }
    // Background runner: executes pending run steps by invoking OpenClaw agents + local PR tooling.
    let eng = state.engine.clone();
    tokio::spawn(async move { runloop(eng).await });
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

async fn runloop(engine: Engine) {
    let mut idle_loops: u32 = 0;
    loop {
        // All DB + process execution work is blocking; keep it off the async runtime.
        let eng = engine.clone();
        let ran = tokio::task::spawn_blocking(move || run_one_step_blocking(&eng).unwrap_or(false))
            .await
            .unwrap_or(false);

        if ran {
            idle_loops = 0;
        } else {
            idle_loops = idle_loops.saturating_add(1);
            // Safety net: periodically reemit queued/pending work if workers appear stuck.
            if idle_loops % 30 == 0 {
                let eng = engine.clone();
                let _ = tokio::task::spawn_blocking(move || reemit_workers(&eng, None)).await;
            }
        }

        if idle_loops % 20 == 0 {
            let eng = engine.clone();
            let _ = tokio::task::spawn_blocking(move || periodic_rebase_reconciler(&eng)).await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    }
}

fn periodic_rebase_reconciler(engine: &Engine) -> anyhow::Result<()> {
    let entities = engine.list_entities()?;
    for base in entities.into_iter().filter(|e| e.kind == "base") {
        let mut payload = parse_payload(&base.payload_json);
        if !payload_auto_rebase_enabled(&payload) {
            continue;
        }
        let Some(repo) = repo_path_from_payload(&payload) else {
            continue;
        };
        let default_branch = detect_default_branch(&repo).unwrap_or_else(|_| "main".to_string());
        let head = git_remote_head_sha(&repo, &default_branch).unwrap_or_default();
        if head.is_empty() {
            continue;
        }
        let last_head = payload
            .get("auto_rebase_last_default_head")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let interval_ms = payload_auto_rebase_interval_sec(&payload) * 1000;
        let last_ms = payload
            .get("auto_rebase_last_reconcile_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let now = now_ms_i64();
        let moved = last_head != head;
        let due = now - last_ms >= interval_ms;
        if moved && due {
            let _ = queue_base_rebase_sweep(engine, &base.id, "periodic.reconciler", Some(&head));
            payload["auto_rebase_last_default_head"] = serde_json::Value::String(head.clone());
            payload["auto_rebase_last_reconcile_ms"] = serde_json::Value::Number(now.into());
            let _ = engine.update_entity_payload(&base.id, &payload.to_string());
        }
    }
    Ok(())
}

fn git_remote_head_sha(repo: &str, branch: &str) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("ls-remote")
        .arg("origin")
        .arg(format!("refs/heads/{branch}"))
        .output()?;
    if !out.status.success() {
        anyhow::bail!("git_ls_remote_failed");
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().next().unwrap_or("");
    Ok(line.split_whitespace().next().unwrap_or("").to_string())
}

#[derive(Debug, Clone)]
struct PendingStep {
    step_row_id: String,
    run_id: String,
    step_id: String,
    agent_id: String,
    task: String,
    context_json: String,
}

fn run_one_step_blocking(engine: &Engine) -> anyhow::Result<bool> {
    let Some(step) = claim_next_step(engine)? else {
        return Ok(false);
    };
    let res = execute_step_blocking(engine, &step);
    match res {
        Ok(out) => finalize_step_done(engine, &step, &out)?,
        Err(e) => finalize_step_failed(engine, &step, &e.to_string())?,
    }
    Ok(true)
}

fn claim_next_step(engine: &Engine) -> anyhow::Result<Option<PendingStep>> {
    let mut conn = engine.open()?;
    let tx = conn.transaction()?;

    // Claim the next runnable step (pending, no earlier steps unfinished, and no step already running for the run).
    let step: Option<PendingStep> = {
        let mut stmt = tx.prepare(
            r#"
SELECT s.id, s.run_id, s.step_id, s.agent_id, r.task, r.context_json
FROM steps s
JOIN runs r ON r.id = s.run_id
WHERE s.status IN ('queued','pending')
  AND r.status IN ('queued','running')
  AND NOT EXISTS (
    SELECT 1 FROM steps s2
    WHERE s2.run_id = s.run_id
      AND s2.step_index < s.step_index
      AND s2.status NOT IN ('done','skipped')
  )
  AND NOT EXISTS (
    SELECT 1 FROM steps s3
    WHERE s3.run_id = s.run_id
      AND s3.status = 'running'
  )
ORDER BY r.created_at ASC, s.step_index ASC
LIMIT 1
"#,
        )?;

        let mut rows = stmt.query([])?;
        let row = rows.next()?;
        match row {
            None => None,
            Some(row) => Some(PendingStep {
                step_row_id: row.get(0)?,
                run_id: row.get(1)?,
                step_id: row.get(2)?,
                agent_id: row.get(3)?,
                task: row.get(4)?,
                context_json: row.get(5)?,
            }),
        }
    };

    let Some(step) = step else {
        tx.commit()?;
        return Ok(None);
    };

    let now = now_rfc3339();
    let updated = tx.execute(
        "UPDATE steps SET status='running', updated_at=?1 WHERE id=?2 AND status IN ('queued','pending')",
        (&now, &step.step_row_id),
    )?;
    if updated == 0 {
        tx.commit()?;
        return Ok(None);
    }
    tx.execute(
        "UPDATE runs SET status='running', updated_at=?1 WHERE id=?2 AND status='queued'",
        (&now, &step.run_id),
    )?;
    tx.execute(
        "INSERT INTO event_log (ts_ms, kind, entity_id, payload_json) VALUES (?1, 'step.running', ?2, ?3)",
        (
            now_ms_i64(),
            &step.step_row_id,
            serde_json::json!({ "run_id": step.run_id, "step_id": step.step_id }).to_string(),
        ),
    )?;
    tx.commit()?;

    Ok(Some(step))
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "now".to_string())
}

fn now_ms_i64() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn has_gh_auth() -> bool {
    let out = Command::new("gh").arg("auth").arg("status").output();
    matches!(out, Ok(o) if o.status.success())
}

fn finalize_step_done(engine: &Engine, step: &PendingStep, out: &str) -> anyhow::Result<()> {
    let mut conn = engine.open()?;
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE steps SET status='done', output_text=?1, updated_at=?2 WHERE id=?3",
        (out, now_rfc3339(), &step.step_row_id),
    )?;
    tx.execute(
        "INSERT INTO event_log (ts_ms, kind, entity_id, payload_json) VALUES (?1, 'step.done', ?2, ?3)",
        (
            now_ms_i64(),
            &step.step_row_id,
            serde_json::json!({ "run_id": step.run_id, "step_id": step.step_id }).to_string(),
        ),
    )?;
    // If all steps done, mark run done.
    let remaining: i64 = tx.query_row(
        "SELECT COUNT(*) FROM steps WHERE run_id=?1 AND status != 'done'",
        [&step.run_id],
        |r| r.get(0),
    )?;
    if remaining == 0 {
        tx.execute(
            "UPDATE runs SET status='done', updated_at=?1 WHERE id=?2",
            (&now_rfc3339(), &step.run_id),
        )?;
        tx.execute(
            "INSERT INTO event_log (ts_ms, kind, entity_id, payload_json) VALUES (?1, 'run.done', ?2, ?3)",
            (
                now_ms_i64(),
                &step.run_id,
                serde_json::json!({ "run_id": step.run_id }).to_string(),
            ),
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn finalize_step_failed(engine: &Engine, step: &PendingStep, err: &str) -> anyhow::Result<()> {
    let mut conn = engine.open()?;
    let tx = conn.transaction()?;
    let now = now_rfc3339();

    tx.execute(
        "UPDATE steps SET status='failed', output_text=?1, updated_at=?2 WHERE id=?3",
        (err, &now, &step.step_row_id),
    )?;

    let mut requeued = false;
    if step.step_id == "test" {
        // Antfarm-like fallback loop: if tests fail, re-open implement->review chain with bounded retries.
        // Guardrail: cap retries to avoid hot loops.
        let attempts: i64 = tx.query_row(
            "SELECT COUNT(*) FROM event_log WHERE kind='run.requeued.test_failed' AND entity_id=?1",
            [&step.run_id],
            |r| r.get(0),
        )?;
        let max_retries = 2_i64;
        if attempts < max_retries {
            tx.execute(
                "UPDATE steps
                 SET status='queued', output_text=NULL, updated_at=?1
                 WHERE run_id=?2 AND step_index >= (
                    SELECT step_index FROM steps WHERE id=?3
                 )",
                (&now, &step.run_id, &step.step_row_id),
            )?;
            tx.execute(
                "UPDATE steps
                 SET status='queued', updated_at=?1
                 WHERE run_id=?2 AND step_id='implement'",
                (&now, &step.run_id),
            )?;
            tx.execute(
                "UPDATE runs SET status='running', updated_at=?1 WHERE id=?2",
                (&now, &step.run_id),
            )?;
            tx.execute(
                "INSERT INTO event_log (ts_ms, kind, entity_id, payload_json) VALUES (?1, 'run.requeued.test_failed', ?2, ?3)",
                (
                    now_ms_i64(),
                    &step.run_id,
                    serde_json::json!({ "run_id": step.run_id, "error": err, "attempt": attempts + 1, "max_attempts": max_retries }).to_string(),
                ),
            )?;
            requeued = true;
        }
    }

    if !requeued {
        tx.execute(
            "UPDATE runs SET status='failed', updated_at=?1 WHERE id=?2",
            (&now, &step.run_id),
        )?;
    }

    tx.execute(
        "INSERT INTO event_log (ts_ms, kind, entity_id, payload_json) VALUES (?1, 'step.failed', ?2, ?3)",
        (
            now_ms_i64(),
            &step.step_row_id,
            serde_json::json!({ "run_id": step.run_id, "step_id": step.step_id, "error": err, "requeued": requeued }).to_string(),
        ),
    )?;
    tx.commit()?;
    Ok(())
}

fn execute_step_blocking(engine: &Engine, step: &PendingStep) -> anyhow::Result<String> {
    let ctx: serde_json::Value =
        serde_json::from_str(&step.context_json).unwrap_or_else(|_| serde_json::json!({}));
    let repo = ctx
        .get("worktree_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let branch = ctx
        .get("branch")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let pr_url = ctx
        .get("pr_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if step.agent_id == "internal/pr" {
        let action = ctx.get("action").and_then(|v| v.as_str()).unwrap_or("");
        if action == "auto_rebase_sweep" {
            return execute_auto_rebase_sweep(engine, step, &ctx);
        }
        let url = create_pr(&repo, &branch, &step.task)?;
        // Persist PR URL into run context for review step.
        let mut conn = engine.open()?;
        let tx = conn.transaction()?;
        let mut v: serde_json::Value =
            serde_json::from_str(&step.context_json).unwrap_or_else(|_| serde_json::json!({}));
        v["pr_url"] = serde_json::Value::String(url.clone());
        tx.execute(
            "UPDATE runs SET context_json=?1, updated_at=?2 WHERE id=?3",
            (v.to_string(), now_rfc3339(), &step.run_id),
        )?;
        tx.commit()?;
        return Ok(url);
    }

    let msg = build_step_message(step, &repo, &branch, &pr_url);
    let out = Command::new("openclaw")
        .arg("agent")
        .arg("--agent")
        .arg(&step.agent_id)
        .arg("--message")
        .arg(msg)
        .arg("--json")
        .arg("--timeout")
        .arg("3600")
        .output()?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "openclaw_failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn build_step_message(step: &PendingStep, repo: &str, branch: &str, pr_url: &str) -> String {
    match step.step_id.as_str() {
        "plan" => format!(
            "TASK:\n{task}\n\nREPO:\n{repo}\n\nBRANCH:\n{branch}\n\nReply with:\nSTATUS: done\nSTORIES_JSON: [{{\"id\":\"s1\",\"title\":\"...\",\"acceptance\":[\"...\"],\"tests\":[\"...\"]}}]\n",
            task = step.task,
            repo = repo,
            branch = branch
        ),
        "setup" => format!(
            "Prepare environment.\n\nTASK:\n{task}\n\nREPO: {repo}\nBRANCH: {branch}\n\nInstructions:\n- cd into repo\n- ensure branch exists and is checked out\n- run build/test baseline\n\nReply with:\nSTATUS: done\nBUILD_CMD: <cmd>\nTEST_CMD: <cmd>\nBASELINE: <status>\n",
            task = step.task,
            repo = repo,
            branch = branch
        ),
        "implement" => format!(
            "Implement the task.\n\nTASK:\n{task}\n\nREPO: {repo}\nBRANCH: {branch}\n\nRequirements:\n- implement\n- add tests\n- run tests\n- commit\n\nReply with:\nSTATUS: done\nCHANGES: ...\nTESTS: ...\n",
            task = step.task,
            repo = repo,
            branch = branch
        ),
        "verify" => format!(
            "Verify the developer work.\n\nTASK:\n{task}\n\nREPO: {repo}\nBRANCH: {branch}\n\nReply with:\nSTATUS: done\nNOTES: ...\n",
            task = step.task,
            repo = repo,
            branch = branch
        ),
        "test" => format!(
            "Integration/E2E testing.\n\nTASK:\n{task}\n\nREPO: {repo}\nBRANCH: {branch}\n\nReply with:\nSTATUS: done\nTEST_RESULTS: ...\n",
            task = step.task,
            repo = repo,
            branch = branch
        ),
        "review" => format!(
            "Review the PR.\n\nTASK:\n{task}\n\nPR: {pr}\n\nReply with:\nSTATUS: done\nREVIEW: ...\n",
            task = step.task,
            pr = pr_url
        ),
        _ => format!("TASK:\n{}\n", step.task),
    }
}

fn create_pr(repo: &str, branch: &str, task: &str) -> anyhow::Result<String> {
    if repo.trim().is_empty() {
        anyhow::bail!("missing_repo: run context has no worktree_path");
    }
    if branch.trim().is_empty() {
        anyhow::bail!("missing_branch: run context has no branch");
    }

    let gh_check = Command::new("gh").arg("--version").output();
    if gh_check.is_err() {
        anyhow::bail!(
            "missing_dependency: gh CLI not installed; install GitHub CLI and run gh auth login"
        );
    }

    let auth = Command::new("gh")
        .arg("auth")
        .arg("status")
        .current_dir(repo)
        .output()?;
    if !auth.status.success() {
        anyhow::bail!(
            "github_auth_required: {}",
            String::from_utf8_lossy(&auth.stderr).trim()
        );
    }

    let remote = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("remote")
        .arg("get-url")
        .arg("origin")
        .output()?;
    if !remote.status.success() {
        anyhow::bail!("git_remote_missing: origin remote is required for PR creation");
    }

    let push = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("push")
        .arg("-u")
        .arg("origin")
        .arg(branch)
        .output()?;
    if !push.status.success() {
        anyhow::bail!(
            "git_push_failed: {}",
            String::from_utf8_lossy(&push.stderr).trim()
        );
    }

    let existing = Command::new("gh")
        .arg("pr")
        .arg("view")
        .arg("--head")
        .arg(branch)
        .arg("--json")
        .arg("url")
        .arg("--jq")
        .arg(".url")
        .current_dir(repo)
        .output()?;
    if existing.status.success() {
        let url = String::from_utf8_lossy(&existing.stdout).trim().to_string();
        if !url.is_empty() {
            return Ok(url);
        }
    }

    let base_out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("symbolic-ref")
        .arg("refs/remotes/origin/HEAD")
        .output()?;
    let base_branch = if base_out.status.success() {
        String::from_utf8_lossy(&base_out.stdout)
            .trim()
            .rsplit('/')
            .next()
            .unwrap_or("main")
            .to_string()
    } else {
        "main".to_string()
    };

    let title = task.lines().next().unwrap_or("Clawdorio run").trim();
    let body = format!("Clawdorio run for:\n\n{}", task);
    let pr = Command::new("gh")
        .arg("pr")
        .arg("create")
        .arg("--head")
        .arg(branch)
        .arg("--base")
        .arg(&base_branch)
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg(body)
        .current_dir(repo)
        .output()?;
    if !pr.status.success() {
        anyhow::bail!(
            "gh_pr_create_failed: {}",
            String::from_utf8_lossy(&pr.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&pr.stdout).trim().to_string())
}

fn execute_auto_rebase_sweep(
    engine: &Engine,
    step: &PendingStep,
    ctx: &serde_json::Value,
) -> anyhow::Result<String> {
    let base_id = ctx
        .get("base_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing_base_id"))?;
    let repo = ctx
        .get("base_repo_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing_base_repo_path"))?;
    let default_branch = ctx
        .get("default_branch")
        .and_then(|v| v.as_str())
        .unwrap_or("main");

    let fetch = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("fetch")
        .arg("origin")
        .output()?;
    if !fetch.status.success() {
        anyhow::bail!(
            "git_fetch_failed: {}",
            String::from_utf8_lossy(&fetch.stderr).trim()
        );
    }

    let pr_list = Command::new("gh")
        .arg("pr")
        .arg("list")
        .arg("--state")
        .arg("open")
        .arg("--json")
        .arg("headRefName")
        .current_dir(repo)
        .output()?;
    if !pr_list.status.success() {
        anyhow::bail!(
            "gh_pr_list_failed: {}",
            String::from_utf8_lossy(&pr_list.stderr).trim()
        );
    }
    let prs: serde_json::Value =
        serde_json::from_slice(&pr_list.stdout).unwrap_or_else(|_| serde_json::json!([]));
    let branches: Vec<String> = prs
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            v.get("headRefName")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .filter(|b| b.starts_with("clawdorio/"))
        .collect();

    let mut ok_branches: Vec<String> = vec![];
    let mut failed: Vec<String> = vec![];

    for branch in branches {
        let checkout = Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("checkout")
            .arg(&branch)
            .output()?;
        if !checkout.status.success() {
            failed.push(format!("{branch}: checkout failed"));
            continue;
        }

        let rebase = Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("rebase")
            .arg(format!("origin/{default_branch}"))
            .output()?;
        if !rebase.status.success() {
            let _ = Command::new("git")
                .arg("-C")
                .arg(repo)
                .arg("rebase")
                .arg("--abort")
                .output();
            failed.push(format!(
                "{branch}: rebase conflict: {}",
                String::from_utf8_lossy(&rebase.stderr).trim()
            ));
            continue;
        }

        let push = Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("push")
            .arg("--force-with-lease")
            .arg("origin")
            .arg(&branch)
            .output()?;
        if !push.status.success() {
            failed.push(format!(
                "{branch}: push failed: {}",
                String::from_utf8_lossy(&push.stderr).trim()
            ));
            continue;
        }
        ok_branches.push(branch);
    }

    let mut conn = engine.open()?;
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO event_log (ts_ms, kind, entity_id, payload_json) VALUES (?1, 'auto_rebase.result', ?2, ?3)",
        (
            now_ms_i64(),
            base_id,
            serde_json::json!({
                "run_id": step.run_id,
                "rebased": ok_branches,
                "failed": failed,
            })
            .to_string(),
        ),
    )?;

    // Bounded retries/backoff for conflicts.
    if !failed.is_empty() {
        let attempts: i64 = tx
            .query_row(
                "SELECT COALESCE(json_extract(context_json, '$.auto_rebase_attempt'), 0) FROM runs WHERE id=?1",
                [&step.run_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if attempts < AUTO_REBASE_MAX_RETRIES {
            let backoff = (attempts + 1) * 30;
            let mut v = ctx.clone();
            v["auto_rebase_attempt"] = serde_json::Value::Number((attempts + 1).into());
            v["auto_rebase_backoff_sec"] = serde_json::Value::Number(backoff.into());
            tx.execute(
                "UPDATE runs SET context_json=?1, updated_at=?2 WHERE id=?3",
                (v.to_string(), now_rfc3339(), &step.run_id),
            )?;
        }
    }
    tx.commit()?;

    if failed.is_empty() {
        Ok("auto-rebase completed".to_string())
    } else {
        anyhow::bail!("needs-attention: {}", failed.join(" | "))
    }
}

fn repair_belt_paths(engine: &Engine) -> anyhow::Result<()> {
    let ents = engine.list_entities()?;
    let belts = engine.list_belts().unwrap_or_default();
    if belts.is_empty() {
        return Ok(());
    }
    let mut conn = engine.open()?;
    let tx = conn.transaction()?;
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    for b in belts {
        let raw = b.path_json.trim();
        if raw != "[]" && !raw.is_empty() {
            continue;
        }
        let Some(a) = ents.iter().find(|e| e.id == b.a_id) else {
            continue;
        };
        let Some(c) = ents.iter().find(|e| e.id == b.b_id) else {
            continue;
        };
        let path = belt_path_cells(&ents, a, c);
        let path_json = serde_json::to_string(&path).unwrap_or_else(|_| "[]".to_string());
        tx.execute(
            "UPDATE belts SET path_json=?1, updated_at_ms=?2, rev=rev+1 WHERE id=?3",
            (&path_json, now, &b.id),
        )?;
        tx.execute(
            "INSERT INTO event_log (ts_ms, kind, entity_id, payload_json) VALUES (?1, ?2, ?3, ?4)",
            (now, "belt.repaired", &b.id, "{}"),
        )?;
    }
    tx.commit()?;
    Ok(())
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
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::PATCH])
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

fn discover_local_repos() -> Vec<LocalRepo> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let workspace_root = PathBuf::from(home).join(".openclaw").join("workspace");
    let roots = vec![workspace_root];

    let skip_dirs: std::collections::HashSet<&'static str> = [
        ".git",
        "node_modules",
        "dist",
        "build",
        ".next",
        "target",
        ".turbo",
        ".cache",
        "coverage",
    ]
    .into_iter()
    .collect();

    let mut repos: Vec<(String, String, u128)> = vec![];
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> = std::collections::VecDeque::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in roots {
        queue.push_back((r, 0));
    }

    let max_depth = 3usize;
    let max_repos = 200usize;

    while let Some((dir, depth)) = queue.pop_front() {
        if repos.len() >= max_repos {
            break;
        }
        let dir_s = dir.to_string_lossy().to_string();
        if dir_s.is_empty() || seen.contains(&dir_s) {
            continue;
        }
        seen.insert(dir_s.clone());

        let git_dir = dir.join(".git");
        // Workspace root may itself be a git repo; still enumerate child repos/worktrees.
        if depth > 0 && git_dir.exists() {
            let name = dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| dir_s.clone());
            let mtime = std::fs::metadata(&dir)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);
            repos.push((dir_s, name, mtime));
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for ent in entries.flatten() {
            let p = ent.path();
            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if !ft.is_dir() {
                continue;
            }
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if skip_dirs.contains(name) {
                    continue;
                }
            }
            queue.push_back((p, depth + 1));
        }
    }

    repos.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    repos
        .into_iter()
        .map(|(path, name, _)| LocalRepo { path, name })
        .collect()
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
      --command-h:240px;
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
    .hud{
      position:absolute;
      left:var(--screen-pad);
      top:var(--screen-pad);
      display:flex;
      gap:10px;
      z-index:60;
      pointer-events:auto;
    }
    .hudbtn{
      width:38px;
      height:38px;
      border:1px solid var(--panel-edge);
      background:#081427cc;
      color:var(--ice);
      font-family:Geist Mono, ui-monospace, SFMono-Regular, Menlo, monospace;
      font-size:13px;
      display:flex;
      align-items:center;
      justify-content:center;
      cursor:pointer;
      box-shadow:0 12px 30px #020c1888;
    }
    .hudbtn:hover{border-color:#8de7ff; box-shadow:0 0 0 1px #95e6ff44 inset, 0 12px 30px #020c1888;}
    .hudnum{font-family:Orbitron,system-ui,sans-serif;font-size:15px;letter-spacing:1px}
    .btn{
      border:1px solid #4f799f;background:#0b1b30;color:var(--ice);
      border-radius:0;padding:8px 10px;font-weight:600;cursor:pointer;
    }
    .btn:hover{border-color:#8de7ff;box-shadow:0 0 0 1px #95e6ff44 inset}

    .dock{
      position:absolute;top:var(--screen-pad);bottom:var(--screen-pad);
      width:var(--dock-w);padding:10px;border:1px solid var(--panel-edge);border-radius:0;
      background:var(--panel);backdrop-filter:blur(10px);
      box-shadow:0 14px 40px #0008;
      overflow:hidden;
      z-index:40;
    }
    .dock.right{right:var(--screen-pad)}
    .dock.is-hidden{display:none}
    .dock h2{font-family:Orbitron,system-ui,sans-serif;font-size:13px;letter-spacing:.6px;margin-bottom:10px}
    .dock .scroll{height:100%;overflow:auto;padding-right:6px}
    /* Custom scrollbar (desktop) for the questbook only. */
    .dock .scroll::-webkit-scrollbar{width:10px}
    .dock .scroll::-webkit-scrollbar-track{background:#040b16;border:1px solid #1b3a57}
    .dock .scroll::-webkit-scrollbar-thumb{background:#0e2a43;border:1px solid #3a7aa3}
    .dock .scroll::-webkit-scrollbar-thumb:hover{background:#123652}
    .dock .scroll{scrollbar-color:#3a7aa3 #040b16; scrollbar-width:thin;}
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
    .commandbar.detail{grid-template-columns: 1fr;}
    .commandbar.detail .palette-wrap{display:none;}
    .commandbar.idle{grid-template-columns: 1fr;}
    .commandbar.idle .bottompanel{display:none;}
    .palette-wrap{
      display:flex;
      flex-direction:column;
      gap:10px;
      min-width:0;
      height:100%;
      max-width:min(860px, 100%);
      margin:0 auto;
    }
    .palette{
      display:grid;
      grid-template-columns:repeat(4, 1fr);
      grid-template-rows:repeat(2, 1fr);
      gap:10px;
      overflow:hidden;
      padding:6px;
      border-radius:0;
      border:0;
      background:#061325aa;
      height:100%;
      align-items:stretch;
      align-content:stretch;
    }
    .palette-card{
      width:auto;
      height:100%;
      flex:0 0 auto;
      border-radius:0;
      border:0;
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
      width:320px;
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
      right:var(--screen-pad);
      top:var(--screen-pad);
      bottom:var(--screen-pad);
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

    .mobile-tabs,.mobile-build-drawer,.mobile-runs{display:none}
    .pr-feed{display:flex;gap:10px;overflow-x:auto;scroll-snap-type:x mandatory;padding:2px}
    .pr-card{min-width:min(88vw,520px);scroll-snap-align:start;border:1px solid #4f799f88;background:#061325dd;padding:10px}
    .pr-card h4{font-size:12px;margin-bottom:6px;font-family:Orbitron,system-ui,sans-serif}
    .pr-files{max-height:180px;overflow:auto;border:1px solid #4f799f55;background:#040b16;padding:8px;margin-top:8px}
    .pr-file{font-size:11px;color:#cfefff;margin-bottom:8px}
    .pr-file pre{white-space:pre-wrap;word-break:break-word;color:#9fd3ff;background:#061325;padding:6px;border:1px solid #28557d}

    /* Small screens: collapse to single column */
    @media (max-width: 980px){
      :root{--dock-w: min(320px, 92vw);--command-h:200px;}
      .hud{top:8px;left:8px;gap:8px}
      .hudbtn{width:44px;height:44px;font-size:14px}
      .viewport{top:0;left:0;right:0;bottom:58px;border-left:0;border-right:0}
      .commandbar{display:none;left:0;right:0;bottom:58px;height:220px;border-left:0;border-right:0;padding:10px}
      .mobile-tabs{position:absolute;left:0;right:0;bottom:0;height:58px;display:grid;grid-template-columns:repeat(4,1fr);z-index:90;border-top:1px solid var(--panel-edge);background:#081427f5}
      .mobile-tabs .tab{border:0;border-right:1px solid #1f4466;background:transparent;color:var(--ice);font-size:12px;font-weight:700;touch-action:manipulation}
      .mobile-tabs .tab:last-child{border-right:0}
      .mobile-tabs .tab.active{background:#0d2a46;color:#6ff8ff}
      .mobile-build-drawer{position:absolute;left:0;right:0;bottom:58px;z-index:88;border-top:1px solid var(--panel-edge);background:#081427f5;padding:8px}
      .mobile-build-title{font-size:11px;color:var(--muted);margin-bottom:6px}
      .palette.mobile{display:flex;overflow-x:auto;gap:8px;height:120px;padding:2px}
      .palette.mobile .palette-card{min-width:120px;height:110px}
      .mobile-runs{position:absolute;left:0;right:0;bottom:58px;top:56px;z-index:70;padding:8px;background:#061325f0;overflow:hidden}
      .dock.right{top:56px;bottom:58px;left:0;right:0;width:auto;border-left:0;border-right:0;z-index:75}
    }
  </style>
</head>
<body>
  <div class="layout">
    <div class="hud">
      <button id="hudAgents" class="hudbtn" type="button" aria-label="Working agents">
        <span id="agentsCount" class="hudnum">0</span>
      </button>
      <button id="hudQuest" class="hudbtn" type="button" aria-label="Questbook">Q</button>
    </div>

    <nav class="mobile-tabs" id="mobileTabs" aria-label="Mobile navigation">
      <button class="tab active" data-tab="base" type="button">Base</button>
      <button class="tab" data-tab="build" type="button">Build</button>
      <button class="tab" data-tab="runs" type="button">Runs/PRs</button>
      <button class="tab" data-tab="quest" type="button">Questbook</button>
    </nav>

    <main class="viewport" id="baseView">
      <canvas id="rtsCanvas"></canvas>
    </main>

    <aside class="dock right is-hidden" id="questbook" aria-label="Questbook">
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

	    <footer class="commandbar" id="commandbar">
	      <section class="palette-wrap" id="paletteWrap">
	        <div class="palette" id="palette" aria-label="Building palette"></div>
	      </section>
	      <section class="bottompanel" id="panel.bottom.bar" aria-label="Selection bottom panel">
	      </section>
	    </footer>
	    <section class="mobile-build-drawer" id="mobileBuildDrawer" aria-label="Build drawer">
	      <div class="mobile-build-title">Build Palette</div>
	      <div class="palette mobile" id="mobilePalette" aria-label="Mobile building palette"></div>
	    </section>
	    <section class="mobile-runs" id="mobileRuns" aria-label="PR feed">
	      <div id="prFeed" class="pr-feed"></div>
	    </section>
	    <div id="baseCreateModal" style="display:none; position:absolute; left:var(--screen-pad); right:var(--screen-pad); bottom:calc(var(--screen-pad) + var(--command-h) + 12px); z-index:80; border:0; background:#081427f0; padding:12px; box-shadow:0 18px 48px #000b;">
	      <div style="display:flex; align-items:center; justify-content:space-between; gap:12px; margin-bottom:10px;">
	        <div style="font-family:Orbitron,system-ui,sans-serif; font-size:12px; letter-spacing:.6px;">Choose Repo For Base</div>
	        <button id="baseCreateCancel" class="btn" type="button">Esc</button>
	      </div>
	      <div style="display:flex; gap:10px; align-items:center;">
	        <select id="baseRepoSelect" style="flex:1; width:100%; border:1px solid #4f799f; background:#0b1b30; color:var(--ice); padding:8px 10px; font-family:Geist Mono, ui-monospace, SFMono-Regular, Menlo, monospace; font-size:12px; appearance:none;"></select>
	        <button id="baseCreatePlace" class="btn" type="button">Place</button>
	      </div>
	    </div>
	  </div>

  <script>
  (async function(){
    const $ = (id) => document.getElementById(id);

    const agentsCountEl = $("agentsCount");
    const hudQuestEl = $("hudQuest");
    const questbookEl = $("questbook");
    const questListEl = $("questList");
    const questTitleEl = $("questTitle");
    const questBodyEl = $("questBody");
    const questKindEl = $("questKind");
    const questStateEl = $("questState");
    const questSaveEl = $("questSave");
    const questNewEl = $("questNew");
    const questDeleteEl = $("questDelete");
	    const paletteEl = $("palette");
	    const mobilePaletteEl = $("mobilePalette");
	    const bottomPanel = $("panel.bottom.bar");
	    const commandbarEl = document.querySelector(".commandbar");
	    const mobileTabsEl = $("mobileTabs");
	    const mobileBuildDrawerEl = $("mobileBuildDrawer");
	    const mobileRunsEl = $("mobileRuns");
	    const prFeedEl = $("prFeed");
	    const baseCreateModalEl = $("baseCreateModal");
	    const baseRepoSelectEl = $("baseRepoSelect");
	    const baseCreatePlaceEl = $("baseCreatePlace");
	    const baseCreateCancelEl = $("baseCreateCancel");

    // Pulled from Antfarm RTS palette/specs via the Rust API, so UI never diverges.
    let BUILDINGS = [];
    let draftKind = null;
    let selected = null;
    let lastRev = 0;
    const featureDraft = new Map();
	    let quests = [];
	    let selectedQuestId = null;
	    let questDirty = false;
	    let localRepos = [];
	    let pendingBasePlacement = null;
	    let belts = [];
	    let selectedBeltId = null;
	    let beltOcc = new Set(); // "x,y" occupied by belt segments (1x1 cells)
	    let mobileTab = "base";
	    let prFeedCards = [];

	    function showBaseModal(show){
	      if (!baseCreateModalEl) return;
	      baseCreateModalEl.style.display = show ? "block" : "none";
	    }

	    function syncBaseRepoSelect(){
	      if (!baseRepoSelectEl) return;
	      baseRepoSelectEl.innerHTML = "";
	      for (const r of localRepos){
	        const opt = document.createElement("option");
	        opt.value = String(r.path || "");
	        opt.textContent = String(r.name || r.path || "");
	        baseRepoSelectEl.appendChild(opt);
	      }
	      if (!localRepos.length){
	        const opt = document.createElement("option");
	        opt.value = "";
	        opt.textContent = "no repos found";
	        baseRepoSelectEl.appendChild(opt);
	      }
	    }

	    async function loadLocalRepos(){
	      try{
	        localRepos = await fetchJson("/api/local-repos");
	      }catch(_e){
	        localRepos = [];
	      }
	      syncBaseRepoSelect();
	    }

	    if (baseCreateCancelEl){
	      baseCreateCancelEl.addEventListener("click", () => {
	        pendingBasePlacement = null;
	        showBaseModal(false);
	        draftKind = null;
	        updatePaletteActive();
	        renderBottomPanel();
	        requestDraw();
	      });
	    }
	    if (baseCreatePlaceEl){
	      baseCreatePlaceEl.addEventListener("click", async () => {
	        const p = pendingBasePlacement;
	        const repo_path = baseRepoSelectEl ? String(baseRepoSelectEl.value || "").trim() : "";
	        if (!p || !repo_path) return;
	        try{
	          await createEntity("base", Number(p.x), Number(p.y), { repo_path });
	          pendingBasePlacement = null;
	          showBaseModal(false);
	        }catch(_e){}
	      });
	    }

    async function loadBuildings(){
      const r = await fetch("/api/buildings", { cache: "no-store" });
      if (!r.ok) throw new Error("buildings_fetch_failed");
      BUILDINGS = await r.json();
      draftKind = null;
    }

    function renderPalette(){
      const mounts = [paletteEl, mobilePaletteEl].filter(Boolean);
      for (const mount of mounts){
        mount.innerHTML = "";
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
	              <span class="tooltip-copy">${esc(b.copy || "")}</span>
	              <span class="tooltip-copy" style="margin-top:6px;">${esc(b.w)}x${esc(b.h)} | ${esc(String(b.hotkey || "").toUpperCase())}</span>
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
          mount.appendChild(btn);
        }
      }
      updatePaletteActive();
    }

    function updatePaletteActive(){
      [paletteEl, mobilePaletteEl].filter(Boolean).forEach((root) => {
        root.querySelectorAll(".palette-card").forEach((el, idx) => {
          const b = BUILDINGS[idx];
          if (!b) return;
          el.classList.toggle("active", draftKind && b.kind === draftKind);
        });
      });
    }

    function esc(s){
      return String(s).replace(/[&<>"]/g, (c) => ({ "&":"&amp;", "<":"&lt;", ">":"&gt;", "\"":"&quot;" }[c]));
    }

    function jsonParse(s){
      try{ return JSON.parse(String(s || "{}")); }catch(_e){ return {}; }
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

    if (hudQuestEl && questbookEl){
      hudQuestEl.addEventListener("click", () => {
        questbookEl.classList.toggle("is-hidden");
      });
    }

    function applyMobileTab(){
      const isMobile = window.matchMedia("(max-width: 980px)").matches;
      if (!isMobile){
        if (mobileBuildDrawerEl) mobileBuildDrawerEl.style.display = "none";
        if (mobileRunsEl) mobileRunsEl.style.display = "none";
        if (questbookEl) questbookEl.classList.add("is-hidden");
        if (commandbarEl) commandbarEl.style.display = "grid";
        return;
      }
      if (commandbarEl) commandbarEl.style.display = (mobileTab === "base") ? "grid" : "none";
      if (mobileBuildDrawerEl) mobileBuildDrawerEl.style.display = (mobileTab === "build") ? "block" : "none";
      if (mobileRunsEl) mobileRunsEl.style.display = (mobileTab === "runs") ? "block" : "none";
      if (questbookEl) questbookEl.classList.toggle("is-hidden", mobileTab !== "quest");
      if (mobileTabsEl){
        mobileTabsEl.querySelectorAll("[data-tab]").forEach((el) => {
          el.classList.toggle("active", el.getAttribute("data-tab") === mobileTab);
        });
      }
    }

    async function refreshPrFeed(){
      if (!selected || selected.kind !== "feature" || !prFeedEl) return;
      const payload = jsonParse(selected.payload_json || "{}");
      const baseId = String(payload.base_id || "");
      try{
        prFeedCards = await fetchJson(`/api/pr-feed?base_id=${encodeURIComponent(baseId)}&limit=30`);
      }catch(_e){ prFeedCards = []; }
      renderPrFeed();
    }

    function renderPrFeed(){
      if (!prFeedEl) return;
      if (!Array.isArray(prFeedCards) || !prFeedCards.length){
        prFeedEl.innerHTML = `<div class="pr-card"><div class="k">No PR runs yet</div></div>`;
        return;
      }
      prFeedEl.innerHTML = prFeedCards.map((c, idx) => {
        const changed = c.changed_files || {};
        const sample = Array.isArray(changed.sample) ? changed.sample : [];
        const files = sample.map((p) => `<div class="pr-file"><div>${esc(p || "")}</div></div>`).join("");
        return `<article class="pr-card" data-run-id="${esc(c.run_id || "")}" data-pr-index="${idx}">
          <h4>${esc(c.title || "PR")}</h4>
          <div class="row"><span>${esc(c.repo || "repo")}</span><span>${esc(c.branch || "branch")}</span></div>
          <div class="row"><span>${esc(c.status || "")}</span><span>${esc(changed.source || "local")}</span></div>
          <div class="chip" style="margin-top:6px;">${esc(c.pr_url || "No PR URL")}</div>
          <div class="k" style="margin-top:6px;">${esc(changed.total_files || 0)} files changed</div>
          <div class="pr-files" data-files-for="${esc(c.run_id || "")}">${files || '<div class="k">No changed files summary.</div>'}</div>
          <button class="btn pr-load-files" type="button" style="margin-top:8px; width:100%; min-height:38px;">Load file snippets</button>
          <textarea class="pr-comment" rows="3" placeholder="Comment + re-emit loop"></textarea>
          <button class="btn pr-comment-send" type="button" style="margin-top:8px; width:100%; min-height:42px;">Comment + Re-emit</button>
        </article>`;
      }).join("");

      prFeedEl.querySelectorAll(".pr-load-files").forEach((btn) => {
        btn.addEventListener("click", async () => {
          const card = btn.closest(".pr-card");
          if (!card) return;
          const runId = String(card.getAttribute("data-run-id") || "");
          const box = card.querySelector(`.pr-files[data-files-for="${runId}"]`);
          if (!runId || !box) return;
          btn.disabled = true;
          try{
            const files = await fetchJson(`/api/pr-feed/${encodeURIComponent(runId)}/files?max_patch_chars=1200`);
            box.innerHTML = (Array.isArray(files) ? files : []).map((f) => `<div class="pr-file"><div>${esc(f.path || "")}</div><div class="k">+${esc(f.additions||0)} -${esc(f.deletions||0)}</div><pre>${esc((f.snippet || "").slice(0, 900))}</pre></div>`).join("") || '<div class="k">No file snippets available.</div>';
          }catch(_e){}
          btn.disabled = false;
        });
      });

      prFeedEl.querySelectorAll(".pr-comment-send").forEach((btn) => {
        btn.addEventListener("click", async () => {
          const card = btn.closest(".pr-card");
          if (!card) return;
          const runId = String(card.getAttribute("data-run-id") || "");
          const ta = card.querySelector(".pr-comment");
          const comment = ta ? String(ta.value || "").trim() : "";
          if (!runId || !comment) return;
          btn.disabled = true;
          try{
            await fetchJson("/api/prs/comment", { method:"POST", headers:{"content-type":"application/json"}, body: JSON.stringify({ run_id: runId, comment }) });
            if (ta) ta.value = "";
          }catch(_e){}
          btn.disabled = false;
        });
      });
    }

    if (mobileTabsEl){
      mobileTabsEl.querySelectorAll("[data-tab]").forEach((el) => {
        el.addEventListener("click", () => {
          mobileTab = String(el.getAttribute("data-tab") || "base");
          if (mobileTab === "runs") refreshPrFeed();
          applyMobileTab();
        });
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
      drag: { active: false, moved: false, start: { sx: 0, sy: 0, wx: 0, wy: 0 }, items: [] },
    };

    let selectedIds = new Set();

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

    function dist1d(a0, a1, b0, b1){
      if (a1 <= b0) return b0 - a1;
      if (b1 <= a0) return a0 - b1;
      return 0;
    }

    function nearAnyBase(x, y, w, h, maxDist){
      const aL = x, aT = y, aR = x + w, aB = y + h;
      for (const e of placed){
        if (!e || e.kind !== "base") continue;
        const bL = e.x, bT = e.y, bR = e.x + (e.w || 1), bB = e.y + (e.h || 1);
        const dx = dist1d(aL, aR, bL, bR);
        const dy = dist1d(aT, aB, bT, bB);
        const d = Math.max(dx, dy);
        if (d <= maxDist) return true;
      }
      return false;
    }

    function entityCoversCell(ent, cx, cy){
      if (!ent) return false;
      const w = Number(ent.w || 1);
      const h = Number(ent.h || 1);
      return cx >= ent.x && cy >= ent.y && cx < (ent.x + w) && cy < (ent.y + h);
    }

	    function hitTestCell(cx, cy){
	      // Use the same depth ordering as draw() so hitboxes match what you see.
	      const list = [...placed].sort((a, b) => {
	        const aw = Math.max(1, Number(a.w || 1));
	        const ah = Math.max(1, Number(a.h || 1));
	        const bw = Math.max(1, Number(b.w || 1));
	        const bh = Math.max(1, Number(b.h || 1));
	        const ak = (Number(a.y || 0) + ah) - Number(a.x || 0);
	        const bk = (Number(b.y || 0) + bh) - Number(b.x || 0);
	        if (ak !== bk) return ak - bk;
	        return Number(a.x || 0) - Number(b.x || 0);
	      });
	      for (let i = list.length - 1; i >= 0; i--){
	        const ent = list[i];
	        if (entityCoversCell(ent, cx, cy)) return ent;
	      }
	      return null;
	    }

	    function beltEndpointWorld(ent){
	      const w = Math.max(1, Number(ent.w || 1));
	      const h = Math.max(1, Number(ent.h || 1));
	      return { x: Number(ent.x) + w*0.5, y: Number(ent.y) + h };
	    }

	    function pointInsideEntity(ent, wx, wy){
	      const w = Math.max(1, Number(ent.w || 1));
	      const h = Math.max(1, Number(ent.h || 1));
	      return wx >= ent.x && wy >= ent.y && wx <= (ent.x + w) && wy <= (ent.y + h);
	    }

	    function beltPolylineWorld(a, b){
	      const pa = beltEndpointWorld(a);
	      const pb = beltEndpointWorld(b);
	      const p1 = [pa, { x: pb.x, y: pa.y }, pb];
	      const p2 = [pa, { x: pa.x, y: pb.y }, pb];

	      const pathBad = (pts) => {
	        for (let i = 0; i < pts.length - 1; i++){
	          const s0 = pts[i], s1 = pts[i+1];
	          const dx = s1.x - s0.x;
	          const dy = s1.y - s0.y;
	          const steps = Math.max(4, Math.ceil(Math.max(Math.abs(dx), Math.abs(dy)) * 4));
	          for (let t = 1; t < steps; t++){
	            const wx = s0.x + dx * (t/steps);
	            const wy = s0.y + dy * (t/steps);
	            for (const e of placed){
	              if (!e) continue;
	              if (e.id === a.id || e.id === b.id) continue;
	              if (pointInsideEntity(e, wx, wy)) return true;
	            }
	          }
	        }
	        return false;
	      };

	      const bad1 = pathBad(p1);
	      const bad2 = pathBad(p2);
	      if (!bad1) return p1;
	      if (!bad2) return p2;
	      return [pa, pb];
	    }

	    function beltPolylineScreen(bt){
	      const a = placed.find((p) => p.id === bt.a_id);
	      const b = placed.find((p) => p.id === bt.b_id);
	      if (!a || !b) return null;
	      // Prefer server-provided occupied cells so visuals match placement blocking.
	      let cells = [];
	      try{ cells = JSON.parse(bt.path_json || "[]"); }catch(_e){ cells = []; }
	      const ptsWorld = [];
	      if (Array.isArray(cells) && cells.length){
	        const pa = beltEndpointWorld(a);
	        ptsWorld.push(pa);
	        for (const c of cells){
	          const x = Number(c && c.x);
	          const y = Number(c && c.y);
	          if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
	          ptsWorld.push({ x: x + 0.5, y: y + 0.5 });
	        }
	        const pb = beltEndpointWorld(b);
	        ptsWorld.push(pb);
	      } else {
	        ptsWorld.push(...beltPolylineWorld(a, b));
	      }
	      return ptsWorld.map((p) => worldToScreen(p.x, p.y));
	    }

	    function pointSegDist(px, py, ax, ay, bx, by){
	      const vx = bx - ax, vy = by - ay;
	      const wx = px - ax, wy = py - ay;
	      const c1 = vx*wx + vy*wy;
	      if (c1 <= 0) return Math.hypot(px - ax, py - ay);
	      const c2 = vx*vx + vy*vy;
	      if (c2 <= c1) return Math.hypot(px - bx, py - by);
	      const t = c1 / c2;
	      const ix = ax + t*vx;
	      const iy = ay + t*vy;
	      return Math.hypot(px - ix, py - iy);
	    }

	    function hitTestBelt(sx, sy){
	      let best = null;
	      let bestD = Infinity;
	      const thresh = 10 * dpr;
	      for (const bt of belts){
	        const pts = beltPolylineScreen(bt);
	        if (!pts || pts.length < 2) continue;
	        for (let i = 0; i < pts.length - 1; i++){
	          const a = pts[i], b = pts[i+1];
	          const d = pointSegDist(sx, sy, a.x, a.y, b.x, b.y);
	          if (d < bestD){
	            bestD = d;
	            best = bt;
	          }
	        }
	      }
	      if (best && bestD <= thresh) return best;
	      return null;
	    }

		    function canPlace(kind, x, y){
		      const fp = footprintFor(kind);
		      // Non-base buildings must connect to an existing base.
		      if (kind !== "base"){
		        if (!nearAnyBase(x, y, fp.w, fp.h, 12)) return false;
		      }
		      // Belts occupy tiles; cannot build over them.
		      for (let dy = 0; dy < fp.h; dy++){
		        for (let dx = 0; dx < fp.w; dx++){
		          const cx = x + dx;
		          const cy = y + dy;
		          if (beltOcc && beltOcc.has(`${cx},${cy}`)) return false;
		        }
		      }
		      // University requires a library (same base) to connect to.
		      if (kind === "university"){
		        const base = nearestBaseForPlacement(x, y, fp.w, fp.h);
		        if (!base) return false;
		        let ok = false;
		        for (const e of placed){
		          if (!e || e.kind !== "library") continue;
		          const p = jsonParse(e.payload_json);
		          if (String(p.base_id || "") === String(base.id || "")){ ok = true; break; }
		        }
		        if (!ok) return false;
		      }
		      for (let dy = 0; dy < fp.h; dy++){
		        for (let dx = 0; dx < fp.w; dx++){
		          const cx = x + dx;
		          const cy = y + dy;
		          if (hitTestCell(cx, cy)) return false;
		        }
		      }
		      return true;
		    }

		    function nearestBaseForPlacement(x, y, w, h){
		      let best = null;
		      let bestD = Infinity;
		      const aL = x, aT = y, aR = x + w, aB = y + h;
		      for (const e of placed){
		        if (!e || e.kind !== "base") continue;
		        const bL = e.x, bT = e.y, bR = e.x + (e.w || 1), bB = e.y + (e.h || 1);
		        const dx = dist1d(aL, aR, bL, bR);
		        const dy = dist1d(aT, aB, bT, bB);
		        const d = Math.max(dx, dy);
		        if (d < bestD){ bestD = d; best = e; }
		      }
		      return best;
		    }

	    function loadImage(src){
	      if (spriteCache.has(src)) return spriteCache.get(src);
	      const img = new Image();
	      img.decoding = "async";
	      img.loading = "eager";
	      img.src = src;
	      const p = img.decode ? img.decode().catch(() => {}) : Promise.resolve();
	      const entry = { img, ready: p, trim: null };
	      spriteCache.set(src, entry);
	      p.then(() => {
	        try{
	          entry.trim = computeTrim(img);
	        }catch(_e){
	          entry.trim = null;
	        }
	        requestDraw();
	      });
	      return entry;
	    }

	    function computeTrim(img){
	      if (!img || !img.naturalWidth || !img.naturalHeight) return null;
	      const w0 = img.naturalWidth;
	      const h0 = img.naturalHeight;
	      // Performance: downscale to a small canvas before scanning alpha bounds.
	      const maxDim = 256;
	      const s = Math.min(1, maxDim / Math.max(w0, h0));
	      const sw = Math.max(1, Math.floor(w0 * s));
	      const sh = Math.max(1, Math.floor(h0 * s));
	      const c = document.createElement("canvas");
	      c.width = sw;
	      c.height = sh;
	      const g = c.getContext("2d", { willReadFrequently: true });
	      if (!g) return null;
	      g.drawImage(img, 0, 0, sw, sh);
	      const data = g.getImageData(0, 0, sw, sh).data;
	      let minX = sw, minY = sh, maxX = -1, maxY = -1;
	      let sumBx = 0;
	      let nBx = 0;
	      for (let y = 0; y < sh; y++){
	        for (let x = 0; x < sw; x++){
	          const a = data[(y*sw + x) * 4 + 3];
	          if (a > 16){
	            if (x < minX) minX = x;
	            if (y < minY) minY = y;
	            if (x > maxX) maxX = x;
	            if (y > maxY) maxY = y;
	          }
	        }
	      }
	      if (maxX < 0 || maxY < 0) return null;
	      // Estimate "feet" anchor: average x of the bottom-most opaque pixels.
	      const by0 = Math.max(0, maxY - 1);
	      for (let y = by0; y <= maxY; y++){
	        for (let x = minX; x <= maxX; x++){
	          const a = data[(y*sw + x) * 4 + 3];
	          if (a > 16){
	            sumBx += x;
	            nBx += 1;
	          }
	        }
	      }
	      const anchorX = nBx ? (sumBx / nBx) : (minX + maxX) * 0.5;
	      const anchorY = maxY;
	      const minX0 = minX / s;
	      const maxX0 = maxX / s;
	      const maxY0 = maxY / s;
	      const ax0 = anchorX / s;
	      const ay0 = anchorY / s;
	      const cx = (minX0 + maxX0) * 0.5;
	      const bottomPad = Math.max(0, (h0 - 1) - maxY0);
	      // ax/ay define where the sprite touches the ground in its own pixels.
	      return { cx, bottomPad, ax: ax0, ay: ay0, w0, h0 };
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
        payload_json: String(e.payload_json || "{}"),
      }));
	      if (selected){
	        selected = placed.find((p) => p.id === selected.id) || null;
	      }
		      belts = Array.isArray(st.belts) ? st.belts.map((b) => ({
		        id: String(b.id),
		        a_id: String(b.a_id),
		        b_id: String(b.b_id),
		        kind: String(b.kind || "link"),
		        path_json: String(b.path_json || "[]"),
		      })) : [];
		      beltOcc = new Set();
		      for (const bt of belts){
		        let cells = [];
		        try{ cells = JSON.parse(bt.path_json || "[]"); }catch(_e){ cells = []; }
		        if (!Array.isArray(cells)) continue;
		        for (const c of cells){
		          const x = Number(c && c.x);
		          const y = Number(c && c.y);
		          if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
		          beltOcc.add(`${x},${y}`);
		        }
		      }
	      // Keep multi-selection stable across refreshes.
	      const alive = new Set(placed.map((p) => p.id));
	      const nextSel = new Set();
	      selectedIds.forEach((id) => { if (alive.has(id)) nextSel.add(id); });
	      selectedIds = nextSel;
	      if (selectedBeltId && !belts.some((b) => b.id === selectedBeltId)) selectedBeltId = null;
	    }

    async function createEntity(kind, x, y, extra){
      const payload = Object.assign({ kind, x, y }, extra && typeof extra === "object" ? extra : {});
      const ent = await fetchJson("/api/entities", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(payload),
      });
      placed = placed.filter((p) => !(p.x === Number(ent.x) && p.y === Number(ent.y)));
      placed.push({
        id: String(ent.id),
        kind: String(ent.kind),
        x: Number(ent.x),
        y: Number(ent.y),
        w: Number(ent.w || 1),
        h: Number(ent.h || 1),
        payload_json: String(ent.payload_json || "{}"),
      });
      selected = placed.find((p) => p.id === String(ent.id)) || null;
      selectedIds = new Set(selected && selected.id ? [selected.id] : []);
      renderBottomPanel();
      // Exit drafting mode after a successful placement.
      draftKind = null;
      updatePaletteActive();
      requestDraw();
      return ent;
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
      selectedIds = new Set();
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

      // Build zones: show the grid only around bases (indicates where one can build).
      const baseZones = placed.filter((e) => e && e.kind === "base");
      if (baseZones.length){
        const s = grid.tile * cam.z;
        const half = s*0.5;
        const quarter = s*0.25;
        const maxDist = 12;
        for (const b of baseZones){
          const bw = Math.max(1, Number(b.w || 1));
          const bh = Math.max(1, Number(b.h || 1));
          const minX = Math.floor(b.x - maxDist);
          const minY = Math.floor(b.y - maxDist);
          const maxX = Math.ceil(b.x + bw + maxDist);
          const maxY = Math.ceil(b.y + bh + maxDist);

          // Outline rectangle bounds.
          const pA = worldToScreen(minX, minY);
          const pB = worldToScreen(maxX, minY);
          const pC = worldToScreen(maxX, maxY);
          const pD = worldToScreen(minX, maxY);
          ctx.beginPath();
          ctx.moveTo(pA.x, pA.y - quarter);
          ctx.lineTo(pB.x + half, pB.y);
          ctx.lineTo(pC.x, pC.y + quarter);
          ctx.lineTo(pD.x - half, pD.y);
          ctx.closePath();
          ctx.fillStyle = "rgba(111,248,255,0.03)";
          ctx.fill();
          ctx.strokeStyle = "rgba(111,248,255,0.22)";
          ctx.lineWidth = 1 * dpr;
          ctx.stroke();

          // Faint diamonds within.
          ctx.strokeStyle = "rgba(127,203,255,0.10)";
          for (let y = minY; y <= maxY; y++){
            for (let x = minX; x <= maxX; x++){
              const p = worldToScreen(x, y);
              ctx.beginPath();
              ctx.moveTo(p.x, p.y - quarter);
              ctx.lineTo(p.x + half, p.y);
              ctx.lineTo(p.x, p.y + quarter);
              ctx.lineTo(p.x - half, p.y);
              ctx.closePath();
              ctx.stroke();
            }
          }
        }
      }

	      // Belts (below structures).
	      if (Array.isArray(belts) && belts.length){
	        for (const bt of belts){
	          const pts = beltPolylineScreen(bt);
	          if (!pts || pts.length < 2) continue;
	          const isSel = selectedBeltId && String(selectedBeltId) === String(bt.id);

	          ctx.save();
	          ctx.lineWidth = (isSel ? 5 : 4) * dpr;
	          ctx.strokeStyle = isSel ? "rgba(111,248,255,0.85)" : "rgba(127,203,255,0.25)";
	          ctx.beginPath();
	          ctx.moveTo(pts[0].x, pts[0].y);
	          for (let i = 1; i < pts.length; i++) ctx.lineTo(pts[i].x, pts[i].y);
	          ctx.stroke();

	          const seg = Math.max(10, 14 * cam.z) * dpr;
	          const step = Math.max(6, 10 * cam.z) * dpr;
	          let carry = (performance.now() * 0.02) % (seg + step);
	          for (let i = 0; i < pts.length - 1; i++){
	            const p0 = pts[i], p1 = pts[i+1];
	            const dx = p1.x - p0.x;
	            const dy = p1.y - p0.y;
	            const len = Math.hypot(dx, dy);
	            if (len < 2) continue;
	            const ux = dx / len;
	            const uy = dy / len;
	            const t0 = carry;
	            for (let t = -t0; t < len; t += (seg + step)){
	              const x0 = p0.x + ux * t;
	              const y0 = p0.y + uy * t;
	              const x1 = p0.x + ux * Math.min(len, t + seg);
	              const y1 = p0.y + uy * Math.min(len, t + seg);
	              ctx.lineWidth = 2 * dpr;
	              ctx.strokeStyle = isSel ? "rgba(111,248,255,0.9)" : "rgba(111,248,255,0.30)";
	              ctx.beginPath();
	              ctx.moveTo(x0, y0);
	              ctx.lineTo(x1, y1);
	              ctx.stroke();
	            }
	            carry = (t0 + len) % (seg + step);
	          }

	          const end0 = pts[pts.length - 2];
	          const end1 = pts[pts.length - 1];
	          const edx = end1.x - end0.x;
	          const edy = end1.y - end0.y;
	          const elen = Math.hypot(edx, edy);
	          if (elen < 2){ ctx.restore(); continue; }
	          const eux = edx / elen;
	          const euy = edy / elen;
	          const ax = end1.x - eux * (10 * cam.z) * dpr;
	          const ay = end1.y - euy * (10 * cam.z) * dpr;
	          ctx.fillStyle = isSel ? "rgba(111,248,255,0.9)" : "rgba(127,203,255,0.55)";
	          ctx.beginPath();
	          ctx.moveTo(end1.x, end1.y);
	          ctx.lineTo(ax + (-euy) * (6 * cam.z) * dpr, ay + (eux) * (6 * cam.z) * dpr);
	          ctx.lineTo(ax + (euy) * (6 * cam.z) * dpr, ay + (-eux) * (6 * cam.z) * dpr);
	          ctx.closePath();
	          ctx.fill();
	          ctx.restore();
	        }
	      }

      const drawList = [...placed].sort((a, b) => {
        const aw = Math.max(1, Number(a.w || 1));
        const ah = Math.max(1, Number(a.h || 1));
        const bw = Math.max(1, Number(b.w || 1));
        const bh = Math.max(1, Number(b.h || 1));
        const ak = (Number(a.y || 0) + ah) - Number(a.x || 0);
        const bk = (Number(b.y || 0) + bh) - Number(b.x || 0);
        if (ak !== bk) return ak - bk;
        return Number(a.x || 0) - Number(b.x || 0);
      });

      // Placed buildings: outline footprint + floating sprite (cached).
      for (const b of drawList){
        const p = worldToScreen(b.x, b.y);
        const s = grid.tile * cam.z;
        const half = s*0.5;
        const quarter = s*0.25;

        const isSel = selectedIds.has(b.id) || (selected && selected.id === b.id);
        const bw = Math.max(1, Number(b.w || 1));
        const bh = Math.max(1, Number(b.h || 1));

        const pA = worldToScreen(b.x, b.y);
        const pB = worldToScreen(b.x + bw, b.y);
        const pC = worldToScreen(b.x + bw, b.y + bh);
        const pD = worldToScreen(b.x, b.y + bh);
        ctx.beginPath();
        ctx.moveTo(pA.x, pA.y - quarter);
        ctx.lineTo(pB.x + half, pB.y);
        ctx.lineTo(pC.x, pC.y + quarter);
        ctx.lineTo(pD.x - half, pD.y);
        ctx.closePath();
        ctx.fillStyle = isSel ? "rgba(111,248,255,0.10)" : "rgba(111,248,255,0.04)";
        ctx.fill();
        ctx.strokeStyle = isSel ? "rgba(111,248,255,0.85)" : "rgba(111,248,255,0.28)";
        ctx.stroke();

	        const spec = buildingSpec(b.kind);
	        if (spec){
	          const e = loadImage(spec.sprite);
	          if (e.img && e.img.complete && e.img.naturalWidth > 0){
		            const targetW = Math.max(140, (b.kind === "base" ? 420 : 170) * cam.z);
	            const scale = targetW / e.img.naturalWidth;
	            const dw = e.img.naturalWidth * scale;
	            const dh = e.img.naturalHeight * scale;
            // Sprite.
            const pc = worldToScreen(b.x + bw*0.5, b.y + bh);
            const trim = e.trim;
            const ax = trim ? Number(trim.ax || (e.img.naturalWidth * 0.5)) : (e.img.naturalWidth * 0.5);
            const ay = trim ? Number(trim.ay || (e.img.naturalHeight - 1)) : (e.img.naturalHeight - 1);
            const shiftX = (e.img.naturalWidth * 0.5 - ax) * scale;
            const shiftY = (e.img.naturalHeight - 1 - ay) * scale;
            ctx.drawImage(e.img, pc.x - dw/2 + shiftX, pc.y - dh - 10*cam.z + shiftY, dw, dh);
		          }else{
		            // If sprites aren't ready yet, keep the world quiet (no placeholder text).
		          }
	        }
	      }

	      // Draft overlay.
	      if (pendingBasePlacement){
	        const kind = "base";
	        const fp = footprintFor(kind);
	        const valid = canPlace(kind, pendingBasePlacement.x, pendingBasePlacement.y);
	        const s = grid.tile * cam.z;
	        const half = s*0.5;
	        const quarter = s*0.25;

	        const pA = worldToScreen(pendingBasePlacement.x, pendingBasePlacement.y);
	        const pB = worldToScreen(pendingBasePlacement.x + fp.w, pendingBasePlacement.y);
	        const pC = worldToScreen(pendingBasePlacement.x + fp.w, pendingBasePlacement.y + fp.h);
	        const pD = worldToScreen(pendingBasePlacement.x, pendingBasePlacement.y + fp.h);
	        ctx.beginPath();
	        ctx.moveTo(pA.x, pA.y - quarter);
	        ctx.lineTo(pB.x + half, pB.y);
	        ctx.lineTo(pC.x, pC.y + quarter);
	        ctx.lineTo(pD.x - half, pD.y);
	        ctx.closePath();
	        ctx.fillStyle = valid ? "rgba(255,208,107,0.06)" : "rgba(255,113,152,0.05)";
	        ctx.fill();
	        ctx.strokeStyle = valid ? "rgba(255,208,107,0.75)" : "rgba(255,113,152,0.75)";
	        ctx.stroke();

	        const spec = buildingSpec(kind);
	        if (spec){
	          const e = loadImage(spec.sprite);
	          if (e.img && e.img.complete && e.img.naturalWidth > 0){
	            const targetW = Math.max(140, 420 * cam.z);
	            const scale = targetW / e.img.naturalWidth;
	            const dw = e.img.naturalWidth * scale;
	            const dh = e.img.naturalHeight * scale;
	            const p0 = worldToScreen(pendingBasePlacement.x + fp.w*0.5, pendingBasePlacement.y + fp.h*0.5);
	            const trim = e.trim;
	            const ax = trim ? Number(trim.ax || (e.img.naturalWidth * 0.5)) : (e.img.naturalWidth * 0.5);
	            const ay = trim ? Number(trim.ay || (e.img.naturalHeight - 1)) : (e.img.naturalHeight - 1);
	            const shiftX = (e.img.naturalWidth * 0.5 - ax) * scale;
	            const shiftY = (e.img.naturalHeight - 1 - ay) * scale;
	            ctx.save();
	            ctx.globalAlpha = valid ? 0.45 : 0.18;
	            ctx.drawImage(e.img, p0.x - dw/2 + shiftX, p0.y - dh - 10*cam.z + shiftY, dw, dh);
	            ctx.restore();
	          }
	        }
	      }

	      if (draftKind && state.hover){
	        const kind = draftKind;
	        const fp = footprintFor(kind);
	        const valid = canPlace(kind, state.hover.x, state.hover.y);
	        const s = grid.tile * cam.z;
	        const half = s*0.5;
	        const quarter = s*0.25;

        // Outline only (no per-cell highlights).
        const pA = worldToScreen(state.hover.x, state.hover.y);
        const pB = worldToScreen(state.hover.x + fp.w, state.hover.y);
        const pC = worldToScreen(state.hover.x + fp.w, state.hover.y + fp.h);
        const pD = worldToScreen(state.hover.x, state.hover.y + fp.h);
        ctx.beginPath();
        ctx.moveTo(pA.x, pA.y - quarter);
        ctx.lineTo(pB.x + half, pB.y);
        ctx.lineTo(pC.x, pC.y + quarter);
        ctx.lineTo(pD.x - half, pD.y);
        ctx.closePath();
        ctx.fillStyle = valid ? "rgba(255,208,107,0.06)" : "rgba(255,113,152,0.05)";
        ctx.fill();
        ctx.strokeStyle = valid ? "rgba(255,208,107,0.75)" : "rgba(255,113,152,0.75)";
        ctx.stroke();

        // Draft sprite (transparent + floating).
        const spec = buildingSpec(kind);
	        if (spec){
	          const e = loadImage(spec.sprite);
	          if (e.img && e.img.complete && e.img.naturalWidth > 0){
	            const targetW = Math.max(140, (kind === "base" ? 420 : 170) * cam.z);
	            const scale = targetW / e.img.naturalWidth;
	            const dw = e.img.naturalWidth * scale;
	            const dh = e.img.naturalHeight * scale;
	            const p0 = worldToScreen(state.hover.x + fp.w*0.5, state.hover.y + fp.h*0.5);

            const trim = e.trim;
            const ax = trim ? Number(trim.ax || (e.img.naturalWidth * 0.5)) : (e.img.naturalWidth * 0.5);
            const ay = trim ? Number(trim.ay || (e.img.naturalHeight - 1)) : (e.img.naturalHeight - 1);
            const shiftX = (e.img.naturalWidth * 0.5 - ax) * scale;
            const shiftY = (e.img.naturalHeight - 1 - ay) * scale;
		            ctx.save();
		            ctx.globalAlpha = valid ? 0.45 : 0.18;
		            ctx.drawImage(e.img, p0.x - dw/2 + shiftX, p0.y - dh - 10*cam.z + shiftY, dw, dh);
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
      if (draftKind){
        const fp = footprintFor(draftKind);
        const valid = canPlace(draftKind, gx, gy);
        state.hover = { x: gx, y: gy, kind: draftKind, w: fp.w, h: fp.h, valid };
      } else {
        state.hover = { x: gx, y: gy };
      }
    }

    canvas.addEventListener("mousemove", (e) => {
      state.mouse.x = e.clientX;
      state.mouse.y = e.clientY;
      if (state.drag.active){
        const r = canvas.getBoundingClientRect();
        const sx = (e.clientX - r.left) * dpr;
        const sy = (e.clientY - r.top) * dpr;
        const cur = screenToWorld(sx, sy);
        const dx = Math.round(cur.wx - state.drag.start.wx);
        const dy = Math.round(cur.wy - state.drag.start.wy);
        const moved = Math.abs(dx) + Math.abs(dy) > 0;
        state.drag.moved = state.drag.moved || moved;
        if (moved){
          for (const it of state.drag.items){
            const ent = placed.find((p) => p.id === it.id);
            if (!ent) continue;
            ent.x = it.x0 + dx;
            ent.y = it.y0 + dy;
          }
          requestDraw();
          return;
        }
      }
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
      if (e.button === 2){
        e.preventDefault();
        state.isPanning = true;
        state.panStart.x = e.clientX;
        state.panStart.y = e.clientY;
        state.panStart.camx = cam.x;
        state.panStart.camy = cam.y;
        return;
      }
      if (e.button !== 0) return;
      if (draftKind) return;
      updateHover(e.clientX, e.clientY);
      const hit = state.hover ? hitTestCell(state.hover.x, state.hover.y) : null;
      if (hit && selectedIds.has(hit.id)){
        const r = canvas.getBoundingClientRect();
        const sx = (e.clientX - r.left) * dpr;
        const sy = (e.clientY - r.top) * dpr;
        const start = screenToWorld(sx, sy);
        state.drag.active = true;
        state.drag.moved = false;
        state.drag.start = { sx: e.clientX, sy: e.clientY, wx: start.wx, wy: start.wy };
        const items = [];
        for (const id of selectedIds){
          const ent = placed.find((p) => p.id === id);
          if (!ent) continue;
          items.push({ id, x0: Number(ent.x || 0), y0: Number(ent.y || 0) });
        }
        // Always drag at least the clicked entity (even if selection got desynced).
        if (!items.some((it) => it.id === hit.id)) items.push({ id: hit.id, x0: Number(hit.x || 0), y0: Number(hit.y || 0) });
        state.drag.items = items;
        return;
      }
    });
    window.addEventListener("mouseup", async () => {
      state.isPanning = false;
      if (!state.drag.active) return;
      const moved = !!state.drag.moved;
      const items = Array.isArray(state.drag.items) ? state.drag.items : [];
      state.drag.active = false;
      state.drag.items = [];
      state.drag.moved = false;
      if (!moved) return;
      // Persist moved entities (server is authoritative, enforces overlaps/base proximity).
      for (const it of items){
        const ent = placed.find((p) => p.id === it.id);
        if (!ent) continue;
        try{
          await fetchJson(`/api/entities/${encodeURIComponent(String(ent.id))}`, {
            method: "PATCH",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ x: Number(ent.x), y: Number(ent.y) }),
          });
        }catch(_e){}
      }
      try{
        const st = await fetchJson("/api/state");
        if (agentsCountEl) agentsCountEl.textContent = String(st.working_agents || 0);
        quests = Array.isArray(st.quests) ? st.quests : [];
        renderQuestList();
        syncQuestEditor();
        applyState(st);
        renderBottomPanel();
        requestDraw();
      }catch(_e){}
    });
    canvas.addEventListener("contextmenu", (e) => e.preventDefault());

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
      if (baseCreateModalEl && baseCreateModalEl.style.display !== "none") return;
      if (state.drag && state.drag.active) return;
      if (state.drag && state.drag.moved) return;

      // Place a building (draft) via the API (DB is source of truth).
      if (draftKind && canPlace(draftKind, state.hover.x, state.hover.y)){
        if (draftKind === "base"){
          pendingBasePlacement = { x: state.hover.x, y: state.hover.y };
          showBaseModal(true);
          if (baseRepoSelectEl) baseRepoSelectEl.focus();
          // Freeze the placement ghost at the clicked cell.
          draftKind = null;
          updatePaletteActive();
        } else {
          createEntity(draftKind, state.hover.x, state.hover.y).catch(() => {});
        }
        return;
      }

      const hit = hitTestCell(state.hover.x, state.hover.y);
      if (hit){
        selectedBeltId = null;
        selected = hit;
        selectedIds.add(hit.id);
        renderBottomPanel();
        requestDraw();
        return;
      }

      // Belt selection (click near a belt line).
      const r = canvas.getBoundingClientRect();
      const sx = (e.clientX - r.left) * dpr;
      const sy = (e.clientY - r.top) * dpr;
      const bt = hitTestBelt(sx, sy);
      if (bt){
        selected = null;
        selectedIds = new Set();
        selectedBeltId = bt.id;
        renderBottomPanel();
        requestDraw();
        return;
      }

      // Empty space clears selection.
      selected = null;
      selectedIds = new Set();
      selectedBeltId = null;
      renderBottomPanel();
      requestDraw();
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
      if (kind === "base"){
        pendingBasePlacement = { x: state.hover.x, y: state.hover.y };
        showBaseModal(true);
        if (baseRepoSelectEl) baseRepoSelectEl.focus();
        return;
      }
      createEntity(kind, state.hover.x, state.hover.y).catch(() => {});
    });

    window.addEventListener("resize", () => { resize(); applyMobileTab(); });
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
        if (baseCreateModalEl && baseCreateModalEl.style.display !== "none"){
          pendingBasePlacement = null;
          showBaseModal(false);
          draftKind = null;
          updatePaletteActive();
          renderBottomPanel();
          requestDraw();
          return;
        }
        if (draftKind){
          draftKind = null;
          updatePaletteActive();
          requestDraw();
        } else {
          selected = null;
          selectedIds = new Set();
          selectedBeltId = null;
          renderBottomPanel();
          requestDraw();
        }
        return;
      }
      if (up === "Delete"){
        if (selectedBeltId){
          const id = selectedBeltId;
          selectedBeltId = null;
          fetchJson(`/api/belts/${encodeURIComponent(String(id))}`, { method: "DELETE" })
            .then(() => fetchJson("/api/state"))
            .then((st) => { applyState(st); renderBottomPanel(); requestDraw(); })
            .catch(() => {});
          return;
        }
        const ids = [...selectedIds];
        if (selected && selected.id && !ids.includes(selected.id)) ids.push(selected.id);
        ids.forEach((id) => deleteEntityById(id).catch(() => {}));
        return;
      }

      if (up === "Q"){
        if (questbookEl) questbookEl.classList.toggle("is-hidden");
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
      const hasDetail = !!selected || !!selectedBeltId;
      if (commandbarEl) commandbarEl.classList.toggle("detail", hasDetail);
      if (commandbarEl) commandbarEl.classList.toggle("idle", !hasDetail);
      if (!selected && !selectedBeltId){
        bottomPanel.innerHTML = "";
        if (prFeedEl) prFeedEl.innerHTML = "";
        return;
      }

      if (!selected && selectedBeltId){
        const bt = belts.find((b) => String(b.id) === String(selectedBeltId)) || null;
        bottomPanel.innerHTML = `
          <div style="display:flex; align-items:center; justify-content:space-between; gap:10px; margin-bottom:10px;">
            <h3>Belt</h3>
            <button id="beltDeleteBtn" class="btn" type="button">Delete</button>
          </div>
          <div class="row"><span>${esc(bt ? bt.id : "")}</span><span>${esc(bt ? (bt.a_id + " -> " + bt.b_id) : "")}</span></div>
        `;
        const delBtn = bottomPanel.querySelector("#beltDeleteBtn");
        if (delBtn && bt){
          delBtn.addEventListener("click", async () => {
            try{
              await fetchJson(`/api/belts/${encodeURIComponent(String(bt.id))}`, { method: "DELETE" });
              const st = await fetchJson("/api/state");
              applyState(st);
              renderBottomPanel();
              requestDraw();
            }catch(_e){}
          });
        }
        return;
      }

      const spec = buildingSpec(selected.kind);
      const title = spec ? spec.title : selected.kind;
      const payload = jsonParse(selected.payload_json);

      bottomPanel.innerHTML = `
        <div style="display:flex; align-items:center; justify-content:space-between; gap:10px; margin-bottom:10px;">
          <h3>${esc(title)}</h3>
          <button id="entityDeleteBtn" class="btn" type="button">Delete</button>
        </div>
        <div class="row"><span>${esc(selected.id || "")}</span><span>${esc(selected.x)},${esc(selected.y)} ${esc(selected.w || 1)}x${esc(selected.h || 1)}</span></div>
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

      if (selected.kind === "base"){
        const repo = String(payload.repo_path || "");
        body.innerHTML = `
          <div style="display:flex; gap:10px; align-items:center;">
            <div style="flex:1; width:100%; border:1px solid #4f799f; background:#0b1b30; color:var(--ice); padding:8px 10px; font-family:Geist Mono, ui-monospace, SFMono-Regular, Menlo, monospace; font-size:12px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;">${esc(repo)}</div>
          </div>
        `;
        return;
      }

      if (selected.kind !== "feature"){
        const baseId = String(payload.base_id || "");
        body.innerHTML = baseId ? `<div class="row"><span>${esc(baseId)}</span><span></span></div>` : "";
        return;
      }

      const key = String(selected.id || "");
      const prev = featureDraft.get(key) || "";
      refreshPrFeed();
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
      let activeStepRowId = null;

      function stepLabel(s){
        const m = {
          plan: "Plan",
          setup: "Setup",
          implement: "Dev",
          verify: "Verify",
          test: "Test",
          pr: "PR",
          review: "Review",
        };
        return m[String(s.step_id || "")] || String(s.step_id || "");
      }

      function renderKanban(run, steps){
        if (!runsEl) return;
        const cols = 7;
        const cards = (Array.isArray(steps) ? steps : []).map((s) => {
          const st = String(s.status || "");
          const isRun = st === "running";
          const isDone = st === "done";
          const isFail = st === "failed";
          const cls = isRun ? "chip" : (isDone ? "chip" : (isFail ? "chip" : "chip"));
          const title = stepLabel(s);
          const agent = String(s.agent_id || "");
          const small = agent.includes("/") ? agent.split("/").slice(-1)[0] : agent;
          const act = activeStepRowId && String(activeStepRowId) === String(s.id) ? " style=\"outline:1px solid #6ff8ff55;\"" : "";
          return `<div class="col" data-step="${esc(String(s.id))}" ${act}>
            <h4>${esc(title)}</h4>
            <div class="${cls}" style="margin-bottom:8px;">${esc(st)}</div>
            <div class="k" style="font-size:10px;color:var(--muted);">${esc(small)}</div>
          </div>`;
        }).join("");

        const prStep = (Array.isArray(steps) ? steps : []).find((s) => String(s.step_id) === "pr" && String(s.status) === "done") || null;
        const prUrl = prStep && prStep.output_text ? String(prStep.output_text).trim() : "";
        const prLine = prUrl ? `<div class="chip" style="margin-top:10px;">PR ${esc(prUrl)}</div>` : "";

        runsEl.innerHTML = `
          <div class="row"><span>${esc(run.status || "")}</span><span>${esc(run.id || "")}</span></div>
          <div class="kanban" style="grid-template-columns:repeat(${cols},1fr); margin-top:10px;">${cards}</div>
          <div id="stepOut" style="margin-top:10px;"></div>
          ${prLine}
        `;

        const outEl = runsEl.querySelector("#stepOut");
        if (outEl){
          const s = (Array.isArray(steps) ? steps : []).find((x) => activeStepRowId && String(x.id) === String(activeStepRowId)) || null;
          const txt = s && s.output_text ? String(s.output_text) : "";
          outEl.innerHTML = txt ? `<pre style="white-space:pre-wrap; word-break:break-word; border:1px solid #4f799f55; background:#040b16; padding:10px; font-size:11px; color:#cfefff; max-height:240px; overflow:auto;">${esc(txt.slice(0, 12000))}</pre>` : "";
        }

        runsEl.querySelectorAll("[data-step]").forEach((el) => {
          el.addEventListener("click", async () => {
            activeStepRowId = el.getAttribute("data-step");
            renderKanban(run, steps);
          });
        });
      }

      async function refreshRuns(){
        if (!runsEl) return;
        try{
          const runs = await fetchJson(`/api/runs?entity_id=${encodeURIComponent(key)}`);
          if (!Array.isArray(runs) || !runs.length){
            runsEl.innerHTML = "";
            return;
          }
          const run = runs[0];
          const steps = await fetchJson(`/api/runs/${encodeURIComponent(String(run.id))}/steps`);
          // Default output panel to the running step.
          if (!activeStepRowId){
            const running = Array.isArray(steps) ? steps.find((s) => String(s.status) === "running") : null;
            if (running) activeStepRowId = String(running.id);
          }
          renderKanban(run, steps);
        }catch(_e){
          runsEl.innerHTML = "";
        }
      }

      refreshRuns();
      // Keep the kanban in sync while this panel is open.
      const poll = setInterval(() => { refreshRuns(); }, 1100);
      // Tear down poll if the panel gets replaced.
      const mo = new MutationObserver(() => {
        if (!document.body.contains(runsEl)){
          clearInterval(poll);
          mo.disconnect();
        }
      });
      mo.observe(document.body, { childList: true, subtree: true });

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
      await loadLocalRepos();
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
    applyMobileTab();
    renderBottomPanel();
    requestDraw();
    wireQuestEditor();
    stateLoop();
  })();
  </script>
</body>
</html>
"###;
