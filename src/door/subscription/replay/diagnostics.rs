//! Core-authored selection diagnostics, separate from coverage.v1. No raw
//! binding, source names, home handles, mail origins or peer reasons escape.
use super::*;

// Diagnostics repeat on every page. Keep ample room for content and the
// ordinary response envelope, independent of the retained cut inventory.
const DIAGNOSTIC_BYTES: usize = 16 * 1024;
const CUTS_PER_SOURCE: usize = 2;

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
            let truncated = status.is_some_and(|s| {
                s["retained_cuts_omitted"] == true
                    || s["cuts"]
                        .as_array()
                        .is_some_and(|cuts| cuts.len() > CUTS_PER_SOURCE)
            });
            pending |= state != "established" || !proof_current || truncated;
            let cuts: Vec<Value> = status
                .into_iter()
                .flat_map(|s| s["cuts"].as_array().into_iter().flatten())
                .take(CUTS_PER_SOURCE)
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
        bounded(
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

fn bounded(mut diagnostic: Value) -> Result<Value> {
    if serde_json::to_vec(&diagnostic)?.len() > DIAGNOSTIC_BYTES {
        // Future diagnostic fields or larger sanitized cuts must not freeze a
        // snapshot that cannot serve even one item. Summarize, never silently
        // claim completeness after withholding retained cut details.
        for source in diagnostic["sources"]
            .as_array_mut()
            .context("invalid diagnostics")?
        {
            if source["cuts"].is_array() {
                source["cuts"] = json!([]);
                source["retained_cuts_omitted"] = json!(true);
            }
        }
        diagnostic["complete"] = json!(false);
        diagnostic["state"] = json!("partial");
        diagnostic["retry"] = json!({"action":"pull_after_contact_or_ack","after_ms":0});
    }
    ensure!(
        serde_json::to_vec(&diagnostic)?.len() <= DIAGNOSTIC_BYTES,
        "backpressure"
    );
    Ok(diagnostic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_retained_cut_inventory_is_summarized_before_freezing() {
        let cut = json!({"received_at":i64::MAX,"age_ms":i64::MAX,
            "adopted_at":i64::MAX,"age_at_adoption_ms":i64::MAX,
            "max_source_age_ms":i64::MAX,"delayed":true,"unproved_interval":true});
        let sources: Vec<_> = (0..16)
            .map(|_| json!({"id":uuid(),"cuts":vec![cut.clone();50],"retained_cuts_omitted":false}))
            .collect();
        let diagnostic = bounded(json!({"sources":sources,"complete":true})).unwrap();
        assert!(serde_json::to_vec(&diagnostic).unwrap().len() <= DIAGNOSTIC_BYTES);
        assert_eq!(diagnostic["complete"], false);
        for source in diagnostic["sources"].as_array().unwrap() {
            assert_eq!(source["cuts"], json!([]));
            assert_eq!(source["retained_cuts_omitted"], true);
        }
    }
}
