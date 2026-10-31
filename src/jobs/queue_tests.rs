//! Queue selection and admission use fixed readings, without clocks or processes.
use super::*;

fn entry(key: &str, demand: Option<Demand>, blocked: Option<Blocker>) -> Queued {
    Queued {
        key: key.into(),
        home: PathBuf::from("alpha"),
        job_id: key.into(),
        incarnation: "alpha".into(),
        created_ms: 0,
        supervisor: None,
        kind: None,
        capacity: Some(QueuedDemand {
            heavy: false,
            demand,
        }),
        blocked,
        blocked_ms: None,
    }
}

fn demand(memory_bytes: u64, disk_bytes: u64) -> Option<Demand> {
    Some(Demand {
        cpus: 0.0,
        memory_bytes,
        disk_bytes,
    })
}

fn host() -> admission::Host {
    admission::Host {
        cpus: 4.0,
        load: 0.0,
        memory_available_bytes: 64,
        memory_total_bytes: 256,
        disk_available_bytes: 100,
        disk_total_bytes: 200,
    }
}

fn settings() -> admission::Settings {
    admission::Settings {
        min_free_disk_bytes: 16,
        min_available_memory_bytes: 16,
        ..Default::default()
    }
}

fn admit(host: &admission::Host, demand: Option<Demand>, charter_floor: u64) -> Result<()> {
    // Another estimated job makes the candidate's own memory count. This
    // reservation and all readings have explicit, fixed times and sizes.
    let held = admission::Held {
        heavy: false,
        home: Path::new("alpha"),
        charter: Some("alpha"),
        created_ms: 0,
        demand: Some(Demand::default()),
        disk_key: Some("alpha"),
        disk_free_at_admit: Some(host.disk_available_bytes),
        disk_trimmed: 0,
        job_id: "alpha",
    };
    admission::admit(
        &settings(),
        host,
        0,
        &[held],
        &admission::Request {
            heavy: false,
            home: Path::new("alpha"),
            charter: "alpha",
            charter_cap: None,
            demand,
            charter_min_free_bytes: charter_floor,
            disk_key: "alpha",
            disk_available_bytes: host.disk_available_bytes,
            disk_total_bytes: host.disk_total_bytes,
            released_load: 0.0,
            started_by: None,
        },
    )
}

fn next(queue: &[Queued], host: &admission::Host) -> Option<usize> {
    queue.iter().position(|q| {
        let c = q.capacity.unwrap();
        queue_allows(queue, &q.key, c.demand, c.heavy) && admit(host, c.demand, 0).is_ok()
    })
}

#[test]
fn zero_disk_job_passes_several_disk_needing_entries_but_they_do_not_pass() {
    let host = host();
    let head = demand(0, 100);
    let error = admit(&host, head, 0).unwrap_err();
    assert_eq!(
        error.downcast_ref::<Refused>().unwrap().blocker,
        Some(Blocker::Disk)
    );
    let mut queue = vec![entry("alpha", head, Some(Blocker::Disk))];
    for key in ["beta", "gamma", "delta"] {
        let d = demand(0, 1);
        assert!(admit(&host, d, 0).is_ok());
        queue.push(entry(key, d, None));
        assert!(!queue_allows(&queue, key, d, false));
    }
    // Resolve an explicit disk_bytes:0 just as reserve does, then persist and
    // reload every entry before choosing. Non-head demand must survive restart.
    let zero_disk = admission::demand(
        &settings(),
        &host,
        false,
        true,
        Some(&Estimate {
            disk_bytes: Some(0),
            ..Default::default()
        }),
    );
    queue.push(entry("epsilon", zero_disk, None));
    let mut queue: Vec<Queued> =
        serde_json::from_str(&serde_json::to_string(&queue).unwrap()).unwrap();
    assert_eq!(next(&queue, &host), Some(4));
    queue.remove(4);
    assert_eq!(next(&queue, &host), None);
}

#[test]
fn a_large_light_passer_cannot_be_repeatedly_overtaken_by_smaller_passers() {
    let mut host = host();
    let large = demand(64, 0);
    let mut queue = vec![
        entry("alpha", demand(0, 100), Some(Blocker::Disk)),
        entry("beta", demand(0, 1), None),
        entry("gamma", large, None),
    ];
    assert!(queue_allows(&queue, "gamma", large, false));
    assert!(admit(&host, large, 0).is_err());
    for size in 1..32 {
        let small = demand(size, 0);
        assert!(admit(&host, small, 0).is_ok());
        // Both an existing queued request and a direct arrival respect gamma.
        queue.push(entry("delta", small, None));
        assert!(!queue_allows(&queue, "delta", small, false));
        assert!(!queue_allows(&queue, "epsilon", small, false));
        assert_eq!(next(&queue, &host), None);
        queue.pop();
    }
    queue.push(entry("delta", demand(1, 0), None));
    host.memory_available_bytes = 128;
    assert_eq!(next(&queue, &host), Some(2));
    queue.remove(2);
    assert_eq!(next(&queue, &host), Some(2));
    assert_eq!(queue[2].key, "delta");
}

#[test]
fn a_memory_blocked_head_cannot_be_passed_by_memory_needing_work() {
    let host = host();
    let head = demand(128, 0);
    let error = admit(&host, head, 0).unwrap_err();
    // Memory refusals deliberately have no passable blocker.
    let blocker = error.downcast_ref::<Refused>().unwrap().blocker;
    assert_eq!(blocker, None);
    let queue = vec![
        entry("alpha", head, blocker),
        entry("beta", demand(1, 0), None),
    ];
    assert!(admit(&host, demand(1, 0), 0).is_ok());
    assert!(!queue_allows(&queue, "beta", demand(1, 0), false));
    assert_eq!(next(&queue, &host), None);
}

#[test]
fn bypass_eligibility_does_not_override_disk_memory_or_cpu_admission() {
    let queue = vec![entry("alpha", demand(0, 100), Some(Blocker::Disk))];
    let zero_disk = demand(0, 0);
    assert!(queue_allows(&queue, "beta", zero_disk, false));
    assert!(admit(&host(), zero_disk, 0).is_ok());
    let disk_low = admission::Host {
        disk_available_bytes: 15,
        ..host()
    };
    assert!(admit(&disk_low, zero_disk, 0).is_err());
    assert!(admit(&host(), zero_disk, 101).is_err()); // charter floor
    let memory_low = admission::Host {
        memory_available_bytes: 15,
        ..host()
    };
    assert!(admit(&memory_low, zero_disk, 0).is_err());
    assert!(admit(&host(), demand(49, 0), 0).is_err());
    let cpu_busy = admission::Host {
        load: 4.0,
        ..host()
    };
    let cpu = Some(Demand {
        cpus: 1.0,
        ..Default::default()
    });
    assert!(queue_allows(&queue, "beta", cpu, false));
    assert!(admit(&cpu_busy, cpu, 0).is_err());
}

#[test]
fn unknown_entries_keep_priority_and_workspaces_are_known_disk_waiters() {
    let mut queue = vec![entry("alpha", demand(0, 100), Some(Blocker::Disk))];
    let mut old = serde_json::to_value(entry("beta", demand(0, 1), None)).unwrap();
    old.as_object_mut().unwrap().remove("capacity");
    queue.push(serde_json::from_value(old).unwrap());
    assert!(!queue_allows(&queue, "gamma", demand(0, 0), false));
    queue[1].kind = Some("workspace".into());
    queue[1].blocked = Some(Blocker::Disk);
    assert!(queue_allows(&queue, "gamma", demand(0, 0), false));
    // Even an unestimated light job is a passer that must not be overtaken.
    queue[1] = entry("beta", None, None);
    assert!(!queue_allows(&queue, "gamma", demand(0, 0), false));
}

#[test]
fn passers_keep_fifo_for_cpu_and_heavy_slot_heads_too() {
    for blocker in [Blocker::Cpu, Blocker::HeavySlot] {
        let mut heavy = entry(
            "beta",
            Some(Demand {
                cpus: 1.0,
                ..Default::default()
            }),
            None,
        );
        heavy.capacity.as_mut().unwrap().heavy = true;
        let mut queue = vec![entry("alpha", None, Some(blocker)), heavy];
        let light = demand(1, 0);
        assert!(queue_allows(&queue, "gamma", light, false));
        queue.push(entry("gamma", light, None));
        assert!(!queue_allows(&queue, "delta", light, false));
        assert!(!queue_allows(&queue, "delta", None, true));
    }
}
