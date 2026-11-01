//! Door v5, INACTIVE until the separate selector activation. Automatic ref
//! and receipt tables stay byte-compatible with v4; history custody lives in
//! tables that kind-unaware readers never inspect.
use super::*;

const CUSTODY: [&str; 6] = [
    "desk_subscription_history_ref",
    "desk_subscription_history_receipt",
    "desk_subscription_selection",
    "desk_subscription_exclusion",
    "desk_subscription_exception",
    "desk_subscription_pin",
];

pub(in crate::door::subscription) fn ready(conn: &Connection) -> Result<bool> {
    Ok(!crate::mail::source::missing(conn, &CUSTODY)?
        && !crate::mail::source::missing(conn, &["desk_replay_revision"])?
        && conn.query_row(
            "SELECT COUNT(*)=2 FROM sqlite_master WHERE type='trigger' AND name IN ('desk_subscription_selection_reset','desk_subscription_selection_retire')",
            [],
            |r| r.get::<_, bool>(0),
        )?)
}

pub(in crate::door::subscription) fn clear(conn: &Connection, id: &str) -> Result<()> {
    for table in CUSTODY {
        conn.execute(&format!("DELETE FROM {table} WHERE subscription=?1"), [id])?;
    }
    Ok(())
}

pub(in crate::door::subscription) fn install(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS desk_subscription_history_ref(
        ref TEXT PRIMARY KEY, subscription TEXT NOT NULL, incarnation TEXT NOT NULL,
        core TEXT NOT NULL, mailbox TEXT NOT NULL, mail TEXT NOT NULL,
        source_order INTEGER NOT NULL, observation TEXT NOT NULL, acked INTEGER NOT NULL DEFAULT 0,
        UNIQUE(subscription,incarnation,core,mailbox,mail,source_order));
      CREATE INDEX IF NOT EXISTS desk_subscription_history_ref_reader ON desk_subscription_history_ref(subscription,incarnation);
      CREATE TABLE IF NOT EXISTS desk_subscription_history_receipt(
        key TEXT PRIMARY KEY, subscription TEXT NOT NULL, incarnation TEXT NOT NULL,
        expires INTEGER NOT NULL, batch TEXT, outcome TEXT);
      CREATE TABLE IF NOT EXISTS desk_replay_revision(
        singleton INTEGER PRIMARY KEY CHECK(singleton=1), revision INTEGER NOT NULL CHECK(revision>=0));
      INSERT OR IGNORE INTO desk_replay_revision VALUES(1,0);
      CREATE TABLE IF NOT EXISTS desk_subscription_selection(
        subscription TEXT NOT NULL, consumed_revision INTEGER NOT NULL DEFAULT 0 CHECK(consumed_revision>=0),
        target_revision INTEGER CHECK(target_revision>=consumed_revision), incarnation TEXT NOT NULL,
        PRIMARY KEY(subscription,incarnation));
      CREATE TABLE IF NOT EXISTS desk_subscription_exclusion(
        subscription TEXT NOT NULL, core TEXT NOT NULL, mailbox TEXT NOT NULL,
        first_order INTEGER NOT NULL CHECK(first_order>0), last_order INTEGER NOT NULL CHECK(last_order>=first_order),
        reasons INTEGER NOT NULL CHECK(reasons BETWEEN 1 AND 7),
        checked_revision INTEGER NOT NULL CHECK(checked_revision>=0),
        through_order INTEGER NOT NULL CHECK(through_order>=first_order-1 AND through_order<=last_order),
        scan_revision INTEGER NOT NULL CHECK(scan_revision>=checked_revision), incarnation TEXT NOT NULL,
        PRIMARY KEY(subscription,incarnation,core,mailbox,first_order));
      CREATE INDEX IF NOT EXISTS desk_subscription_exclusion_work ON desk_subscription_exclusion(subscription,incarnation,checked_revision,core,mailbox,first_order);
      CREATE TABLE IF NOT EXISTS desk_subscription_exception(
        subscription TEXT NOT NULL, core TEXT NOT NULL, mailbox TEXT NOT NULL, mail TEXT NOT NULL,
        source_order INTEGER NOT NULL CHECK(source_order>0), observation TEXT NOT NULL,
        bytes INTEGER NOT NULL CHECK(bytes>0),
        delivery_class TEXT NOT NULL CHECK(delivery_class IN ('history','since_switch_on','unknown')),
        cause TEXT NOT NULL CHECK(cause IN ('proof','offer','closure')), incarnation TEXT NOT NULL,
        PRIMARY KEY(subscription,incarnation,core,mailbox,mail,source_order));
      CREATE TABLE IF NOT EXISTS desk_subscription_pin(
        subscription TEXT NOT NULL, core TEXT NOT NULL, mailbox TEXT NOT NULL, mail TEXT NOT NULL,
        delivery_class TEXT NOT NULL CHECK(delivery_class IN ('history','since_switch_on','unknown')),
        offered_order INTEGER NOT NULL CHECK(offered_order>0), closure_order INTEGER,
        closure_acked INTEGER NOT NULL DEFAULT 0 CHECK(closure_acked IN (0,1)), incarnation TEXT NOT NULL,
        PRIMARY KEY(subscription,incarnation,core,mailbox,mail));")?;
    // Also protect rollback to fence-unaware writers: an old visibility reset
    // or terminal retirement must not leave retained bodies in these tables.
    let cleanup = CUSTODY
        .iter()
        .map(|table| format!("DELETE FROM {table} WHERE subscription=OLD.id;"))
        .collect::<String>();
    conn.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS desk_subscription_selection_reset
         AFTER UPDATE OF incarnation ON desk_subscription WHEN OLD.incarnation!=NEW.incarnation
         BEGIN {cleanup} END;
         CREATE TRIGGER IF NOT EXISTS desk_subscription_selection_retire
         AFTER DELETE ON desk_subscription BEGIN {cleanup} END;"
    ))?;
    Ok(())
}
