//! A review's copy on the forge: one PR comment the runtime builds from the
//! review record, never from the reviewer's prose, and a merge's comment
//! from its merge record. `review.list` stays the source; the comment is a
//! copy of it.
//!
//! The repository may be public, so every piece of text that came from a
//! model (a finding's file and text, a refusal) is masked as transcripts are
//! (`transcript::mask_text`) and then cleared of fleet details: a configured
//! ship or machine name, this machine's name, a home or ship folder path, any
//! other absolute path, and job, errand or review ids. Values the runtime
//! checked itself (a commit sha, a verdict, a tier) are written as they are.
//!
//! Posting needs the task's charter to allow `publish_review`; without it the
//! review is recorded as withheld. A failed post is recorded with its error.
use super::filter::{Finding, clip};
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Characters of one finding's text a comment carries.
const FINDING_CHARS: usize = 400;
const FLEET: &str = "[fleet]";
const PATH: &str = "[path]";
const ID: &str = "[id]";

/// The fleet details a public comment must not name.
#[derive(Clone, Debug, Default)]
pub struct Redact {
    /// Ship and machine names, matched as whole words, case-insensitively.
    pub names: Vec<String>,
    /// Folders whose paths are dropped wherever they appear.
    pub paths: Vec<String>,
}

impl Redact {
    /// This home's ships, the fleet registry's ships, machines and homes,
    /// this machine's name, and the home folders. Whatever cannot be read is
    /// left out; any other absolute path is dropped all the same.
    pub fn of(store: &Store) -> Self {
        let mut redact = Redact::default();
        if let Ok(mut statement) = store.connection().prepare("SELECT name FROM ships")
            && let Ok(rows) = statement.query_map([], |r| r.get::<_, String>(0))
        {
            redact.names.extend(rows.flatten());
        }
        if let Ok(conn) = crate::fleet::registry() {
            for (query, into_paths) in [
                ("SELECT name FROM ships", false),
                ("SELECT home FROM ships", true),
                ("SELECT name FROM machines", false),
            ] {
                if let Ok(mut statement) = conn.prepare(query)
                    && let Ok(rows) = statement.query_map([], |r| r.get::<_, String>(0))
                {
                    let found = rows.flatten();
                    if into_paths {
                        redact.paths.extend(found);
                    } else {
                        redact.names.extend(found);
                    }
                }
            }
        }
        redact.names.push(crate::machines::hostname());
        redact
            .paths
            .push(store.home().to_string_lossy().into_owned());
        if let Some(folder) = store.home().parent() {
            redact.paths.push(folder.to_string_lossy().into_owned());
        }
        if let Some(home) = std::env::var_os("HOME") {
            redact.paths.push(home.to_string_lossy().into_owned());
        }
        redact.names.retain(|n| n.chars().count() >= 3);
        redact.paths.retain(|p| p.len() > 1);
        redact
    }
}

/// Whether a word is a job, errand, crew or review id, or a bare uuid.
fn id_shaped(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let tail = ["errand-", "review-", "job-", "crew-", "wake-"]
        .iter()
        .find_map(|p| word.strip_prefix(p))
        .unwrap_or(word);
    let hexish = tail.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    hexish && tail.chars().filter(char::is_ascii_hexdigit).count() >= 16 && tail.contains('-')
}

/// Text from a model, safe for a public comment: masked as transcripts are,
/// then cleared of fleet details word by word, on one line.
pub fn sanitize(text: &str, redact: &Redact) -> String {
    let masked = crate::transcript::mask_text(text);
    let mut words = vec![];
    for word in masked.split_whitespace() {
        let path = word.trim_start_matches(['`', '"', '\'', '(', '[', '<']);
        let word = if redact.paths.iter().any(|p| word.contains(p.as_str()))
            || path.starts_with('/') && path.matches('/').count() >= 2
            || path.starts_with("~/")
        {
            PATH.to_owned()
        } else if id_shaped(word) {
            ID.to_owned()
        } else {
            let mut kept = word.to_owned();
            for name in &redact.names {
                kept = replace_word(&kept, &name.to_ascii_lowercase());
            }
            kept
        };
        words.push(word);
    }
    words.join(" ")
}

/// `word` with `name` replaced where it stands as a whole word (between
/// characters that are not letters, digits, `-` or `_`).
fn replace_word(word: &str, name: &str) -> String {
    let lower = word.to_ascii_lowercase();
    if name.is_empty() || !lower.contains(name) {
        return word.to_owned();
    }
    let inner = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let mut out = String::new();
    let mut at = 0;
    while let Some(found) = lower[at..].find(name) {
        let start = at + found;
        let end = start + name.len();
        let before = lower[..start].chars().next_back().is_none_or(|c| !inner(c));
        let after = lower[end..].chars().next().is_none_or(|c| !inner(c));
        out.push_str(&word[at..start]);
        out.push_str(if before && after {
            FLEET
        } else {
            &word[start..end]
        });
        at = end;
    }
    out.push_str(&word[at..]);
    out
}

/// The marker that keeps one comment per review on the forge: a digest of
/// the review's id, so the id itself is not published.
pub fn marker(id: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(id.as_bytes()));
    format!("<!-- privateer-review {} -->", &digest[..24])
}

fn verdict_word(verdict: &str) -> &'static str {
    match verdict {
        "approve" => "APPROVE",
        "changes_requested" => "CHANGES_REQUESTED",
        "none" => "no verdict",
        "running" => "running",
        _ => "failed",
    }
}

fn reviewer(review: &Value, redact: &Redact) -> String {
    let text = |key: &str| review[key].as_str().unwrap_or_default();
    let model = match text("route_model") {
        "" => "an unnamed model".to_owned(),
        model => format!("`{}`", sanitize(model, redact)),
    };
    let route = match text("route_name") {
        "" => "the first mate's own route".to_owned(),
        name => format!("route `{}`", sanitize(name, redact)),
    };
    let tier = review["route_tier"]
        .as_i64()
        .map(|t| format!(", tier {t}"))
        .unwrap_or_default();
    format!("{model} on {route}{tier}")
}

fn finding_line(finding: &Finding, redact: &Redact) -> String {
    format!(
        "`{}:{}` {}",
        sanitize(&finding.file, redact).replace('`', "'"),
        finding.line,
        sanitize(&clip(&finding.text, FINDING_CHARS), redact)
    )
}

/// An earlier must-fix finding and the commit that resolved it: the first
/// commit after the head it was found at that changed its file, else the
/// approved head.
pub struct Resolved {
    pub finding: Finding,
    pub found_at: String,
    pub resolved_by: String,
}

/// The must-fix findings of earlier reviews of a PR, at heads before this
/// approved one, each with the commit that resolved it.
pub fn resolved(store: &Store, review: &Value) -> Result<Vec<Resolved>> {
    let head = review["head_sha"].as_str().unwrap_or_default();
    let clone = review["clone"].as_str().unwrap_or_default();
    let mut statement = store.connection().prepare(
        "SELECT head_sha, findings FROM reviews WHERE repo=?1 AND pr=?2 AND created_ms<?3
           AND head_sha!=?4 AND findings IS NOT NULL ORDER BY created_ms",
    )?;
    let rows = statement.query_map(
        params![
            review["repo"].as_str(),
            review["pr"].as_i64(),
            review["created_ms"].as_i64(),
            head
        ],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )?;
    let mut found = vec![];
    for row in rows {
        let (earlier, findings) = row?;
        let findings: Vec<Finding> = serde_json::from_str(&findings).unwrap_or_default();
        for finding in findings.into_iter().filter(|f| f.severity == "must-fix") {
            let range = format!("{earlier}..{head}");
            let first = super::pr::git(
                Path::new(clone),
                &[
                    "log",
                    "--reverse",
                    "--format=%H",
                    &range,
                    "--",
                    &finding.file,
                ],
            )
            .ok()
            .and_then(|out| out.lines().next().map(str::to_owned))
            .filter(|sha| sha.len() == 40)
            .unwrap_or_else(|| head.to_owned());
            found.push(Resolved {
                finding,
                found_at: earlier.clone(),
                resolved_by: first,
            });
        }
    }
    Ok(found)
}

/// A review's comment, from its record alone.
pub fn review_body(review: &Value, resolved: &[Resolved], redact: &Redact) -> String {
    let text = |key: &str| review[key].as_str().unwrap_or_default();
    let verdict = text("verdict");
    let head = text("head_sha");
    let mut body = format!(
        "{}\n**Privateer review: {}**\n\n- Reviewed head: `{head}`\n- Reviewer: {}\n- Verdict: {}\n",
        marker(text("id")),
        verdict_word(verdict),
        reviewer(review, redact),
        verdict_word(verdict),
    );
    if matches!(verdict, "none" | "failed") && !text("refusal").is_empty() {
        body.push_str(&format!(
            "- Why: {}\n",
            sanitize(&clip(text("refusal"), 300), redact)
        ));
    }
    let findings: Vec<Finding> =
        serde_json::from_value(review["findings"].clone()).unwrap_or_default();
    let (must, other): (Vec<&Finding>, Vec<&Finding>) =
        findings.iter().partition(|f| f.severity == "must-fix");
    if !must.is_empty() {
        body.push_str("\nMust-fix findings:\n");
        for finding in must {
            body.push_str(&format!("- {}\n", finding_line(finding, redact)));
        }
    }
    if !other.is_empty() {
        body.push_str("\nOther findings:\n");
        for finding in other {
            body.push_str(&format!(
                "- {} {}\n",
                finding.severity,
                finding_line(finding, redact)
            ));
        }
    }
    if verdict == "approve" && !resolved.is_empty() {
        body.push_str("\nMust-fix findings of earlier reviews, resolved:\n");
        for r in resolved {
            body.push_str(&format!(
                "- {} (found at `{}`), resolved by `{}`\n",
                finding_line(&r.finding, redact),
                r.found_at,
                r.resolved_by
            ));
        }
    }
    body.push_str(&format!(
        "\nThis review covers `{head}` only: commits pushed after it are not reviewed by it.\n\
         Written by the Privateer runtime from its review record.\n"
    ));
    body
}

/// A merge's comment, from its merge record alone.
pub fn merge_body(id: &str, merge: &Value, redact: &Redact) -> String {
    let text = |key: &str| merge[key].as_str().unwrap_or_default();
    let mut body = format!(
        "{}\n**Privateer merge record**\n\n- Merged head: `{}`\n",
        marker(id),
        text("head_sha")
    );
    if !text("merge_sha").is_empty() {
        body.push_str(&format!("- Merge commit: `{}`\n", text("merge_sha")));
    }
    match merge["review"].as_object() {
        Some(review) => body.push_str(&format!(
            "- Covered by the review of `{}`: APPROVE by {}\n",
            review["head_sha"].as_str().unwrap_or_default(),
            reviewer(&merge["review"], redact)
        )),
        None => body.push_str("- No approving review covered this merge.\n"),
    }
    let unreviewed = merge["unreviewed"].as_array().cloned().unwrap_or_default();
    if !unreviewed.is_empty() {
        body.push_str("\nNot reviewed:\n");
        for commit in &unreviewed {
            let written: Vec<String> = commit["written_by"]
                .as_array()
                .map(|w| {
                    w.iter()
                        .filter_map(Value::as_str)
                        .map(|m| format!("`{}`", sanitize(m, redact)))
                        .collect()
                })
                .unwrap_or_default();
            body.push_str(&format!(
                "- `{}`, written by {}\n",
                commit["sha"].as_str().unwrap_or_default(),
                if written.is_empty() {
                    "an unrecorded model".to_owned()
                } else {
                    written.join(", ")
                }
            ));
        }
    }
    body.push_str(&format!(
        "\nCoverage: {}.\nWritten by the Privateer runtime from its merge record.\n",
        text("coverage")
    ));
    body
}

/// Whether the task's charter lets the runtime post to the forge for it.
pub fn allowed(store: &Store, task_id: &str) -> Result<(), String> {
    store
        .authorize_running(task_id, "publish_review")
        .map(|_| ())
        .map_err(|error| format!("{error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_comment_is_masked_and_names_no_fleet_detail() {
        let redact = Redact {
            names: vec!["engineering".into(), "laptop-7".into()],
            paths: vec!["/home/captain".into()],
        };
        let text = "the token ghp_4fK9xQ2mZ7pL1vB8nR3tY6wE0aS5dC2hJ8 leaks; see \
             /home/captain/ships/engineering/.privateer/jobs/x.log and /srv/other/thing \
             from errand-0a1b2c3d-4e5f-6789-abcd-ef0123456789 on Engineering at laptop-7. \
             src/lib.rs:12 is fine; engineering-tools stays.";
        let clean = sanitize(text, &redact);
        for absent in [
            "ghp_4fK9xQ2mZ7pL1vB8nR3tY6wE0aS5dC2hJ8",
            "/home/captain",
            "/srv/other",
            "errand-0a1b",
            "Engineering ",
            "laptop-7",
        ] {
            assert!(!clean.contains(absent), "{absent} in {clean}");
        }
        for kept in ["src/lib.rs:12", "engineering-tools", "[fleet]", "[path]"] {
            assert!(clean.contains(kept), "{kept} missing from {clean}");
        }
        assert!(!clean.contains('\n'));
        assert!(id_shaped("(job-0a1b2c3d-4e5f-6789-abcd-ef0123456789)"));
        assert!(!id_shaped("src/lib.rs:12") && !id_shaped("2026-10-22"));
        // The marker names a digest, never the review id.
        let marker = marker("review-0a1b2c3d");
        assert!(!marker.contains("0a1b2c3d") && marker.starts_with("<!-- privateer-review "));
    }
}
