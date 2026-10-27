//! Bounded, offset-addressed batches. Old single-file endpoints remain for old
//! coordinators; new coordinators negotiate this protocol before stopping a ship.
use super::*;

const WIRE_LIMIT: usize = 768 * 1024;
const MAX_ENTRIES: usize = 1024;
const ATTEMPTS: u32 = 3;
const STALE_MS: i64 = 24 * 60 * 60 * 1000;

pub(super) fn protocol() -> Value {
    json!({"transfer": 1})
}

pub(super) fn negotiate(source: &Side, target: &Side) -> Result<()> {
    for side in [source, target] {
        let answer = retry(side, "move.protocol", json!({}), None).context(
            "batched moves require current privateer on both machines; upgrade before moving",
        )?;
        ensure!(
            answer["transfer"] == 1,
            "incompatible move transfer protocol; upgrade both machines"
        );
    }
    Ok(())
}

pub(super) fn retry(
    side: &Side,
    command: &str,
    args: Value,
    timeout: Option<Duration>,
) -> Result<Value> {
    for attempt in 0..ATTEMPTS {
        match side.call(command, args.clone(), timeout) {
            Ok(answer) => return Ok(answer),
            Err(error) if attempt + 1 == ATTEMPTS => {
                return Err(error.context(format!("{command} failed after {ATTEMPTS} attempts")));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(250 << attempt)),
        }
    }
    unreachable!()
}

/// Budget both data and escaped path/offset metadata, not just raw bytes.
fn batches(manifest: &Value) -> Result<Vec<Vec<Value>>> {
    let mut batches = vec![];
    let (mut batch, mut raw, mut wire) = (vec![], 0usize, 128usize);
    for file in manifest["files"].as_array().context("files are listed")? {
        let path = required(file, "path")?;
        let size = file["size"].as_u64().context("file size")?;
        let mut offset = 0;
        loop {
            let length = (size - offset).min(CHUNK as u64) as usize;
            let entry = json!({"path": path, "offset": offset, "length": length});
            let cost = entry.to_string().len() + length * 2 + 32;
            ensure!(
                cost + 128 <= WIRE_LIMIT,
                "move path metadata exceeds batch limit"
            );
            if !batch.is_empty()
                && (raw + length > CHUNK || wire + cost > WIRE_LIMIT || batch.len() == MAX_ENTRIES)
            {
                batches.push(std::mem::take(&mut batch));
                raw = 0;
                wire = 128;
            }
            batch.push(entry);
            raw += length;
            wire += cost;
            offset += length as u64;
            if offset == size {
                break;
            }
        }
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    Ok(batches)
}

pub(super) fn copy(source: &Side, target: &Side, id: &str, manifest: &Value) -> Result<()> {
    for entries in batches(manifest)? {
        let answer = retry(
            source,
            "move.read_batch",
            json!({"id": id, "entries": entries}),
            None,
        )?;
        let chunks = answer["chunks"]
            .as_array()
            .context("batch chunks are required")?;
        ensure!(chunks.len() == entries.len(), "incomplete move batch");
        for (chunk, entry) in chunks.iter().zip(&entries) {
            ensure!(
                chunk["path"] == entry["path"]
                    && chunk["offset"] == entry["offset"]
                    && chunk["hex"].as_str().context("hex is required")?.len() as u64
                        == entry["length"].as_u64().unwrap() * 2,
                "move batch does not match the requested ranges"
            );
        }
        retry(
            target,
            "move.write_batch",
            json!({"id": id, "chunks": chunks}),
            None,
        )?;
    }
    Ok(())
}

fn bounded<'a>(args: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    ensure!(
        args.to_string().len() <= WIRE_LIMIT,
        "move batch exceeds wire limit"
    );
    let entries = args[key].as_array().context("batch entries are required")?;
    ensure!(
        !entries.is_empty() && entries.len() <= MAX_ENTRIES,
        "invalid move batch count"
    );
    Ok(entries)
}

pub(super) fn read_batch(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let entries = bounded(args, "entries")?;
    let conn = crate::fleet::registry()?;
    let (state, detail) = exported(&conn, id)?;
    ensure!(state == "exported", "move {id} is {state} here");
    let mut total = 0;
    let mut chunks = vec![];
    for entry in entries {
        let path = required(entry, "path")?;
        let offset = entry["offset"].as_u64().context("offset is required")?;
        let length = entry["length"].as_u64().context("length is required")?;
        total += length;
        let size = listed(&detail["manifest"], path)?["size"]
            .as_u64()
            .context("file size")?;
        ensure!(
            total <= CHUNK as u64 && offset <= size && length <= size - offset,
            "invalid move batch range"
        );
        let mut file = fs::File::open(staging(id, "export")?.join(relative(path)?))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; length as usize];
        file.read_exact(&mut bytes)?;
        chunks.push(json!({"path": path, "offset": offset, "hex": hex(&bytes)}));
    }
    let answer = json!({"chunks": chunks});
    ensure!(
        answer.to_string().len() <= WIRE_LIMIT,
        "move batch response exceeds wire limit"
    );
    Ok(answer)
}

pub(super) fn write_batch(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let chunks = bounded(args, "chunks")?;
    let mut conn = crate::fleet::registry()?;
    // Serialize against discard/expiry. A timed-out writer cannot recreate a
    // stage after cleanup; a lost response can safely replay identical offsets.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (state, detail) = imported(&tx, id)?;
    ensure!(state == "receiving", "move {id} is already {state} here");
    let mut total = 0usize;
    for chunk in chunks {
        let path = required(chunk, "path")?;
        let offset = chunk["offset"].as_u64().context("offset is required")?;
        let bytes = unhex(chunk["hex"].as_str().context("hex is required")?)?;
        total += bytes.len();
        ensure!(total <= CHUNK, "move batch exceeds byte limit");
        write_chunk(&detail["manifest"], id, path, offset, &bytes)?;
    }
    tx.execute(
        "UPDATE moves SET updated_ms=?2 WHERE id=?1 AND role='import'",
        params![id, Store::now_ms()],
    )?;
    tx.commit()?;
    Ok(json!({"written": total}))
}

/// Lazy target-side expiry runs whenever the fleet registry is opened (also
/// by dispatcher passes). Only receiving stages expire, never placed/active ships.
pub(super) fn expire(conn: &Connection) -> Result<()> {
    let cutoff = Store::now_ms() - STALE_MS;
    let ids = conn
        .prepare(
            "SELECT id FROM moves WHERE role='import' AND state='receiving' AND updated_ms<?1",
        )?
        .query_map([cutoff], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if ids.is_empty() {
        return Ok(());
    }
    // init defers expiry errors. In particular a busy COMMIT must roll back
    // on drop, not leave unrelated registry writes inside this transaction.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    for id in ids {
        let stale: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM moves WHERE id=?1 AND role='import' AND state='receiving' AND updated_ms<?2)", params![id, cutoff], |r| r.get(0))?;
        if stale {
            let state = match discard_staging(&id, "import") {
                Ok(()) => "discarded",
                Err(error) => {
                    // Keep receiving but back off for another expiry interval,
                    // rather than walk a blocked tree on every registry open.
                    eprintln!("move {id} stage expiry deferred: {error:#}");
                    "receiving"
                }
            };
            tx.execute(
                "UPDATE moves SET state=?2, updated_ms=?3 WHERE id=?1 AND role='import'",
                params![id, state, Store::now_ms()],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_expiry_commit_failure_does_not_swallow_later_registry_writes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("registry.sqlite");
        let conn = Connection::open(&path).unwrap();
        super::super::init(&conn).unwrap();
        // An invalid stage id reliably defers deletion without touching any
        // real stage. Expiry still updates its retry clock in the transaction.
        conn.execute(
            "INSERT INTO moves VALUES('invalid/id','import','scout','receiving','{}',0)",
            [],
        )
        .unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN; SELECT * FROM moves;").unwrap();
        conn.busy_timeout(Duration::ZERO).unwrap();
        let error = expire(&conn).unwrap_err();
        assert!(error.to_string().contains("locked"), "{error:#}");
        assert!(
            conn.is_autocommit(),
            "failed commit must drop its transaction"
        );
        // init swallows exactly this error. It must also return a clean conn.
        super::super::init(&conn).unwrap();
        assert!(conn.is_autocommit());
        reader.execute_batch("ROLLBACK").unwrap();
        put(&conn, "later", "run", "scout", "started", &json!({})).unwrap();
        drop(conn);
        let reopened = Connection::open(&path).unwrap();
        assert_eq!(
            row(&reopened, "later", "run").unwrap().unwrap().0,
            "started"
        );
    }

    #[test]
    fn batches_bound_data_metadata_and_include_empty_files() {
        let files: Vec<_> = (0..1120)
            .map(|i| json!({"path": format!("notes/{i}"), "size": 100}))
            .collect();
        let planned = batches(&json!({"files": files})).unwrap();
        assert_eq!(planned.len(), 2); // entry cap, not 1,120 round trips
        assert_eq!(planned.iter().map(Vec::len).sum::<usize>(), 1120);
        let planned = batches(
            &json!({"files": [{"path":"empty", "size":0}, {"path":"large", "size":CHUNK*3+1}]}),
        )
        .unwrap();
        assert_eq!(planned.len(), 4);
        assert_eq!(planned[0][0]["length"], 0);
        for batch in planned {
            let raw: u64 = batch.iter().map(|v| v["length"].as_u64().unwrap()).sum();
            assert!(raw <= CHUNK as u64);
        }
    }
}
