//! The captain's crew and errand routes: named harness/model choices and the
//! order each ship tries them in. Launches without a route take the first one
//! that is not backing off; agents launch nothing unlisted. Without a routes
//! file, crew keep their first mate's route and errands the ship's own.
use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

const KINDS: [&str; 3] = ["crew", "errand", "first_mate"];
const COSTS: [&str; 3] = ["free", "plan", "credits"];

fn path() -> Result<PathBuf> {
    let path = crate::fleet::registry_path()?.with_extension("routes.json");
    ensure!(path.is_absolute(), "fleet routes path must be absolute");
    Ok(path)
}

fn order_list(value: &Value, routes: &Map<String, Value>, at: &str) -> Result<()> {
    let names = value
        .as_array()
        .with_context(|| format!("{at} must be a list"))?;
    ensure!(names.len() <= 16, "{at} lists at most 16 routes");
    let mut seen = std::collections::HashSet::new();
    for name in names {
        let name = name
            .as_str()
            .with_context(|| format!("{at} holds route names"))?;
        ensure!(routes.contains_key(name), "{at} names unknown route {name}");
        ensure!(seen.insert(name), "{at} lists {name} twice");
    }
    Ok(())
}

fn validate(value: &Value) -> Result<()> {
    let object = value.as_object().context("routes must be an object")?;
    for key in object.keys() {
        ensure!(
            matches!(
                key.as_str(),
                "routes" | "crew" | "errand" | "first_mate" | "ships"
            ),
            "unknown routes setting {key}"
        );
    }
    let routes = value["routes"]
        .as_object()
        .context("routes.routes must be an object of named routes")?;
    ensure!(routes.len() <= 32, "at most 32 routes");
    for (name, route) in routes {
        ensure!(
            (1..=40).contains(&name.len())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "route names are 1-40 lowercase letters, digits and dashes"
        );
        let fields = route
            .as_object()
            .with_context(|| format!("route {name} must be an object"))?;
        for key in fields.keys() {
            ensure!(
                matches!(key.as_str(), "manifest" | "options" | "cost" | "note"),
                "unknown setting {key} on route {name}"
            );
        }
        let manifest = route["manifest"]
            .as_str()
            .with_context(|| format!("route {name} needs a manifest"))?;
        let resolved = crate::bundled::resolve(&crate::bundled::normalize(manifest)?)?;
        ensure!(
            resolved.is_absolute() && resolved.is_file(),
            "route {name} manifest must be an existing absolute file or bundled manifest"
        );
        ensure!(
            route.get("options").is_none_or(Value::is_object),
            "route {name} options must be an object"
        );
        ensure!(
            route["cost"].as_str().is_some_and(|c| COSTS.contains(&c)),
            "route {name} cost must be free, plan or credits"
        );
        ensure!(
            route
                .get("note")
                .is_none_or(|n| n.as_str().is_some_and(|n| n.len() <= 500)),
            "route {name} note must be text of at most 500 bytes"
        );
    }
    for kind in KINDS {
        if let Some(list) = value.get(kind) {
            order_list(list, routes, kind)?;
        }
    }
    if let Some(ships) = value.get("ships") {
        for (ship, lists) in ships
            .as_object()
            .context("routes.ships must be an object")?
        {
            let lists = lists
                .as_object()
                .with_context(|| format!("routes for ship {ship} must be an object"))?;
            for (kind, list) in lists {
                ensure!(
                    KINDS.contains(&kind.as_str()),
                    "unknown route list {kind} for {ship}"
                );
                order_list(list, routes, &format!("ships.{ship}.{kind}"))?;
            }
        }
    }
    Ok(())
}

/// The fleet's routes file, if the captain has written one.
pub fn load() -> Result<Option<Value>> {
    match fs::File::open(path()?) {
        Ok(file) => {
            let mut raw = Vec::new();
            file.take(65_537).read_to_end(&mut raw)?;
            ensure!(raw.len() <= 65_536, "fleet routes file too large");
            let value: Value = serde_json::from_slice(&raw).context("invalid fleet routes file")?;
            validate(&value)?;
            Ok(Some(value))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Classic task-only homes and isolated pilots are not fleet ships.
fn for_store(store: &Store) -> Result<Option<Value>> {
    if crate::presence::folder_ship(store).is_some() {
        load()
    } else {
        Ok(None)
    }
}

/// Replace the whole routes file. Captain only.
pub fn set(caller: &Caller, value: &Value) -> Result<Value> {
    caller.require_operator("routes.set")?;
    validate(value)?;
    // The fleet registry's write lock serializes captain updates; the private
    // temporary file is atomically renamed into place.
    let mut conn = crate::fleet::registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let path = path()?;
    let temp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        fs::File::open(path.parent().context("routes directory")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    tx.commit()?;
    Ok(value.clone())
}

fn order(policy: &Value, ship: &str, kind: &str) -> Vec<String> {
    let list = policy["ships"][ship]
        .get(kind)
        .or_else(|| policy.get(kind))
        .and_then(Value::as_array);
    list.into_iter()
        .flatten()
        .filter_map(|n| n.as_str().map(str::to_owned))
        .collect()
}

fn identity(route: &Value) -> Option<String> {
    crate::routes::route_of(route["manifest"].as_str()?, &route["options"])
}

/// A route chosen for a launch.
pub(crate) struct Choice {
    pub manifest: Value,
    pub options: Value,
}

/// First listed available route for a ship, when a fleet policy provides one.
/// An empty list preserves callers' existing route defaults.
pub(crate) fn first_mate_fallback(store: &Store, ship: &str) -> Result<Option<Choice>> {
    let Some(policy) = for_store(store)? else {
        return Ok(None);
    };
    let names = order(&policy, ship, "first_mate");
    for name in names {
        let route = &policy["routes"][name.as_str()];
        if identity(route)
            .map(|id| crate::routes::refusal(store, &id))
            .transpose()?
            .flatten()
            .is_none()
        {
            return Ok(Some(Choice {
                manifest: route["manifest"].clone(),
                options: route.get("options").cloned().unwrap_or(json!({})),
            }));
        }
    }
    Ok(None)
}

/// The route a ship's crew or errand launch should use: the one it names, or
/// the first listed route that is not backing off. `None` means the captain
/// lists no routes of this kind, so the old defaults apply.
pub(crate) fn choose(
    store: &Store,
    ship: &str,
    kind: &str,
    requested: Option<&str>,
) -> Result<Option<Choice>> {
    let Some(policy) = for_store(store)? else {
        ensure!(
            requested.is_none(),
            "the captain has listed no routes to choose from"
        );
        return Ok(None);
    };
    let names = order(&policy, ship, kind);
    if names.is_empty() {
        ensure!(
            requested.is_none(),
            "the captain has listed no {kind} routes for {ship}"
        );
        return Ok(None);
    }
    let candidates: Vec<&String> = match requested {
        Some(name) => {
            ensure!(
                names.iter().any(|n| n == name),
                "route {name} is not one of {ship}'s {kind} routes: {}",
                names.join(", ")
            );
            names.iter().filter(|n| *n == name).collect()
        }
        None => names.iter().collect(),
    };
    let mut refusals = vec![];
    for name in candidates {
        let route = &policy["routes"][name.as_str()];
        if let Some(refusal) = identity(route)
            .map(|id| crate::routes::refusal(store, &id))
            .transpose()?
            .flatten()
        {
            refusals.push(format!("{name}: {refusal}"));
            continue;
        }
        return Ok(Some(Choice {
            manifest: route["manifest"].clone(),
            options: route.get("options").cloned().unwrap_or(json!({})),
        }));
    }
    bail!(
        "every {kind} route for {ship} is unavailable:\n{}",
        refusals.join("\n")
    )
}

/// Refuse an agent's explicit harness/model unless the captain lists it for
/// this ship. Operators may launch anything.
pub(crate) fn check(
    store: &Store,
    ship: &str,
    kind: &str,
    manifest: &str,
    options: &Value,
) -> Result<()> {
    if !matches!(store.caller(), Caller::Agent { .. }) {
        return Ok(());
    }
    let Some(policy) = for_store(store)? else {
        return Ok(());
    };
    let names = order(&policy, ship, kind);
    if names.is_empty() {
        return Ok(());
    }
    let wanted = crate::routes::route_of(manifest, options);
    ensure!(
        wanted.is_some()
            && names
                .iter()
                .any(|n| identity(&policy["routes"][n.as_str()]) == wanted),
        "{} is not one of {ship}'s {kind} routes; pass \"route\" with one of: {}",
        wanted.as_deref().unwrap_or(manifest),
        names.join(", ")
    );
    Ok(())
}

/// The bearings line: each list in order with the captain's notes, and
/// routes backing off marked.
pub(crate) fn summary(store: &Store, ship: &str) -> Result<Option<String>> {
    let Some(policy) = for_store(store)? else {
        return Ok(None);
    };
    let mut parts = vec![];
    for kind in KINDS {
        let names = order(&policy, ship, kind);
        if names.is_empty() {
            continue;
        }
        let mut shown = vec![];
        for name in names {
            let route = &policy["routes"][name.as_str()];
            let cost = route["cost"].as_str().unwrap_or("?");
            let note = route["note"].as_str().filter(|note| !note.is_empty());
            let mut label = match note {
                Some(note) => format!("{name} ({cost}; {note})"),
                None => format!("{name} ({cost})"),
            };
            if identity(route)
                .map(|id| crate::routes::refusal(store, &id))
                .transpose()?
                .flatten()
                .is_some()
            {
                label.push_str(" backing off");
            }
            shown.push(label);
        }
        parts.push(format!("{kind}: {}", shown.join(", ")));
    }
    Ok((!parts.is_empty()).then(|| {
        format!(
            "Routes ({}; the first available is used, or pass \"route\":\"<name>\").",
            parts.join("; ")
        )
    }))
}

pub fn cli(args: &[String], caller: &Caller) -> Result<()> {
    let value = match args {
        [] => load()?.unwrap_or(Value::Null),
        [verb] if verb == "show" => load()?.unwrap_or(Value::Null),
        [verb, file] if verb == "set" => {
            let raw = fs::read(file).with_context(|| format!("reading {file}"))?;
            ensure!(raw.len() <= 65_536, "routes file too large");
            let value = serde_json::from_slice(&raw)
                .context("invalid routes JSON (for YAML use routes.set --file)")?;
            set(caller, &value)?
        }
        _ => bail!("usage: privateer routes [show|set FILE.json]"),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "routes.show" => Ok(load()?.unwrap_or(Value::Null)),
        "routes.set" => {
            let value = set(store.caller(), &args)?;
            store.audit(
                "routes.set",
                &json!({"routes": value["routes"].as_object().map(|r| r.len())}),
            )?;
            Ok(value)
        }
        _ => bail!("unknown command {command}; try routes.show or routes.set"),
    }
}
