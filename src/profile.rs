//! `ship.profile`: one ship's configuration as the captain's portal shows it,
//! read-only, so the portal never reads ship folders.
//!
//! Every part is what the runtime itself uses: the first-mate record, the
//! fleet route lists and which of them applies, the charter, the fleet tool
//! permissions, the tool bundle plan the launch path computes, and the
//! attended plugins `privateer start` loads for the harness. No secret value
//! is returned: options whose names look secret read `"<set>"`, MCP servers
//! carry credential names only and URLs keep only scheme, host and port, and no environment
//! value is read.
use crate::store::{Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::io::Read;
use std::path::Path;

/// What a set option whose name looks secret reads as.
const SET: &str = "<set>";
const MAX_SKILL_BYTES: u64 = 65_536;

/// Options with every value under a secret-looking name replaced, at any depth.
fn masked(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    // Everything under a secret-looking name is hidden, nested
                    // maps included: their keys need not look secret themselves.
                    let shown = if crate::block_ask::sensitive_name(key) {
                        json!(SET)
                    } else {
                        masked(value)
                    };
                    (key.clone(), shown)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(masked).collect()),
        other => other.clone(),
    }
}

/// A hosted first mate sleeps after `sleep_after_secs` idle (0: never);
/// a ship that is not hosted is woken per turn and has no pane to sleep.
fn sleep(first_mate: &Value, hosted: bool) -> Value {
    if !hosted {
        return json!({"policy": "not_hosted", "idle_minutes": null});
    }
    let secs = first_mate["sleep_after_secs"]
        .as_u64()
        .unwrap_or(crate::host::sleep::DEFAULT_AFTER_SECS);
    if secs == 0 {
        return json!({"policy": "never", "idle_minutes": null});
    }
    let minutes = if secs % 60 == 0 {
        json!(secs / 60)
    } else {
        json!(secs as f64 / 60.0)
    };
    json!({"policy": "after_idle", "idle_minutes": minutes})
}

/// Each kind's list in the order launches try it, whose list it is, and the
/// cost, tier and uses of each route listed.
fn routes(ship: &str) -> Result<Value> {
    let policy = crate::crew_routes::load()?;
    let mut out = Map::new();
    let mut facts = Map::new();
    for kind in crate::crew_routes::KINDS {
        let (list, names) = match &policy {
            Some(policy) => (
                crate::crew_routes::source(policy, ship, kind),
                crate::crew_routes::order(policy, ship, kind),
            ),
            None => ("none", vec![]),
        };
        if let Some(policy) = &policy {
            for name in &names {
                facts.insert(
                    name.clone(),
                    crate::crew_routes::facts(&policy["routes"][name.as_str()]),
                );
            }
        }
        out.insert(kind.to_owned(), json!({"list": list, "names": names}));
    }
    out.insert("routes".to_owned(), Value::Object(facts));
    Ok(Value::Object(out))
}

/// A skill's description, from its `SKILL.md` frontmatter (a plain line or a
/// JSON-quoted string, as bundle validation accepts).
fn skill_description(root: &str) -> Option<String> {
    let mut text = String::new();
    std::fs::File::open(Path::new(root).join("SKILL.md"))
        .ok()?
        .take(MAX_SKILL_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let front = text.strip_prefix("---\n")?.split("\n---").next()?;
    let raw = front
        .lines()
        .find_map(|line| line.strip_prefix("description:"))?
        .trim();
    if raw.starts_with('"') {
        serde_json::from_str(raw).ok()
    } else {
        Some(raw.to_owned())
    }
}

/// Only a URL's scheme, host and port: which server it is. Some services put
/// the secret in the path, the query or the fragment.
fn bare_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    // The adapter refuses userinfo; drop it anyway.
    let host = host.rsplit('@').next().unwrap_or(host);
    if scheme.is_empty() {
        host.to_owned()
    } else {
        format!("{scheme}://{host}")
    }
}

/// The skills and MCP servers a first mate's bounded launch plan selects.
/// Without a selection the native harness loads its ambient resources, which
/// the runtime does not list.
fn bundles(store: &Store, first_mate: &Value, harness: &str) -> Value {
    let Some(selected) = first_mate.get("tool_bundles") else {
        return json!({"bounded": false, "selected": null, "skills": [], "mcp_servers": []});
    };
    let plan = match crate::tool_bundles::resolve(
        store,
        selected,
        "first_mate",
        harness,
        crate::plugins::Wait::Declared,
    ) {
        Ok(plan) => plan,
        // The adapter's refusal names a bundle and field, never a value.
        Err(error) => {
            return json!({"bounded": true, "selected": selected, "skills": [],
                "mcp_servers": [], "error": format!("{error:#}")});
        }
    };
    let skills: Vec<Value> = plan["skills"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|skill| {
            let description = skill["root"].as_str().and_then(skill_description);
            json!({"name": skill["name"], "description": description})
        })
        .collect();
    let servers: Vec<Value> = plan["mcp_servers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|server| {
            let credentials: Vec<&Value> = server.get("credential").into_iter().collect();
            json!({"name": server["name"], "url": server["url"].as_str().map(bare_url),
                "description": server.get("description"), "credentials": credentials})
        })
        .collect();
    json!({"bounded": true, "selected": selected, "skills": skills, "mcp_servers": servers})
}

/// The attended plugins `privateer start` loads for this harness.
fn plugins(harness: &str) -> Vec<Value> {
    let pi = |file: &str, description: &str| json!({"name": file, "description": description});
    match harness {
        "claude" => {
            let manifest = crate::cli::plugins_dir()
                .ok()
                .and_then(|dir| {
                    std::fs::read(dir.join("attended-claude/.claude-plugin/plugin.json")).ok()
                })
                .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
                .unwrap_or(Value::Null);
            vec![json!({
                "name": manifest["name"].as_str().unwrap_or("privateer-mail"),
                "description": manifest["description"]
                    .as_str()
                    .unwrap_or("Bounded attended mailbox delivery for a Privateer ship."),
            })]
        }
        "pi" | "pi-signed" | "omp" => {
            let mut list = vec![
                pi(
                    "attended/mail.ts",
                    "Privateer's attended mail adapter: the ship's mail in the live session.",
                ),
                pi(
                    "attended/captain.ts",
                    "The captain's fleet commands, such as /stow, in the live session.",
                ),
            ];
            if harness != "omp" {
                list.push(pi(
                    "attended/calm.ts",
                    "The /calm conversation view for Pi sessions.",
                ));
            }
            list
        }
        _ => vec![],
    }
}

pub fn profile(args: &Value) -> Result<Value> {
    let name = required(args, "ship")?;
    identifier(name)?;
    let Some((_, home)) = crate::captain::fleet()?
        .into_iter()
        .find(|(ship, _)| ship == name)
    else {
        if let Some((_, machine, _)) = crate::machines::remote_ships()?
            .into_iter()
            .find(|(ship, _, _)| ship == name)
        {
            bail!("ship {name} is on machine {machine}; ask there");
        }
        bail!("unknown ship {name}");
    };
    let store = Store::open(&home)?;
    let record = crate::mail::ship(&store, name)?
        .with_context(|| format!("ship {name} has no record in its home"))?;
    ensure!(
        record["location"] == "here",
        "ship {name} is not hosted by its fleet home"
    );
    let first_mate = &record["first_mate"];
    let harness = first_mate["manifest"]
        .as_str()
        .and_then(crate::bundled::harness);
    let hosted = crate::host::hosted(first_mate);
    let charter = match store.charter(name) {
        Ok(charter) => json!({"actions": charter["actions"], "active": charter["active"]}),
        Err(_) => json!({"actions": [], "active": false}),
    };
    let permissions = crate::permissions::load()?;
    let bundles = bundles(&store, first_mate, harness.as_deref().unwrap_or(""));
    Ok(json!({
        "ship": name,
        "machine": crate::machines::this_machine(),
        "hosted": hosted,
        "sleep": sleep(first_mate, hosted),
        "first_mate": {
            "harness": harness,
            "options": masked(first_mate.get("options").unwrap_or(&json!({}))),
        },
        "routes": routes(name)?,
        "charter": charter,
        "permissions": {
            "fleet_mode": permissions["mode"],
            "fleet_rules": permissions["claude_allow"],
            "ship_rules": [],
        },
        "tool_bundles": {
            "bounded": bundles["bounded"],
            "selected": bundles["selected"],
            "error": bundles.get("error"),
        },
        "skills": bundles["skills"],
        "plugins": plugins(harness.as_deref().unwrap_or("")),
        "mcp_servers": bundles["mcp_servers"],
    }))
}
