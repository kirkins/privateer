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
