//! Local, grant-bound chat approval challenges. No client prose is authority.
//! The registry writer lock is held by scoped admission through the home commit;
//! the home transaction consumes the challenge with the native reply/approval.
use super::scoped::{DelegatedContext, Denial};
use crate::store::Store;
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

const VERSION: u64 = 4;
const LIFETIME_MS: i64 = 600_000;

pub(super) fn validate_args(args: &Value) -> Result<(), Denial> {
    let fields = args.as_object().ok_or(Denial::Arguments)?;
    if fields.keys().any(|k| {
        ![
            "mode",
            "id",
            "ship",
            "digest",
            "version",
            "action",
            "option_id",
            "note",
        ]
        .contains(&k.as_str())
    }) || !args["id"]
        .as_str()
        .is_some_and(|s| !s.is_empty() && s.len() <= 128)
        || ["ship", "digest", "action", "option_id", "note"]
            .iter()
            .any(|k| {
                fields
                    .get(*k)
                    .is_some_and(|v| v.as_str().is_none_or(|s| s.len() > 4096))
            })
    {
        return Err(Denial::Arguments);
    }
    match args["mode"].as_str() {
        Some("prepare")
            if ["digest", "version", "action", "option_id", "note"]
                .iter()
                .all(|k| !fields.contains_key(*k)) =>
        {
            Ok(())
        }
        Some("answer")
            if args["digest"].as_str().is_some_and(|s| s.len() == 64)
                && args["version"].as_u64().is_some()
                && ((matches!(args["action"].as_str(), Some("approve" | "deny"))
                    && !fields.contains_key("option_id"))
                    || (!fields.contains_key("action")
                        && args["option_id"]
                            .as_str()
                            .is_some_and(|s| !s.is_empty() && s.len() <= 128))) =>
        {
            Ok(())
        }
        _ => Err(Denial::Arguments),
    }
}

fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS door_approval_challenges(
         digest TEXT PRIMARY KEY, version INTEGER NOT NULL, grant_id TEXT NOT NULL,
         resource TEXT NOT NULL, ship TEXT NOT NULL, home TEXT NOT NULL,
         ask_id TEXT NOT NULL, content TEXT NOT NULL, expires_ms INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS door_approval_challenges_expiry ON door_approval_challenges(expires_ms);
         CREATE TABLE IF NOT EXISTS captain_approval_door_provenance(
         approval_id TEXT PRIMARY KEY REFERENCES captain_approvals(id) ON DELETE CASCADE,
         provenance TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS captain_approval_door_sentences(
         approval_id TEXT PRIMARY KEY REFERENCES captain_approvals(id) ON DELETE CASCADE,
         sentence TEXT NOT NULL);",
    )?;
    Ok(())
}

// serde_json's default Map is key-sorted, including nested objects. Only stable
// ask content participates: read cursors and presentation annotations do not.
fn content(ask: &Value) -> String {
    crate::core::hash(&json!({"id":ask["id"], "from":ask["from"], "to":ask["to"],
        "kind":ask["kind"], "task_id":ask["task_id"], "request_kind":ask["request_kind"],
        "body":ask["body"], "payload":ask["payload"], "created_ms":ask["created_ms"],
        "deadline_ms":ask["deadline_ms"], "correlation":ask["correlation"],
        "because_of":ask["because_of"], "urgency":ask["urgency"]}))
}

fn eligible(store: &Store, ask: &Value, context: &DelegatedContext) -> Result<()> {
    ensure!(context.has("approval:answer"), Denial::Scope);
    let ship = ask["from"].as_str().unwrap_or_default();
    ensure!(context.allows(ship), Denial::Ship);
    // Desk copies and linked origins are never authoritative here.
    ensure!(ask["delivery"] != "received", Denial::ApprovalUnavailable);
    ensure!(
        ask["to"] == crate::mail::CAPTAIN
            && matches!(ask["kind"].as_str(), Some("request" | "decision")),
        Denial::ApprovalUnavailable
    );
    // Only ordinary declared step asks are reviewed for this path. mail.send
    // normalizes decision labels into choices and may add a topic (about).
    // Other payload extensions, including future categories, fail closed.
    ensure!(
        ask["request_kind"].is_null()
            && !["type", "privileged", "needs_hands", "project_consent"]
                .iter()
                .any(|key| ask["payload"].get(*key).is_some())
            && ask["payload"].get("approves").is_some(),
        Denial::ConfirmationRequired
    );
    ensure!(
        ask["payload"].as_object().is_some_and(|fields| fields
            .keys()
            .all(|k| matches!(k.as_str(), "approves" | "decision" | "choices" | "about"))),
        Denial::ApprovalUnavailable
    );
    ensure!(ask["state"] == "open", Denial::ApprovalStale);
    let task = ask["task_id"].as_str().unwrap_or_default();
    ensure!(
        crate::captain_approvals::open_task(store, ship, task),
        Denial::ApprovalStale
    );
    ensure!(
        ask["deadline_ms"]
            .as_i64()
            .is_none_or(|d| d > Store::now_ms()),
        Denial::ApprovalStale
    );
    let action = ask["payload"]["approves"]["action"]
        .as_str()
        .unwrap_or_default();
    ensure!(
        !action.is_empty()
            && action.chars().count() <= 300
            && !action.chars().any(char::is_control)
            && crate::block_ask::mask(action) == action,
        Denial::ConfirmationRequired
    );
    Ok(())
}

fn prepare(store: &Store, ask: &Value, context: &DelegatedContext) -> Result<Value> {
    prepare_bound(store, ask, context, None, &|| Ok(()))
}

fn prepare_bound(
    store: &Store,
    ask: &Value,
    context: &DelegatedContext,
    link: Option<&Value>,
    recheck: &dyn Fn() -> Result<(), Denial>,
) -> Result<Value> {
    init(store)?;
    let tx = store.write_transaction()?;
    let expected_ship = &ask["from"];
    let ask = crate::mail::find(store, ask["id"].as_str().unwrap_or_default())?
        .ok_or(Denial::ApprovalUnavailable)?;
    ensure!(ask["from"] == *expected_ship, Denial::ApprovalStale);
    eligible(store, &ask, context)?;
    let now = Store::now_ms();
    tx.execute(
        "DELETE FROM door_approval_challenges WHERE expires_ms<=?1",
        [now],
    )?;
    // One current challenge per grant/ask, and a hard per-home storage bound.
    tx.execute(
        "DELETE FROM door_approval_challenges WHERE grant_id=?1 AND resource=?2 AND ask_id=?3",
        params![context.client(), context.resource(), ask["id"].as_str()],
    )?;
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM door_approval_challenges", [], |r| {
        r.get(0)
    })?;
    ensure!(count < 4096, Denial::RetryCapacity);
    // Pre-mint the native reply identity and fix the grant expiry here, so
    // what the client displays is exactly what bearings/auto mode will read.
    // A delayed answer consumes the remaining TTL; it never extends consent.
    let ttl = crate::captain_approvals::step_ttl(&ask["payload"]["approves"]);
    let reply_id = uuid::Uuid::new_v4().to_string();
    let grant_expires = now + ttl as i64 * 1000;
    let grant = json!({"reply_id":reply_id,"command_prefix":ask["payload"]["approves"]["command_prefix"],
        "ttl_secs":ttl,"expires_ms":grant_expires,
        "sentence":crate::captain_approvals::chat_sentence(&reply_id,
            ask["id"].as_str().unwrap_or_default(), ask["from"].as_str().unwrap_or_default(),
            ask["task_id"].as_str().unwrap_or_default(), &crate::routes::utc_time(grant_expires),
            ask["payload"]["approves"]["action"].as_str().unwrap_or_default(),
            ask["payload"]["approves"]["command_prefix"].as_str())});
    // Admission blocks only an approving answer, never denial/holding. This
    // is advisory at prepare time; native insertion rechecks in the answer's
    // transaction, so newly filled capacity cannot create a false approval.
    let approve_unavailable = crate::captain_approvals::admission(
        store,
        ask["from"].as_str().unwrap_or_default(),
        grant["sentence"].as_str().unwrap_or_default(),
        now,
    )?
    .is_some();
    let mut prepared_content = json!({"ask":content(&ask),"grant":grant});
    if let Some(link) = link {
        prepared_content["link"] = link.clone();
    }
    let content = prepared_content.to_string();
    let home = store.home().to_string_lossy().into_owned();
    let expires = (now + LIFETIME_MS).min(grant_expires);
    let digest = crate::core::hash(&json!({"nonce":uuid::Uuid::new_v4().to_string(),
        "version":VERSION,"grant":context.client(),"resource":context.resource(),
        "home":home,"ship":ask["from"],"id":ask["id"],"content":content,"expires_ms":expires}));
    tx.execute(
        "INSERT INTO door_approval_challenges VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            digest,
            VERSION,
            context.client(),
            context.resource(),
            ask["from"].as_str(),
            home,
            ask["id"].as_str(),
            content,
            expires
        ],
    )?;
    let mut audit = json!({"ask_id":ask["id"],"version":VERSION});
    if let Some(link) = link {
        audit["link"] = link.clone();
    }
    store.with_delegated_audit(context, || store.audit("door.approval_prepared", &audit))?;
    recheck()?;
    ensure!(expires > Store::now_ms(), Denial::ApprovalStale);
    eligible(store, &ask, context)?;
    tx.commit()?;
    // Deliberately no body, captain prose, or unrestricted payload dump. The
    // bounded decision fields are the exact choices this challenge binds.
    let options: Vec<Value> = ask["payload"]["decision"]["options"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|o| {
            json!({"id":o["id"],"label":o["label"],"what_happens":o["what_happens"],
            "approves":o["approves"] == true})
        })
        .collect();
    Ok(
        json!({"digest":digest,"version":VERSION,"expires_ms":expires,"origin_home":home,
        "ship":ask["from"],"id":ask["id"],"kind":ask["kind"],
        "action_summary":ask["payload"]["approves"]["action"],"grant":grant,
        "approve_unavailable":approve_unavailable,
        "question":ask["payload"]["decision"]["question"],"options":options}),
    )
}

fn challenge_content(store: &Store, digest: &str) -> Result<Value> {
    let raw: Option<String> = store
        .connection()
        .query_row(
            "SELECT content FROM door_approval_challenges WHERE digest=?1 AND version=?2",
            params![digest, VERSION],
            |r| r.get(0),
        )
        .optional()?;
    let raw = raw.ok_or(Denial::ApprovalStale)?;
    Ok(serde_json::from_str(&raw)?)
}

/// Called only inside mail's IMMEDIATE write transaction. Validation and
/// deletion roll back if any subsequent native approval/reply write fails.
pub(crate) fn consume(store: &Store, args: &Value, context: &DelegatedContext) -> Result<Value> {
    let ask = crate::mail::find(store, args["id"].as_str().unwrap_or_default())?
        .ok_or(Denial::ApprovalUnavailable)?;
    eligible(store, &ask, context)?;
    ensure!(args["version"] == VERSION, Denial::ApprovalStale);
    let challenge = challenge_content(store, args["digest"].as_str().unwrap_or_default())?;
    // Linked prepare is deliberately not answerable in this phase, including
    // through a local door at the origin. Future linked receipts must validate
    // this binding, never treat a linked challenge as an ordinary local one.
    ensure!(challenge.get("link").is_none(), Denial::ApprovalUnavailable);
    ensure!(challenge["ask"] == content(&ask), Denial::ApprovalStale);
    let deleted = store.connection().execute(
        "DELETE FROM door_approval_challenges WHERE digest=?1 AND version=?2 AND grant_id=?3
         AND resource=?4 AND ship=?5 AND home=?6 AND ask_id=?7 AND content=?8 AND expires_ms>?9",
        params![
            args["digest"].as_str(),
            VERSION,
            context.client(),
            context.resource(),
            ask["from"].as_str(),
            store.home().to_string_lossy(),
            args["id"].as_str(),
            challenge.to_string(),
            Store::now_ms()
        ],
    )?;
    ensure!(deleted == 1, Denial::ApprovalStale);
    Ok(challenge["grant"].clone())
}

pub(super) fn request(
    args: &Value,
    context: &DelegatedContext,
    recheck: &dyn Fn() -> Result<(), Denial>,
) -> Result<Value> {
    ensure!(context.has("approval:answer"), Denial::Scope);
    if let Some(ship) = args["ship"].as_str() {
        ensure!(context.allows(ship), Denial::Ship);
    }
    let id = args["id"].as_str().ok_or(Denial::Arguments)?;
    // Visibility is checked before opening a home or searching for an id.
    for (ship, home) in super::members()?.into_iter().filter(|(s, _)| {
        context.allows(s) && args["ship"].as_str().is_none_or(|wanted| wanted == s)
    }) {
        let store = Store::open(&home)?;
        let Some(ask) = crate::mail::find(&store, id)? else {
            continue;
        };
        if ask["from"] != ship || ask["delivery"] == "received" {
            continue;
        }
        eligible(&store, &ask, context)?;
        if args["mode"] == "prepare" {
            let result = prepare(&store, &ask, context)?;
            recheck()?;
            return Ok(result);
        }
        init(&store)?;
        let challenge = challenge_content(&store, args["digest"].as_str().unwrap_or_default())?;
        // Report changed choices as stale even if the selected option was
        // removed. The transactional consume repeats this check before commit.
        ensure!(
            args["version"] == VERSION && challenge["ask"] == content(&ask),
            Denial::ApprovalStale
        );
        let (body, answer, approves) = if ask["kind"] == "decision" {
            let option = args["option_id"].as_str().ok_or(Denial::Arguments)?;
            let choice = ask["payload"]["decision"]["options"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|o| o["id"] == option)
                .ok_or(Denial::Arguments)?;
            ("", json!({"option_id":option}), choice["approves"] == true)
        } else {
            match args["action"].as_str() {
                Some("approve") => ("yes", Value::Null, true),
                Some("deny") => ("no", Value::Null, false),
                _ => return Err(Denial::Arguments.into()),
            }
        };
        let reply = json!({"id":challenge["grant"]["reply_id"],"from":crate::mail::CAPTAIN,
            "to":ship,"kind":"reply","correlation":id,"body":body,
            "payload":{"answer":answer,"chat_door":true}});
        let result =
            crate::mail::send_door_approval(&store, &reply, args, context, approves, recheck)?;
        return Ok(json!({"id":id,"ship":ship,"answered_via":"chat-door","mail":result}));
    }
    Err(Denial::ApprovalUnavailable.into())
}

/// Receiving linked gateway: a selected origin only, never search other homes
/// or route onward. The binding is a core-owned preparation receipt committed
/// with the challenge; even the local answer path cannot consume it yet.
pub(super) fn prepare_linked(
    args: &Value,
    context: &DelegatedContext,
    link: &Value,
    recheck: &dyn Fn() -> Result<(), Denial>,
) -> Result<Value> {
    let ship = args["ship"].as_str().ok_or(Denial::Arguments)?;
    ensure!(context.has("approval:answer"), Denial::Scope);
    ensure!(context.allows(ship), Denial::Ship);
    ensure!(args["mode"] == "prepare", Denial::ApprovalUnavailable);
    let home = super::members()?
        .into_iter()
        .find(|(s, _)| s == ship)
        .map(|(_, home)| home)
        .ok_or(Denial::ApprovalUnavailable)?;
    let store = Store::open(&home)?;
    let ask = crate::mail::find(&store, args["id"].as_str().ok_or(Denial::Arguments)?)?
        .ok_or(Denial::ApprovalUnavailable)?;
    ensure!(ask["from"] == ship, Denial::ApprovalUnavailable);
    eligible(&store, &ask, context)?;
    prepare_bound(&store, &ask, context, Some(link), recheck)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FrozenWallClock;

    fn context() -> DelegatedContext {
        DelegatedContext::resolved(
            "alpha-grant".into(),
            "door:alpha".into(),
            vec!["approval:answer".into()],
            Some(vec!["alpha".into()]),
        )
    }

    fn fixture() -> (tempfile::TempDir, Store, Value) {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("home")).unwrap();
        store.connection().execute_batch(
            "INSERT INTO charters VALUES('charter','{\"active\":true}');
             INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('task','charter','alpha','Build','open',0);
             INSERT INTO ships VALUES('alpha','{\"name\":\"alpha\",\"scope\":\"Build\"}');
             INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state)
             VALUES('ask','alpha','captain','request','task','Build?',
             '{\"approves\":{\"action\":\"Submit build 55\",\"command_prefix\":\"make release\",\"ttl_secs\":60}}','normal',100,'open');"
        ).unwrap();
        let ask = crate::mail::find(&store, "ask").unwrap().unwrap();
        (temp, store, ask)
    }

    fn answer(prepared: &Value) -> Value {
        json!({"mode":"answer","id":"ask","ship":"alpha","digest":prepared["digest"],
            "version":prepared["version"],"action":"approve"})
    }

    #[test]
    fn linked_preparation_receipt_is_durable_and_not_locally_consumable() {
        let (_temp, store, ask) = fixture();
        let binding = json!({"version":1,"origin":"alpha","link":"beta","authority":"gamma"});
        let prepared = prepare_bound(&store, &ask, &context(), Some(&binding), &|| Ok(())).unwrap();
        let reopened = Store::open(store.home()).unwrap();
        let saved = challenge_content(&reopened, prepared["digest"].as_str().unwrap()).unwrap();
        assert_eq!(saved["link"], binding);
        assert_eq!(saved["grant"], prepared["grant"]);
        assert_eq!(
            consume(&reopened, &answer(&prepared), &context())
                .unwrap_err()
                .downcast_ref::<Denial>(),
            Some(&Denial::ApprovalUnavailable)
        );
        assert_eq!(
            crate::mail::find(&reopened, "ask").unwrap().unwrap()["state"],
            "open"
        );
    }

    #[test]
    fn linked_receive_deadline_failure_rolls_back_preparation_and_its_audit() {
        let (_temp, store, ask) = fixture();
        let result = prepare_bound(
            &store,
            &ask,
            &context(),
            Some(&json!({"link":"beta"})),
            &|| Err(Denial::ApprovalUnavailable),
        );
        assert_eq!(
            result.unwrap_err().downcast_ref::<Denial>(),
            Some(&Denial::ApprovalUnavailable)
        );
        let count: i64 = store
            .connection()
            .query_row("SELECT count(*) FROM door_approval_challenges", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        assert!(
            !store
                .connection()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM audit WHERE kind='door.approval_prepared')",
                    [],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
    }

    #[test]
    fn challenge_expiry_is_enforced_without_sleeping() {
        let (_temp, store, ask) = fixture();
        let frozen = FrozenWallClock::at(1_000_000);
        let prepared = prepare(&store, &ask, &context()).unwrap();
        let a = answer(&prepared);
        drop(frozen);
        let _later = FrozenWallClock::at(1_000_000 + LIFETIME_MS);
        assert_eq!(
            consume(&store, &a, &context())
                .unwrap_err()
                .downcast_ref::<Denial>(),
            Some(&Denial::ApprovalStale)
        );
    }

    #[test]
    fn prepared_sentence_and_expiry_are_exactly_the_native_grant_after_a_delayed_answer() {
        let (_temp, store, ask) = fixture();
        let clock = FrozenWallClock::at(1_000_000);
        let prepared = prepare(&store, &ask, &context()).unwrap();
        assert!(
            prepared["grant"]["sentence"]
                .as_str()
                .unwrap()
                .starts_with("The captain approved this via the chat door in reply ")
        );
        assert_eq!(prepared["approve_unavailable"], false);
        assert_eq!(prepared["grant"]["command_prefix"], "make release");
        assert_eq!(prepared["grant"]["ttl_secs"], 60);
        assert_eq!(prepared["grant"]["expires_ms"], 1_060_000);
        // A short TTL also caps the challenge lifetime.
        assert_eq!(prepared["expires_ms"], 1_060_000);
        drop(clock);
        let _later = FrozenWallClock::at(1_030_000);
        let reply = json!({"id":prepared["grant"]["reply_id"],"from":"captain","to":"alpha",
            "kind":"reply","correlation":"ask","body":"yes","payload":{"chat_door":true}});
        crate::mail::send_door_approval(
            &store,
            &reply,
            &answer(&prepared),
            &context(),
            true,
            &|| Ok(()),
        )
        .unwrap();
        let native = crate::captain_approvals::sentences(&store, "alpha", Store::now_ms()).unwrap();
        assert_eq!(
            native,
            vec![prepared["grant"]["sentence"].as_str().unwrap()]
        );
        let expires: i64 = store
            .connection()
            .query_row("SELECT expires_ms FROM captain_approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(expires, 1_060_000);
        store
            .connection()
            .execute("DELETE FROM mail WHERE kind='reply'", [])
            .unwrap();
        let reopened = Store::open(store.home()).unwrap();
        assert_eq!(
            crate::captain_approvals::sentences(&reopened, "alpha", Store::now_ms()).unwrap(),
            native
        );
    }

    #[test]
    fn provenance_alone_does_not_retroactively_mark_legacy_approvals() {
        let (_temp, store, ask) = fixture();
        let _clock = FrozenWallClock::at(1_000_000);
        let prepared = prepare(&store, &ask, &context()).unwrap();
        let reply = json!({"id":prepared["grant"]["reply_id"],"from":"captain","to":"alpha",
            "kind":"reply","correlation":"ask","body":"yes","payload":{"chat_door":true}});
        crate::mail::send_door_approval(
            &store,
            &reply,
            &answer(&prepared),
            &context(),
            true,
            &|| Ok(()),
        )
        .unwrap();
        // Step-1 homes retained provenance, but did not store marked sentences.
        store
            .connection()
            .execute("DROP TABLE captain_approval_door_sentences", [])
            .unwrap();
        let native = crate::captain_approvals::sentence(
            prepared["grant"]["reply_id"].as_str().unwrap(),
            "ask",
            "alpha",
            "task",
            &crate::routes::utc_time(prepared["grant"]["expires_ms"].as_i64().unwrap()),
            "Submit build 55",
            Some("make release"),
        );
        assert_eq!(
            crate::captain_approvals::sentences(&store, "alpha", Store::now_ms()).unwrap(),
            vec![native.clone()]
        );
        assert!(
            !crate::captain_approvals::bearings_lines(&store, "alpha")
                .unwrap()
                .join("\n")
                .contains("via chat door")
        );
        // Even once a new prepare creates the empty table, old rows stay native.
        init(&store).unwrap();
        assert_eq!(
            crate::captain_approvals::sentences(&store, "alpha", Store::now_ms()).unwrap(),
            vec![native]
        );
    }

    #[test]
    fn non_approving_bound_answer_rolls_back_if_native_record_would_grant() {
        let (_temp, store, ask) = fixture();
        let _clock = FrozenWallClock::at(1_000_000);
        let prepared = prepare(&store, &ask, &context()).unwrap();
        let mut args = answer(&prepared);
        args["action"] = json!("deny");
        // Deliberately inconsistent internal inputs: the public tool never sends
        // a yes body for deny. The bound postcondition still must catch this.
        let mut reply = json!({"id":prepared["grant"]["reply_id"],"from":"captain","to":"alpha",
            "kind":"reply","correlation":"ask","body":"yes","payload":{"chat_door":true}});
        assert_eq!(
            crate::mail::send_door_approval(&store, &reply, &args, &context(), false, &|| Ok(()),)
                .unwrap_err()
                .downcast_ref::<Denial>(),
            Some(&Denial::ApprovalUnavailable)
        );
        assert!(
            crate::captain_approvals::sentences(&store, "alpha", Store::now_ms())
                .unwrap()
                .is_empty()
        );
        assert!(
            crate::mail::find(&store, prepared["grant"]["reply_id"].as_str().unwrap())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            crate::mail::find(&store, "ask").unwrap().unwrap()["state"],
            "open"
        );
        // Challenge consumption rolled back too: the correct deny still works.
        reply["body"] = json!("no");
        crate::mail::send_door_approval(&store, &reply, &args, &context(), false, &|| Ok(()))
            .unwrap();
        assert_eq!(
            crate::mail::find(&store, "ask").unwrap().unwrap()["state"],
            "answered"
        );
    }

    #[test]
    fn prepare_allows_deny_for_a_sentence_native_admission_would_skip() {
        let (_temp, store, _) = fixture();
        let _clock = FrozenWallClock::at(1_000_000);
        let task = "t".repeat(128);
        let id = "a".repeat(128);
        store.connection().execute(
            "INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES(?1,'charter','alpha','Build','open',0)",
            [&task],
        ).unwrap();
        store
            .connection()
            .execute(
                "UPDATE mail SET id=?1,task_id=?2,payload=?3 WHERE id='ask'",
                params![
                    id,
                    task,
                    json!({"approves":{"action":"x".repeat(300)}}).to_string()
                ],
            )
            .unwrap();
        let ask = crate::mail::find(&store, &id).unwrap().unwrap();
        // The complete grant is too long for auto mode, but a denial must
        // remain possible and must not create an approval.
        eligible(&store, &ask, &context()).unwrap();
        let prepared = prepare(&store, &ask, &context()).unwrap();
        assert_eq!(prepared["approve_unavailable"], true);
        let mut a = answer(&prepared);
        a["id"] = json!(id);
        let mut reply = json!({"id":prepared["grant"]["reply_id"],"from":"captain","to":"alpha",
            "kind":"reply","correlation":id,"body":"yes","payload":{"chat_door":true}});
        assert_eq!(
            crate::mail::send_door_approval(&store, &reply, &a, &context(), true, &|| Ok(()))
                .unwrap_err()
                .downcast_ref::<Denial>(),
            Some(&Denial::ApprovalUnavailable)
        );
        a["action"] = json!("deny");
        reply["body"] = json!("no");
        crate::mail::send_door_approval(&store, &reply, &a, &context(), false, &|| Ok(())).unwrap();
        let count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM door_approval_challenges", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        assert!(
            crate::captain_approvals::sentences(&store, "alpha", Store::now_ms())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            crate::mail::find(&store, &id).unwrap().unwrap()["state"],
            "answered"
        );
    }

    #[test]
    fn oversized_prefix_is_shown_but_unrecordable_and_deny_still_works() {
        let (_temp, store, _) = fixture();
        let _clock = FrozenWallClock::at(1_000_000);
        let action = "x".repeat(300);
        let prefix = format!("make {}", "x".repeat(115));
        store
            .connection()
            .execute(
                "UPDATE mail SET payload=?1 WHERE id='ask'",
                [
                    json!({"approves":{"action":action,"command_prefix":prefix,"ttl_secs":60}})
                        .to_string(),
                ],
            )
            .unwrap();
        let ask = crate::mail::find(&store, "ask").unwrap().unwrap();
        let prepared = prepare(&store, &ask, &context()).unwrap();
        assert_eq!(prepared["approve_unavailable"], true);
        assert_eq!(prepared["grant"]["command_prefix"], prefix);
        assert_eq!(prepared["grant"]["ttl_secs"], 60);
        assert_eq!(prepared["grant"]["expires_ms"], 1_060_000);
        let sentence = prepared["grant"]["sentence"].as_str().unwrap();
        let (plain, shown_prefix) = sentence
            .split_once(" Only commands that begin exactly with: ")
            .unwrap();
        assert!(plain.chars().count() <= 600);
        assert!(sentence.chars().count() > 600);
        assert!(plain.starts_with("The captain approved this via the chat door in reply "));
        assert!(plain.ends_with(&action));
        assert_eq!(shown_prefix, prefix);
        let mut args = answer(&prepared);
        let mut reply = json!({"id":prepared["grant"]["reply_id"],"from":"captain","to":"alpha",
            "kind":"reply","correlation":"ask","body":"yes","payload":{"chat_door":true}});
        assert_eq!(
            crate::mail::send_door_approval(&store, &reply, &args, &context(), true, &|| Ok(()))
                .unwrap_err()
                .downcast_ref::<Denial>(),
            Some(&Denial::ApprovalUnavailable)
        );
        assert_eq!(
            crate::mail::find(&store, "ask").unwrap().unwrap()["state"],
            "open"
        );
        assert!(
            crate::mail::find(&store, prepared["grant"]["reply_id"].as_str().unwrap())
                .unwrap()
                .is_none()
        );
        let count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert!(
            crate::captain_approvals::sentences(&store, "alpha", Store::now_ms())
                .unwrap()
                .is_empty()
        );
        // The failed approval rolls back challenge consumption as well as mail
        // and grant writes: the same prepared choice can still be denied.
        args["action"] = json!("deny");
        reply["body"] = json!("no");
        crate::mail::send_door_approval(&store, &reply, &args, &context(), false, &|| Ok(()))
            .unwrap();
        assert_eq!(
            crate::mail::find(&store, "ask").unwrap().unwrap()["state"],
            "answered"
        );
        assert_eq!(
            consume(&store, &args, &context())
                .unwrap_err()
                .downcast_ref::<Denial>(),
            Some(&Denial::ApprovalStale)
        );
    }

    #[test]
    fn capacity_filled_after_prepare_rolls_back_and_keeps_the_ask_open() {
        let (_temp, store, ask) = fixture();
        let _clock = FrozenWallClock::at(1_000_000);
        let initial = prepare(&store, &ask, &context()).unwrap();
        assert_eq!(initial["approve_unavailable"], false);
        for n in 0..crate::captain_approvals::MAX_LIVE {
            store.connection().execute(
                "INSERT INTO captain_approvals(id,ship,task_id,request_id,reply_id,action,created_ms,expires_ms,source)
                 VALUES(?1,'alpha','task','other',?1,'Other step',1000000,2000000,'reply')",
                [format!("other-{n}")],
            ).unwrap();
        }
        let prepared = prepare(&store, &ask, &context()).unwrap();
        assert_eq!(prepared["approve_unavailable"], true);
        let reply = json!({"id":prepared["grant"]["reply_id"],"from":"captain","to":"alpha",
            "kind":"reply","correlation":"ask","body":"yes","payload":{"chat_door":true}});
        assert_eq!(
            crate::mail::send_door_approval(
                &store,
                &reply,
                &answer(&prepared),
                &context(),
                true,
                &|| Ok(()),
            )
            .unwrap_err()
            .downcast_ref::<Denial>(),
            Some(&Denial::ApprovalUnavailable)
        );
        assert_eq!(
            crate::mail::find(&store, "ask").unwrap().unwrap()["state"],
            "open"
        );
        let count: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM captain_approvals WHERE request_id='ask'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        let replies: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM mail WHERE kind='reply'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(replies, 0);
        // The failed answer preserves the challenge. Once capacity is free,
        // the same explicit consent works.
        store
            .connection()
            .execute("DELETE FROM captain_approvals WHERE id='other-0'", [])
            .unwrap();
        crate::mail::send_door_approval(
            &store,
            &reply,
            &answer(&prepared),
            &context(),
            true,
            &|| Ok(()),
        )
        .unwrap();
        assert_eq!(
            crate::mail::find(&store, "ask").unwrap().unwrap()["state"],
            "answered"
        );
    }

    #[test]
    fn every_displayed_grant_and_decision_field_is_bound() {
        let (_temp, store, _) = fixture();
        let _clock = FrozenWallClock::at(1_000_000);
        let payload = json!({"approves":{"action":"Submit build 55","command_prefix":"make release","ttl_secs":60},
            "decision":{"question":"Submit now?","options":[
                {"id":"submit","label":"Submit build 55","what_happens":"Publish build","approves":true},
                {"id":"hold","label":"Hold","what_happens":"Wait","approves":false}]}});
        store
            .connection()
            .execute(
                "UPDATE mail SET kind='decision',payload=?1 WHERE id='ask'",
                [payload.to_string()],
            )
            .unwrap();
        let ask = crate::mail::find(&store, "ask").unwrap().unwrap();
        let prepared = prepare(&store, &ask, &context()).unwrap();
        assert_eq!(prepared["question"], "Submit now?");
        assert_eq!(prepared["options"], payload["decision"]["options"]);
        let a = answer(&prepared);
        for (pointer, value) in [
            ("/approves/action", json!("Submit build 56")),
            ("/approves/command_prefix", json!("make publish")),
            ("/approves/ttl_secs", json!(120)),
            ("/decision/question", json!("Submit later?")),
            ("/decision/options/0/id", json!("publish")),
            ("/decision/options/0/label", json!("Publish")),
            (
                "/decision/options/0/what_happens",
                json!("Different outcome"),
            ),
            ("/decision/options/0/approves", json!(false)),
        ] {
            let mut changed = payload.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            store
                .connection()
                .execute(
                    "UPDATE mail SET payload=?1 WHERE id='ask'",
                    [changed.to_string()],
                )
                .unwrap();
            assert_eq!(
                consume(&store, &a, &context())
                    .unwrap_err()
                    .downcast_ref::<Denial>(),
                Some(&Denial::ApprovalStale),
                "{pointer}"
            );
        }
        store
            .connection()
            .execute(
                "UPDATE mail SET payload=?1 WHERE id='ask'",
                [payload.to_string()],
            )
            .unwrap();
        assert!(consume(&store, &a, &context()).is_ok());
    }

    #[test]
    fn sensitive_runtime_asks_and_nonlocal_copies_never_prepare() {
        let (_temp, store, ask) = fixture();
        let mut copy = ask.clone();
        copy["delivery"] = json!("received");
        assert_eq!(
            eligible(&store, &copy, &context())
                .unwrap_err()
                .downcast_ref::<Denial>(),
            Some(&Denial::ApprovalUnavailable)
        );
        for kind in [
            "production_approval",
            crate::privileged::REQUEST_KIND,
            "sudo",
        ] {
            let mut sensitive = ask.clone();
            sensitive["request_kind"] = json!(kind);
            assert_eq!(
                eligible(&store, &sensitive, &context())
                    .unwrap_err()
                    .downcast_ref::<Denial>(),
                Some(&Denial::ConfirmationRequired)
            );
        }
        for key in ["type", "privileged", "project_consent", "needs_hands"] {
            let mut sensitive = ask.clone();
            sensitive["payload"][key] = json!(true);
            assert_eq!(
                eligible(&store, &sensitive, &context())
                    .unwrap_err()
                    .downcast_ref::<Denial>(),
                Some(&Denial::ConfirmationRequired)
            );
        }
        for key in ["block", "placement", "relayed_by", "future_confirmation"] {
            let mut unknown = ask.clone();
            unknown["payload"][key] = json!({"secret":"not returned"});
            assert_eq!(
                eligible(&store, &unknown, &context())
                    .unwrap_err()
                    .downcast_ref::<Denial>(),
                Some(&Denial::ApprovalUnavailable)
            );
        }
    }

    #[test]
    fn pending_storage_is_bounded_and_expired_rows_reclaimed() {
        let (_temp, store, ask) = fixture();
        let frozen = FrozenWallClock::at(1_000_000);
        init(&store).unwrap();
        store.connection().execute_batch("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<4096)
            INSERT INTO door_approval_challenges SELECT CAST(x AS TEXT),1,'other','door:alpha','alpha','home','other','content',1600000 FROM n;").unwrap();
        assert_eq!(
            prepare(&store, &ask, &context())
                .unwrap_err()
                .downcast_ref::<Denial>(),
            Some(&Denial::RetryCapacity)
        );
        drop(frozen);
        let _later = FrozenWallClock::at(1_600_000);
        assert!(prepare(&store, &ask, &context()).is_ok());
    }

    #[test]
    fn content_is_canonical_and_covers_action_options_body_and_task() {
        let (_temp, _store, ask) = fixture();
        let digest = content(&ask);
        let reordered: Value = serde_json::from_str(&serde_json::to_string(&ask).unwrap()).unwrap();
        assert_eq!(digest, content(&reordered));
        for path in ["body", "task_id", "kind", "request_kind", "deadline_ms"] {
            let mut changed = ask.clone();
            changed[path] = json!("changed");
            assert_ne!(digest, content(&changed));
        }
        for payload in [
            json!({"approves":{"action":"Other"}}),
            json!({"approves":{"action":"Submit build 55"},"decision":{"options":[]}}),
        ] {
            let mut changed = ask.clone();
            changed["payload"] = payload;
            assert_ne!(digest, content(&changed));
        }
    }

    #[test]
    fn answer_is_single_winner_with_native_approval_across_connections() {
        let (_temp, store, ask) = fixture();
        let _clock = FrozenWallClock::at(1_000_000);
        let prepared = prepare(&store, &ask, &context()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let home = store.home().to_owned();
                let a = answer(&prepared);
                let reply_id = prepared["grant"]["reply_id"].clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let _clock = FrozenWallClock::at(1_000_001);
                    let store = Store::open(&home).unwrap();
                    let context = context();
                    barrier.wait();
                    let reply = json!({"id":reply_id,"from":"captain","to":"alpha",
                    "kind":"reply","correlation":"ask","body":"yes","payload":{"chat_door":true}});
                    crate::mail::send_door_approval(&store, &reply, &a, &context, true, &|| Ok(()))
                        .is_ok()
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            threads
                .into_iter()
                .filter_map(|t| t.join().unwrap().then_some(()))
                .count(),
            1
        );
        let count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let replies: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM mail WHERE kind='reply'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(replies, 1);
    }

    #[test]
    fn authority_lost_before_commit_rolls_back_reply_approval_and_challenge() {
        let (_temp, store, ask) = fixture();
        let _clock = FrozenWallClock::at(1_000_000);
        let prepared = prepare(&store, &ask, &context()).unwrap();
        let a = answer(&prepared);
        let calls = std::cell::Cell::new(0);
        let recheck = || {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Ok(())
            } else {
                Err(Denial::Revoked)
            }
        };
        let reply = json!({"id":prepared["grant"]["reply_id"],"from":"captain","to":"alpha",
            "kind":"reply","correlation":"ask","body":"yes","payload":{"chat_door":true}});
        let result =
            crate::mail::send_door_approval(&store, &reply, &a, &context(), true, &recheck);
        assert_eq!(
            result.unwrap_err().downcast_ref::<Denial>(),
            Some(&Denial::Revoked)
        );
        let count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(
            crate::mail::find(&store, "ask").unwrap().unwrap()["state"],
            "open"
        );
        assert!(consume(&store, &a, &context()).is_ok());
    }

    #[test]
    fn challenge_is_durable_bounded_and_rolls_back_with_failed_native_record() {
        let (_temp, store, ask) = fixture();
        let _clock = FrozenWallClock::at(1_000_000);
        let first = prepare(&store, &ask, &context()).unwrap();
        let prepared = prepare(&store, &ask, &context()).unwrap();
        let a = answer(&prepared);
        let reopened = Store::open(store.home()).unwrap();
        let other_resource = DelegatedContext::resolved(
            "alpha-grant".into(),
            "door:beta".into(),
            vec!["approval:answer".into()],
            None,
        );
        assert!(consume(&reopened, &a, &other_resource).is_err());
        reopened
            .connection()
            .execute("UPDATE door_approval_challenges SET home='wrong-home'", [])
            .unwrap();
        assert!(consume(&reopened, &a, &context()).is_err());
        reopened
            .connection()
            .execute(
                "UPDATE door_approval_challenges SET home=?1",
                [store.home().to_string_lossy().as_ref()],
            )
            .unwrap();
        assert!(consume(&reopened, &answer(&first), &context()).is_err());
        let mut wrong_version = a.clone();
        wrong_version["version"] = json!(VERSION + 1);
        assert_eq!(
            consume(&reopened, &wrong_version, &context())
                .unwrap_err()
                .downcast_ref::<Denial>(),
            Some(&Denial::ApprovalStale)
        );
        let tx = reopened.write_transaction().unwrap();
        assert!(consume(&reopened, &a, &context()).is_ok());
        tx.rollback().unwrap();
        assert!(consume(&reopened, &a, &context()).is_ok());
        assert!(consume(&reopened, &a, &context()).is_err());
    }
}
