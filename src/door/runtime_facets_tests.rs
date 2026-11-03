use super::*;

fn row(mode: &str) -> Value {
    json!({"name":"alpha","machine":if mode == "local_live" { "local" } else { "beta" },
        "runtime":{"mode":mode,"current":matches!(mode,"local_live"|"remote_live"|"reused"),"age_ms":42,
            "facts":{"open_tasks":0,"jobs_total":0,"jobs_truncated":false,
                "census":{"activity_complete":true,"workers_complete":true,"sessions_complete":true,
                    "reason":"identity_checked_census"}}}})
}

#[test]
fn read_facets_keep_completeness_separate_from_freshness_and_zero() {
    for mode in ["local_live", "remote_live", "reused", "cache", "carried"] {
        let mut r = row(mode);
        ship(&mut r, false, None);
        for facet in ["census", "jobs", "backlog"] {
            assert_eq!(r["facets"][facet]["state"], "read", "{mode} {facet}");
            assert_eq!(
                r["facets"][facet]["current"],
                matches!(mode, "local_live" | "remote_live" | "reused")
            );
            assert_eq!(
                r["facets"][facet]["provenance"],
                match mode {
                    "local_live" | "remote_live" => "fresh",
                    "carried" => "carried",
                    _ => "cached",
                }
            );
        }
        assert_eq!(r["facets"]["directory"]["state"], "read");
    }
}

#[test]
fn missing_and_truncated_facets_never_become_read_zeroes() {
    for truncated in [false, true] {
        let mut r = json!({"name":"alpha","machine":"beta"});
        ship(&mut r, truncated, None);
        for facet in ["census", "jobs", "backlog"] {
            assert_eq!(
                r["facets"][facet]["state"],
                if truncated { "truncated" } else { "missing" }
            );
            assert_eq!(r["facets"][facet]["current"], false);
            assert!(r["facets"][facet]["age_ms"].is_null());
        }
    }
    let mut r = row("remote_live");
    r["runtime"]["facts"]["jobs_truncated"] = json!(true);
    r["runtime"]["facts"]["census"]["sessions_complete"] = json!(false);
    ship(&mut r, false, None);
    assert_eq!(r["facets"]["census"]["state"], "truncated");
    assert_eq!(r["facets"]["jobs"]["state"], "truncated");
    assert_eq!(r["facets"]["backlog"]["state"], "read");
    let mut r = json!({"name":"alpha","machine":"local","open_tasks":0});
    ship(&mut r, false, None);
    assert_eq!(r["facets"]["backlog"]["state"], "read");
    assert_eq!(r["facets"]["census"]["state"], "missing");
}

#[test]
fn contacts_do_not_imply_home_reads_and_dated_reverse_only_is_not_current() {
    let mut contact = omitted("beta", "reverse_only");
    contact["reverse_only"] = json!(true);
    for received in [Value::Null, json!(123)] {
        contact["received_at"] = received.clone();
        let mut r = json!({"name":"alpha","machine":"beta"});
        ship(&mut r, false, Some(&contact));
        assert_eq!(
            r["facets"]["contact"]["state"],
            if received.is_null() {
                "missing"
            } else {
                "read"
            }
        );
        assert_eq!(r["facets"]["contact"]["current"], false);
        assert!(r["facets"]["contact"]["age_ms"].is_null());
    }
    let mut r = json!({"name":"alpha","machine":"beta"});
    contact["successful"] = json!(true);
    ship(&mut r, true, Some(&contact));
    assert_eq!(r["facets"]["contact"]["state"], "read");
    assert_eq!(r["facets"]["census"]["state"], "truncated");
    assert_eq!(r["facets"]["jobs"]["state"], "truncated");
}
