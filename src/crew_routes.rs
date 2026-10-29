//! The captain's crew and errand routes: named harness/model choices and the
//! order each ship tries them in. Launches without a route take the first one
//! that is not backing off; agents launch nothing unlisted. Without a routes
//! file, crew keep their first mate's route and errands the ship's own.
//!
//! A route may declare its `allowance` shape: a daily reset (`daily_utc`), or a
//! rolling window (`window_hours`, with an optional `weekly_utc` cap), and then
//! must state its `tier` (1 strongest to 9). A route states its `tier` and the
//! `uses` it is strong enough for; a tier is a floor a launch requires, never a
//! preference. Crew and errand launches try use-it-or-lose-it allowances first,
//! nearest their refresh, then idle rolling windows, then carry-over routes in
//! the captain's order. An explicit route is never re-ranked.
//! Routes marked `explicit_only` stay authorized for named launches and
//! continuations, but are never selected automatically for new work.
//!
//! A route may also declare how much of its plan to keep in reserve. Only
//! Claude reports what it has spent, so a route on any other plan is measured
//! by the runs Privateer counts for it: `allowance.runs` is the number of runs
//! the window holds, with no default, because no runtime can guess one.
//! `crew_headroom_pct` and `first_mate_headroom_pct` (both whole percents, 80
//! and 95 by default) are the shares at which the route stops taking new
//! crew and errands, and at which a first mate moves to its fallback. Running
//! work is never stopped: it is what spends the reserve.
//!
//! A `routes.set` may be temporary: with `until_ms`, or `until_reset` naming a
//! route whose allowance has a fixed reset, the file keeps the routes it
//! replaced, verbatim, under `temporary.previous`. Once the time passes they
//! are in force again: readers see them at once, and the dispatcher pass
//! writes them back, audits it and tells each ship whose lists change. A
//! later `routes.set` replaces the whole file, so a temporary change never
//! reverts over a newer one. A temporary change made while another is in
//! force stacks on it: when it ends, the one it replaced is back with its own
//! end time (and gives way at once if that has passed too). Each temporary
//! change undoes only itself; a permanent one ends the whole stack.
use crate::routes::Who;
use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

pub(crate) const KINDS: [&str; 4] = ["crew", "errand", "first_mate", "review"];
const COSTS: [&str; 3] = ["free", "plan", "credits"];
/// The words a route's `uses` may name: what a route is strong enough to do.
pub(crate) const USES: [&str; 4] = ["implement", "review", "diagnose", "design"];
/// The share of a plan new crew and errands stop taking by default.
pub(crate) const CREW_HEADROOM_PCT: f64 = 80.0;
/// The larger share a first mate keeps for itself by default.
pub(crate) const FIRST_MATE_HEADROOM_PCT: f64 = 95.0;
/// The largest run budget a window may hold: far past any plan, so a
/// mistyped figure refuses rather than refusing nothing.
const MAX_RUNS: u64 = 10_000;
/// The routes file's largest size, previous routes of temporary changes included.
const MAX_FILE_BYTES: usize = 65_536;
/// How many temporary changes may stack on one another.
const MAX_STACKED: usize = 8;
/// How far ahead a temporary change may end: past a weekly reset, with room.
const MAX_TEMPORARY_MS: i64 = 31 * DAY_MS;

/// What a route keeps in reserve: the share of its plan at which new crew and
/// errands are refused, the share at which a first mate moves to its fallback,
/// and the runs its window holds when the harness reports no usage of its own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Limits {
    pub crew: f64,
    pub first_mate: f64,
    pub runs: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            crew: CREW_HEADROOM_PCT / 100.0,
            first_mate: FIRST_MATE_HEADROOM_PCT / 100.0,
            runs: None,
        }
    }
}

/// The reserve a named route keeps: its own settings, or the defaults for the
/// shares and no budget at all, since only the captain knows one.
pub(crate) fn limits(route: &Value) -> Limits {
    let pct = |name: &str, default: f64| route[name].as_f64().unwrap_or(default) / 100.0;
    Limits {
        crew: pct("crew_headroom_pct", CREW_HEADROOM_PCT),
        first_mate: pct("first_mate_headroom_pct", FIRST_MATE_HEADROOM_PCT),
        runs: allowance(route).and_then(|allowance| allowance.runs),
    }
}

/// The reserve a route identity keeps in the fleet routes file, or the
/// defaults for a route the captain has not named there, such as a ship's own
/// first-mate profile.
pub(crate) fn limits_of(store: &Store, route: &str) -> Result<Limits> {
    Ok(for_store(store)?
        .as_ref()
        .map_or_else(Limits::default, |policy| named_limits(policy, route)))
}

/// The reserve the first named route of this identity keeps, or the defaults.
fn named_limits(policy: &Value, route: &str) -> Limits {
    policy["routes"]
        .as_object()
        .into_iter()
        .flatten()
        .find(|(_, named)| identity(named).as_deref() == Some(route))
        .map_or_else(Limits::default, |(_, named)| limits(named))
}

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
    validate_at(value, 0)
}

/// A routes file, and under a temporary change the routes it replaced, at
/// most `MAX_STACKED` deep.
fn validate_at(value: &Value, depth: usize) -> Result<()> {
    let object = value.as_object().context("routes must be an object")?;
    for key in object.keys() {
        ensure!(
            matches!(
                key.as_str(),
                "routes" | "crew" | "errand" | "first_mate" | "review" | "ships" | "temporary"
            ),
            "unknown routes setting {key}"
        );
    }
    if let Some(temporary) = value.get("temporary") {
        ensure!(
            depth < MAX_STACKED,
            "at most {MAX_STACKED} temporary routes changes stack; make a permanent change"
        );
        let fields = temporary
            .as_object()
            .context("routes temporary must be an object")?;
        for key in fields.keys() {
            ensure!(
                matches!(
                    key.as_str(),
                    "id" | "set_ms" | "until_ms" | "until_reset" | "previous"
                ),
                "unknown temporary routes setting {key}"
            );
        }
        ensure!(
            temporary["id"]
                .as_str()
                .is_some_and(|id| (1..=64).contains(&id.len())),
            "routes temporary needs an id"
        );
        ensure!(
            temporary["set_ms"].is_i64() && temporary["until_ms"].is_i64(),
            "routes temporary needs set_ms and until_ms"
        );
        ensure!(
            temporary.get("until_reset").is_none_or(Value::is_string),
            "routes temporary until_reset must be a route name"
        );
        match temporary.get("previous") {
            Some(Value::Null) => {}
            Some(previous) => validate_at(previous, depth + 1)
                .context("the routes a temporary change replaced")?,
            None => bail!("routes temporary needs the previous routes, or null"),
        }
    }
    let routes = value["routes"]
        .as_object()
        .context("routes.routes must be an object of named routes")?;
    ensure!(routes.len() <= 32, "at most 32 routes");
    let mut shapes = std::collections::HashMap::new();
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
                matches!(
                    key.as_str(),
                    "manifest"
                        | "options"
                        | "cost"
                        | "note"
                        | "allowance_resets_utc"
                        | "allowance"
                        | "tier"
                        | "uses"
                        | "crew_headroom_pct"
                        | "first_mate_headroom_pct"
                        | "explicit_only"
                ),
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
            route.get("explicit_only").is_none_or(Value::is_boolean),
            "route {name} explicit_only must be a boolean"
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
        ensure!(
            route
                .get("allowance_resets_utc")
                .is_none_or(|at| utc_minute(at).is_some()),
            "route {name} allowance_resets_utc must be a UTC time of day, HH:MM"
        );
        ensure!(
            route
                .get("tier")
                .is_none_or(|t| t.as_u64().is_some_and(|t| (1..=9).contains(&t))),
            "route {name} tier must be a whole number from 1 (strongest) to 9"
        );
        for key in ["crew_headroom_pct", "first_mate_headroom_pct"] {
            ensure!(
                route
                    .get(key)
                    .is_none_or(|p| p.as_u64().is_some_and(|p| (1..=100).contains(&p))),
                "route {name} {key} must be a whole percent from 1 to 100"
            );
        }
        if let Some(uses) = route.get("uses") {
            ensure!(
                route.get("tier").is_some(),
                "route {name} states uses, so it must state its tier (1-9)"
            );
            let words = uses
                .as_array()
                .filter(|words| !words.is_empty())
                .with_context(|| format!("route {name} uses must be a non-empty list"))?;
            let mut seen = std::collections::HashSet::new();
            for word in words {
                let word = word
                    .as_str()
                    .with_context(|| format!("route {name} uses holds words"))?;
                ensure!(
                    USES.contains(&word),
                    "route {name} uses {word} is not one of {}",
                    USES.join(", ")
                );
                ensure!(seen.insert(word), "route {name} lists use {word} twice");
            }
        }
        if let Some(allowance) = route.get("allowance") {
            ensure!(
                route.get("allowance_resets_utc").is_none(),
                "route {name} states both allowance and allowance_resets_utc; \
                 move the time into allowance.daily_utc"
            );
            ensure!(
                route.get("tier").is_some(),
                "route {name} declares an allowance, so it must state its tier (1-9)"
            );
            let shape = shape(allowance).with_context(|| format!("route {name} allowance"))?;
            let runs = runs(allowance)
                .map_err(|error| anyhow::anyhow!("route {name} allowance: {error:#}"))?;
            if let Some(id) = identity(route)
                && let Some((other, earlier)) =
                    shapes.insert(id.clone(), (name.clone(), (shape, runs)))
                && earlier != (shape, runs)
            {
                bail!("routes {other} and {name} run {id} with different allowance shapes");
            }
        }
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

/// The fleet's routes in force, if the captain has written a routes file: a
/// temporary change past its end already gives way to what it replaced, even
/// before the dispatcher writes that back.
pub fn load() -> Result<Option<Value>> {
    Ok(in_force(read()?, Store::now_ms()))
}

/// The routes file as written, temporary changes and all.
fn read() -> Result<Option<Value>> {
    match fs::File::open(path()?) {
        Ok(file) => {
            let mut raw = Vec::new();
            file.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut raw)?;
            ensure!(raw.len() <= MAX_FILE_BYTES, "fleet routes file too large");
            let value: Value = serde_json::from_slice(&raw).context("invalid fleet routes file")?;
            validate(&value)?;
            Ok(Some(value))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The routes in force at `now`: each temporary change whose end has passed
/// gives way to the routes it replaced, which may be none.
fn in_force(mut value: Option<Value>, now: i64) -> Option<Value> {
    while value
        .as_ref()
        .and_then(|value| value["temporary"]["until_ms"].as_i64())
        .is_some_and(|until| until <= now)
    {
        value = value
            .map(|mut value| value["temporary"]["previous"].take())
            .filter(|previous| !previous.is_null());
    }
    value
}

/// Write the routes file, or remove it when no routes are left, under the
/// caller's fleet registry lock.
fn write(value: Option<&Value>) -> Result<()> {
    let path = path()?;
    let Some(value) = value else {
        match fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error.into());
            }
            _ => {}
        }
        fs::File::open(path.parent().context("routes directory")?)?.sync_all()?;
        return Ok(());
    };
    let raw = serde_json::to_vec_pretty(value)?;
    ensure!(
        raw.len() <= MAX_FILE_BYTES,
        "the routes file would pass {MAX_FILE_BYTES} bytes; with temporary changes stacked, \
         make a permanent change instead"
    );
    // The private temporary file is atomically renamed into place.
    let temp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&raw)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        fs::File::open(path.parent().context("routes directory")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// When a temporary change ends: `until_ms`, or the next fixed reset of the
/// allowance of the route `until_reset` names, in the new routes or else the
/// current ones. None for a permanent change.
fn until(
    args: &Value,
    new: &Value,
    current: Option<&Value>,
    now: i64,
) -> Result<Option<(i64, Option<String>)>> {
    let at = match (args.get("until_ms"), args.get("until_reset")) {
        (None, None) => return Ok(None),
        (Some(at), None) => (at.as_i64().context("until_ms must be a time in ms")?, None),
        (None, Some(name)) => {
            let name = name
                .as_str()
                .context("until_reset must name a route")?
                .to_owned();
            let route = [Some(new), current]
                .into_iter()
                .flatten()
                .map(|routes| &routes["routes"][name.as_str()])
                .find(|route| route.is_object())
                .with_context(|| format!("until_reset names unknown route {name}"))?;
            (
                next_reset(route, now).with_context(|| {
                    format!(
                        "route {name} has no fixed reset: declare allowance.daily_utc or \
                     allowance.weekly_utc on it, or pass until_ms"
                    )
                })?,
                Some(name),
            )
        }
        (Some(_), Some(_)) => bail!("pass until_ms or until_reset, not both"),
    };
    ensure!(at.0 > now, "until_ms must be in the future");
    ensure!(
        at.0 - now <= MAX_TEMPORARY_MS,
        "a temporary routes change ends within 31 days"
    );
    Ok(Some(at))
}

/// The next time a route's allowance renews at a fixed time: daily, or the
/// weekly cap of a rolling window. A rolling window alone opens at first use.
fn next_reset(route: &Value, now: i64) -> Option<i64> {
    match allowance(route).map(|allowance| allowance.shape) {
        Some(Shape::Daily(minute)) => Some(next_daily(minute, now)),
        Some(Shape::Rolling {
            weekly: Some(minute),
            ..
        }) => Some(next_weekly(minute, now)),
        Some(Shape::Rolling { weekly: None, .. }) => None,
        None => route
            .get("allowance_resets_utc")
            .and_then(utc_minute)
            .map(|minute| next_daily(minute, now)),
    }
}

/// Classic task-only homes and isolated pilots are not fleet ships.
pub(crate) fn for_store(store: &Store) -> Result<Option<Value>> {
    if crate::presence::folder_ship(store).is_some() {
        load()
    } else {
        Ok(None)
    }
}

/// Replace the whole routes file. Captain only. The reply carries `warnings`
/// for what the runtime cannot refuse: notes still prefixed with a STRENGTH
/// label, and errand routes with no `review` use. Its `readiness` lists each
/// tool a route's harness requires that is not installed on this command's
/// PATH, with the install step; the file is written all the same.
///
/// With `until_ms` or `until_reset` the change is temporary: the routes file
/// as it stands, any temporary change in it included, is kept to come back
/// then, and the reply's `temporary` says what reverts when.
pub fn set(caller: &Caller, args: &Value) -> Result<Value> {
    caller.require_operator("routes.set")?;
    let mut value = args.clone();
    if let Some(object) = value.as_object_mut() {
        ensure!(
            !object.contains_key("temporary"),
            "temporary is the runtime's record of a temporary change: remove it, and pass \
             until_ms or until_reset to make this change temporary too"
        );
        object.remove("until_ms");
        object.remove("until_reset");
    }
    validate(&value)?;
    let warnings = warnings(&value);
    // The fleet registry's write lock serializes captain updates and the
    // dispatcher's reverts, so the routes a temporary change keeps are the
    // ones it replaced.
    let mut conn = crate::fleet::registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let now = Store::now_ms();
    let current = in_force(read()?, now);
    let mut written = value.clone();
    if let Some((until_ms, reset)) = until(args, &value, current.as_ref(), now)? {
        let mut temporary = json!({"id": uuid::Uuid::new_v4().to_string(), "set_ms": now,
            "until_ms": until_ms, "previous": current});
        if let Some(reset) = reset {
            temporary["until_reset"] = json!(reset);
        }
        written["temporary"] = temporary;
        validate(&written)?;
    }
    write(Some(&written))?;
    tx.commit()?;
    let mut reply = value.clone();
    if written.get("temporary").is_some() {
        reply["temporary"] = temporary_view(&written);
    }
    reply["warnings"] = json!(warnings);
    reply["readiness"] = crate::readiness::harness_report_here(&manifests(&value));
    Ok(reply)
}

/// Every list a routes file sets, by where it sits: `crew`, or
/// `ships.<ship>.crew`.
fn lists(value: Option<&Value>) -> Map<String, Value> {
    let mut lists = Map::new();
    let Some(value) = value else {
        return lists;
    };
    for kind in KINDS {
        if let Some(list) = value.get(kind) {
            lists.insert(kind.to_owned(), list.clone());
        }
    }
    for (ship, own) in value["ships"].as_object().into_iter().flatten() {
        for (kind, list) in own.as_object().into_iter().flatten() {
            lists.insert(format!("ships.{ship}.{kind}"), list.clone());
        }
    }
    lists
}

/// What a temporary change in force changed and when it reverts: each list
/// and route it set differently from the routes it replaced, with what comes
/// back (null where nothing was set), and the change it stacked on, if any.
fn temporary_view(value: &Value) -> Value {
    let temporary = &value["temporary"];
    let previous = Some(&temporary["previous"]).filter(|p| !p.is_null());
    let until_ms = temporary["until_ms"].as_i64().unwrap_or_default();
    let (now, before) = (lists(Some(value)), lists(previous));
    let mut changed = Map::new();
    for key in now.keys().chain(before.keys()) {
        if now.get(key) != before.get(key) {
            changed.insert(key.clone(), before.get(key).cloned().unwrap_or(Value::Null));
        }
    }
    let before_routes = previous.map_or(Value::Null, |p| p["routes"].clone());
    for (name, route) in value["routes"].as_object().into_iter().flatten() {
        let was = &before_routes[name.as_str()];
        if was != route {
            changed.insert(format!("routes.{name}"), was.clone());
        }
    }
    for (name, was) in before_routes.as_object().into_iter().flatten() {
        if value["routes"].get(name).is_none() {
            changed.insert(format!("routes.{name}"), was.clone());
        }
    }
    let mut view = json!({
        "until_ms": until_ms,
        "reverts_at": crate::routes::utc_time(until_ms),
        "set_ms": temporary["set_ms"],
        "reverts": changed,
        "stacked_on": previous.and_then(|p| p.get("temporary")).map(temporary_view_of),
    });
    if let Some(reset) = temporary.get("until_reset") {
        view["until_reset"] = reset.clone();
    }
    view
}

fn temporary_view_of(temporary: &Value) -> Value {
    let until_ms = temporary["until_ms"].as_i64().unwrap_or_default();
    json!({"until_ms": until_ms, "reverts_at": crate::routes::utc_time(until_ms)})
}

/// The ships a revert from `from` to `to` concerns: each ship whose own
/// lists change, or every ship the routes file covers when a fleet list or a
/// route itself changes. `None` stands for every ship.
fn concerned(from: Option<&Value>, to: Option<&Value>) -> Option<Vec<String>> {
    let fleet = |value: Option<&Value>| {
        value.map(|value| {
            let mut value = value.clone();
            if let Some(object) = value.as_object_mut() {
                object.remove("ships");
                object.remove("temporary");
            }
            value
        })
    };
    if fleet(from) != fleet(to) {
        return None;
    }
    let ships = |value: Option<&Value>| value.map_or(Value::Null, |v| v["ships"].clone());
    let (from, to) = (ships(from), ships(to));
    let mut names: Vec<String> = from
        .as_object()
        .into_iter()
        .chain(to.as_object())
        .flat_map(|ships| ships.keys().cloned())
        .filter(|ship| from.get(ship) != to.get(ship))
        .collect();
    names.sort();
    names.dedup();
    Some(names)
}

/// The dispatcher pass's revert: write back the routes each ended temporary
/// change replaced, audit it here, and tell each concerned ship with a low
/// notice. Most passes read the file and find nothing due.
pub(crate) fn expire(store: &Store) -> Result<Option<Value>> {
    if crate::presence::folder_ship(store).is_none() {
        return Ok(None);
    }
    let now = Store::now_ms();
    let due = |value: &Option<Value>| {
        value
            .as_ref()
            .and_then(|value| value["temporary"]["until_ms"].as_i64())
            .is_some_and(|until| until <= now)
    };
    if !due(&read()?) {
        return Ok(None);
    }
    // Read again under the lock: a routes.set since replaced the file, and
    // what it wrote is what reverts now, if anything.
    let mut conn = crate::fleet::registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let ended = read()?;
    if !due(&ended) {
        return Ok(None);
    }
    let restored = in_force(ended.clone(), now);
    write(restored.as_ref())?;
    let members = crate::fleet::members(&tx)?;
    tx.commit()?;
    let temporary = ended
        .as_ref()
        .map_or(Value::Null, |v| v["temporary"].clone());
    let ships: Vec<(String, std::path::PathBuf)> =
        match concerned(ended.as_ref(), restored.as_ref()) {
            None => members.into_iter().map(|m| (m.name, m.home)).collect(),
            Some(names) => members
                .into_iter()
                .filter(|m| names.contains(&m.name))
                .map(|m| (m.name, m.home))
                .collect(),
        };
    let id = temporary["id"].as_str().unwrap_or_default().to_owned();
    let until_ms = temporary["until_ms"].as_i64().unwrap_or(now);
    let names: Vec<&str> = ships.iter().map(|(name, _)| name.as_str()).collect();
    store.audit(
        "routes.reverted",
        &json!({"id": id, "set_ms": temporary["set_ms"], "until_ms": until_ms,
            "until_reset": temporary.get("until_reset"), "ships": names,
            "temporary_left": restored.as_ref().and_then(|r| r.get("temporary")).is_some()}),
    )?;
    let mut told = vec![];
    for (ship, home) in &ships {
        let mut body = format!(
            "The captain's temporary routes change ended at {}; the routes before it are back.",
            crate::routes::utc_time(until_ms)
        );
        if let Some(restored) = &restored {
            for kind in KINDS {
                let names = order(restored, ship, kind);
                if !names.is_empty() {
                    body.push_str(&format!(" {kind}: {}.", names.join(", ")));
                }
            }
        } else {
            body.push_str(" No routes file is in force.");
        }
        let notice = json!({"id": format!("routes-reverted-{id}-{ship}"), "from": "captain",
            "to": ship, "kind": "notice", "urgency": "low", "body": body,
            "payload": {"routes_reverted": {"id": id, "until_ms": until_ms}}});
        let sent = if home.as_path() == store.home() {
            crate::mail::record_runtime(store, &notice)
        } else {
            Store::open(home).and_then(|other| crate::mail::record_runtime(&other, &notice))
        };
        match sent {
            Ok(_) => told.push(ship.clone()),
            Err(error) => {
                eprintln!("privateer: could not tell {ship} its routes reverted: {error:#}")
            }
        }
    }
    Ok(Some(
        json!({"reverted": id, "until_ms": until_ms, "told": told}),
    ))
}

/// Each route's harness manifest, with the route that names it.
fn manifests(value: &Value) -> Vec<(String, String)> {
    value["routes"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, route)| {
            Some((
                format!("route:{name}"),
                route["manifest"].as_str()?.to_owned(),
            ))
        })
        .collect()
}

fn warnings(value: &Value) -> Vec<String> {
    let mut warnings = vec![];
    for (name, route) in value["routes"].as_object().into_iter().flatten() {
        if route["note"]
            .as_str()
            .is_some_and(|note| note.trim_start().starts_with("STRENGTH"))
        {
            warnings.push(format!(
                "route {name} note starts with STRENGTH; its tier and uses now carry that, so remove the prefix"
            ));
        }
    }
    // A review list holds reviewer routes, and a review always needs the
    // `review` use at tier 1; an errand list may be asked to review.
    for kind in ["errand", "review"] {
        for name in listed(value, kind) {
            let route = &value["routes"][name.as_str()];
            let review_use = uses(route).iter().any(|word| word == "review");
            match kind {
                "review" => {
                    let tiered = route.get("tier").and_then(Value::as_u64) == Some(1);
                    if !tiered || !review_use {
                        warnings.push(format!(
                            "route {name} is on a review list but is not tier 1 with the review use"
                        ));
                    }
                }
                _ => {
                    if !review_use {
                        warnings.push(format!(
                            "route {name} is on an errand list but has no review use"
                        ));
                    }
                }
            }
        }
    }
    warnings.sort();
    warnings.dedup();
    warnings
}

/// Every route name a kind lists, fleet-wide and per ship.
fn listed(value: &Value, kind: &str) -> Vec<String> {
    let mut names = names_of(&value[kind]);
    if let Some(ships) = value["ships"].as_object() {
        for lists in ships.values() {
            names.extend(names_of(&lists[kind]));
        }
    }
    names
}

fn names_of(list: &Value) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n.as_str().map(str::to_owned))
        .collect()
}

/// Which list a ship's kind takes: its own list when it has one, else the
/// fleet's. A ship's own list replaces the fleet's rather than adding to it.
pub(crate) fn source(policy: &Value, ship: &str, kind: &str) -> &'static str {
    if policy["ships"][ship].get(kind).is_some() {
        "ship"
    } else if policy.get(kind).is_some() {
        "fleet"
    } else {
        "none"
    }
}

pub(crate) fn order(policy: &Value, ship: &str, kind: &str) -> Vec<String> {
    let list = policy["ships"][ship]
        .get(kind)
        .or_else(|| policy.get(kind))
        .and_then(Value::as_array);
    list.into_iter()
        .flatten()
        .filter_map(|n| n.as_str().map(str::to_owned))
        .collect()
}

/// A time of day, `HH:MM` in UTC, as the minute of the day.
fn utc_minute(value: &Value) -> Option<u32> {
    let text = value.as_str()?;
    let shape = text.len() == 5
        && text.bytes().enumerate().all(|(i, b)| {
            if i == 2 {
                b == b':'
            } else {
                b.is_ascii_digit()
            }
        });
    let (hour, minute) = (
        text.get(..2)?.parse::<u32>().ok()?,
        text.get(3..)?.parse::<u32>().ok()?,
    );
    (shape && hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

/// The captain's free routes by route id, each with the minute of the UTC day
/// its allowance renews, when the captain knows it.
pub(crate) fn free_allowances(
    store: &Store,
) -> Result<std::collections::HashMap<String, Option<u32>>> {
    let Some(policy) = for_store(store)? else {
        return Ok(Default::default());
    };
    let routes = policy["routes"].as_object().into_iter().flatten();
    Ok(routes
        .filter(|(_, route)| route["cost"] == "free")
        .filter_map(|(_, route)| {
            Some((
                identity(route)?,
                route.get("allowance_resets_utc").and_then(utc_minute),
            ))
        })
        .collect())
}

/// How a route's allowance renews.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// Renews at this minute of the UTC day.
    Daily(u32),
    /// A window that opens at first use and closes this long after; an
    /// optional weekly cap renews at this minute of the UTC week (Monday 00:00
    /// is 0).
    Rolling { window_ms: i64, weekly: Option<u32> },
}

/// A route's declared allowance, keyed by route identity in `allowances`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Allowance {
    pub shape: Shape,
    pub free: bool,
    /// The runs the window holds, when the captain states a budget. Only a
    /// harness that reports nothing about its plan is measured this way.
    pub runs: Option<u64>,
}

const DAY_MS: i64 = 86_400_000;
const WEEK_MS: i64 = 7 * DAY_MS;
/// The Unix epoch fell on a Thursday; the first Monday 00:00 UTC came four days later.
const FIRST_MONDAY_MS: i64 = 4 * DAY_MS;
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

fn shape(value: &Value) -> Result<Shape> {
    let object = value.as_object().context("must be an object")?;
    for key in object.keys() {
        ensure!(
            matches!(
                key.as_str(),
                "daily_utc" | "window_hours" | "weekly_utc" | "runs"
            ),
            "unknown setting {key}"
        );
    }
    match (value.get("daily_utc"), value.get("window_hours")) {
        (Some(at), None) => {
            ensure!(
                value.get("weekly_utc").is_none(),
                "weekly_utc goes with window_hours"
            );
            Ok(Shape::Daily(
                utc_minute(at).context("daily_utc must be a UTC time of day, HH:MM")?,
            ))
        }
        (None, Some(hours)) => {
            let hours = hours
                .as_i64()
                .filter(|h| (1..=168).contains(h))
                .context("window_hours must be a whole number from 1 to 168")?;
            let weekly = value
                .get("weekly_utc")
                .map(|at| {
                    weekly_minute(at).context("weekly_utc must read like \"Mon 07:00\" (UTC)")
                })
                .transpose()?;
            Ok(Shape::Rolling {
                window_ms: hours * 3_600_000,
                weekly,
            })
        }
        _ => bail!("must hold exactly one of daily_utc or window_hours"),
    }
}

/// The runs a window holds, when the captain states a budget. It has no
/// default: no runtime can tell what a plan allows, and a route that reports
/// its own usage is never measured this way.
fn runs(value: &Value) -> Result<Option<u64>> {
    match value.get("runs") {
        None => Ok(None),
        Some(runs) => runs
            .as_u64()
            .filter(|runs| (1..=MAX_RUNS).contains(runs))
            .map(Some)
            .context("runs must be a whole number of runs from 1 to 10000"),
    }
}

/// `"Mon HH:MM"` in UTC, as the minute of the week from Monday 00:00.
fn weekly_minute(value: &Value) -> Option<u32> {
    let (day, time) = value.as_str()?.split_once(' ')?;
    let day = WEEKDAYS.iter().position(|d| *d == day)? as u32;
    Some(day * 1440 + utc_minute(&json!(time))?)
}

/// The next time at or after `now` past this minute of the UTC day (a full day
/// on, if it is now).
pub(crate) fn next_daily(minute: u32, now: i64) -> i64 {
    let delay = (i64::from(minute) * 60_000 - now).rem_euclid(DAY_MS);
    now + if delay == 0 { DAY_MS } else { delay }
}

/// The next time past this minute of the UTC week.
pub(crate) fn next_weekly(minute: u32, now: i64) -> i64 {
    let delay = (FIRST_MONDAY_MS + i64::from(minute) * 60_000 - now).rem_euclid(WEEK_MS);
    now + if delay == 0 { WEEK_MS } else { delay }
}

/// The captain's shaped routes by route identity.
pub(crate) fn allowances(store: &Store) -> Result<std::collections::HashMap<String, Allowance>> {
    let Some(policy) = for_store(store)? else {
        return Ok(Default::default());
    };
    Ok(policy["routes"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, route)| Some((identity(route)?, allowance(route)?)))
        .collect())
}

fn allowance(route: &Value) -> Option<Allowance> {
    let value = route.get("allowance")?;
    Some(Allowance {
        shape: shape(value).ok()?,
        free: route["cost"] == "free",
        runs: runs(value).ok()?,
    })
}

fn tier(route: &Value) -> u64 {
    route["tier"].as_u64().unwrap_or(1)
}

/// Whether a route requires a named launch rather than automatic selection.
/// This does not change list membership or authorization of continuations.
fn explicit_only(route: &Value) -> bool {
    route["explicit_only"].as_bool().unwrap_or(false)
}

fn explicit_only_reason(name: &str) -> String {
    format!("{name}: explicit-only; name this route to use it")
}

/// A route's cost, selection policy, tier and uses as `ship.profile` reports
/// them. A route with no tier reports a null tier.
pub(crate) fn facts(route: &Value) -> Value {
    json!({
        "cost": route["cost"],
        "tier": route.get("tier").map(|_| tier(route)),
        "uses": uses(route),
        "explicit_only": explicit_only(route),
    })
}

/// The words in a route's `uses`, as declared.
fn uses(route: &Value) -> Vec<String> {
    route["uses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|word| word.as_str().map(str::to_owned))
        .collect()
}

/// A route's strength as bearings and profiles show it: its tier and uses, or
/// `unranked` when it states no tier.
fn strength(route: &Value) -> String {
    if route.get("tier").is_none() {
        return "unranked".to_owned();
    }
    let words = uses(route);
    if words.is_empty() {
        format!("tier {}, no uses", tier(route))
    } else {
        format!("tier {}, uses {}", tier(route), words.join("/"))
    }
}

/// The order to try a list in: shaped routes with a deadline nearest their
/// loss, then idle rolling windows (by their weekly reset when known), then
/// carry-over routes. Equal keys keep the captain's order. A tier is a floor a
/// launch requires, so it never reorders a list. `usage` is keyed by route
/// identity.
pub(crate) fn rank(
    names: &[String],
    policy: &Value,
    usage: &std::collections::HashMap<String, crate::routes::Usage>,
    now: i64,
) -> Vec<String> {
    let mut keyed: Vec<((u8, i64), &String)> = names
        .iter()
        .map(|name| {
            let route = &policy["routes"][name.as_str()];
            let used = identity(route).and_then(|id| usage.get(&id));
            let (class, key) = match allowance(route).map(|a| a.shape) {
                None => (2, 0),
                Some(Shape::Daily(minute)) => (0, next_daily(minute, now)),
                Some(Shape::Rolling { weekly, .. }) => {
                    let weekly = used
                        .and_then(|u| u.weekly_ends_ms)
                        .filter(|end| *end > now)
                        .or_else(|| weekly.map(|w| next_weekly(w, now)));
                    match used.and_then(|u| u.window_ends_ms).filter(|end| *end > now) {
                        Some(end) => (0, weekly.map_or(end, |w| w.min(end))),
                        None => (1, weekly.unwrap_or(i64::MAX)),
                    }
                }
            };
            ((class, key / 60_000), name)
        })
        .collect();
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, name)| name.clone()).collect()
}

pub(crate) fn identity(route: &Value) -> Option<String> {
    crate::routes::route_of(route["manifest"].as_str()?, &route["options"])
}

/// The reviewer a review runs on, with the overlap rule's verdict on it.
#[derive(Clone, Debug)]
pub struct Reviewer {
    /// The chosen route, whose `why` line the review record keeps.
    pub choice: Choice,
    /// `Some("first_mate")` when the reviewer's model is one only the ship's
    /// first mate wrote the code on: allowed, and recorded with the review.
    pub overlap: Option<String>,
    /// Every review runs in a new session: never a warm one.
    pub fresh: bool,
}

/// Review candidates after the overlap rule: what is left to offer, in the
/// order to offer it, and what the code's authors excluded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReviewOrder {
    /// Candidates cheapest first, those with no overlap ahead of those that
    /// overlap only the first mate's models. The model is the overlapping
    /// one, when the candidate overlaps.
    pub(crate) offered: Vec<(String, Option<String>)>,
    /// Excluded candidates, with why: a crew job ran on their model, a crew
    /// job ran on an alias their model carries, or their own model cannot be
    /// named.
    pub(crate) excluded: Vec<(String, String)>,
}

/// Order review candidates: routes whose model wrote none of the code first,
/// then routes that overlap only the first mate's models; within each group
/// `ranked`'s order (cheapest first). A route whose model a crew job wrote is
/// excluded outright, with no exception, and so is one whose model cannot be
/// named, and one whose model carries the bare alias of a job the ship
/// declared: the alias names no version, so every model that carries it is
/// excluded, whatever its version.
pub(crate) fn order_reviewers(
    ranked: &[String],
    policy: &Value,
    authors: &crate::review::authors::Authors,
) -> ReviewOrder {
    let mut clean = vec![];
    let mut overlap = vec![];
    let mut excluded = vec![];
    for name in ranked {
        let route = &policy["routes"][name.as_str()];
        let model = identity(route)
            .as_deref()
            .and_then(crate::routes::model_key);
        // The aliases a declared job ran on, as carried by this model. A
        // declaration cannot prove which version its job's alias resolved
        // to, so every model that carries the alias is excluded; the
        // exclusion outranks a first-mate overlap, since the job is crew.
        let carried_aliases = |model: &str| {
            authors
                .crew_aliases
                .iter()
                .filter(|(alias, _)| model.contains(alias.as_str()))
                .collect::<Vec<_>>()
        };
        match model {
            Some(model) if authors.crew_jobs.contains_key(&model) => {
                let jobs = authors.crew_jobs[&model].join(", ");
                excluded.push((
                    name.clone(),
                    format!("crew job(s) {jobs} ran on its model {model}"),
                ));
            }
            Some(model) if !carried_aliases(&model).is_empty() => {
                let why = carried_aliases(&model)
                    .into_iter()
                    .map(|(alias, jobs)| {
                        format!(
                            "crew job(s) {} ran on the alias {alias} its model \
                             {model} carries",
                            jobs.join(", ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                excluded.push((name.clone(), why));
            }
            Some(model) if authors.first_mate_models.contains(&model) => {
                overlap.push((name.clone(), Some(model)));
            }
            Some(_) => clean.push((name.clone(), None)),
            None => excluded.push((
                name.clone(),
                "its model cannot be named (no model, or a bare alias such as opus)".to_owned(),
            )),
        }
    }
    clean.extend(overlap);
    ReviewOrder {
        offered: clean,
        excluded,
    }
}

/// The route a review's reviewer runs on: the cheapest route on the ship's
/// `review` list that meets review at tier 1, whose model no crew job in the
/// author set wrote, and that carries no alias a declared job ran on. A model
/// only the first mate wrote on is allowed, with the overlap recorded. A
/// route whose harness is not installed here is passed over. There
/// is no fall back to any other list, and no warm session: the reply says
/// what is missing instead.
pub fn choose_reviewer(
    store: &Store,
    ship: &str,
    authors: &crate::review::authors::Authors,
) -> Result<Reviewer> {
    ensure!(
        authors.unknown_jobs.is_empty(),
        "cannot choose a reviewer for {ship}: the model of crew job(s) {} cannot \
         be named; declare that job's route in author_routes",
        authors.unknown_jobs.join(", ")
    );
    let policy = for_store(store)?.with_context(|| {
        format!("the captain has listed no routes to choose a reviewer for {ship} from")
    })?;
    let names = order(&policy, ship, "review");
    ensure!(
        !names.is_empty(),
        "the captain has listed no review routes for {ship}"
    );
    let require = Require {
        r#use: Some("review".to_owned()),
        tier: Some(1),
    };
    let qualifying: Vec<String> = names
        .iter()
        .filter(|n| meets(&policy["routes"][n.as_str()], Some(&require)))
        .cloned()
        .collect();
    ensure!(
        !qualifying.is_empty(),
        "no review route for {ship} meets {}:\n{}",
        require.summary(),
        names
            .iter()
            .map(|n| {
                format!(
                    "  {n}: {}",
                    describe_strength(&policy["routes"][n.as_str()])
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    );
    // Keep explicit-only routes out of author matching as well as selection:
    // they are authorized choices, not automatic reviewer candidates.
    let (manual, qualifying): (Vec<_>, Vec<_>) = qualifying
        .into_iter()
        .partition(|name| explicit_only(&policy["routes"][name.as_str()]));
    let manual: Vec<_> = manual
        .iter()
        .map(|name| explicit_only_reason(name))
        .collect();
    if qualifying.is_empty() {
        return Err(every_refused("review", ship, &manual, &[]));
    }
    let usage = crate::routes::usage(&crate::jobs::host_ledger(store)?)?;
    let ordered = order_reviewers(
        &rank(&qualifying, &policy, &usage, Store::now_ms()),
        &policy,
        authors,
    );
    ensure!(
        !ordered.offered.is_empty(),
        "every review route for {ship} is excluded by the code's authors:\n{}",
        ordered
            .excluded
            .iter()
            .map(|(name, why)| {
                format!(
                    "  {name} ({}): {why}",
                    describe_strength(&policy["routes"][name.as_str()])
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    );
    let mut refusals = manual;
    let mut missing = vec![];
    for (name, overlap) in ordered.offered {
        let route = &policy["routes"][name.as_str()];
        let absent = harness_missing(store, route)?;
        if !absent.is_empty() {
            refusals.push(format!("{name}: {}", unavailable(&absent)));
            missing.extend(absent);
            continue;
        }
        let Some(id) = identity(route) else {
            refusals.push(format!("{name}: its manifest names no route"));
            continue;
        };
        if let Some(refusal) = crate::routes::refusal_for(store, &id, Who::Crew, limits(route))? {
            refusals.push(format!("{name}: {refusal}"));
            continue;
        }
        let why = match &overlap {
            Some(_) => format!(
                "cheapest on {ship}'s review list clear of the code's authors, \
                 overlapping only the first mate's models: {name}"
            ),
            None => format!("cheapest on {ship}'s review list clear of the code's authors: {name}"),
        };
        return Ok(Reviewer {
            choice: Choice {
                name: name.clone(),
                manifest: route["manifest"].clone(),
                options: route.get("options").cloned().unwrap_or(json!({})),
                identity: Some(id),
                tier: route["tier"].as_u64(),
                uses: uses(route),
                why,
            },
            overlap: overlap.map(|_| "first_mate".to_owned()),
            fresh: true,
        });
    }
    Err(every_refused("review", ship, &refusals, &missing))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Require {
    pub r#use: Option<String>,
    pub tier: Option<u64>,
}

impl Require {
    pub fn summary(&self) -> String {
        match (&self.r#use, self.tier) {
            (Some(u), Some(t)) => format!("{u} at tier <= {t}"),
            (Some(u), None) => u.clone(),
            (None, Some(t)) => format!("tier <= {t}"),
            (None, None) => "no requirement".to_owned(),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        if let Some(ref u) = self.r#use {
            map.insert("use".to_string(), Value::String(u.clone()));
        }
        if let Some(t) = self.tier {
            map.insert("tier".to_string(), Value::Number(t.into()));
        }
        Value::Object(map)
    }
}

pub(crate) fn parse_require(val: Option<&Value>) -> Result<Option<Require>> {
    let Some(val) = val else {
        return Ok(None);
    };
    if val.is_null() {
        return Ok(None);
    }
    let obj = val.as_object().context("require must be an object")?;
    for k in obj.keys() {
        ensure!(
            matches!(k.as_str(), "use" | "tier"),
            "unknown require field: {k}"
        );
    }
    let r_use = match obj.get("use") {
        Some(u) if !u.is_null() => {
            let u = u.as_str().context("require use must be a string")?;
            ensure!(
                USES.contains(&u),
                "unknown require use: {u}; valid uses are: {}",
                USES.join(", ")
            );
            Some(u.to_owned())
        }
        _ => None,
    };
    let r_tier = match obj.get("tier") {
        Some(t) if !t.is_null() => {
            let t = t
                .as_u64()
                .context("require tier must be a whole number from 1 (strongest) to 9")?;
            ensure!(
                (1..=9).contains(&t),
                "require tier must be a whole number from 1 (strongest) to 9"
            );
            Some(t)
        }
        _ => None,
    };
    ensure!(
        r_use.is_some() || r_tier.is_some(),
        "require must state at least 'use' or 'tier'"
    );
    Ok(Some(Require {
        r#use: r_use,
        tier: r_tier,
    }))
}

/// A route's strength description: its tier and uses, or `unranked`.
pub(crate) fn describe_strength(route: &Value) -> String {
    let Some(t) = route.get("tier").and_then(Value::as_u64) else {
        return "unranked".to_owned();
    };
    let words = uses(route);
    if words.is_empty() {
        format!("tier {t} with no uses")
    } else {
        format!("tier {t} with uses [{}]", words.join(", "))
    }
}

/// Whether a route meets a requirement. An unranked route (no tier or no uses)
/// never meets any requirement.
pub(crate) fn meets(route: &Value, require: Option<&Require>) -> bool {
    let Some(req) = require else {
        return true;
    };
    let Some(route_tier) = route.get("tier").and_then(Value::as_u64) else {
        return false;
    };
    let route_uses = uses(route);
    if route_uses.is_empty() {
        return false;
    }
    if let Some(req_tier) = req.tier {
        if route_tier > req_tier {
            return false;
        }
    }
    if let Some(req_use) = &req.r#use {
        if !route_uses.iter().any(|u| u == req_use) {
            return false;
        }
    }
    true
}

/// A route chosen for a launch.
#[derive(Clone, Debug)]
pub struct Choice {
    pub name: String,
    pub manifest: Value,
    pub options: Value,
    pub identity: Option<String>,
    pub tier: Option<u64>,
    pub uses: Vec<String>,
    pub why: String,
}

impl Choice {
    pub fn to_route_json(&self) -> Value {
        json!({
            "name": self.name,
            "identity": self.identity,
            "tier": self.tier,
            "uses": self.uses,
            "why": self.why,
        })
    }
}

/// First listed available route for a ship, when a fleet policy provides one.
/// An empty list preserves callers' existing route defaults. A charter-pinned
/// provider adapter always wins over a first-mate route list.
pub(crate) fn first_mate_fallback(
    store: &Store,
    ship: &str,
    task: &str,
    profile: &Value,
) -> Result<Option<Choice>> {
    fallback(store, ship, task, profile, None)
}

/// The route a retry of a failed wake takes: the first listed route that can
/// take a first mate now, other than the one just tried. A wake that failed on
/// a provider limit or an outage moves even when its own route is clear, so
/// this never returns the route that just failed.
pub(crate) fn retry_fallback(
    store: &Store,
    ship: &str,
    task: &str,
    profile: &Value,
    failed_route: Option<&str>,
) -> Result<Option<Choice>> {
    fallback(store, ship, task, profile, failed_route)
}

/// A first mate's fallback route. Its own route is used while it can take the
/// wake; once it cannot (held off, or past the share it keeps for first mates)
/// the first listed route that can is taken instead. With `failed_route`, the
/// route just tried is passed over whatever its state, which is what a retry wants.
/// So is a listed route whose harness the last readiness check found missing.
fn fallback(
    store: &Store,
    ship: &str,
    task: &str,
    profile: &Value,
    failed_route: Option<&str>,
) -> Result<Option<Choice>> {
    let Some(own_route) = identity(&json!({
        "manifest": profile["manifest"],
        "options": profile["options"]
    })) else {
        return Ok(None);
    };
    // A charter-pinned provider adapter is the captain's route for this ship,
    // and no list may move a first mate off it.
    if store.authorize(task, "spawn")?["provider_plugin"]
        .as_object()
        .is_some()
    {
        return Ok(None);
    }
    let ledger = crate::jobs::host_ledger(store)?;
    let now = Store::now_ms();
    let Some(policy) = for_store(store)? else {
        return Ok(None);
    };
    if failed_route.is_none()
        && !crate::routes::held_off(
            &ledger,
            &own_route,
            Who::FirstMate,
            &named_limits(&policy, &own_route),
            now,
        )?
    {
        return Ok(None);
    }
    // Routes passed over, each with why, named in the chosen route's `why`.
    let mut passed = vec![];
    for name in order(&policy, ship, "first_mate") {
        let route = &policy["routes"][name.as_str()];
        if explicit_only(route) {
            passed.push(explicit_only_reason(&name));
            continue;
        }
        let Some(route_id) = identity(route) else {
            continue;
        };
        if route_id == own_route {
            continue;
        }
        if failed_route == Some(route_id.as_str()) {
            passed.push(format!("{name}: the wake just failed on it"));
            continue;
        }
        let missing = harness_missing(store, route)?;
        if !missing.is_empty() {
            passed.push(format!("{name}: {}", unavailable(&missing)));
            continue;
        }
        if crate::routes::held_off(&ledger, &route_id, Who::FirstMate, &limits(route), now)? {
            passed.push(format!("{name}: backing off or past its first-mate share"));
            continue;
        }
        let mut why =
            format!("first available on {ship}'s first_mate list: {name} (no requirement)");
        if !passed.is_empty() {
            why.push_str(&format!("; passed over {}", passed.join("; ")));
        }
        return Ok(Some(Choice {
            name: name.clone(),
            manifest: route["manifest"].clone(),
            options: route.get("options").cloned().unwrap_or(json!({})),
            identity: Some(route_id),
            tier: route["tier"].as_u64(),
            uses: uses(route),
            why,
        }));
    }
    Ok(None)
}

/// What a first mate's profile takes from the captain's first_mate list.
#[derive(Clone, Debug)]
pub(crate) enum Listed {
    /// The profile names its own model, the charter pins a provider adapter,
    /// or no first_mate routes are listed for the ship: the profile runs as
    /// it is.
    Unlisted,
    /// The first listed route that can take the first mate now.
    Route(Choice),
    /// Every listed route is backing off, past its first-mate share or
    /// missing its harness, with each one's reason. The first mate waits
    /// rather than run on its harness's own default.
    Unavailable(String),
}

/// The route a first mate whose profile names no model runs on: the first on
/// the ship's first_mate list that is not held off and whose harness the last
/// readiness check did not find missing, read at each launch so it follows
/// the routes file. A profile with its own model, set by `ship.put` or
/// `init`, always wins, and so does a charter-pinned provider adapter.
pub(crate) fn listed_first_mate(store: &Store, ship: &str, profile: &Value) -> Result<Listed> {
    listed_for(store, ship, profile, Who::FirstMate)
}

/// `listed_first_mate` for a launch of kind `who`: crew that take the first
/// mate's route are judged against each listed route's crew share, through
/// the same list order, so a route past it is passed over for a later one.
pub(crate) fn listed_for(store: &Store, ship: &str, profile: &Value, who: Who) -> Result<Listed> {
    if profile["options"]
        .get("model")
        .is_some_and(|m| !m.is_null())
    {
        return Ok(Listed::Unlisted);
    }
    let Some(policy) = for_store(store)? else {
        return Ok(Listed::Unlisted);
    };
    let names = order(&policy, ship, "first_mate");
    if names.is_empty() {
        return Ok(Listed::Unlisted);
    }
    if let Some(duty) = profile["duty_task"].as_str()
        && store
            .authorize(duty, "spawn")
            .is_ok_and(|charter| charter["provider_plugin"].is_object())
    {
        return Ok(Listed::Unlisted);
    }
    let mut refusals = vec![];
    for name in names {
        let route = &policy["routes"][name.as_str()];
        if explicit_only(route) {
            refusals.push(explicit_only_reason(&name));
            continue;
        }
        let Some(route_id) = identity(route) else {
            refusals.push(format!("{name}: its harness cannot be named"));
            continue;
        };
        let missing = harness_missing(store, route)?;
        if !missing.is_empty() {
            refusals.push(format!("{name}: {}", unavailable(&missing)));
            continue;
        }
        if let Some(refusal) = crate::routes::refusal_for(store, &route_id, who, limits(route))? {
            refusals.push(format!("{name}: {refusal}"));
            continue;
        }
        let mut why = format!(
            "first available on {ship}'s first_mate list: {name} (the profile names no model)"
        );
        if !refusals.is_empty() {
            why.push_str(&format!("; passed over {}", refusals.join("; ")));
        }
        return Ok(Listed::Route(Choice {
            name: name.clone(),
            manifest: route["manifest"].clone(),
            options: route.get("options").cloned().unwrap_or(json!({})),
            identity: Some(route_id),
            tier: route["tier"].as_u64(),
            uses: uses(route),
            why,
        }));
    }
    Ok(Listed::Unavailable(format!(
        "every first_mate route for {ship} is unavailable: {}",
        refusals.join("; ")
    )))
}

/// A first-mate profile moved onto `choice`: its manifest, and its options
/// over the profile's own on the same harness, such as a sandbox or
/// permission mode, with the provider and model the route's alone. On another
/// harness the route's options, plugin config and tool bundles are not
/// carried, since they are the profile harness's own.
pub(crate) fn onto(profile: &mut Value, choice: &Choice) {
    let harness = |manifest: &Value| manifest.as_str().and_then(crate::bundled::harness);
    let same = harness(&profile["manifest"]).is_some()
        && harness(&profile["manifest"]) == harness(&choice.manifest);
    let mut options = if same {
        profile["options"].as_object().cloned().unwrap_or_default()
    } else {
        Map::new()
    };
    options.remove("provider");
    options.remove("model");
    options.extend(choice.options.as_object().cloned().unwrap_or_default());
    profile["manifest"] = choice.manifest.clone();
    profile["options"] = Value::Object(options);
    if !same && let Some(object) = profile.as_object_mut() {
        object.remove("config");
        object.remove("tool_bundles");
    }
}

/// The tools a route's harness requires that this machine's last readiness
/// check found missing: read from the stored rows, never probed. A tool not
/// checked yet counts as installed.
fn harness_missing(store: &Store, route: &Value) -> Result<Vec<crate::readiness::Missing>> {
    match route["manifest"].as_str() {
        Some(manifest) => crate::readiness::missing_for(store.connection(), manifest),
        None => Ok(vec![]),
    }
}

/// A route passed over for its harness, in the shape of a backoff reason:
/// tools not found are not installed; one found that fails its check gives
/// that check's own why.
fn unavailable(missing: &[crate::readiness::Missing]) -> String {
    format!("unavailable: {}", missing_reasons(missing))
}

fn missing_reasons(missing: &[crate::readiness::Missing]) -> String {
    let absent: Vec<&str> = missing
        .iter()
        .filter(|m| m.failed.is_none())
        .map(|m| m.tool.as_str())
        .collect();
    let mut reasons: Vec<String> = missing.iter().filter_map(|m| m.failed.clone()).collect();
    if !absent.is_empty() {
        reasons.insert(0, format!("{} not installed", absent.join(", ")));
    }
    reasons.join("; ")
}

/// A bearings line for each route on `ship`'s lists whose harness the last
/// readiness check found missing, as backed-off routes get theirs, so a first
/// mate learns before a launch that its routes cannot run here.
pub(crate) fn missing_lines(store: &Store, ship: &str) -> Result<Vec<String>> {
    let Some(policy) = for_store(store)? else {
        return Ok(vec![]);
    };
    let mut seen: Vec<String> = vec![];
    let mut lines = vec![];
    for kind in KINDS {
        for name in order(&policy, ship, kind) {
            if seen.contains(&name) {
                continue;
            }
            let missing = harness_missing(store, &policy["routes"][name.as_str()])?;
            if !missing.is_empty() {
                lines.push(format!(
                    "Model route {name} is unavailable ({}); launches pass it over, and one that names it is refused.",
                    missing_reasons(&missing)
                ));
            }
            seen.push(name);
        }
    }
    Ok(lines)
}

/// The install step for every missing tool, once each, to close a refusal.
fn install_steps(missing: &[crate::readiness::Missing]) -> String {
    let mut steps: Vec<String> = missing
        .iter()
        .map(|m| format!("{}: {}", m.tool, m.install))
        .collect();
    steps.sort();
    steps.dedup();
    let heading = if missing.iter().all(|m| m.failed.is_none()) {
        "not installed on this machine"
    } else {
        "to fix on this machine"
    };
    format!(
        "{heading}: {}; see runtime.needs readiness",
        steps.join("; ")
    )
}

/// Refuse a route the launch named when its harness is missing: it is never
/// swapped for another.
fn refuse_uninstalled(store: &Store, name: &str, route: &Value) -> Result<()> {
    let missing = harness_missing(store, route)?;
    ensure!(
        missing.is_empty(),
        "route {name} is {}; {}",
        unavailable(&missing),
        install_steps(&missing)
    );
    Ok(())
}

/// Every route was refused: why each was, and, when harnesses are missing,
/// how to install every one.
fn every_refused(
    kind: &str,
    ship: &str,
    refusals: &[String],
    missing: &[crate::readiness::Missing],
) -> anyhow::Error {
    let mut text = format!(
        "every {kind} route for {ship} is unavailable:\n{}",
        refusals.join("\n")
    );
    if !missing.is_empty() {
        text.push('\n');
        text.push_str(&install_steps(missing));
    }
    anyhow::anyhow!(text)
}

fn check_requirement(
    name: &str,
    route: &Value,
    require: Option<&Require>,
    ship: &str,
    kind: &str,
    qualifying: &[String],
) -> Result<()> {
    if let Some(req) = require {
        if !meets(route, Some(req)) {
            let meeting = if qualifying.is_empty() {
                format!("no routes on {ship}'s {kind} list meet it")
            } else {
                format!(
                    "routes on {ship}'s {kind} list that meet it: {}",
                    qualifying.join(", ")
                )
            };
            bail!(
                "route {name} is {}; this launch needs {}; {meeting}",
                describe_strength(route),
                req.summary()
            );
        }
    }
    Ok(())
}

/// The route a ship's crew or errand launch should use: the one it names, or
/// the cheapest route meeting `require` that is not backing off in `rank`'s order.
/// A route whose harness the last readiness check found missing is passed
/// over like one backing off, and the chosen route's `why` says so; one the
/// launch names is refused instead. When no route remains the refusal names
/// every missing tool with its install step.
/// `None` means the captain lists no routes of this kind, so old defaults apply.
pub(crate) fn choose(
    store: &Store,
    ship: &str,
    kind: &str,
    requested: Option<&str>,
    require: Option<&Require>,
) -> Result<Option<Choice>> {
    let Some(policy) = for_store(store)? else {
        ensure!(
            requested.is_none() && require.is_none(),
            "the captain has listed no routes to choose from"
        );
        return Ok(None);
    };
    let names = order(&policy, ship, kind);
    if names.is_empty() {
        ensure!(
            requested.is_none() && require.is_none(),
            "the captain has listed no {kind} routes for {ship}"
        );
        return Ok(None);
    }
    let qualifying: Vec<String> = names
        .iter()
        .filter(|n| meets(&policy["routes"][n.as_str()], require))
        .cloned()
        .collect();
    let candidates: Vec<String> = match requested {
        Some(name) => {
            ensure!(
                names.iter().any(|n| n == name),
                "route {name} is not one of {ship}'s {kind} routes: {}",
                names.join(", ")
            );
            let route = &policy["routes"][name];
            check_requirement(name, route, require, ship, kind, &qualifying)?;
            refuse_uninstalled(store, name, route)?;
            vec![name.to_owned()]
        }
        None => {
            if let Some(req) = require {
                if qualifying.is_empty() {
                    let details: Vec<String> = names
                        .iter()
                        .map(|n| {
                            format!(
                                "  {n}: {}",
                                describe_strength(&policy["routes"][n.as_str()])
                            )
                        })
                        .collect();
                    bail!(
                        "no {kind} route for {ship} meets this launch's requirement ({}):\n{}",
                        req.summary(),
                        details.join("\n")
                    );
                }
            }
            let usage = crate::routes::usage(&crate::jobs::host_ledger(store)?)?;
            rank(&qualifying, &policy, &usage, Store::now_ms())
        }
    };
    let mut refusals = vec![];
    // Routes passed over because their harness is not installed here.
    let mut uninstalled = vec![];
    let mut missing = vec![];
    for name in candidates {
        let route = &policy["routes"][name.as_str()];
        if requested.is_none() && explicit_only(route) {
            refusals.push(explicit_only_reason(&name));
            continue;
        }
        let absent = harness_missing(store, route)?;
        if !absent.is_empty() {
            let refusal = format!("{name}: {}", unavailable(&absent));
            refusals.push(refusal.clone());
            uninstalled.push(refusal);
            missing.extend(absent);
            continue;
        }
        let id = identity(route);
        if let Some(refusal) = id
            .as_ref()
            .map(|id| crate::routes::refusal_for(store, id, Who::Crew, limits(route)))
            .transpose()?
            .flatten()
        {
            refusals.push(format!("{name}: {refusal}"));
            continue;
        }
        let why = match requested {
            Some(_) => match require {
                Some(req) => format!("explicit route meeting {}: {name}", req.summary()),
                None => format!("explicit route: {name}"),
            },
            None => match require {
                Some(req) => format!(
                    "cheapest on {ship}'s {kind} list meeting {}: {name}",
                    req.summary()
                ),
                None => format!(
                    "first in allowance order on {ship}'s {kind} list: {name} (no requirement)"
                ),
            },
        };
        let why = if uninstalled.is_empty() {
            why
        } else {
            format!("{why}; passed over {}", uninstalled.join("; "))
        };
        return Ok(Some(Choice {
            name,
            manifest: route["manifest"].clone(),
            options: route.get("options").cloned().unwrap_or(json!({})),
            identity: id,
            tier: route["tier"].as_u64(),
            uses: uses(route),
            why,
        }));
    }
    Err(every_refused(kind, ship, &refusals, &missing))
}

/// Refuse an agent's explicit harness/model unless the captain lists it for
/// this ship, it meets any requirement, and its harness is not missing here.
/// Operators may launch anything.
pub(crate) fn check(
    store: &Store,
    ship: &str,
    kind: &str,
    manifest: &str,
    options: &Value,
    require: Option<&Require>,
    resume_why: Option<&str>,
) -> Result<Option<Choice>> {
    if !matches!(store.caller(), Caller::Agent { .. }) {
        if let Some(policy) = for_store(store)? {
            let names = order(&policy, ship, kind);
            let wanted = crate::routes::route_of(manifest, options);
            if let Some(wanted) = &wanted {
                if let Some(name) = names
                    .iter()
                    .find(|n| identity(&policy["routes"][n.as_str()]) == Some(wanted.clone()))
                {
                    let route = &policy["routes"][name.as_str()];
                    let qualifying: Vec<String> = names
                        .iter()
                        .filter(|n| meets(&policy["routes"][n.as_str()], require))
                        .cloned()
                        .collect();
                    check_requirement(name, route, require, ship, kind, &qualifying)?;
                    let why = match resume_why {
                        Some(why) if !why.is_empty() => why.to_owned(),
                        Some(_) => format!("resumed on {name}"),
                        None => match require {
                            Some(req) => {
                                format!("explicit route meeting {}: {name}", req.summary())
                            }
                            None => format!("explicit route: {name}"),
                        },
                    };
                    return Ok(Some(Choice {
                        name: name.clone(),
                        manifest: route["manifest"].clone(),
                        options: route.get("options").cloned().unwrap_or(json!({})),
                        identity: Some(wanted.clone()),
                        tier: route["tier"].as_u64(),
                        uses: uses(route),
                        why,
                    }));
                }
            }
            if require.is_some() {
                if names.is_empty() {
                    bail!("the captain has listed no {kind} routes for {ship}");
                }
                bail!(
                    "{} is not one of {ship}'s {kind} routes; pass \"route\" with one of: {}",
                    wanted.as_deref().unwrap_or(manifest),
                    names.join(", ")
                );
            }
            return Ok(None);
        }
        if require.is_some() {
            bail!("the captain has listed no routes to choose from");
        }
        return Ok(None);
    }
    let Some(policy) = for_store(store)? else {
        ensure!(
            require.is_none(),
            "the captain has listed no routes to choose from"
        );
        return Ok(None);
    };
    let names = order(&policy, ship, kind);
    if names.is_empty() {
        ensure!(
            require.is_none(),
            "the captain has listed no {kind} routes for {ship}"
        );
        return Ok(None);
    }
    let wanted = crate::routes::route_of(manifest, options);
    let matched = wanted.as_ref().and_then(|w| {
        names
            .iter()
            .find(|n| identity(&policy["routes"][n.as_str()]).as_deref() == Some(w.as_str()))
    });
    let Some(name) = matched else {
        bail!(
            "{} is not one of {ship}'s {kind} routes; pass \"route\" with one of: {}",
            wanted.as_deref().unwrap_or(manifest),
            names.join(", ")
        );
    };
    let route = &policy["routes"][name.as_str()];
    let qualifying: Vec<String> = names
        .iter()
        .filter(|n| meets(&policy["routes"][n.as_str()], require))
        .cloned()
        .collect();
    check_requirement(name, route, require, ship, kind, &qualifying)?;
    refuse_uninstalled(store, name, route)?;
    let why = match require {
        Some(req) => format!("explicit route meeting {}: {name}", req.summary()),
        None => format!("explicit route: {name}"),
    };
    Ok(Some(Choice {
        name: name.clone(),
        manifest: route["manifest"].clone(),
        options: route.get("options").cloned().unwrap_or(json!({})),
        identity: wanted,
        tier: route["tier"].as_u64(),
        uses: uses(route),
        why,
    }))
}

/// A crew or errand replay on a route the captain no longer lists for the
/// ship (removed, or never listed). Lists that are absent or empty restrict
/// nothing, as they do for launches.
pub(crate) fn retry_refusal(store: &Store, ship: &str, route: &str) -> Result<Option<String>> {
    let Some(policy) = for_store(store)? else {
        return Ok(None);
    };
    let names: Vec<String> = ["crew", "errand"]
        .into_iter()
        .flat_map(|kind| order(&policy, ship, kind))
        .collect();
    if names.is_empty()
        || names
            .iter()
            .any(|n| identity(&policy["routes"][n.as_str()]).as_deref() == Some(route))
    {
        return Ok(None);
    }
    Ok(Some(format!(
        "route {route} is no longer one of {ship}'s crew or errand routes ({}); \
         start a fresh crew with agent.start to take the first available route",
        names.join(", ")
    )))
}

/// Whether a parked crew may resume on the route it was launched on: the
/// captain lists it for the crew's ship (or lists no crew routes, which
/// restricts nothing), it is not backing off, and the last readiness check
/// did not find its harness missing. Otherwise the crew starts fresh, on the
/// first route still offered. A resume is a runtime launch, so it is checked
/// here as an agent start is in `check`.
pub fn resume_route(store: &Store, ship: &str, manifest: &str, options: &Value) -> Result<bool> {
    let Some(policy) = for_store(store)? else {
        return Ok(true);
    };
    let names = order(&policy, ship, "crew");
    if names.is_empty() {
        return Ok(true);
    }
    let Some(wanted) = crate::routes::route_of(manifest, options) else {
        return Ok(false);
    };
    let listed = names
        .iter()
        .find(|n| identity(&policy["routes"][n.as_str()]).as_deref() == Some(wanted.as_str()));
    let Some(name) = listed else {
        return Ok(false);
    };
    let route = &policy["routes"][name.as_str()];
    if !harness_missing(store, route)?.is_empty() {
        return Ok(false);
    }
    let ledger = crate::jobs::host_ledger(store)?;
    let now = Store::now_ms();
    Ok(identity(route)
        .map(|id| crate::routes::blocked_at(&ledger, &id, now))
        .transpose()?
        .flatten()
        .is_none())
}

/// The routes in force with, for each ship that has lists of its own, which
/// list applies to each kind: the ship's own, or the fleet's. Under a
/// temporary change, `temporary` says when it reverts and what comes back in
/// place of each list and route it changed, rather than the whole file kept.
fn show(value: Option<Value>) -> Value {
    let Some(mut value) = value else {
        return Value::Null;
    };
    if value.get("temporary").is_some() {
        value["temporary"] = temporary_view(&value);
    }
    let ships: Vec<String> = value["ships"]
        .as_object()
        .into_iter()
        .flat_map(|ships| ships.keys().cloned())
        .collect();
    let mut applied = Map::new();
    for ship in ships {
        let mut lists = Map::new();
        for kind in KINDS {
            let which = source(&value, &ship, kind);
            if which != "none" {
                lists.insert(kind.to_owned(), json!(which));
            }
        }
        applied.insert(ship, Value::Object(lists));
    }
    value["applied"] = Value::Object(applied);
    value
}

/// What a route can do now, as bearings and `fleet.snapshot` read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Available,
    /// Not held off, and its declared allowance unused since it last renewed.
    Idle,
    BackingOff,
    /// Held off with its allowance spent until it reopens.
    Capped,
}

impl State {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            State::Available => "available",
            State::Idle => "idle",
            State::BackingOff => "backing_off",
            State::Capped => "capped",
        }
    }
}

/// One route's state as of a time. Every field is a stored fact or derived
/// from one, so it changes with time only when `now` crosses a stored instant
/// (a hold's end, a window's end, a daily renewal).
#[derive(Clone, Debug)]
pub(crate) struct RouteState {
    /// The captain's name for it; None for a ledger hold no listed route names.
    pub name: Option<String>,
    /// Its identity (harness/provider/model), or the ledger key held.
    pub route: Option<String>,
    pub cost: Option<String>,
    pub tier: Option<u64>,
    pub state: State,
    /// When a hold ends.
    pub until_ms: Option<i64>,
    /// The hold's classified error, as `routes::refusal` shows it.
    pub error: Option<String>,
    pub allowance: Option<Allowance>,
    pub usage: Option<crate::routes::Usage>,
    /// The share of the plan spent, when a reading or a run budget gives one.
    pub spent: Option<crate::routes::Spent>,
}

/// Every route the policy names, in name order, then each ledger hold that no
/// named route's identity is, in key order.
pub(crate) fn states(
    policy: &Value,
    ledger: &rusqlite::Connection,
    now: i64,
) -> Result<Vec<RouteState>> {
    let usage = crate::routes::usage(ledger)?;
    let capped = |id: &str, until: i64| {
        usage
            .get(id)
            .and_then(|u| u.capped_until_ms)
            .is_some_and(|cap| cap > now)
            .then_some(until)
    };
    let mut states = vec![];
    let mut named = std::collections::HashSet::new();
    for (name, route) in policy["routes"].as_object().into_iter().flatten() {
        let id = identity(route);
        let mut used = id.as_ref().and_then(|id| usage.get(id)).cloned();
        if let Some(account) = id.as_deref().and_then(crate::routes::account_key) {
            used = crate::routes::with_account_windows(used, usage.get(&account));
        }
        let declared = allowance(route);
        let blocked = match &id {
            Some(id) => crate::routes::blocked_at(ledger, id, now)?,
            None => None,
        };
        let state = match &blocked {
            Some((until, _)) if id.as_deref().and_then(|id| capped(id, *until)).is_some() => {
                State::Capped
            }
            Some(_) => State::BackingOff,
            None if declared.is_some_and(|a| idle(a.shape, used.as_ref(), now)) => State::Idle,
            None => State::Available,
        };
        named.extend(id.clone());
        let (until_ms, error) = blocked.unzip();
        states.push(RouteState {
            spent: used
                .as_ref()
                .and_then(|used| crate::routes::spent(used, declared.and_then(|a| a.runs), now)),
            name: Some(name.clone()),
            route: id,
            cost: route["cost"].as_str().map(str::to_owned),
            tier: route["tier"].as_u64(),
            state,
            until_ms,
            error,
            allowance: declared,
            usage: used,
        });
    }
    for (key, until, error) in crate::routes::held(ledger, now)? {
        if named.contains(&key) {
            continue;
        }
        let state = match capped(&key, until) {
            Some(_) => State::Capped,
            None => State::BackingOff,
        };
        let used = usage.get(&key).cloned();
        states.push(RouteState {
            spent: used
                .as_ref()
                .and_then(|used| crate::routes::spent(used, None, now)),
            name: None,
            usage: used,
            route: Some(key),
            cost: None,
            tier: None,
            state,
            until_ms: Some(until),
            error: Some(error),
            allowance: None,
        });
    }
    Ok(states)
}

/// The bearings lines: each list in order with the captain's notes and each
/// allowance's shape, routes backing off or capped marked, and then any
/// allowance that is going unused.
pub(crate) fn summary(store: &Store, ship: &str) -> Result<Option<String>> {
    let Some(policy) = for_store(store)? else {
        return Ok(None);
    };
    let ledger = crate::jobs::host_ledger(store)?;
    let now = Store::now_ms();
    let states: std::collections::HashMap<String, RouteState> = states(&policy, &ledger, now)?
        .into_iter()
        .filter_map(|state| Some((state.name.clone()?, state)))
        .collect();
    let temporary = policy.get("temporary").map(|_| {
        let view = temporary_view(&policy);
        let until = view["until_ms"].as_i64().unwrap_or_default();
        (
            until,
            view["reverts"].as_object().cloned().unwrap_or_default(),
        )
    });
    let mut parts = vec![];
    let mut shaped = false;
    let mut ranked = false;
    let mut has_explicit = false;
    let mut idle: Vec<String> = vec![];
    for kind in KINDS {
        let names = order(&policy, ship, kind);
        if names.is_empty() {
            continue;
        }
        let mut shown = vec![];
        for name in names {
            let route = &policy["routes"][name.as_str()];
            ranked |=
                kind != "first_mate" && route.get("tier").is_some() && !uses(route).is_empty();
            let cost = route["cost"].as_str().unwrap_or("?");
            let read = states.get(&name);
            let used = read.and_then(|r| r.usage.as_ref());
            let mut details = vec![cost.to_owned()];
            if explicit_only(route) {
                has_explicit = true;
                details.push("explicit-only".to_owned());
            }
            details.extend(
                route["note"]
                    .as_str()
                    .filter(|note| !note.is_empty())
                    .map(str::to_owned),
            );
            let declared = allowance(route);
            if let Some(allowance) = declared {
                shaped = true;
                let mut about = format!("{}, {}", describe(allowance.shape), strength(route));
                for (window, utilization) in [
                    ("window", used.and_then(|u| u.window_utilization)),
                    ("week", used.and_then(|u| u.weekly_utilization)),
                ] {
                    if let Some(share) = utilization {
                        about.push_str(&format!(", {window} {:.0}% used", share * 100.0));
                    }
                }
                if let Some((runs, budget)) = allowance.runs.and_then(|budget| {
                    used.map(|used| (used.runs, budget))
                        .filter(|(runs, _)| *runs > 0)
                }) {
                    about.push_str(&format!(", {runs} of {budget} runs"));
                }
                details.push(about);
            } else {
                let mut about = strength(route);
                // An unshaped route that reports its own usage still has a
                // plan to keep in reserve, so its share is shown too.
                if let Some(spent) = read.and_then(|read| read.spent.as_ref()) {
                    about.push_str(&format!(
                        ", {:.0}% of its {} used",
                        spent.share * 100.0,
                        spent.what
                    ));
                }
                details.push(about);
            }
            let mut label = format!("{name} ({})", details.join("; "));
            match read.map_or((State::Available, None), |r| (r.state, r.until_ms)) {
                (State::Capped, Some(until)) => {
                    label.push_str(&format!(" capped until {}", clock(until, now)));
                }
                (State::Capped | State::BackingOff, _) => label.push_str(" backing off"),
                (state, _) => {
                    if let Some(end) = used.and_then(|u| u.window_ends_ms).filter(|e| *e > now)
                        && matches!(declared.map(|a| a.shape), Some(Shape::Rolling { .. }))
                    {
                        label.push_str(&format!(" window open until {}", clock(end, now)));
                    }
                    if kind != "first_mate"
                        && !explicit_only(route)
                        && state == State::Idle
                        && let Some(allowance) = declared
                        && let Some(line) = idle_line(&name, allowance.shape, used, now)
                        && !idle.contains(&line)
                    {
                        idle.push(line);
                    }
                }
            }
            shown.push(label);
        }
        let (list, at) = match source(&policy, ship, kind) {
            "ship" => ("ship's own list", format!("ships.{ship}.{kind}")),
            _ => ("fleet list", kind.to_owned()),
        };
        // A list a temporary change set says when it reverts.
        let list = match &temporary {
            Some((until, reverts)) if reverts.contains_key(&at) => {
                format!("{list}, temporary until {}", clock(*until, now))
            }
            _ => list.to_owned(),
        };
        parts.push(format!("{kind}: {} ({list})", shown.join(", ")));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    let rule = if shaped {
        "use-it-or-lose-it allowances nearest their refresh go first"
    } else {
        "the first available is used"
    };
    let rule = if has_explicit {
        format!("automatic selection skips explicit-only routes; {rule}")
    } else {
        rule.to_owned()
    };
    // A launch that names a route skips this order, so where routes declare
    // their strength the line steers launches to `require` instead.
    let choice = if ranked {
        format!(
            "; pass \"require\":{{\"use\":\"<use>\",\"tier\":<n>}} (uses {}; tier 1 is strongest) \
             for the first in that order that meets it, and name a \"route\":\"<name>\" only when \
             the work needs that one route, since a named route skips the order",
            USES.join(", ")
        )
    } else {
        ", or pass \"route\":\"<name>\"".to_owned()
    };
    let mut text = format!("Routes ({}; {rule}{choice}).", parts.join("; "));
    if !idle.is_empty() {
        text.push_str(&format!("\nIdle allowances: {}.", idle.join(", ")));
    }
    Ok(Some(text))
}

pub(crate) fn describe(shape: Shape) -> String {
    match shape {
        Shape::Daily(minute) => format!("daily {:02}:{:02} UTC", minute / 60, minute % 60),
        Shape::Rolling { window_ms, weekly } => {
            let mut text = format!("{}h window", window_ms / 3_600_000);
            if let Some(minute) = weekly {
                let day = WEEKDAYS[(minute / 1440) as usize % 7];
                let minute = minute % 1440;
                text.push_str(&format!(
                    ", weekly {day} {:02}:{:02} UTC",
                    minute / 60,
                    minute % 60
                ));
            }
            text
        }
    }
}

/// An allowance with no use since it last renewed: a daily one not used since
/// its last reset, a rolling one with no window open.
fn idle(shape: Shape, used: Option<&crate::routes::Usage>, now: i64) -> bool {
    match shape {
        Shape::Daily(minute) => used
            .and_then(|u| u.last_succeeded_ms)
            .is_none_or(|at| at < next_daily(minute, now) - DAY_MS),
        Shape::Rolling { .. } => used
            .and_then(|u| u.window_ends_ms)
            .is_none_or(|end| end <= now),
    }
}

/// The bearings line for an idle allowance, when it is idle.
fn idle_line(
    name: &str,
    shape: Shape,
    used: Option<&crate::routes::Usage>,
    now: i64,
) -> Option<String> {
    if !idle(shape, used, now) {
        return None;
    }
    let last_used = match used.and_then(|u| u.last_succeeded_ms) {
        Some(at) => format!("last used {} ago", span(now - at)),
        None => "no use recorded".to_owned(),
    };
    Some(match shape {
        Shape::Daily(minute) => format!(
            "{name} ({}, refreshes in {}, {last_used})",
            describe(shape),
            span(next_daily(minute, now) - now)
        ),
        Shape::Rolling { .. } => {
            format!("{name} ({}, no window open, {last_used})", describe(shape))
        }
    })
}

/// A UTC time of day, with its weekday when it is a day or more away.
fn clock(at: i64, now: i64) -> String {
    let minute = at.div_euclid(60_000);
    let time = format!("{:02}:{:02} UTC", minute / 60 % 24, minute % 60);
    if at - now < DAY_MS {
        time
    } else {
        let day = (at - FIRST_MONDAY_MS).div_euclid(DAY_MS).rem_euclid(7) as usize;
        format!("{} {time}", WEEKDAYS[day])
    }
}

/// A duration as days, hours or minutes.
fn span(ms: i64) -> String {
    let minutes = (ms / 60_000).max(1);
    match minutes {
        m if m >= 2 * 1440 => format!("{}d", m / 1440),
        m if m >= 60 && m % 60 != 0 && m < 600 => format!("{}h{}m", m / 60, m % 60),
        m if m >= 60 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

pub fn cli(args: &[String], caller: &Caller) -> Result<()> {
    let value = match args {
        [] => show(load()?),
        [verb] if verb == "show" => show(load()?),
        [verb, file] if verb == "set" => {
            let raw = fs::read(file).with_context(|| format!("reading {file}"))?;
            ensure!(raw.len() <= 65_536, "routes file too large");
            let value = serde_json::from_slice(&raw)
                .context("invalid routes JSON (for YAML use routes.set --file)")?;
            set(caller, &value)?
        }
        _ => bail!(
            "usage: privateer routes [show|set FILE.json]; a FILE.json with \"until_ms\" or \
             \"until_reset\":\"ROUTE\" sets the routes until then"
        ),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// The argument keys of each command here; see `crate::Accepted`.
pub(crate) const ACCEPTED: crate::Accepted = &[
    (
        "routes.set",
        &[
            "routes",
            "crew",
            "errand",
            "first_mate",
            "review",
            "ships",
            "until_ms",
            "until_reset",
            // Refused by `set` itself, with a reason that names the fix.
            "temporary",
        ],
    ),
    ("routes.show", &[]),
];

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "routes.show" => Ok(show(load()?)),
        "routes.set" => {
            let value = set(store.caller(), &args)?;
            store.audit(
                "routes.set",
                &json!({"routes": value["routes"].as_object().map(|r| r.len()),
                    "until_ms": value["temporary"]["until_ms"]}),
            )?;
            Ok(value)
        }
        _ => bail!("unknown command {command}; try routes.show or routes.set"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::Usage;
    use std::collections::HashMap;

    const HOUR: i64 = 3_600_000;
    // 2026-10-17 19:50 UTC, a Saturday.
    const NOW: i64 = 1_792_266_600_000;

    fn route(model: &str, extra: Value) -> Value {
        let mut route = json!({"manifest":"bundled:codex","options":{"model":model},"cost":"plan"});
        for (key, value) in extra.as_object().unwrap() {
            route[key] = value.clone();
        }
        route
    }

    fn daily_in(hours: i64, now: i64) -> Value {
        let minute = (now + hours * HOUR) / 60_000 % 1440;
        json!({"daily_utc": format!("{:02}:{:02}", minute / 60, minute % 60)})
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn route_states_come_from_stored_holds_and_usage_only() {
        let policy = json!({"routes": {
            "busy": route("busy", json!({})),
            "capped": route("capped", json!({"allowance": {"window_hours": 5}, "tier": 1})),
            "fresh": route("fresh", json!({"allowance": daily_in(3, NOW), "tier": 2})),
            "open": route("open", json!({"allowance": {"window_hours": 5}, "tier": 1})),
            "plain": route("plain", json!({})),
        }});
        let id = |name: &str| identity(&policy["routes"][name]).unwrap();
        let ledger = rusqlite::Connection::open_in_memory().unwrap();
        crate::routes::init(&ledger).unwrap();
        let hold = |route: &str, until: i64, error: &str| {
            ledger
                .execute(
                    "INSERT INTO model_routes VALUES(?1, 1, ?2, ?3, ?4)",
                    rusqlite::params![route, until, error, NOW],
                )
                .unwrap();
        };
        hold(&id("busy"), NOW + 10 * 60_000, "429 rate limited");
        hold(&id("capped"), NOW + 4 * HOUR, "usage limit reached");
        hold("opencode/zen/other", NOW + 2 * HOUR, "free allowance spent");
        let usage = |route: &str, last: i64, window_ends: Option<i64>, capped: Option<i64>| {
            ledger
                .execute(
                    "INSERT INTO route_usage(route, last_succeeded_ms, strikes, window_ends_ms,
                       window_utilization, capped_until_ms, updated_ms)
                     VALUES(?1, ?2, 0, ?3, 0.4, ?4, ?2)",
                    rusqlite::params![route, last, window_ends, capped],
                )
                .unwrap();
        };
        usage(&id("capped"), NOW - HOUR, None, Some(NOW + 4 * HOUR));
        usage(&id("open"), NOW - HOUR, Some(NOW + 3 * HOUR), None);
        // Daily `fresh` was last used before its last reset, a day ago: idle.
        usage(&id("fresh"), NOW - 22 * HOUR, None, None);
        type Read = Vec<(Option<String>, State, Option<i64>, Option<String>)>;
        let read = |now: i64| -> Read {
            states(&policy, &ledger, now)
                .unwrap()
                .into_iter()
                .map(|s| (s.name, s.state, s.until_ms, s.error))
                .collect()
        };
        let name = |n: &str| Some(n.to_owned());
        let at_now = read(NOW);
        assert_eq!(
            at_now,
            vec![
                (
                    name("busy"),
                    State::BackingOff,
                    Some(NOW + 10 * 60_000),
                    name("429 rate limited")
                ),
                (
                    name("capped"),
                    State::Capped,
                    Some(NOW + 4 * HOUR),
                    name("usage limit reached")
                ),
                (name("fresh"), State::Idle, None, None),
                (name("open"), State::Available, None, None),
                (name("plain"), State::Available, None, None),
                (
                    None,
                    State::BackingOff,
                    Some(NOW + 2 * HOUR),
                    name("free allowance spent")
                ),
            ]
        );
        let unnamed = states(&policy, &ledger, NOW).unwrap().pop().unwrap();
        assert_eq!(unnamed.route.as_deref(), Some("opencode/zen/other"));
        // Time passing without crossing a stored instant changes nothing.
        assert_eq!(read(NOW + 5 * 60_000), at_now);
        // Crossing one does: the backoff ends, the window closes and the
        // rolling allowance goes idle, the daily one renews and stays idle.
        let later = read(NOW + 3 * HOUR + 60_000);
        assert_eq!(later[0].1, State::Available);
        assert_eq!(later[1].1, State::Capped);
        assert_eq!(later[3].1, State::Idle);
        assert_eq!(later.len(), 5, "the unnamed hold ended");
    }

    #[test]
    fn two_daily_allowances_both_drain_across_a_refresh() {
        let policy = json!({"routes": {
            "a": route("a", json!({"allowance": daily_in(2, NOW), "tier": 1})),
            "b": route("b", json!({"allowance": daily_in(20, NOW), "tier": 1})),
            "plan": route("plan", json!({})),
        }});
        let list = names(&["plan", "b", "a"]);
        let usage = HashMap::new();
        // A loses its headroom first, so it goes first, then B, then carry-over.
        assert_eq!(
            rank(&list, &policy, &usage, NOW),
            names(&["a", "b", "plan"])
        );
        // Past A's refresh, A's next loss is a day away and B's is nearer.
        let later = NOW + 3 * HOUR;
        assert_eq!(
            rank(&list, &policy, &usage, later),
            names(&["b", "a", "plan"])
        );
    }

    #[test]
    fn tier_filters_and_never_orders_and_idle_windows_wait_behind_deadlines() {
        let policy = json!({"routes": {
            "weak": route("weak", json!({"allowance": daily_in(1, NOW), "tier": 2})),
            "steady": route("steady", json!({})),
            "idle": route("idle", json!({"allowance": {"window_hours": 5}, "tier": 1})),
            "open": route("open", json!({"allowance": {"window_hours": 5}, "tier": 1})),
            "daily": route("daily", json!({"allowance": daily_in(8, NOW), "tier": 1})),
        }});
        let mut usage = HashMap::new();
        usage.insert(
            "codex/-/open".to_owned(),
            Usage {
                window_ends_ms: Some(NOW + 2 * HOUR),
                ..Usage::default()
            },
        );
        // A tier-2 route with a deadline goes first: tier is a floor, not a sort.
        let list = names(&["steady", "weak", "idle", "daily", "open"]);
        assert_eq!(
            rank(&list, &policy, &usage, NOW),
            names(&["weak", "open", "daily", "idle", "steady"])
        );
        // A list with no shapes keeps the captain's order.
        let plain = names(&["steady"]);
        assert_eq!(rank(&plain, &policy, &usage, NOW), plain);
    }

    #[test]
    fn idle_rolling_windows_rank_by_their_weekly_reset() {
        let policy = json!({"routes": {
            "late": route("late", json!({"allowance": {"window_hours": 5, "weekly_utc": "Fri 00:00"}, "tier": 1})),
            "soon": route("soon", json!({"allowance": {"window_hours": 5, "weekly_utc": "Mon 00:00"}, "tier": 1})),
            "none": route("none", json!({"allowance": {"window_hours": 5}, "tier": 1})),
        }});
        let list = names(&["none", "late", "soon"]);
        assert_eq!(
            rank(&list, &policy, &HashMap::new(), NOW),
            names(&["soon", "late", "none"])
        );
    }

    #[test]
    fn shapes_and_reset_times_parse() {
        assert_eq!(
            shape(&json!({"daily_utc":"07:00"})).unwrap(),
            Shape::Daily(420)
        );
        assert_eq!(
            shape(&json!({"window_hours":5,"weekly_utc":"Tue 01:00"})).unwrap(),
            Shape::Rolling {
                window_ms: 5 * HOUR,
                weekly: Some(1440 + 60)
            }
        );
        for bad in [
            json!({}),
            json!({"daily_utc":"07:00","window_hours":5}),
            json!({"daily_utc":"07:00","weekly_utc":"Mon 00:00"}),
            json!({"window_hours":0}),
            json!({"window_hours":5,"weekly_utc":"Monday 00:00"}),
            json!({"window_hours":5,"pool":"go"}),
        ] {
            assert!(shape(&bad).is_err(), "{bad}");
        }
        // Monday 00:00 UTC after Saturday 2026-10-17 is 2026-10-19.
        assert_eq!(
            crate::routes::utc_time(next_weekly(0, NOW)),
            "2026-10-19 00:00 UTC"
        );
        assert_eq!(
            crate::routes::utc_time(next_daily(420, NOW)),
            "2026-10-18 07:00 UTC"
        );
        assert_eq!(span(26 * HOUR), "26h");
        assert_eq!(span(80 * 60_000), "1h20m");
    }

    #[test]
    fn parse_require_treats_null_fields_as_missing() {
        let req_tier = parse_require(Some(&json!({"tier": 1, "use": null})))
            .unwrap()
            .unwrap();
        assert_eq!(
            req_tier,
            Require {
                r#use: None,
                tier: Some(1),
            }
        );
        assert_eq!(req_tier.to_json(), json!({"tier": 1}));

        let req_use = parse_require(Some(&json!({"tier": null, "use": "review"})))
            .unwrap()
            .unwrap();
        assert_eq!(
            req_use,
            Require {
                r#use: Some("review".to_string()),
                tier: None,
            }
        );
        assert_eq!(req_use.to_json(), json!({"use": "review"}));

        let err = parse_require(Some(&json!({"tier": null, "use": null}))).unwrap_err();
        assert!(
            err.to_string()
                .contains("require must state at least 'use' or 'tier'")
        );
    }

    #[test]
    fn a_first_mate_onto_a_listed_route_keeps_its_harness_settings_only_on_its_harness() {
        let choice = |manifest: &str, options: Value| Choice {
            name: "flash".into(),
            manifest: json!(manifest),
            options,
            identity: None,
            tier: None,
            uses: vec![],
            why: String::new(),
        };
        let profile = json!({"manifest": "bundled:codex", "duty_task": "d",
            "options": {"sandbox": "workspace-write", "provider": "old"},
            "tool_bundles": [], "config": "/c.json"});
        let mut same = profile.clone();
        onto(
            &mut same,
            &choice("bundled:codex", json!({"model": "flash-model"})),
        );
        assert_eq!(
            same["options"],
            json!({"sandbox": "workspace-write", "model": "flash-model"})
        );
        assert_eq!(same["tool_bundles"], json!([]));
        assert_eq!(same["config"], "/c.json");
        let mut other = profile;
        onto(
            &mut other,
            &choice("bundled:pi", json!({"provider": "p", "model": "m"})),
        );
        assert_eq!(other["manifest"], "bundled:pi");
        assert_eq!(other["options"], json!({"provider": "p", "model": "m"}));
        assert!(other.get("tool_bundles").is_none() && other.get("config").is_none());
        assert_eq!(other["duty_task"], "d");
    }

    #[test]
    fn a_route_keeps_four_fifths_for_crew_and_nineteen_twentieths_for_a_first_mate() {
        let plain = route("plain", json!({}));
        assert_eq!(limits(&plain), Limits::default());
        let default = Limits::default();
        assert_eq!(default.crew, 0.8);
        assert_eq!(default.first_mate, 0.95);
        assert_eq!(default.runs, None, "no runtime can guess a run budget");
        let mut own = route(
            "own",
            json!({"crew_headroom_pct": 60,
            "first_mate_headroom_pct": 90,
            "allowance": {"window_hours": 5, "runs": 12}, "tier": 1}),
        );
        assert_eq!(limits(&own).crew, 0.6);
        assert_eq!(limits(&own).first_mate, 0.9);
        assert_eq!(limits(&own).runs, Some(12));
        // A run budget needs no share settings, and shares alone no budget.
        own["allowance"] = json!({"window_hours": 5});
        assert_eq!(limits(&own).runs, None);
        assert_eq!(runs(&json!({"daily_utc": "07:00"})).unwrap(), None);
        for bad in [
            json!({"runs": 0}),
            json!({"runs": -1}),
            json!({"runs": "20"}),
        ] {
            assert!(
                runs(&json!({"window_hours": 5, "runs": bad["runs"]})).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn review_candidates_without_overlap_go_first_and_exclusions_name_jobs() {
        let policy = json!({"routes": {
            "gem": route("gemini-3.8-flash-high", json!({
                "tier": 1, "uses": ["review"], "allowance": {"window_hours": 5}})),
            "sol": route("gpt-6.1-sol", json!({"tier": 1, "uses": ["review"]})),
            "opus": route("claude-opus-5-5", json!({"tier": 1, "uses": ["review"]})),
        }});
        let mut authors = crate::review::authors::Authors::default();
        authors.crew_jobs.insert(
            "gemini-3.8-flash-high".to_owned(),
            vec!["job-gem".to_owned()],
        );
        authors
            .first_mate_models
            .insert("claude-opus-5-5".to_owned());
        // `rank` puts the route with an open window first, and the two
        // carry-over routes keep the captain's order; the overlap rule then
        // puts the clean candidate ahead of the overlapping one whatever the
        // allowance order says, and drops the crew model with its jobs named.
        let usage = HashMap::from([(
            "codex/-/gemini-3.8-flash-high".to_owned(),
            Usage {
                window_ends_ms: Some(NOW + 2 * HOUR),
                ..Usage::default()
            },
        )]);
        let ranked = rank(&names(&["opus", "sol", "gem"]), &policy, &usage, NOW);
        assert_eq!(ranked, names(&["gem", "opus", "sol"]));
        let ordered = order_reviewers(&ranked, &policy, &authors);
        assert_eq!(
            ordered.offered,
            vec![
                ("sol".to_owned(), None),
                ("opus".to_owned(), Some("claude-opus-5-5".to_owned())),
            ]
        );
        assert_eq!(
            ordered.excluded,
            vec![(
                "gem".to_owned(),
                "crew job(s) job-gem ran on its model gemini-3.8-flash-high".to_owned()
            )]
        );
        // A candidate whose own model cannot be named is excluded too, since
        // it cannot be shown clear of the authors.
        let mut aliased = policy.clone();
        aliased["routes"]["sonnet"] = route("sonnet", json!({"tier": 1, "uses": ["review"]}));
        let ordered = order_reviewers(&names(&["sonnet"]), &aliased, &authors);
        assert!(ordered.offered.is_empty());
        assert_eq!(ordered.excluded.len(), 1);
        assert!(ordered.excluded[0].1.contains("cannot be named"));
    }

    #[test]
    fn a_declared_jobs_alias_excludes_every_model_that_carries_it() {
        let policy = json!({"routes": {
            "sol": route("gpt-6.1-sol", json!({"tier": 1, "uses": ["review"]})),
            "opus": route("claude-opus-5-5", json!({"tier": 1, "uses": ["review"]})),
        }});
        // The job ran on the bare alias opus and the ship declared it
        // claude-opus-4-7: the fit held, but it proved no version, so the
        // alias is kept beside the declared model and excludes every model
        // that carries it, whatever its version and whatever else it is.
        let mut authors = crate::review::authors::Authors::default();
        authors
            .crew_jobs
            .insert("claude-opus-4-7".to_owned(), vec!["job-alias".to_owned()]);
        authors
            .crew_aliases
            .insert("opus".to_owned(), vec!["job-alias".to_owned()]);
        authors
            .first_mate_models
            .insert("claude-opus-5-5".to_owned());
        let ordered = order_reviewers(&names(&["opus", "sol"]), &policy, &authors);
        assert_eq!(ordered.offered, vec![("sol".to_owned(), None)]);
        assert_eq!(ordered.excluded.len(), 1);
        let (name, why) = &ordered.excluded[0];
        assert_eq!(name, "opus");
        assert!(why.contains("job-alias"), "{why}");
        assert!(why.contains("alias opus"), "{why}");
        assert!(why.contains("claude-opus-5-5"), "{why}");
    }

    fn temporary(value: Value, until_ms: i64, previous: Value) -> Value {
        let mut value = value;
        value["temporary"] =
            json!({"id": "t", "set_ms": 0, "until_ms": until_ms, "previous": previous});
        value
    }

    #[test]
    fn ended_temporary_changes_give_way_to_what_each_replaced() {
        let original = json!({"crew": ["c", "a", "b"]});
        let first = temporary(json!({"crew": ["b"]}), NOW + HOUR, original.clone());
        let second = temporary(json!({"crew": ["a"]}), NOW, first.clone());
        assert_eq!(
            in_force(Some(second.clone()), NOW - 1),
            Some(second.clone())
        );
        assert_eq!(in_force(Some(second.clone()), NOW), Some(first));
        assert_eq!(in_force(Some(second), NOW + HOUR), Some(original));
        let from_nothing = temporary(json!({"crew": ["a"]}), NOW, Value::Null);
        assert_eq!(in_force(Some(from_nothing), NOW), None);
    }

    #[test]
    fn a_revert_concerns_ships_whose_own_lists_change_or_every_ship() {
        let base = json!({"crew": ["a"], "ships": {"x": {"crew": ["a"]}, "y": {"crew": ["b"]}}});
        let mut own = base.clone();
        own["ships"]["y"]["crew"] = json!(["a"]);
        own["ships"]["z"] = json!({"errand": ["a"]});
        assert_eq!(concerned(Some(&own), Some(&base)), Some(names(&["y", "z"])));
        let mut fleet = base.clone();
        fleet["crew"] = json!(["b"]);
        assert_eq!(concerned(Some(&fleet), Some(&base)), None);
        assert_eq!(concerned(Some(&base), None), None);
    }
}
