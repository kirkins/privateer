#[test]
fn non_mcp_door_status_omits_mcp_identity() {
    let fleet = Fleet::with_ships(&[("alpha", true)]);
    fleet.ok(&["door", "add", "alpha", "--manifest", fleet.root.join("adapter/manifest.json").to_str().unwrap()]);
    for args in [vec!["door", "status", "alpha"], vec!["door", "list"]] {
        let status = fleet.ok(&args);
        assert!(status.contains("alpha:"));
        assert!(!status.lines().any(|line| line.trim().starts_with("identity ")));
    }
}

#[test]
fn door_status_detects_old_adapter_behind_scoped_guard() {
    let fleet = Fleet::with_ships(&[("alpha", true)]);
    let root = fleet.root.join("releases/1111111111111111111111111111111111111111/share/privateer/plugins/mcp");
    fs::create_dir_all(&root).unwrap();
    let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/mcp");
    for entry in fs::read_dir(&bundled).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), root.join(entry.file_name())).unwrap();
        }
    }
    fleet.ok(&["door", "add", "alpha", "--manifest", root.join("manifest.json").to_str().unwrap(), "--protocol", "2"]);
    let identity = || -> Value {
        let status = fleet.ok(&["door", "status", "alpha"]);
        serde_json::from_str(status.lines().find_map(|line| line.trim().strip_prefix("identity ")).unwrap()).unwrap()
    };
    let matching = identity();
    assert_eq!(matching["adapter_drift"], false);
    assert_eq!(matching["adapter"]["release_commit"], "1111111111111111111111111111111111111111");
    // All semantic versions stay 0.1.0: content, not the label, detects drift.
    fs::write(root.join("tools.py"), "# previous catalog\n").unwrap();
    let drift = identity();
    assert_eq!(drift["adapter_drift"], true);
    assert_ne!(drift["adapter"]["revision"], drift["core"]["adapter_revision"]);
    assert_eq!(drift["adapter"]["manifest_revision"], drift["core"]["manifest_revision"]);
    assert!(!drift.to_string().contains(&fleet.root.display().to_string()));
    fs::rename(root.join("manifest.json"), root.join("unavailable-manifest.json")).unwrap();
    let unavailable = identity();
    assert_eq!(unavailable["identity_status"], "unavailable");
    assert!(unavailable["adapter_drift"].is_null());
}
