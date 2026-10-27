//! Unacknowledged mail already covered by a successful wake. Coverage is not
//! an acknowledgement, nor proof that a bounded prompt rendered every body.
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct Summary {
    pub count: i64,
    pub oldest_seq: Option<i64>,
    pub woken_through_seq: i64,
}

pub(crate) fn read(store: &Store, ship: &str) -> Result<Summary> {
    // Use the same durable coverage as activation::unwoken. A later failed or
    // running wake must not hide previously covered mail or mark new mail seen.
    // Timestamp age would wrongly include mail arriving during a running wake.
    let woken_through_seq = store.connection().query_row(
        "SELECT COALESCE(MAX(through_seq), 0) FROM wakes WHERE ship=?1 AND status='succeeded'",
        [ship],
        |r| r.get::<_, i64>(0),
    )?;
    let (count, oldest_seq) = store.connection().query_row(
        "SELECT COUNT(*), MIN(seq) FROM mail
         WHERE recipient=?1 AND COALESCE(delivery, 'received')='received'
           AND seq > COALESCE((SELECT through_seq FROM mail_cursor WHERE address=?1), 0)
           AND seq <= ?2",
        params![ship, woken_through_seq],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(Summary {
        count,
        oldest_seq,
        woken_through_seq,
    })
}

pub(crate) fn line(summary: &Summary, ship: &str) -> Option<String> {
    summary.oldest_seq.map(|oldest| {
        format!(
            "Stale unacknowledged mail: {} message(s), oldest seq {oldest}, covered on an earlier successful wake (coverage ends at seq {}) and still needing handling or an ack. Coverage does not prove every body was seen. Read mail.inbox {{\"address\":\"{ship}\"}}; acknowledge only handled mail. A no-action instruction in one message applies only to that message, not to other mail in this or later wakes.",
            summary.count, summary.woken_through_seq
        )
    })
}
