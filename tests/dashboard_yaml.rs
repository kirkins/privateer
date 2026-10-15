#![cfg(feature = "dashboard")]

use privateer::{dashboard::model::Snapshot, dispatch, store::Store};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};

struct Fixture {
    _root: tempfile::TempDir,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let mut store = Store::open(&root.path().join("home")).unwrap();
        dispatch(
            &mut store,
            "charter.put",
            json!({"id":"example","objective":"Read presentation configuration","repo":repo,"min_free_bytes":0}),
        )
        .unwrap();
        dispatch(
            &mut store,
            "task.create",
            json!({"id":"work","charter_id":"example","owner":"runtime-owner","title":"Preserve recorded ownership"}),
        )
        .unwrap();
        Self { _root: root, store }
    }

    fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.store.home().join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn read(&self) -> Snapshot {
        Snapshot::read(self.store.home()).unwrap()
    }

    fn refused(&self, reason: &str) {
        let error = format!("{:#}", Snapshot::read(self.store.home()).unwrap_err());
        assert!(error.contains(reason), "expected {reason:?}: {error}");
    }
}

#[test]
fn shipped_yaml_registry_matches_legacy_json_and_preserves_recorded_work() {
    let fixture = Fixture::new();
    let before = fixture.read();
    let legacy = json!({"ships":[
        {"id":"navigator","name":"Navigator","liaison":true,
         "description":"A familiar point of contact; an equal peer","terminals":[]},
        {"id":"apps","name":"Apps","description":"Application work",
         "terminals":[{"pane_id":"REPLACE_WITH_ACTUAL_PANE_ID","terminal_id":"REPLACE_WITH_ACTUAL_TERMINAL_ID"}]},
        {"id":"research","name":"Research","description":"Available for assignments","terminals":[]}
    ]});
    let json_path = fixture.write("dashboard.json", legacy.to_string());
    let from_json = fixture.read();
    fs::remove_file(json_path).unwrap();

    fixture.write("dashboard.yaml", include_str!("../examples/dashboard.yaml"));
    let from_yaml = fixture.read();
    assert_eq!(
        serde_json::to_value(&from_yaml).unwrap(),
        serde_json::to_value(from_json).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&from_yaml.tasks).unwrap(),
        serde_json::to_value(before.tasks).unwrap()
    );
    assert_eq!(from_yaml.tasks[0].owner, "runtime-owner");
    assert!(
        from_yaml
            .ships
            .iter()
            .any(|ship| ship.id == "runtime-owner")
    );
    assert!(
        from_yaml
            .ships
            .iter()
            .any(|ship| ship.id == "navigator" && ship.liaison)
    );
}

#[test]
fn yaml_and_yml_suffixes_preserve_strings_booleans_and_terminal_bindings() {
    let fixture = Fixture::new();
    let yaml = "ships:\n  - id: contact\n    name: 'true'\n    description: |-\n      First line\n      Second line\n    liaison: true\n    terminals:\n      - pane_id: '001'\n        terminal_id: 'false'\n";
    let mut previous: Option<Value> = None;
    for name in ["dashboard.yaml", "dashboard.yml"] {
        let path = fixture.write(name, yaml);
        let snapshot = fixture.read();
        let ship = snapshot
            .ships
            .iter()
            .find(|ship| ship.id == "contact")
            .unwrap();
        assert_eq!(ship.name, "true");
        assert_eq!(ship.description, "First line\nSecond line");
        assert!(ship.liaison);
        assert_eq!(ship.terminals[0].pane_id, "001");
        assert_eq!(ship.terminals[0].terminal_id, "false");
        let current = serde_json::to_value(snapshot).unwrap();
        if let Some(previous) = &previous {
            assert_eq!(&current, previous);
        }
        previous = Some(current);
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn multiple_registry_formats_are_refused_even_when_their_contents_agree() {
    for names in [
        ["dashboard.yaml", "dashboard.yml"],
        ["dashboard.yaml", "dashboard.json"],
        ["dashboard.yml", "dashboard.json"],
    ] {
        let fixture = Fixture::new();
        fixture.write(names[0], "{\"ships\":[]}");
        fixture.write(names[1], "{\"ships\":[]}");
        assert!(
            Snapshot::read(fixture.store.home()).is_err(),
            "accepted ambiguous files {names:?}"
        );
    }
}

#[test]
fn invalid_yaml_never_falls_back_to_an_empty_or_legacy_registry() {
    let fixture = Fixture::new();
    let invalid = fixture.write("dashboard.yaml", "ships: [\n");
    fixture.refused("YAML");
    fixture.write("dashboard.json", "{\"ships\":[]}");
    assert!(Snapshot::read(fixture.store.home()).is_err());
    fs::remove_file(invalid).unwrap();
    assert!(
        fixture
            .read()
            .ships
            .iter()
            .any(|ship| ship.id == "runtime-owner")
    );
}

#[test]
fn yaml_registries_reject_duplicate_keys_custom_constructors_and_alias_expansion() {
    let fixture = Fixture::new();
    for (yaml, reason) in [
        ("ships: []\nships: []\n", "duplicate"),
        (
            "ships:\n  - id: contact\n    name: !Construct Contact\n",
            "custom constructors",
        ),
        (
            "ships:\n  - id: contact\n    name: Contact\n    name: Other\n",
            "duplicate",
        ),
        (
            "ships: []\n? &key [one, two]\n: value\n? *key\n: other\n",
            "string object key",
        ),
    ] {
        fixture.write("dashboard.yaml", yaml);
        fixture.refused(reason);
    }
    let repeated = |alias: &str| {
        std::iter::repeat_n(alias, 10)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let yaml = format!(
        "ships: []\nseed: &a [{}]\nfirst: &b [{}]\nsecond: &c [{}]\nthird: [{}]\n",
        repeated("x"),
        repeated("*a"),
        repeated("*b"),
        repeated("*c")
    );
    assert!(yaml.len() < 1024);
    fixture.write("dashboard.yaml", yaml);
    fixture.refused("10000 nodes");
}

#[test]
fn yaml_registries_keep_typed_schema_and_presentation_validation() {
    let fixture = Fixture::new();
    for (yaml, reason) in [
        ("ships:\n  - id: contact\n    name: 7\n", "invalid type"),
        (
            "ships:\n  - id: contact\n    name: Contact\n    liaison: 'true'\n",
            "invalid type",
        ),
        ("ships: []\nunknown: true\n", "unknown field"),
        (
            "ships:\n  - {id: contact, name: Contact}\n  - {id: contact, name: Other}\n",
            "duplicate ship identity",
        ),
        (
            "ships:\n  - {id: one, name: One, liaison: true}\n  - {id: two, name: Two, liaison: true}\n",
            "only one ship may be the liaison",
        ),
        (
            "ships:\n  - id: one\n    name: One\n    terminals: [{pane_id: pane, terminal_id: terminal}]\n  - id: two\n    name: Two\n    terminals: [{pane_id: pane, terminal_id: terminal}]\n",
            "a pane can belong to only one ship",
        ),
    ] {
        fixture.write("dashboard.yaml", yaml);
        fixture.refused(reason);
    }
}

#[test]
fn registry_symlinks_and_nonregular_files_remain_refused_for_every_suffix() {
    let fixture = Fixture::new();
    let target = fixture.write("presentation", "{\"ships\":[]}");
    for name in ["dashboard.yaml", "dashboard.yml", "dashboard.json"] {
        let path = fixture.store.home().join(name);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        fixture.refused("symlink");
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(fixture.store.home().join("missing"), &path).unwrap();
        fixture.refused("symlink");
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        fixture.refused("regular file");
        fs::remove_dir(&path).unwrap();
    }
    fixture.write("dashboard.json", "{\"ships\":[]}");
    std::os::unix::fs::symlink(
        fixture.store.home().join("missing"),
        fixture.store.home().join("dashboard.yaml"),
    )
    .unwrap();
    fixture.refused("symlink");
}

#[test]
fn yaml_registry_raw_size_is_bounded_before_schema_validation() {
    let fixture = Fixture::new();
    let yaml = format!("ships: []\n#{}\n", "x".repeat(65_536));
    fixture.write("dashboard.yaml", yaml);
    fixture.refused("64 KiB");
}
