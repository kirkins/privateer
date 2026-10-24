// Real owner PTY, CLI, registry, supervisor and adapter process. This is core
// protocol coverage, not an HTTP/browser/ChatGPT interoperability claim.
const OAUTH_RESOURCE: &str = "https://resource.example/mcp";
const OAUTH_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const OAUTH_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

fn oauth_wire(fleet: &Fleet, command: &str, args: Value, auth: Option<Value>) -> Value {
    use std::io::Write;
    let id = uuid::Uuid::new_v4().to_string();
    let mut request = json!({"type":"request","id":id,"command":command,"args":args});
    if let Some(auth) = auth {
        request["auth"] = auth;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(fleet.root.join("requests.jsonl"))
        .unwrap();
    writeln!(file, "{request}").unwrap();
    fleet.wait("OAuth response", |entries| response(entries, &id).cloned())
}
fn oauth_rpc(fleet: &Fleet, command: &str, args: Value) -> Value {
    let response = oauth_wire(fleet, command, args, None);
    assert_eq!(response["ok"], true, "{response}");
    response["result"].clone()
}
fn oauth_begin(fleet: &Fleet, id: &str) -> Value {
    oauth_rpc(
        fleet,
        "oauth.authorize-begin",
        json!({"client_id":id,"redirect_uri":"https://client.example/callback",
        "resource":OAUTH_RESOURCE,"response_type":"code","scope":ALL_DELEGATED_SCOPES.replace(','," "),
        "state":"client-state","code_challenge_method":"S256","code_challenge":OAUTH_CHALLENGE}),
    )
}
fn oauth_owner_code(fleet: &Fleet, pending: &Value) -> String {
    let (out, tty) = client_terminal(
        fleet,
        &[
            "door",
            "oauth-client",
            "login-code",
            pending["transaction"].as_str().unwrap(),
        ],
        "login-code",
    );
    assert_eq!(out["transaction"], pending["transaction"]);
    let code = tty
        .lines()
        .find_map(|l| l.strip_prefix("Login code (shown once): "))
        .unwrap()
        .trim()
        .to_owned();
    assert!(!out.to_string().contains(&code));
    code
}
fn oauth_consent(fleet: &Fleet, pending: &Value, login_code: &str) -> Value {
    let login = oauth_rpc(
        fleet,
        "oauth.login-submit",
        json!({"transaction":pending["transaction"],"csrf":pending["csrf"],"login_code":login_code,"source":"127.0.0.1"}),
    );
    assert_eq!(login["next"], "consent", "{login}");
    assert_eq!(login["default_scopes"], json!([]));
    assert_eq!(login["default_ships"], json!([]));
    oauth_rpc(
        fleet,
        "oauth.consent-submit",
        json!({"transaction":pending["transaction"],"consent_token":login["consent_token"],"scopes":["fleet:read"],"ships":["ops"]}),
    )
}
fn oauth_token_args(id: &str, code: &Value) -> Value {
    json!({"grant_type":"authorization_code","client_id":id,"code":code["code"],"redirect_uri":"https://client.example/callback","resource":OAUTH_RESOURCE,"code_verifier":OAUTH_VERIFIER})
}

#[test]
fn oauth_real_owner_cli_core_restart_subset_admission_refresh_and_revoke() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    client_terminal(
        &fleet,
        &[
            "door",
            "oauth-client",
            "configure",
            "fake",
            "--issuer",
            "https://issuer.example",
            "--resource",
            OAUTH_RESOURCE,
        ],
        "configure",
    );
    let (app, _) = client_terminal(
        &fleet,
        &[
            "door",
            "oauth-client",
            "add",
            "fake",
            "--name",
            "<untrusted application>",
            "--redirect-uri",
            "https://client.example/callback",
        ],
        "add",
    );
    let id = app["client_id"].as_str().unwrap();
    let applications: Value =
        serde_json::from_str(&fleet.ok(&["door", "oauth-client", "list", "fake"])).unwrap();
    assert_eq!(applications["applications"].as_array().unwrap().len(), 1);
    assert_eq!(app["grants"], json!([]));
    let grants: Value = serde_json::from_str(&fleet.ok(&["door", "client", "list"])).unwrap();
    assert_eq!(
        grants["clients"],
        json!([]),
        "registration gives no fleet grant"
    );
    let agent = fleet
        .command(&fleet.root)
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "crew")
        .args(["door", "oauth-client", "list"])
        .output()
        .unwrap();
    assert!(!agent.status.success());
    let job = fleet
        .command(&fleet.root)
        .env("PRIVATEER_JOB", "test-job")
        .args(["door", "oauth-client", "list"])
        .output()
        .unwrap();
    assert!(!job.status.success());
    let home = fleet.home("ops");
    assert!(
        fleet
            .refused(&[
                "--home",
                home.to_str().unwrap(),
                "door.oauth-client.add",
                "--json",
                "{}"
            ])
            .contains("unknown command")
    );
    let supervisor = fleet.supervise();
    fleet.wait("OAuth hello", |e| e.first().cloned());
    let meta = oauth_rpc(&fleet, "oauth.metadata", json!({}));
    assert_eq!(meta["issuer"], "https://issuer.example");
    assert_eq!(meta["protected_resource"]["resource"], OAUTH_RESOURCE);
    assert_eq!(
        oauth_wire(&fleet, "oauth.unknown", json!({}), None)["ok"],
        false
    );
    let pending = oauth_begin(&fleet, id);
    let login_code = oauth_owner_code(&fleet, &pending);
    // Standalone credentials are not fleet bearer credentials.
    assert_eq!(
        delegated_exchange(&fleet, &login_code, "ships", json!({}))["ok"],
        false
    );
    stop(supervisor);
    fs::write(fleet.root.join("requests.jsonl"), "").unwrap();
    let supervisor = fleet.supervise();
    let code = oauth_consent(&fleet, &pending, &login_code);
    assert_eq!(code["iss"], "https://issuer.example");
    assert_eq!(code["state"], "client-state");
    let mut args = oauth_token_args(id, &code);
    args["code_verifier"] = json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(
        oauth_rpc(&fleet, "oauth.token", args)["error"],
        "invalid_grant"
    );
    let tokens = oauth_rpc(&fleet, "oauth.token", oauth_token_args(id, &code));
    assert_eq!(tokens["scope"], "fleet:read");
    assert_eq!(tokens["expires_in"], 900);
    let token = tokens["access_token"].as_str().unwrap();
    assert_eq!(
        delegated_exchange(&fleet, token, "ships", json!({}))["ok"],
        false,
        "OAuth requires audience"
    );
    assert_eq!(
        oauth_wire(
            &fleet,
            "ships",
            json!({}),
            Some(json!({"token":token,"resource":"https://wrong.example/mcp"}))
        )["ok"],
        false
    );
    let auth = json!({"token":token,"resource":OAUTH_RESOURCE});
    let ships = oauth_wire(&fleet, "ships", json!({}), Some(auth.clone()));
    assert_eq!(ships["ok"], true, "{ships}");
    assert_eq!(ships["result"]["provenance"]["client"], id);
    assert_ne!(ships["result"]["provenance"]["grant"], id);
    let grant = ships["result"]["provenance"]["grant"].as_str().unwrap();
    assert!(
        fleet
            .refused(&["door", "client", "rotate", grant])
            .contains("OAuth grants use refresh rotation")
    );
    assert_eq!(
        ships["result"]["ships"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["ops"],
        "ship restrictions survive OAuth"
    );
    assert_eq!(
        oauth_wire(&fleet, "held", json!({}), Some(auth.clone()))["ok"],
        false,
        "all-scopes request is not all-scopes consent"
    );
    assert_eq!(
        oauth_wire(&fleet, "oauth.consent-submit", json!({}), Some(auth))["ok"],
        false,
        "a bearer cannot self-approve"
    );
    let refreshed = oauth_rpc(
        &fleet,
        "oauth.token",
        json!({"grant_type":"refresh_token","client_id":id,"refresh_token":tokens["refresh_token"],"resource":OAUTH_RESOURCE}),
    );
    assert!(refreshed["access_token"].is_string(), "{refreshed}");
    assert_eq!(
        oauth_rpc(&fleet, "oauth.token", oauth_token_args(id, &code))["error"],
        "invalid_grant"
    );
    assert_eq!(
        oauth_wire(&fleet, "ships", json!({}), Some(json!({"token":refreshed["access_token"],"resource":OAUTH_RESOURCE})))["ok"],
        false,
        "authorization-code replay revokes the refreshed family across real processes"
    );
    assert_eq!(
        oauth_rpc(
            &fleet,
            "oauth.revoke",
            json!({"client_id":id,"token":refreshed["refresh_token"]})
        ),
        json!({})
    );
    assert_eq!(
        oauth_wire(
            &fleet,
            "ships",
            json!({}),
            Some(json!({"token":refreshed["access_token"],"resource":OAUTH_RESOURCE}))
        )["ok"],
        false
    );
    stop(supervisor);
    let mut db = fs::read(fleet.root.join("state/fleet.sqlite")).unwrap();
    if let Ok(wal) = fs::read(fleet.root.join("state/fleet.sqlite-wal")) {
        db.extend(wal);
    }
    let log = fs::read(fleet.root.join("state/doors/fake/door.log")).unwrap();
    let audit = fleet.ok(&["door", "client", "audit"]);
    for secret in [
        login_code.as_str(),
        code["code"].as_str().unwrap(),
        token,
        tokens["refresh_token"].as_str().unwrap(),
        refreshed["refresh_token"].as_str().unwrap(),
    ] {
        assert!(
            !db.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "secret in registry"
        );
        assert!(
            !log.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "secret in supervisor log"
        );
        assert!(!audit.contains(secret), "secret in audit");
    }
}

#[test]
fn oauth_real_confidential_client_secret_post_basic_rotation_and_removal() {
    use base64::Engine;
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    client_terminal(
        &fleet,
        &[
            "door",
            "oauth-client",
            "configure",
            "fake",
            "--issuer",
            "https://issuer.example",
            "--resource",
            OAUTH_RESOURCE,
        ],
        "configure",
    );
    let (app, tty) = client_terminal(
        &fleet,
        &[
            "door",
            "oauth-client",
            "add",
            "fake",
            "--name",
            "Confidential",
            "--redirect-uri",
            "https://client.example/callback",
            "--type",
            "confidential",
        ],
        "add",
    );
    let id = app["client_id"].as_str().unwrap();
    let secret = tty
        .lines()
        .find_map(|l| l.strip_prefix("Client secret (shown once): "))
        .unwrap()
        .trim()
        .to_owned();
    assert!(!app.to_string().contains(&secret));
    let supervisor = fleet.supervise();
    let pending = oauth_begin(&fleet, id);
    let login = oauth_owner_code(&fleet, &pending);
    let code = oauth_consent(&fleet, &pending, &login);
    let mut args = oauth_token_args(id, &code);
    args["client_secret"] = json!("wrong");
    assert_eq!(
        oauth_rpc(&fleet, "oauth.token", args.clone())["error"],
        "invalid_client"
    );
    args["client_secret"] = json!(secret);
    let token = oauth_rpc(&fleet, "oauth.token", args);
    assert!(token["access_token"].is_string());
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
    let refreshed = oauth_rpc(
        &fleet,
        "oauth.token",
        json!({"grant_type":"refresh_token","client_secret_basic":basic,"refresh_token":token["refresh_token"],"resource":OAUTH_RESOURCE}),
    );
    assert!(refreshed["access_token"].is_string(), "{refreshed}");
    let (_, tty) = client_terminal(
        &fleet,
        &["door", "oauth-client", "rotate-secret", id],
        "rotate-secret",
    );
    let rotated = tty
        .lines()
        .find_map(|l| l.strip_prefix("Client secret (shown once): "))
        .unwrap()
        .trim()
        .to_owned();
    assert_ne!(rotated, secret);
    assert_eq!(
        oauth_rpc(
            &fleet,
            "oauth.token",
            json!({"grant_type":"refresh_token","client_id":id,"client_secret":secret,"refresh_token":refreshed["refresh_token"],"resource":OAUTH_RESOURCE})
        )["error"],
        "invalid_client"
    );
    assert_eq!(
        oauth_wire(
            &fleet,
            "ships",
            json!({}),
            Some(json!({"token":refreshed["access_token"],"resource":OAUTH_RESOURCE}))
        )["ok"],
        false
    );
    client_terminal(&fleet, &["door", "oauth-client", "remove", id], "remove");
    assert_eq!(oauth_begin(&fleet, id)["error"], "invalid_client");
    stop(supervisor);
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    for s in [secret.as_str(), rotated.as_str()] {
        let raw = fs::read(fleet.root.join("state/fleet.sqlite")).unwrap();
        assert!(!raw.windows(s.len()).any(|w| w == s.as_bytes()));
    }
    let hashes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM door_oauth_apps WHERE secret_hash IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hashes, 0, "removed client clears hash");
}
