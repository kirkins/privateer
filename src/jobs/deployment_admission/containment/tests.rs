use super::*;

#[test]
fn ordinary_directory_is_structured_refusal_not_fake_containment() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("cgroup.events"), "populated 0\n").unwrap();
    std::fs::write(temp.path().join("cgroup.kill"), "").unwrap();
    let refused = qualify(temp.path()).unwrap_err();
    assert!(!refused.reason.is_empty());
    assert!(refused.reason.len() <= 1024);
    assert!(refused.retained.is_none());
    #[cfg(target_os = "linux")]
    assert!(
        refused.reason.contains("not cgroup2") || refused.reason.contains("requires Linux >=5.16"),
        "{refused:?}"
    );
    // Fake files never become proof and are not written to.
    assert_eq!(std::fs::read(temp.path().join("cgroup.kill")).unwrap(), b"");
}

#[cfg(target_os = "linux")]
#[test]
fn symlink_and_relative_delegations_refuse_without_traversal() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    symlink(temp.path(), temp.path().join("alias")).unwrap();
    for path in [temp.path().join("alias"), std::path::PathBuf::from("alpha")] {
        let refused = qualify(&path).unwrap_err();
        assert!(refused.retained.is_none());
        assert!(!refused.reason.is_empty());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn conservative_kernel_floor_requires_open_time_checks() {
    for release in ["5.14.0", "5.15.99", "5.4.200", "", "alpha", "6", "5.beta.0"] {
        assert!(!super::linux::kernel_floor(release), "{release}");
    }
    for release in ["5.16.0", "5.19.1-alpha", "6.1.0", "7.0.0"] {
        assert!(super::linux::kernel_floor(release), "{release}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn empty_proof_requires_exact_unique_populated_field() {
    assert_eq!(
        super::linux::populated("populated 0\nfrozen 0\n").unwrap(),
        0
    );
    assert_eq!(super::linux::populated("populated 1\n").unwrap(), 1);
    for events in [
        "",
        "populated 2\n",
        "populated 0\npopulated 1\n",
        "populated 00\n",
    ] {
        assert!(super::linux::populated(events).is_err(), "{events}");
    }
}

#[test]
fn real_kernel_qualification_or_explicit_unqualified_refusal() {
    // Only opt into a real-kernel positive test with a task-private delegated
    // subtree. Never auto-discover, join or alter the host/live fleet subtree.
    if let Some(path) = std::env::var_os("PRIVATEER_TEST_CGROUP_DELEGATION") {
        let path = std::path::PathBuf::from(path);
        let _proof =
            qualify(&path).expect("explicit delegation must qualify; no simulated fallback");
        #[cfg(target_os = "linux")]
        _proof.dispose_test_boundary();
    } else {
        let temp = tempfile::tempdir().unwrap();
        let refused = qualify(temp.path()).unwrap_err();
        eprintln!(
            "unqualified test environment; activation refused: {}",
            refused.reason
        );
        assert!(refused.retained.is_none());
    }
}
