use super::*;

#[test]
fn enrichment_interrupt_is_an_empty_undrained_resumable_page() {
    let (_dir, store, c) = tests::setup();
    tests::add(&store, "alpha", "running");
    // Exceed the observation VM budget in the enrichment join, before any DTO
    // is expanded. One million rows exceeds the fixed VM-instruction bound,
    // rather than depending on whether this machine takes 250 ms to scan
    // 300,000 rows. This used to fail the entire page with SQLITE_INTERRUPT.
    store.connection().execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms) SELECT 'beta-'||x,'survey','survey','notice','','{}','normal',x FROM n;").unwrap();
    let page = scan(
        &store,
        "survey",
        &c,
        false,
        0,
        1,
        None,
        None,
        false,
        Store::now_ms(),
    )
    .unwrap();
    assert!(page.jobs.is_empty());
    assert!(page.enrichment_interrupted);
    assert!(!page.complete && !page.drained);
    assert_eq!(page.last, 0, "retry must not skip an interrupted row");
    store.connection().execute("DELETE FROM mail", []).unwrap();
    let retry = scan(
        &store,
        "survey",
        &c,
        false,
        page.last,
        1,
        None,
        None,
        false,
        Store::now_ms(),
    )
    .unwrap();
    assert_eq!(retry.jobs.len(), 1);
    assert!(retry.drained && retry.complete);
    assert!(!retry.enrichment_interrupted);
}
