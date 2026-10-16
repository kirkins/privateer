mod support;

#[test]
fn the_live_session_reports_its_attention() {
    let output = support::command("node")
        .arg("tests/pi-attention.mjs")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Node is required to check attention reports");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
