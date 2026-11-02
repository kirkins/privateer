use super::*;

const EXPOSURE_CUT: &str = "CREATE TABLE IF NOT EXISTS desk_exposure_cut(
        binding TEXT NOT NULL, source TEXT NOT NULL, home TEXT NOT NULL,
        mailbox TEXT NOT NULL, nonce TEXT NOT NULL, journal_upper INTEGER NOT NULL,
        body TEXT NOT NULL, PRIMARY KEY(binding,source,home,mailbox,nonce));";

/// Whether a `desk_exposure_cut` table exists without `journal_upper`. No
/// release created that shape, but a development build did, and `CREATE TABLE
/// IF NOT EXISTS` keeps it, so its indexes then cannot be created.
fn cut_needs_rebuild(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='desk_exposure_cut')
           AND NOT EXISTS(SELECT 1 FROM pragma_table_info('desk_exposure_cut') WHERE name='journal_upper')",
        [],
        |r| r.get(0),
    )?)
}

/// Rebuild an old `desk_exposure_cut` in the current shape. A saved cut's
/// upper bound is in its body (`checkpoint.upper`, which the insert also
/// writes as `journal_upper`); a row without one cannot be ordered, so it is
/// dropped and the next cut is saved again.
fn rebuild_cut(conn: &Connection) -> Result<()> {
    if !cut_needs_rebuild(conn)? {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "{}
         INSERT OR IGNORE INTO desk_exposure_cut_rebuilt
           SELECT binding, source, home, mailbox, nonce,
                  json_extract(body, '$.checkpoint.upper'), body
           FROM desk_exposure_cut
           WHERE json_valid(body) AND json_type(body, '$.checkpoint.upper') = 'integer';
         DROP TABLE desk_exposure_cut;
         ALTER TABLE desk_exposure_cut_rebuilt RENAME TO desk_exposure_cut;",
        EXPOSURE_CUT.replace("desk_exposure_cut(", "desk_exposure_cut_rebuilt(")
    ))?;
    Ok(())
}

pub(in crate::door) fn install(conn: &Connection) -> Result<()> {
    rebuild_cut(conn)?;
    conn.execute_batch(EXPOSURE_CUT)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS desk_subscription(
        id TEXT PRIMARY KEY, domain TEXT NOT NULL, reader TEXT NOT NULL,
        owner TEXT NOT NULL, incarnation TEXT NOT NULL, visibility TEXT NOT NULL,
        UNIQUE(domain,reader));
      CREATE TABLE IF NOT EXISTS desk_subscription_replay(
        subscription TEXT PRIMARY KEY, binding TEXT);
      CREATE INDEX IF NOT EXISTS desk_exposure_cut_order ON desk_exposure_cut(binding,source,journal_upper);
      CREATE INDEX IF NOT EXISTS desk_exposure_cut_mailbox ON desk_exposure_cut(binding,source,mailbox,journal_upper);
      CREATE TABLE IF NOT EXISTS desk_exposure_upgrade(
        binding TEXT NOT NULL, core TEXT NOT NULL, mailbox TEXT NOT NULL,
        id TEXT NOT NULL, body TEXT NOT NULL,
        PRIMARY KEY(binding,core,mailbox,id));
      CREATE TABLE IF NOT EXISTS desk_subscription_snapshot(
        id TEXT PRIMARY KEY, subscription TEXT NOT NULL, operation TEXT NOT NULL,
        expires INTEGER NOT NULL, body TEXT NOT NULL, coverage TEXT NOT NULL, ack_key TEXT NOT NULL);
      CREATE INDEX IF NOT EXISTS desk_subscription_snapshot_reader ON desk_subscription_snapshot(subscription);
      CREATE TABLE IF NOT EXISTS desk_subscription_ref(
        ref TEXT PRIMARY KEY, subscription TEXT NOT NULL, incarnation TEXT NOT NULL,
        core TEXT NOT NULL, mailbox TEXT NOT NULL, mail TEXT NOT NULL,
        source_order INTEGER NOT NULL, observation TEXT NOT NULL, acked INTEGER NOT NULL DEFAULT 0,
        UNIQUE(subscription,incarnation,core,mailbox,mail,source_order));
      CREATE INDEX IF NOT EXISTS desk_subscription_ref_reader ON desk_subscription_ref(subscription);
      CREATE TABLE IF NOT EXISTS desk_subscription_progress(
        subscription TEXT NOT NULL, core TEXT NOT NULL, mailbox TEXT NOT NULL,
        through_order INTEGER NOT NULL, PRIMARY KEY(subscription,core,mailbox));
      CREATE TABLE IF NOT EXISTS desk_subscription_receipt(
        key TEXT PRIMARY KEY, subscription TEXT NOT NULL, expires INTEGER NOT NULL,
        batch TEXT, outcome TEXT);")?;
    selection::install(conn)?;
    Ok(())
}

pub(crate) fn init(conn: &Connection) -> Result<()> {
    if !crate::mail::source::missing(
        conn,
        &[
            "desk_subscription",
            "desk_subscription_replay",
            "desk_exposure_cut",
            "desk_exposure_upgrade",
            "desk_subscription_snapshot",
            "desk_subscription_ref",
            "desk_subscription_progress",
            "desk_subscription_receipt",
        ],
    )? && selection::ready(conn)?
        && !cut_needs_rebuild(conn)?
    {
        return Ok(());
    }
    super::super::schema::write(conn, || install(conn))
}
