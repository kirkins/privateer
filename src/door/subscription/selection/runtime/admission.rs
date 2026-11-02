//! Side-effect-free offer capacity planning. Prefix construction reserves soft
//! slots/bytes only for retained rows, so early fresh eligibility cannot bury
//! existing carried custody when the durable ledger is full. Final offers and
//! snapshot/ref custody still commit together in the admitted writer.
use super::*;

#[derive(Clone, Copy, Default)]
pub(in crate::door::subscription) struct Cost {
    pub slots: i64,
    pub spans: i64,
    pub bytes: i64,
    pub pins: i64,
}
impl Cost {
    pub(in crate::door::subscription) fn add(&mut self, other: Self) {
        self.slots += other.slots;
        self.spans += other.spans;
        self.bytes += other.bytes;
        self.pins += other.pins;
    }
    pub(in crate::door::subscription) fn remove(&mut self, other: Self) {
        self.slots -= other.slots;
        self.spans -= other.spans;
        self.bytes -= other.bytes;
        self.pins -= other.pins;
    }
    pub(in crate::door::subscription) fn fits(self, used: Self) -> bool {
        used.slots <= self.slots
            && used.spans <= self.spans
            && used.bytes <= self.bytes
            && used.pins <= self.pins
    }
}
impl Read<'_> {
    pub(in crate::door::subscription) fn retired_spans(&self) -> u64 {
        self.retired_spans.get()
    }
    // Retire only old skipped evidence, never a carried observation or an
    // outstanding raw ref. Its complement below the prefix becomes permanently
    // thread-only, NOT a positive acknowledgment or a newness assertion.
    fn retire_span(&self) -> Result<bool> {
        let removed = self.conn.execute("DELETE FROM desk_subscription_exclusion WHERE rowid=(SELECT e.rowid FROM desk_subscription_exclusion e JOIN desk_subscription_progress p ON p.subscription=e.subscription AND p.core=e.core AND p.mailbox=e.mailbox WHERE e.subscription=?1 AND e.incarnation=?2 AND e.last_order<p.through_order AND NOT EXISTS(SELECT 1 FROM desk_subscription_exception x WHERE x.subscription=e.subscription AND x.incarnation=e.incarnation AND x.core=e.core AND x.mailbox=e.mailbox AND x.source_order BETWEEN e.first_order AND e.last_order) AND NOT EXISTS(SELECT 1 FROM desk_subscription_ref r WHERE r.subscription=e.subscription AND r.incarnation=e.incarnation AND r.core=e.core AND r.mailbox=e.mailbox AND r.acked=0 AND r.source_order BETWEEN e.first_order AND e.last_order) ORDER BY e.last_order,e.core,e.mailbox,e.first_order LIMIT 1)", params![self.sub.id,self.sub.incarnation])?;
        self.retired_spans
            .set(self.retired_spans.get() + removed as u64);
        Ok(removed != 0)
    }
    pub(in crate::door::subscription) fn capacity_for(&self, used: Cost) -> Result<Cost> {
        let mut capacity = self.capacity()?;
        while capacity.spans < used.spans && self.retire_span()? {
            capacity = self.capacity()?;
        }
        Ok(capacity)
    }
    /// Exclusion is optional work under pressure, never a rollback loop. Charge
    /// new spans against the soft reservations of already retained offers. Ack
    /// compaction cannot retire evidence: only a pull freezes retirement counts.
    pub(in crate::door::subscription) fn try_exclude(
        &self,
        o: &Observation,
        reserved: i64,
        retire: bool,
    ) -> Result<bool> {
        if !self.bound {
            return Ok(true); // Immutable NULL binding: old skips stay thread-only.
        }
        loop {
            self.conn
                .execute_batch("SAVEPOINT subscription_exclusion_admission")?;
            let result = self.exclude(o);
            let fits = result.is_ok() && self.capacity()?.spans >= reserved;
            self.conn.execute_batch(if fits {
                "RELEASE subscription_exclusion_admission"
            } else {
                "ROLLBACK TO subscription_exclusion_admission; RELEASE subscription_exclusion_admission"
            })?;
            if let Err(e) = result
                && e.to_string() != "backpressure"
            {
                return Err(e);
            }
            if fits {
                return Ok(true);
            }
            if !retire || !self.retire_span()? {
                return Ok(false);
            }
        }
    }
    pub(in crate::door::subscription) fn capacity(&self) -> Result<Cost> {
        writer(self.conn, self.sub)?;
        let bytes:i64=self.conn.query_row("SELECT COALESCE(SUM(bytes),0) FROM desk_subscription_exception WHERE subscription=?1 AND incarnation=?2", params![self.sub.id,self.sub.incarnation], |r|r.get(0))?;
        Ok(Cost {
            slots: WINDOW - count(self.conn, self.sub, "desk_subscription_exception")?,
            spans: SPANS - span_slots(self.conn, self.sub)?,
            bytes: EXCEPTION_BYTES - bytes,
            pins: WINDOW - count(self.conn, self.sub, "desk_subscription_pin")?,
        })
    }
    pub(in crate::door::subscription) fn cost(&self, o: &Observation) -> Result<Cost> {
        if acknowledged(self.conn, self.sub, o)? {
            return Ok(Cost::default());
        }
        let carried:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM desk_subscription_exception WHERE subscription=?1 AND incarnation=?2 AND core=?3 AND mailbox=?4 AND mail=?5 AND source_order=?6)",params![self.sub.id,self.sub.incarnation,o.origin.core,o.origin.mailbox,o.origin.id,o.order], |r|r.get(0))?;
        let pins = i64::from(
            pin(self.conn, self.sub, o)?.is_none()
                && o.state.as_deref() == Some("open")
                && o.content["to"] == "captain"
                && matches!(o.content["kind"].as_str(), Some("request" | "decision")),
        );
        // Use the longest class/cause; classification labels already frozen in
        // an existing carry cost no new bytes, and no home is opened here.
        let bytes = if carried {
            0
        } else {
            serde_json::to_vec(&json!({"subscription":self.sub.id,"incarnation":self.sub.incarnation,"origin":o.origin,"observation":o,"delivery_class":"since_switch_on","cause":"closure"}))?.len() as i64
        };
        Ok(Cost {
            slots: i64::from(!carried),
            spans: i64::from(!carried),
            bytes,
            pins,
        })
    }
}
