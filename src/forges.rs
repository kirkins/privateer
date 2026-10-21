//! The captain's forges: `forges.json`, fleet-wide beside the routes file,
//! naming for each repository the review gate covers its forge kind, API,
//! protected branch, required checks, and the reviewer and merger accounts
//! with the credential refs that hold their tokens. It holds no secrets.
//! Only the captain writes it (`forges.set`) or reads it whole
//! (`forges.show`); the runtime reads it for `review.start`, posting and
//! `pr.merge`, and a repository missing from it is not reviewed or merged.
//!
//! Every forge call goes through `plugins/forge.py` (argv `<op>`, request on
//! stdin, JSON reply on stdout), with the token's file path, never its value,
//! in `PRIVATEER_FORGE_TOKEN_FILE` on that child alone. A debug build (the
//! test suites) may name a fake adapter in `PRIVATEER_FORGE_ADAPTER`; a
//! release build ignores that variable. That does not pin the adapter on
//! its own: outside a release image, `PRIVATEER_PLUGINS` still chooses the
//! folder `forge.py` is read from and `PATH` the `python3` that runs it, so
//! whoever sets the runtime's environment can still swap the adapter. Only
//! a release image, which fixes both, closes that. A credential ref named
//! here is never given to a launch: `bundle.configure` and `bundles.py`
//! refuse it.
use crate::store::Store;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const MAX_BYTES: usize = 64 * 1024;
const MAX_REPOS: usize = 64;
const MAX_CHECKS: usize = 32;

/// One account on a forge and the credential ref holding its token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub account: String,
    pub credential: String,
}

/// One repository's entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    /// `host/owner/name`, as forges.json keys it.
    pub key: String,
    /// `owner/name`, as the forge's API names it.
    pub repo: String,
    /// `github` or `forgejo`.
    pub forge: String,
    pub api: String,
    pub branch: String,
    /// The checks a merge needs green. Empty means branch protection's.
    pub required_checks: Vec<String>,
    pub reviewer: Account,
    pub merger: Account,
}

fn path() -> Result<PathBuf> {
    let path = crate::fleet::registry_path()?.with_extension("forges.json");
    ensure!(path.is_absolute(), "fleet forges path must be absolute");
    Ok(path)
}

fn valid_ref(value: &str) -> bool {
    value.len() <= 40
        && value.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn api_ok(api: &str) -> bool {
    let Some((scheme, rest)) = api.split_once("://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
    let loopback = matches!(name, "127.0.0.1" | "localhost" | "[::1]");
    (scheme == "https" || (scheme == "http" && loopback))
        && !host.is_empty()
        && !host.contains('@')
        && !rest.contains(['?', '#'])
        && !api.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn account(value: &Value, at: &str) -> Result<Account> {
    let object = value
        .as_object()
        .with_context(|| format!("{at} must be {{account, credential}}"))?;
    for key in object.keys() {
        ensure!(
            matches!(key.as_str(), "account" | "credential"),
            "unknown {at} setting {key}"
        );
    }
    let name = object
        .get("account")
        .and_then(Value::as_str)
        .filter(|a| segment(a) && a.len() <= 64)
        .with_context(|| format!("{at}.account must be the forge login"))?;
    let credential = object
        .get("credential")
        .and_then(Value::as_str)
        .filter(|c| valid_ref(c))
        .with_context(|| {
            format!("{at}.credential must be a credential ref (lowercase words joined by hyphens)")
        })?;
    Ok(Account {
        account: name.to_owned(),
        credential: credential.to_owned(),
    })
}

fn entry(key: &str, value: &Value) -> Result<Repo> {
    let at = format!("repos.{key}");
    let parts: Vec<&str> = key.split('/').collect();
    ensure!(
        parts.len() == 3 && parts.iter().all(|p| segment(p)) && key == key.to_ascii_lowercase(),
        "{at}: a repository is keyed host/owner/name, in lowercase"
    );
    let object = value
        .as_object()
        .with_context(|| format!("{at} must be an object"))?;
    for name in object.keys() {
        ensure!(
            matches!(
                name.as_str(),
                "forge" | "api" | "branch" | "required_checks" | "reviewer" | "merger" | "note"
            ),
            "unknown {at} setting {name}"
        );
    }
    let forge = object
        .get("forge")
        .and_then(Value::as_str)
        .filter(|f| matches!(*f, "github" | "forgejo"))
        .with_context(|| format!("{at}.forge must be github or forgejo"))?;
    let api = object
        .get("api")
        .and_then(Value::as_str)
        .filter(|a| api_ok(a) && a.len() <= 200)
        .with_context(|| {
            format!("{at}.api must be an https URL with no credentials, query or fragment")
        })?;
    let branch = object
        .get("branch")
        .and_then(Value::as_str)
        .filter(|b| {
            !b.is_empty()
                && b.len() <= 200
                && !b.starts_with('-')
                && !b.chars().any(|c| c.is_whitespace() || c.is_control())
        })
        .with_context(|| format!("{at}.branch must name the protected branch"))?;
    let required_checks = match object.get("required_checks") {
        None | Some(Value::Null) => vec![],
        Some(list) => {
            let list = list
                .as_array()
                .with_context(|| format!("{at}.required_checks must be a list"))?;
            ensure!(
                list.len() <= MAX_CHECKS,
                "{at}.required_checks lists at most {MAX_CHECKS}"
            );
            let mut checks = vec![];
            for check in list {
                let check = check
                    .as_str()
                    .filter(|c| !c.trim().is_empty() && c.len() <= 200)
                    .with_context(|| format!("{at}.required_checks holds check names"))?;
                ensure!(
                    !checks.iter().any(|c| c == check),
                    "{at}.required_checks lists {check} twice"
                );
                checks.push(check.to_owned());
            }
            checks
        }
    };
    if let Some(note) = object.get("note") {
        ensure!(
            note.as_str().is_some_and(|n| n.len() <= 500),
            "{at}.note must be text"
        );
    }
    let reviewer = account(
        object
            .get("reviewer")
            .with_context(|| format!("{at}.reviewer is required"))?,
        &format!("{at}.reviewer"),
    )?;
    let merger = account(
        object
            .get("merger")
            .with_context(|| format!("{at}.merger is required: the account pr.merge merges as"))?,
        &format!("{at}.merger"),
    )?;
    ensure!(
        !reviewer.account.eq_ignore_ascii_case(&merger.account)
            && reviewer.credential != merger.credential,
        "{at}: the reviewer and the merger are different accounts with different credentials"
    );
    Ok(Repo {
        key: key.to_owned(),
        repo: format!("{}/{}", parts[1], parts[2]),
        forge: forge.to_owned(),
        api: api.trim_end_matches('/').to_owned(),
        branch: branch.to_owned(),
        required_checks,
        reviewer,
        merger,
    })
}

fn validate(value: &Value) -> Result<Vec<Repo>> {
    let object = value.as_object().context("forges must be an object")?;
    for key in object.keys() {
        ensure!(key == "repos", "unknown forges setting {key}");
    }
    let repos = object
        .get("repos")
        .and_then(Value::as_object)
        .context("forges.repos must be an object keyed host/owner/name")?;
    ensure!(repos.len() <= MAX_REPOS, "at most {MAX_REPOS} repositories");
    repos.iter().map(|(key, value)| entry(key, value)).collect()
}

fn read() -> Result<Option<Value>> {
    match fs::File::open(path()?) {
        Ok(file) => {
            let mut raw = Vec::new();
            file.take(MAX_BYTES as u64 + 1).read_to_end(&mut raw)?;
            ensure!(raw.len() <= MAX_BYTES, "fleet forges file too large");
            Ok(Some(
                serde_json::from_slice(&raw).context("invalid fleet forges file")?,
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Every repository in forges.json; none when there is no file.
pub fn load() -> Result<Vec<Repo>> {
    match read()? {
        Some(value) => validate(&value),
        None => Ok(vec![]),
    }
}

/// Every credential ref forges.json names. A file that cannot be read or
/// parsed is an error, so a launch that checks against it fails closed.
pub(crate) fn credential_refs() -> Result<BTreeSet<String>> {
    Ok(load()?
        .into_iter()
        .flat_map(|r| [r.reviewer.credential, r.merger.credential])
        .collect())
}

/// The forges.json entry for a clone's origin: by `host/owner/name`, or,
/// for an origin with no host (a local path), the one entry for
/// `owner/name`.
pub fn for_origin(host: Option<&str>, repo: &str) -> Result<Repo> {
    let repos = load()?;
    let repo_lower = repo.to_ascii_lowercase();
    let found: Vec<&Repo> = match host {
        Some(host) => {
            let key = format!("{}/{repo_lower}", host.to_ascii_lowercase());
            repos.iter().filter(|r| r.key == key).collect()
        }
        None => repos
            .iter()
            .filter(|r| r.repo.to_ascii_lowercase() == repo_lower)
            .collect(),
    };
    match found.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!(
            "{}{repo} is not in the captain's forges.json, so it is not reviewed or merged \
             here; the captain adds it with forges.set",
            host.map(|h| format!("{h}/")).unwrap_or_default()
        ),
        _ => bail!("forges.json has more than one entry for {repo}; name its host"),
    }
}

/// The forges.json entry keyed `host/owner/name`.
pub fn for_key(key: &str) -> Result<Repo> {
    load()?
        .into_iter()
        .find(|r| r.key == key)
        .with_context(|| format!("{key} is no longer in the captain's forges.json"))
}

/// The adapter a forge call runs: `plugins/forge.py`, or in a debug build
/// the fake that `PRIVATEER_FORGE_ADAPTER` names.
fn adapter() -> Result<Command> {
    if cfg!(debug_assertions)
        && let Some(fake) = std::env::var_os("PRIVATEER_FORGE_ADAPTER").filter(|a| !a.is_empty())
    {
        return Ok(Command::new(fake));
    }
    let script = crate::cli::plugins_dir()?.join("forge.py");
    ensure!(
        script.is_file(),
        "the forge adapter {} is missing",
        script.display()
    );
    let mut command = Command::new("python3");
    command.arg("-I").arg(script);
    Ok(command)
}

/// One forge call as `account`: `op` with the request on stdin, the repo's
/// forge, API and `owner/name` added, and the account's token file named in
/// the child's environment alone. The reply, or why it failed.
pub fn call(repo: &Repo, account: &Account, op: &str, request: Value) -> Result<Value> {
    call_until(repo, account, op, request, None)
}

/// `call`, stopped at `until` when one is given.
pub fn call_until(
    repo: &Repo,
    account: &Account,
    op: &str,
    request: Value,
    until: Option<Instant>,
) -> Result<Value> {
    let reply = exchange(repo, account, op, request, until)?;
    ensure!(
        reply["ok"] == true,
        "the forge refused {op} on {}: {}",
        repo.key,
        reply["error"].as_str().unwrap_or("no reason given")
    );
    Ok(reply)
}

/// One forge call's reply as the adapter gave it, `ok` or not; an error
/// only when no reply came (the adapter could not run, ran out of time, or
/// wrote no JSON). A caller that must tell a refusal from a lost reply
/// reads `ok` and `status_code` itself.
pub fn exchange(
    repo: &Repo,
    account: &Account,
    op: &str,
    request: Value,
    until: Option<Instant>,
) -> Result<Value> {
    let token = crate::credentials::file_of(&account.credential)?;
    let mut request = request;
    let fields = request
        .as_object_mut()
        .context("a forge request is an object")?;
    fields.insert("forge".into(), json!(repo.forge));
    fields.insert("api".into(), json!(repo.api));
    fields.insert("repo".into(), json!(repo.repo));
    let mut command = adapter()?;
    command.arg(op).env_clear();
    for name in ["PATH", "HOME", "LANG", "SSL_CERT_FILE", "SSL_CERT_DIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("PRIVATEER_FORGE_TOKEN_FILE", &token);
    let output =
        crate::review::run_bounded(command, Some(request.to_string().as_bytes()), until)
            .with_context(|| format!("run the forge adapter's {op}"))?;
    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "the forge adapter's {op} reply is not JSON: {}",
            crate::review::clip(String::from_utf8_lossy(&output.stderr).trim(), 300)
        )
    })
}

/// The checks a merge of the repo needs green: forges.json's list, else the
/// ones branch protection requires. Never every check: a check the captain
/// did not require (a flaky platform) never blocks a merge.
pub fn required_checks(repo: &Repo) -> Result<Vec<String>> {
    if !repo.required_checks.is_empty() {
        return Ok(repo.required_checks.clone());
    }
    let protection = protection(repo)?;
    let checks: Vec<String> = protection["status_checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    ensure!(
        !checks.is_empty(),
        "{} names no required checks: list them as required_checks in forges.json, or \
         require them in {}'s branch protection",
        repo.key,
        repo.branch
    );
    Ok(checks)
}

fn protection(repo: &Repo) -> Result<Value> {
    call(
        repo,
        &repo.reviewer,
        "protection.read",
        json!({"branch": repo.branch, "reviewer": repo.reviewer.account,
            "merger": repo.merger.account, "required_checks": repo.required_checks}),
    )
}

/// Refs a bundle registry gives to launches.
pub(crate) fn bundle_refs(registry: &Value) -> BTreeSet<String> {
    registry["bundles"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|b| b["credentials"].as_array().into_iter().flatten())
        .filter_map(|c| c["ref"].as_str())
        .map(str::to_owned)
        .collect()
}

/// `forges.set`: replace the whole file, captain only. Refused when this
/// home's bundle registry gives one of its refs to launches. The reply's
/// `warnings` hold what the runtime cannot refuse: a credential not stored
/// yet, and branch protection read back that misses a setting.
fn set(store: &Store, value: &Value) -> Result<Value> {
    store.caller().require_operator("forges.set")?;
    ensure!(
        serde_json::to_vec(value)?.len() <= MAX_BYTES,
        "forges file too large"
    );
    let repos = validate(value)?;
    if let Some(registry) = crate::tool_bundles::registry(store)? {
        let given = bundle_refs(&registry);
        for repo in &repos {
            for account in [&repo.reviewer, &repo.merger] {
                ensure!(
                    !given.contains(&account.credential),
                    "credential {} is given to launches by this home's bundle registry; a forge \
                     token is never given to a launch, so use another ref",
                    account.credential
                );
            }
        }
    }
    let mut conn = crate::fleet::registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let path = path()?;
    let temp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let written = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        fs::File::open(path.parent().context("forges directory")?)?.sync_all()?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written?;
    tx.commit()?;
    let mut warnings = vec![];
    let mut protection_read = Map::new();
    for repo in &repos {
        let missing: Vec<&str> = [&repo.reviewer, &repo.merger]
            .into_iter()
            .filter(|a| crate::credentials::file_of(&a.credential).is_err())
            .map(|a| a.credential.as_str())
            .collect();
        if !missing.is_empty() {
            warnings.push(format!(
                "{}: credential {} is not stored yet; store it with credential.set",
                repo.key,
                missing.join(", ")
            ));
            if missing.contains(&repo.reviewer.credential.as_str()) {
                continue;
            }
        }
        match protection(repo) {
            Ok(read) => {
                for warning in read["warnings"].as_array().into_iter().flatten() {
                    warnings.push(format!(
                        "{} {}: {}",
                        repo.key,
                        repo.branch,
                        warning.as_str().unwrap_or_default()
                    ));
                }
                protection_read.insert(repo.key.clone(), read);
            }
            Err(error) => warnings.push(format!(
                "{}: branch protection could not be read back: {error:#}",
                repo.key
            )),
        }
    }
    Ok(json!({
        "repos": value["repos"],
        "protection": protection_read,
        "warnings": warnings,
    }))
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "forges.show" => {
            store.caller().require_operator(command)?;
            Ok(read()?.unwrap_or(Value::Null))
        }
        "forges.set" => {
            let reply = set(store, &args)?;
            store.audit(
                "forges.set",
                &json!({"repos": reply["repos"].as_object().map(|r| r.keys().cloned().collect::<Vec<_>>())}),
            )?;
            Ok(reply)
        }
        _ => bail!("unknown command {command}; try forges.show or forges.set"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github() -> Value {
        json!({"forge": "github", "api": "https://api.github.com", "branch": "main",
            "required_checks": ["runtime (ubuntu-latest)"],
            "reviewer": {"account": "privateer-reviewer", "credential": "forge-reviewer-github"},
            "merger": {"account": "kirkins", "credential": "forge-ship-github"}})
    }

    #[test]
    fn a_forges_file_names_each_repository_fully_and_nothing_else() {
        let repos =
            validate(&json!({"repos": {"github.com/kirkins/privateer": github()}})).unwrap();
        assert_eq!(repos[0].repo, "kirkins/privateer");
        assert_eq!(repos[0].required_checks, ["runtime (ubuntu-latest)"]);
        assert_eq!(repos[0].merger.credential, "forge-ship-github");
        for (change, why) in [
            (json!({"forge": "gitlab"}), "forge must be"),
            (json!({"api": "http://api.github.com"}), "api must be"),
            (
                json!({"api": "https://user:pw@api.github.com"}),
                "api must be",
            ),
            (json!({"token": "x"}), "unknown"),
            (json!({"branch": ""}), "branch"),
            (json!({"required_checks": "ci"}), "required_checks"),
            (
                json!({"reviewer": {"account": "kirkins", "credential": "forge-ship-github"}}),
                "different accounts",
            ),
            (json!({"merger": null}), "merger"),
            (
                json!({"reviewer": {"account": "r", "credential": "Bad_Ref"}}),
                "credential ref",
            ),
        ] {
            let mut entry = github();
            for (key, value) in change.as_object().unwrap() {
                if value.is_null() {
                    entry.as_object_mut().unwrap().remove(key);
                } else {
                    entry[key] = value.clone();
                }
            }
            let error = validate(&json!({"repos": {"github.com/kirkins/privateer": entry}}))
                .unwrap_err()
                .to_string();
            assert!(error.contains(why), "{change}: {error}");
        }
        for key in [
            "kirkins/privateer",
            "GitHub.com/kirkins/privateer",
            "github.com/../x",
        ] {
            assert!(
                validate(&json!({"repos": {key: github()}})).is_err(),
                "{key}"
            );
        }
        // A local forge for tests may be plain http on loopback.
        let mut local = github();
        local["api"] = json!("http://127.0.0.1:3000");
        assert!(validate(&json!({"repos": {"git.example.net/hermes/app": local}})).is_ok());
    }
}
