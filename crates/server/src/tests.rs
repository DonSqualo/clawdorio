use super::*;
use axum::http::{HeaderMap, HeaderValue};
use axum::Json;
use std::sync::Arc;

fn temp_engine() -> Engine {
    let p = std::env::temp_dir().join(format!(
        "clawdorio-server-test-{}.db",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let engine = Engine::new(p);
    let _ = engine.open().expect("open db");
    engine
}

fn seed_run(engine: &Engine, run_id: &str, entity_id: &str, run_status: &str) {
    let conn = engine.open().unwrap();
    conn.execute(
        "INSERT INTO runs (id, workflow_id, task, status, entity_id, context_json, created_at, updated_at)
         VALUES (?1, 'wf', 'task', ?2, ?3, '{}', ?4, ?4)",
        (run_id, run_status, entity_id, now_rfc3339()),
    )
    .unwrap();
}

fn seed_step(engine: &Engine, id: &str, run_id: &str, step_id: &str, idx: i64, status: &str) {
    let conn = engine.open().unwrap();
    conn.execute(
        "INSERT INTO steps (id, run_id, step_id, agent_id, step_index, status, input_json, output_text, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'a', ?4, ?5, '{}', NULL, ?6, ?6)",
        (id, run_id, step_id, idx, status, now_rfc3339()),
    )
    .unwrap();
}

#[test]
fn claim_promotes_run_from_queued() {
    let engine = temp_engine();
    seed_run(&engine, "r1", "e1", "queued");
    seed_step(&engine, "s1", "r1", "plan", 0, "queued");

    let claimed = claim_next_step(&engine).unwrap().expect("claimed");
    assert_eq!(claimed.step_row_id, "s1");

    let conn = engine.open().unwrap();
    let run_status: String = conn
        .query_row("SELECT status FROM runs WHERE id='r1'", [], |r| r.get(0))
        .unwrap();
    let step_status: String = conn
        .query_row("SELECT status FROM steps WHERE id='s1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(run_status, "running");
    assert_eq!(step_status, "running");
}

#[test]
fn test_failure_requeues_with_guardrail() {
    let engine = temp_engine();
    seed_run(&engine, "r2", "e1", "running");
    seed_step(&engine, "s-plan", "r2", "plan", 0, "done");
    seed_step(&engine, "s-impl", "r2", "implement", 1, "done");
    seed_step(&engine, "s-test", "r2", "test", 2, "running");
    seed_step(&engine, "s-pr", "r2", "pr", 3, "queued");

    let pending = PendingStep {
        step_row_id: "s-test".to_string(),
        run_id: "r2".to_string(),
        step_id: "test".to_string(),
        agent_id: "feature-dev/tester".to_string(),
        task: "task".to_string(),
        context_json: "{}".to_string(),
    };

    finalize_step_failed(&engine, &pending, "boom").unwrap();

    let conn = engine.open().unwrap();
    let run_status: String = conn
        .query_row("SELECT status FROM runs WHERE id='r2'", [], |r| r.get(0))
        .unwrap();
    let impl_status: String = conn
        .query_row("SELECT status FROM steps WHERE id='s-impl'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let test_status: String = conn
        .query_row("SELECT status FROM steps WHERE id='s-test'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(run_status, "running");
    assert_eq!(impl_status, "queued");
    assert_eq!(test_status, "queued");
}

#[test]
fn reemit_workers_scoped_to_base() {
    let engine = temp_engine();
    let conn = engine.open().unwrap();
    conn.execute(
        "INSERT INTO entities (id, kind, x, y, w, h, payload_json, created_at_ms, updated_at_ms, rev)
         VALUES (?1,'feature',0,0,3,4,?2,1,1,1)",
        ("f1", r#"{"base_id":"b1"}"#),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO entities (id, kind, x, y, w, h, payload_json, created_at_ms, updated_at_ms, rev)
         VALUES (?1,'feature',0,0,3,4,?2,1,1,1)",
        ("f2", r#"{"base_id":"b2"}"#),
    )
    .unwrap();

    seed_run(&engine, "ra", "f1", "running");
    seed_run(&engine, "rb", "f2", "running");
    seed_step(&engine, "sa", "ra", "plan", 0, "running");
    seed_step(&engine, "sb", "rb", "plan", 0, "running");

    let report = reemit_workers(&engine, Some("b1")).unwrap();
    assert_eq!(report.scanned_runs, 1);

    let sa: String = conn
        .query_row("SELECT status FROM steps WHERE id='sa'", [], |r| r.get(0))
        .unwrap();
    let sb: String = conn
        .query_row("SELECT status FROM steps WHERE id='sb'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sa, "queued");
    assert_eq!(sb, "running");
}

fn init_git_repo() -> std::path::PathBuf {
    let repo = std::env::temp_dir().join(format!(
        "clawdorio-server-git-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    std::fs::create_dir_all(&repo).unwrap();
    std::process::Command::new("git")
        .arg("init")
        .arg("-b")
        .arg("main")
        .current_dir(&repo)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("config")
        .arg("user.email")
        .arg("test@example.com")
        .current_dir(&repo)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("config")
        .arg("user.name")
        .arg("Test")
        .current_dir(&repo)
        .output()
        .unwrap();
    std::fs::write(
        repo.join("README.md"),
        "x
",
    )
    .unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&repo)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&repo)
        .output()
        .unwrap();

    let bare = repo.with_extension("origin.git");
    std::process::Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(&bare)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["remote", "add", "origin", bare.to_string_lossy().as_ref()])
        .current_dir(&repo)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["push", "-u", "origin", "main"])
        .current_dir(&repo)
        .output()
        .unwrap();

    repo
}

#[test]
fn sync_now_queues_once_idempotent() {
    let engine = temp_engine();
    let repo = init_git_repo();
    engine
        .create_entity_with_payload(
            "base",
            0,
            0,
            9,
            9,
            &serde_json::json!({
                "repo_path": repo.to_string_lossy().to_string(),
                "auto_rebase_enabled": true,
                "auto_rebase_interval_sec": 120,
            })
            .to_string(),
        )
        .unwrap();
    let base = engine
        .list_entities()
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "base")
        .unwrap();

    assert!(queue_base_rebase_sweep(&engine, &base.id, "test", None).unwrap());
    assert!(!queue_base_rebase_sweep(&engine, &base.id, "test", None).unwrap());

    let conn = engine.open().unwrap();
    let c: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE workflow_id='auto-rebase' AND entity_id=?1",
            [&base.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(c, 1);
}

#[test]
fn periodic_reconciler_skips_when_disabled() {
    let engine = temp_engine();
    let repo = init_git_repo();
    let _ = engine
        .create_entity_with_payload(
            "base",
            0,
            0,
            9,
            9,
            &serde_json::json!({
                "repo_path": repo.to_string_lossy().to_string(),
                "auto_rebase_enabled": false,
                "auto_rebase_interval_sec": 30,
            })
            .to_string(),
        )
        .unwrap();

    periodic_rebase_reconciler(&engine).unwrap();
    let conn = engine.open().unwrap();
    let c: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE workflow_id='auto-rebase'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(c, 0);
}

#[tokio::test]
async fn manual_sync_handler_queues_run() {
    let engine = temp_engine();
    let repo = init_git_repo();
    let base = engine
        .create_entity_with_payload(
            "base",
            0,
            0,
            9,
            9,
            &serde_json::json!({
                "repo_path": repo.to_string_lossy().to_string(),
                "auto_rebase_enabled": true,
                "auto_rebase_interval_sec": 120,
            })
            .to_string(),
        )
        .unwrap();
    let state = Arc::new(AppState {
        engine: engine.clone(),
    });
    let _ = api_bases_sync_now(
        axum::extract::State(state),
        axum::extract::Path(base.id.clone()),
    )
    .await
    .unwrap();

    let conn = engine.open().unwrap();
    let c: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE workflow_id='auto-rebase' AND entity_id=?1",
            [&base.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(c, 1);
}

#[tokio::test]
async fn webhook_push_queues_auto_rebase() {
    let engine = temp_engine();
    let repo = init_git_repo();
    std::process::Command::new("git")
        .args([
            "remote",
            "set-url",
            "origin",
            "https://github.com/acme/demo.git",
        ])
        .current_dir(&repo)
        .output()
        .unwrap();

    let base = engine
        .create_entity_with_payload(
            "base",
            0,
            0,
            9,
            9,
            &serde_json::json!({
                "repo_path": repo.to_string_lossy().to_string(),
                "auto_rebase_enabled": true,
                "auto_rebase_interval_sec": 120,
            })
            .to_string(),
        )
        .unwrap();

    let state = Arc::new(AppState {
        engine: engine.clone(),
    });
    let mut headers = HeaderMap::new();
    headers.insert("x-github-event", HeaderValue::from_static("push"));
    let payload = serde_json::json!({
        "ref": "refs/heads/main",
        "after": "abc123",
        "repository": { "full_name": "acme/demo" }
    });
    let _ = api_github_webhook(axum::extract::State(state), headers, Json(payload))
        .await
        .unwrap();

    let conn = engine.open().unwrap();
    let c: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE workflow_id='auto-rebase' AND entity_id=?1",
            [&base.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(c, 1);
}

#[tokio::test]
async fn pr_feed_lists_feature_runs_with_fallback_summary() {
    let engine = temp_engine();
    let base = engine
        .create_entity_with_payload(
            "base",
            0,
            0,
            9,
            9,
            &serde_json::json!({"repo_path":"/tmp/no-such-repo"}).to_string(),
        )
        .unwrap();
    let feature = engine
        .create_entity_with_payload(
            "feature",
            12,
            0,
            3,
            4,
            &serde_json::json!({"base_id": base.id}).to_string(),
        )
        .unwrap();
    let conn = engine.open().unwrap();
    conn.execute(
        "INSERT INTO runs (id, workflow_id, task, status, entity_id, context_json, created_at, updated_at) VALUES (?1,'feature-dev',?2,'running',?3,?4,?5,?5)",
        (
            "r-pr-feed-1",
            "Implement mobile feed",
            &feature.id,
            serde_json::json!({
                "base_repo_path":"/tmp/no-such-repo",
                "branch":"clawdorio/r-pr-feed-1",
                "pr_url":"https://github.com/acme/demo/pull/42"
            })
            .to_string(),
            now_rfc3339(),
        ),
    )
    .unwrap();

    let out = api_pr_feed(
        axum::extract::State(Arc::new(AppState {
            engine: engine.clone(),
        })),
        axum::extract::Query(PrFeedQuery {
            base_id: Some(base.id.clone()),
            limit: Some(10),
        }),
    )
    .await
    .unwrap();
    assert_eq!(out.0.len(), 1);
    assert_eq!(out.0[0].run_id, "r-pr-feed-1");
    assert_eq!(out.0[0].pr_number, Some(42));
    assert_eq!(out.0[0].changed_files.total_files, 0);
    assert_eq!(out.0[0].changed_files.source, "fallback");
}

#[tokio::test]
async fn pr_comment_reemit_idempotency_and_rate_limit() {
    let engine = temp_engine();
    let base = engine
        .create_entity_with_payload(
            "base",
            0,
            0,
            9,
            9,
            &serde_json::json!({"repo_path":"/tmp/no-such-repo"}).to_string(),
        )
        .unwrap();
    let feature = engine
        .create_entity_with_payload(
            "feature",
            12,
            0,
            3,
            4,
            &serde_json::json!({"base_id": base.id}).to_string(),
        )
        .unwrap();
    let conn = engine.open().unwrap();
    conn.execute(
        "INSERT INTO runs (id, workflow_id, task, status, entity_id, context_json, created_at, updated_at) VALUES (?1,'feature-dev','task','running',?2,'{}',?3,?3)",
        ("r-comment-1", &feature.id, now_rfc3339()),
    )
    .unwrap();
    seed_step(
        &engine,
        "s-comment-1",
        "r-comment-1",
        "implement",
        0,
        "running",
    );

    let state = axum::extract::State(Arc::new(AppState {
        engine: engine.clone(),
    }));
    let first = api_pr_comment(
        state.clone(),
        Json(PrCommentInput {
            run_id: Some("r-comment-1".to_string()),
            pr_url: None,
            pr_number: None,
            comment: "please rerun".to_string(),
            idempotency_key: Some("idem-1".to_string()),
        }),
    )
    .await
    .unwrap();
    assert!(first.0.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));

    let replay = api_pr_comment(
        state.clone(),
        Json(PrCommentInput {
            run_id: Some("r-comment-1".to_string()),
            pr_url: None,
            pr_number: None,
            comment: "please rerun".to_string(),
            idempotency_key: Some("idem-1".to_string()),
        }),
    )
    .await
    .unwrap();
    assert!(replay
        .0
        .get("idempotent_replay")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let err = api_pr_comment(
        state,
        Json(PrCommentInput {
            run_id: Some("r-comment-1".to_string()),
            pr_url: None,
            pr_number: None,
            comment: "please rerun again".to_string(),
            idempotency_key: Some("idem-2".to_string()),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.0, axum::http::StatusCode::TOO_MANY_REQUESTS);
}
