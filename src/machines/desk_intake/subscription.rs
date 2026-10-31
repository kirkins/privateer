//! Bounded, read-only projection access for the local subscription engine.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Observation {
    pub origin: MailIdentity,
    pub order: i64,
    pub content: Value,
    pub state: Option<String>,
}

/// Must run in the caller's registry transaction under subscription_owner's
/// fence. Stream history rather than materializing its bodies. A subscription
/// prefix can exclude old acknowledged rows in SQL; the consumer bounds only
/// the visible, operation-eligible rows it retains in an immutable snapshot.
/// Never page the mutable source_order projection directly.
pub(crate) fn visit_observations(
    conn: &Connection,
    m: &Manifest,
    subscription: Option<&str>,
    ships: Option<&[String]>,
    thread: Option<(&MailIdentity, &str)>,
    mut visit: impl FnMut(Observation) -> Result<()>,
) -> Result<()> {
    // Filter history before reading bodies into Rust. SQLite may still visit
    // historical rows to evaluate JSON visibility; that work does not consume
    // the snapshot's retained-row/byte budget or allocate a history-sized Vec.
    let mut stmt = conn.prepare("SELECT f.core,f.mailbox,f.id,f.source_order,f.content,f.state,f.source FROM desk_intake_fact f WHERE domain=?1 AND owner_core=?2 AND generation=?3 AND incarnation=?4
        AND (?5 IS NULL OR source_order>COALESCE((SELECT through_order FROM desk_subscription_progress p WHERE p.subscription=?5 AND p.core=f.core AND p.mailbox=f.mailbox),0))
        AND json_type(content,'$.from')='text' AND json_type(content,'$.to')='text'
        AND ((json_extract(content,'$.from')='captain') != (json_extract(content,'$.to')='captain'))
        AND (?6 IS NULL OR EXISTS(SELECT 1 FROM json_each(?6) WHERE value=CASE WHEN json_extract(content,'$.from')='captain' THEN json_extract(content,'$.to') ELSE json_extract(content,'$.from') END))
        AND (?7 IS NULL OR (f.core=?7 AND f.mailbox=?8 AND (f.id=?9 OR json_extract(content,'$.correlation')=?9)))
        ORDER BY f.core,f.mailbox,f.source_order,f.id")?;
    for row in stmt.query_map(
        params![
            m.domain,
            m.owner_core,
            m.generation,
            m.incarnation,
            subscription,
            ships.map(serde_json::to_string).transpose()?,
            thread.map(|(a, _)| &a.core),
            thread.map(|(a, _)| &a.mailbox),
            thread.map(|(_, root)| root),
        ],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
            ))
        },
    )? {
        let (core, mailbox, id, order, content, state, source) = row?;
        ensure!(
            source == core && m.sources.iter().any(|s| s.core == core),
            "integrity_gap"
        );
        visit(Observation {
            origin: MailIdentity::new(&core, &mailbox, &id)?,
            order,
            content: serde_json::from_str(&content)?,
            state,
        })?;
    }
    Ok(())
}

pub(crate) fn observation(
    conn: &Connection,
    m: &Manifest,
    origin: &MailIdentity,
) -> Result<Option<Observation>> {
    let row: Option<(i64, String, Option<String>, String)> = conn.query_row(
        "SELECT source_order,content,state,source FROM desk_intake_fact WHERE domain=?1 AND owner_core=?2 AND generation=?3 AND incarnation=?4 AND core=?5 AND mailbox=?6 AND id=?7",
        params![m.domain,m.owner_core,m.generation,m.incarnation,origin.core,origin.mailbox,origin.id],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).optional()?;
    row.map(|(order, content, state, source)| {
        ensure!(
            source == origin.core && m.sources.iter().any(|s| s.core == source),
            "integrity_gap"
        );
        Ok(Observation {
            origin: origin.clone(),
            order,
            content: serde_json::from_str(&content)?,
            state,
        })
    })
    .transpose()
}

pub(crate) fn source_vector(conn: &Connection, m: &Manifest) -> Vec<Value> {
    // No new contact is fabricated here. The read vector is as of completed
    // source barriers, including local/collect barriers, not on-demand now.
    let Ok(_read_only) = completion::ReadOnly::new(conn) else {
        return Vec::new();
    };
    let clock = transport::process_clock();
    let deadline = transport::contact_deadline();
    m.sources
        .iter()
        .map(|s| {
            completion::snapshot(conn, m, &s.core, &clock, deadline)
                .unwrap_or_else(|_| json!({"id":s.core,"state":"unavailable","complete":false}))
        })
        .collect()
}
