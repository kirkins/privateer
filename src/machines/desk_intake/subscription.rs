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
/// fence. Never page the mutable source_order projection directly.
pub(crate) fn observations(conn: &Connection, m: &Manifest) -> Result<Vec<Observation>> {
    let mut stmt = conn.prepare("SELECT core,mailbox,id,source_order,content,state,source FROM desk_intake_fact WHERE domain=?1 AND owner_core=?2 AND generation=?3 AND incarnation=?4 ORDER BY core,mailbox,source_order,id LIMIT 10001")?;
    let mut out = Vec::new();
    let mut bytes = 0;
    for row in stmt.query_map(
        params![m.domain, m.owner_core, m.generation, m.incarnation],
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
        bytes += content.len();
        ensure!(
            out.len() < 10_000 && bytes <= 16 * 1024 * 1024,
            "backpressure"
        );
        ensure!(
            source == core && m.sources.iter().any(|s| s.core == core),
            "integrity_gap"
        );
        out.push(Observation {
            origin: MailIdentity::new(&core, &mailbox, &id)?,
            order,
            content: serde_json::from_str(&content)?,
            state,
        });
    }
    Ok(out)
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
