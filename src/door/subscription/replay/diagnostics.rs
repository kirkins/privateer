//! Core-authored selection diagnostics, separate from coverage.v1. No raw
//! binding, source names, home handles, mail origins or peer reasons escape.
use super::*;

#[derive(Default)]
pub(in crate::door::subscription) struct Diagnostics {
    pub history: u64,
    pub delayed: u64,
    pub unknown: u64,
    pub work_pending: bool,
}
impl Diagnostics {
    pub(in crate::door::subscription) fn exclude(&mut self, reasons: i64) {
        if reasons & selection::HISTORY != 0 {
            self.history += 1;
        }
        if reasons & selection::DELAYED != 0 {
            self.delayed += 1;
        }
        if reasons & selection::UNKNOWN != 0 {
            self.unknown += 1;
        }
    }
    pub(in crate::door::subscription) fn freeze(
        &self,
        conn: &Connection,
        sub: &Subscription,
        m: &Manifest,
        c: &DelegatedContext,
        read: &selection::runtime::Read<'_>,
        coverage: &Value,
    ) -> Result<Value> {
        let binding = binding(conn, sub)?;
        let raw = binding
            .as_ref()
            .map(|b| intake::replay_cuts(conn, m, b))
            .transpose()?
            .unwrap_or(json!({"sources":[]}));
        let restricted = c.ships().is_some();
        let mut sources = Vec::new();
        let mut pending = self.work_pending || self.delayed != 0 || self.unknown != 0;
        for visible in coverage["sources"].as_array().into_iter().flatten() {
            let core: String = conn.query_row(
                "SELECT core FROM door_coverage_source WHERE domain=?1 AND handle=?2",
                params![m.domain, visible["id"].as_str()],
                |r| r.get(0),
            )?;
            // A partially visible source can contain hidden homes/cuts. Withhold
            // even their counts/state; never leak them through retry/completeness.
            if restricted {
                sources.push(json!({"id":visible["id"],"state":"withheld","cuts":null,"peer":null,"proof_current":null,"retained_cuts_omitted":null}));
                continue;
            }
            let status = raw["sources"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|s| s["source"] == core);
            let proof_current = read.source_current(&core)?;
            let peer = intake::replay_observation(conn, m, &core)?;
            let state = status
                .map(|s| s["state"].clone())
                .unwrap_or(json!("switch_on_pending"));
            let truncated = status.is_some_and(|s| s["retained_cuts_omitted"] == true);
            pending |= state != "established" || !proof_current || truncated;
            let cuts: Vec<Value> = status
                .into_iter()
                .flat_map(|s| s["cuts"].as_array().into_iter().flatten())
                .map(|cut| {
                    json!({"received_at":cut["received_at"],"age_ms":cut["age_ms"],
                    "adopted_at":cut["adopted_at"],"age_at_adoption_ms":cut["age_at_adoption_ms"],
                    "max_source_age_ms":cut["max_source_age_ms"],"delayed":cut["delayed"],
                    "unproved_interval":cut["unproved_interval"]})
                })
                .collect();
            sources.push(json!({"id":visible["id"],"state":state,"cuts":cuts,"peer":peer,"proof_current":proof_current,"retained_cuts_omitted":truncated}));
        }
        let count = |n| if restricted { Value::Null } else { json!(n) };
        Ok(
            json!({"schema":"replay_selection.v1","policy":"open_and_new",
            "complete":!restricted && !pending && coverage["complete"] == true,
            "state":if restricted {"restricted"} else if pending {"partial"} else {"selected"},
            "sources":sources,"history_excluded":count(self.history),
            "delayed_cut_excluded":count(self.delayed),"unknown_proof_withheld":count(self.unknown),
            "bounded_work_pending":if restricted {Value::Null} else {json!(self.work_pending)},
            "retry":if restricted || pending {json!({"action":"pull_after_contact_or_ack","after_ms":0})} else {Value::Null}}),
        )
    }
}
