//! Door v5, INACTIVE until the separate selector activation. Existing writers
//! omit kind and keep using the automatic namespace; no old row is relabelled.
use super::*;

const REF: &str = "CREATE TABLE IF NOT EXISTS desk_subscription_ref(
    ref TEXT PRIMARY KEY, subscription TEXT NOT NULL, incarnation TEXT NOT NULL,
    core TEXT NOT NULL, mailbox TEXT NOT NULL, mail TEXT NOT NULL,
    source_order INTEGER NOT NULL, observation TEXT NOT NULL, acked INTEGER NOT NULL DEFAULT 0,
    kind TEXT NOT NULL DEFAULT 'automatic' CHECK(kind IN ('automatic','history')),
    UNIQUE(subscription,incarnation,kind,core,mailbox,mail,source_order));";

fn has_kind(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name='kind')"),
        [],
        |r| r.get(0),
    )?)
}

pub(in crate::door::subscription) fn ready(conn: &Connection) -> Result<bool> {
    Ok(!crate::mail::source::missing(
        conn,
        &[
            "desk_replay_revision",
            "desk_subscription_selection",
            "desk_subscription_exclusion",
            "desk_subscription_exception",
            "desk_subscription_pin",
        ],
    )? && has_kind(conn, "desk_subscription_ref")?
        && has_kind(conn, "desk_subscription_receipt")?)
}

pub(in crate::door::subscription) fn install(conn: &Connection) -> Result<()> {
    // The v4 uniqueness key cannot distinguish history and automatic custody.
    // Recreate under the enclosing migration writer/savepoint, using a temp
    // copy rather than renaming the old table (which rewrites grant triggers).
    if !has_kind(conn, "desk_subscription_ref")? {
        conn.execute_batch(
            "CREATE TEMP TABLE desk_ref_v4 AS SELECT * FROM desk_subscription_ref;
            DROP TABLE desk_subscription_ref;",
        )?;
        conn.execute_batch(REF)?;
        conn.execute_batch("INSERT INTO desk_subscription_ref(ref,subscription,incarnation,core,mailbox,mail,source_order,observation,acked)
            SELECT ref,subscription,incarnation,core,mailbox,mail,source_order,observation,acked FROM desk_ref_v4;
            DROP TABLE temp.desk_ref_v4;")?;
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS desk_subscription_ref_reader ON desk_subscription_ref(subscription)")?;
    if !has_kind(conn, "desk_subscription_receipt")? {
        conn.execute_batch("ALTER TABLE desk_subscription_receipt ADD COLUMN kind TEXT NOT NULL DEFAULT 'automatic' CHECK(kind IN ('automatic','history'))")?;
    }
    conn.execute_batch("CREATE TABLE IF NOT EXISTS desk_replay_revision(
        singleton INTEGER PRIMARY KEY CHECK(singleton=1), revision INTEGER NOT NULL CHECK(revision>=0));
      INSERT OR IGNORE INTO desk_replay_revision VALUES(1,0);
      CREATE TABLE IF NOT EXISTS desk_subscription_selection(
        subscription TEXT PRIMARY KEY, consumed_revision INTEGER NOT NULL DEFAULT 0 CHECK(consumed_revision>=0),
        target_revision INTEGER CHECK(target_revision>=consumed_revision));
      CREATE TABLE IF NOT EXISTS desk_subscription_exclusion(
        subscription TEXT NOT NULL, core TEXT NOT NULL, mailbox TEXT NOT NULL,
        first_order INTEGER NOT NULL CHECK(first_order>0), last_order INTEGER NOT NULL CHECK(last_order>=first_order),
        reasons INTEGER NOT NULL CHECK(reasons BETWEEN 1 AND 7),
        checked_revision INTEGER NOT NULL CHECK(checked_revision>=0),
        through_order INTEGER NOT NULL CHECK(through_order>=first_order-1 AND through_order<=last_order),
        scan_revision INTEGER NOT NULL CHECK(scan_revision>=checked_revision),
        PRIMARY KEY(subscription,core,mailbox,first_order));
      CREATE INDEX IF NOT EXISTS desk_subscription_exclusion_work ON desk_subscription_exclusion(subscription,checked_revision,core,mailbox,first_order);
      CREATE TABLE IF NOT EXISTS desk_subscription_exception(
        subscription TEXT NOT NULL, core TEXT NOT NULL, mailbox TEXT NOT NULL, mail TEXT NOT NULL,
        source_order INTEGER NOT NULL CHECK(source_order>0), observation TEXT NOT NULL,
        bytes INTEGER NOT NULL CHECK(bytes>0),
        delivery_class TEXT NOT NULL CHECK(delivery_class IN ('history','since_switch_on','unknown')),
        cause TEXT NOT NULL CHECK(cause IN ('proof','offer','closure')),
        PRIMARY KEY(subscription,core,mailbox,mail,source_order));
      CREATE TABLE IF NOT EXISTS desk_subscription_pin(
        subscription TEXT NOT NULL, core TEXT NOT NULL, mailbox TEXT NOT NULL, mail TEXT NOT NULL,
        delivery_class TEXT NOT NULL CHECK(delivery_class IN ('history','since_switch_on','unknown')),
        offered_order INTEGER NOT NULL CHECK(offered_order>0), closure_order INTEGER,
        closure_acked INTEGER NOT NULL DEFAULT 0 CHECK(closure_acked IN (0,1)),
        PRIMARY KEY(subscription,core,mailbox,mail));")?;
    Ok(())
}
