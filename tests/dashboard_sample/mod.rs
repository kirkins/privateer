//! Synthetic fleet for dashboard render, focus and mailbox tests. This is test
//! data only: nothing here is read from a home, socket or registry, and no
//! bridge mode presents it.
#![allow(dead_code)]
use privateer::dashboard::{
    captain::Mail,
    fleet::{ShipState, Wake},
    herdr::{Pane, View},
    model::{Custody, Discovery, Job, Notice, Ship, Snapshot, Task, TerminalBinding},
};
use std::path::PathBuf;

fn ship(id: &str, name: &str, description: &str, agent: Option<&str>) -> Ship {
    Ship {
        id: id.into(),
        name: name.into(),
        description: description.into(),
        liaison: id == "navigator",
        live: None,
        remote: None,
        terminals: agent
            .map(|_| TerminalBinding {
                pane_id: format!("sample:{id}"),
                terminal_id: format!("sample-terminal-{id}"),
            })
            .into_iter()
            .collect(),
    }
}

pub fn snapshot() -> Snapshot {
    let mut sample = Snapshot {
        recent: Vec::new(),
        presentation_error: None,
        machines: Vec::new(),
        ships: vec![
            ship(
                "navigator",
                "Navigator",
                "Your usual point of contact; an equal peer",
                Some("pi"),
            ),
            ship(
                "apps",
                "Apps",
                "Application development and builds",
                Some("codex"),
            ),
            ship(
                "infrastructure",
                "Infrastructure",
                "Services, connectivity and runtime investigations",
                Some("pi"),
            ),
            ship(
                "research",
                "Research",
                "Available for the next investigation",
                None,
            ),
        ],
        custody: vec![Custody {
            home: None,
            seq: 5,
            task_id: "app-build".into(),
            actor: "apps".into(),
            kind: "task.handoff".into(),
            owner: "apps".into(),
        }],
        tasks: vec![
            Task {
                home: None,
                id: "navigation".into(),
                owner: "navigator".into(),
                title: "Coordinate the next voyage".into(),
                status: "running".into(),
                handoff_to: None,
                scheduling: None,
            },
            Task {
                home: None,
                id: "app-build".into(),
                owner: "apps".into(),
                title: "Investigate the failing build".into(),
                status: "running".into(),
                handoff_to: Some("infrastructure".into()),
                scheduling: None,
            },
            Task {
                home: None,
                id: "route-check".into(),
                owner: "infrastructure".into(),
                title: "Check the provider route".into(),
                status: "blocked".into(),
                handoff_to: None,
                scheduling: None,
            },
        ],
        pending: vec![Notice {
            home: None,
            owner: "infrastructure".into(),
            seq: 50,
            task_id: "route-check".into(),
            issue_key: "connection".into(),
            kind: "blocked".into(),
            summary: "Repeated connection facts appear as one pending issue".into(),
        }],
        activity: vec![Notice {
            home: None,
            owner: "apps".into(),
            seq: 51,
            task_id: "app-build".into(),
            issue_key: "tests".into(),
            kind: "outcome".into(),
            summary: "Sample logbook outcome — demonstration data only".into(),
        }],
        jobs: vec![Job {
            home: None,
            owner: "apps".into(),
            id: "build-1".into(),
            task_id: "app-build".into(),
            status: "running".into(),
        }],
        truncated: false,
        discovery: Discovery::Complete,
    };
    // Sample states: a working, an idle, a waiting and a stopped ship. No home is read: these paths do not exist.
    for ship in &mut sample.ships {
        let (dispatcher, wake, requests) = match ship.id.as_str() {
            "navigator" => (true, Some("running"), 0),
            "apps" => (true, None, 0),
            "infrastructure" => (true, None, 1),
            _ => (false, None, 0),
        };
        ship.live = Some(ShipState {
            home: PathBuf::from(format!("/sample/{}", ship.id)),
            identity: None,
            liaison: ship.liaison,
            binding: None,
            harness: "sample".into(),
            attended: !ship.terminals.is_empty(),
            dispatcher,
            requests,
            unread: 0,
            wakes: wake
                .map(|status| Wake {
                    mode: "mail".into(),
                    status: status.into(),
                })
                .into_iter()
                .collect(),
            approvals: Vec::new(),
            error: None,
        });
    }
    sample.include_observed_ships();
    sample
}

/// Herdr terminals for the sample fleet, one per ship with an agent.
pub fn view() -> View {
    View {
        version: "sample".into(),
        protocol: 20,
        panes: [
            ("navigator", "pi"),
            ("apps", "codex"),
            ("infrastructure", "pi"),
        ]
        .into_iter()
        .map(|(id, agent)| Pane {
            pane_id: format!("sample:{id}"),
            terminal_id: format!("sample-terminal-{id}"),
            workspace_id: format!("sample-{id}"),
            tab_id: format!("sample-{id}:tab"),
            agent: Some(agent.into()),
            agent_status: "idle".into(),
            title: Some(format!("{id} primary")),
            agent_session: None,
        })
        .collect(),
        socket_identity: (0, 0),
    }
}

pub fn mail() -> Vec<Mail> {
    vec![
        Mail {
            payload: serde_json::json!({}),
            home: None, seq: 0, acknowledged: false,
            identity: None, origin: None,
            id: "sample-decision".into(),
            kind: "Decision".into(),
            from: "Research".into(),
            subject: "Choose the next investigation".into(),
            body: "Research is available. Would you like a reliability review or a provider comparison? This is a sample decision; no work has been assigned.".into(),
            status: "Awaiting captain".into(),
        },
        Mail {
            payload: serde_json::json!({}),
            home: None, seq: 0, acknowledged: false,
            identity: None, origin: None,
            id: "sample-reply".into(),
            kind: "Reply".into(),
            from: "Apps".into(),
            subject: "Build investigation update".into(),
            body: "The sample build investigation is still owned by Apps. Infrastructure has an incoming handoff offer; responsibility changes only after acceptance.".into(),
            status: "Reply to your request".into(),
        },
        Mail {
            payload: serde_json::json!({}),
            home: None, seq: 0, acknowledged: false,
            identity: None, origin: None,
            id: "sample-notice".into(),
            kind: "Notice".into(),
            from: "Infrastructure".into(),
            subject: "Provider connection needs attention".into(),
            body: "Repeated connection alerts are grouped into one item. Detailed operational facts remain in the ship's Bridge and Logbook.".into(),
            status: "Grouped notice".into(),
        },
    ]
}
