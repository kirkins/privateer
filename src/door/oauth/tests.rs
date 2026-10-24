use super::super::scoped::Denial;
use super::*;

const DOOR: &str = "test";
const RESOURCE: &str = "https://resource.example/mcp";
const REDIRECT: &str = "https://client.example/callback";
// RFC 7636 appendix B vector.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

fn setup() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    clients::init(&conn).unwrap();
    conn.execute(
        "INSERT INTO door_oauth_servers VALUES(?1,'https://issuer.example',?2)",
        params![DOOR, RESOURCE],
    )
    .unwrap();
    add_app(&conn, "public", "public", None);
    add_app(&conn, "other", "public", None);
    add_app(&conn, "confidential", "confidential", Some("owner-secret"));
    conn
}
fn add_app(conn: &Connection, id: &str, kind: &str, credential: Option<&str>) {
    conn.execute("INSERT INTO door_oauth_apps(id,door,name,redirects,kind,secret_hash,created_ms,updated_ms) VALUES(?1,?2,'<untrusted>',?3,?4,?5,0,0)",
        params![id,DOOR,json!([REDIRECT]).to_string(),kind,credential.map(hash)]).unwrap();
}
fn authorization(app: &str) -> Value {
    json!({"client_id":app,"redirect_uri":REDIRECT,"resource":RESOURCE,"response_type":"code",
        "state":"client-state","scope":clients::SCOPES.join(" "),"code_challenge_method":"S256","code_challenge":CHALLENGE})
}
fn call(conn: &mut Connection, command: &str, args: &Value) -> Value {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    match operate(&tx, DOOR, command, args) {
        Ok(value) => {
            tx.commit().unwrap();
            value
        }
        Err(e) => {
            tx.rollback().unwrap();
            error(&e.to_string())
        }
    }
}
fn start(conn: &mut Connection, id: &str) -> Value {
    let pending = call(conn, "oauth.authorize-begin", &authorization(id));
    assert!(pending["error"].is_null(), "{pending}");
    pending
}
fn issue_login(conn: &Connection, pending: &Value) -> String {
    let code = secret("login");
    conn.execute(
        "UPDATE door_oauth_transactions SET login_hash=?2,login_expires=?3 WHERE id=?1",
        params![
            pending["transaction"].as_str().unwrap(),
            hash(&code),
            Store::now_ms() + LOGIN_MS
        ],
    )
    .unwrap();
    code
}
fn logged_in(conn: &mut Connection, id: &str) -> Value {
    let pending = start(conn, id);
    let login_code = issue_login(conn, &pending);
    let result = call(
        conn,
        "oauth.login-submit",
        &json!({"transaction":pending["transaction"],"csrf":pending["csrf"],"login_code":login_code,"source":"127.0.0.1"}),
    );
    assert_eq!(result["next"], "consent", "{result}");
    result
}
fn code(conn: &mut Connection, id: &str) -> Value {
    let login = logged_in(conn, id);
    let code = call(
        conn,
        "oauth.consent-submit",
        &json!({"transaction":login["transaction"],"consent_token":login["consent_token"],"scopes":["fleet:read"],"ships":["visible"]}),
    );
    assert!(code["code"].is_string(), "{code}");
    code
}
fn exchange(id: &str, code: &Value) -> Value {
    let mut args = json!({"grant_type":"authorization_code","client_id":id,"code":code["code"],"redirect_uri":REDIRECT,"resource":RESOURCE,"code_verifier":VERIFIER});
    if id == "confidential" {
        args["client_secret"] = json!("owner-secret");
    }
    args
}
fn refresh(id: &str, token: &Value) -> Value {
    json!({"grant_type":"refresh_token","client_id":id,"resource":RESOURCE,"refresh_token":token["refresh_token"]})
}
fn admitted(conn: &Connection, token: &Value, resource: Option<&str>) -> bool {
    clients::resolve(
        conn,
        DOOR,
        token["access_token"].as_str().unwrap(),
        &["fleet:read"],
        resource,
    )
    .unwrap()
    .is_ok()
}

#[test]
fn lifecycle_subset_audience_live_grant_and_revocation() {
    let mut conn = setup();
    let code = code(&mut conn, "public");
    assert_eq!(code["iss"], "https://issuer.example");
    assert_eq!(code["state"], "client-state");
    let token = call(&mut conn, "oauth.token", &exchange("public", &code));
    assert_eq!(token["scope"], "fleet:read");
    assert_eq!(token["expires_in"], 900);
    assert!(admitted(&conn, &token, Some(RESOURCE)));
    assert!(!admitted(&conn, &token, Some("https://wrong.example/mcp")));
    assert!(!admitted(&conn, &token, None));
    assert!(
        clients::resolve(
            &conn,
            "other",
            token["access_token"].as_str().unwrap(),
            &[],
            Some(RESOURCE)
        )
        .unwrap()
        .is_err()
    );
    assert!(matches!(
        clients::resolve(
            &conn,
            DOOR,
            token["access_token"].as_str().unwrap(),
            &["mail:read"],
            Some(RESOURCE)
        )
        .unwrap(),
        Err(Denial::Scope)
    ));
    conn.execute("UPDATE door_clients SET paused=1", [])
        .unwrap();
    assert!(!admitted(&conn, &token, Some(RESOURCE)));
    conn.execute("UPDATE door_clients SET paused=0", [])
        .unwrap();
    let rotated = call(&mut conn, "oauth.token", &refresh("public", &token));
    assert!(admitted(&conn, &rotated, Some(RESOURCE)));
    assert_eq!(
        call(
            &mut conn,
            "oauth.revoke",
            &json!({"client_id":"other","token":rotated["refresh_token"]})
        ),
        json!({})
    );
    assert!(admitted(&conn, &rotated, Some(RESOURCE)));
    assert_eq!(
        call(
            &mut conn,
            "oauth.revoke",
            &json!({"client_id":"public","token":rotated["refresh_token"],"token_type_hint":"unknown"})
        ),
        json!({})
    );
    assert!(!admitted(&conn, &rotated, Some(RESOURCE)));
    assert!(!admitted(&conn, &token, Some(RESOURCE)));
    assert_eq!(
        call(
            &mut conn,
            "oauth.revoke",
            &json!({"client_id":"public","token":"unknown"})
        ),
        json!({})
    );
}

#[test]
fn pkce_exact_redirect_client_resource_and_replay() {
    let mut conn = setup();
    let mut args = authorization("public");
    args["code_challenge_method"] = json!("plain");
    assert_eq!(
        call(&mut conn, "oauth.authorize-begin", &args)["error"],
        "invalid_request"
    );
    args = authorization("public");
    args["redirect_uri"] = json!(format!("{REDIRECT}/"));
    assert_eq!(
        call(&mut conn, "oauth.authorize-begin", &args)["error"],
        "invalid_request"
    );
    args = authorization("public");
    args["scope"] = json!("unknown:scope");
    assert_eq!(
        call(&mut conn, "oauth.authorize-begin", &args)["error"],
        "invalid_scope"
    );
    let code = code(&mut conn, "public");
    for (key, value, err) in [
        (
            "code_verifier",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "invalid_grant",
        ),
        ("client_id", "other", "invalid_grant"),
        (
            "redirect_uri",
            "https://client.example/other",
            "invalid_grant",
        ),
        ("resource", "https://other.example/mcp", "invalid_target"),
    ] {
        let mut args = exchange("public", &code);
        args[key] = json!(value);
        assert_eq!(call(&mut conn, "oauth.token", &args)["error"], err, "{key}");
    }
    let args = exchange("public", &code);
    assert!(call(&mut conn, "oauth.token", &args)["access_token"].is_string());
    assert_eq!(
        call(&mut conn, "oauth.token", &args)["error"],
        "invalid_grant"
    );
}

#[test]
fn confidential_post_basic_and_constant_time_secret_authentication() {
    let mut conn = setup();
    let first = code(&mut conn, "confidential");
    let mut args = exchange("confidential", &first);
    args["client_secret"] = json!("wrong");
    assert_eq!(
        call(&mut conn, "oauth.token", &args)["error"],
        "invalid_client"
    );
    args["client_secret"] = json!("owner-secret");
    assert!(call(&mut conn, "oauth.token", &args)["access_token"].is_string());
    let second = code(&mut conn, "confidential");
    args = exchange("confidential", &second);
    args.as_object_mut().unwrap().remove("client_id");
    args.as_object_mut().unwrap().remove("client_secret");
    args["client_secret_basic"] = json!(STANDARD.encode("confidential:owner%2Dsecret"));
    assert!(call(&mut conn, "oauth.token", &args)["access_token"].is_string());
    args["client_id"] = json!("confidential");
    assert_eq!(
        call(&mut conn, "oauth.token", &args)["error"],
        "invalid_client"
    );
    assert_eq!(form_decode("x%3Ay+z").unwrap(), "x:y z");
    assert!(form_decode("%0").is_err());
}

#[test]
fn refresh_reuse_revokes_all_siblings_and_grant_revocation_is_live() {
    let mut conn = setup();
    let c = code(&mut conn, "public");
    let first = call(&mut conn, "oauth.token", &exchange("public", &c));
    let second = call(&mut conn, "oauth.token", &refresh("public", &first));
    assert!(admitted(&conn, &first, Some(RESOURCE)));
    assert_eq!(
        call(&mut conn, "oauth.token", &refresh("public", &first))["error"],
        "invalid_grant"
    );
    assert!(!admitted(&conn, &first, Some(RESOURCE)));
    assert!(!admitted(&conn, &second, Some(RESOURCE)));
    assert_eq!(
        call(&mut conn, "oauth.token", &refresh("public", &second))["error"],
        "invalid_grant"
    );
    let c = code(&mut conn, "other");
    let other = call(&mut conn, "oauth.token", &exchange("other", &c));
    assert!(admitted(&conn, &other, Some(RESOURCE)));
    conn.execute("UPDATE door_clients SET revoked_ms=1", [])
        .unwrap();
    assert!(!admitted(&conn, &other, Some(RESOURCE)));
    assert_eq!(
        call(&mut conn, "oauth.token", &refresh("other", &other))["error"],
        "invalid_grant"
    );
}

#[test]
fn login_is_transaction_bound_single_use_csrf_and_consent_is_explicit() {
    let mut conn = setup();
    let first = start(&mut conn, "public");
    let second = start(&mut conn, "other");
    let login = issue_login(&conn, &first);
    let mut args = json!({"transaction":second["transaction"],"csrf":second["csrf"],"login_code":login,"source":"source"});
    assert_eq!(
        call(&mut conn, "oauth.login-submit", &args)["error"],
        "access_denied"
    );
    args["transaction"] = first["transaction"].clone();
    assert_eq!(
        call(&mut conn, "oauth.login-submit", &args)["error"],
        "invalid_grant"
    );
    args["csrf"] = first["csrf"].clone();
    let consent = call(&mut conn, "oauth.login-submit", &args);
    assert!(consent["consent_token"].is_string());
    assert_eq!(
        call(&mut conn, "oauth.login-submit", &args)["error"],
        "invalid_grant"
    );
    let mut approval = json!({"transaction":first["transaction"],"consent_token":first["csrf"],"scopes":["fleet:read"],"ships":[]});
    assert_eq!(
        call(&mut conn, "oauth.consent-submit", &approval)["error"],
        "access_denied"
    );
    approval["consent_token"] = consent["consent_token"].clone();
    approval["scopes"] = json!(["unknown"]);
    assert_eq!(
        call(&mut conn, "oauth.consent-submit", &approval)["error"],
        "invalid_scope"
    );
    approval["scopes"] = json!([]);
    let code = call(&mut conn, "oauth.consent-submit", &approval);
    assert!(code["code"].is_string());
    assert_eq!(
        call(&mut conn, "oauth.consent-submit", &approval)["error"],
        "invalid_grant"
    );
    let token = call(&mut conn, "oauth.token", &exchange("public", &code));
    assert_eq!(token["scope"], "");
    let grants: i64 = conn
        .query_row("SELECT COUNT(*) FROM door_clients", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        grants, 1,
        "registration/login alone give no grant to other client"
    );
}

#[test]
fn login_limits_are_per_application_and_source_and_expiry_is_enforced() {
    let mut conn = setup();
    let pending = start(&mut conn, "public");
    let login = issue_login(&conn, &pending);
    let args = json!({"transaction":pending["transaction"],"csrf":pending["csrf"],"login_code":"wrong","source":"same-source"});
    for _ in 0..5 {
        assert_eq!(
            call(&mut conn, "oauth.login-submit", &args)["error"],
            "access_denied"
        );
    }
    assert_eq!(
        call(&mut conn, "oauth.login-submit", &args)["error"],
        "slow_down"
    );
    let other = start(&mut conn, "other");
    let other_login = issue_login(&conn, &other);
    let mut other_args = json!({"transaction":other["transaction"],"csrf":other["csrf"],"login_code":other_login,"source":"same-source"});
    assert_eq!(
        call(&mut conn, "oauth.login-submit", &other_args)["error"],
        "slow_down"
    );
    other_args["source"] = json!("new-source");
    assert_eq!(
        call(&mut conn, "oauth.login-submit", &other_args)["next"],
        "consent"
    );
    let mut same_app = args.clone();
    same_app["source"] = json!("new-source");
    same_app["login_code"] = json!(login);
    assert_eq!(
        call(&mut conn, "oauth.login-submit", &same_app)["error"],
        "slow_down"
    );
    conn.execute("UPDATE door_oauth_failures SET window_ms=0", [])
        .unwrap();
    conn.execute("UPDATE door_oauth_transactions SET login_expires=0", [])
        .unwrap();
    assert_eq!(
        call(&mut conn, "oauth.login-submit", &same_app)["error"],
        "access_denied"
    );
    conn.execute("UPDATE door_oauth_transactions SET expires_ms=0", [])
        .unwrap();
    assert_eq!(
        call(&mut conn, "oauth.login-submit", &same_app)["error"],
        "invalid_grant"
    );
    let c = code(&mut conn, "public");
    conn.execute("UPDATE door_oauth_codes SET expires_ms=0", [])
        .unwrap();
    assert_eq!(
        call(&mut conn, "oauth.token", &exchange("public", &c))["error"],
        "invalid_grant"
    );
    let c = code(&mut conn, "public");
    let token = call(&mut conn, "oauth.token", &exchange("public", &c));
    conn.execute("UPDATE door_oauth_access SET expires_ms=0", [])
        .unwrap();
    assert!(!admitted(&conn, &token, Some(RESOURCE)));
}

#[test]
fn restart_and_two_connection_code_and_refresh_races() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("oauth.sqlite");
    let conn = setup();
    // Use SQLite backup-free SQL initialization on a real file.
    let mut disk = Connection::open(&path).unwrap();
    clients::init(&disk).unwrap();
    disk.execute(
        "INSERT INTO door_oauth_servers VALUES(?1,'https://issuer.example',?2)",
        params![DOOR, RESOURCE],
    )
    .unwrap();
    add_app(&disk, "public", "public", None);
    drop(conn);
    let pending = start(&mut disk, "public");
    let login = issue_login(&disk, &pending);
    drop(disk);
    let mut disk = Connection::open(&path).unwrap();
    let consent = call(
        &mut disk,
        "oauth.login-submit",
        &json!({"transaction":pending["transaction"],"csrf":pending["csrf"],"login_code":login,"source":"source"}),
    );
    let c = call(
        &mut disk,
        "oauth.consent-submit",
        &json!({"transaction":pending["transaction"],"consent_token":consent["consent_token"],"scopes":["fleet:read"],"ships":[]}),
    );
    drop(disk);
    let race = |args: Value| {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads = (0..2)
            .map(|_| {
                let path = path.clone();
                let args = args.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut conn = Connection::open(path).unwrap();
                    conn.busy_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                    barrier.wait();
                    call(&mut conn, "oauth.token", &args)
                })
            })
            .collect::<Vec<_>>();
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>()
    };
    let codes = race(exchange("public", &c));
    assert_eq!(
        codes
            .iter()
            .filter(|v| v["access_token"].is_string())
            .count(),
        1,
        "{codes:?}"
    );
    assert_eq!(
        codes
            .iter()
            .filter(|v| v["error"] == "invalid_grant")
            .count(),
        1
    );
    let token = codes
        .iter()
        .find(|v| v["access_token"].is_string())
        .unwrap();
    let tokens = race(refresh("public", token));
    assert_eq!(
        tokens
            .iter()
            .filter(|v| v["access_token"].is_string())
            .count(),
        1,
        "{tokens:?}"
    );
    assert_eq!(
        tokens
            .iter()
            .filter(|v| v["error"] == "invalid_grant")
            .count(),
        1
    );
    let conn = Connection::open(&path).unwrap();
    for token in tokens.iter().filter(|v| v["access_token"].is_string()) {
        assert!(
            !admitted(&conn, token, Some(RESOURCE)),
            "losing refresh replay revokes winning family"
        );
    }
}

#[test]
fn metadata_uris_no_plaintext_secrets_and_door_re_registration() {
    let mut conn = setup();
    let metadata = call(&mut conn, "oauth.metadata", &json!({}));
    assert_eq!(
        metadata["code_challenge_methods_supported"],
        json!(["S256"])
    );
    assert_eq!(
        metadata["authorization_response_iss_parameter_supported"],
        true
    );
    for bad in [
        "http://example.com/cb",
        "https://user@example.com/cb",
        "https://example.com/cb#fragment",
        "https://example.com\\@evil.test",
        "https://example.com/\n",
        "https://[::1]:not-port/cb",
    ] {
        assert!(uri(bad).is_err(), "{bad}");
    }
    for good in [
        REDIRECT,
        "http://127.0.0.1:8765/cb",
        "http://[::1]:1234/cb",
        "http://localhost/cb",
    ] {
        assert!(uri(good).is_ok(), "{good}");
    }
    let c = code(&mut conn, "public");
    let token = call(&mut conn, "oauth.token", &exchange("public", &c));
    // All credential columns contain only SHA-256 digests; durable transaction
    // bindings contain neither login credentials nor server-issued CSRF values.
    for table in [
        "door_oauth_codes",
        "door_oauth_refresh",
        "door_oauth_access",
    ] {
        let length: i64 = conn
            .query_row(&format!("SELECT MIN(length(hash)) FROM {table}"), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(length, 32);
    }
    let digest: Vec<u8> = conn
        .query_row(
            "SELECT secret_hash FROM door_oauth_apps WHERE id='confidential'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(digest, hash("owner-secret"));
    let audit: String = conn
        .query_row(
            "SELECT group_concat(event||reason) FROM door_client_audit",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!audit.contains("pv_") && !audit.contains("owner-secret"));
    revoke_door(&conn, DOOR).unwrap();
    assert!(!admitted(&conn, &token, Some(RESOURCE)));
    assert_eq!(
        call(&mut conn, "oauth.authorize-begin", &authorization("public"))["error"],
        "invalid_client"
    );
}
