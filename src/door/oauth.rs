//! Standalone authorization server. The adapter is a transport, never an issuer.
//! All state transitions serialize on the fleet registry writer lock. Display
//! strings are untrusted data; HTTP rendering/escaping belongs to the adapter.
use anyhow::{Context, Result, bail, ensure};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use subtle::ConstantTimeEq;

use super::clients;
use crate::store::Store;

const LOGIN_MS: i64 = 300_000;
const CONSENT_MS: i64 = 600_000;

// Login is single-use and must finish inside the initial five-minute window.
// Thus this extension can never exceed fifteen minutes from authorize.
fn consent_expiry(login_deadline: i64, now: i64) -> i64 {
    (now + CONSENT_MS).min(login_deadline + CONSENT_MS)
}
const CODE_MS: i64 = 60_000;
const ACCESS_MS: i64 = 900_000;
const REFRESH_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const MAX_PENDING: i64 = 1024;
const MAX_CONSENT_SHIPS: usize = 1024;
// 1024 maximum-length (160-byte ASCII) identifiers plus JSON and controls.
const MAX_CONSENT_BYTES: usize = 192 * 1024;
// Supports normal 15-minute refreshes throughout a 30-day family, while
// keeping a single application's retained replay evidence from crowding peers.
const MAX_APP_TOKENS: i64 = 4096;
const WORK_WINDOW_MS: i64 = 60_000;
const MAX_APP_PENDING: i64 = 32;

#[cfg(test)]
mod tests;
pub(super) const BOOTSTRAP: &[&str] = &[
    "oauth.authorize-begin",
    "oauth.login-submit",
    "oauth.consent-submit",
    "oauth.token",
    "oauth.revoke",
    "oauth.metadata",
];

// Schema DDL changes must also bump clients.rs SCHEMA_MARKER and its fingerprint.
pub(super) fn init(conn: &Connection) -> Result<()> {
    super::schema::write(conn, || migrate(conn))
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS door_oauth_servers(
        door TEXT PRIMARY KEY, issuer TEXT NOT NULL, resource TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS door_oauth_apps(
        id TEXT PRIMARY KEY, door TEXT NOT NULL, name TEXT NOT NULL,
        redirects TEXT NOT NULL, kind TEXT NOT NULL, secret_hash BLOB,
        removed INTEGER NOT NULL DEFAULT 0, created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS door_oauth_transactions(
        id TEXT PRIMARY KEY, app TEXT NOT NULL, door TEXT NOT NULL, data TEXT NOT NULL,
        csrf_hash BLOB NOT NULL, login_hash BLOB, login_expires INTEGER,
        authenticated INTEGER NOT NULL DEFAULT 0, expires_ms INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS door_oauth_grants(
        grant_id TEXT PRIMARY KEY, app TEXT NOT NULL,
        resource TEXT NOT NULL, issuer TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS door_oauth_codes(
        hash BLOB PRIMARY KEY, app TEXT NOT NULL, grant_id TEXT NOT NULL,
        data TEXT NOT NULL, expires_ms INTEGER NOT NULL, consumed INTEGER NOT NULL DEFAULT 0);
      CREATE TABLE IF NOT EXISTS door_oauth_families(
        id TEXT PRIMARY KEY, grant_id TEXT NOT NULL, app TEXT NOT NULL,
        door TEXT NOT NULL, resource TEXT NOT NULL, expires_ms INTEGER NOT NULL,
        revoked INTEGER NOT NULL DEFAULT 0);
      CREATE TABLE IF NOT EXISTS door_oauth_access(
        hash BLOB PRIMARY KEY, family TEXT NOT NULL, expires_ms INTEGER NOT NULL,
        revoked INTEGER NOT NULL DEFAULT 0);
      CREATE TABLE IF NOT EXISTS door_oauth_refresh(
        hash BLOB PRIMARY KEY, family TEXT NOT NULL, consumed INTEGER NOT NULL DEFAULT 0);
      CREATE TABLE IF NOT EXISTS door_oauth_failures(
        key BLOB PRIMARY KEY, window_ms INTEGER NOT NULL, attempts INTEGER NOT NULL);
      CREATE INDEX IF NOT EXISTS door_oauth_access_family ON door_oauth_access(family);
      CREATE INDEX IF NOT EXISTS door_oauth_refresh_family ON door_oauth_refresh(family);
      CREATE INDEX IF NOT EXISTS door_oauth_families_app ON door_oauth_families(app);")?;
    Ok(())
}

fn hash(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}
fn secret(kind: &str) -> String {
    // UUID v4 uses OS randomness; 244 random bits, as in local issuance.
    format!(
        "pv_{kind}_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn matches_hash(value: &str, digest: &[u8]) -> bool {
    bool::from(hash(value).as_slice().ct_eq(digest))
}
fn error(code: &str) -> Value {
    let code = code.split_ascii_whitespace().next().unwrap_or("");
    let code = if [
        "invalid_request",
        "invalid_client",
        "invalid_grant",
        "invalid_scope",
        "invalid_target",
        "access_denied",
        "unsupported_grant_type",
        "unsupported_response_type",
        "temporarily_unavailable",
        "slow_down",
    ]
    .contains(&code)
    {
        code
    } else {
        "server_error"
    };
    json!({"error":code})
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    let value = v[key]
        .as_str()
        .with_context(|| format!("invalid_request parameter={key}"))?;
    ensure!(
        !value.is_empty() && value.len() <= 4096,
        "invalid_request parameter={key}"
    );
    Ok(value)
}
fn fields(v: &Value, allowed: &[&str]) -> Result<()> {
    ensure!(
        v.as_object()
            .is_some_and(|o| o.keys().all(|k| allowed.contains(&k.as_str()))),
        "invalid_request parameter=unknown"
    );
    Ok(())
}
fn scopes(v: &Value) -> Result<Vec<String>> {
    let values: Vec<String> =
        serde_json::from_value(v.clone()).map_err(|_| anyhow::anyhow!("invalid_scope"))?;
    let mut unique = values;
    unique.sort();
    unique.dedup();
    ensure!(
        unique.len() <= clients::SCOPES.len()
            && unique.iter().all(|s| clients::SCOPES.contains(&s.as_str())),
        "invalid_scope"
    );
    Ok(unique)
}

// Deliberately conservative registration: HTTPS, or literal loopback HTTP.
// No URL normalization at authorization/exchange: matching is byte-for-byte.
fn uri(value: &str) -> Result<()> {
    ensure!(
        value.len() <= 2048
            && value.is_ascii()
            && !value.bytes().any(|c| c <= 32 || c == 127)
            && !value.contains(['#', '\\']),
        "invalid URI"
    );
    let (scheme, rest) = value.split_once("://").context("invalid URI")?;
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    ensure!(
        !authority.is_empty() && !authority.contains(['@', '%']),
        "invalid URI"
    );
    let host = if authority.starts_with('[') {
        let end = authority.find(']').context("invalid URI")?;
        ensure!(&authority[..=end] == "[::1]", "invalid URI");
        let tail = &authority[end + 1..];
        ensure!(
            tail.is_empty() || tail.strip_prefix(':').is_some_and(valid_port),
            "invalid URI"
        );
        "[::1]"
    } else {
        let mut parts = authority.split(':');
        let host = parts.next().unwrap_or("");
        ensure!(
            !host.is_empty()
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
            "invalid URI"
        );
        if let Some(port) = parts.next() {
            ensure!(valid_port(port), "invalid URI");
        }
        ensure!(parts.next().is_none(), "invalid URI");
        host
    };
    ensure!(
        scheme == "https"
            || (scheme == "http" && matches!(host, "127.0.0.1" | "[::1]" | "localhost")),
        "HTTPS or loopback required"
    );
    Ok(())
}
fn valid_port(value: &str) -> bool {
    value.parse::<u16>().is_ok_and(|p| p > 0)
}

#[derive(Serialize, Deserialize)]
struct Binding {
    redirect_uri: String,
    resource: String,
    state: String,
    scopes: Vec<String>,
    challenge: String,
    issuer: String,
}
fn server(conn: &Connection, door: &str) -> Result<(String, String)> {
    conn.query_row(
        "SELECT issuer,resource FROM door_oauth_servers WHERE door=?1",
        [door],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .context("temporarily_unavailable")
}
type Application = (String, Vec<String>, String, Option<Vec<u8>>);
fn app(conn: &Connection, door: &str, id: &str) -> Result<Application> {
    let (name, redirects, kind, digest): (String,String,String,Option<Vec<u8>>) = conn.query_row(
        "SELECT name,redirects,kind,secret_hash FROM door_oauth_apps WHERE id=?1 AND door=?2 AND removed=0",
        params![id,door], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))
        .optional()?.context("invalid_client")?;
    Ok((name, serde_json::from_str(&redirects)?, kind, digest))
}
fn metadata(conn: &Connection, door: &str, args: &Value) -> Result<Value> {
    fields(args, &[])?;
    let (issuer, resource) = server(conn, door)?;
    Ok(
        json!({"issuer":issuer,"authorization_endpoint":format!("{issuer}/authorize"),
        "token_endpoint":format!("{issuer}/token"),"revocation_endpoint":format!("{issuer}/revoke"),
        "code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["client_secret_post","client_secret_basic","none"],
        "revocation_endpoint_auth_methods_supported":["client_secret_post","client_secret_basic","none"],
        "scopes_supported":clients::SCOPES,"response_types_supported":["code"],
        "grant_types_supported":["authorization_code","refresh_token"],
        "authorization_response_iss_parameter_supported":true,
        "protected_resource":{"resource":resource,"authorization_servers":[issuer],"bearer_methods_supported":["header"]}}),
    )
}
fn prune(conn: &Connection) -> Result<()> {
    let now = Store::now_ms();
    conn.execute(
        "DELETE FROM door_oauth_transactions WHERE expires_ms<=?1",
        [now],
    )?;
    // Recover pre-activation-era pending grants, without removing the durable
    // owner-management record of any grant that actually issued tokens.
    conn.execute("DELETE FROM door_clients WHERE id IN (SELECT grant_id FROM door_oauth_codes WHERE consumed=0 AND expires_ms<=?1)", [now])?;
    // Keep consumed code hashes for the family lifetime: a replay must revoke
    // issued tokens even after the code's exchange deadline has passed.
    conn.execute("DELETE FROM door_oauth_codes WHERE expires_ms<=?1 AND (consumed=0 OR NOT EXISTS(SELECT 1 FROM door_oauth_families f WHERE f.grant_id=door_oauth_codes.grant_id AND f.expires_ms>?1))", [now])?;
    conn.execute(
        "DELETE FROM door_oauth_grants WHERE grant_id NOT IN (SELECT id FROM door_clients)",
        [],
    )?;
    conn.execute("DELETE FROM door_oauth_access WHERE expires_ms<=?1 OR family IN (SELECT id FROM door_oauth_families WHERE revoked=1)", [now])?;
    // Preserve replay evidence for every live family. Revoked families reject
    // all credentials unconditionally, so their token rows can be reclaimed.
    conn.execute("DELETE FROM door_oauth_refresh WHERE family IN (SELECT id FROM door_oauth_families WHERE expires_ms<=?1 OR revoked=1)",[now])?;
    conn.execute(
        "DELETE FROM door_oauth_families WHERE expires_ms<=?1",
        [now],
    )?;
    conn.execute(
        "DELETE FROM door_oauth_failures WHERE window_ms<=?1",
        [now - LOGIN_MS],
    )?;
    Ok(())
}
fn bounded(conn: &Connection, table: &str, limit: i64) -> Result<()> {
    let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
    ensure!(count < limit, "temporarily_unavailable");
    Ok(())
}
// Keys are hashes of core-known IDs and fixed operation names, never arbitrary
// source strings. Returning a protocol error (not Err) commits the brake.
fn budget(conn: &Connection, key: &str, limit: i64, window: i64) -> Result<bool> {
    let digest = hash(key);
    let now = Store::now_ms();
    let old: Option<(i64, i64)> = conn
        .query_row(
            "SELECT window_ms,attempts FROM door_oauth_failures WHERE key=?1",
            [&digest],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if old.is_some_and(|(start, attempts)| start > now - window && attempts >= limit) {
        return Ok(false);
    }
    if old.is_none() {
        bounded(conn, "door_oauth_failures", 4096)?;
    }
    conn.execute(
        "INSERT INTO door_oauth_failures(key,window_ms,attempts) VALUES(?1,?2,1)
        ON CONFLICT(key) DO UPDATE SET attempts=CASE WHEN window_ms<=?3 THEN 1 ELSE attempts+1 END,
        window_ms=CASE WHEN window_ms<=?3 THEN excluded.window_ms ELSE window_ms END",
        params![digest, now, now - window],
    )?;
    Ok(true)
}
fn prelogin(conn: &Connection, door: &str, reason: &str) -> Result<()> {
    clients::audit(conn, door, None, "oauth.prelogin", reason)
}
fn begin(conn: &Connection, door: &str, args: &Value) -> Result<Value> {
    fields(
        args,
        &[
            "client_id",
            "redirect_uri",
            "resource",
            "state",
            "scope",
            "code_challenge",
            "code_challenge_method",
            "response_type",
        ],
    )?;
    ensure!(
        text(args, "response_type")? == "code",
        "unsupported_response_type"
    );
    ensure!(
        text(args, "code_challenge_method")? == "S256",
        "invalid_request"
    );
    let challenge = text(args, "code_challenge")?;
    ensure!(
        challenge.len() == 43
            && URL_SAFE_NO_PAD
                .decode(challenge)
                .is_ok_and(|v| v.len() == 32),
        "invalid_request"
    );
    let id = text(args, "client_id")?;
    let (_, redirects, _, _) = app(conn, door, id)?;
    let redirect = text(args, "redirect_uri")?;
    ensure!(redirects.iter().any(|r| r == redirect), "invalid_request");
    let (issuer, resource) = server(conn, door)?;
    ensure!(text(args, "resource")? == resource, "invalid_target");
    let requested_scope = args
        .get("scope")
        .map(|v| v.as_str().context("invalid_scope"))
        .transpose()?
        .unwrap_or("");
    ensure!(requested_scope.len() <= 4096, "invalid_scope");
    let requested = requested_scope
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let requested = scopes(&json!(requested))?;
    let binding = Binding {
        redirect_uri: redirect.into(),
        resource,
        state: text(args, "state")?.into(),
        scopes: requested,
        challenge: challenge.into(),
        issuer,
    };
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM door_oauth_transactions WHERE app=?1",
        [id],
        |r| r.get(0),
    )?;
    if pending >= MAX_APP_PENDING {
        prelogin(conn, door, "begin_limited")?;
        return Ok(error("slow_down"));
    }
    bounded(conn, "door_oauth_transactions", MAX_PENDING)?;
    let transaction = uuid::Uuid::new_v4().to_string();
    let csrf = secret("csrf");
    conn.execute("INSERT INTO door_oauth_transactions(id,app,door,data,csrf_hash,expires_ms) VALUES(?1,?2,?3,?4,?5,?6)",
        params![transaction,id,door,serde_json::to_string(&binding)?,hash(&csrf),Store::now_ms()+LOGIN_MS])?;
    prelogin(conn, door, "pending_login")?;
    Ok(json!({"transaction":transaction,"csrf":csrf,"expires_in":LOGIN_MS/1000,"next":"login"}))
}
fn transaction(
    conn: &Connection,
    door: &str,
    id: &str,
) -> Result<(String, Binding, Vec<u8>, bool)> {
    let (app_id,data,csrf,authenticated): (String,String,Vec<u8>,bool) = conn.query_row(
        "SELECT app,data,csrf_hash,authenticated FROM door_oauth_transactions WHERE id=?1 AND door=?2 AND expires_ms>?3",
        params![id,door,Store::now_ms()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))
        .optional()?.context("invalid_grant")?;
    app(conn, door, &app_id)?;
    let binding: Binding = serde_json::from_str(&data)?;
    let (issuer, resource) = server(conn, door)?;
    ensure!(
        binding.resource == resource && binding.issuer == issuer,
        "invalid_target"
    );
    Ok((app_id, binding, csrf, authenticated))
}
fn summary(conn: &Connection, door: &str, id: &str) -> Result<Value> {
    let (app_id, binding, _, _) = transaction(conn, door, id)?;
    let (name, _, _, _) = app(conn, door, &app_id)?;
    Ok(
        json!({"transaction":id,"client_id":app_id,"display_name":name,"redirect_uri":binding.redirect_uri,
        "resource":binding.resource,"requested_scopes":binding.scopes,"default_scopes":[],"default_ships":[]}),
    )
}
fn login(conn: &Connection, door: &str, args: &Value) -> Result<Value> {
    fields(args, &["transaction", "csrf", "login_code", "source"])?;
    let id = text(args, "transaction")?;
    text(args, "source")?;
    let (app_id, _, csrf, authenticated) = transaction(conn, door, id)?;
    ensure!(
        !authenticated && matches_hash(text(args, "csrf")?, &csrf),
        "invalid_grant"
    );
    let digest: Option<Vec<u8>> = conn
        .query_row(
            "SELECT login_hash FROM door_oauth_transactions WHERE id=?1 AND login_expires>?2",
            params![id, Store::now_ms()],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    // Bind failures to this transaction AND this owner-issued login code.
    // Reissuing a code resets its budget, not another transaction's failures.
    let login_key = format!(
        "login:{id}:{}",
        URL_SAFE_NO_PAD.encode(digest.as_deref().unwrap_or(&[]))
    );
    let failures: i64 = conn
        .query_row(
            "SELECT attempts FROM door_oauth_failures WHERE key=?1 AND window_ms>?2",
            params![hash(&login_key), Store::now_ms() - LOGIN_MS],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if failures >= 5 {
        prelogin(conn, door, "login_limited")?;
        return Ok(error("slow_down"));
    }
    if !digest.is_some_and(|d| matches_hash(args["login_code"].as_str().unwrap_or(""), &d)) {
        // A bounded door-wide brake limits bad guesses across transactions,
        // but is deliberately checked only AFTER rejecting a wrong code: an
        // attacker cannot use it to lock out a fresh, correct owner login.
        if !budget(conn, &format!("login-global:{door}"), 1000, LOGIN_MS)? {
            prelogin(conn, door, "login_limited")?;
            return Ok(error("slow_down"));
        }
        budget(conn, &login_key, 5, LOGIN_MS)?;
        prelogin(conn, door, "login_failed")?;
        return Ok(error("access_denied"));
    }
    let consent = secret("consent");
    let login_deadline: i64 = conn.query_row(
        "SELECT expires_ms FROM door_oauth_transactions WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    let expires = consent_expiry(login_deadline, Store::now_ms());
    conn.execute("UPDATE door_oauth_transactions SET authenticated=1,login_hash=NULL,csrf_hash=?2,expires_ms=?3 WHERE id=?1",params![id,hash(&consent),expires])?;
    clients::audit(
        conn,
        door,
        Some(&app_id),
        "oauth.login",
        "owner_authenticated",
    )?;
    let mut result = summary(conn, door, id)?;
    // Release inventory only after consuming the owner's login code, with the
    // new browser-only consent credential. summary() is also used before login.
    let mut stmt = conn.prepare("SELECT name FROM ships WHERE retired_ms IS NULL UNION SELECT name FROM machine_ships ORDER BY name LIMIT ?1")?;
    let ships = stmt
        .query_map([MAX_CONSENT_SHIPS], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    result["known_ships"] = json!(ships);
    result["consent_token"] = json!(consent);
    result["next"] = json!("consent");
    result["expires_in"] = json!((expires - Store::now_ms()).max(0) / 1000);
    Ok(result)
}
fn consent(conn: &Connection, door: &str, args: &Value) -> Result<Value> {
    fields(
        args,
        &[
            "transaction",
            "consent_token",
            "scopes",
            "ships",
            "decision",
        ],
    )?;
    let id = text(args, "transaction")?;
    let (app_id, mut binding, csrf, authenticated) = transaction(conn, door, id)?;
    ensure!(
        authenticated && matches_hash(text(args, "consent_token")?, &csrf),
        "access_denied"
    );
    let decision = args
        .get("decision")
        .map(|v| v.as_str().context("invalid_request"))
        .transpose()?
        .unwrap_or("approve");
    ensure!(matches!(decision, "approve" | "deny"), "invalid_request");
    if decision == "deny" {
        conn.execute("DELETE FROM door_oauth_transactions WHERE id=?1", [id])?;
        clients::audit(conn, door, Some(&app_id), "oauth.consent", "owner_denied")?;
        return Ok(
            json!({"redirect_uri":binding.redirect_uri,"error":"access_denied","state":binding.state,"iss":binding.issuer}),
        );
    }
    let approved = scopes(&args["scopes"])?;
    ensure!(
        approved.iter().all(|s| binding.scopes.contains(s)),
        "invalid_scope"
    );
    // An explicit array is mandatory. [] is minimal/no ship visibility; null
    // (unrestricted ships) must be a deliberate selection on the owner page.
    ensure!(args.get("ships").is_some(), "invalid_request");
    let ships: Option<Vec<String>> = serde_json::from_value(args["ships"].clone())
        .map_err(|_| anyhow::anyhow!("invalid_request"))?;
    if let Some(ships) = &ships {
        if ships.len() > MAX_CONSENT_SHIPS {
            return Ok(json!({"error":"invalid_request",
                "error_description":"Select at most 1024 named ships; reduce the selection or deliberately choose All ships (current and future)."}));
        }
        for ship in ships {
            crate::store::identifier(ship).map_err(|_| anyhow::anyhow!("invalid_request"))?;
        }
    }
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM door_oauth_codes WHERE consumed=0",
        [],
        |r| r.get(0),
    )?;
    ensure!(pending < MAX_PENDING, "temporarily_unavailable");
    let grant = uuid::Uuid::new_v4().to_string();
    binding.scopes = approved;
    // Consent is an expiring authorization, not yet a live grant. Only a
    // successful exchange inserts authority into the shared client store.
    let mut data = serde_json::to_value(&binding)?;
    data["ships"] = json!(ships);
    let code = secret("code");
    conn.execute(
        "INSERT INTO door_oauth_codes(hash,app,grant_id,data,expires_ms) VALUES(?1,?2,?3,?4,?5)",
        params![
            hash(&code),
            app_id,
            grant,
            serde_json::to_string(&data)?,
            Store::now_ms() + CODE_MS
        ],
    )?;
    conn.execute("DELETE FROM door_oauth_transactions WHERE id=?1", [id])?;
    clients::audit(conn, door, Some(&app_id), "oauth.consent", "owner_consent")?;
    Ok(
        json!({"redirect_uri":binding.redirect_uri,"code":code,"state":binding.state,"iss":binding.issuer}),
    )
}

// RFC 6749 Basic credentials are form-url-encoded before base64. Decoding is
// bounded by the core message bound; reject duplicate authentication methods.
fn form_decode(value: &str) -> Result<String> {
    let mut out = Vec::new();
    let mut bytes = value.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let h = (bytes.next().context("invalid_client")? as char)
                    .to_digit(16)
                    .context("invalid_client")?;
                let l = (bytes.next().context("invalid_client")? as char)
                    .to_digit(16)
                    .context("invalid_client")?;
                out.push((h * 16 + l) as u8);
            }
            _ => out.push(b),
        }
    }
    String::from_utf8(out).map_err(|_| anyhow::anyhow!("invalid_client"))
}
fn authenticate(conn: &Connection, door: &str, args: &Value) -> Result<String> {
    let (id, supplied) = if let Some(basic) = args.get("client_secret_basic") {
        ensure!(
            args.get("client_id").is_none() && args.get("client_secret").is_none(),
            "invalid_client"
        );
        let encoded = basic.as_str().context("invalid_client")?;
        ensure!(encoded.len() <= 4096, "invalid_client");
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| anyhow::anyhow!("invalid_client"))?;
        let decoded = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("invalid_client"))?;
        let (id, secret) = decoded.split_once(':').context("invalid_client")?;
        (form_decode(id)?, Some(form_decode(secret)?))
    } else {
        (
            text(args, "client_id")?.to_owned(),
            args.get("client_secret")
                .map(|s| s.as_str().map(str::to_owned).context("invalid_client"))
                .transpose()?,
        )
    };
    let (_, _, kind, digest) = app(conn, door, &id)?;
    match kind.as_str() {
        "public" => ensure!(
            supplied.is_none() && args.get("client_secret_basic").is_none(),
            "invalid_client"
        ),
        "confidential" => ensure!(
            supplied
                .zip(digest)
                .is_some_and(|(s, d)| matches_hash(&s, &d)),
            "invalid_client"
        ),
        _ => bail!("invalid_client"),
    }
    Ok(id)
}
fn live_grant(conn: &Connection, grant: &str) -> Result<Vec<String>> {
    let scopes: String = conn.query_row("SELECT scopes FROM door_clients WHERE id=?1 AND revoked_ms IS NULL AND paused=0 AND (expires_ms IS NULL OR expires_ms>?2)",
        params![grant,Store::now_ms()],|r|r.get(0)).optional()?.context("invalid_grant")?;
    Ok(serde_json::from_str(&scopes)?)
}
fn mint(conn: &Connection, family: &str, grant: &str) -> Result<Value> {
    let scopes = live_grant(conn, grant)?;
    // Retain replay evidence for the entire family lifetime, but charge both
    // live access and retained refresh rows to their application, not everyone.
    for table in ["door_oauth_access", "door_oauth_refresh"] {
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} t JOIN door_oauth_families f ON f.id=t.family WHERE f.revoked=0 AND f.app=(SELECT app FROM door_oauth_families WHERE id=?1)"),
            [family], |r| r.get(0))?;
        ensure!(count < MAX_APP_TOKENS, "temporarily_unavailable");
    }
    let access = secret("access");
    let refresh = secret("refresh");
    conn.execute(
        "INSERT INTO door_oauth_access(hash,family,expires_ms) VALUES(?1,?2,?3)",
        params![hash(&access), family, Store::now_ms() + ACCESS_MS],
    )?;
    conn.execute(
        "INSERT INTO door_oauth_refresh(hash,family) VALUES(?1,?2)",
        params![hash(&refresh), family],
    )?;
    Ok(
        json!({"access_token":access,"token_type":"Bearer","expires_in":ACCESS_MS/1000,"refresh_token":refresh,"scope":scopes.join(" ")}),
    )
}
fn token(conn: &Connection, door: &str, args: &Value) -> Result<Value> {
    fields(
        args,
        &[
            "grant_type",
            "client_id",
            "client_secret",
            "client_secret_basic",
            "code",
            "redirect_uri",
            "resource",
            "code_verifier",
            "refresh_token",
        ],
    )?;
    let id = authenticate(conn, door, args)?;
    let (_, resource) = server(conn, door)?;
    // RFC 8707 permits omission at exchange/refresh. The lookups below still
    // require the code/family's original audience to equal this resource.
    if args.get("resource").is_some() {
        ensure!(
            text(args, "resource")? == resource,
            "invalid_target parameter=resource"
        );
    }
    match text(args, "grant_type")? {
        "authorization_code" => {
            ensure!(
                args.get("refresh_token").is_none(),
                "invalid_request parameter=refresh_token"
            );
            let digest = hash(text(args, "code")?);
            let (grant,data,consumed,expires): (String,String,bool,i64) = conn.query_row("SELECT grant_id,data,consumed,expires_ms FROM door_oauth_codes WHERE hash=?1 AND app=?2",
                params![digest,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.context("invalid_grant")?;
            if consumed {
                let changed = conn.execute(
                    "UPDATE door_oauth_families SET revoked=1 WHERE grant_id=?1 AND revoked=0",
                    [&grant],
                )?;
                if changed > 0 {
                    clients::audit(
                        conn,
                        door,
                        Some(&grant),
                        "oauth.token",
                        "code_reuse_revoked_family",
                    )?;
                }
                return Ok(error("invalid_grant"));
            }
            ensure!(expires > Store::now_ms(), "invalid_grant");
            let binding: Binding = serde_json::from_str(&data)?;
            let (issuer, _) = server(conn, door)?;
            ensure!(
                binding.resource == resource
                    && binding.issuer == issuer
                    && text(args, "redirect_uri")? == binding.redirect_uri,
                "invalid_grant"
            );
            let verifier = text(args, "code_verifier")?;
            ensure!(
                (43..=128).contains(&verifier.len())
                    && verifier
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)),
                "invalid_grant"
            );
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            ensure!(
                bool::from(challenge.as_bytes().ct_eq(binding.challenge.as_bytes())),
                "invalid_grant"
            );
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM door_clients WHERE id=?1)",
                [&grant],
                |r| r.get(0),
            )?;
            if !exists {
                let count: i64 = conn.query_row("SELECT COUNT(*) FROM door_oauth_grants g JOIN door_clients c ON c.id=g.grant_id WHERE c.revoked_ms IS NULL AND (c.expires_ms IS NULL OR c.expires_ms>?1)", [Store::now_ms()], |r| r.get(0))?;
                ensure!(count < 4096, "temporarily_unavailable");
                let data: Value = serde_json::from_str(&data)?;
                let ships: Option<Vec<String>> =
                    serde_json::from_value(data.get("ships").context("invalid_grant")?.clone())?;
                let (name, _, _, _) = app(conn, door, &id)?;
                // Internal delegation retains door:NAME; external audiences
                // remain bound separately and checked on every admission.
                conn.execute("INSERT INTO door_clients(id,name,door,resource,owner,registration,scopes,ships,created_ms,updated_ms,token_hash)
                    VALUES(?1,?2,?3,?4,'local-captain','oauth-standalone',?5,?6,?7,?7,?8)",
                    params![grant,name,door,format!("door:{door}"),serde_json::to_string(&binding.scopes)?,ships.map(|s|serde_json::to_string(&s)).transpose()?,Store::now_ms(),hash(&secret("unissued"))])?;
                conn.execute("INSERT INTO door_oauth_grants(grant_id,app,resource,issuer) VALUES(?1,?2,?3,?4)", params![grant,id,binding.resource,binding.issuer])?;
            }
            live_grant(conn, &grant)?;
            bounded(conn, "door_oauth_families", 4096)?;
            let family = uuid::Uuid::new_v4().to_string();
            conn.execute("INSERT INTO door_oauth_families(id,grant_id,app,door,resource,expires_ms) VALUES(?1,?2,?3,?4,?5,?6)",
                params![family,grant,id,door,resource,Store::now_ms()+REFRESH_MS])?;
            let result = mint(conn, &family, &grant)?;
            conn.execute(
                "UPDATE door_oauth_codes SET consumed=1 WHERE hash=?1",
                [digest],
            )?;
            clients::audit(conn, door, Some(&grant), "oauth.token", "code_exchanged")?;
            Ok(result)
        }
        "refresh_token" => {
            for parameter in ["code", "redirect_uri", "code_verifier"] {
                ensure!(
                    args.get(parameter).is_none(),
                    "invalid_request parameter={parameter}"
                );
            }
            let digest = hash(text(args, "refresh_token")?);
            let (family,grant,consumed): (String,String,bool) = conn.query_row("SELECT f.id,f.grant_id,r.consumed FROM door_oauth_refresh r JOIN door_oauth_families f ON f.id=r.family WHERE r.hash=?1 AND f.app=?2 AND f.door=?3 AND f.resource=?4 AND f.revoked=0 AND f.expires_ms>?5",
                params![digest,id,door,resource,Store::now_ms()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.context("invalid_grant")?;
            if consumed {
                conn.execute(
                    "UPDATE door_oauth_families SET revoked=1 WHERE id=?1",
                    [&family],
                )?;
                clients::audit(
                    conn,
                    door,
                    Some(&grant),
                    "oauth.refresh",
                    "reuse_revoked_family",
                )?;
                return Ok(error("invalid_grant"));
            }
            let result = mint(conn, &family, &grant)?;
            conn.execute(
                "UPDATE door_oauth_refresh SET consumed=1 WHERE hash=?1",
                [digest],
            )?;
            clients::audit(conn, door, Some(&grant), "oauth.refresh", "rotated")?;
            Ok(result)
        }
        _ => bail!("unsupported_grant_type"),
    }
}
fn revoke(conn: &Connection, door: &str, args: &Value) -> Result<Value> {
    fields(
        args,
        &[
            "client_id",
            "client_secret",
            "client_secret_basic",
            "token",
            "token_type_hint",
        ],
    )?;
    let id = authenticate(conn, door, args)?;
    if let Some(hint) = args.get("token_type_hint") {
        ensure!(hint.is_string(), "invalid_request");
    }
    let digest = hash(text(args, "token")?);
    // Ignore hints for lookup, and never disclose existence/ownership.
    let families = conn.execute("UPDATE door_oauth_families SET revoked=1 WHERE app=?1 AND door=?2 AND revoked=0 AND expires_ms>?4 AND id IN (SELECT family FROM door_oauth_refresh WHERE hash=?3)",params![id,door,digest,Store::now_ms()])?;
    let access = conn.execute("UPDATE door_oauth_access SET revoked=1 WHERE hash=?1 AND revoked=0 AND expires_ms>?4 AND family IN (SELECT id FROM door_oauth_families WHERE app=?2 AND door=?3 AND revoked=0 AND expires_ms>?4)",params![digest,id,door,Store::now_ms()])?;
    if families + access > 0 {
        clients::audit(conn, door, Some(&id), "oauth.revoke", "live_token_revoked")?;
    } else {
        prelogin(conn, door, "revoke_accepted")?;
    }
    Ok(json!({}))
}

/// Exact allowlist, before delegated identity validation (OAuth necessarily has
/// client_id/resource fields). Only owner-authenticated login releases ship names.
pub(super) fn request(door: &str, command: &str, args: &Value) -> Value {
    let run = || -> Result<Value> {
        let mut conn = crate::fleet::registry()?;
        clients::init(&conn)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = operate(&tx, door, command, args);
        match result {
            Ok(value) => {
                tx.commit()?;
                Ok(value)
            }
            Err(e) => {
                tx.rollback()?;
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                clients::audit(&tx, door, None, "auth.failure", "oauth_denied")?;
                tx.commit()?;
                Err(e)
            }
        }
    };
    match run() {
        Ok(value) => value,
        Err(e) => error(&e.to_string()),
    }
}
fn operate(conn: &Connection, door: &str, command: &str, args: &Value) -> Result<Value> {
    if !matches!(command, "oauth.token" | "oauth.revoke") {
        return operate_inner(conn, door, command, args);
    }
    // Roll back partial issuance on failure, but retain replay revocations and
    // rate budgets returned as protocol errors by the state machine.
    conn.execute_batch("SAVEPOINT oauth_exchange")?;
    let result = operate_inner(conn, door, command, args);
    if result.is_err() {
        conn.execute_batch("ROLLBACK TO oauth_exchange")?;
    }
    conn.execute_batch("RELEASE oauth_exchange")?;
    let detail = result.as_ref().err().map(ToString::to_string);
    let value = match result {
        Ok(value) => value,
        Err(e) => error(&e.to_string()),
    };
    if value.get("error").is_some() {
        exchange_failure(conn, door, command, args, &value, detail.as_deref())?;
    }
    Ok(value)
}

fn exchange_failure(
    conn: &Connection,
    door: &str,
    command: &str,
    args: &Value,
    result: &Value,
    detail: Option<&str>,
) -> Result<()> {
    // Only IDs found in this door's registry may enter audit. Basic payloads
    // are decoded for the ID only, never logged (nor any supplied credentials).
    let basic_id = args["client_secret_basic"]
        .as_str()
        .filter(|s| s.len() <= 4096)
        .and_then(|s| STANDARD.decode(s).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.split_once(':').and_then(|(id, _)| form_decode(id).ok()));
    let candidate = args["client_id"].as_str().or(basic_id.as_deref());
    let client = candidate.filter(|id| app(conn, door, id).is_ok());
    let grant_type = match args["grant_type"].as_str() {
        Some("authorization_code") => "authorization_code",
        Some("refresh_token") => "refresh_token",
        None if args.get("grant_type").is_none() => "missing",
        _ => "other",
    };
    let parameter = detail
        .and_then(|s| {
            s.split_ascii_whitespace()
                .find_map(|s| s.strip_prefix("parameter="))
        })
        .filter(|p| {
            [
                "client_id",
                "client_secret",
                "client_secret_basic",
                "grant_type",
                "code",
                "redirect_uri",
                "resource",
                "code_verifier",
                "refresh_token",
                "token",
                "token_type_hint",
                "unknown",
                "method",
                "query",
                "content_type",
                "form",
                "authorization",
            ]
            .contains(p)
        });
    // Revoke preserves its work budget by converting errors to protocol values;
    // identify malformed required fields without copying any supplied values.
    let parameter = parameter
        .or_else(|| {
            if result["error"] != "invalid_request" {
                return None;
            }
            if command == "oauth.revoke" {
                if args["token"]
                    .as_str()
                    .is_none_or(|s| s.is_empty() || s.len() > 4096)
                {
                    return Some("token");
                }
                if args.get("token_type_hint").is_some_and(|v| !v.is_string()) {
                    return Some("token_type_hint");
                }
            }
            Some("unknown")
        })
        .unwrap_or("unspecified");
    let reason = json!({"operation":if command == "oauth.token" {"token"} else {"revoke"},
        "error":result["error"], "parameter":parameter, "grant_type":grant_type})
    .to_string();
    clients::audit(conn, door, client, "oauth.exchange_failure", &reason)
}

fn operate_inner(conn: &Connection, door: &str, command: &str, args: &Value) -> Result<Value> {
    let max_bytes = if command == "oauth.consent-submit" {
        MAX_CONSENT_BYTES
    } else {
        16384
    };
    ensure!(
        serde_json::to_vec(args)?.len() <= max_bytes,
        "invalid_request"
    );
    prune(conn)?;
    if matches!(command, "oauth.token" | "oauth.revoke") {
        if let Some(parameter) = args.get("transport_error") {
            let parameter = parameter
                .as_str()
                .filter(|p| {
                    ["method", "query", "content_type", "form", "authorization"].contains(p)
                })
                .context("invalid_request parameter=unknown")?;
            if parameter == "authorization" {
                bail!("invalid_client parameter=authorization");
            }
            bail!("invalid_request parameter={parameter}");
        }
    }
    if matches!(command, "oauth.authorize-begin" | "oauth.revoke") {
        let (id, operation, limit) =
            if command == "oauth.authorize-begin" {
                let id = text(args, "client_id")?.to_owned();
                app(conn, door, &id)?;
                (id, "begin", 32)
            } else {
                let id = match authenticate(conn, door, args) {
                    Ok(id) => id,
                    Err(_) => {
                        let allowed =
                            budget(conn, &format!("revoke-failed:{door}"), 60, WORK_WINDOW_MS)?;
                        prelogin(conn, door, "revoke_denied")?;
                        return Ok(error(if allowed {
                            "invalid_client"
                        } else {
                            "slow_down"
                        }));
                    }
                };
                // Public client IDs are not authentication. Charge their client
                // budget only when possession of an owned token has been proved.
                // Unknown/wrong-owner tokens share a bounded anonymous bucket.
                let (_, _, kind, _) = app(conn, door, &id)?;
                let owned: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM door_oauth_families f WHERE f.app=?1 AND f.door=?2 AND
                 (f.id IN (SELECT family FROM door_oauth_refresh WHERE hash=?3) OR
                  f.id IN (SELECT family FROM door_oauth_access WHERE hash=?3)))",
                params![id, door, hash(args["token"].as_str().unwrap_or(""))], |r| r.get(0))?;
                if kind == "public" && !owned {
                    ("anonymous".to_owned(), "revoke-anonymous", 60)
                } else {
                    (id, "revoke", 60)
                }
            };
        if !budget(
            conn,
            &format!("{operation}:{door}:{id}"),
            limit,
            WORK_WINDOW_MS,
        )? {
            prelogin(
                conn,
                door,
                if operation == "begin" {
                    "begin_limited"
                } else {
                    "revoke_limited"
                },
            )?;
            return Ok(error("slow_down"));
        }
        // Preserve the work budget even for invalid bindings/fields, while
        // rolling back any partial state transition. Never serialize raw errors.
        conn.execute_batch("SAVEPOINT oauth_work")?;
        let result = if operation == "begin" {
            begin(conn, door, args)
        } else {
            revoke(conn, door, args)
        };
        if result.is_err() {
            conn.execute_batch("ROLLBACK TO oauth_work")?;
        }
        conn.execute_batch("RELEASE oauth_work")?;
        return match result {
            Ok(value) => Ok(value),
            Err(e) => {
                prelogin(
                    conn,
                    door,
                    if operation == "begin" {
                        "begin_denied"
                    } else {
                        "revoke_denied"
                    },
                )?;
                Ok(error(&e.to_string()))
            }
        };
    }
    match command {
        "oauth.metadata" => metadata(conn, door, args),
        "oauth.login-submit" => login(conn, door, args),
        "oauth.consent-submit" => consent(conn, door, args),
        "oauth.token" => token(conn, door, args),
        _ => bail!("invalid_request"),
    }
}

/// Called only by the hashed credential resolver under its admission lock.
/// Local tokens never enter this table; OAuth tokens require explicit audience.
pub(super) fn credential(
    conn: &Connection,
    door: &str,
    digest: &[u8],
    resource: Option<&str>,
) -> Result<String> {
    let (grant,bound_door,bound_resource,stored): (String,String,String,Vec<u8>) = conn.query_row(
        "SELECT f.grant_id,f.door,f.resource,a.hash FROM door_oauth_access a
         JOIN door_oauth_families f ON f.id=a.family JOIN door_oauth_apps c ON c.id=f.app
         JOIN door_oauth_grants g ON g.grant_id=f.grant_id AND g.app=f.app AND g.resource=f.resource
         JOIN door_oauth_servers s ON s.door=f.door AND s.resource=f.resource AND s.issuer=g.issuer
         WHERE a.hash=?1 AND a.revoked=0 AND a.expires_ms>?2 AND f.revoked=0 AND f.expires_ms>?2 AND c.removed=0",
        params![digest,Store::now_ms()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))
        .optional()?.context("invalid_grant")?;
    ensure!(
        bound_door == door
            && resource == Some(bound_resource.as_str())
            && bool::from(digest.ct_eq(&stored)),
        "invalid_target"
    );
    Ok(grant)
}

const USAGE: &str = "usage: privateer door oauth-client configure DOOR --issuer URL --resource URL | add DOOR --name NAME --redirect-uri URI [--type public|confidential] | edit ID --redirect-uri URI | list [DOOR] | inspect ID | remove ID | rotate-secret ID | login-code TRANSACTION";

pub(super) fn cli(args: &[String]) -> Result<()> {
    for marker in ["PRIVATEER_ROLE", "PRIVATEER_JOB"] {
        ensure!(
            std::env::var_os(marker).is_none(),
            "OAuth lifecycle is captain-only, outside agent/job sessions"
        );
    }
    let flags = crate::cli::flags(
        args,
        &["issuer", "resource", "name", "redirect-uri", "type"],
    )?;
    ensure!(
        flags.switches.is_empty() && flags.positional.len() <= 2,
        USAGE
    );
    let verb = flags
        .positional
        .first()
        .map(String::as_str)
        .context(USAGE)?;
    let target = flags.positional.get(1).map(String::as_str);
    let mut conn = crate::fleet::registry()?;
    clients::init(&conn)?;
    match verb {
        "configure" => {
            ensure!(flags.values.len() == 2, USAGE);
            let door = target.context(USAGE)?;
            let issuer = flags.values.get("issuer").context(USAGE)?;
            let resource = flags.values.get("resource").context(USAGE)?;
            uri(issuer)?;
            uri(resource)?;
            ensure!(
                !issuer.ends_with('/') && !issuer.contains('?'),
                "issuer must have no trailing slash or query"
            );
            let mut tty = clients::terminal(
                &json!({"door":door,"issuer":issuer,"resource":resource}),
                "configure",
            )?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            scoped_door(&tx, door)?;
            // Changing authority/audience requires removing clients first.
            let old = server(&tx, door).ok();
            ensure!(
                old.is_none()
                    || old == Some((issuer.clone(), resource.clone()))
                    || !tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM door_oauth_apps WHERE door=?1 AND removed=0)",
                        [door],
                        |r| r.get::<_, bool>(0)
                    )?,
                "remove OAuth clients before changing issuer/resource"
            );
            tx.execute("INSERT INTO door_oauth_servers(door,issuer,resource) VALUES(?1,?2,?3) ON CONFLICT(door) DO UPDATE SET issuer=excluded.issuer,resource=excluded.resource",params![door,issuer,resource])?;
            clients::audit(&tx, door, None, "oauth.configure", "owner_action")?;
            tx.commit()?;
            writeln!(
                tty,
                "OAuth authority configured (HTTP serving is separate)."
            )?;
            println!("{}", json!({"configured":true}));
        }
        "add" => {
            ensure!(flags.values.len() == 2 || flags.values.len() == 3, USAGE);
            let door = target.context(USAGE)?;
            let name = flags.values.get("name").context(USAGE)?;
            ensure!(
                !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control),
                "invalid display name"
            );
            let redirects = flags.values.get("redirect-uri").context(USAGE)?;
            let redirects = redirect_list(redirects)?;
            let kind = flags
                .values
                .get("type")
                .map(String::as_str)
                .unwrap_or("public");
            ensure!(matches!(kind, "public" | "confidential"), USAGE);
            let id = uuid::Uuid::new_v4().to_string();
            let display = json!({"client_id":id,"door":door,"display_name":name,"redirect_uris":redirects,"client_type":kind,"grants":[]});
            let mut tty = clients::terminal(&display, "add")?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            scoped_door(&tx, door)?;
            server(&tx, door)?;
            bounded(&tx, "door_oauth_apps", 1024)?;
            let credential = (kind == "confidential").then(|| secret("secret"));
            tx.execute("INSERT INTO door_oauth_apps(id,door,name,redirects,kind,secret_hash,created_ms,updated_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
                params![id,door,name,serde_json::to_string(&redirects)?,kind,credential.as_deref().map(hash),Store::now_ms()])?;
            clients::audit(
                &tx,
                door,
                Some(&id),
                "oauth.register",
                "owner_registration_no_grant",
            )?;
            tx.commit()?;
            if let Some(credential) = credential {
                writeln!(tty, "Client secret (shown once): {credential}")?;
            }
            println!("{display}");
        }
        "edit" => {
            ensure!(flags.values.len() == 1, USAGE);
            let id = target.context(USAGE)?;
            let redirects = redirect_list(flags.values.get("redirect-uri").context(USAGE)?)?;
            let door: String = conn
                .query_row(
                    "SELECT door FROM door_oauth_apps WHERE id=?1 AND removed=0",
                    [id],
                    |r| r.get(0),
                )
                .optional()?
                .context("unknown OAuth client")?;
            let (name, old, _, _) = app(&conn, &door, id)?;
            let display = json!({"client_id":id,"door":door,"display_name":name,
                "old_redirect_uris":old,"redirect_uris":redirects,
                "existing_grants":"preserved", "pending_authorizations":"invalidated"});
            let _tty = clients::terminal(&display, "edit")?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            scoped_door(&tx, &door)?;
            let (_, current, _, _) = app(&tx, &door, id)?;
            ensure!(
                current == old,
                "registration changed during confirmation; retry"
            );
            tx.execute(
                "UPDATE door_oauth_apps SET redirects=?2,updated_ms=?3 WHERE id=?1",
                params![id, serde_json::to_string(&redirects)?, Store::now_ms()],
            )?;
            tx.execute("DELETE FROM door_oauth_transactions WHERE app=?1", [id])?;
            // Keep consumed-code replay evidence for surviving token families.
            tx.execute(
                "DELETE FROM door_oauth_codes WHERE app=?1 AND consumed=0",
                [id],
            )?;
            clients::audit(
                &tx,
                &door,
                Some(id),
                "oauth.lifecycle",
                "redirects_edited_grants_preserved",
            )?;
            tx.commit()?;
            println!("{display}");
        }
        "login-code" => {
            ensure!(flags.values.is_empty(), USAGE);
            let id = target.context(USAGE)?;
            let door: String = conn
                .query_row(
                    "SELECT door FROM door_oauth_transactions WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?
                .context("unknown transaction")?;
            let display = summary(&conn, &door, id)?;
            let mut tty = clients::terminal(&display, "login-code")?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            scoped_door(&tx, &door)?;
            let (_, _, _, authenticated) = transaction(&tx, &door, id)?;
            ensure!(!authenticated, "already authenticated");
            let credential = secret("login");
            tx.execute(
                "UPDATE door_oauth_transactions SET login_hash=?2,login_expires=?3 WHERE id=?1",
                params![id, hash(&credential), Store::now_ms() + LOGIN_MS],
            )?;
            clients::audit(&tx, &door, None, "oauth.login_code", "owner_issued")?;
            tx.commit()?;
            writeln!(tty, "Login code (shown once): {credential}")?;
            println!("{}", json!({"transaction":id,"expires_in":LOGIN_MS/1000}));
        }
        "list" | "inspect" => {
            ensure!(flags.values.is_empty(), USAGE);
            let mut stmt = conn.prepare("SELECT id,door,name,redirects,kind,removed,created_ms,updated_ms FROM door_oauth_apps WHERE (?1 IS NULL OR id=?1) AND (?2 IS NULL OR door=?2) ORDER BY created_ms,id")?;
            let rows = stmt.query_map(params![if verb=="inspect" {Some(target.context(USAGE)?)} else {None},if verb=="list" {target} else {None}],|r|Ok(json!({"client_id":r.get::<_,String>(0)?,"door":r.get::<_,String>(1)?,"display_name":r.get::<_,String>(2)?,"redirect_uris":r.get::<_,String>(3)?,"client_type":r.get::<_,String>(4)?,"removed":r.get::<_,bool>(5)?,"created_ms":r.get::<_,i64>(6)?,"updated_ms":r.get::<_,i64>(7)?,"registration":"owner-terminal"})))?;
            let mut values = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            if verb == "inspect" {
                ensure!(values.len() == 1, "unknown OAuth client");
            }
            for value in &mut values {
                value["redirect_uris"] = serde_json::from_str(
                    value["redirect_uris"]
                        .as_str()
                        .context("invalid redirects")?,
                )?;
            }
            println!("{}", json!({"applications":values}));
        }
        "remove" | "rotate-secret" => {
            ensure!(flags.values.is_empty(), USAGE);
            let id = target.context(USAGE)?;
            let door: String = conn
                .query_row(
                    "SELECT door FROM door_oauth_apps WHERE id=?1 AND removed=0",
                    [id],
                    |r| r.get(0),
                )
                .optional()?
                .context("unknown OAuth client")?;
            let (name, redirects, kind, _) = app(&conn, &door, id)?;
            ensure!(
                verb != "rotate-secret" || kind == "confidential",
                "public client has no secret"
            );
            let mut tty = clients::terminal(
                &json!({"client_id":id,"display_name":name,"redirect_uris":redirects,"door":door}),
                verb,
            )?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            app(&tx, &door, id)?;
            let credential = (verb == "rotate-secret").then(|| secret("secret"));
            if let Some(credential) = &credential {
                tx.execute(
                    "UPDATE door_oauth_apps SET secret_hash=?2,updated_ms=?3 WHERE id=?1",
                    params![id, hash(credential), Store::now_ms()],
                )?;
            } else {
                tx.execute("UPDATE door_oauth_apps SET removed=1,secret_hash=NULL,updated_ms=?2 WHERE id=?1",params![id,Store::now_ms()])?;
                tx.execute("UPDATE door_clients SET revoked_ms=?2,updated_ms=?2 WHERE id IN (SELECT grant_id FROM door_oauth_grants WHERE app=?1)",params![id,Store::now_ms()])?;
            }
            tx.execute(
                "UPDATE door_oauth_families SET revoked=1 WHERE app=?1",
                [id],
            )?;
            tx.execute("DELETE FROM door_oauth_transactions WHERE app=?1", [id])?;
            tx.execute("DELETE FROM door_oauth_codes WHERE app=?1", [id])?;
            clients::audit(
                &tx,
                &door,
                Some(id),
                "oauth.lifecycle",
                if credential.is_some() {
                    "secret_rotated"
                } else {
                    "removed"
                },
            )?;
            tx.commit()?;
            if let Some(credential) = credential {
                writeln!(tty, "Client secret (shown once): {credential}")?;
            }
            println!("{}", json!({"client_id":id,"action":verb}));
        }
        _ => bail!(USAGE),
    }
    Ok(())
}
fn redirect_list(value: &str) -> Result<Vec<String>> {
    // CSV convenience, or one JSON array for URIs containing commas.
    let redirects: Vec<String> = if value.starts_with('[') {
        serde_json::from_str(value)?
    } else {
        value.split(',').map(str::to_owned).collect()
    };
    ensure!(
        !redirects.is_empty() && redirects.len() <= 16,
        "invalid redirect list"
    );
    for redirect in &redirects {
        uri(redirect)?;
    }
    Ok(redirects)
}

fn scoped_door(conn: &Connection, door: &str) -> Result<()> {
    let protocol: Option<u32> = conn
        .query_row("SELECT protocol FROM doors WHERE name=?1", [door], |r| {
            r.get(0)
        })
        .optional()?;
    ensure!(protocol == Some(2), "OAuth requires a scoped v2 door");
    Ok(())
}

/// Door removal/source replacement invalidates registration as well as grants.
pub(super) fn revoke_door(conn: &Connection, door: &str) -> Result<()> {
    conn.execute(
        "UPDATE door_oauth_apps SET removed=1,secret_hash=NULL WHERE door=?1",
        [door],
    )?;
    conn.execute(
        "UPDATE door_oauth_families SET revoked=1 WHERE door=?1",
        [door],
    )?;
    conn.execute("DELETE FROM door_oauth_transactions WHERE door=?1", [door])?;
    conn.execute(
        "DELETE FROM door_oauth_codes WHERE app IN (SELECT id FROM door_oauth_apps WHERE door=?1)",
        [door],
    )?;
    Ok(())
}
