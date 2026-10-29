// Inject platform clock inputs, not the legacy shared clock/session override.
#[test]
fn intake_boot_continuity_survives_real_gateway_and_status_processes_in_both_directions() {
    assert!(cfg!(feature = "test-intake-budget"));
    for owner in ["alpha", "beta"] {
        let (_t, mut a, mut b) = pair();
        let boot = json!({"boot_id":"alpha-boot", "wall_ms":1_000_000, "boot_ms":2000});
        for m in [&mut a, &mut b] {
            m.intake_budget_ms = Some(10_000);
            m.boot_clock = Some(boot.clone());
            assert!(m.intake_clock.is_none(), "no shared legacy clock override");
        }
        intake_notice(if owner == "alpha" { &b } else { &a }, "alpha-boot-retained");
        intake_enroll(&a, owner);
        let core = intake_external_source(if owner == "alpha" { &a } else { &b });
        let covered = completion_settle(&a, if owner == "alpha" { &a } else { &b }, if owner == "alpha" { &b } else { &a }, &core);
        assert_eq!(covered["complete"], true);
        let desk = if owner == "alpha" { &mut a } else { &mut b };
        let db = intake_db(desk);
        let stored: (String, i64) = db.query_row("SELECT session,tick_ms FROM desk_intake_checkpoint WHERE source=?1", [&core], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(stored, ("boot-v1:alpha-boot".into(), 2000));
        db.execute_batch("CREATE TABLE alpha_status_writes(n INTEGER); INSERT INTO alpha_status_writes VALUES(0);
            CREATE TRIGGER alpha_status_checkpoint AFTER UPDATE ON desk_intake_checkpoint BEGIN UPDATE alpha_status_writes SET n=n+1; END;
            CREATE TRIGGER alpha_status_proof AFTER UPDATE ON desk_intake_proof BEGIN UPDATE alpha_status_writes SET n=n+1; END;").unwrap();
        // Each operator read and the accepting gateway are separate processes.
        // No assumption about real wall time or actual boot uptime is needed.
        for _ in 0..2 { assert_eq!(completion_source(desk, &core)["complete"], true); }
        desk.boot_clock = Some(json!({"boot_id":"beta-boot", "wall_ms":1_000_000, "boot_ms":2000}));
        let reboot = completion_source(desk, &core);
        assert_eq!(reboot["complete"], false);
        assert_eq!(reboot["reason"], "contact_not_fresh");
        desk.boot_clock = Some(json!({"boot_id":"alpha-boot", "wall_ms":1_000_000, "boot_ms":1999}));
        assert_eq!(completion_source(desk, &core)["complete"], false, "backwards boot clock");
        desk.boot_clock = Some(json!({"boot_id":"alpha-boot", "wall_ms":1_003_000, "boot_ms":2000}));
        assert_eq!(completion_source(desk, &core)["complete"], false, "wall/boot discontinuity");
        desk.boot_clock = Some(json!({"boot_id":"", "wall_ms":1_000_000, "boot_ms":2000}));
        assert_eq!(completion_source(desk, &core)["complete"], false, "unavailable boot identity");
        desk.boot_clock = Some(boot);
        assert_eq!(completion_source(desk, &core)["complete"], true, "status must not invalidate another process's receipt");
        assert_eq!(db.query_row("SELECT n FROM alpha_status_writes", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(completion_source(desk, &core)["received_at"], covered["received_at"]);
        // Peer placement calls must not evaluate or disclose source diagnostics.
        let reply = desk.gateway("desk.placement.status", json!({"machine":if owner == "alpha" {"beta"} else {"alpha"}}));
        assert!(reply["error"].is_null(), "{reply}");
        assert!(reply["result"].get("source_status").is_none(), "{reply}");
    }
}
