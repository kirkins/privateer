//! 2B1 runtime helpers, deliberately UNHOOKED from engine serving. Callers own
//! the admitted writer transaction and must roll it back on errors. Offering
//! always reserves exact carry before snapshot/ref custody, so every later ack
//! has split room. History must never call these helpers. 2B2 wires them in.
use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(in crate::door::subscription) enum Choice {
    Hidden,
    Acknowledged,
    Eligible(replay::DeliveryClass),
    Excluded(i64),
}
fn label(raw: String) -> Result<replay::DeliveryClass> {
    Ok(serde_json::from_value(json!(raw))?)
}
fn skipped(conn: &Connection, sub: &Subscription, o: &Observation) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_subscription_exclusion WHERE subscription=?1 AND incarnation=?2 AND core=?3 AND mailbox=?4 AND first_order<=?5 AND last_order>=?5)", params![sub.id,sub.incarnation,o.origin.core,o.origin.mailbox,o.order], |r|r.get(0))?)
}
fn pin(
    conn: &Connection,
    sub: &Subscription,
    o: &Observation,
) -> Result<Option<(replay::DeliveryClass, i64)>> {
    let row: Option<(String,i64)> = conn.query_row("SELECT delivery_class,offered_order FROM desk_subscription_pin WHERE subscription=?1 AND incarnation=?2 AND core=?3 AND mailbox=?4 AND mail=?5", params![sub.id,sub.incarnation,o.origin.core,o.origin.mailbox,o.origin.id], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    row.map(|(raw, order)| Ok((label(raw)?, order))).transpose()
}

pub(in crate::door::subscription) struct Read<'a> {
    conn: &'a Connection,
    sub: &'a Subscription,
    manifest: &'a Manifest,
    context: &'a DelegatedContext,
    proof: replay::Read<'a>,
}
impl<'a> Read<'a> {
    pub(in crate::door::subscription) fn prepare(
        conn: &'a Connection,
        sub: &'a Subscription,
        m: &'a Manifest,
        c: &'a DelegatedContext,
    ) -> Result<Self> {
        writer(conn, sub)?;
        Ok(Self {
            conn,
            sub,
            manifest: m,
            context: c,
            proof: replay::Read::prepare(conn, sub, m)?,
        })
    }
    pub(in crate::door::subscription) fn choice(&self, o: &Observation) -> Result<Choice> {
        let (conn, sub) = (self.conn, self.sub);
        writer(conn, sub)?;
        if !visible(self.manifest, self.context, o) {
            return Ok(Choice::Hidden);
        }
        // Actual acknowledgments win over positive proof and origin pins. The
        // complement of retained exclusions below the prefix is a durable ack.
        let raw_ref: Option<bool> = conn.query_row("SELECT acked FROM desk_subscription_ref WHERE subscription=?1 AND incarnation=?2 AND core=?3 AND mailbox=?4 AND mail=?5 AND source_order=?6",params![sub.id,sub.incarnation,o.origin.core,o.origin.mailbox,o.origin.id,o.order], |r|r.get(0)).optional()?;
        if raw_ref == Some(true) || (o.order <= through(conn, sub, o)? && !skipped(conn, sub, o)?) {
            return Ok(Choice::Acknowledged);
        }
        let carried: Option<(String,String)> = conn.query_row("SELECT observation,delivery_class FROM desk_subscription_exception WHERE subscription=?1 AND incarnation=?2 AND core=?3 AND mailbox=?4 AND mail=?5 AND source_order=?6", params![sub.id,sub.incarnation,o.origin.core,o.origin.mailbox,o.origin.id,o.order], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((body, raw)) = carried {
            ensure!(body == serde_json::to_string(o)?, "conflicting exception");
            return Ok(Choice::Eligible(label(raw)?));
        }
        if let Some((class, offered)) = pin(conn, sub, o)?
            && o.order >= offered
        {
            return Ok(Choice::Eligible(class));
        }
        if raw_ref == Some(false) {
            // Legacy unlabeled pending custody must not be withdrawn by the
            // policy switch or relabeled new merely because proof arrived.
            return Ok(Choice::Eligible(replay::DeliveryClass::Unknown));
        }
        let (class, reason) = self.proof.classify(o)?;
        if class == replay::DeliveryClass::SinceSwitchOn
            || (o.state.as_deref() == Some("open")
                && o.content["to"] == "captain"
                && matches!(o.content["kind"].as_str(), Some("request" | "decision")))
        {
            Ok(Choice::Eligible(class))
        } else {
            Ok(Choice::Excluded(reason))
        }
    }
    /// Reserve exact offered custody and a frozen label BEFORE creating any
    /// snapshot/ref. Origin pins arise only from an actual incoming open offer,
    /// or update an already offered origin to its authoritative closure.
    pub(in crate::door::subscription) fn offer(
        &self,
        o: &Observation,
    ) -> Result<replay::DeliveryClass> {
        let Choice::Eligible(class) = self.choice(o)? else {
            anyhow::bail!("stale_or_unknown")
        };
        let old = pin(self.conn, self.sub, o)?;
        carry(
            self.conn,
            self.sub,
            o,
            class,
            if old.is_some() && o.state.as_deref() == Some("closed") {
                "closure"
            } else {
                "offer"
            },
        )?;
        if old.is_some()
            || (o.state.as_deref() == Some("open")
                && o.content["to"] == "captain"
                && matches!(o.content["kind"].as_str(), Some("request" | "decision")))
        {
            offer_pin(self.conn, self.sub, o, class)?;
        }
        Ok(class)
    }
    pub(in crate::door::subscription) fn exclude(&self, o: &Observation) -> Result<()> {
        if let Choice::Excluded(reasons) = self.choice(o)? {
            exclude(
                self.conn,
                self.sub,
                &Span {
                    core: o.origin.core.clone(),
                    mailbox: o.origin.mailbox.clone(),
                    first: o.order,
                    last: o.order,
                    reasons,
                    checked: revision(self.conn)?,
                    through: o.order - 1,
                    scan: revision(self.conn)?,
                },
            )?;
        }
        Ok(())
    }
    /// Recheck existing origin pins without relying on the ordinary prefix or
    /// latest proof. Returned rows are caller-authorized, bounded carried bodies.
    /// A pressure error rolls back this whole call, preserving the original pin.
    pub(in crate::door::subscription) fn refresh_closures(&self) -> Result<()> {
        writer(self.conn, self.sub)?;
        let origins = self.conn.prepare("SELECT core,mailbox,mail,delivery_class,offered_order FROM desk_subscription_pin WHERE subscription=?1 AND incarnation=?2 LIMIT ?3")?
            .query_map(params![self.sub.id,self.sub.incarnation,WINDOW+1], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(origins.len() <= WINDOW as usize, "backpressure");
        for (core, mailbox, id, raw, offered) in origins {
            let origin = MailIdentity::new(&core, &mailbox, &id)?;
            if let Some(o) = intake::observation(self.conn, self.manifest, &origin)?
                && visible(self.manifest, self.context, &o)
                && o.state.as_deref() == Some("closed")
                && o.order >= offered
            {
                if matches!(self.choice(&o)?, Choice::Acknowledged) {
                    continue;
                }
                let class = label(raw)?;
                carry(self.conn, self.sub, &o, class, "closure")?;
                offer_pin(self.conn, self.sub, &o, class)?;
            }
        }
        Ok(())
    }
    /// At most 128 source rows and 50 spans per call, including empty spans.
    /// On exception pressure, checkpoint only the already reserved prefix and
    /// return false; pending target work survives restart, never consumed early.
    pub(in crate::door::subscription) fn revisit(
        &self,
        row_limit: usize,
        span_limit: usize,
    ) -> Result<bool> {
        ensure!(
            (1..=128).contains(&row_limit) && (1..=50).contains(&span_limit),
            "invalid_bounds"
        );
        writer(self.conn, self.sub)?;
        let state: Option<(i64,Option<i64>)>=self.conn.query_row("SELECT consumed_revision,target_revision FROM desk_subscription_selection WHERE subscription=?1 AND incarnation=?2",params![self.sub.id,self.sub.incarnation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if state == Some((revision(self.conn)?, None)) {
            return Ok(true);
        }
        start_revisit(self.conn, self.sub)?;
        let mut remaining = row_limit;
        for _ in 0..span_limit {
            let Some(work) = revisit(self.conn, self.sub)? else {
                finish_revisit(self.conn, self.sub)?;
                return Ok(true);
            };
            if remaining == 0 {
                return Ok(false);
            }
            let limit = remaining;
            let mut blocked = false;
            let (visited, last) = intake::visit_range(
                self.conn,
                self.manifest,
                &work.core,
                &work.mailbox,
                work.through.saturating_add(1)..=work.last,
                limit,
                |o| {
                    if let Choice::Eligible(class) = self.choice(&o)? {
                        match carry(self.conn, self.sub, &o, class, "proof") {
                            Err(e) if e.to_string() == "backpressure" => {
                                blocked = true;
                                return Ok(false);
                            }
                            result => result?,
                        }
                    }
                    Ok(true)
                },
            )?;
            remaining -= visited;
            let through = if !blocked && visited < limit {
                work.last
            } else {
                last.unwrap_or(work.through)
            };
            if through > work.through {
                checkpoint(self.conn, self.sub, &work, through)?;
            }
            if blocked {
                return Ok(false);
            }
        }
        if revisit(self.conn, self.sub)?.is_none() {
            finish_revisit(self.conn, self.sub)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Process a real automatic ack in the caller's transaction, with carry split
/// reservations if present. No history ack may enter here. The caller also
/// commits the validated raw ref ack/compaction in this same transaction.
pub(in crate::door::subscription) fn acknowledge(
    conn: &Connection,
    sub: &Subscription,
    o: &Observation,
) -> Result<()> {
    writer(conn, sub)?;
    let carried: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_subscription_exception WHERE subscription=?1 AND incarnation=?2 AND core=?3 AND mailbox=?4 AND mail=?5 AND source_order=?6)",params![sub.id,sub.incarnation,o.origin.core,o.origin.mailbox,o.origin.id,o.order],|r|r.get(0))?;
    if carried {
        acknowledge_carried(conn, sub, o)?;
    } else {
        acknowledge_automatic(conn, sub, o)?;
    }
    retire_closed_pins(conn, sub)
}
