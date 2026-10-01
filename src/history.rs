use std::{fs, path::Path};

use rusqlite::{Connection, OptionalExtension, params, types::Type};
use serde::Serialize;
use serde_json::Value;

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
             );",
        )?;
        Ok(Self(connection))
    }

    pub fn last_id(&self) -> rusqlite::Result<u64> {
        self.0
            .query_row("SELECT coalesce(max(id), 0) FROM calls", [], |row| row.get(0))
    }

    pub fn save(&mut self, id: u64, summary: &Value, detail: &Value) -> rusqlite::Result<()> {
        let transaction = self.0.transaction()?;
        transaction.execute(
            "INSERT INTO calls (id, summary, detail) VALUES (?1, ?2, ?3)",
            params![id, summary.to_string(), detail.to_string()],
        )?;
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

    pub fn detail(&self, id: u64) -> rusqlite::Result<Option<Value>> {
        self.0
            .query_row("SELECT detail FROM calls WHERE id = ?1", [id], value)
            .optional()
    }
}

fn value(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    serde_json::from_str(&row.get::<_, String>(0)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
    })
}
