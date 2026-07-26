use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    model::{Job, JobState, Segment, TransferMode, UrlMode},
    Error, Result,
};

#[derive(Clone, Debug)]
pub struct Store {
    db_path: PathBuf,
}

impl Store {
    pub fn open_default() -> Result<Self> {
        let root = std::env::var_os("DOWNGET_STATE_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs_next::data_dir().map(|path| path.join("downget")))
            .ok_or_else(|| {
                Error::User("não foi possível localizar o diretório de dados do usuário".into())
            })?;
        Self::open(root)
    }

    pub fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("locks"))?;
        let store = Self {
            db_path: root.join("state.sqlite3"),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn lock_dir(&self) -> PathBuf {
        self.db_path
            .parent()
            .expect("database has a parent")
            .join("locks")
    }

    fn connection(&self) -> Result<Connection> {
        let connection = Connection::open(&self.db_path)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )?;
        Ok(connection)
    }

    fn migrate(&self) -> Result<()> {
        let connection = self.connection()?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL
              );
              CREATE TABLE IF NOT EXISTS jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                dest_path TEXT NOT NULL DEFAULT '',
                part_path TEXT NOT NULL DEFAULT '',
                state TEXT NOT NULL,
                transfer_mode TEXT,
                requested_concurrency INTEGER NOT NULL,
                effective_concurrency INTEGER NOT NULL,
                parallelism_note TEXT,
                url_mode TEXT NOT NULL,
                safe_url TEXT,
                source_display TEXT NOT NULL DEFAULT '',
                size INTEGER,
                etag TEXT,
                last_modified TEXT,
                sha256_expected TEXT,
                retry_summary TEXT,
                simple_attempts INTEGER NOT NULL DEFAULT 0,
                last_error_code TEXT,
                last_error_action TEXT,
                pause_requested INTEGER NOT NULL DEFAULT 0,
                control_seq INTEGER NOT NULL DEFAULT 0,
                control_request TEXT,
                control_ack_seq INTEGER NOT NULL DEFAULT 0,
                active_run_id TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
              );
              CREATE TABLE IF NOT EXISTS segments (
                job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                start INTEGER NOT NULL,
                end INTEGER NOT NULL,
                committed_end INTEGER NOT NULL,
                state TEXT NOT NULL,
                attempts_used INTEGER NOT NULL DEFAULT 0,
                last_error_code TEXT,
                PRIMARY KEY(job_id, ordinal)
              );
              CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at INTEGER NOT NULL
              );
              INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES (1, strftime('%s','now'));",
        )?;
        let columns = connection
            .prepare("PRAGMA table_info(jobs)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (name, definition) in [
            ("simple_attempts", "INTEGER NOT NULL DEFAULT 0"),
            ("control_seq", "INTEGER NOT NULL DEFAULT 0"),
            ("control_request", "TEXT"),
            ("control_ack_seq", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            if !columns.iter().any(|column| column == name) {
                connection.execute(
                    &format!("ALTER TABLE jobs ADD COLUMN {name} {definition}"),
                    [],
                )?;
            }
        }
        Ok(())
    }

    pub fn get_concurrency(&self) -> Result<u8> {
        let connection = self.connection()?;
        let value: Option<String> = connection
            .query_row(
                "SELECT value FROM settings WHERE key = 'concurrency'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match value {
            Some(value) => value
                .parse::<u8>()
                .ok()
                .filter(|value| (1..=8).contains(value))
                .ok_or_else(|| {
                    Error::Internal("configuração de concorrência inválida no estado".into())
                }),
            None => Ok(2),
        }
    }

    pub fn set_concurrency(&self, concurrency: u8) -> Result<()> {
        if !(1..=8).contains(&concurrency) {
            return Err(Error::User("concorrência deve estar entre 1 e 8".into()));
        }
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO settings(key, value, updated_at) VALUES ('concurrency', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![concurrency.to_string(), now()],
        )?;
        Ok(())
    }

    pub fn create_initial_job(&self, concurrency: u8, sha256: Option<&str>) -> Result<i64> {
        let connection = self.connection()?;
        let timestamp = now();
        connection.execute(
            "INSERT INTO jobs(state, requested_concurrency, effective_concurrency, url_mode, sha256_expected, created_at, updated_at)
             VALUES (?1, ?2, ?2, 'retained', ?3, ?4, ?4)",
            params![JobState::Initializing.as_str(), concurrency, sha256, timestamp],
        )?;
        Ok(connection.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn initialize_job(
        &self,
        id: i64,
        dest: &Path,
        part: &Path,
        mode: TransferMode,
        size: Option<u64>,
        etag: Option<&str>,
        last_modified: Option<&str>,
        url_mode: UrlMode,
        safe_url: Option<&str>,
        source_display: &str,
    ) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE jobs SET dest_path=?2, part_path=?3, state=?4, transfer_mode=?5, size=?6, etag=?7,
             last_modified=?8, url_mode=?9, safe_url=?10, source_display=?11, updated_at=?12 WHERE id=?1",
            params![id, path_text(dest), path_text(part), JobState::Probing.as_str(), mode.as_str(),
                size.map(|v| v as i64), etag, last_modified, url_mode.as_str(), safe_url, source_display, now()],
        )?;
        Ok(())
    }

    pub fn delete_initial_job(&self, id: i64) -> Result<()> {
        self.connection()?.execute(
            "DELETE FROM jobs WHERE id=?1 AND state='initializing'",
            [id],
        )?;
        Ok(())
    }

    pub fn job(&self, id: i64) -> Result<Job> {
        self.connection()?.query_row(
            "SELECT id,dest_path,part_path,state,transfer_mode,requested_concurrency,effective_concurrency,
                    parallelism_note,url_mode,safe_url,source_display,size,etag,last_modified,sha256_expected,
                    retry_summary,last_error_code,last_error_action,pause_requested FROM jobs WHERE id=?1",
            [id], row_to_job,
        ).optional()?.ok_or_else(|| Error::User(format!("Job {id} não encontrado")))
    }

    pub fn jobs(&self) -> Result<Vec<Job>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,dest_path,part_path,state,transfer_mode,requested_concurrency,effective_concurrency,
                    parallelism_note,url_mode,safe_url,source_display,size,etag,last_modified,sha256_expected,
                    retry_summary,last_error_code,last_error_action,pause_requested FROM jobs ORDER BY id",
        )?;
        let jobs = statement
            .query_map([], row_to_job)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(jobs)
    }

    pub fn set_state(
        &self,
        id: i64,
        state: JobState,
        code: Option<&str>,
        action: Option<&str>,
    ) -> Result<()> {
        self.connection()?.execute(
            "UPDATE jobs SET state=?2,last_error_code=?3,last_error_action=?4,updated_at=?5 WHERE id=?1",
            params![id, state.as_str(), code, action, now()],
        )?;
        Ok(())
    }

    pub fn set_active(&self, id: i64, active: bool) -> Result<()> {
        self.connection()?.execute(
            "UPDATE jobs SET active_run_id=?2,updated_at=?3 WHERE id=?1",
            params![
                id,
                if active {
                    Some(format!("run-{}", now()))
                } else {
                    None
                },
                now()
            ],
        )?;
        Ok(())
    }

    pub fn request_pause(&self, id: i64) -> Result<i64> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let sequence: i64 =
            tx.query_row("SELECT control_seq+1 FROM jobs WHERE id=?1", [id], |row| {
                row.get(0)
            })?;
        tx.execute("UPDATE jobs SET control_seq=?2,control_request='pause',pause_requested=1,updated_at=?3 WHERE id=?1", params![id, sequence, now()])?;
        tx.commit()?;
        Ok(sequence)
    }

    pub fn clear_pause_request(&self, id: i64) -> Result<()> {
        self.connection()?.execute(
            "UPDATE jobs SET pause_requested=0,updated_at=?2 WHERE id=?1",
            params![id, now()],
        )?;
        Ok(())
    }

    pub fn acknowledge_pause(&self, id: i64) -> Result<()> {
        self.connection()?.execute(
            "UPDATE jobs SET control_ack_seq=control_seq,control_request=NULL,pause_requested=0,updated_at=?2 WHERE id=?1",
            params![id, now()],
        )?;
        Ok(())
    }

    pub fn pause_acknowledged(&self, id: i64, sequence: i64) -> Result<bool> {
        Ok(self.connection()?.query_row(
            "SELECT state='paused' AND active_run_id IS NULL AND control_ack_seq>=?2 FROM jobs WHERE id=?1",
            params![id, sequence],
            |row| row.get::<_, i64>(0),
        )? != 0)
    }

    pub fn pause_requested(&self, id: i64) -> Result<bool> {
        Ok(self.connection()?.query_row(
            "SELECT pause_requested FROM jobs WHERE id=?1",
            [id],
            |row| row.get::<_, i64>(0),
        )? != 0)
    }

    pub fn create_segments(&self, id: i64, segments: &[(u64, u64)]) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction()?;
        for (ordinal, (start, end)) in segments.iter().enumerate() {
            tx.execute(
                "INSERT INTO segments(job_id,ordinal,start,end,committed_end,state) VALUES (?1,?2,?3,?4,?5,'pending')",
                params![id, ordinal as u32, *start as i64, *end as i64, *start as i64 - 1],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn segments(&self, id: i64) -> Result<Vec<Segment>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT ordinal,start,end,committed_end,attempts_used,state FROM segments WHERE job_id=?1 ORDER BY ordinal",
        )?;
        let segments = statement
            .query_map([id], |row| {
                Ok(Segment {
                    ordinal: row.get(0)?,
                    start: row.get::<_, i64>(1)? as u64,
                    end: row.get::<_, i64>(2)? as u64,
                    committed_end: row.get(3)?,
                    attempts_used: row.get::<_, i64>(4)? as u8,
                    state: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(segments)
    }

    pub fn checkpoint_segment(&self, job_id: i64, segment: &Segment, attempts: u8) -> Result<()> {
        self.checkpoint_segment_at(job_id, segment, segment.end, attempts)
    }

    pub fn checkpoint_segment_at(
        &self,
        job_id: i64,
        segment: &Segment,
        committed_end: u64,
        attempts: u8,
    ) -> Result<()> {
        let state = if committed_end == segment.end {
            "completed"
        } else {
            "pending"
        };
        self.connection()?.execute(
            "UPDATE segments SET committed_end=?3,state=?4,attempts_used=?5,last_error_code=NULL
             WHERE job_id=?1 AND ordinal=?2",
            params![
                job_id,
                segment.ordinal,
                committed_end as i64,
                state,
                attempts
            ],
        )?;
        Ok(())
    }

    /// Atomically consumes one of the five request attempts for this segment.
    /// Returning `None` is a durable refusal to make a sixth request, even
    /// after a later `resume` process opens the same SQLite database.
    pub fn begin_segment_attempt(&self, job_id: i64, ordinal: u32) -> Result<Option<u8>> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let used: i64 = tx.query_row(
            "SELECT attempts_used FROM segments WHERE job_id=?1 AND ordinal=?2",
            params![job_id, ordinal],
            |row| row.get(0),
        )?;
        if used >= 5 {
            tx.commit()?;
            return Ok(None);
        }
        let next = used + 1;
        tx.execute("UPDATE segments SET attempts_used=?3,last_error_code='in_progress' WHERE job_id=?1 AND ordinal=?2", params![job_id, ordinal, next])?;
        tx.commit()?;
        Ok(Some(next as u8))
    }

    pub fn fail_segment_terminal(&self, job_id: i64, ordinal: u32, code: &str) -> Result<()> {
        self.connection()?.execute("UPDATE segments SET state='failed_terminal',last_error_code=?3 WHERE job_id=?1 AND ordinal=?2", params![job_id, ordinal, code])?;
        Ok(())
    }

    /// Like `begin_segment_attempt`, this counter survives process restart.
    pub fn begin_simple_attempt(&self, id: i64) -> Result<Option<u8>> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let used: i64 = tx.query_row(
            "SELECT simple_attempts FROM jobs WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        if used >= 5 {
            tx.commit()?;
            return Ok(None);
        }
        let next = used + 1;
        tx.execute(
            "UPDATE jobs SET simple_attempts=?2,retry_summary=?3,updated_at=?4 WHERE id=?1",
            params![id, next, format!("tentativa {next}/5"), now()],
        )?;
        tx.commit()?;
        Ok(Some(next as u8))
    }

    pub fn record_segment_attempt(
        &self,
        job_id: i64,
        ordinal: u32,
        attempts: u8,
        code: &str,
    ) -> Result<()> {
        self.connection()?.execute(
            "UPDATE segments SET attempts_used=?3,last_error_code=?4 WHERE job_id=?1 AND ordinal=?2",
            params![job_id, ordinal, attempts, code],
        )?;
        Ok(())
    }

    pub fn reset_segments(&self, id: i64) -> Result<()> {
        self.connection()?.execute(
            "UPDATE segments SET committed_end=start-1,state='pending',attempts_used=0,last_error_code=NULL WHERE job_id=?1", [id],
        )?;
        Ok(())
    }

    pub fn set_parallelism_reduced(&self, id: i64) -> Result<()> {
        self.connection()?.execute(
            "UPDATE jobs SET effective_concurrency=1,parallelism_note='reduced_after_429_or_503',updated_at=?2 WHERE id=?1",
            params![id, now()],
        )?;
        Ok(())
    }

    pub fn set_retry_summary(&self, id: i64, summary: &str) -> Result<()> {
        self.connection()?.execute(
            "UPDATE jobs SET retry_summary=?2,updated_at=?3 WHERE id=?1",
            params![id, summary, now()],
        )?;
        Ok(())
    }

    pub fn update_sha256(&self, id: i64, value: &str) -> Result<()> {
        self.connection()?.execute(
            "UPDATE jobs SET sha256_expected=?2,updated_at=?3 WHERE id=?1",
            params![id, value, now()],
        )?;
        Ok(())
    }

    pub fn require_replacement_url(&self, id: i64) -> Result<()> {
        self.connection()?.execute(
            "UPDATE jobs SET url_mode='replacement_required',safe_url=NULL,updated_at=?2 WHERE id=?1",
            params![id, now()],
        )?;
        Ok(())
    }

    pub fn refresh_source(
        &self,
        id: i64,
        mode: TransferMode,
        size: Option<u64>,
        etag: Option<&str>,
        last_modified: Option<&str>,
        source_display: &str,
    ) -> Result<()> {
        self.connection()?.execute(
            "UPDATE jobs SET transfer_mode=?2,size=?3,etag=?4,last_modified=?5,source_display=?6,updated_at=?7 WHERE id=?1",
            params![id, mode.as_str(), size.map(|v| v as i64), etag, last_modified, source_display, now()],
        )?;
        Ok(())
    }

    pub fn delete_job(&self, id: i64) -> Result<()> {
        self.connection()?
            .execute("DELETE FROM jobs WHERE id=?1", [id])?;
        Ok(())
    }
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let state: String = row.get(3)?;
    let mode: Option<String> = row.get(4)?;
    let url_mode: String = row.get(8)?;
    Ok(Job {
        id: row.get(0)?,
        dest_path: PathBuf::from(row.get::<_, String>(1)?),
        part_path: PathBuf::from(row.get::<_, String>(2)?),
        state: JobState::parse(&state).ok_or(rusqlite::Error::InvalidQuery)?,
        transfer_mode: mode.as_deref().and_then(TransferMode::parse),
        requested_concurrency: row.get::<_, i64>(5)? as u8,
        effective_concurrency: row.get::<_, i64>(6)? as u8,
        parallelism_note: row.get(7)?,
        url_mode: UrlMode::parse(&url_mode).ok_or(rusqlite::Error::InvalidQuery)?,
        safe_url: row.get(9)?,
        source_display: row.get(10)?,
        size: row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        etag: row.get(12)?,
        last_modified: row.get(13)?,
        sha256_expected: row.get(14)?,
        retry_summary: row.get(15)?,
        last_error_code: row.get(16)?,
        last_error_action: row.get(17)?,
        pause_requested: row.get::<_, i64>(18)? != 0,
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temporary_store(name: &str) -> (Store, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "downget-store-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (Store::open(root.clone()).unwrap(), root)
    }

    #[test]
    fn retry_attempts_survive_reopen_and_refuse_a_sixth_request() {
        let (store, root) = temporary_store("retry");
        let id = store.create_initial_job(2, None).unwrap();
        let destination = root.join("file");
        let partial = root.join("file.part");
        store
            .initialize_job(
                id,
                &destination,
                &partial,
                TransferMode::Segmented,
                Some(8),
                Some("\"v1\""),
                None,
                UrlMode::Retained,
                Some("http://example.test/file"),
                "http://example.test/file",
            )
            .unwrap();
        store.create_segments(id, &[(0, 7)]).unwrap();
        for expected in 1..=5 {
            assert_eq!(store.begin_segment_attempt(id, 0).unwrap(), Some(expected));
        }
        assert_eq!(store.begin_segment_attempt(id, 0).unwrap(), None);
        drop(store);

        let reopened = Store::open(root.clone()).unwrap();
        assert_eq!(reopened.begin_segment_attempt(id, 0).unwrap(), None);
        assert_eq!(reopened.segments(id).unwrap()[0].attempts_used, 5);
        assert_eq!(reopened.begin_simple_attempt(id).unwrap(), Some(1));
        for expected in 2..=5 {
            assert_eq!(reopened.begin_simple_attempt(id).unwrap(), Some(expected));
        }
        assert_eq!(reopened.begin_simple_attempt(id).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
