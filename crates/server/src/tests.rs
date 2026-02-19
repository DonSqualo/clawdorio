use super::*;

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
