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

pub(super) fn schema_ready(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('mail_source_enrollment') WHERE name='nonce') AND EXISTS(SELECT 1 FROM pragma_table_info('mail_source_snapshot') WHERE name='nonce') AND EXISTS(SELECT 1 FROM pragma_table_info('mail_source_enrollment') WHERE name='prior_lost') AND EXISTS(SELECT 1 FROM pragma_table_info('mail_source_enrollment') WHERE name='prior_loss')", [], |r| r.get(0))?)
}

pub(super) fn upgrade(conn: &Connection) -> Result<()> {
    for table in ["mail_source_enrollment", "mail_source_snapshot"] {
        let present: bool = conn.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name='nonce')"
            ),
            [],
            |r| r.get(0),
        )?;
        if !present {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN nonce TEXT"))?;
        }
    }
    for (name, kind) in [("prior_lost", "INTEGER"), ("prior_loss", "TEXT")] {
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('mail_source_enrollment') WHERE name=?1)",
            [name], |r| r.get(0),
        )?;
        if !present {
            conn.execute_batch(&format!(
                "ALTER TABLE mail_source_enrollment ADD COLUMN {name} {kind}"
            ))?;
        }
    }
    Ok(())
}

/// Negative uppers are enrollment fences, not published history.
pub(crate) fn published(home: &Connection) -> Result<bool> {
    Ok(home.query_row(
        "SELECT EXISTS(SELECT 1 FROM mail_source_snapshot WHERE only=1 AND upper_seq>=0)",
        [],
        |r| r.get(0),
    )?)
}

pub(crate) fn nonce(home: &Connection, binding: &str) -> Result<String> {
    let (old, nonce): (String, Option<String>) = home.query_row(
        "SELECT binding,nonce FROM mail_source_enrollment WHERE only=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(old == binding, "stale_generation");
    nonce.context("integrity_gap: enrollment predates instance binding; explicit recovery required")
}

/// A legacy inline baseline has no enrollment row. An off-path baseline must
/// have the same nonce in all three places (home marker, home upper, sidecar).
pub(crate) fn published_nonce(home: &Connection, binding: &str) -> Result<Option<String>> {
    ensure!(
        published(home)?,
        "backpressure: baseline enrollment pending off contact"
    );
    let enrolled: bool = home.query_row(
        "SELECT EXISTS(SELECT 1 FROM mail_source_enrollment)",
        [],
        |r| r.get(0),
    )?;
    if !enrolled {
        return Ok(None);
    }
    let instance = nonce(home, binding)?;
    let published: Option<String> = home.query_row(
        "SELECT nonce FROM mail_source_snapshot WHERE only=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        published.as_deref() == Some(&instance),
        "integrity_gap: published enrollment instance mismatch"
    );
    Ok(Some(instance))
}

/// Activating tracking is independent of publication. Writers append in their
/// own transaction. Failed pre-publication attempts are deactivated below.
pub(crate) fn begin(home: &Connection, binding: &str) -> Result<()> {
    let mode: String = home.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "backpressure: enrollment requires WAL"
    );
    let tx = rusqlite::Transaction::new_unchecked(home, TransactionBehavior::Immediate)?;
    ensure!(!tx.query_row("SELECT NOT EXISTS(SELECT 1 FROM mail_source_enrollment) AND EXISTS(SELECT 1 FROM mail_source_outbox)", [], |r| r.get::<_,bool>(0))?, "backpressure: abandoned enrollment cleanup pending");
    ensure!(
        !published(&tx)?,
        "integrity_gap: enrollment already published"
    );
    let instance = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO mail_source_enrollment(only,binding,nonce,prior_lost,prior_loss)
         VALUES(1,?1,?2,(SELECT lost FROM mail_source_health WHERE only=1),(SELECT value FROM metadata WHERE key='mail_source_loss'))
         ON CONFLICT(only) DO UPDATE SET nonce=excluded.nonce",
        params![binding, instance],
    )?;
    let old: String = tx.query_row(
        "SELECT binding FROM mail_source_enrollment WHERE only=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(old == binding, "stale_generation");
    // #514 writers test only row existence and therefore append deltas. Its
    // contacts read the deliberately impossible binding and stop stale before
    // any inline snapshot, staging or reclamation, without interfering with mail.
    tx.execute("INSERT INTO mail_source_snapshot(only,binding,upper_seq,copied_through,nonce) VALUES(1,?1,-1,-1,?2)
        ON CONFLICT(only) DO UPDATE SET binding=excluded.binding,upper_seq=-1,copied_through=-1,nonce=excluded.nonce",
        params![format!("enrolling:{instance}"),instance])?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn frozen(sidecar: &Connection, binding: &str, nonce: &str) -> Result<Option<i64>> {
    Ok(sidecar
        .query_row(
            "SELECT upper_seq FROM mail_source_snapshot WHERE only=1 AND upper_seq>=0 AND binding=?1 AND nonce=?2",
            params![binding, nonce],
            |r| r.get(0),
        )
        .optional()?)
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
    let read = rusqlite::Transaction::new_unchecked(home, TransactionBehavior::Deferred)?;
    let instance = nonce(&read, binding)?;
    if let Some(upper) = frozen(sidecar, binding, &instance)? {
        return Ok(upper);
    }
    let upper: i64 = read.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM mail_source_outbox",
        [],
        |r| r.get(0),
    )?;
    after_read();
    let tx = rusqlite::Transaction::new_unchecked(sidecar, TransactionBehavior::Immediate)?;
    // A restored/recreated home can leave a committed sidecar from a different
    // enrollment instance. Discard all of it before capturing the new snapshot.
    // This bounded deletion holds only the private sidecar's writer lock.
    tx.execute("DELETE FROM mail_source_snapshot", [])?;
    tx.execute("DELETE FROM mail_source_import", [])?;
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
    tx.execute("INSERT INTO mail_source_snapshot(only,binding,upper_seq,copied_through,nonce) VALUES(1,?1,?2,?2,?3)", params![binding,upper,instance])?;
    tx.commit()?;
    Ok(upper)
}

/// Only the immutable upper is installed in the home. History stays in the
/// sidecar; contacts never materialize or copy a whole baseline under a lock.
/// A crash before this commit resumes from the exact committed sidecar.
pub(crate) fn publish(home: &Connection, sidecar: &Connection, binding: &str) -> Result<i64> {
    let tx = rusqlite::Transaction::new_unchecked(home, TransactionBehavior::Immediate)?;
    let instance = nonce(&tx, binding)?;
    let upper = frozen(sidecar, binding, &instance)?
        .context("integrity_gap: baseline not frozen for enrollment instance")?;
    tx.execute("INSERT INTO mail_source_snapshot(only,binding,upper_seq,copied_through,nonce) VALUES(1,?1,?2,?2,?3)
        ON CONFLICT(only) DO UPDATE SET binding=excluded.binding,upper_seq=excluded.upper_seq,copied_through=excluded.copied_through,nonce=excluded.nonce
        WHERE mail_source_snapshot.upper_seq<0 AND mail_source_snapshot.nonce=excluded.nonce", params![binding,upper,instance])?;
    let (old, published, published_nonce): (String, i64, Option<String>) = tx.query_row(
        "SELECT binding,upper_seq,nonce FROM mail_source_snapshot WHERE only=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!(
        old == binding && published == upper && published_nonce.as_deref() == Some(&instance),
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
          WHERE seq<=(SELECT upper_seq FROM mail_source_snapshot WHERE only=1 AND upper_seq>=0)
           AND EXISTS(SELECT 1 FROM mail_source_enrollment e JOIN mail_source_snapshot s ON e.nonce=s.nonce AND e.binding=s.binding)
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

/// Stop tracking before reclaiming a failed unpublished attempt. No published
/// baseline or registry receipt relies on these observations: a later attempt
/// must capture all retained facts anew under a fresh nonce.
pub(crate) fn deactivate(home: &Connection) -> Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(home, TransactionBehavior::Immediate)?;
    if published(&tx)? {
        return Ok(());
    }
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM mail_source_enrollment)",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(());
    }
    // Restore only the pre-attempt loss state, never erase older provenance or
    // observation failures. NULL prior_lost denotes an older enrollment whose
    // original evidence was not recorded: retain current loss conservatively.
    let (prior_lost, prior_loss): (Option<i64>, Option<String>) = tx.query_row(
        "SELECT prior_lost,prior_loss FROM mail_source_enrollment WHERE only=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if let Some(lost) = prior_lost {
        tx.execute("UPDATE mail_source_health SET lost=?1 WHERE only=1", [lost])?;
        tx.execute("DELETE FROM metadata WHERE key='mail_source_loss'", [])?;
        if let Some(loss) = prior_loss {
            tx.execute(
                "INSERT INTO metadata(key,value) VALUES('mail_source_loss',?1)",
                [loss],
            )?;
        }
    }
    tx.execute("DELETE FROM mail_source_snapshot WHERE upper_seq<0", [])?;
    tx.execute("DELETE FROM mail_source_enrollment", [])?;
    tx.commit()?;
    Ok(())
}

/// Returns false until bounded pre-publication cleanup is complete. A worker
/// must not activate a new instance over observations from an aborted one.
pub(crate) fn clean_inactive(home: &Connection) -> Result<bool> {
    let dirty: bool = home.query_row("SELECT NOT EXISTS(SELECT 1 FROM mail_source_enrollment) AND NOT EXISTS(SELECT 1 FROM mail_source_snapshot WHERE upper_seq>=0) AND EXISTS(SELECT 1 FROM mail_source_outbox)", [], |r| r.get(0))?;
    if !dirty {
        return Ok(true);
    }
    let tx = rusqlite::Transaction::new_unchecked(home, TransactionBehavior::Immediate)?;
    let (count, bytes, upper): (i64, i64, i64) = tx.query_row("SELECT COUNT(*),COALESCE(SUM(length(CAST(event AS BLOB))),0),MAX(seq) FROM (SELECT seq,event FROM mail_source_outbox ORDER BY seq LIMIT 50)", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    tx.execute("DELETE FROM mail_source_outbox WHERE seq<=?1", [upper])?;
    tx.execute(
        "UPDATE mail_source_health SET entries=entries-?1,bytes=bytes-?2 WHERE only=1",
        params![count, bytes],
    )?;
    let done = !tx.query_row("SELECT EXISTS(SELECT 1 FROM mail_source_outbox)", [], |r| {
        r.get::<_, bool>(0)
    })?;
    tx.commit()?;
    Ok(done)
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
