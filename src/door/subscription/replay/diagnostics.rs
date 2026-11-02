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
        let mut pending = self.work_pending
            || self.delayed != 0
            || self.unknown != 0
            || read.retired_spans() != 0
            || read.demoted_pins() != 0;
        for visible in coverage["sources"].as_array().into_iter().flatten() {
            let core: String = conn.query_row(
                "SELECT core FROM door_coverage_source WHERE domain=?1 AND handle=?2",
                params![m.domain, visible["id"].as_str()],
                |r| r.get(0),
            )?;
            // A partially visible source can contain hidden homes/cuts. Withhold
            // even their counts/state; never leak them through retry/completeness.
            if restricted {
                sources.push(json!({"id":visible["id"],"state":"withheld","cuts":null,"peer":null,"proof_current":null,"retained_cuts_omitted":null,"cut_summary":null}));
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
            // Detail trimming is not unfinished selection. Only actual source
            // establishment/validation and bounded selection work need retry.
            pending |= state != "established" || !proof_current;
            let (cuts, summary, omitted) = source_cuts(status);
            sources.push(json!({"id":visible["id"],"state":state,"cuts":cuts,"cut_summary":summary,"peer":peer,"proof_current":proof_current,"retained_cuts_omitted":omitted}));
        }
        let count = |n| if restricted { Value::Null } else { json!(n) };
        bounded(
            json!({"schema":"replay_selection.v1","policy":"open_and_new",
            "complete":!restricted && !pending && coverage["complete"] == true,
            "state":if restricted {"restricted"} else if pending {"partial"} else {"selected"},
            "sources":sources,"history_excluded":count(self.history),
            "delayed_cut_excluded":count(self.delayed),"unknown_proof_withheld":count(self.unknown),
            "bounded_work_pending":if restricted {Value::Null} else {json!(self.work_pending)},
            "exclusion_spans_retired":count(read.retired_spans()),
            "closure_pins_demoted":count(read.demoted_pins()),
            "retry":if restricted || pending {json!({"action":"pull_after_contact_or_ack","after_ms":0})} else {Value::Null}}),
        )
    }
}

// Counts/flags cover ALL retained cuts, not just the two repeated details.
// This fallback also summarizes older internal diagnostic shapes.
fn cut_summary(cuts: &[Value]) -> Value {
    json!({"count":cuts.len(),
        "any_delayed":cuts.iter().any(|c| c["delayed"] == true),
        "any_unproved":cuts.iter().any(|c| !c["unproved_interval"].is_null() && c["unproved_interval"] != false),
        "worst_age_ms":cuts.iter().filter_map(|c| c["age_ms"].as_i64()).max(),
        "any_age_unknown":cuts.iter().any(|c| !c["age_ms"].is_i64())})
}

fn source_cuts(status: Option<&Value>) -> (Vec<Value>, Value, bool) {
    let all = status
        .and_then(|s| s["cuts"].as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let summary = status
        .map(|s| &s["cut_summary"])
        .filter(|s| s.is_object())
        .cloned()
        .unwrap_or_else(|| cut_summary(all));
    let omitted =
        all.len() > CUTS_PER_SOURCE || status.is_some_and(|s| s["retained_cuts_omitted"] == true);
    let cuts = all
        .iter()
        .take(CUTS_PER_SOURCE)
        .map(|cut| {
            json!({"received_at":cut["received_at"],"age_ms":cut["age_ms"],
        "adopted_at":cut["adopted_at"],"age_at_adoption_ms":cut["age_at_adoption_ms"],
        "max_source_age_ms":cut["max_source_age_ms"],"delayed":cut["delayed"],
        "unproved_interval":cut["unproved_interval"]})
        })
        .collect();
    (cuts, summary, omitted)
}

fn bounded(diagnostic: Value) -> Result<Value> {
    fit_diagnostic(diagnostic, DIAGNOSTIC_BYTES)
}

pub(in crate::door::subscription) fn fit_diagnostic(
    mut diagnostic: Value,
    budget: usize,
) -> Result<Value> {
    let budget = budget.min(DIAGNOSTIC_BYTES);
    if serde_json::to_vec(&diagnostic)?.len() > budget {
        // Shrink detail, not selection completeness. The summary retains the
        // full cut inventory's count, delayed/unproved flags and worst age.
        for source in diagnostic["sources"]
            .as_array_mut()
            .context("invalid diagnostics")?
        {
            if let Some(cuts) = source["cuts"].as_array() {
                if !source["cut_summary"].is_object() {
                    source["cut_summary"] = cut_summary(cuts);
                }
                source["cuts"] = json!([]);
                source["retained_cuts_omitted"] = json!(true);
            }
        }
    }
    ensure!(
        serde_json::to_vec(&diagnostic)?.len() <= budget,
        "oversized_item"
    );
    Ok(diagnostic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn near_page_limit_item_gets_a_smaller_frozen_diagnostic() {
        let diagnostic = json!({"sources":[{"id":uuid(),"cuts":[{"received_at":i64::MAX,"unproved_interval":"x".repeat(1000)}]}],"complete":true});
        let diagnostic = fit_diagnostic(diagnostic, 512).unwrap();
        assert!(serde_json::to_vec(&diagnostic).unwrap().len() <= 512);
        assert_eq!(diagnostic["sources"][0]["retained_cuts_omitted"], true);
    }

    #[test]
    fn three_current_cuts_keep_selection_complete_without_a_hot_retry() {
        let cut = json!({"age_ms":10,"delayed":false,"unproved_interval":null});
        let status = json!({"state":"established","cuts":vec![cut;3]});
        let (cuts, summary, omitted) = source_cuts(Some(&status));
        assert_eq!(cuts.len(), 2);
        assert!(omitted);
        assert_eq!(
            summary,
            json!({"count":3,"any_delayed":false,"any_unproved":false,
            "worst_age_ms":10,"any_age_unknown":false})
        );
        let diagnostic = fit_diagnostic(
            json!({"sources":[{"cuts":cuts,
            "cut_summary":summary,"retained_cuts_omitted":omitted}],
            "complete":true,"state":"selected","retry":null}),
            512,
        )
        .unwrap();
        assert_eq!(diagnostic["complete"], true);
        assert_eq!(diagnostic["state"], "selected");
        assert_eq!(diagnostic["retry"], Value::Null);
    }

    #[test]
    fn omitted_cut_flags_and_worst_age_survive_both_detail_limits() {
        let mut cuts = vec![json!({"age_ms":10,"delayed":false,"unproved_interval":null}); 3];
        cuts[2] = json!({"age_ms":999,"delayed":true,"unproved_interval":{"from_ms":1}});
        let (details, summary, _) = source_cuts(Some(&json!({"cuts":cuts})));
        assert_eq!(summary["count"], 3);
        assert_eq!(summary["any_delayed"], true);
        assert_eq!(summary["any_unproved"], true);
        assert_eq!(summary["worst_age_ms"], 999);
        let diagnostic = fit_diagnostic(
            json!({"sources":[{"cuts":details,
            "cut_summary":summary}],"complete":false,"state":"partial",
            "retry":{"action":"pull_after_contact_or_ack","after_ms":0}}),
            400,
        )
        .unwrap();
        assert_eq!(diagnostic["sources"][0]["cuts"], json!([]));
        assert_eq!(diagnostic["sources"][0]["cut_summary"], summary);
        assert_eq!(diagnostic["state"], "partial");
        assert_eq!(diagnostic["retry"]["action"], "pull_after_contact_or_ack");
    }

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
        assert_eq!(diagnostic["complete"], true);
        for source in diagnostic["sources"].as_array().unwrap() {
            assert_eq!(source["cuts"], json!([]));
            assert_eq!(source["retained_cuts_omitted"], true);
            assert_eq!(source["cut_summary"]["count"], 50);
            assert_eq!(source["cut_summary"]["any_delayed"], true);
            assert_eq!(source["cut_summary"]["any_unproved"], true);
            assert_eq!(source["cut_summary"]["worst_age_ms"], i64::MAX);
        }
    }
}
