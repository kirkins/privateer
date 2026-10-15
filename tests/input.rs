mod support;

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

struct Files {
    temp: tempfile::TempDir,
    home: PathBuf,
}

impl Files {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        Self { temp, home }
    }
    fn command(&self) -> Command {
        let mut command = support::command(BIN);
        command
            .current_dir(self.temp.path())
            .args(["--home", self.home.to_str().unwrap()])
            .env("PRIVATEER_FLEET", self.temp.path().join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.temp.path().join("ledger"))
            .env("PRIVATEER_SERVICE", "none");
        command
    }
    fn file(&self, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.temp.path().join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
    fn json(&self, command: &str, args: Value) -> Value {
        checked(
            self.command()
                .args([command, "--json", &args.to_string()])
                .output()
                .unwrap(),
        )
    }
    fn read(&self, command: &str, path: &Path) -> Value {
        checked(
            self.command()
                .args([command, "--file"])
                .arg(path)
                .output()
                .unwrap(),
        )
    }
    fn rejected(&self, name: &str, bytes: impl AsRef<[u8]>, reason: &str) {
        let path = self.file(name, bytes);
        let output = self
            .command()
            .args(["init", "--file"])
            .arg(path)
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted {name}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(reason), "{name}: {error}");
        assert!(!self.home.exists(), "{name}: parsing created a home");
        assert!(!self.temp.path().join("fleet.sqlite").exists());
        assert!(!self.temp.path().join("ledger").exists());
    }
}

fn checked(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn yaml_and_json_files_use_the_same_command_validation_and_results() {
    let files = Files::new();
    let repo = files.temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let args = json!({"id":"mission","repo":repo,"objective":"Repair a regression\nKeep its evidence",
        "actions":["inspect","implement","test"],"min_free_bytes":0,"job_timeout_secs":60});
    let json_path = files.file("charter.json", args.to_string());
    let yaml_path = files.file("charter.yaml", format!(
        "# Human-authored command arguments\nid: !!str mission\nrepo: {}\nobjective: |-\n  Repair a regression\n  Keep its evidence\nactions: [inspect, implement, test]\nmin_free_bytes: 0\njob_timeout_secs: 60\n",
        json!(repo),
    ));
    let inline = files.json("charter.put", args);
    let mut from_json = files.read("charter.put", &json_path);
    let mut from_yaml = files.read("charter.put", &yaml_path);
    from_json["revision"] = inline["revision"].clone();
    from_yaml["revision"] = inline["revision"].clone();
    assert_eq!(from_json, inline);
    assert_eq!(from_yaml, inline);
    let invalid = files.file(
        "invalid-charter.yml",
        "id: invalid\nrepo: .\nobjective: invalid path\n",
    );
    let output = files
        .command()
        .args(["charter.put", "--file"])
        .arg(invalid)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("absolute"));
    assert!(
        files.json("charter.show", json!({"id":"mission"}))["active"]
            .as_bool()
            .unwrap()
    );
}

#[test]
fn file_input_preserves_json_types_and_treats_annotations_as_inert_data() {
    let files = Files::new();
    files.json("ship.put", json!({"name":"apps"}));
    files.json("ship.put", json!({"name":"infra"}));
    let marker = files.temp.path().join("must-not-exist");
    let path = files.file("message.yml", format!(
        "id: typed\nfrom: apps\nto: infra\nkind: notice\nbody: YAML data\npayload:\n  values: [true, false, null, -7, 18446744073709551615, 1.25, 船]\n  numeric_name: {{\"1\": one}}\n  command: !<tag:yaml.org,2002:python/object/apply:os.system> {}\n  source: !<tag:example.invalid,2026:include> /a/file/that/is/not/read\n  literal_env: $PRIVATEER_INPUT_TEST\n  implicit_null:\n",
        json!(format!("touch {}", marker.display())),
    ));
    let result = checked(
        files
            .command()
            .args(["mail.send", "--file"])
            .arg(path)
            .env("PRIVATEER_INPUT_TEST", "must-not-be-expanded")
            .output()
            .unwrap(),
    );
    assert_eq!(
        result["payload"]["values"],
        json!([true, false, null, -7, u64::MAX, 1.25, "船"])
    );
    assert_eq!(result["payload"]["numeric_name"], json!({"1":"one"}));
    assert_eq!(result["payload"]["source"], "/a/file/that/is/not/read");
    assert_eq!(result["payload"]["literal_env"], "$PRIVATEER_INPUT_TEST");
    assert!(result["payload"]["implicit_null"].is_null());
    assert_eq!(
        result["payload"]["command"],
        format!("touch {}", marker.display())
    );
    assert!(!marker.exists());
}

#[test]
fn reused_yaml_anchor_names_keep_the_latest_scalar_sequence_and_mapping() {
    let files = Files::new();
    files.json("ship.put", json!({"name":"apps"}));
    files.json("ship.put", json!({"name":"infra"}));
    let path = files.file("anchors.yaml", "id: anchors\nfrom: apps\nto: infra\nkind: notice\nbody: aliases\npayload:\n  scalar:\n    one: &a inspect\n    before: *a\n    two: &a implement\n    three: &b cleanup\n    latest: *a\n    distinct: *b\n    keys: {*a : value}\n  sequence:\n    one: &a [inspect]\n    before: *a\n    two: &a [inspect, implement]\n    three: &b [inspect, cleanup]\n    latest: *a\n    distinct: *b\n  mapping:\n    one: &a {actions: [inspect]}\n    before: *a\n    two: &a {actions: [inspect, implement]}\n    three: &b {actions: [inspect, cleanup]}\n    latest: *a\n    distinct: *b\n");
    let sent = files.read("mail.send", &path);
    let stored = files.json("mail.show", json!({"id":"anchors"}));
    for message in [sent, stored] {
        for (shape, old, latest, distinct) in [
            (
                "scalar",
                json!("inspect"),
                json!("implement"),
                json!("cleanup"),
            ),
            (
                "sequence",
                json!(["inspect"]),
                json!(["inspect", "implement"]),
                json!(["inspect", "cleanup"]),
            ),
            (
                "mapping",
                json!({"actions":["inspect"]}),
                json!({"actions":["inspect", "implement"]}),
                json!({"actions":["inspect", "cleanup"]}),
            ),
        ] {
            assert_eq!(message["payload"][shape]["before"], old);
            assert_eq!(message["payload"][shape]["latest"], latest);
            assert_eq!(message["payload"][shape]["distinct"], distinct);
        }
        assert_eq!(
            message["payload"]["scalar"]["keys"],
            json!({"implement":"value"})
        );
    }
    let repo = files.temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    for (id, anchors, actions) in [
        (
            "scalar",
            "one: &a inspect\ntwo: &a implement\nthree: &b cleanup",
            "[inspect, *a]",
        ),
        (
            "sequence",
            "one: &a [inspect]\ntwo: &a [inspect, implement]\nthree: &b [inspect, cleanup]",
            "*a",
        ),
    ] {
        let path = files.file("charter.yaml", format!("id: {id}\nrepo: {}\nobjective: correct permissions\n{anchors}\nactions: {actions}\n", json!(repo)));
        assert_eq!(
            files.read("charter.put", &path)["actions"],
            json!(["inspect", "implement"])
        );
        assert_eq!(
            files.json("charter.show", json!({"id":id}))["actions"],
            json!(["inspect", "implement"])
        );
    }
}

#[test]
fn yaml_numeric_overflow_is_rejected_for_values_and_keys_before_home_creation() {
    let files = Files::new();
    for literal in [
        "1e999",
        "+1e999",
        "-1e999",
        "1.0e999",
        "+1.0e999",
        "-1.0e999",
        "18446744073709551616",
        "+18446744073709551616",
        "-9223372036854775809",
        "340282366920938463463374607431768211456",
        "0x100000000000000000000000000000000",
        "+0x100000000000000000000000000000000",
        "-0x100000000000000000000000000000000",
        "0o2000000000000000000000",
        "0b10000000000000000000000000000000000000000000000000000000000000000",
    ] {
        for document in [
            format!("ship: {literal}\n"),
            format!("{literal}: ignored\nship: safe\n"),
            format!("data: &a {literal}\nship: *a\n"),
        ] {
            files.rejected("overflow.yaml", document, "JSON");
        }
    }
    for document in [
        "ship: !!float '1e999'\n",
        "ship: !!int '0x100000000000000000000000000000000'\n",
        "!!float '1e999': ignored\n",
        "!!int '18446744073709551616': ignored\n",
    ] {
        files.rejected("tagged-overflow.yaml", document, "JSON");
    }
}

#[test]
fn quoted_numeric_text_and_explicit_strings_keep_their_values_and_keys() {
    let files = Files::new();
    files.json("ship.put", json!({"name":"apps"}));
    files.json("ship.put", json!({"name":"infra"}));
    for literal in [
        "1e999",
        "+1e999",
        "-1e999",
        "18446744073709551616",
        "0x100000000000000000000000000000000",
    ] {
        for spelling in [
            json!(literal).to_string(),
            format!("'{literal}'"),
            format!("!!str {literal}"),
            format!("!<tag:yaml.org,2002:str> {literal}"),
        ] {
            let path = files.file("strings.yml", format!("from: apps\nto: infra\nkind: notice\nbody: strings\npayload:\n  value: &s {spelling}\n  alias: *s\n  keys: {{{spelling}: preserved}}\n"));
            let sent = files.read("mail.send", &path);
            assert_eq!(sent["payload"]["value"], literal);
            assert_eq!(sent["payload"]["alias"], literal);
            assert_eq!(sent["payload"]["keys"][literal], "preserved");
        }
    }
    let path = files.file("core.yml", "from: apps\nto: infra\nkind: notice\nbody: core\npayload:\n  values: [True, FALSE, Null, ~, 0x10, -0x10, +0o10, 0b10, +42, -9223372036854775808, 1e2, .5, 1., -.5, 012]\n  tags: [!!str true, !!int '42', !!float '42', !!bool 'True', !!null null]\n");
    let sent = files.read("mail.send", &path);
    assert_eq!(
        sent["payload"]["values"],
        json!([
            true,
            false,
            null,
            null,
            16,
            -16,
            8,
            2,
            42,
            i64::MIN,
            100.0,
            0.5,
            1.0,
            -0.5,
            12
        ])
    );
    assert_eq!(
        sent["payload"]["tags"],
        json!(["true", 42, 42.0, true, null])
    );
}

#[test]
fn malformed_or_ambiguous_files_are_rejected_before_home_creation() {
    let files = Files::new();
    for (name, bytes, reason) in [
        ("duplicate.yaml", "ship: first\nship: second\n", "duplicate"),
        ("nested-duplicate.yaml", "data: {x: 1, x: 2}\n", "duplicate"),
        (
            "duplicate.json",
            "{\"data\":{\"x\":1,\"x\":2}}",
            "duplicate",
        ),
        ("numeric-key.yaml", "1: value\n", "string object key"),
        ("boolean-key.yaml", "true: value\n", "string object key"),
        (
            "alias-key.yaml",
            "data: &a 1\n? *a\n: value\n",
            "string object key",
        ),
        (
            "alias-duplicate.yaml",
            "data: &a name\nname: first\n? *a\n: second\n",
            "duplicate",
        ),
        (
            "complex-key.yaml",
            "? [x, y]\n: value\n",
            "string object key",
        ),
        (
            "include.yaml",
            "ship: !include /private/config\n",
            "constructors",
        ),
        (
            "env.yaml",
            "ship: !env PRIVATEER_INPUT_TEST\n",
            "constructors",
        ),
        ("nan.yaml", "data: .nan\n", "finite"),
        ("inf.yaml", "data: .inf\n", "finite"),
        (
            "overflow.yaml",
            "data: 340282366920938463463374607431768211455\n",
            "JSON number range",
        ),
        ("overflow.json", "{\"data\":1e999}", "number out of range"),
        (
            "many.yaml",
            "ship: first\n---\nship: second\n",
            "more than one document",
        ),
        ("many.json", "{} {}", "trailing"),
        ("array.yaml", "[one, two]", "one object"),
        ("scalar.json", "true", "one object"),
        ("empty.yaml", "", "one object"),
        ("malformed.yaml", "data: [unterminated", "invalid YAML"),
        ("unknown.txt", "{}", "extension"),
    ] {
        files.rejected(name, bytes, reason);
    }
    files.rejected("invalid-utf8.yaml", [0xff], "invalid YAML");
    let directory = files.temp.path().join("directory.yaml");
    fs::create_dir(&directory).unwrap();
    let output = files
        .command()
        .args(["init", "--file"])
        .arg(directory)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!files.home.exists());
}

#[test]
fn file_input_bounds_bytes_depth_nodes_keys_and_alias_expansion() {
    let files = Files::new();
    files.rejected("large.yaml", "#".repeat(65_537), "file exceeds 64 KiB");
    files.rejected(
        "deep.yaml",
        format!("data: {}0{}", "[".repeat(70), "]".repeat(70)),
        "nesting",
    );
    files.rejected(
        "deep.json",
        format!("{{\"data\":{}0{}}}", "[".repeat(70), "]".repeat(70)),
        "nesting",
    );
    files.rejected(
        "many-nodes.yaml",
        format!("data: [{}]", vec!["0"; 10_001].join(",")),
        "10000 nodes",
    );
    // 5,000 values alone fit the budget; their keys must also be charged.
    let keys = (0..5_000)
        .map(|n| format!("\"k{n}\":0"))
        .collect::<Vec<_>>()
        .join(",");
    files.rejected("many-keys.json", format!("{{{keys}}}"), "10000 nodes");
    files.rejected(
        "aliases.yaml",
        format!(
            "data: &data {}\nrepeated: [{}]\n",
            "x".repeat(60_000),
            vec!["*data"; 50].join(",")
        ),
        "scalar/key data exceeds 1 MiB",
    );
    files.rejected("recursive.yaml", "data: &data [*data]\n", "nesting");
    files.rejected("collection-aliases.yaml", "a: &a [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]\nb: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a, *a]\nc: &c [*b, *b, *b, *b, *b, *b, *b, *b, *b, *b]\nd: [*c, *c, *c, *c, *c, *c, *c, *c, *c, *c]\n", "10000 nodes");
    files.rejected(
        "escaping.yaml",
        format!("data: |+\n{}", "\n".repeat(40_000)),
        "normalized arguments exceed 64 KiB",
    );
}

#[test]
fn file_flags_do_not_mix_repeat_or_silently_enter_other_input_modes() {
    let files = Files::new();
    let path = files.file("args.yaml", "{}\n");
    let path = path.to_str().unwrap();
    for args in [
        vec!["init", "--file", path, "--json", "{}"],
        vec!["init", "--json", "{}", "--file", path],
        vec!["init", "--file", path, "--file", path],
        vec!["--file", path],
        vec!["rpc", "--file", path],
        vec!["hook", "claude", "SessionStart", "--file", path],
        vec!["init", "--file"],
    ] {
        let output = files.command().args(&args).output().unwrap();
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(!files.home.exists(), "{args:?} created a home");
    }
    // Inline JSON retains its existing last-value behavior.
    let result = checked(
        files
            .command()
            .args([
                "init",
                "--json",
                "{\"ship\":\"first\"}",
                "--json",
                "{\"ship\":\"last\"}",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(result["config"]["ship"], "last");
}

#[test]
fn file_input_keeps_the_agent_role_and_existing_owner_checks() {
    let files = Files::new();
    let repo = files.temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    files.json(
        "charter.put",
        json!({"id":"mission","repo":repo,"objective":"test"}),
    );
    files.json(
        "task.create",
        json!({"id":"task","charter_id":"mission","owner":"apps","title":"test"}),
    );
    let update = files.file("update.yaml", "id: task\nactor: apps\nstatus: running\n");
    let result = checked(
        files
            .command()
            .args(["task.update", "--file"])
            .arg(update)
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "apps")
            .output()
            .unwrap(),
    );
    assert_eq!(result["status"], "running");
    let revoke = files.file("revoke.yaml", "id: mission\n");
    let output = files
        .command()
        .args(["charter.revoke", "--file"])
        .arg(revoke)
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "apps")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("operator command"));
    assert_eq!(
        files.json("charter.show", json!({"id":"mission"}))["active"],
        true
    );
    let stolen = files.file("stolen.yaml", "id: task\nactor: infra\nstatus: done\n");
    let output = files
        .command()
        .args(["task.update", "--file"])
        .arg(stolen)
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "apps")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot act as"));
    assert_eq!(
        files.json("task.show", json!({"id":"task"}))["status"],
        "running"
    );
}

#[test]
fn native_start_keeps_file_flags_after_its_argument_separator() {
    use std::os::unix::fs::PermissionsExt;
    let files = Files::new();
    let bin = files.temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let seen = files.temp.path().join("seen.json");
    let codex = bin.join("codex");
    fs::write(&codex, "#!/usr/bin/env python3\nimport json,os,sys\nopen(os.environ['PRIVATEER_INPUT_SEEN'],'w').write(json.dumps(sys.argv[1:]))\n").unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let initialized = files
        .command()
        .args(["init", "--ship", "apps", "--harness", "codex"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    let output = files
        .command()
        .args(["start", "codex", "--", "--file", "missing.txt"])
        .env("PATH", path)
        .env("PRIVATEER_INPUT_SEEN", &seen)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args: Vec<String> = serde_json::from_slice(&fs::read(seen).unwrap()).unwrap();
    assert!(
        args.windows(2)
            .any(|args| args == ["--file", "missing.txt"]),
        "{args:?}"
    );
}
