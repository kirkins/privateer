use super::*;

#[cfg(unix)]
#[test]
fn explicit_config_validation_fails_closed() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("alpha.conf");
    let path = config.to_str().unwrap();
    assert!(validate_ssh_config("alpha.conf").is_err());
    assert!(validate_ssh_config("/alpha/./beta.conf").is_err());
    assert!(validate_ssh_config("/alpha/../beta.conf").is_err());
    assert!(validate_ssh_config(path).is_err());
    std::fs::write(&config, "Host *\n").unwrap();
    for mode in [0o600, 0o644, 0o400] {
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(mode)).unwrap();
        validate_ssh_config(path).unwrap();
    }
    for mode in [0o620, 0o602, 0o666] {
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(
            validate_ssh_config(path)
                .unwrap_err()
                .to_string()
                .contains("writable")
        );
    }
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = root.path().join("beta.conf");
    symlink(&config, &link).unwrap();
    assert!(validate_ssh_config(link.to_str().unwrap()).is_err());
    assert!(validate_ssh_config(root.path().to_str().unwrap()).is_err());
    // Exercise the actual ownership predicate without requiring chown privileges.
    let uid = unsafe { libc::getuid() };
    assert!(validate_ssh_config_permissions(path, uid.wrapping_add(1), 0o600).is_err());
    // Where available, also exercise metadata from a genuinely different owner.
    if std::fs::metadata("/etc/passwd").is_ok_and(|m| {
        use std::os::unix::fs::MetadataExt;
        m.uid() != uid
    }) {
        assert!(
            validate_ssh_config("/etc/passwd")
                .unwrap_err()
                .to_string()
                .contains("current uid")
        );
    }
    for transport in ["local", "collected"] {
        let peer: Peer = serde_json::from_value(json!({
            "name":"alpha", "transport":transport, "ssh_config":path
        }))
        .unwrap();
        assert!(
            validate(&peer)
                .unwrap_err()
                .to_string()
                .contains("requires SSH")
        );
    }
}

#[test]
fn classify_only_openssh_config_ownership_exit_255() {
    let diagnostic = "Bad owner or permissions on /alpha/ssh config\r\n";
    let error = ssh_config_failure(Some(255), diagnostic).unwrap();
    for expected in ["/alpha/ssh config", "ssh_config", "namespace", "job.start"] {
        assert!(error.contains(expected), "{error}");
    }
    for code in [None, Some(0), Some(1)] {
        assert!(ssh_config_failure(code, diagnostic).is_none());
    }
    for stderr in [
        "Permission denied (publickey).",
        "WARNING: UNPROTECTED PRIVATE KEY FILE!",
        "Bad permissions: ignore key: /alpha/key",
        "Host key verification failed.",
        "Bad owner or permissions on ",
    ] {
        assert!(ssh_config_failure(Some(255), stderr).is_none());
    }
}
