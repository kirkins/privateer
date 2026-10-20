//! Files the captain sends to ships.
//!
//! `file.put {to, name, content_file}` stores a file in the receiving ship's
//! own home, as `files/<file_id>/<name>`, with a row recording its owner,
//! name, size, type and sha256. It answers with the `file_id` and the `path`
//! on the ship's machine. A ship on a linked machine gets the same file in
//! chunks over the machine link: `file.begin` declares the name, size and
//! sha256, each `file.append` sends one chunk at its offset (well under the
//! gateway's 1 MiB line), and `file.commit` checks the size and sha256 before
//! the file is made. An upload nobody commits expires and is cleaned up.
//!
//! The captain's mail names files as `payload.files: [{file_id}]`, nothing
//! more: every other field is the runtime's. `mail.send` checks each entry
//! and fills it in — from the home that hosts the ship when it is on this
//! machine, else by asking that machine with `file.describe` — and mail that
//! crosses a machine link is checked again where it lands, against the rows
//! of the home that reads it. A ship reads a file from `path`.
use crate::machines;
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const CAPTAIN: &str = crate::mail::CAPTAIN;
pub const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// Stored files kept per ship; the oldest beyond this are removed.
const KEEP_PER_SHIP: i64 = 200;
/// The bytes one ship's stored files may total; the oldest beyond it go.
const SHIP_FILE_BUDGET_BYTES: i64 = 1024 * 1024 * 1024;
/// The raw bytes one `file.append` carries.
const CHUNK_BYTES: usize = 512 * 1024;
/// How long an upload may wait for its commit before it is cleaned up.
const UPLOAD_TTL_MS: i64 = 60 * 60 * 1000;
const MAX_NAME_CHARS: usize = 128;
const MAX_FILES_PER_MESSAGE: usize = 20;

thread_local! {
    /// The files runtime's clock, so a test can try expiry and pruning order
    /// by moving its own thread's time without touching anything real.
    static CLOCK_SHIFT_MS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

/// The clock the files runtime runs on.
fn now_ms() -> i64 {
    CLOCK_SHIFT_MS.with(|shift| Store::now_ms() + shift.get())
}

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS files(file_id TEXT PRIMARY KEY, ship TEXT NOT NULL,
           name TEXT NOT NULL, bytes INTEGER NOT NULL, type TEXT NOT NULL, sha256 TEXT NOT NULL,
           created_ms INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS files_ship ON files(ship, created_ms);
         CREATE TABLE IF NOT EXISTS file_uploads(upload_id TEXT PRIMARY KEY, ship TEXT NOT NULL,
           name TEXT NOT NULL, bytes INTEGER NOT NULL, sha256 TEXT NOT NULL,
           received INTEGER NOT NULL, created_ms INTEGER NOT NULL);",
    )?;
    Ok(())
}

pub fn dispatch(store: &Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "file.put" => put(store, &args),
        _ => bail!("unknown file command {command}"),
    }
}

/// The base name a file is stored under: what follows the last separator, and
/// never a dot-file, so a name cannot leave its ship's folder.
pub(crate) fn file_name(raw: &str) -> Result<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    ensure!(
        !base.is_empty()
            && !base.starts_with('.')
            && base.chars().count() <= MAX_NAME_CHARS
            && !base.chars().any(char::is_control),
        "file name must be a base name of 1 to {MAX_NAME_CHARS} characters, not starting with a dot"
    );
    Ok(base.to_string())
}

/// A file's MIME type from its extension; anything unknown is opaque bytes.
fn mime_type(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// The home on this machine that hosts `ship`, when its folder is present.
fn local_home(ship: &str) -> Result<Option<PathBuf>> {
    Ok(crate::captain::fleet()?
        .into_iter()
        .find(|(name, _)| name == ship)
        .map(|(_, home)| home))
}

/// Whether this store's home is the one that hosts `ship`: its own record
/// says it lives here, so its files are this store's rows.
fn hosted_here(store: &Store, ship: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1
               AND json_extract(record,'$.location')='here')",
        [ship],
        |r| r.get(0),
    )?)
}

fn staging_dir(home: &Path) -> PathBuf {
    home.join("files").join("uploads")
}

fn staging_path(home: &Path, upload_id: &str) -> PathBuf {
    staging_dir(home).join(format!("{upload_id}.part"))
}

fn stored_path(home: &Path, file_id: &str, name: &str) -> PathBuf {
    home.join("files").join(file_id).join(name)
}

fn entry(home: &Path, file_id: &str, name: &str, bytes: u64, mime: &str) -> Value {
    json!({"file_id": file_id, "name": name, "bytes": bytes, "type": mime,
        "path": stored_path(home, file_id, name)})
}

/// Why a file this home does not hold cannot ride mail for `ship`: another
/// ship owns it, or it is `ship`'s but lives in `ship`'s home, not this one —
/// which names both ships either way. `home` names this home's own ship.
fn foreign_error(file_id: &str, owner: &str, ship: &str, home: Option<&str>) -> String {
    if owner == ship {
        return match home {
            Some(home) => format!(
                "file {file_id} is {ship}'s, in {ship}'s home on this machine, \
                 not in {home}'s; write the captain's mail to {ship} in its own home"
            ),
            None => format!(
                "file {file_id} is {ship}'s, in {ship}'s home on this machine, \
                 not in this one; write the captain's mail to {ship} in its own home"
            ),
        };
    }
    format!("file {file_id} is {owner}'s, not {ship}'s")
}

/// This home's own ship, the one whose mail it holds.
fn home_ship(store: &Store) -> Option<String> {
    store
        .connection()
        .query_row(
            "SELECT name FROM ships WHERE json_extract(record,'$.location')='here' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
}

/// A stored file for `ship`, from this home. Another ship's file is refused by
/// name, and an unknown one likewise.
fn lookup(store: &Store, ship: &str, file_id: &str) -> Result<Value> {
    identifier(file_id)?;
    let found: Option<(String, String, i64, String)> = store
        .connection()
        .query_row(
            "SELECT ship,name,bytes,type FROM files WHERE file_id=?1",
            [file_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((owner, name, bytes, mime)) = found else {
        let owner = owner_elsewhere(file_id, store.home())
            .with_context(|| format!("no file {file_id} on this machine"))?;
        bail!(
            "{}",
            foreign_error(file_id, &owner, ship, home_ship(store).as_deref())
        );
    };
    ensure!(
        owner == ship,
        "{}",
        foreign_error(file_id, &owner, ship, home_ship(store).as_deref())
    );
    Ok(entry(store.home(), file_id, &name, bytes as u64, &mime))
}

/// The ship a file belongs to, when another ship's home on this machine holds it.
fn owner_elsewhere(file_id: &str, here: &Path) -> Option<String> {
    crate::captain::fleet()
        .ok()?
        .into_iter()
        .filter(|(_, home)| home != here)
        .find_map(|(_, home)| {
            let other = Store::open_readonly(&home, Caller::Operator).ok()?;
            other
                .connection()
                .query_row("SELECT ship FROM files WHERE file_id=?1", [file_id], |r| {
                    r.get(0)
                })
                .optional()
                .ok()?
        })
}

/// `payload.files` as a bounded list of objects that name a file, either as
/// `{file_id}` or filled in with its fields.
fn references(files: &Value) -> Result<&Vec<Value>> {
    let files = files.as_array().context("payload.files must be an array")?;
    ensure!(
        files.len() <= MAX_FILES_PER_MESSAGE,
        "payload.files holds at most {MAX_FILES_PER_MESSAGE} files"
    );
    for file in files {
        let object = file
            .as_object()
            .context("each payload.files entry is {file_id}")?;
        ensure!(
            object
                .keys()
                .all(|k| matches!(k.as_str(), "file_id" | "name" | "bytes" | "type" | "path"))
                && object.get("file_id").is_some_and(Value::is_string),
            "each payload.files entry is {{file_id}}"
        );
    }
    Ok(files)
}

/// A filled-in entry carries every field the runtime gives it.
fn filled(file: &Value) -> bool {
    file["name"].is_string()
        && file["bytes"].is_u64()
        && file["type"].is_string()
        && file["path"].is_string()
}

/// Check `payload.files` on mail the captain sends, which only the operator
/// writes: each entry names a file as `{file_id}` alone, and the runtime
/// fills in the rest. A ship hosted in this home has its file read from this
/// store's own row; a ship behind a machine link is asked there with
/// `file.describe`, so the mail leaves carrying the path the ship will read.
/// A caller never supplies the filled-in fields, whatever the ship's home.
pub(crate) fn check_files(store: &Store, from: &str, to: &str, payload: &mut Value) -> Result<()> {
    let Some(files) = payload.get("files") else {
        return Ok(());
    };
    ensure!(
        from == CAPTAIN,
        "payload.files is the captain's, on mail to a ship"
    );
    let hosted = hosted_here(store, to)?;
    let mut checked = vec![];
    for file in references(files)? {
        let file_id = file["file_id"].as_str().unwrap_or_default();
        ensure!(
            file.as_object().is_some_and(|fields| fields.len() == 1),
            "payload.files entry {file_id} carries more than file_id; \
             name a file as {{file_id}} and the runtime fills in the rest"
        );
        checked.push(if hosted {
            lookup(store, to, file_id)?
        } else {
            machines::ship_call(to, "file.describe", json!({"ship": to, "file_id": file_id}))?
        });
    }
    payload["files"] = Value::Array(checked);
    Ok(())
}

/// Check `payload.files` on the captain's mail that crossed a machine link:
/// the machine that sent it filled each entry in, and this home, which hosts
/// the ship, replaces every one with what its own rows say, so nothing the
/// sending side wrote about a file's place is kept.
pub(crate) fn check_delivered(store: &Store, to: &str, payload: &mut Value) -> Result<()> {
    let Some(files) = payload.get("files") else {
        return Ok(());
    };
    let mut checked = vec![];
    for file in references(files)? {
        let file_id = file["file_id"].as_str().unwrap_or_default();
        ensure!(
            file.get("path").is_none() || filled(file),
            "payload.files entry {file_id} is incomplete"
        );
        checked.push(lookup(store, to, file_id)?);
    }
    payload["files"] = Value::Array(checked);
    Ok(())
}

/// The files on a captain's mail, one line each for the wake summary.
pub(crate) fn summary(payload: &Value) -> String {
    payload["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| {
            Some(format!(
                "\n  file {} ({} bytes, {}) at {}",
                file["name"].as_str()?,
                file["bytes"].as_u64()?,
                file["type"].as_str()?,
                file["path"].as_str()?
            ))
        })
        .collect()
}

/// The captain stores a file for a ship. A ship here gets it in its own home
/// directly; a ship on a linked machine gets it in chunks over that link.
fn put(store: &Store, args: &Value) -> Result<Value> {
    ensure!(
        *store.caller() == Caller::Operator,
        "only the captain puts files with file.put"
    );
    let to = required(args, "to")?;
    identifier(to)?;
    let name = file_name(required(args, "name")?)?;
    let source = Path::new(required(args, "content_file")?);
    let (bytes, sha256) = digest_file(source)?;
    let mut stored = if local_home(to)?.is_some() {
        send_file(
            source,
            to,
            &name,
            bytes,
            &sha256,
            &|command: &str, args: Value| serve(command, &args),
        )?
    } else {
        send_file(
            source,
            to,
            &name,
            bytes,
            &sha256,
            &|command: &str, args: Value| machines::ship_call(to, command, args),
        )?
    };
    store.audit(
        "file.put",
        &json!({"ship": to, "file_id": stored["file_id"], "bytes": bytes}),
    )?;
    stored["ship"] = json!(to);
    Ok(stored)
}

/// The file's size and sha256, read from the operator's own file.
fn digest_file(source: &Path) -> Result<(u64, String)> {
    ensure!(
        source.is_absolute(),
        "content_file must be an absolute path"
    );
    let file = File::open(source)
        .with_context(|| format!("cannot open content_file {}", source.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "content_file must be a regular file"
    );
    digest(file)
}

fn digest(mut reader: impl Read) -> Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = vec![0u8; CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        ensure!(total <= MAX_FILE_BYTES, "a file is at most 50 MiB");
        hasher.update(&buffer[..read]);
    }
    Ok((total, hex(hasher.finalize())))
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Send a file through `call`: begin, one append per chunk, then commit. The
/// same steps serve a home here and a ship behind a machine link.
fn send_file(
    source: &Path,
    to: &str,
    name: &str,
    bytes: u64,
    sha256: &str,
    call: &dyn Fn(&str, Value) -> Result<Value>,
) -> Result<Value> {
    let begun = call(
        "file.begin",
        json!({"ship": to, "name": name, "bytes": bytes, "sha256": sha256}),
    )?;
    let upload_id = begun["upload_id"]
        .as_str()
        .context("file.begin gave no upload_id")?
        .to_string();
    let mut file = File::open(source)?;
    let mut buffer = vec![0u8; CHUNK_BYTES];
    let mut offset = 0u64;
    loop {
        let read = read_chunk(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        call(
            "file.append",
            json!({"ship": to, "upload_id": upload_id, "offset": offset,
                "data": base64_encode(&buffer[..read])}),
        )?;
        offset += read as u64;
    }
    ensure!(
        offset == bytes,
        "{} changed while it was sent; send it again",
        source.display()
    );
    call("file.commit", json!({"ship": to, "upload_id": upload_id}))
}

/// Fill `buffer` from `file` unless the file ends first.
fn read_chunk(file: &mut File, buffer: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

/// The gateway's file commands, for a ship hosted on this machine. The gateway
/// has checked that the caller is the operator.
pub(crate) fn serve(command: &str, args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    identifier(ship)?;
    let home =
        local_home(ship)?.with_context(|| format!("no ship named {ship} on this machine"))?;
    let store = Store::open(&home)?;
    match command {
        "file.begin" => begin(&store, ship, args),
        "file.append" => append(&store, ship, args),
        "file.commit" => commit(&store, ship, args),
        "file.describe" => {
            let file_id = required(args, "file_id")?;
            lookup(&store, ship, file_id)
        }
        _ => bail!("unknown file command {command}"),
    }
}

/// An upload being received for a ship.
struct Upload {
    id: String,
    name: String,
    bytes: u64,
    sha256: String,
    received: u64,
}

fn upload(store: &Store, ship: &str, args: &Value) -> Result<Upload> {
    let id = required(args, "upload_id")?;
    identifier(id)?;
    store
        .connection()
        .query_row(
            "SELECT name,bytes,sha256,received FROM file_uploads WHERE upload_id=?1 AND ship=?2",
            params![id, ship],
            |r| {
                Ok(Upload {
                    id: id.to_string(),
                    name: r.get(0)?,
                    bytes: r.get::<_, i64>(1)? as u64,
                    sha256: r.get(2)?,
                    received: r.get::<_, i64>(3)? as u64,
                })
            },
        )
        .optional()?
        .with_context(|| format!("no upload {id} for {ship}; it may have expired"))
}

/// Remove uploads nobody committed in time, with what they staged, and any
/// staged part whose row is gone (a crash between staging and recording).
fn sweep_uploads(store: &Store) -> Result<()> {
    let cutoff = now_ms() - UPLOAD_TTL_MS;
    let expired: Vec<String> = {
        let mut statement = store
            .connection()
            .prepare("SELECT upload_id FROM file_uploads WHERE created_ms < ?1")?;
        statement
            .query_map([cutoff], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for id in &expired {
        // A staged file already gone is what cleanup wants.
        let _ = fs::remove_file(staging_path(store.home(), id));
    }
    store
        .connection()
        .execute("DELETE FROM file_uploads WHERE created_ms < ?1", [cutoff])?;
    // A staged part with no row is debris: nothing refers to it, and the
    // folder it waits in is this home's own.
    let live: HashSet<String> = store
        .connection()
        .prepare("SELECT upload_id FROM file_uploads")?
        .query_map([], |r| {
            Ok(staging_path(store.home(), &r.get::<_, String>(0)?)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default())
        })?
        .collect::<rusqlite::Result<_>>()?;
    if let Ok(entries) = fs::read_dir(staging_dir(store.home())) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !live.contains(&name) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

fn begin(store: &Store, ship: &str, args: &Value) -> Result<Value> {
    let name = file_name(required(args, "name")?)?;
    let bytes = args["bytes"]
        .as_u64()
        .context("bytes must be a whole number")?;
    ensure!(bytes <= MAX_FILE_BYTES, "a file is at most 50 MiB");
    let sha256 = required(args, "sha256")?;
    ensure!(is_sha256(sha256), "sha256 must be 64 lowercase hex digits");
    sweep_uploads(store)?;
    let id = format!("upload-{}", uuid::Uuid::new_v4());
    fs::create_dir_all(staging_dir(store.home()))?;
    File::create(staging_path(store.home(), &id))?;
    store.connection().execute(
        "INSERT INTO file_uploads(upload_id,ship,name,bytes,sha256,received,created_ms)
         VALUES(?1,?2,?3,?4,?5,0,?6)",
        params![id, ship, name, bytes as i64, sha256, now_ms()],
    )?;
    Ok(json!({"upload_id": id}))
}

fn append(store: &Store, ship: &str, args: &Value) -> Result<Value> {
    sweep_uploads(store)?;
    let upload = upload(store, ship, args)?;
    let offset = args["offset"]
        .as_u64()
        .context("offset must be a whole number")?;
    let data = base64_decode(required(args, "data")?)?;
    ensure!(
        !data.is_empty() && data.len() <= CHUNK_BYTES,
        "a chunk is 1 byte to 512 KiB"
    );
    ensure!(
        offset == upload.received,
        "chunk offset {offset} is not the next byte, {}",
        upload.received
    );
    ensure!(
        upload.received + data.len() as u64 <= upload.bytes,
        "the chunks are longer than the declared size"
    );
    let mut staged = fs::OpenOptions::new()
        .append(true)
        .open(staging_path(store.home(), &upload.id))?;
    staged.write_all(&data)?;
    let received = upload.received + data.len() as u64;
    store.connection().execute(
        "UPDATE file_uploads SET received=?2 WHERE upload_id=?1",
        params![upload.id, received as i64],
    )?;
    Ok(json!({"received": received}))
}

fn commit(store: &Store, ship: &str, args: &Value) -> Result<Value> {
    sweep_uploads(store)?;
    let upload = upload(store, ship, args)?;
    let staged = staging_path(store.home(), &upload.id);
    ensure!(
        upload.received == upload.bytes,
        "upload {} has {} of {} bytes",
        upload.id,
        upload.received,
        upload.bytes
    );
    let (bytes, sha256) = digest(File::open(&staged)?)?;
    if bytes != upload.bytes || sha256 != upload.sha256 {
        // The bytes are not the ones declared: nothing of this upload is kept.
        let _ = fs::remove_file(&staged);
        store
            .connection()
            .execute("DELETE FROM file_uploads WHERE upload_id=?1", [&upload.id])?;
        bail!(
            "upload {} does not match its declared size and sha256; it was discarded",
            upload.id
        );
    }
    let file_id = format!("file-{}", uuid::Uuid::new_v4());
    let mime = mime_type(&upload.name);
    let tx = store.write_transaction()?;
    tx.execute(
        "INSERT INTO files(file_id,ship,name,bytes,type,sha256,created_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![file_id, ship, upload.name, bytes as i64, mime, sha256, now_ms()],
    )?;
    tx.execute("DELETE FROM file_uploads WHERE upload_id=?1", [&upload.id])?;
    let pruned = prune(&tx, ship)?;
    let dir = store.home().join("files").join(&file_id);
    fs::create_dir_all(&dir)?;
    fs::rename(&staged, dir.join(&upload.name))?;
    tx.commit()?;
    for old in &pruned {
        // A leftover folder is harmless once its row is gone.
        let _ = fs::remove_dir_all(store.home().join("files").join(old));
    }
    store.audit(
        "file.commit",
        &json!({"ship": ship, "file_id": file_id, "bytes": bytes, "pruned": pruned.len()}),
    )?;
    Ok(entry(store.home(), &file_id, &upload.name, bytes, mime))
}

/// Keep each ship's newest files, within both its count and its bytes: the
/// rows beyond either go, oldest first, and their ids are returned so their
/// folders can follow.
fn prune(tx: &Transaction, ship: &str) -> Result<Vec<String>> {
    let rows: Vec<(String, i64)> = {
        let mut statement = tx.prepare(
            "SELECT file_id, bytes FROM files WHERE ship=?1 ORDER BY created_ms ASC, rowid ASC",
        )?;
        statement
            .query_map([ship], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut total: i64 = rows.iter().map(|(_, bytes)| bytes).sum();
    let mut count = rows.len() as i64;
    let mut stale = vec![];
    for (file_id, bytes) in rows {
        if count <= KEEP_PER_SHIP && total <= SHIP_FILE_BUDGET_BYTES {
            break;
        }
        stale.push(file_id);
        total -= bytes;
        count -= 1;
    }
    for file_id in &stale {
        tx.execute("DELETE FROM files WHERE file_id=?1", [file_id])?;
    }
    Ok(stale)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[((group >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    let bytes = text.as_bytes();
    ensure!(bytes.len() % 4 == 0, "data is not base64");
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, quad) in bytes.chunks(4).enumerate() {
        let last = (index + 1) * 4 == bytes.len();
        let mut group = 0u32;
        let mut padding = 0usize;
        for (i, &c) in quad.iter().enumerate() {
            if c == b'=' {
                ensure!(last && i >= 2, "data is not base64");
                padding += 1;
                group <<= 6;
            } else {
                ensure!(padding == 0, "data is not base64");
                let value = BASE64
                    .iter()
                    .position(|&b| b == c)
                    .context("data is not base64")?;
                group = (group << 6) | value as u32;
            }
        }
        let produced = 3 - padding;
        out.extend_from_slice(&group.to_be_bytes()[1..1 + produced]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(plain.as_bytes()), coded);
            assert_eq!(base64_decode(coded).unwrap(), plain.as_bytes());
        }
        for bad in ["Zg=", "Z===", "Zm=v", "Zm9v!", "Zg"] {
            assert!(base64_decode(bad).is_err(), "{bad} decoded");
        }
    }

    #[test]
    fn names_are_base_names_without_leading_dots() {
        assert_eq!(file_name("report.pdf").unwrap(), "report.pdf");
        assert_eq!(file_name("/tmp/x/plan.md").unwrap(), "plan.md");
        assert_eq!(file_name("C:\\docs\\a.txt").unwrap(), "a.txt");
        assert_eq!(file_name("../secret").unwrap(), "secret");
        let long = "n".repeat(129);
        for bad in ["", "dir/", ".env", "a\nb", long.as_str()] {
            assert!(file_name(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn types_come_from_the_extension() {
        assert_eq!(mime_type("Plan.PDF"), "application/pdf");
        assert_eq!(mime_type("notes.md"), "text/markdown");
        assert_eq!(mime_type("blob"), "application/octet-stream");
    }

    #[test]
    fn digests_are_sha256_of_the_bytes() {
        let (bytes, sha) = digest(&b"abc"[..]).unwrap();
        assert_eq!(bytes, 3);
        assert_eq!(
            sha,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(is_sha256(&sha));
        assert!(!is_sha256("ABC"));
    }

    fn scratch() -> (tempfile::TempDir, Store) {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("home")).unwrap();
        (temp, store)
    }

    /// A store that plays a home hosting `ship`, as its own record says.
    fn hosting(store: &Store, ship: &str) {
        store
            .connection()
            .execute(
                "INSERT INTO ships(name, record) VALUES(?1, ?2)",
                params![ship, json!({"name": ship, "location": "here"}).to_string()],
            )
            .unwrap();
    }

    fn begin_upload(store: &Store, name: &str, bytes: u64, sha256: &str) -> String {
        let begun = begin(
            store,
            "apps",
            &json!({"name": name, "bytes": bytes, "sha256": sha256}),
        )
        .unwrap();
        begun["upload_id"].as_str().unwrap().to_string()
    }

    fn append_chunks(store: &Store, id: &str, data: &[u8]) {
        for (index, chunk) in data.chunks(CHUNK_BYTES).enumerate() {
            let offset = (index * CHUNK_BYTES) as u64;
            let data = base64_encode(chunk);
            append(
                store,
                "apps",
                &json!({"upload_id": id, "offset": offset, "data": data}),
            )
            .unwrap();
        }
    }

    #[test]
    fn a_file_of_several_chunks_is_committed_with_its_bytes() {
        let (_temp, store) = scratch();
        let data: Vec<u8> = (0..CHUNK_BYTES * 2 + 1000)
            .map(|i| (i % 251) as u8)
            .collect();
        let (bytes, sha) = digest(&data[..]).unwrap();
        let id = begin_upload(&store, "../big.bin", bytes, &sha);
        append_chunks(&store, &id, &data);
        let stored = commit(&store, "apps", &json!({"upload_id": id})).unwrap();
        assert_eq!(stored["name"], "big.bin");
        assert_eq!(stored["bytes"], data.len());
        assert_eq!(stored["type"], "application/octet-stream");
        let path = PathBuf::from(stored["path"].as_str().unwrap());
        assert_eq!(fs::read(path).unwrap(), data);
    }

    #[test]
    fn chunks_arrive_in_order_and_a_short_upload_waits_for_its_bytes() {
        let (_temp, store) = scratch();
        let id = begin_upload(&store, "a.txt", 5, &"0".repeat(64));
        let early = append(
            &store,
            "apps",
            &json!({"upload_id": id, "offset": 3, "data": base64_encode(b"lo")}),
        );
        assert!(early.unwrap_err().to_string().contains("not the next byte"));
        append(
            &store,
            "apps",
            &json!({"upload_id": id, "offset": 0, "data": base64_encode(b"he")}),
        )
        .unwrap();
        let short = commit(&store, "apps", &json!({"upload_id": id}));
        assert!(short.unwrap_err().to_string().contains("has 2 of 5 bytes"));
        assert!(upload(&store, "apps", &json!({"upload_id": id})).is_ok());
    }

    #[test]
    fn bytes_that_do_not_match_their_sha256_are_discarded() {
        let (_temp, store) = scratch();
        let id = begin_upload(&store, "a.txt", 5, &"0".repeat(64));
        append(
            &store,
            "apps",
            &json!({"upload_id": id, "offset": 0, "data": base64_encode(b"hello")}),
        )
        .unwrap();
        let refused = commit(&store, "apps", &json!({"upload_id": id}));
        assert!(refused.unwrap_err().to_string().contains("does not match"));
        assert!(upload(&store, "apps", &json!({"upload_id": id})).is_err());
        assert!(!staging_path(store.home(), &id).exists());
    }

    #[test]
    fn an_upload_nobody_commits_expires_and_its_staging_goes() {
        let (_temp, store) = scratch();
        let stale = begin_upload(&store, "a.txt", 5, &"0".repeat(64));
        let staged = staging_path(store.home(), &stale);
        assert!(staged.exists());
        store
            .connection()
            .execute("UPDATE file_uploads SET created_ms=0", [])
            .unwrap();
        // The next upload begun here sweeps the expired one.
        begin_upload(&store, "b.txt", 1, &"0".repeat(64));
        assert!(!staged.exists());
        assert!(upload(&store, "apps", &json!({"upload_id": stale})).is_err());
    }

    #[test]
    fn an_expired_upload_is_refused_at_append_and_commit_and_cleaned() {
        let (_temp, store) = scratch();
        let id = begin_upload(&store, "a.txt", 5, &"0".repeat(64));
        append(
            &store,
            "apps",
            &json!({"upload_id": id, "offset": 0, "data": base64_encode(b"he")}),
        )
        .unwrap();
        // An hour passes: the upload is gone, its staging with it.
        CLOCK_SHIFT_MS.with(|shift| shift.set(UPLOAD_TTL_MS + 1000));
        let refused = append(
            &store,
            "apps",
            &json!({"upload_id": id, "offset": 2, "data": base64_encode(b"ll")}),
        );
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("may have expired")
        );
        assert!(!staging_path(store.home(), &id).exists());
        let gone = commit(&store, "apps", &json!({"upload_id": id}));
        assert!(gone.unwrap_err().to_string().contains("may have expired"));
        CLOCK_SHIFT_MS.with(|shift| shift.set(0));
    }

    #[test]
    fn a_staged_part_with_no_row_is_removed_by_the_next_sweep() {
        let (_temp, store) = scratch();
        let dir = staging_dir(store.home());
        fs::create_dir_all(&dir).unwrap();
        let stray = dir.join("upload-stray.part");
        fs::write(&stray, b"debris").unwrap();
        begin_upload(&store, "a.txt", 1, &"0".repeat(64));
        assert!(!stray.exists());
    }

    #[test]
    fn each_ship_keeps_its_newest_files_and_no_more() {
        let (_temp, store) = scratch();
        let tx = store.write_transaction().unwrap();
        for i in 0..=KEEP_PER_SHIP {
            tx.execute(
                "INSERT INTO files(file_id,ship,name,bytes,type,sha256,created_ms)
                 VALUES(?1,'apps','a',0,'t','x',?2)",
                params![format!("file-{i:03}"), i],
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO files(file_id,ship,name,bytes,type,sha256,created_ms)
             VALUES('file-other','infra','a',0,'t','x',0)",
            [],
        )
        .unwrap();
        let pruned = prune(&tx, "apps").unwrap();
        tx.commit().unwrap();
        assert_eq!(pruned, ["file-000"]);
        let kept: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM files WHERE ship='apps'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kept, KEEP_PER_SHIP);
        let others: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM files WHERE ship='infra'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(others, 1);
    }

    #[test]
    fn a_ships_files_stay_within_its_bytes_oldest_first() {
        let (_temp, store) = scratch();
        let tx = store.write_transaction().unwrap();
        // Three files of 600 MiB each: the two oldest go so the newest fit.
        for (i, bytes) in [600, 600, 600].into_iter().enumerate() {
            tx.execute(
                "INSERT INTO files(file_id,ship,name,bytes,type,sha256,created_ms)
                 VALUES(?1,'apps','a',?2,'t','x',?3)",
                params![format!("file-{i:03}"), bytes * 1024 * 1024, i as i64],
            )
            .unwrap();
        }
        let pruned = prune(&tx, "apps").unwrap();
        tx.commit().unwrap();
        assert_eq!(pruned, ["file-000", "file-001"]);
        let kept: i64 = store
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(bytes),0)/1048576 FROM files WHERE ship='apps'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 600);
    }

    #[test]
    fn foreign_files_are_refused_naming_both_ships() {
        // Another ship's file names its owner and the mail's ship.
        let refused = foreign_error("file-x", "apps", "infra", Some("infra"));
        assert_eq!(refused, "file file-x is apps's, not infra's");
        // A file that is the ship's own, read from a home that is not its:
        // both ships are named, and neither is blamed twice.
        let misplaced = foreign_error("file-x", "apps", "apps", Some("infra"));
        assert!(misplaced.contains("apps's home"), "{misplaced}");
        assert!(misplaced.contains("not in infra's"), "{misplaced}");
        assert!(!misplaced.contains("apps's, not apps's"), "{misplaced}");
        let unnamed = foreign_error("file-x", "apps", "apps", None);
        assert!(unnamed.contains("not in this one"), "{unnamed}");
    }

    #[test]
    fn mail_fills_in_only_the_ships_own_files() {
        let (_temp, store) = scratch();
        hosting(&store, "apps");
        let id = begin_upload(&store, "plan.md", 2, &digest(&b"ok"[..]).unwrap().1);
        append(
            &store,
            "apps",
            &json!({"upload_id": id, "offset": 0, "data": base64_encode(b"ok")}),
        )
        .unwrap();
        let stored = commit(&store, "apps", &json!({"upload_id": id})).unwrap();
        let file_id = stored["file_id"].as_str().unwrap().to_string();

        let mut payload = json!({"files": [{"file_id": file_id}]});
        check_files(&store, CAPTAIN, "apps", &mut payload).unwrap();
        assert_eq!(payload["files"][0]["name"], "plan.md");
        assert_eq!(payload["files"][0]["path"], stored["path"]);
        assert_eq!(payload["files"][0]["type"], "text/markdown");

        // A caller never fills an entry in, not even with the true fields.
        let mut filled = json!({"files": [{"file_id": file_id, "name": "plan.md",
            "bytes": 2, "type": "text/markdown", "path": "/etc/passwd"}]});
        let refused = check_files(&store, CAPTAIN, "apps", &mut filled);
        let said = refused.unwrap_err().to_string();
        assert!(said.contains("carries more than file_id"), "{said}");
        let mut extra = json!({"files": [{"file_id": file_id, "secret": 1}]});
        assert!(check_files(&store, CAPTAIN, "apps", &mut extra).is_err());
        let mut unknown = json!({"files": [{"file_id": "file-missing"}]});
        let refused = check_files(&store, CAPTAIN, "apps", &mut unknown);
        assert!(refused.unwrap_err().to_string().contains("file-missing"));
        let mut from_ship = json!({"files": [{"file_id": file_id}]});
        assert!(check_files(&store, "apps", "apps", &mut from_ship).is_err());

        // A captain's mail that crossed a link is filled in again from the
        // rows of the home that reads it: a forged path does not survive.
        let mut delivered = json!({"files": [{"file_id": file_id, "name": "plan.md",
            "bytes": 2, "type": "text/markdown", "path": "/home/x/.ssh/id_ed25519"}]});
        check_delivered(&store, "apps", &mut delivered).unwrap();
        assert_eq!(delivered["files"][0]["path"], stored["path"]);
        let mut foreign = json!({"files": [{"file_id": "file-missing"}]});
        assert!(check_delivered(&store, "apps", &mut foreign).is_err());
    }
}
