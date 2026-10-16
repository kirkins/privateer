mod support;

#[test]
fn quiet_compaction_waits_for_a_quiet_full_session_and_keeps_its_records() {
    let output = support::command("node")
        .arg("tests/pi-quiet.mjs")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Node is required to check quiet compaction");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
