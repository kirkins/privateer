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
//!
//! A route may also declare how much of its plan to keep in reserve. Only
//! Claude reports what it has spent, so a route on any other plan is measured
//! by the runs Privateer counts for it: `allowance.runs` is the number of runs
//! the window holds, with no default, because no runtime can guess one.
//! `crew_headroom_pct` and `first_mate_headroom_pct` (both whole percents, 80
//! and 95 by default) are the shares at which the route stops taking new
//! crew and errands, and at which a first mate moves to its fallback. Running
//! work is never stopped: it is what spends the reserve.
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

pub(crate) const KINDS: [&str; 3] = ["crew", "errand", "first_mate"];
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

/// Replace the whole routes file. Captain only. The reply carries `warnings`
/// for what the runtime cannot refuse: notes still prefixed with a STRENGTH
/// label, and errand routes with no `review` use.
pub fn set(caller: &Caller, value: &Value) -> Result<Value> {
    caller.require_operator("routes.set")?;
    validate(value)?;
    let warnings = warnings(value);
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
    let mut reply = value.clone();
    reply["warnings"] = json!(warnings);
    Ok(reply)
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
    let mut errand: Vec<String> = names_of(&value["errand"]);
    if let Some(ships) = value["ships"].as_object() {
        for lists in ships.values() {
            errand.extend(names_of(&lists["errand"]));
        }
    }
    for name in errand {
        let route = &value["routes"][name.as_str()];
        if !uses(route).iter().any(|word| word == "review") {
            warnings.push(format!(
                "route {name} is on an errand list but has no review use"
            ));
        }
    }
    warnings.sort();
    warnings.dedup();
    warnings
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

/// A route's cost, tier and uses as `ship.profile` reports them. A route with
/// no tier reports a null tier.
pub(crate) fn facts(route: &Value) -> Value {
    json!({
        "cost": route["cost"],
        "tier": route.get("tier").map(|_| tier(route)),
        "uses": uses(route),
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

fn identity(route: &Value) -> Option<String> {
    crate::routes::route_of(route["manifest"].as_str()?, &route["options"])
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
pub(crate) struct Choice {
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
    fallback(store, ship, task, profile, false)
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
    for name in order(&policy, ship, "first_mate") {
        let route = &policy["routes"][name.as_str()];
        let Some(route_id) = identity(route) else {
            continue;
        };
        if route_id == own_route || failed_route == Some(route_id.as_str()) {
            continue;
        }
        if !crate::routes::held_off(&ledger, &route_id, Who::FirstMate, &limits(route), now)? {
            return Ok(Some(Choice {
                name: name.clone(),
                manifest: route["manifest"].clone(),
                options: route.get("options").cloned().unwrap_or(json!({})),
                identity: Some(route_id),
                tier: route["tier"].as_u64(),
                uses: uses(route),
                why: format!(
                    "first available on {ship}'s first_mate list: {name} (no requirement)"
                ),
            }));
        }
    }
    Ok(None)
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
    for name in candidates {
        let route = &policy["routes"][name.as_str()];
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
    bail!(
        "every {kind} route for {ship} is unavailable:\n{}",
        refusals.join("\n")
    )
}

/// Refuse an agent's explicit harness/model unless the captain lists it for
/// this ship and it meets any requirement. Operators may launch anything.
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
/// restricts nothing), and it is not backing off. A resume is a runtime
/// launch, so it is checked here as an agent start is in `check`.
pub fn resume_route(
    store: &Store,
    ship: &str,
    manifest: &str,
    options: &Value,
) -> Result<bool> {
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
    let ledger = crate::jobs::host_ledger(store)?;
    let now = Store::now_ms();
    Ok(identity(route)
        .map(|id| crate::routes::blocked_at(&ledger, &id, now))
        .transpose()?
        .flatten()
        .is_none())
}

/// The routes file with, for each ship that has lists of its own, which list
/// applies to each kind: the ship's own, or the fleet's.
fn show(value: Option<Value>) -> Value {
    let Some(mut value) = value else {
        return Value::Null;
    };
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
            if let Some(account_used) = usage.get(&account) {
                let account_is_newer = used.as_ref().is_none_or(|u| {
                    account_used.updated_ms > u.updated_ms
                        || (u.window_utilization.is_none() && u.weekly_utilization.is_none())
                });
                if account_is_newer
                    && (account_used.window_utilization.is_some()
                        || account_used.weekly_utilization.is_some())
                {
                    if let Some(u) = &mut used {
                        u.window_ends_ms = account_used.window_ends_ms;
                        u.window_utilization = account_used.window_utilization;
                        u.weekly_ends_ms = account_used.weekly_ends_ms;
                        u.weekly_utilization = account_used.weekly_utilization;
                        u.updated_ms = u.updated_ms.max(account_used.updated_ms);
                    } else {
                        let mut account_u = account_used.clone();
                        account_u.runs = 0;
                        account_u.runs_window_ends_ms = None;
                        used = Some(account_u);
                    }
                }
            }
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
    let mut parts = vec![];
    let mut shaped = false;
    let mut idle: Vec<String> = vec![];
    for kind in KINDS {
        let names = order(&policy, ship, kind);
        if names.is_empty() {
            continue;
        }
        let mut shown = vec![];
        for name in names {
            let route = &policy["routes"][name.as_str()];
            let cost = route["cost"].as_str().unwrap_or("?");
            let read = states.get(&name);
            let used = read.and_then(|r| r.usage.as_ref());
            let mut details = vec![cost.to_owned()];
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
        let list = match source(&policy, ship, kind) {
            "ship" => " (ship's own list)",
            _ => " (fleet list)",
        };
        parts.push(format!("{kind}: {}{list}", shown.join(", ")));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    let rule = if shaped {
        "use-it-or-lose-it allowances nearest their refresh go first"
    } else {
        "the first available is used"
    };
    let mut text = format!(
        "Routes ({}; {rule}, or pass \"route\":\"<name>\").",
        parts.join("; ")
    );
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
        _ => bail!("usage: privateer routes [show|set FILE.json]"),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "routes.show" => Ok(show(load()?)),
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
}
