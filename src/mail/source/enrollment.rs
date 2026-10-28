//! Off-contact enrollment. A short home commit enables transactional deltas;
//! a bounded WAL read snapshot freezes history AND its outbox upper into an
//! independent private database. Only a committed sidecar can be published.
//! No long home writer transaction, distributed transaction, or new mail route.
use super::*;
use std::{
    fs,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::Instant,
};

pub(crate) const RETRY_MS: i64 = 60_000;
pub(crate) fn path(home: &Path) -> PathBuf {
    home.join("mail-baseline.sqlite")
}
pub(crate) fn open(home: &Path, create: bool) -> Result<Connection> {
    crate::store::reject_symlinks(home)?;
    let path = path(home);
    crate::store::reject_symlinks(&path)?;
    if create {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
    }
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(&path)?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
        "unreadable: baseline sidecar permissions"
    );
    for suffix in ["-wal", "-shm"] {
        crate::store::reject_symlinks(&PathBuf::from(format!("{}{suffix}", path.display())))?;
    }
    let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(Duration::ZERO)?;
    Ok(conn)
}

/// Activating tracking is independent of publication. Writers already append
/// observations in their own transaction when this marker is present. Bounds
/// and the existing sticky loss marker apply even if enrollment never finishes.
pub(crate) fn begin(home: &Connection, binding: &str) -> Result<()> {
    let mode: String = home.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "backpressure: enrollment requires WAL"
    );
    let tx = rusqlite::Transaction::new_unchecked(home, TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT OR IGNORE INTO mail_source_enrollment(only,binding) VALUES(1,?1)",
        [binding],
    )?;
    let old: String = tx.query_row(
        "SELECT binding FROM mail_source_enrollment WHERE only=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(old == binding, "stale_generation");
    tx.commit()?;
    Ok(())
}

pub(crate) fn frozen(sidecar: &Connection, binding: &str) -> Result<Option<i64>> {
    let row: Option<(String, i64)> = sidecar
        .query_row(
            "SELECT binding,upper_seq FROM mail_source_snapshot WHERE only=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(old, upper)| {
        ensure!(old == binding, "stale_generation");
        Ok(upper)
    })
    .transpose()
}

/// Caller sets VM and Rust deadlines. The sidecar transaction is all-or-nothing
/// and is safe to reuse after process death. The read transaction establishes
/// its snapshot with the upper query before enumerating any home mail.
pub(crate) fn freeze(
    home: &Connection,
    sidecar: &Connection,
    binding: &str,
    deadline: Instant,
) -> Result<i64> {
    freeze_after(home, sidecar, binding, deadline, || {})
}

pub(crate) fn freeze_after(
    home: &Connection,
    sidecar: &Connection,
    binding: &str,
    deadline: Instant,
    after_read: impl FnOnce(),
) -> Result<i64> {
    if let Some(upper) = frozen(sidecar, binding)? {
        return Ok(upper);
    }
    let old: String = home.query_row(
        "SELECT binding FROM mail_source_enrollment WHERE only=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(old == binding, "stale_generation");
    let read = rusqlite::Transaction::new_unchecked(home, TransactionBehavior::Deferred)?;
    let upper: i64 = read.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM mail_source_outbox",
        [],
        |r| r.get(0),
    )?;
    after_read();
    let tx = rusqlite::Transaction::new_unchecked(sidecar, TransactionBehavior::Immediate)?;
    let mut stmt = read.prepare("SELECT id FROM mail WHERE sender='captain' OR recipient='captain' ORDER BY seq LIMIT 10001")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        ids.len() <= LIMIT as usize,
        "baseline_too_large: retained history"
    );
    let mut bytes = 0usize;
    for id in ids {
        ensure!(
            work_now() < deadline,
            "baseline_too_large: snapshot work budget"
        );
        let f = fact(&read, &id, None)?.context("snapshot mail vanished")?;
        let event = serde_json::to_string(&f)?;
        bytes = bytes.saturating_add(event.len());
        ensure!(
            bytes <= BYTES as usize && event.len() <= ITEM_BYTES,
            "baseline_too_large: retained history bytes"
        );
        tx.execute(
            "INSERT INTO mail_source_import(intent,event) VALUES(?1,?2)",
            params![uuid::Uuid::new_v4().to_string(), event],
        )?;
    }
    drop(stmt);
    tx.execute("INSERT INTO mail_source_snapshot(only,binding,upper_seq,copied_through) VALUES(1,?1,?2,?2)", params![binding,upper])?;
    tx.commit()?;
    Ok(upper)
}

/// Only the immutable upper is installed in the home. History stays in the
/// sidecar; contacts never materialize or copy a whole baseline under a lock.
/// A crash before this commit resumes from the exact committed sidecar.
pub(crate) fn publish(home: &Connection, sidecar: &Connection, binding: &str) -> Result<i64> {
    let upper = frozen(sidecar, binding)?.context("backpressure: baseline not frozen")?;
    let tx = rusqlite::Transaction::new_unchecked(home, TransactionBehavior::Immediate)?;
    let old: String = tx.query_row(
        "SELECT binding FROM mail_source_enrollment WHERE only=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(old == binding, "stale_generation");
    tx.execute("INSERT OR IGNORE INTO mail_source_snapshot(only,binding,upper_seq,copied_through) VALUES(1,?1,?2,?2)", params![binding,upper])?;
    let (old, published): (String, i64) = tx.query_row(
        "SELECT binding,upper_seq FROM mail_source_snapshot WHERE only=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        old == binding && published == upper,
        "integrity_gap: baseline publication"
    );
    tx.commit()?;
    Ok(upper)
}

/// These pre-upper observations are represented by the committed baseline.
/// Reclaim at most 50 per idle unit, including admission counters. Never touch
/// later deltas or mail, and never do a history-sized deletion under a lock.
pub(crate) fn reclaim_covered_deltas(home: &Connection) -> Result<()> {
    let (count, bytes, through): (i64, i64, Option<i64>) = home.query_row(
        "SELECT COUNT(*),COALESCE(SUM(length(CAST(event AS BLOB))),0),MAX(seq)
         FROM (SELECT seq,event FROM mail_source_outbox
          WHERE seq<=(SELECT upper_seq FROM mail_source_snapshot WHERE only=1)
           AND EXISTS(SELECT 1 FROM mail_source_enrollment)
          ORDER BY seq LIMIT 50)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let Some(through) = through else {
        return Ok(());
    };
    let tx = rusqlite::Transaction::new_unchecked(home, TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM mail_source_outbox WHERE seq<=?1", [through])?;
    tx.execute(
        "UPDATE mail_source_health SET entries=entries-?1,bytes=bytes-?2 WHERE only=1",
        params![count, bytes],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn init_sidecar(conn: &Connection) -> Result<()> {
    // Fixed disk admission in addition to the serialized-history bound. SQLite
    // may retain free pages after rollback, but never an unbounded sidecar.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA max_page_count=32768;",
    )?;
    init(conn)?;
    init_intake(conn)?;
    Ok(())
}
