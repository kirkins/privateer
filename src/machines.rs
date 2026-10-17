//! Machines linked into one fleet.
//!
//! `privateer machine add NAME SSH_TARGET` links this machine's fleet with
//! another machine's, once, from the side that can connect. Each side records
//! the other in its registry and gives every one of its homes a single peer,
//! `machine-NAME`, behind which all of the other machine's ships live, so a
//! ship created on either side is reachable without further setup. Ship names
//! stay bare mail addresses and are unique across linked machines.
//!
//! A link is one-way: the machine that added it reaches the other machine's
//! gateway (`privateer gateway`, one JSON request per line over SSH), and the
//! other machine never connects back. Mail sent this way is delivered through
//! the gateway into the recipient's home. Mail sent the other way waits as
//! pending behind a `collected` peer, and the linking machine's dispatchers
//! collect it each pass, deliver it locally by its id and acknowledge it, so
//! a crash between the two collects it again and redelivery stays idempotent.
//! Directories are exchanged when the link is made and refreshed at a bounded
//! interval. The captain's mail does not cross machines yet.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

/// How often a linking machine re-exchanges ship directories.
const REFRESH_MS: i64 = 300_000;
/// Dispatchers pass every few seconds; a machine is contacted at most this
/// often, and after failures (a sleeping Mac, a tunnel down) backs off to
/// at most `MAX_BACKOFF_MS`.
const CONTACT_MS: i64 = 30_000;
const MAX_BACKOFF_MS: i64 = 600_000;
/// Mail collected in one call: a bounded count and, roughly, size.
const COLLECT_LIMIT: usize = 50;
const COLLECT_BYTES: usize = 512 * 1024;
/// SSH through a tunnel can be slow to start; links get more than a peer's
/// ten-second default.
const LINK_TIMEOUT_MS: u64 = 30_000;

pub(crate) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS machines(name TEXT PRIMARY KEY,
           role TEXT NOT NULL CHECK(role IN ('collects','collected')),
           peer TEXT NOT NULL, known_as TEXT, added_ms INTEGER NOT NULL, refreshed_ms INTEGER,
           next_contact_ms INTEGER, failures INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS machine_ships(machine TEXT NOT NULL, name TEXT NOT NULL,
           scope TEXT NOT NULL, PRIMARY KEY(machine, name));",
    )?;
    Ok(())
}

/// One linked machine as this registry records it. `collects`: this machine
/// reaches it and collects its mail. `collected`: it reaches this machine.
struct Machine {
    name: String,
    role: String,
    peer: Value,
    /// The name the other machine knows this one by (`collects` only).
    known_as: Option<String>,
    refreshed_ms: Option<i64>,
    next_contact_ms: Option<i64>,
    failures: u32,
}

fn peer_name(machine: &str) -> String {
    format!("machine-{machine}")
}

fn machines(conn: &Connection) -> Result<Vec<Machine>> {
    let mut statement = conn
        .prepare("SELECT name, role, peer, known_as, refreshed_ms, next_contact_ms, failures FROM machines ORDER BY name")?;
    let rows = statement.query_map([], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            (r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?),
            r.get::<_, u32>(6)?,
        ))
    })?;
    rows.map(|row| {
        let ((name, role), peer, known_as, (refreshed_ms, next_contact_ms), failures) = row?;
        Ok(Machine {
            name,
            role,
            peer: serde_json::from_str(&peer).context("corrupt machine peer")?,
            known_as,
            refreshed_ms,
            next_contact_ms,
            failures,
        })
    })
    .collect()
}

fn machine(conn: &Connection, name: &str) -> Result<Machine> {
    machines(conn)?
        .into_iter()
        .find(|m| m.name == name)
        .with_context(|| format!("no linked machine named {name}"))
}

/// The other machine's ships, as last exchanged.
fn cached(conn: &Connection, machine: &str) -> Result<Vec<(String, String)>> {
    let mut statement =
        conn.prepare("SELECT name, scope FROM machine_ships WHERE machine=?1 ORDER BY name")?;
    let rows = statement.query_map([machine], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Refuse a ship name already taken on a linked machine.
pub(crate) fn refuse_remote_name(conn: &Connection, name: &str) -> Result<()> {
    let owner: Option<String> = conn
        .query_row(
            "SELECT machine FROM machine_ships WHERE name=?1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(machine) = owner {
        bail!(
            "{name} is a ship on the linked machine {machine}; ship names are unique across linked machines"
        );
    }
    Ok(())
}

/// This machine's active ships and their scopes, the directory it offers.
fn local_ships(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut ships = vec![];
    for member in crate::fleet::members(conn)? {
        // Opening a home creates it; never resurrect a deleted ship folder.
        if !member.home.is_dir() {
            continue;
        }
        let Ok(mut store) = Store::open(&member.home) else {
            continue;
        };
        let scope = crate::fleet::scope_of(&mut store, &member.name);
        ships.push((member.name, scope));
    }
    Ok(ships)
}

fn directory(ships: &[(String, String)]) -> Value {
    json!(
        ships
            .iter()
            .map(|(name, scope)| json!({"name": name, "scope": scope}))
            .collect::<Vec<_>>()
    )
}

fn parse_directory(value: &Value) -> Result<Vec<(String, String)>> {
    value
        .as_array()
        .context("ships must be a list")?
        .iter()
        .map(|ship| {
            let name = required(ship, "name")?;
            identifier(name)?;
            crate::mail::unreserved(name)?;
            let scope = ship["scope"].as_str().unwrap_or("");
            Ok((name.to_owned(), scope.to_owned()))
        })
        .collect()
}

/// Names both machines would hold.
fn clashes(conn: &Connection, theirs: &[(String, String)]) -> Result<Vec<String>> {
    let ours: Vec<String> = crate::fleet::members(conn)?
        .into_iter()
        .map(|m| m.name)
        .collect();
    Ok(theirs
        .iter()
        .filter(|(name, _)| ours.contains(name))
        .map(|(name, _)| name.clone())
        .collect())
}

fn record(conn: &Connection, machine: &Machine, ships: &[(String, String)]) -> Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO machines(name, role, peer, known_as, added_ms, refreshed_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?5) ON CONFLICT(name) DO UPDATE SET role=excluded.role,
           peer=excluded.peer, known_as=excluded.known_as, refreshed_ms=excluded.refreshed_ms",
        params![
            machine.name,
            machine.role,
            machine.peer.to_string(),
            machine.known_as,
            Store::now_ms()
        ],
    )?;
    tx.execute(
        "DELETE FROM machine_ships WHERE machine=?1",
        [&machine.name],
    )?;
    for (name, scope) in ships {
        tx.execute(
            "INSERT INTO machine_ships(machine, name, scope) VALUES(?1, ?2, ?3)",
            params![machine.name, name, scope],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Give one home the machine's peer and a directory entry for each of its
/// ships. Ships the home hosts are never touched.
fn install(store: &mut Store, machine: &Machine, ships: &[(String, String)]) -> Result<usize> {
    crate::peers::dispatch(store, "peer.put", machine.peer.clone())?;
    let peer = peer_name(&machine.name);
    let mut installed = 0;
    for (name, scope) in ships {
        let current = crate::mail::dispatch(store, "ship.show", json!({"name": name})).ok();
        if current.as_ref().is_some_and(|s| s["location"] == "here") {
            continue;
        }
        crate::mail::dispatch(
            store,
            "ship.put",
            json!({"name": name, "scope": scope, "location": {"peer": peer}}),
        )?;
        installed += 1;
    }
    Ok(installed)
}

fn install_everywhere(
    conn: &Connection,
    machine: &Machine,
    ships: &[(String, String)],
) -> Result<usize> {
    let mut homes = 0;
    for member in crate::fleet::members(conn)? {
        if !member.home.is_dir() {
            continue;
        }
        let mut store = Store::open(&member.home)?;
        install(&mut store, machine, ships)?;
        homes += 1;
    }
    Ok(homes)
}

/// Make every linked machine's ships reachable from a newly linked home.
pub(crate) fn reach_remote(store: &mut Store, conn: &Connection) -> Result<usize> {
    let mut installed = 0;
    for machine in machines(conn)? {
        installed += install(store, &machine, &cached(conn, &machine.name)?)?;
    }
    Ok(installed)
}

fn call(machine: &Machine, command: &str, args: Value, timeout: Option<Duration>) -> Result<Value> {
    let response = crate::peers::call_config(
        &machine.peer,
        &format!("{command}-{}", uuid::Uuid::new_v4()),
        command,
        args,
        &Caller::Operator,
        timeout,
    )?;
    match response["ok"].as_bool() {
        Some(true) => Ok(response["result"].clone()),
        _ => bail!(
            "machine {}: {}",
            machine.name,
            response["error"].as_str().unwrap_or("gateway refused")
        ),
    }
}

/// Exchange directories with a machine this one reaches, and install its
/// ships in every home here. Clashing names are refused on both sides.
fn exchange(conn: &Connection, machine: &Machine) -> Result<Vec<(String, String)>> {
    let known_as = machine
        .known_as
        .as_deref()
        .context("link has no local name")?;
    let ours = local_ships(conn)?;
    let result = call(
        machine,
        "fleet.link",
        json!({"machine": known_as, "ships": directory(&ours)}),
        None,
    )?;
    let theirs = parse_directory(&result["ships"])?;
    let clashing = clashes(conn, &theirs)?;
    ensure!(
        clashing.is_empty(),
        "ships named {} exist on both machines; rename one before linking",
        clashing.join(", ")
    );
    record(conn, machine, &theirs)?;
    install_everywhere(conn, machine, &theirs)?;
    Ok(theirs)
}

fn hostname() -> String {
    let mut buffer = [0u8; 256];
    let ok = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
    let raw = if ok {
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..end]).into_owned()
    } else {
        String::new()
    };
    raw.split('.')
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn add(args: &Value) -> Result<Value> {
    let name = required(args, "name")?;
    identifier(name)?;
    let target = required(args, "target")?;
    let executable = required(args, "executable")?;
    let known_as = match args.get("as").and_then(Value::as_str) {
        Some(known_as) => known_as.to_owned(),
        None => hostname(),
    };
    identifier(&known_as).context("name this machine with --as NAME")?;
    ensure!(known_as != name, "both machines cannot be named {name}");
    let mut peer = json!({"name": peer_name(name), "transport": "ssh", "host": target,
        "executable": executable, "gateway": true, "timeout_ms": LINK_TIMEOUT_MS});
    if let Some(fleet) = args.get("fleet").and_then(Value::as_str) {
        peer["fleet"] = json!(fleet);
    }
    if let Some(transport) = args.get("transport").and_then(Value::as_str) {
        // Tests reach a gateway on the same machine without SSH.
        peer["transport"] = json!(transport);
        peer.as_object_mut().unwrap().remove("host");
    }
    let conn = crate::fleet::registry()?;
    if let Some(existing) = machines(&conn)?.into_iter().find(|m| m.name == name) {
        ensure!(
            existing.role == "collects",
            "{name} already reaches this machine; a link is made from one side"
        );
    }
    let machine = Machine {
        name: name.to_owned(),
        role: "collects".into(),
        peer,
        known_as: Some(known_as.clone()),
        refreshed_ms: None,
        next_contact_ms: None,
        failures: 0,
    };
    let theirs = exchange(&conn, &machine)?;
    Ok(json!({"machine": name, "known_as": known_as,
        "ships": theirs.iter().map(|(n, _)| n).collect::<Vec<_>>()}))
}

fn list() -> Result<Value> {
    let conn = crate::fleet::registry()?;
    let mut out = vec![];
    for machine in machines(&conn)? {
        out.push(json!({
            "name": machine.name,
            "direction": if machine.role == "collects" { "reached from here" } else { "reaches here" },
            "target": machine.peer["host"],
            "known_as": machine.known_as,
            "refreshed_ms": machine.refreshed_ms,
            "ships": cached(&conn, &machine.name)?.into_iter().map(|(n, _)| n).collect::<Vec<_>>(),
        }));
    }
    Ok(json!({"machines": out}))
}

/// Forget a link here. The other machine keeps its side until removed there.
fn remove(name: &str) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    machine(&conn, name)?;
    for member in crate::fleet::members(&conn)? {
        if member.home.is_dir() {
            crate::peers::forget(&Store::open(&member.home)?, &peer_name(name))?;
        }
    }
    conn.execute("DELETE FROM machine_ships WHERE machine=?1", [name])?;
    conn.execute("DELETE FROM machines WHERE name=?1", [name])?;
    Ok(json!({"removed": name}))
}

/// Collect now from machines this one reaches, outside the schedule.
fn collect_now(name: Option<&str>) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    let mut out = vec![];
    for machine in machines(&conn)?
        .into_iter()
        .filter(|m| m.role == "collects" && name.is_none_or(|n| n == m.name))
    {
        out.push(collect(
            &conn,
            &machine,
            Duration::from_millis(LINK_TIMEOUT_MS),
        )?);
    }
    Ok(json!({"collected": out}))
}

/// Record a contact's outcome and when the machine is next due.
fn schedule(conn: &Connection, machine: &Machine, succeeded: bool) -> Result<()> {
    let failures = if succeeded {
        0
    } else {
        machine.failures.saturating_add(1).min(10)
    };
    let delay = if succeeded {
        CONTACT_MS
    } else {
        CONTACT_MS
            .saturating_mul(1 << failures.min(5))
            .min(MAX_BACKOFF_MS)
    };
    conn.execute(
        "UPDATE machines SET next_contact_ms=?2, failures=?3 WHERE name=?1",
        params![
            machine.name,
            Store::now_ms().saturating_add(delay),
            failures
        ],
    )?;
    Ok(())
}

fn refresh(name: Option<&str>) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    let mut out = vec![];
    for machine in machines(&conn)?
        .into_iter()
        .filter(|m| m.role == "collects" && name.is_none_or(|n| n == m.name))
    {
        let ships = exchange(&conn, &machine)?;
        out.push(json!({"machine": machine.name, "ships": ships.len()}));
    }
    Ok(json!({"refreshed": out}))
}

/// Collect mail waiting on a machine this one reaches, deliver it into the
/// homes here, and acknowledge what was delivered or refused.
fn collect(conn: &Connection, machine: &Machine, timeout: Duration) -> Result<Value> {
    let known_as = machine
        .known_as
        .as_deref()
        .context("link has no local name")?;
    let result = call(
        machine,
        "fleet.collect",
        json!({"machine": known_as}),
        Some(timeout),
    )?;
    let messages = result["messages"]
        .as_array()
        .context("collected messages")?;
    let homes: Vec<_> = crate::fleet::members(conn)?;
    let (mut delivered, mut refused) = (vec![], vec![]);
    for envelope in messages {
        let id = required(envelope, "id")?.to_owned();
        let to = envelope["to"].as_str().unwrap_or("");
        let Some(home) = homes.iter().find(|m| m.name == to && m.home.is_dir()) else {
            refused.push(json!({"id": id, "error": format!("no ship named {to} on this machine")}));
            continue;
        };
        // A home that cannot be opened now is retried on the next pass.
        let Ok(mut store) = Store::open(&home.home) else {
            continue;
        };
        match crate::dispatch(&mut store, "mail.deliver", envelope.clone()) {
            Ok(result) => delivered
                .push(json!({"id": id, "end_state": crate::mail::applied_end(envelope, &result)})),
            Err(error) => refused.push(json!({"id": id, "error": format!("{error:#}")})),
        }
    }
    if !delivered.is_empty() || !refused.is_empty() {
        call(
            machine,
            "fleet.collected",
            json!({"machine": known_as, "delivered": delivered, "refused": refused}),
            Some(timeout),
        )?;
    }
    Ok(json!({"machine": machine.name, "delivered": delivered.len(), "refused": refused.len()}))
}

/// One dispatcher pass: for each machine this one reaches, refresh the
/// directories when due and collect waiting mail. One dispatcher on this
/// machine does each link's work at a time, and only a dispatcher of a home
/// in this registry does it at all.
pub(crate) fn tick(home: &std::path::Path, timeout: Duration) -> Value {
    let work = || -> Result<Vec<Value>> {
        let path = crate::fleet::registry_path()?;
        if !path.is_file() {
            return Ok(vec![]);
        }
        let conn = crate::fleet::registry()?;
        let reaching: Vec<Machine> = machines(&conn)?
            .into_iter()
            .filter(|m| m.role == "collects")
            .collect();
        if reaching.is_empty() || !crate::fleet::members(&conn)?.iter().any(|m| m.home == home) {
            return Ok(vec![]);
        }
        let mut out = vec![];
        for machine in reaching {
            let lock_path = path.with_file_name(format!("{}.lock", peer_name(&machine.name)));
            let lock = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                continue;
            }
            // Another dispatcher may have contacted it while this one waited.
            let machine = self::machine(&conn, &machine.name)?;
            if machine
                .next_contact_ms
                .is_some_and(|at| Store::now_ms() < at)
            {
                continue;
            }
            let due = machine
                .refreshed_ms
                .is_none_or(|at| Store::now_ms() - at >= REFRESH_MS);
            let refreshed = if due {
                exchange(&conn, &machine).err().map(|e| format!("{e:#}"))
            } else {
                None
            };
            let collected = collect(&conn, &machine, timeout);
            schedule(&conn, &machine, collected.is_ok() && refreshed.is_none())?;
            let mut outcome = collected.unwrap_or_else(
                |error| json!({"machine": machine.name, "error": format!("{error:#}")}),
            );
            if let Some(error) = refreshed {
                outcome["refresh_error"] = json!(error);
            }
            out.push(outcome);
        }
        Ok(out)
    };
    work().map(Value::from).unwrap_or_else(
        |error| json!([{"error": format!("{error:#}").chars().take(2_000).collect::<String>()}]),
    )
}

/// The home here that hosts a ship.
fn home_of(conn: &Connection, ship: &str) -> Result<PathBuf> {
    crate::fleet::members(conn)?
        .into_iter()
        .find(|m| m.name == ship && m.home.is_dir())
        .map(|m| m.home)
        .with_context(|| format!("no ship named {ship} on this machine"))
}

/// A machine that reaches this one, by the name it gave.
fn reaching_here(conn: &Connection, name: &str) -> Result<Machine> {
    identifier(name)?;
    let machine = machine(conn, name)?;
    ensure!(
        machine.role == "collected",
        "{name} is reached from here, not linked to it"
    );
    Ok(machine)
}

/// What a gateway answers for its machine's fleet. Linking and collecting
/// are the operator's, whose SSH access to this machine is real authority
/// here; an agent may only deliver mail as itself.
fn serve_request(caller: &Caller, command: &str, args: &Value) -> Result<Value> {
    ensure!(args.is_object(), "command arguments must be a JSON object");
    // Delivery through a machine peer arrives as an ordinary mail.deliver.
    let delivery = matches!(command, "fleet.deliver" | "mail.deliver");
    if !delivery {
        caller.require_operator(command)?;
    }
    let conn = crate::fleet::registry()?;
    match command {
        "fleet.link" => {
            let name = required(args, "machine")?;
            identifier(name)?;
            if let Some(existing) = machines(&conn)?.into_iter().find(|m| m.name == name) {
                ensure!(
                    existing.role == "collected",
                    "this machine already reaches {name}; a link is made from one side"
                );
            }
            let theirs = parse_directory(&args["ships"])?;
            let clashing = clashes(&conn, &theirs)?;
            ensure!(
                clashing.is_empty(),
                "ships named {} exist on both machines; rename one before linking",
                clashing.join(", ")
            );
            let machine = Machine {
                name: name.to_owned(),
                role: "collected".into(),
                peer: json!({"name": peer_name(name), "transport": "collected"}),
                known_as: None,
                refreshed_ms: None,
                next_contact_ms: None,
                failures: 0,
            };
            record(&conn, &machine, &theirs)?;
            install_everywhere(&conn, &machine, &theirs)?;
            Ok(json!({"ships": directory(&local_ships(&conn)?)}))
        }
        _ if delivery => {
            let home = home_of(&conn, required(args, "to")?)?;
            let mut store = Store::open_as(&home, caller.clone())?;
            crate::dispatch(&mut store, "mail.deliver", args.clone())
        }
        "fleet.collect" => {
            let machine = reaching_here(&conn, required(args, "machine")?)?;
            let peer = peer_name(&machine.name);
            let mut messages = vec![];
            let mut bytes = 0;
            for member in crate::fleet::members(&conn)? {
                if messages.len() >= COLLECT_LIMIT || bytes >= COLLECT_BYTES {
                    break;
                }
                if !member.home.is_dir() {
                    continue;
                }
                let store = Store::open(&member.home)?;
                for envelope in crate::mail::awaiting(
                    &store,
                    &peer,
                    COLLECT_LIMIT - messages.len(),
                    COLLECT_BYTES - bytes,
                )? {
                    bytes += envelope.to_string().len();
                    messages.push(envelope);
                }
            }
            Ok(json!({"messages": messages}))
        }
        "fleet.collected" => {
            let machine = reaching_here(&conn, required(args, "machine")?)?;
            let peer = peer_name(&machine.name);
            let list = |key: &str| args[key].as_array().cloned().unwrap_or_default();
            let delivered: Vec<(String, Option<String>)> = list("delivered")
                .iter()
                .filter_map(|d| {
                    let id = d["id"].as_str()?.to_owned();
                    Some((id, d["end_state"].as_str().map(str::to_owned)))
                })
                .collect();
            let refused: Vec<(String, String)> = list("refused")
                .iter()
                .filter_map(|r| {
                    Some((
                        r["id"].as_str()?.to_owned(),
                        r["error"].as_str().unwrap_or("refused").to_owned(),
                    ))
                })
                .collect();
            let mut recorded = 0;
            for member in crate::fleet::members(&conn)? {
                if member.home.is_dir() {
                    let store = Store::open(&member.home)?;
                    recorded += crate::mail::collected(&store, &peer, &delivered, &refused)?;
                }
            }
            Ok(json!({"recorded": recorded}))
        }
        _ => bail!("unknown gateway command {command}"),
    }
}

/// Run a machine's gateway: one JSON request per line on stdin, one response
/// per line on stdout, like a home's RPC.
pub fn gateway(caller: Caller) -> Result<()> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout().lock();
    loop {
        let mut line = Vec::new();
        let read = input
            .by_ref()
            .take(1024 * 1024 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(());
        }
        ensure!(line.len() <= 1024 * 1024, "gateway line exceeds 1 MiB");
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => {
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let command = request["command"].as_str().unwrap_or("");
                let args = request.get("args").cloned().unwrap_or_else(|| json!({}));
                match serve_request(&caller, command, &args) {
                    Ok(result) => json!({"id": id, "ok": true, "result": result}),
                    Err(error) => json!({"id": id, "ok": false, "error": error.to_string()}),
                }
            }
            Err(_) => json!({"id": null, "ok": false, "error": "invalid JSON request"}),
        };
        serde_json::to_writer(&mut out, &response)?;
        out.write_all(b"\n")?;
        out.flush()?;
    }
}

/// `privateer machine add NAME SSH_TARGET --executable PATH [--remote-fleet
/// PATH] [--as NAME] | list | refresh [NAME] | collect [NAME] | remove NAME`
pub fn cli(args: &[String]) -> Result<()> {
    let usage = "usage: privateer machine add NAME SSH_TARGET --executable /remote/privateer [--remote-fleet /remote/fleet.sqlite] [--as THIS_MACHINE] | list | refresh [NAME] | collect [NAME] | remove NAME";
    let flags = crate::cli::flags(args, &["executable", "remote-fleet", "as"])?;
    let result = match flags.positional.first().map(String::as_str) {
        Some("add") => {
            let mut request = json!({
                "name": flags.positional.get(1).context(usage)?,
                "target": flags.positional.get(2).context(usage)?,
                "executable": flags.values.get("executable").context(usage)?,
            });
            if let Some(fleet) = flags.values.get("remote-fleet") {
                request["fleet"] = json!(fleet);
            }
            if let Some(known_as) = flags.values.get("as") {
                request["as"] = json!(known_as);
            }
            add(&request)?
        }
        Some("list") => list()?,
        Some("refresh") => refresh(flags.positional.get(1).map(String::as_str))?,
        Some("collect") => collect_now(flags.positional.get(1).map(String::as_str))?,
        Some("remove") => remove(flags.positional.get(1).context(usage)?)?,
        _ => bail!(usage),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

/// The `machine.*` JSON commands, for the operator.
pub fn dispatch(store: &Store, command: &str, args: Value) -> Result<Value> {
    store.caller().require_operator(command)?;
    match command {
        "machine.add" => add(&args),
        "machine.list" => list(),
        "machine.refresh" => refresh(args.get("name").and_then(Value::as_str)),
        "machine.collect" => collect_now(args.get("name").and_then(Value::as_str)),
        "machine.remove" => remove(required(&args, "name")?),
        _ => bail!("unknown machine command {command}"),
    }
}
