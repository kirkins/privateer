mod support;

#[test]
fn hosted_ready_work_crosses_the_native_adapter_and_real_runtime_boundary() {
    let output = support::command("node")
        .arg("tests/attended-work.mjs")
        .arg(support::BIN)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Node is required for the native work-delivery fixture");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
