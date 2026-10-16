mod support;

#[test]
fn a_first_mate_that_stops_on_a_promise_gets_one_more_step() {
    let output = support::command("node")
        .arg("tests/pi-follow.mjs")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Node is required to check follow-through");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
