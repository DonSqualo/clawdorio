use anyhow::Context;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn new_id(prefix: &str) -> String {
    let c = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{c}", now_ms())
}

#[derive(Debug, Clone)]
pub struct Engine {
    db_path: PathBuf,
}

impl Engine {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn open(&self) -> anyhow::Result<Connection> {
        let path = self.db_path.clone();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create db dir: {}", dir.display()))?;
        }

        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open sqlite db: {}", path.display()))?;

        // Durable + fast defaults.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        migrate(&conn)?;
        Ok(conn)
    }

    pub fn list_entities(&self) -> anyhow::Result<Vec<Entity>> {
        let conn = self.open()?;
        let mut stmt = conn.prepare(
            "SELECT id, kind, x, y, payload_json, created_at_ms, updated_at_ms, rev FROM entities ORDER BY updated_at_ms DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Entity {
                id: row.get(0)?,
                kind: row.get(1)?,
                x: row.get(2)?,
                y: row.get(3)?,
                payload_json: row.get(4)?,
                created_at_ms: row.get(5)?,
                updated_at_ms: row.get(6)?,
                rev: row.get(7)?,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn create_entity(&self, kind: &str, x: i64, y: i64) -> anyhow::Result<Entity> {
        let mut conn = self.open()?;
        let tx = conn.transaction()?;
        let id = new_id("ent");
        let ts = now_ms();
        let payload_json = "{}".to_string();
        tx.execute(
            "INSERT INTO entities (id, kind, x, y, payload_json, created_at_ms, updated_at_ms, rev)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 1)",
            (&id, kind, x, y, &payload_json, ts),
        )?;
        append_event_tx(
            &tx,
            "entity.created",
            Some(&id),
            serde_json::json!({ "id": id, "kind": kind, "x": x, "y": y }),
        )?;
        tx.commit()?;
        Ok(Entity {
            id,
            kind: kind.to_string(),
            x,
            y,
            payload_json,
            created_at_ms: ts,
            updated_at_ms: ts,
            rev: 1,
        })
    }

    pub fn delete_entity(&self, id: &str) -> anyhow::Result<bool> {
        let mut conn = self.open()?;
        let tx = conn.transaction()?;
        let n = tx.execute("DELETE FROM entities WHERE id = ?1", [id])?;
        if n > 0 {
            append_event_tx(
                &tx,
                "entity.deleted",
                Some(id),
                serde_json::json!({ "id": id }),
            )?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    pub fn get_rev(&self) -> anyhow::Result<i64> {
        let conn = self.open()?;
        let rev: Option<i64> =
            conn.query_row("SELECT MAX(seq) FROM event_log", [], |row| row.get(0))?;
        Ok(rev.unwrap_or(0))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub kind: String,
    pub x: i64,
    pub y: i64,
    pub payload_json: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub rev: i64,
}

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    // Lightweight migrations. We use `user_version` + IF NOT EXISTS + best-effort ALTERs,
    // because the schema is still young and we want installs to be resilient.
    let v: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if v < 1 {
        conn.execute_batch(
            r#"
CREATE TABLE IF NOT EXISTS events (
  id TEXT PRIMARY KEY,
  ts TEXT NOT NULL,
  kind TEXT NOT NULL,
  payload_json TEXT NOT NULL DEFAULT '{}'
);

-- Monotonic revision source for UI sync.
CREATE TABLE IF NOT EXISTS event_log (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  ts_ms INTEGER NOT NULL,
  kind TEXT NOT NULL,
  entity_id TEXT,
  payload_json TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_event_log_ts ON event_log(ts_ms);
CREATE INDEX IF NOT EXISTS idx_event_log_kind ON event_log(kind);

-- Unified UI + machine state lives here. External resources use desired/observed fields
-- with reconciliation so the DB never "drifts" from what the UI shows.
CREATE TABLE IF NOT EXISTS entities (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  x INTEGER NOT NULL,
  y INTEGER NOT NULL,
  payload_json TEXT NOT NULL DEFAULT '{}',
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  rev INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(kind);
CREATE INDEX IF NOT EXISTS idx_entities_updated_at ON entities(updated_at_ms);

CREATE TABLE IF NOT EXISTS agents (
  id TEXT PRIMARY KEY,
  role TEXT,
  desired_json TEXT NOT NULL DEFAULT '{}',
  observed_json TEXT NOT NULL DEFAULT '{}',
  observed_at_ms INTEGER NOT NULL DEFAULT 0,
  updated_at_ms INTEGER NOT NULL,
  rev INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS worktrees (
  id TEXT PRIMARY KEY,
  repo_path TEXT,
  desired_json TEXT NOT NULL DEFAULT '{}',
  observed_json TEXT NOT NULL DEFAULT '{}',
  observed_at_ms INTEGER NOT NULL DEFAULT 0,
  updated_at_ms INTEGER NOT NULL,
  rev INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS runs (
  id TEXT PRIMARY KEY,
  workflow_id TEXT NOT NULL,
  task TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'running',
  context_json TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS steps (
  id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL REFERENCES runs(id),
  step_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  step_index INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'waiting',
  input_json TEXT NOT NULL DEFAULT '{}',
  output_text TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
"#,
        )?;

        conn.pragma_update(None, "user_version", 1_i64)?;
    }

    // Best-effort column additions for existing DBs.
    ensure_column(conn, "entities", "rev", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "agents", "rev", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "worktrees", "rev", "INTEGER NOT NULL DEFAULT 0")?;

    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, col: &str, decl: &str) -> anyhow::Result<()> {
    let sql = format!("ALTER TABLE {table} ADD COLUMN {col} {decl}");
    match conn.execute(&sql, []) {
        Ok(_) => Ok(()),
        Err(e) => {
            // Ignore "duplicate column name".
            if e.to_string().to_lowercase().contains("duplicate column") {
                return Ok(());
            }
            Err(e).with_context(|| format!("ensure column {table}.{col}"))
        }
    }
}

fn append_event_tx(
    tx: &rusqlite::Transaction<'_>,
    kind: &str,
    entity_id: Option<&str>,
    payload: serde_json::Value,
) -> anyhow::Result<i64> {
    let ts = now_ms();
    let payload_json = payload.to_string();
    tx.execute(
        "INSERT INTO event_log (ts_ms, kind, entity_id, payload_json) VALUES (?1, ?2, ?3, ?4)",
        (ts, kind, entity_id, payload_json),
    )?;
    Ok(tx.last_insert_rowid())
}
