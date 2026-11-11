//! SQLite event store. Current state is always rebuilt from rows, so a
//! restart loses nothing that was committed.

use crate::event::Event;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const SCHEMA_VERSION: i64 = 1;

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    pub cwd: Option<String>,
    pub first_ts_ms: i64,
    pub last_ts_ms: i64,
    pub event_count: i64,
    /// True once a SessionEnd event was seen. False means "unknown": the
    /// session may still be running, or its end event was never delivered.
    pub ended: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestRunRecord {
    pub id: i64,
    pub cwd: String,
    pub command: String,
    pub started_ms: i64,
    pub duration_ms: i64,
    /// None when the process was killed by a signal.
    pub exit_code: Option<i32>,
    pub output_tail: String,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "database schema v{version} is newer than this build supports (v{SCHEMA_VERSION})"
            );
        }
        if version < 1 {
            self.conn.execute_batch(
                "CREATE TABLE events (
                    id TEXT PRIMARY KEY,
                    ts_ms INTEGER NOT NULL,
                    ingested_ms INTEGER NOT NULL,
                    source TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    session_id TEXT,
                    cwd TEXT,
                    agent_id TEXT,
                    tool TEXT,
                    tool_use_id TEXT,
                    file TEXT,
                    cmd_head TEXT,
                    cmd_kind TEXT,
                    status TEXT,
                    detail TEXT NOT NULL
                );
                CREATE INDEX events_ts ON events(ts_ms);
                CREATE INDEX events_session ON events(session_id, ts_ms);
                CREATE INDEX events_file ON events(file) WHERE file IS NOT NULL;
                CREATE TABLE spool_offsets (
                    file TEXT PRIMARY KEY,
                    offset INTEGER NOT NULL
                );
                CREATE TABLE test_runs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    cwd TEXT NOT NULL,
                    command TEXT NOT NULL,
                    started_ms INTEGER NOT NULL,
                    duration_ms INTEGER NOT NULL,
                    exit_code INTEGER,
                    output_tail TEXT NOT NULL
                );
                PRAGMA user_version = 1;",
            )?;
        }
        Ok(())
    }

    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }
    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }
    pub fn rollback(&self) {
        let _ = self.conn.execute_batch("ROLLBACK");
    }

    /// Insert an event. Returns false when an event with this id already
    /// exists (duplicate delivery), which is not an error.
    pub fn insert_event(&self, e: &Event, source: &str, ingested_ms: i64) -> Result<bool> {
        let detail = serde_json::to_string(e)?;
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO events
             (id, ts_ms, ingested_ms, source, kind, session_id, cwd, agent_id, tool,
              tool_use_id, file, cmd_head, cmd_kind, status, detail)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                e.id,
                e.ts_ms,
                ingested_ms,
                source,
                e.event,
                e.session_id,
                e.cwd,
                e.agent_id,
                e.tool,
                e.tool_use_id,
                e.file,
                e.cmd_head,
                e.cmd_kind,
                e.status,
                detail
            ],
        )?;
        Ok(n == 1)
    }

    /// Newest `limit` events, returned oldest first (timeline order).
    pub fn recent_events(&self, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT detail FROM (SELECT detail, ts_ms, rowid FROM events
                                 ORDER BY ts_ms DESC, rowid DESC LIMIT ?1)
             ORDER BY ts_ms ASC, rowid ASC",
        )?;
        let rows = stmt.query_map([limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn sessions(&self) -> Result<Vec<SessionSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id,
                    (SELECT cwd FROM events e2 WHERE e2.session_id = e.session_id
                       AND e2.cwd IS NOT NULL ORDER BY ts_ms ASC LIMIT 1),
                    MIN(ts_ms), MAX(ts_ms), COUNT(*),
                    MAX(CASE WHEN kind = 'SessionEnd' THEN 1 ELSE 0 END)
             FROM events e WHERE session_id IS NOT NULL
             GROUP BY session_id ORDER BY MAX(ts_ms) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SessionSummary {
                session_id: r.get(0)?,
                cwd: r.get(1)?,
                first_ts_ms: r.get(2)?,
                last_ts_ms: r.get(3)?,
                event_count: r.get(4)?,
                ended: r.get::<_, i64>(5)? == 1,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Latest successful file-edit event for an absolute path, if any:
    /// `(ts_ms, session_id)`. Used as the evidence for attributing a change.
    pub fn last_edit_of(&self, abs_path: &str) -> Result<Option<(i64, Option<String>)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ts_ms, session_id FROM events
                 WHERE file = ?1 AND kind = 'PostToolUse' AND status = 'ok'
                 ORDER BY ts_ms DESC LIMIT 1",
                [abs_path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn get_offset(&self, file: &str) -> Result<u64> {
        Ok(self
            .conn
            .query_row(
                "SELECT offset FROM spool_offsets WHERE file = ?1",
                [file],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0) as u64)
    }

    pub fn set_offset(&self, file: &str, offset: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO spool_offsets(file, offset) VALUES (?1, ?2)
             ON CONFLICT(file) DO UPDATE SET offset = excluded.offset",
            params![file, offset as i64],
        )?;
        Ok(())
    }

    pub fn insert_test_run(&self, r: &TestRunRecord) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO test_runs (cwd, command, started_ms, duration_ms, exit_code, output_tail)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                r.cwd,
                r.command,
                r.started_ms,
                r.duration_ms,
                r.exit_code,
                r.output_tail
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn recent_test_runs(&self, limit: usize) -> Result<Vec<TestRunRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, command, started_ms, duration_ms, exit_code, output_tail
             FROM test_runs ORDER BY started_ms DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(TestRunRecord {
                id: r.get(0)?,
                cwd: r.get(1)?,
                command: r.get(2)?,
                started_ms: r.get(3)?,
                duration_ms: r.get(4)?,
                exit_code: r.get(5)?,
                output_tail: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Delete all collected history (events, offsets, test runs).
    pub fn delete_all_history(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM events; DELETE FROM spool_offsets; DELETE FROM test_runs;",
        )?;
        Ok(())
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn ev(id: &str, ts: i64, kind: &str, session: &str) -> Event {
        Event {
            v: 1,
            id: id.into(),
            ts_ms: ts,
            event: kind.into(),
            session_id: Some(session.into()),
            cwd: Some("/p".into()),
            agent_id: None,
            agent_type: None,
            transcript_path: None,
            tool: None,
            tool_use_id: None,
            file: None,
            cmd_head: None,
            cmd_kind: None,
            status: None,
            source: None,
            reason: None,
            notification_type: None,
        }
    }

    #[test]
    fn duplicate_ids_are_ignored() {
        let s = Store::open_in_memory().unwrap();
        assert!(s
            .insert_event(&ev("a", 1, "SessionStart", "s"), "claude", 0)
            .unwrap());
        assert!(!s
            .insert_event(&ev("a", 1, "SessionStart", "s"), "claude", 0)
            .unwrap());
        assert_eq!(s.recent_events(10).unwrap().len(), 1);
    }

    #[test]
    fn out_of_order_events_come_back_in_time_order() {
        let s = Store::open_in_memory().unwrap();
        s.insert_event(&ev("b", 20, "Stop", "s"), "claude", 0)
            .unwrap();
        s.insert_event(&ev("a", 10, "SessionStart", "s"), "claude", 0)
            .unwrap();
        let ids: Vec<_> = s
            .recent_events(10)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn session_without_end_event_is_not_marked_ended() {
        let s = Store::open_in_memory().unwrap();
        s.insert_event(&ev("a", 1, "SessionStart", "s1"), "claude", 0)
            .unwrap();
        s.insert_event(&ev("b", 2, "SessionStart", "s2"), "claude", 0)
            .unwrap();
        s.insert_event(&ev("c", 3, "SessionEnd", "s2"), "claude", 0)
            .unwrap();
        let sessions = s.sessions().unwrap();
        let s1 = sessions.iter().find(|x| x.session_id == "s1").unwrap();
        let s2 = sessions.iter().find(|x| x.session_id == "s2").unwrap();
        assert!(!s1.ended);
        assert!(s2.ended);
    }

    #[test]
    fn history_survives_reopen_and_can_be_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_event(&ev("a", 1, "SessionStart", "s"), "claude", 0)
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.recent_events(10).unwrap().len(), 1);
        s.delete_all_history().unwrap();
        assert!(s.recent_events(10).unwrap().is_empty());
    }

    #[test]
    fn newer_database_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        Store::open(&path).unwrap();
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(Store::open(&path).is_err());
    }
}
