#[test]
fn delegated_file_put_is_scoped_bounded_and_descriptor_safe() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, full) = delegated_issue(&fleet, "full", ALL_DELEGATED_SCOPES, None);
    let (_, observer) = delegated_issue(&fleet, "observer", "fleet:read", None);
    let (_, restricted) = delegated_issue(&fleet, "restricted", "file:transfer", Some("liaison"));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |e| e.first().cloned());
    let state = fleet.root.join("state/doors/fake/state");
    fs::create_dir_all(state.join("nested")).unwrap();
    let source = state.join("nested/source.txt");
    fs::write(&source, "safe content").unwrap();
    let args = json!({"to":"ops","name":"result.txt","content_file":source});
    credential_request(&fleet, &observer, "file.put", args.clone(), "insufficient_scope");
    credential_request(&fleet, &restricted, "file.put", args.clone(), "ship_not_allowed");
    let stored = delegated_ok(&fleet, &full, "file.put", args);
    assert_eq!(fs::read_to_string(stored["path"].as_str().unwrap()).unwrap(), "safe content");
    symlink(&source, state.join("symlink")).unwrap();
    fs::hard_link(&source, state.join("hardlink")).unwrap();
    let outside = fleet.root.join("outside.txt");
    fs::write(&outside, "outside").unwrap();
    symlink(&outside, state.join("escape")).unwrap();
    let fifo = state.join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let oversized = state.join("oversized");
    fs::File::create(&oversized).unwrap().set_len(50 * 1024 * 1024 + 1).unwrap();
    for path in [source, state.join("symlink"), state.join("hardlink"), state.join("escape"), fifo,
        outside, oversized, state.join("../outside.txt")] {
        credential_request(&fleet, &full, "file.put", json!({"to":"ops","name":"denied.txt","content_file":path}), "execution_failed");
    }
    delegated_ok(&fleet, &full, "ships", json!({}));
    stop(supervisor);
}
