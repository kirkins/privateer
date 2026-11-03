use super::*;
use crate::herdr::{Binding, Pane};

fn binding(session: Value) -> Binding {
    Binding {
        socket: "/fictional/host.sock".into(),
        socket_identity: (1, 2),
        pane: Pane {
            pane_id: "alpha".into(),
            terminal_id: "beta".into(),
            workspace_id: "gamma".into(),
            tab_id: "delta".into(),
            agent: Some("pi".into()),
            agent_status: "idle".into(),
            title: None,
            agent_session: Some(session),
        },
    }
}
fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .connection()
        .execute_batch("INSERT INTO ships(name,record) VALUES('survey','{\"location\":\"here\"}');")
        .unwrap();
    (dir, store)
}
fn attend_binding(store: &Store, b: Binding) {
    attend_with_capture(store, "survey", "host:alpha", 90, None, || Some(b)).unwrap();
}
fn current(store: &Store) -> Value {
    presence(store, "survey").unwrap()["lease"]["binding"].clone()
}

#[test]
fn heartbeat_enriches_then_replaces_session_and_preserves_incarnation() {
    let (_dir, store) = store();
    attend_binding(&store, binding(Value::Null));
    let since = binding_fence(&store, "survey").unwrap().unwrap().since_ms;
    for session in [json!({"id":"alpha"}), json!({"id":"beta"})] {
        assert!(
            refresh_with_capture(&store, "survey", "host:alpha", 90, || Some(binding(
                session.clone()
            )))
            .unwrap()
        );
        assert_eq!(current(&store)["pane"]["agent_session"], session);
        assert_eq!(
            binding_fence(&store, "survey").unwrap().unwrap().since_ms,
            since
        );
    }
    // Missing adapter data or failed capture must not downgrade an existing proof.
    for captured in [None, Some(binding(Value::Null))] {
        assert!(refresh_with_capture(&store, "survey", "host:alpha", 90, || captured).unwrap());
        assert_eq!(
            current(&store)["pane"]["agent_session"],
            json!({"id":"beta"})
        );
    }
}

#[test]
fn refresh_fences_holder_incarnation_old_binding_and_expiry() {
    for sql in [
        "UPDATE presence SET holder='host:beta'",
        "UPDATE presence SET holder_since_ms=holder_since_ms+1",
        "UPDATE presence SET binding='{}'",
        "UPDATE presence SET expires_ms=0",
    ] {
        let (_dir, store) = store();
        attend_binding(&store, binding(json!({"id":"alpha"})));
        let result = refresh_with_capture(&store, "survey", "host:alpha", 90, || {
            // Independent writer wins while host I/O is in flight.
            let other = Store::open(store.home()).unwrap();
            other.connection().execute(sql, []).unwrap();
            Some(binding(json!({"id":"stale"})))
        })
        .unwrap();
        assert!(!result, "{sql}");
        assert_ne!(
            current(&store)["pane"]["agent_session"],
            json!({"id":"stale"})
        );
    }
    let (_dir, store) = store();
    attend_binding(&store, binding(json!({"id":"alpha"})));
    assert!(
        !refresh_with_capture(&store, "survey", "host:stale", 90, || panic!(
            "stale holder must not capture"
        ))
        .unwrap()
    );
}

#[test]
fn attend_cannot_overwrite_a_replacement_during_capture() {
    let (_dir, store) = store();
    attend_binding(&store, binding(json!({"id":"alpha"})));
    assert!(
        attend_with_capture(&store, "survey", "host:alpha", 90, None, || {
            store
                .connection()
                .execute(
                    "UPDATE presence SET holder='host:beta',holder_since_ms=holder_since_ms+1",
                    [],
                )
                .unwrap();
            Some(binding(json!({"id":"stale"})))
        })
        .is_err()
    );
    assert_eq!(
        presence(&store, "survey").unwrap()["lease"]["holder"],
        "host:beta"
    );
    assert_eq!(
        current(&store)["pane"]["agent_session"],
        json!({"id":"alpha"})
    );
}

#[test]
fn binding_refresh_never_trades_physical_endpoint() {
    let old = serde_json::to_string(&binding(json!({"id":"alpha"}))).unwrap();
    for field in ["socket", "inode", "pane", "terminal", "workspace", "tab"] {
        let mut new = binding(json!({"id":"beta"}));
        match field {
            "socket" => new.socket = "/fictional/other.sock".into(),
            "inode" => new.socket_identity.1 += 1,
            "pane" => new.pane.pane_id = "other".into(),
            "terminal" => new.pane.terminal_id = "other".into(),
            "workspace" => new.pane.workspace_id = "other".into(),
            _ => new.pane.tab_id = "other".into(),
        }
        assert!(
            renewed_binding(Some(&old), Some(new)).unwrap().is_none(),
            "{field}"
        );
    }
}
