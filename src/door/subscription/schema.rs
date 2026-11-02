use super::*;

pub(in crate::door) fn install(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS desk_subscription(
        id TEXT PRIMARY KEY, domain TEXT NOT NULL, reader TEXT NOT NULL,
        owner TEXT NOT NULL, incarnation TEXT NOT NULL, visibility TEXT NOT NULL,
        UNIQUE(domain,reader));
      CREATE TABLE IF NOT EXISTS desk_subscription_replay(
        subscription TEXT PRIMARY KEY, binding TEXT);
      CREATE TABLE IF NOT EXISTS desk_exposure_cut(
        binding TEXT NOT NULL, source TEXT NOT NULL, home TEXT NOT NULL,
        mailbox TEXT NOT NULL, nonce TEXT NOT NULL, journal_upper INTEGER NOT NULL,
        body TEXT NOT NULL, PRIMARY KEY(binding,source,home,mailbox,nonce));
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
    intake::install_replay_observation(conn)?;
    Ok(())
}

pub(crate) fn init(conn: &Connection) -> Result<()> {
    if !crate::mail::source::missing(
        conn,
        &[
            "desk_subscription",
            "desk_subscription_replay",
            "desk_replay_observation",
            "desk_exposure_cut",
            "desk_exposure_upgrade",
            "desk_subscription_snapshot",
            "desk_subscription_ref",
            "desk_subscription_progress",
            "desk_subscription_receipt",
        ],
    )? && selection::ready(conn)?
    {
        return Ok(());
    }
    super::super::schema::write(conn, || install(conn))
}
