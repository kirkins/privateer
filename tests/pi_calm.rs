mod support;

#[test]
fn pi_calm_has_bounded_event_clock_and_queue_coverage() {
    let output = support::command("node")
        .arg("tests/pi-calm.mjs")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Node is required to check Pi calm");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
