use std::{fs, path::Path};

use rusqlite::{Connection, OptionalExtension, params, types::Type};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Default, Serialize)]
pub struct Snapshot {
    pub target: String,
    pub status: Option<u16>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub complete: bool,
}

#[derive(Serialize)]
pub struct Attempt {
    pub request: Snapshot,
    pub response: Option<Snapshot>,
}

#[derive(Default, Serialize)]
pub struct Exchange {
    pub request: Snapshot,
    pub attempts: Vec<Attempt>,
    pub response: Option<Snapshot>,
    pub error: Option<String>,
}

pub struct History(Connection);

impl History {
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA auto_vacuum=FULL;
             CREATE TABLE IF NOT EXISTS calls (
                 id INTEGER PRIMARY KEY, summary TEXT NOT NULL, detail TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS usage (
                 id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL,
                 provider TEXT NOT NULL, model TEXT NOT NULL,
                 input_tokens INTEGER, output_tokens INTEGER,
                 cache_read_tokens INTEGER, cache_write_tokens INTEGER
             );
             CREATE INDEX IF NOT EXISTS usage_timestamp ON usage(timestamp);",
        )?;
        save_usage(&connection, None)?;
        Ok(Self(connection))
    }

    pub fn last_id(&self) -> rusqlite::Result<i64> {
        self.0
            .query_row("SELECT coalesce(max(id), 0) FROM calls", [], |row| {
                row.get(0)
            })
    }

    pub fn save(&mut self, id: i64, summary: &Value, detail: &Value) -> rusqlite::Result<()> {
        let transaction = self.0.transaction()?;
        transaction.execute(
            "INSERT INTO calls (id, summary, detail) VALUES (?1, ?2, ?3)",
            params![id, summary.to_string(), detail.to_string()],
        )?;
        save_usage(&transaction, Some(id))?;
        transaction.execute(
            "DELETE FROM calls WHERE id NOT IN (SELECT id FROM calls ORDER BY id DESC LIMIT 100)",
            [],
        )?;
        transaction.commit()
    }

    pub fn list(&self) -> rusqlite::Result<Vec<Value>> {
        self.0
            .prepare("SELECT summary FROM calls ORDER BY id DESC")?
            .query_map([], value)?
            .collect()
    }

    pub fn detail(&self, id: i64) -> rusqlite::Result<Option<Value>> {
        self.0
            .query_row("SELECT detail FROM calls WHERE id = ?1", [id], value)
            .optional()
    }

    pub fn usage(&self, since: i64) -> rusqlite::Result<Value> {
        let aggregate = |provider: &str, model: &str, group: &str| {
            self.0
                .prepare(&format!(
                    "SELECT {provider}, {model}, count(*), sum(input_tokens), sum(output_tokens),
                     sum(cache_read_tokens), sum(cache_write_tokens), count(input_tokens),
                     count(output_tokens), count(cache_read_tokens), count(cache_write_tokens),
                     100.0 * sum(CASE WHEN input_tokens IS NOT NULL THEN cache_read_tokens END)
                     / nullif(sum(CASE WHEN cache_read_tokens IS NOT NULL THEN input_tokens END), 0)
                     FROM usage WHERE timestamp >= ?1 {group}"
                ))?
                .query_map([since], |row| {
                    Ok(json!({
                        "provider": row.get::<_, Option<String>>(0)?,
                        "model": row.get::<_, Option<String>>(1)?,
                        "requests": row.get::<_, i64>(2)?,
                        "input_tokens": row.get::<_, Option<i64>>(3)?,
                        "output_tokens": row.get::<_, Option<i64>>(4)?,
                        "cache_read_tokens": row.get::<_, Option<i64>>(5)?,
                        "cache_write_tokens": row.get::<_, Option<i64>>(6)?,
                        "input_reported_requests": row.get::<_, i64>(7)?,
                        "output_reported_requests": row.get::<_, i64>(8)?,
                        "cache_reported_requests": row.get::<_, i64>(9)?,
                        "cache_write_reported_requests": row.get::<_, i64>(10)?,
                        "cache_hit_rate": row.get::<_, Option<f64>>(11)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<Value>>>()
        };
        Ok(json!({
            "total": aggregate("NULL", "NULL", "")?.remove(0),
            "providers": aggregate("provider", "NULL", "GROUP BY provider ORDER BY provider")?,
            "models": aggregate("provider", "model", "GROUP BY provider, model ORDER BY provider, model")?,
        }))
    }
}

fn save_usage(connection: &Connection, id: Option<i64>) -> rusqlite::Result<()> {
    // Backfill retained history on upgrade; the primary key makes subsequent opens idempotent.
    connection.execute(
        "INSERT OR IGNORE INTO usage
         SELECT id, json_extract(summary, '$.timestamp'),
         CASE WHEN substr(json_extract(summary, '$.model'), 1, 9) = 'opencode/' THEN 'opencode' ELSE 'stepfun' END,
         CASE WHEN substr(json_extract(summary, '$.model'), 1, 9) = 'opencode/' THEN substr(json_extract(summary, '$.model'), 10)
              WHEN substr(json_extract(summary, '$.model'), 1, 8) = 'stepfun/' THEN substr(json_extract(summary, '$.model'), 9)
              ELSE json_extract(summary, '$.model') END,
         json_extract(summary, '$.cache.input_tokens'), json_extract(summary, '$.cache.output_tokens'),
         json_extract(summary, '$.cache.cache_read_tokens'), json_extract(summary, '$.cache.cache_write_tokens')
         FROM calls WHERE ?1 IS NULL OR id = ?1",
        [id],
    )?;
    Ok(())
}

fn value(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    serde_json::from_str(&row.get::<_, String>(0)?)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error)))
}
