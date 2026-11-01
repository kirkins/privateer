//! One predicate for local-owner and remote intake. No mutation on reads.
use super::*;

#[derive(Serialize, Deserialize, PartialEq)]
struct First {
    source: String,
    order: i64,
    kind: String,
    digest: String,
}
#[derive(Serialize, Deserialize)]
struct Upgrade {
    first: First,
    witness: Witness,
}
fn first(c: &Connection, m: &Manifest, o: &MailIdentity) -> Result<Option<First>> {
    let row: Option<(Option<String>,i64,Option<String>,String)> = c.query_row(
        "SELECT o.source,o.first_order,o.first_kind,f.content FROM desk_intake_original o JOIN desk_intake_fact f USING(domain,core,mailbox,id)
        WHERE o.domain=?1 AND o.owner_core=?2 AND o.generation=?3 AND o.incarnation=?4 AND o.core=?5 AND o.mailbox=?6 AND o.id=?7
        AND f.owner_core=o.owner_core AND f.generation=o.generation AND f.incarnation=o.incarnation",
        params![m.domain,m.owner_core,m.generation,m.incarnation,o.core,o.mailbox,o.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    Ok(match row {
        Some((Some(source), order, Some(kind), content))
            if matches!(kind.as_str(), "native" | "copy") && order > 0 =>
        {
            Some(First {
                source,
                order,
                kind,
                digest: crate::core::hash(&serde_json::from_str::<Value>(&content)?),
            })
        }
        _ => None,
    })
}
fn witness(c: &Connection, m: &Manifest, o: &MailIdentity, f: &First) -> Result<Option<Witness>> {
    let row: Option<(String,i64,String,String)> = c.query_row(
        "SELECT first_source,first_order,first_kind,body FROM desk_replay_adopted WHERE domain=?1 AND core=?2 AND mailbox=?3 AND id=?4 AND binding=?5",
        params![m.domain,o.core,o.mailbox,o.id,binding(m)?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((source, order, kind, body)) = row else {
        return Ok(None);
    };
    let w: Witness = serde_json::from_str(&body)?;
    Ok((source == f.source
        && order == f.order
        && kind == f.kind
        && w.origin == *o
        && w.source == o.core
        && w.binding == binding(m)?
        && w.mailbox == o.mailbox
        && w.digest == f.digest)
        .then_some(w))
}
fn order_boundary(b: &Value, source: &str) -> Option<i64> {
    // Old coverage is negative evidence only; it never establishes a replay
    // cut. Compare the saved supplying source, never the origin's namespace.
    let s = b["switch_on"]
        .as_array()?
        .iter()
        .find(|s| s["source"] == source && s["state"] == "established")?;
    let (id, order) = s["through"].as_str()?.rsplit_once(':')?;
    if uuid::Uuid::parse_str(id).ok()?.to_string() != id {
        return None;
    }
    order.parse::<i64>().ok().filter(|j| *j >= 0)
}
fn history(
    c: &Connection,
    m: &Manifest,
    b: &Value,
    o: &MailIdentity,
    f: &First,
    w: Option<&Witness>,
) -> Result<Option<&'static str>> {
    if !m.sources.iter().any(|s| s.core == f.source) {
        return Ok(None);
    }
    if order_boundary(b, &f.source).is_some_and(|upper| f.order <= upper) {
        return Ok(Some("history"));
    }
    let key = crate::core::hash(b);
    let mut delayed = false;
    // For a native first tuple the origin mailbox identifies its own home.
    // A copy's supplying mailbox is unknown: only the earliest source-wide
    // boundary is safe, never a later cut from an unrelated home.
    let native =
        (f.kind == "native" && f.source == o.core).then_some((o.mailbox.as_str(), f.order, w));
    let earliest = order_cut(c, &key, &f.source, None)?;
    let mapped = native
        .map(|native| order_cut(c, &key, &f.source, Some(native)))
        .transpose()?
        .flatten();
    for cut in [earliest, mapped].into_iter().flatten() {
        if f.order <= cut.checkpoint.upper {
            if !cut.delayed {
                return Ok(Some("history"));
            }
            delayed = true;
        }
    }
    // Exact native evidence at/below its effective home cut also vetoes a
    // copy-first upgrade, without comparing its J to the supplying copy's J.
    if let Some(w) = w
        && let Some(cut) = witness_cut(c, &key, w)?
        && (w.journal_order <= cut.checkpoint.upper
            || w.delta_order <= cut.home.delta_upper
            || w.mail_order <= cut.home.mail_upper)
    {
        if !cut.delayed {
            return Ok(Some("history"));
        }
        delayed = true;
    }
    Ok(delayed.then_some("delayed_cut"))
}
fn witness_cut(c: &Connection, key: &str, w: &Witness) -> Result<Option<SavedCut>> {
    find(
        c,
        key,
        &w.source,
        &Home {
            home: w.home.clone(),
            mailbox: w.mailbox.clone(),
            nonce: w.nonce.clone(),
            baseline_upper: 0,
            delta_upper: 0,
            mail_upper: 0,
        },
    )
}
/// Local-owner calls currently revalidate live on every classification.
/// Before reader exposure, 4b-2 must revalidate once per page or read and
/// share that result across its items; never multiply home I/O by page size.
fn positive(c: &Connection, m: &Manifest, b: &Value, w: &Witness) -> Result<bool> {
    if !m.sources.iter().any(|s| s.core == w.source) {
        return Ok(false);
    }
    let Some(latest) = latest(c, m, &w.source)? else {
        return Ok(false);
    };
    if !current(c, m, &w.source, &latest)? {
        return Ok(false);
    }
    let key = crate::core::hash(b);
    let Some(cut) = witness_cut(c, &key, w)? else {
        return Ok(false);
    };
    let Some(now) = latest.homes.iter().find(|h| same_home(h, &cut.home)) else {
        return Ok(false);
    };
    // Ordinary advancement is legal; regressed or replaced enrollment and
    // publication identities withhold proof without recapturing the cut.
    if now.baseline_upper != cut.home.baseline_upper
        || now.delta_upper < cut.home.delta_upper
        || now.mail_upper < cut.home.mail_upper
        || latest.evidence.checkpoint.issued < cut.checkpoint.issued
        || latest.evidence.checkpoint.upper < cut.checkpoint.upper
    {
        return Ok(false);
    }
    let proved = Cut {
        checkpoint_digest: crate::core::hash(&json!(cut.checkpoint)),
        binding: binding(m)?,
        source: w.source.clone(),
        home: cut.home.home.clone(),
        mailbox: cut.home.mailbox.clone(),
        nonce: cut.home.nonce.clone(),
        journal_upper: cut.checkpoint.upper,
        delta_upper: cut.home.delta_upper,
        mail_upper: cut.home.mail_upper,
    };
    if w.beyond(&proved)
        && w.journal_order <= latest.evidence.checkpoint.upper
        && w.delta_order <= now.delta_upper
        && w.mail_order <= now.mail_upper
    {
        return Ok(true);
    }
    Ok(false)
}

pub(super) fn link(c: &Connection, m: &Manifest, b: &Value, w: &Witness) -> Result<()> {
    let Some(f) = first(c, m, &w.origin)? else {
        return Ok(());
    };
    if f.kind != "copy" {
        return Ok(());
    }
    let Some(kept) = witness(c, m, &w.origin, &f)? else {
        return Ok(());
    };
    if kept != *w || history(c, m, b, &w.origin, &f, Some(w))?.is_some() || !positive(c, m, b, w)? {
        return Ok(());
    }
    // Additive link preserves the exact original tuple, including same-source
    // copy-first. The own-source witness is its separate committed ledger
    // tuple, not a replacement first_order or a cross-source comparison.
    c.execute(
        "INSERT OR IGNORE INTO desk_exposure_upgrade VALUES(?1,?2,?3,?4,?5)",
        params![
            crate::core::hash(b),
            w.origin.core,
            w.origin.mailbox,
            w.origin.id,
            serde_json::to_string(&Upgrade {
                first: f,
                witness: w.clone()
            })?
        ],
    )?;
    Ok(())
}

pub(in crate::machines::desk_intake) fn classify(
    c: &Connection,
    m: &Manifest,
    b: &Value,
    o: &MailIdentity,
) -> Result<&'static str> {
    if !matches_binding(b, m)? {
        return Ok("unknown");
    }
    let Some(f) = first(c, m, o)? else {
        return Ok("unknown");
    };
    let w = witness(c, m, o, &f)?;
    if let Some(reason) = history(c, m, b, o, &f, w.as_ref())? {
        return Ok(reason);
    }
    let Some(w) = w else { return Ok("unknown") };
    if !positive(c, m, b, &w)? {
        return Ok("unknown");
    }
    if f.kind == "native" && f.source == w.source && f.order == w.journal_order {
        return Ok("since_switch_on");
    }
    if f.kind == "copy" {
        let raw: Option<String> = c.query_row("SELECT body FROM desk_exposure_upgrade WHERE binding=?1 AND core=?2 AND mailbox=?3 AND id=?4",params![crate::core::hash(b),o.core,o.mailbox,o.id],|r|r.get(0)).optional()?;
        if let Some(raw) = raw {
            let link: Upgrade = serde_json::from_str(&raw)?;
            if link.first == f && link.witness == w {
                return Ok("since_switch_on");
            }
        }
    }
    Ok("unknown")
}
