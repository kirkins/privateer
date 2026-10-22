//! The Python unit suites beside the fixtures they cover, run under
//! `cargo test` so they cannot go stale unnoticed. Each is hermetic: a
//! loopback `http.server`, mocks or pure parsing, with no native harness,
//! namespace, network or privilege. The native Pi canaries under `scripts/`
//! are manual (see docs/pilot.md).
mod support;

use std::path::Path;

fn suite(script: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = support::command(support::python())
        .arg(root.join(script))
        .current_dir(root)
        .output()
        .unwrap();
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{script}:\n{report}");
    // unittest reports "Ran N tests" and then "OK"; a suite that found
    // nothing to run is not a pass.
    assert!(
        report.contains("\nOK") && !report.contains("Ran 0 tests"),
        "{script}:\n{report}"
    );
}

#[test]
fn the_forge_adapter_suite_passes() {
    suite("tests/forge_adapter_test.py");
}

// The boundary is a Linux network namespace (`unshare`, `/proc/self/ns/net`),
// so its suite runs only where one exists.
#[cfg(target_os = "linux")]
#[test]
fn the_pi_offline_boundary_suite_passes() {
    suite("tests/pi_offline_netns_test.py");
}

#[test]
fn the_terminal_text_suite_passes() {
    suite("scripts/test_terminal_text.py");
}

#[test]
fn the_dashboard_smoke_suite_passes() {
    suite("scripts/test_dashboard_smoke.py");
}
