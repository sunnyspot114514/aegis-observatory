use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::util::{
    available_bytes, dir_size, pressure_level, utc_now, worst_pressure, Bucket, CONDITION_VERSION,
    DATA_DIR_BUDGET, DB_MAX_PAGES, DB_PAGE_SIZE, MAX_EVENTS, MAX_MESSAGES, MAX_SIM_EVENTS,
    MIN_FREE_BYTES, PAGE_LIMIT, WRITE_BURST, WRITE_RATE,
};

#[derive(Debug, Clone)]
pub struct Message {
    pub id: i64,
    pub created_at: String,
    pub body: String,
    pub source_kind: String,
}

#[derive(Debug, Clone)]
pub struct Page {
    pub items: Vec<Message>,
    pub has_more: bool,
    pub latest_id: i64,
    pub next_before: Option<i64>,
    pub next_after: Option<i64>,
    pub revision: i64,
    pub limit: i64,
}

#[derive(Debug, Clone)]
pub struct InsertOk {
    pub id: i64,
    pub created_at: String,
    pub replay: bool,
}

#[derive(Debug, Clone)]
pub enum InsertErr {
    Conflict,
    Full,
    Disk,
    DiskQuery,
    #[allow(dead_code)]
    Invalid(String),
    Busy,
    SqliteFull,
    WriteQuota(u64),
}

#[derive(Debug, Clone)]
pub struct Health {
    pub readable: bool,
    pub writable: bool,
    pub write_reason: String,
    pub capacity: Capacity,
}

#[derive(Debug, Clone)]
pub struct Capacity {
    pub messages: i64,
    pub events: i64,
    pub data_dir_bytes: u64,
    pub db_pages: i64,
    pub disk_free: Option<u64>,
    pub archive_hint: String,
}

#[derive(Debug, Clone)]
pub struct ExportBatch {
    pub instance_id: String,
    pub condition_version: String,
    pub from_seq: i64,
    pub to_seq: i64,
    pub events: Vec<ExportEvent>,
}

#[derive(Debug, Clone)]
pub struct ExportEvent {
    pub event_seq: i64,
    pub created_at: String,
    pub source_kind: String,
    pub action: String,
    pub result: String,
    pub condition_version: String,
    pub message_id: Option<i64>,
    pub body: Option<String>,
    pub request_id: Option<String>,
}

pub enum Cmd {
    List {
        before: Option<i64>,
        after: Option<i64>,
        limit: i64,
        resp: tokio::sync::oneshot::Sender<Result<Page, String>>,
    },
    Insert {
        body: String,
        request_id: Option<String>,
        source_kind: String,
        resp: tokio::sync::oneshot::Sender<Result<InsertOk, InsertErr>>,
    },
    Health {
        resp: tokio::sync::oneshot::Sender<Health>,
    },
    Export {
        from_seq: i64,
        limit: i64,
        resp: tokio::sync::oneshot::Sender<Result<ExportBatch, String>>,
    },
    SimEvent {
        session: String,
        action: String,
        result: String,
        resp: tokio::sync::oneshot::Sender<Result<(), InsertErr>>,
    },
}

#[derive(Clone)]
pub struct Store {
    tx: std::sync::mpsc::SyncSender<Cmd>,
}

impl Store {
    pub fn start(data_dir: PathBuf, instance_id: String) -> Result<Self, String> {
        std::fs::create_dir_all(&data_dir).map_err(|e| format!("data dir: {e}"))?;
        let db_path = data_dir.join("aegis.sqlite");
        let q = crate::util::env_usize("AEGIS_DB_QUEUE", crate::util::DB_QUEUE);
        let (tx, rx) = mpsc::sync_channel::<Cmd>(q);
        thread::Builder::new()
            .name("aegis-db".into())
            .stack_size(1024 * 1024)
            .spawn(move || db_loop(db_path, data_dir, instance_id, rx))
            .map_err(|e| format!("db thread: {e}"))?;
        Ok(Self { tx })
    }

    pub fn try_send(&self, cmd: Cmd) -> Result<(), Cmd> {
        self.tx.try_send(cmd).map_err(|e| match e {
            mpsc::TrySendError::Full(c) | mpsc::TrySendError::Disconnected(c) => c,
        })
    }
}

fn db_loop(db_path: PathBuf, data_dir: PathBuf, instance_id: String, rx: mpsc::Receiver<Cmd>) {
    let conn = match open_db(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("aegis db open failed: {e}");
            return;
        }
    };
    let mut conn = conn;
    let mut write_q = Bucket::new(WRITE_RATE, WRITE_BURST);
    while let Ok(cmd) = rx.recv() {
        if cmd_closed(&cmd) {
            continue;
        }
        match cmd {
            Cmd::List {
                before,
                after,
                limit,
                resp,
            } => {
                if resp.is_closed() {
                    continue;
                }
                let _ = resp.send(list_messages(&conn, before, after, limit));
            }
            Cmd::Insert {
                body,
                request_id,
                source_kind,
                resp,
            } => {
                if resp.is_closed() {
                    continue;
                }
                stall_if_requested();
                if resp.is_closed() {
                    continue;
                }
                let _ = resp.send(insert_message(
                    &mut conn,
                    &data_dir,
                    &instance_id,
                    &body,
                    request_id.as_deref(),
                    &source_kind,
                    &mut write_q,
                ));
            }
            Cmd::Health { resp } => {
                if resp.is_closed() {
                    continue;
                }
                let readable = conn
                    .query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
                    .is_ok();
                let (writable, write_reason) = if !readable {
                    (false, "unreadable".into())
                } else {
                    match write_allowed(&conn, &data_dir) {
                        Ok(()) => (true, "ok".into()),
                        Err(e) => (false, write_reason(&e).into()),
                    }
                };
                let _ = resp.send(Health {
                    readable,
                    writable,
                    write_reason,
                    capacity: capacity_of(&conn, &data_dir),
                });
            }
            Cmd::Export {
                from_seq,
                limit,
                resp,
            } => {
                if resp.is_closed() {
                    continue;
                }
                let _ = resp.send(export_batch(&conn, &instance_id, from_seq, limit));
            }
            Cmd::SimEvent {
                session,
                action,
                result,
                resp,
            } => {
                if resp.is_closed() {
                    continue;
                }
                let _ = resp.send(insert_sim_event(
                    &mut conn,
                    &data_dir,
                    &instance_id,
                    &session,
                    &action,
                    &result,
                ));
            }
        }
    }
}

fn stall_if_requested() {
    #[cfg(debug_assertions)]
    if let Ok(ms) = std::env::var("AEGIS_DB_STALL_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            if ms > 0 {
                thread::sleep(Duration::from_millis(ms.min(5_000)));
            }
        }
    }
}

fn cmd_closed(cmd: &Cmd) -> bool {
    match cmd {
        Cmd::List { resp, .. } => resp.is_closed(),
        Cmd::Insert { resp, .. } => resp.is_closed(),
        Cmd::Health { resp } => resp.is_closed(),
        Cmd::Export { resp, .. } => resp.is_closed(),
        Cmd::SimEvent { resp, .. } => resp.is_closed(),
    }
}

fn open_db(path: &PathBuf) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    let busy_ms = crate::util::env_usize("AEGIS_BUSY_MS", 3000) as u64;
    conn.busy_timeout(Duration::from_millis(busy_ms))
        .map_err(|e| e.to_string())?;
    conn.execute_batch(
        r#"
        PRAGMA journal_mode=DELETE;
        PRAGMA synchronous=FULL;
        PRAGMA page_size=4096;
        PRAGMA mmap_size=0;
        PRAGMA cache_size=-512;
        PRAGMA temp_store=MEMORY;
        PRAGMA foreign_keys=ON;
        "#,
    )
    .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "max_page_count", DB_MAX_PAGES)
        .map_err(|e| e.to_string())?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS meta (
            k TEXT PRIMARY KEY,
            v TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            created_at TEXT NOT NULL,
            body TEXT NOT NULL,
            content_version INTEGER NOT NULL DEFAULT 1,
            source_kind TEXT NOT NULL CHECK(source_kind IN ('public','controlled_test')),
            request_id TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_request_id
            ON messages(request_id) WHERE request_id IS NOT NULL;
        CREATE TABLE IF NOT EXISTS events (
            instance_id TEXT NOT NULL,
            event_seq INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            source_kind TEXT NOT NULL,
            action TEXT NOT NULL,
            result TEXT NOT NULL,
            condition_version TEXT NOT NULL,
            message_id INTEGER,
            PRIMARY KEY (instance_id, event_seq)
        );
        CREATE TABLE IF NOT EXISTS sim_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            created_at TEXT NOT NULL,
            session TEXT NOT NULL,
            action TEXT NOT NULL,
            result TEXT NOT NULL,
            experiment_version TEXT NOT NULL
        );
        INSERT OR IGNORE INTO meta(k,v) VALUES('revision','0');
        INSERT OR IGNORE INTO meta(k,v) VALUES('event_seq','0');
        "#,
    )
    .map_err(|e| e.to_string())?;
    let _ = DB_PAGE_SIZE;
    Ok(conn)
}

fn meta_i64(conn: &Connection, k: &str) -> Result<i64, rusqlite::Error> {
    conn.query_row("SELECT v FROM meta WHERE k=?1", [k], |r| {
        let s: String = r.get(0)?;
        s.parse::<i64>()
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
    })
}

fn set_meta(conn: &Connection, k: &str, v: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO meta(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        params![k, v.to_string()],
    )?;
    Ok(())
}

pub fn write_reason(e: &InsertErr) -> &'static str {
    match e {
        InsertErr::Disk => "disk_low",
        InsertErr::DiskQuery => "disk_query_failed",
        InsertErr::Full => "full",
        InsertErr::Busy => "busy",
        InsertErr::SqliteFull => "sqlite_full",
        InsertErr::Conflict => "conflict",
        InsertErr::Invalid(_) => "invalid",
        InsertErr::WriteQuota(_) => "write_quota",
    }
}

pub(crate) fn map_sql(e: rusqlite::Error) -> InsertErr {
    match e.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy) | Some(rusqlite::ErrorCode::DatabaseLocked) => {
            InsertErr::Busy
        }
        Some(rusqlite::ErrorCode::DiskFull) => InsertErr::SqliteFull,
        _ => InsertErr::Busy,
    }
}

/// Visitor/sim inserts share this check. Replay does not write. Startup schema
/// and the capped error.log are separate operational writes, not HTTP insert paths.
fn write_allowed(conn: &Connection, data_dir: &Path) -> Result<(), InsertErr> {
    if !crate::util::skip_disk_allowed() {
        match available_bytes(data_dir) {
            Err(_) => return Err(InsertErr::DiskQuery),
            Ok(free) if free < MIN_FREE_BYTES => return Err(InsertErr::Disk),
            Ok(_) => {}
        }
    }
    if dir_size(data_dir).saturating_add(DB_PAGE_SIZE as u64 * 4) > DATA_DIR_BUDGET {
        return Err(InsertErr::Disk);
    }
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .map_err(map_sql)?;
    if n >= MAX_MESSAGES {
        return Err(InsertErr::Full);
    }
    let ev: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .map_err(map_sql)?;
    if ev >= MAX_EVENTS {
        return Err(InsertErr::Full);
    }
    Ok(())
}

fn capacity_of(conn: &Connection, data_dir: &Path) -> Capacity {
    let messages: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap_or(0);
    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap_or(0);
    let data_dir_bytes = dir_size(data_dir);
    let db_pages: i64 = conn
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap_or(0);
    let disk_free = available_bytes(data_dir).ok();
    let msg_r = messages as f64 / MAX_MESSAGES as f64;
    let ev_r = events as f64 / MAX_EVENTS as f64;
    let dir_r = data_dir_bytes as f64 / DATA_DIR_BUDGET as f64;
    let db_r = db_pages as f64 / DB_MAX_PAGES as f64;
    let disk_r = match disk_free {
        Some(free) if free > 0 => (MIN_FREE_BYTES as f64 / free as f64).min(2.0),
        Some(_) => 1.0,
        None => 0.0,
    };
    let archive_hint = worst_pressure(&[
        pressure_level(msg_r),
        pressure_level(ev_r),
        pressure_level(dir_r),
        pressure_level(db_r),
        pressure_level(disk_r),
    ])
    .to_string();
    Capacity {
        messages,
        events,
        data_dir_bytes,
        db_pages,
        disk_free,
        archive_hint,
    }
}

fn export_batch(
    conn: &Connection,
    instance_id: &str,
    from_seq: i64,
    limit: i64,
) -> Result<ExportBatch, String> {
    let limit = limit.clamp(1, 100);
    let from_seq = from_seq.max(1);
    let mut stmt = conn
        .prepare(
            "SELECT e.event_seq, e.created_at, e.source_kind, e.action, e.result,
                    e.condition_version, e.message_id, m.body, m.request_id
             FROM events e
             LEFT JOIN messages m ON m.id = e.message_id
             WHERE e.instance_id=?1 AND e.event_seq>=?2
             ORDER BY e.event_seq ASC LIMIT ?3",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![instance_id, from_seq, limit], |r| {
            Ok(ExportEvent {
                event_seq: r.get(0)?,
                created_at: r.get(1)?,
                source_kind: r.get(2)?,
                action: r.get(3)?,
                result: r.get(4)?,
                condition_version: r.get(5)?,
                message_id: r.get(6)?,
                body: r.get(7)?,
                request_id: r.get(8)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut events = Vec::new();
    for r in rows {
        events.push(r.map_err(|e| e.to_string())?);
    }
    let to_seq = events.last().map(|e| e.event_seq).unwrap_or(from_seq.saturating_sub(1));
    Ok(ExportBatch {
        instance_id: instance_id.to_string(),
        condition_version: CONDITION_VERSION.to_string(),
        from_seq,
        to_seq,
        events,
    })
}

fn list_messages(
    conn: &Connection,
    before: Option<i64>,
    after: Option<i64>,
    limit: i64,
) -> Result<Page, String> {
    if before.is_some() && after.is_some() {
        return Err("before and after are mutually exclusive".into());
    }
    let limit = limit.clamp(crate::util::PAGE_LIMIT_MIN, PAGE_LIMIT);
    let latest: i64 = conn
        .query_row("SELECT COALESCE(MAX(id),0) FROM messages", [], |r| r.get(0))
        .unwrap_or(0);
    let revision = meta_i64(conn, "revision").unwrap_or(0);
    let mut items = Vec::new();
    let fetch = limit + 1;
    if let Some(a) = after {
        let mut stmt = conn
            .prepare(
                "SELECT id, created_at, body, source_kind FROM messages
                 WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![a, fetch], row_msg)
            .map_err(|e| e.to_string())?;
        for r in rows {
            items.push(r.map_err(|e| e.to_string())?);
        }
        let has_more = items.len() as i64 > limit;
        if has_more {
            items.truncate(limit as usize);
        }
        let next_after = items.last().map(|m| m.id);
        Ok(Page {
            items,
            has_more,
            latest_id: latest,
            next_before: None,
            next_after,
            revision,
            limit,
        })
    } else {
        let mut sql = String::from(
            "SELECT id, created_at, body, source_kind FROM messages ",
        );
        if before.is_some() {
            sql.push_str("WHERE id < ?1 ORDER BY id DESC LIMIT ?2");
        } else {
            sql.push_str("ORDER BY id DESC LIMIT ?1");
        }
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mapped = if let Some(b) = before {
            stmt.query_map(params![b, fetch], row_msg)
        } else {
            stmt.query_map(params![fetch], row_msg)
        };
        for r in mapped.map_err(|e| e.to_string())? {
            items.push(r.map_err(|e| e.to_string())?);
        }
        let has_more = items.len() as i64 > limit;
        if has_more {
            items.truncate(limit as usize);
        }
        let next_before = items.last().map(|m| m.id);
        Ok(Page {
            items,
            has_more,
            latest_id: latest,
            next_before,
            next_after: None,
            revision,
            limit,
        })
    }
}

fn row_msg(r: &rusqlite::Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        created_at: r.get(1)?,
        body: r.get(2)?,
        source_kind: r.get(3)?,
    })
}

fn find_by_request_id(
    conn: &Connection,
    rid: &str,
) -> Result<Option<(i64, String, String)>, InsertErr> {
    conn.query_row(
        "SELECT id, created_at, body FROM messages WHERE request_id=?1",
        [rid],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )
    .optional()
    .map_err(map_sql)
}

pub fn readonly_stats(data_dir: &Path, db: &Path) -> String {
    let log = data_dir.join("error.log");
    let error_log_bytes = std::fs::metadata(&log).map(|m| m.len()).ok();
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let counts = Connection::open_with_flags(db, flags).ok().and_then(|conn| {
        let _ = conn.pragma_update(None, "query_only", true);
        let msgs: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .ok()?;
        let ev: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .ok()?;
        Some((msgs, ev))
    });
    let (msgs, ev) = match counts {
        Some((m, e)) => (m.to_string(), e.to_string()),
        None => ("unavailable".into(), "unavailable".into()),
    };
    format!(
        "messages={msgs}\nevents={ev}\ndata_dir_bytes={}\nerror_log_bytes={}\nerror_log_dropped=unavailable\nerror_log_written=unavailable\n",
        crate::util::dir_size(data_dir),
        error_log_bytes
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unavailable".into()),
    )
}

fn insert_message(
    conn: &mut Connection,
    data_dir: &Path,
    instance_id: &str,
    body: &str,
    request_id: Option<&str>,
    source_kind: &str,
    write_q: &mut Bucket,
) -> Result<InsertOk, InsertErr> {
    // Idempotency, write-quota, and INSERT run on this single worker thread.
    // HTTP request limiter (attempts) is separate and already taken before the body.
    // Write quota limits successful *new* inserts: reserve after idempotency miss,
    // refund if the write fails. Replays do not take a token.
    if let Some(rid) = request_id {
        match find_by_request_id(conn, rid)? {
            Some((id, created_at, old)) if old == body => {
                return Ok(InsertOk {
                    id,
                    created_at,
                    replay: true,
                });
            }
            Some(_) => return Err(InsertErr::Conflict),
            None => {}
        }
    }
    write_allowed(conn, data_dir)?;
    let reserved = if crate::util::unlimited_writes() {
        false
    } else if let Some(secs) = write_q.take() {
        return Err(InsertErr::WriteQuota(secs.max(1)));
    } else {
        true
    };
    let now = utc_now();
    let wrote = (|| {
        let tx = conn.transaction().map_err(map_sql)?;
        tx.execute(
            "INSERT INTO messages(created_at, body, content_version, source_kind, request_id)
             VALUES(?1,?2,1,?3,?4)",
            params![now, body, source_kind, request_id],
        )
        .map_err(map_sql)?;
        let id = tx.last_insert_rowid();
        let seq = meta_i64(&tx, "event_seq").unwrap_or(0) + 1;
        tx.execute(
            "INSERT INTO events(instance_id, event_seq, created_at, source_kind, action, result, condition_version, message_id)
             VALUES(?1,?2,?3,?4,'post_message','accepted',?5,?6)",
            params![instance_id, seq, now, source_kind, CONDITION_VERSION, id],
        )
        .map_err(map_sql)?;
        set_meta(&tx, "event_seq", seq).map_err(map_sql)?;
        let rev = meta_i64(&tx, "revision").unwrap_or(0) + 1;
        set_meta(&tx, "revision", rev).map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        Ok(InsertOk {
            id,
            created_at: now.clone(),
            replay: false,
        })
    })();
    if wrote.is_err() && reserved {
        write_q.give_back();
    }
    wrote
}

fn insert_sim_event(
    conn: &mut Connection,
    data_dir: &Path,
    instance_id: &str,
    session: &str,
    action: &str,
    result: &str,
) -> Result<(), InsertErr> {
    let _ = instance_id;
    write_allowed(conn, data_dir)?;
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM sim_events", [], |r| r.get(0))
        .map_err(map_sql)?;
    if n >= MAX_SIM_EVENTS {
        return Err(InsertErr::Full);
    }
    conn.execute(
        "INSERT INTO sim_events(created_at, session, action, result, experiment_version)
         VALUES(?1,?2,?3,?4,'v5')",
        params![utc_now(), session, action, result],
    )
    .map_err(map_sql)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_full_is_not_busy() {
        let full = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(13), None);
        assert!(matches!(map_sql(full), InsertErr::SqliteFull));
        let busy = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(5), None);
        assert!(matches!(map_sql(busy), InsertErr::Busy));
        let locked = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(6), None);
        assert!(matches!(map_sql(locked), InsertErr::Busy));
    }
}
