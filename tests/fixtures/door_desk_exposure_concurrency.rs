#[test]
fn concurrent_identical_desk_enables_preserve_one_boundary_and_conflicting_enable_refuses() {
    let fleet = Fleet::with_ships(&[("alpha", true)]);
    delegated_adapter(&fleet);
    desk_exposure_owner(&fleet);
    let args = desk_exposure_args().to_string();
    let mut children = Vec::new();
    for _ in 0..2 {
        children.push(
            fleet
                .command(&fleet.root.join("ships/alpha"))
                .args(["door.desk.enable", "--json", &args])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let results: Vec<Value> = children
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).unwrap()
        })
        .collect();
    assert_eq!(results[0]["binding"], results[1]["binding"]);
    desk_exposure_failure(
        &fleet,
        "door.desk.enable",
        json!({"door":"fake","domain":"gamma-domain","mode":"fresh_replay","replay_policy":"open_and_new"}),
        "stale_generation",
    );
    let status = fleet.call("alpha", "door.desk.status", json!({"door":"fake"}));
    assert_eq!(status["binding"], results[0]["binding"]);
    assert_eq!(status["enabled"], true);
}
