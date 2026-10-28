use super::*;

#[cfg(target_os = "linux")]
#[test]
fn disk_readings_are_per_filesystem_and_unknown_keys_stay_unknown() {
    let (_dir, store, ledger, registry, c) = tests::fixture();
    let other = tempfile::tempdir_in("/dev/shm").unwrap();
    let home_key = crate::admission::disk_key(store.home()).unwrap();
    let other_key = crate::admission::disk_key(other.path()).unwrap();
    assert_ne!(home_key, other_key, "fixture needs distinct filesystems");
    tests::hold(&ledger, &store, "alpha", "survey", true);
    tests::hold(&ledger, &store, "beta", "relay", false);
    ledger.execute("UPDATE reservations SET record=json_set(record,'$.disk_key',?1,'$.disk_free_bytes',?2) WHERE key='alpha'",rusqlite::params![home_key,crate::admission::disk(store.home()).unwrap().0]).unwrap();
    ledger.execute("UPDATE reservations SET record=json_set(record,'$.home',?1,'$.disk_key',?2,'$.disk_free_bytes',?3) WHERE key='beta'",rusqlite::params![other.path().to_str().unwrap(),other_key,crate::admission::disk(other.path()).unwrap().0]).unwrap();
    let args = json!({"machine":"local","ship":"survey"});
    let cap = local_read(&registry, Command::MachineCapacity, &args, &c)
        .unwrap()
        .capacity
        .unwrap();
    let disks = &cap.readings.unwrap().disks;
    assert_eq!(disks.len(), 2);
    assert!(
        disks
            .iter()
            .all(|d| d.state == "covered" && d.committed_bytes == Some(200))
    );
    // A recorded target device without a safe measurable path is a distinct
    // unknown reading, not silently measured on the root filesystem.
    ledger.execute("UPDATE reservations SET record=json_set(record,'$.disk_key','unmounted-canary') WHERE key='beta'",[]).unwrap();
    let cap = local_read(&registry, Command::MachineCapacity, &args, &c)
        .unwrap()
        .capacity
        .unwrap();
    let disks = &cap.readings.unwrap().disks;
    assert_eq!(disks.len(), 3);
    assert_eq!(
        disks
            .iter()
            .filter(|d| d.state == "unknown"
                && d.available_bytes.is_none()
                && d.committed_bytes.is_none())
            .count(),
        1
    );
}
