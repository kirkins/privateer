//! Moving an idle ship to another linked machine.
//!
//! `privateer ship move NAME --to MACHINE` hands a ship this machine hosts to
//! a machine this one reaches, and `privateer ship move NAME --here` brings
//! one here from a machine this one reaches. Only the machine that made a link
//! can reach the other, so it runs every move, in either direction, and the
//! other machine takes part through its gateway.
//!
//! A ship moves only while idle. Its dispatcher is stopped first, and the
//! move is refused while a wake, job, errand, held workspace or live session
//! is in flight, or a project holds uncommitted or unpushed work. Then:
//!
//! 1. Export, on the ship's machine: a consistent snapshot of its database
//!    and a copy of its folder, without its projects and runtime files, are
//!    staged beside the fleet registry, with each project's remote.
//! 2. Import, on the other machine: the files arrive in bounded chunks and
//!    are checked against their digests. The folder is laid out beside the
//!    captain's home there, and what belonged to the old machine (its peers,
//!    sessions, presence and the charter's repository path) is rewritten. The
//!    projects are cloned again, one call each, and the ship is registered,
//!    linked and its dispatcher started.
//! 3. Release, on the old machine: the old copy's charter is revoked, its
//!    registry entry retired, the other ships there pointed at the machine it
//!    went to, and its folder marked as moved, so no command runs it there
//!    again. The folder itself is kept.
//!
//! Every step is idempotent for its move and the running side records its
//! progress, so `ship move` run again after an interruption picks up where
//! it stopped. A move that fails before the ship is active on the other
//! machine is undone there, and the old machine restarts its dispatcher.
use crate::store::{Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Bytes carried in one gateway call; hex doubles them, well under the
/// gateway's one-MiB line.
const CHUNK: usize = 256 * 1024;
/// The most a ship's folder may carry, without its projects.
const MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Cloning one project is the slowest call; a machine call allows this long.
const SLOW_CALL: Duration = Duration::from_secs(120);
/// Left in a ship's old folder, naming where it went.
const MARKER: &str = "moved.json";

pub(crate) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS moves(id TEXT NOT NULL,
           role TEXT NOT NULL CHECK(role IN ('run','export','import')), ship TEXT NOT NULL,
           state TEXT NOT NULL, detail TEXT NOT NULL, updated_ms INTEGER NOT NULL,
           PRIMARY KEY(id, role));",
    )?;
    Ok(())
}

fn row(conn: &Connection, id: &str, role: &str) -> Result<Option<(String, Value)>> {
    conn.query_row(
        "SELECT state, detail FROM moves WHERE id=?1 AND role=?2",
        params![id, role],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .optional()?
    .map(|(state, detail)| Ok((state, serde_json::from_str(&detail)?)))
    .transpose()
}

fn put(
    conn: &Connection,
    id: &str,
    role: &str,
    ship: &str,
    state: &str,
    detail: &Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO moves(id, role, ship, state, detail, updated_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id, role) DO UPDATE SET state=excluded.state, detail=excluded.detail,
           updated_ms=excluded.updated_ms",
        params![id, role, ship, state, detail.to_string(), Store::now_ms()],
    )?;
    Ok(())
}

/// Where one side of a move keeps its files, beside the fleet registry.
fn staging(id: &str, role: &str) -> Result<PathBuf> {
    identifier(id)?;
    Ok(crate::fleet::registry_path()?
        .parent()
        .context("fleet registry directory")?
        .join("moves")
        .join(id)
        .join(role))
}

fn discard_staging(id: &str, role: &str) -> Result<()> {
    let stage = staging(id, role)?;
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    if let Some(parent) = stage.parent() {
        // Gone only once both sides on this machine are done with it.
        let _ = fs::remove_dir(parent);
    }
    Ok(())
}

/// A path inside a ship's folder, as a move names it.
fn relative(path: &str) -> Result<PathBuf> {
    let candidate = Path::new(path);
    ensure!(
        !path.is_empty()
            && candidate
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "a move names files inside the ship's folder, not {path}"
    );
    Ok(candidate.to_path_buf())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    ensure!(text.len() % 2 == 0, "a move chunk has an odd length");
    let digit = |b: u8| -> Result<u8> {
        Ok(match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            _ => bail!("a move chunk is not hex"),
        })
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| Ok(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

fn digest(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(hex(&hasher.finalize()))
}

fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )?)
}

/// What keeps a ship from moving now, in the captain's words.
fn busy(store: &Store) -> Result<Vec<String>> {
    let conn = store.connection();
    let mut reasons = vec![];
    for (table, condition, what) in [
        (
            "wakes",
            "status IN ('offered','running')",
            "first-mate wakes in flight",
        ),
        (
            "jobs",
            "json_extract(record,'$.status') IN ('starting','queued','launching','running','orphaned')",
            "jobs not finished",
        ),
        ("errands", "status='running'", "errands running"),
        (
            "workspaces",
            "json_extract(record,'$.state')<>'released'",
            "workspaces still held (release them, or archive unfinished work)",
        ),
        (
            "presence",
            "expires_ms > (CAST(strftime('%s','now') AS INTEGER) * 1000)",
            "live sessions aboard (close the first mate's session)",
        ),
    ] {
        if !has_table(conn, table)? {
            continue;
        }
        let n: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {condition}"),
            [],
            |r| r.get(0),
        )?;
        if n > 0 {
            reasons.push(format!("{n} {what}"));
        }
    }
    Ok(reasons)
}

/// Each project under the folder with the remote to clone it from, and what
/// would be lost by cloning it afresh.
fn projects(folder: &Path) -> Result<(Vec<Value>, Vec<String>)> {
    let dir = folder.join("projects");
    let (mut found, mut problems) = (vec![], vec![]);
    if !dir.is_dir() {
        return Ok((found, problems));
    }
    let mut entries = fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        // A clone still being staged, never published.
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let git = |args: &[&str]| -> Option<String> {
            let out = Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        };
        let Some(url) = git(&["remote", "get-url", "origin"]) else {
            problems.push(format!("project {name} has no origin to clone it from"));
            continue;
        };
        if git(&["status", "--porcelain"]).is_none_or(|s| !s.is_empty()) {
            problems.push(format!("project {name} has uncommitted changes"));
        }
        if git(&["log", "--branches", "--not", "--remotes", "--oneline"])
            .is_none_or(|s| !s.is_empty())
        {
            problems.push(format!("project {name} has commits not pushed anywhere"));
        }
        found.push(json!({"name": name, "url": url}));
    }
    Ok((found, problems))
}

/// Runtime files that belong to the machine rather than the ship.
fn skipped(relative: &Path) -> bool {
    let parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match parts.as_slice() {
        [first, ..] if first == "projects" => true,
        [state, rest @ ..] if state == crate::cli::STATE_DIR => match rest {
            [dir, ..] if dir == "jobs" => true,
            [file] => {
                file.starts_with("privateer.sqlite")
                    || file.starts_with("serve.")
                    || file.ends_with(".lock")
                    || file == MARKER
            }
            _ => false,
        },
        _ => false,
    }
}

/// Regular files under `dir`, relative to `root`; symbolic links are left
/// behind and named.
fn walk(root: &Path, dir: &Path, files: &mut Vec<PathBuf>, links: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(root)?.to_path_buf();
        if skipped(&rel) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            links.push(rel.to_string_lossy().into_owned());
        } else if kind.is_dir() {
            walk(root, &path, files, links)?;
        } else if kind.is_file() {
            files.push(rel);
        }
    }
    Ok(())
}

/// Stop a ship, check it is idle and stage what moves.
fn export(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    identifier(id)?;
    let ship = required(args, "ship")?;
    identifier(ship)?;
    let conn = crate::fleet::registry()?;
    if let Some((state, detail)) = row(&conn, id, "export")? {
        ensure!(state == "exported", "move {id} is already {state} here");
        return Ok(detail["manifest"].clone());
    }
    let member = crate::fleet::members(&conn)?
        .into_iter()
        .find(|m| m.name == ship && m.home.is_dir())
        .with_context(|| format!("no ship named {ship} on this machine"))?;
    let home = member.home.clone();
    let folder = home.parent().context("ship folder")?.to_path_buf();
    // Nothing new starts once its dispatcher is stopped.
    crate::service::stop(&home, ship)?;
    let staged = (|| -> Result<Value> {
        ensure!(
            !crate::service::running(&home)?,
            "its dispatcher is still running"
        );
        let mut store = Store::open(&home)?;
        let mut problems = busy(&store)?;
        let (projects, unsafe_projects) = projects(&folder)?;
        problems.extend(unsafe_projects);
        ensure!(
            problems.is_empty(),
            "it is not idle: {}",
            problems.join("; ")
        );
        let profile = crate::mail::dispatch(&mut store, "ship.show", json!({"name": ship}))?;
        let stage = staging(id, "export")?;
        if stage.exists() {
            fs::remove_dir_all(&stage)?;
        }
        fs::create_dir_all(stage.join(crate::cli::STATE_DIR))?;
        let (mut files, mut links) = (vec![], vec![]);
        walk(&folder, &folder, &mut files, &mut links)?;
        for rel in &files {
            let target = stage.join(rel);
            fs::create_dir_all(target.parent().context("staged file")?)?;
            fs::copy(folder.join(rel), &target)?;
        }
        // A consistent copy of the database, whatever its journal holds.
        let database = Path::new(crate::cli::STATE_DIR).join("privateer.sqlite");
        store
            .connection()
            .execute("VACUUM INTO ?1", [stage.join(&database).to_string_lossy()])?;
        files.push(database);
        let mut listed = vec![];
        let mut total = 0u64;
        for rel in &files {
            let staged = stage.join(rel);
            let size = fs::metadata(&staged)?.len();
            let mode = fs::metadata(folder.join(rel))
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0o600);
            total += size;
            listed
                .push(json!({"path": rel, "size": size, "sha256": digest(&staged)?, "mode": mode}));
        }
        ensure!(
            total <= MAX_BYTES,
            "its folder holds {} MiB besides its projects, more than a move carries",
            total / (1024 * 1024)
        );
        Ok(
            json!({"id": id, "ship": ship, "created_by": member.created_by,
            "folder": folder, "files": listed, "projects": projects, "links_left": links,
            "first_mate": profile["first_mate"]}),
        )
    })();
    match staged {
        Ok(manifest) => {
            put(
                &conn,
                id,
                "export",
                ship,
                "exported",
                &json!({"manifest": manifest, "home": home}),
            )?;
            Ok(manifest)
        }
        Err(error) => {
            let _ = discard_staging(id, "export");
            let restarted = crate::service::ensure(&home, ship)
                .unwrap_or_else(|e| format!("its dispatcher did not restart: {e:#}"));
            Err(anyhow::anyhow!(
                "{ship} stays on this machine, {error:#} ({restarted})"
            ))
        }
    }
}

fn exported(conn: &Connection, id: &str) -> Result<(String, Value)> {
    let (state, detail) =
        row(conn, id, "export")?.with_context(|| format!("no move {id} leaves this machine"))?;
    Ok((state, detail))
}

fn listed<'a>(manifest: &'a Value, path: &str) -> Result<&'a Value> {
    manifest["files"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| f["path"] == path)
        .with_context(|| format!("{path} is not part of this move"))
}

/// One chunk of a staged file.
fn read(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let path = required(args, "path")?;
    let offset = args["offset"].as_u64().context("offset is required")?;
    let conn = crate::fleet::registry()?;
    let (state, detail) = exported(&conn, id)?;
    ensure!(state == "exported", "move {id} is {state} here");
    listed(&detail["manifest"], path)?;
    let mut file = fs::File::open(staging(id, "export")?.join(relative(path)?))?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = Vec::with_capacity(CHUNK);
    file.take(CHUNK as u64).read_to_end(&mut buffer)?;
    Ok(json!({"hex": hex(&buffer)}))
}

/// Give up a move from this machine: the ship stays, and runs again.
fn abort(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let conn = crate::fleet::registry()?;
    let (state, detail) = exported(&conn, id)?;
    if state != "exported" {
        return Ok(json!({"state": state}));
    }
    let ship = detail["manifest"]["ship"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let home = PathBuf::from(detail["home"].as_str().unwrap_or_default());
    let dispatcher = crate::service::ensure(&home, &ship)?;
    put(&conn, id, "export", &ship, "aborted", &detail)?;
    discard_staging(id, "export")?;
    Ok(json!({"state": "aborted", "dispatcher": dispatcher}))
}

fn git_quietly(command: &mut Command) -> &mut Command {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0");
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    command
}

fn project_entry(project: &Value) -> Result<(String, String)> {
    let name = required(project, "name")?;
    identifier(name)?;
    ensure!(!name.starts_with('.'), "a project name cannot start with .");
    let url = required(project, "url")?;
    let scheme = url.to_ascii_lowercase();
    ensure!(
        !url.starts_with('-')
            && !url.chars().any(char::is_control)
            && !scheme.starts_with("ext::")
            && !scheme.starts_with("fd::"),
        "project {name} has an unusable remote"
    );
    Ok((name.to_owned(), url.to_owned()))
}

/// Accept a move to this machine: check it can host the ship, and make room.
fn begin(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    identifier(id)?;
    let manifest = args.get("manifest").context("manifest is required")?;
    let ship = required(manifest, "ship")?;
    identifier(ship)?;
    let conn = crate::fleet::registry()?;
    if let Some((_, detail)) = row(&conn, id, "import")? {
        return Ok(json!({"folder": detail["folder"]}));
    }
    ensure!(
        !crate::fleet::members(&conn)?.iter().any(|m| m.name == ship),
        "{ship} is already a ship on this machine"
    );
    if let Some(plugin) = manifest["first_mate"]["manifest"].as_str() {
        ensure!(
            plugin.starts_with("bundled:") || Path::new(plugin).is_file(),
            "{ship}'s first mate uses the plugin {plugin}, which this machine does not have"
        );
    }
    let path = crate::service::login_path();
    let harness = manifest["first_mate"]["manifest"]
        .as_str()
        .and_then(crate::bundled::harness);
    if let Some(binary) = harness.as_deref().and_then(crate::cli::harness_binary) {
        ensure!(
            crate::cli::on_path_in(binary, &path),
            "{ship}'s first mate runs {binary}, which is not installed here"
        );
    }
    for project in manifest["projects"].as_array().into_iter().flatten() {
        let (name, url) = project_entry(project)?;
        let reached = git_quietly(Command::new("git").args(["ls-remote", "--heads", "--", &url]))
            .status()
            .is_ok_and(|s| s.success());
        ensure!(
            reached,
            "this machine cannot fetch {ship}'s project {name} from {url}"
        );
    }
    let mut total = 0u64;
    for file in manifest["files"].as_array().context("files are listed")? {
        relative(required(file, "path")?)?;
        total += file["size"].as_u64().context("each file has a size")?;
    }
    ensure!(
        total <= MAX_BYTES,
        "this move is larger than a move carries"
    );
    let folder = match args.get("folder").and_then(Value::as_str) {
        Some(folder) => PathBuf::from(folder),
        None => match crate::machines::captain_home(&conn)? {
            Some((_, home)) => home
                .parent()
                .and_then(Path::parent)
                .context("the captain's home here has no parent folder")?
                .join(ship),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join("ships")
                .join(ship),
        },
    };
    ensure!(folder.is_absolute(), "folder must be absolute");
    // A folder this ship left behind earlier is set aside, never deleted.
    let left = left_behind(&folder, ship)?;
    ensure!(
        left.is_some() || !folder.exists() || fs::read_dir(&folder)?.next().is_none(),
        "{} already exists here and is not empty",
        folder.display()
    );
    fs::create_dir_all(staging(id, "import")?)?;
    put(
        &conn,
        id,
        "import",
        ship,
        "receiving",
        &json!({"manifest": manifest, "folder": folder, "path": path}),
    )?;
    Ok(json!({"folder": folder}))
}

/// The folder this ship was moved away from, if `folder` is one.
fn left_behind(folder: &Path, ship: &str) -> Result<Option<Value>> {
    let marker = folder.join(crate::cli::STATE_DIR).join(MARKER);
    if !marker.is_file() {
        return Ok(None);
    }
    let note: Value = serde_json::from_slice(&fs::read(&marker)?)?;
    Ok((note["ship"] == ship).then_some(note))
}

fn imported(conn: &Connection, id: &str) -> Result<(String, Value)> {
    row(conn, id, "import")?.with_context(|| format!("no move {id} arrives on this machine"))
}

/// One chunk of a file arriving.
fn write(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let path = required(args, "path")?;
    let offset = args["offset"].as_u64().context("offset is required")?;
    let bytes = unhex(required(args, "hex")?)?;
    let conn = crate::fleet::registry()?;
    let (state, detail) = imported(&conn, id)?;
    ensure!(state == "receiving", "move {id} is already {state} here");
    let size = listed(&detail["manifest"], path)?["size"]
        .as_u64()
        .unwrap_or_default();
    ensure!(
        offset + bytes.len() as u64 <= size,
        "a chunk of {path} runs past its end"
    );
    let target = staging(id, "import")?.join(relative(path)?);
    fs::create_dir_all(target.parent().context("arriving file")?)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&target)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&bytes)?;
    Ok(json!({"written": bytes.len()}))
}

/// Make the arrived folder this machine's: point what named the old folder at
/// the new one and forget what belonged to the old machine.
fn rewrite(home: &Path, manifest: &Value, folder: &Path, path: &str) -> Result<Vec<String>> {
    let old = PathBuf::from(required(manifest, "folder")?);
    let ship = required(manifest, "ship")?;
    let mut notes = vec![];
    let store = Store::open(home)?;
    let tx = store.write_transaction()?;
    let charters: Vec<(String, String)> = tx
        .prepare("SELECT id, data FROM charters")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, data) in charters {
        let mut charter: Value = serde_json::from_str(&data)?;
        let Some(repo) = charter["repo"].as_str().map(PathBuf::from) else {
            continue;
        };
        match repo.strip_prefix(&old) {
            Ok(rest) => {
                let moved = if rest.as_os_str().is_empty() {
                    folder.to_path_buf()
                } else {
                    folder.join(rest)
                };
                charter["repo"] = json!(moved);
                tx.execute(
                    "UPDATE charters SET data=?2 WHERE id=?1",
                    params![id, charter.to_string()],
                )?;
            }
            Err(_) => notes.push(format!(
                "charter {id} names the repository {} outside the ship's folder; set it with charter.put",
                repo.display()
            )),
        }
    }
    for table in [
        "peer_endpoints",
        "presence",
        "sessions",
        "host_launches",
        "hosted_block_alerts",
    ] {
        if has_table(&tx, table)? {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
    }
    tx.execute(
        "DELETE FROM metadata WHERE key IN ('pid_namespace','dispatcher_generation')",
        [],
    )?;
    if has_table(&tx, "workspaces")? {
        tx.execute(
            "DELETE FROM workspaces WHERE json_extract(record,'$.state')='released'",
            [],
        )?;
    }
    let hosted = manifest["first_mate"]
        .get("host")
        .is_some_and(|h| !h.is_null());
    if hosted && !crate::cli::on_path_in("herdr", path) {
        tx.execute(
            "UPDATE ships SET record=json_remove(record,'$.first_mate.host') WHERE name=?1",
            [ship],
        )?;
        notes.push("Herdr is not installed here, so its first mate runs headless".into());
    }
    tx.commit()?;
    Ok(notes)
}

/// Lay the arrived folder out, checked against the digests.
fn place(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let conn = crate::fleet::registry()?;
    let (state, mut detail) = imported(&conn, id)?;
    if state != "receiving" {
        return Ok(json!({"state": state}));
    }
    let manifest = detail["manifest"].clone();
    let ship = required(&manifest, "ship")?.to_owned();
    let folder = PathBuf::from(required(&detail, "folder")?);
    let stage = staging(id, "import")?;
    for file in manifest["files"].as_array().into_iter().flatten() {
        let path = required(file, "path")?;
        let arrived = stage.join(relative(path)?);
        ensure!(
            arrived.is_file()
                && fs::metadata(&arrived)?.len() == file["size"].as_u64().unwrap_or(u64::MAX)
                && digest(&arrived)? == required(file, "sha256")?,
            "{path} arrived incomplete or damaged; run the move again"
        );
    }
    if let Some(note) = left_behind(&folder, &ship)? {
        let aside = folder.with_file_name(format!(
            "{}.left-{}",
            folder.file_name().unwrap_or_default().to_string_lossy(),
            note["move"]
                .as_str()
                .unwrap_or("earlier")
                .chars()
                .take(8)
                .collect::<String>()
        ));
        fs::rename(&folder, &aside)?;
        detail["set_aside"] = json!(aside);
    }
    ensure!(
        !folder.exists() || fs::read_dir(&folder)?.next().is_none(),
        "{} already exists here and is not empty",
        folder.display()
    );
    let laid = (|| -> Result<Vec<String>> {
        for file in manifest["files"].as_array().into_iter().flatten() {
            let rel = relative(required(file, "path")?)?;
            let target = folder.join(&rel);
            fs::create_dir_all(target.parent().context("ship file")?)?;
            fs::copy(stage.join(&rel), &target)?;
            let mode = file["mode"].as_u64().unwrap_or(0o600) as u32 & 0o777;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
        }
        let home = folder.join(crate::cli::STATE_DIR);
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
        fs::create_dir_all(folder.join("projects"))?;
        rewrite(
            &home,
            &manifest,
            &folder,
            detail["path"].as_str().unwrap_or(""),
        )
    })();
    match laid {
        Ok(notes) => {
            detail["notes"] = json!(notes);
            put(&conn, id, "import", &ship, "placed", &detail)?;
            Ok(json!({"state": "placed"}))
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&folder);
            if let Some(aside) = detail["set_aside"].as_str() {
                let _ = fs::rename(aside, &folder);
            }
            Err(error)
        }
    }
}

/// Clone one of the ship's projects into its new folder.
fn clone(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let name = required(args, "name")?;
    let conn = crate::fleet::registry()?;
    let (state, detail) = imported(&conn, id)?;
    ensure!(state == "placed", "move {id} is {state} here");
    let project = detail["manifest"]["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == name)
        .with_context(|| format!("{name} is not one of this ship's projects"))?;
    let (name, url) = project_entry(project)?;
    let projects = PathBuf::from(required(&detail, "folder")?).join("projects");
    let destination = projects.join(&name);
    if destination.join(".git").exists() {
        return Ok(json!({"project": name, "cloned": false}));
    }
    let staging = projects.join(format!(".{name}-{id}.clone"));
    let _ = fs::remove_dir_all(&staging);
    let cloned = git_quietly(
        Command::new("git")
            .args([
                "-c",
                "protocol.ext.allow=never",
                "clone",
                "--quiet",
                "--",
                &url,
            ])
            .arg(&staging),
    )
    .status()
    .is_ok_and(|s| s.success());
    if !cloned {
        let _ = fs::remove_dir_all(&staging);
        bail!("cloning {name} from {url} failed here");
    }
    fs::rename(&staging, &destination)?;
    Ok(json!({"project": name, "cloned": true}))
}

/// Make the placed ship one of this machine's: register, link and start it.
fn activate(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let conn = crate::fleet::registry()?;
    let (state, mut detail) = imported(&conn, id)?;
    if state == "finished" {
        return Ok(detail["result"].clone());
    }
    ensure!(state == "placed", "move {id} is {state} here");
    let manifest = &detail["manifest"];
    let ship = required(manifest, "ship")?.to_owned();
    let created_by = manifest["created_by"]
        .as_str()
        .unwrap_or("captain")
        .to_owned();
    let folder = PathBuf::from(required(&detail, "folder")?);
    let home = folder.join(crate::cli::STATE_DIR);
    // Its old directory entry here named the machine it came from.
    conn.execute("DELETE FROM machine_ships WHERE name=?1", [&ship])?;
    crate::fleet::register(&ship, &home, &created_by)?;
    let store = Store::open(&home)?;
    crate::fleet::link(&store, &ship)?;
    // Ships it knew through peers this machine does not have stay unknown
    // here until a link brings them.
    let forgotten = store.connection().execute(
        "DELETE FROM ships WHERE json_extract(record,'$.location.peer') IS NOT NULL
           AND json_extract(record,'$.location.peer') NOT IN (SELECT name FROM peer_endpoints)",
        [],
    )?;
    let mut notes: Vec<Value> = detail["notes"].as_array().cloned().unwrap_or_default();
    if forgotten > 0 {
        notes.push(json!(format!(
            "{forgotten} ships it knew are not reachable from this machine"
        )));
    }
    // Captain mail it carried that the captain already has, taken by a
    // machine link or shown by a front door where it was written, is not
    // offered again from here; the rest still reaches the captain.
    let shown = if has_table(store.connection(), "door_cursor")? {
        "OR seq <= (SELECT COALESCE(MAX(through_seq), 0) FROM door_cursor)"
    } else {
        ""
    };
    let handed = if has_table(store.connection(), "machine_collected")? {
        "EXISTS(SELECT 1 FROM machine_collected c WHERE c.id=mail.id AND c.error IS NULL)"
    } else {
        "0"
    };
    let carried: Vec<(String, Option<String>)> = store
        .connection()
        .prepare(&format!(
            "SELECT id FROM mail WHERE recipient=?1 AND ({handed} {shown})"
        ))?
        .query_map([crate::mail::CAPTAIN], |r| {
            Ok((r.get::<_, String>(0)?, None))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let peers: Vec<String> = store
        .connection()
        .prepare("SELECT name FROM peer_endpoints")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for peer in peers {
        if crate::peers::machine(&store, &peer)? {
            crate::mail::collected_captain(&store, &peer, &carried)?;
        }
    }
    store.audit("ship.arrived", &json!({"ship": ship, "move": id}))?;
    let dispatcher =
        crate::service::ensure_with_path(&home, &ship, detail["path"].as_str().unwrap_or(""))
            .unwrap_or_else(|error| format!("not started: {error:#}"));
    let result = json!({"ship": ship, "folder": folder, "dispatcher": dispatcher,
        "set_aside": detail["set_aside"], "notes": notes});
    detail["result"] = result.clone();
    put(&conn, id, "import", &ship, "finished", &detail)?;
    discard_staging(id, "import")?;
    Ok(result)
}

/// Where a move to this machine stands.
fn status(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let conn = crate::fleet::registry()?;
    Ok(match row(&conn, id, "import")? {
        Some((state, detail)) => json!({"state": state, "result": detail["result"]}),
        None => json!({"state": null}),
    })
}

/// Undo a move to this machine that did not finish.
fn discard(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let conn = crate::fleet::registry()?;
    let Some((state, detail)) = row(&conn, id, "import")? else {
        return Ok(json!({"state": null}));
    };
    ensure!(state != "finished", "move {id} already finished here");
    if state == "placed" {
        if let Some(folder) = detail["folder"].as_str() {
            let home = Path::new(folder).join(crate::cli::STATE_DIR);
            conn.execute("DELETE FROM ships WHERE home=?1", [home.to_string_lossy()])?;
            let _ = fs::remove_dir_all(folder);
        }
        if let Some(aside) = detail["set_aside"].as_str() {
            if let Some(folder) = detail["folder"].as_str() {
                let _ = fs::rename(aside, folder);
            }
        }
    }
    conn.execute("DELETE FROM moves WHERE id=?1 AND role='import'", [id])?;
    discard_staging(id, "import")?;
    Ok(json!({"state": "discarded"}))
}

/// The ship is active elsewhere now: retire the copy here.
fn release(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let peer = required(args, "peer")?;
    identifier(peer)?;
    let conn = crate::fleet::registry()?;
    let (state, mut detail) = exported(&conn, id)?;
    if state == "released" {
        return Ok(detail["released"].clone());
    }
    ensure!(state == "exported", "move {id} is {state} here");
    let ship = required(&detail["manifest"], "ship")?.to_owned();
    let home = PathBuf::from(required(&detail, "home")?);
    if home.is_dir() {
        let mut old = Store::open(&home)?;
        if old.charter(&ship).is_ok_and(|c| c["active"] == true) {
            crate::core::dispatch(&mut old, "charter.revoke", json!({"id": ship}))?;
        }
        old.audit("ship.moved", &json!({"ship": ship, "move": id, "to": peer}))?;
    }
    conn.execute(
        "UPDATE ships SET retired_ms=?2 WHERE name=?1 AND home=?3 AND retired_ms IS NULL",
        params![ship, Store::now_ms(), home.to_string_lossy()],
    )?;
    // The other ships here reach it through the machine it went to.
    let location = json!({"peer": peer}).to_string();
    for other in crate::fleet::members(&conn)? {
        if other.home.is_dir() {
            Store::open(&other.home)?.connection().execute(
                "UPDATE ships SET record=json_set(record,'$.location',json(?2))
                 WHERE name=?1 AND json_extract(record,'$.location.peer')=?3",
                params![ship, location, format!("fleet-{ship}")],
            )?;
        }
    }
    let machine = peer.strip_prefix("machine-").unwrap_or(peer);
    if home.is_dir() {
        fs::write(
            home.join(MARKER),
            json!({"ship": ship, "machine": machine, "move": id, "moved_ms": Store::now_ms()})
                .to_string(),
        )?;
    }
    let released = json!({"ship": ship, "folder_kept": home.parent(), "to": machine});
    detail["released"] = released.clone();
    put(&conn, id, "export", &ship, "released", &detail)?;
    discard_staging(id, "export")?;
    Ok(released)
}

/// Refuse to run a ship from the folder it moved away from.
pub fn refuse_moved(home: &Path) -> Result<()> {
    let marker = home.join(MARKER);
    if !marker.is_file() {
        return Ok(());
    }
    let note: Value = serde_json::from_slice(&fs::read(&marker)?).unwrap_or(Value::Null);
    bail!(
        "{} moved to the machine {}; this folder is what it left behind, kept for reference",
        note["ship"].as_str().unwrap_or("this ship"),
        note["machine"].as_str().unwrap_or("elsewhere")
    )
}

/// One step of a move on this machine, for the machine running it or for
/// that machine's gateway.
pub(crate) fn serve(command: &str, args: &Value) -> Result<Value> {
    match command {
        "move.export" => export(args),
        "move.read" => read(args),
        "move.abort" => abort(args),
        "move.begin" => begin(args),
        "move.write" => write(args),
        "move.place" => place(args),
        "move.clone" => clone(args),
        "move.activate" => activate(args),
        "move.status" => status(args),
        "move.discard" => discard(args),
        "move.release" => release(args),
        _ => bail!("unknown move command {command}"),
    }
}

/// One side of a move: this machine, or one this machine reaches.
enum Side {
    Here,
    There(String),
}

impl Side {
    fn call(&self, command: &str, args: Value, timeout: Option<Duration>) -> Result<Value> {
        match self {
            Side::Here => serve(command, &args),
            Side::There(machine) => crate::machines::call_reached(machine, command, args, timeout),
        }
    }
}

/// Undo what a failed move started on both sides; the ship runs where it was.
fn undo(source: &Side, target: &Side, id: &str) {
    let _ = target.call("move.discard", json!({"id": id}), None);
    let _ = source.call("move.abort", json!({"id": id}), None);
}

/// Where a new move goes: the machine on the other side and the peer the
/// ship's old machine will reach it through.
fn plan(conn: &Connection, ship: &str, to: &str) -> Result<Value> {
    let local = crate::fleet::members(conn)?
        .iter()
        .any(|m| m.name == ship && m.home.is_dir());
    if to == "here" {
        ensure!(!local, "{ship} is already on this machine");
        let machine: String = conn
            .query_row(
                "SELECT machine FROM machine_ships WHERE name=?1",
                [ship],
                |r| r.get(0),
            )
            .optional()?
            .with_context(|| format!("no linked machine lists a ship named {ship}"))?;
        let known_as = crate::machines::known_as(conn, &machine)?;
        Ok(json!({"to": to, "machine": machine, "peer": format!("machine-{known_as}")}))
    } else {
        identifier(to)?;
        ensure!(
            local,
            "{ship} is not a ship on this machine; to bring it here, run this with --here"
        );
        crate::machines::known_as(conn, to)?;
        Ok(json!({"to": to, "machine": to, "peer": format!("machine-{to}")}))
    }
}

fn run(args: &Value) -> Result<Value> {
    let ship = required(args, "name")?;
    identifier(ship)?;
    let to = required(args, "to")?;
    let conn = crate::fleet::registry()?;
    // An interrupted move of this ship is picked up where it stopped.
    let unfinished = conn
        .query_row(
            "SELECT id, state, detail FROM moves WHERE role='run' AND ship=?1
               AND state NOT IN ('done','failed') ORDER BY updated_ms DESC LIMIT 1",
            [ship],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let (id, prior, record) = match unfinished {
        Some((id, state, detail)) => {
            let record: Value = serde_json::from_str(&detail)?;
            ensure!(
                record["to"] == to,
                "an earlier move of {ship} to {} is unfinished; run that move again to finish it",
                record["to"].as_str().unwrap_or("another machine")
            );
            (id, state, record)
        }
        None => (
            uuid::Uuid::new_v4().to_string(),
            "new".to_owned(),
            plan(&conn, ship, to)?,
        ),
    };
    let machine = required(&record, "machine")?.to_owned();
    let peer = required(&record, "peer")?.to_owned();
    let (source, target) = if to == "here" {
        (Side::There(machine.clone()), Side::Here)
    } else {
        (Side::Here, Side::There(machine.clone()))
    };
    let (mut arrived, mut released) = (json!({}), json!({}));
    if prior != "released" {
        if prior == "new" {
            put(&conn, &id, "run", ship, "started", &record)?;
        }
        let fail = |error: anyhow::Error| -> anyhow::Error {
            undo(&source, &target, &id);
            let _ = put(&conn, &id, "run", ship, "failed", &record);
            error
        };
        let manifest = source
            .call("move.export", json!({"id": id, "ship": ship}), None)
            .inspect_err(|_| {
                let _ = put(&conn, &id, "run", ship, "failed", &record);
            })?;
        // A resumed move may already have made the ship active there; never
        // undo one that might have.
        let finished = match target.call("move.status", json!({"id": id}), None) {
            Ok(status) => (status["state"] == "finished").then(|| status["result"].clone()),
            Err(error) if prior == "new" => return Err(fail(error)),
            Err(error) => bail!(
                "{error:#}; {ship} may already be active on {machine}: run this move again to finish it"
            ),
        };
        arrived = match finished {
            Some(result) => result,
            None => {
                let transfer = || -> Result<()> {
                    target.call(
                        "move.begin",
                        json!({"id": id, "manifest": manifest, "folder": args.get("folder")}),
                        Some(SLOW_CALL),
                    )?;
                    for file in manifest["files"].as_array().into_iter().flatten() {
                        let path = required(file, "path")?;
                        let size = file["size"].as_u64().unwrap_or_default();
                        let mut offset = 0;
                        while offset < size {
                            let chunk = source.call(
                                "move.read",
                                json!({"id": id, "path": path, "offset": offset}),
                                None,
                            )?;
                            let hex = required(&chunk, "hex")?;
                            ensure!(!hex.is_empty(), "{path} ended early");
                            target.call(
                                "move.write",
                                json!({"id": id, "path": path, "offset": offset, "hex": hex}),
                                None,
                            )?;
                            offset += hex.len() as u64 / 2;
                        }
                    }
                    target.call("move.place", json!({"id": id}), Some(SLOW_CALL))?;
                    for project in manifest["projects"].as_array().into_iter().flatten() {
                        target.call(
                            "move.clone",
                            json!({"id": id, "name": project["name"]}),
                            Some(SLOW_CALL),
                        )?;
                    }
                    Ok(())
                };
                transfer().map_err(fail)?;
                match target.call("move.activate", json!({"id": id}), Some(SLOW_CALL)) {
                    Ok(arrived) => arrived,
                    // The ship may be active there though the answer was lost.
                    Err(error) => match target.call("move.status", json!({"id": id}), None) {
                        Ok(status) if status["state"] == "finished" => status["result"].clone(),
                        Ok(_) => return Err(fail(error)),
                        Err(_) => bail!(
                            "{error:#}; {ship} may already be active on {machine}: run this move again to finish it"
                        ),
                    },
                }
            }
        };
        put(&conn, &id, "run", ship, "arrived", &record)?;
        released = source.call("move.release", json!({"id": id, "peer": peer}), None)?;
        put(&conn, &id, "run", ship, "released", &record)?;
    }
    // Both machines learn where it lives now rather than at the next refresh.
    let directory = crate::machines::refresh_one(&machine)
        .err()
        .map(|error| format!("directories refresh on the next pass: {error:#}"));
    put(&conn, &id, "run", ship, "done", &record)?;
    Ok(json!({"ship": ship, "move": id,
        "from": if to == "here" { machine.as_str() } else { "here" },
        "to": if to == "here" { "here" } else { machine.as_str() },
        "folder": arrived["folder"], "dispatcher": arrived["dispatcher"],
        "set_aside": arrived["set_aside"], "notes": arrived["notes"],
        "folder_kept": released["folder_kept"], "directory": directory}))
}

/// `ship.move`, for the operator.
pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    store.caller().require_operator(command)?;
    match command {
        "ship.move" => run(&args),
        _ => bail!("unknown move command {command}"),
    }
}
