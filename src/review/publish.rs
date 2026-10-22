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
/// Characters a comment may hold: a forge allows about 65,000.
pub const COMMENT_CHARS: usize = 60_000;
const FLEET: &str = "[fleet]";
const PATH: &str = "[path]";
const ID: &str = "[id]";
/// What a runtime id may begin with.
const ID_PREFIXES: [&str; 6] = ["errand-", "review-", "merge-", "job-", "crew-", "wake-"];

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
    /// this machine's name, and the home folders, except the owner and name
    /// of `repo`, the PR's own repository, whose comment may name it.
    /// Whatever cannot be read is left out; any other absolute path is
    /// dropped all the same.
    pub fn of(store: &Store, repo: &str) -> Self {
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
        redact.keeping(repo)
    }

    /// Without the names `repo` (`owner/name`) is spelled with, and without
    /// names and paths too short to match safely.
    fn keeping(mut self, repo: &str) -> Self {
        let own: Vec<&str> = repo.split('/').filter(|p| !p.is_empty()).collect();
        self.names
            .retain(|n| n.chars().count() >= 3 && !own.iter().any(|o| o.eq_ignore_ascii_case(n)));
        self.paths.retain(|p| p.len() > 1);
        self
    }
}

/// Whether a word is a job, errand, crew or review id, or a bare uuid.
fn id_shaped(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let tail = ID_PREFIXES
        .iter()
        .find_map(|p| word.strip_prefix(p))
        .unwrap_or(word);
    let hexish = tail.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    hexish && tail.chars().filter(char::is_ascii_hexdigit).count() >= 16 && tail.contains('-')
}

/// Whether `text` begins with a uuid (8-4-4-4-12 hex digits).
fn uuid_at(text: &[u8]) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut at = 0;
    for (n, len) in GROUPS.iter().enumerate() {
        if n > 0 {
            if text.get(at) != Some(&b'-') {
                return false;
            }
            at += 1;
        }
        let Some(group) = text.get(at..at + len) else {
            return false;
        };
        if !group.iter().all(u8::is_ascii_hexdigit) {
            return false;
        }
        at += len;
    }
    !text.get(at).is_some_and(u8::is_ascii_alphanumeric)
}

/// `word` with every id in it replaced: a uuid wherever it stands (inside a
/// path, after `task:` or a ship's name), with the runtime prefix before it,
/// and any run of letters, digits and `-` that is id-shaped.
fn without_ids(word: &str) -> String {
    let bytes = word.as_bytes();
    let mut out = String::new();
    let mut at = 0;
    let mut i = 0;
    while i < bytes.len() {
        let starts = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        if starts && uuid_at(&bytes[i..]) {
            let mut from = i;
            if let Some(prefix) = ID_PREFIXES.iter().find(|p| word[at..i].ends_with(*p)) {
                from = i - prefix.len();
            }
            out.push_str(&word[at..from]);
            out.push_str(ID);
            i += 36;
            at = i;
            continue;
        }
        i += 1;
    }
    out.push_str(&word[at..]);
    let run = |c: char| c.is_ascii_alphanumeric() || c == '-';
    let mut kept = String::new();
    let mut piece = String::new();
    for c in out.chars().chain(std::iter::once('\0')) {
        if run(c) {
            piece.push(c);
            continue;
        }
        if id_shaped(&piece) {
            kept.push_str(ID);
        } else {
            kept.push_str(&piece);
        }
        piece.clear();
        if c != '\0' {
            kept.push(c);
        }
    }
    kept
}

/// What may stand just before a path inside a word: `--home=/x`,
/// `file:/x`, `](/x)`, `"/x"`.
fn opens_path(c: char) -> bool {
    matches!(
        c,
        '=' | ':' | '(' | '[' | '<' | '{' | '"' | '\'' | '`' | ','
    )
}

/// Where an absolute path begins in `word`: a configured folder anywhere; a
/// `/`, `~/`, `C:\` or `\\` at its start or after one of `opens_path`'s
/// characters; or whatever follows `file:`. A URL's path is not one.
fn path_start(word: &str, redact: &Redact) -> Option<usize> {
    let configured = redact
        .paths
        .iter()
        .filter_map(|p| word.find(p.as_str()))
        .min();
    let chars: Vec<(usize, char)> = word.char_indices().collect();
    let generic = chars.iter().enumerate().find_map(|(n, &(at, c))| {
        let before = n.checked_sub(1).map(|b| chars[b].1);
        if !before.is_none_or(opens_path) {
            return None;
        }
        let rest = &word[at..];
        if word[..at].to_ascii_lowercase().ends_with("file:") {
            return Some(at);
        }
        let next = rest[c.len_utf8()..].chars().next();
        let posix = c == '/' && next.is_some_and(|n| n != '/') && rest.matches('/').count() >= 2;
        let home = rest.starts_with("~/");
        let drive = c.is_ascii_alphabetic() && rest[1..].starts_with(":\\");
        let unc = rest.starts_with("\\\\");
        (posix || home || drive || unc).then_some(at)
    });
    [configured, generic].into_iter().flatten().min()
}

/// Where a path that begins in a word ends: before the closing quotes,
/// brackets and punctuation after it.
fn path_end(word: &str) -> usize {
    word.trim_end_matches([
        ')', ']', '>', '}', '"', '\'', '`', ',', ';', '.', ':', '!', '?',
    ])
    .len()
}

/// Text from a model, safe for a public comment: masked as transcripts are,
/// then cleared of fleet details word by word, on one line. A path that
/// goes on past a space (inside quotes, or after `\ `) is dropped whole.
pub fn sanitize(text: &str, redact: &Redact) -> String {
    let masked = crate::transcript::mask_text(text);
    let mut words = vec![];
    // The rest of a path dropped in an earlier word: the quote that closes
    // it, or a backslash while each word ends in an escaped space.
    let mut inside: Option<char> = None;
    for word in masked.split_whitespace() {
        match inside {
            Some('\\') => {
                if !word.ends_with('\\') {
                    inside = None;
                    words.push(word[path_end(word)..].to_owned());
                }
                continue;
            }
            Some(quote) => {
                if let Some(at) = word.find(quote) {
                    inside = None;
                    words.push(word[at..].to_owned());
                }
                continue;
            }
            None => {}
        }
        let word = match path_start(word, redact) {
            Some(start) => {
                let end = path_end(word).max(start);
                let opened = word[..start]
                    .chars()
                    .next_back()
                    .filter(|c| matches!(c, '"' | '\'' | '`'));
                if word[..end].ends_with('\\') {
                    inside = Some('\\');
                } else if let Some(quote) = opened
                    && !word[start..].contains(quote)
                {
                    inside = Some(quote);
                }
                format!("{}{PATH}{}", &word[..start], &word[end..])
            }
            None => word.to_owned(),
        };
        let mut kept = without_ids(&word);
        for name in &redact.names {
            kept = replace_word(&kept, &name.to_ascii_lowercase());
        }
        words.push(kept);
    }
    words.retain(|w| !w.is_empty());
    words.join(" ")
}

/// `word` with `name` replaced where it stands as a whole word (between
/// characters that are not letters, digits or `_`): `fleet-<name>` and
/// `<name>.local` lose it too.
fn replace_word(word: &str, name: &str) -> String {
    let lower = word.to_ascii_lowercase();
    if name.is_empty() || !lower.contains(name) {
        return word.to_owned();
    }
    let inner = |c: char| c.is_ascii_alphanumeric() || c == '_';
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

/// Text cut to `limit` characters on a character boundary, after it was
/// masked, so a secret is never posted in part.
fn masked_clip(text: &str, limit: usize, redact: &Redact) -> String {
    clip(&sanitize(text, redact), limit)
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
        masked_clip(&finding.text, FINDING_CHARS, redact)
    )
}

/// `body` cut to the comment limit on a character boundary, should a part
/// that is not cut otherwise still be too long. The marker leads it.
fn capped(body: String) -> String {
    if body.chars().count() <= COMMENT_CHARS {
        return body;
    }
    let note = "\n\n(Cut at the forge's comment limit; review.list holds the whole record.)\n";
    let keep = COMMENT_CHARS - note.chars().count();
    format!("{}{note}", body.chars().take(keep).collect::<String>())
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

/// A review's comment, from its record alone. When it would pass the
/// comment limit the earlier findings it resolved are cut first, saying how
/// many were left out.
pub fn review_body(review: &Value, resolved: &[Resolved], redact: &Redact) -> String {
    let text = |key: &str| review[key].as_str().unwrap_or_default();
    let verdict = text("verdict");
    let head = text("head_sha");
    let named = if text("kind") == "errand" {
        " (the head the first mate named, checked to be the PR's head or one of its commits)"
    } else {
        " (the PR's head, read from the forge)"
    };
    let mut body = format!(
        "{}\n**Privateer review: {}**\n\n- Reviewed head: `{head}`{named}\n- Reviewer: {}\n- Verdict: {}\n",
        marker(text("id")),
        verdict_word(verdict),
        reviewer(review, redact),
        verdict_word(verdict),
    );
    if matches!(verdict, "none" | "failed") && !text("refusal").is_empty() {
        body.push_str(&format!(
            "- Why: {}\n",
            masked_clip(text("refusal"), 300, redact)
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
    let footer = format!(
        "\nThis review covers `{head}` only: commits pushed after it are not reviewed by it.\n\
         Written by the Privateer runtime from its review record.\n"
    );
    if verdict == "approve" && !resolved.is_empty() {
        let lines: Vec<String> = resolved
            .iter()
            .map(|r| {
                format!(
                    "- {} (found at `{}`), resolved by `{}`\n",
                    finding_line(&r.finding, redact),
                    r.found_at,
                    r.resolved_by
                )
            })
            .collect();
        let heading = "\nMust-fix findings of earlier reviews, resolved:\n";
        // Room for the heading, the footer and the line saying what was cut.
        let room = COMMENT_CHARS.saturating_sub(
            body.chars().count() + heading.chars().count() + footer.chars().count() + 80,
        );
        let mut used = 0;
        let fits = lines
            .iter()
            .take_while(|line| {
                used += line.chars().count();
                used <= room
            })
            .count();
        body.push_str(heading);
        for line in &lines[..fits] {
            body.push_str(line);
        }
        if fits < lines.len() {
            body.push_str(&format!(
                "- {} more left out for the comment limit; review.list holds them.\n",
                lines.len() - fits
            ));
        }
    }
    body.push_str(&footer);
    capped(body)
}

/// One review a merge record names, on one line: its head, verdict and
/// reviewer.
fn named_review(review: &Value, redact: &Redact) -> String {
    format!(
        "`{}`: {} by {}",
        review["head_sha"].as_str().unwrap_or_default(),
        verdict_word(review["verdict"].as_str().unwrap_or_default()),
        reviewer(review, redact)
    )
}

/// What a merge's coverage means, in words.
fn coverage_words(coverage: &str) -> &'static str {
    match coverage {
        "reviewed" => "reviewed: a tier-1 review route approved the merged head",
        "written_by_top_tier" => {
            "written by a tier-1 review model: the commits no review covered were written on one"
        }
        "changes_requested" => {
            "changes requested: a review asked for changes at the merged head or after the covering approval"
        }
        _ => "unreviewed: commits no tier-1 approval covered were merged",
    }
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
        Some(_) => body.push_str(&format!(
            "- Covered by the review of {}\n",
            named_review(&merge["review"], redact)
        )),
        None => body.push_str("- No approval by a tier-1 review route covered this merge.\n"),
    }
    let list = |key: &str| merge[key].as_array().cloned().unwrap_or_default();
    let weaker = list("approvals_not_covering");
    if !weaker.is_empty() {
        body.push_str(
            "\nApprovals recorded that do not cover it (only a tier-1 review route's does):\n",
        );
        for review in &weaker {
            body.push_str(&format!(
                "- {}: {}\n",
                named_review(review, redact),
                sanitize(review["why"].as_str().unwrap_or_default(), redact)
            ));
        }
    }
    let changes = list("changes_requested");
    if !changes.is_empty() {
        body.push_str("\nChanges requested at the merged head or after the covering approval:\n");
        for review in &changes {
            body.push_str(&format!("- {}\n", named_review(review, redact)));
        }
    }
    let unreviewed = list("unreviewed");
    if !unreviewed.is_empty() {
        body.push_str("\nNot reviewed:\n");
        let models = |key: &str, commit: &Value| -> Vec<String> {
            commit[key]
                .as_array()
                .map(|w| {
                    w.iter()
                        .filter_map(Value::as_str)
                        .map(|m| format!("`{}`", sanitize(m, redact)))
                        .collect()
                })
                .unwrap_or_default()
        };
        for commit in &unreviewed {
            let written = models("written_by", commit);
            let mut line = format!(
                "- `{}`, written by {}",
                commit["sha"].as_str().unwrap_or_default(),
                if written.is_empty() {
                    "an unrecorded model".to_owned()
                } else {
                    format!(
                        "{} (from the {})",
                        written.join(", "),
                        commit["from"].as_str().unwrap_or_default()
                    )
                }
            );
            if commit["disagrees"] == true {
                line.push_str(&format!(
                    "; its commit trailer names {}, which the crew record does not, so it is not counted as written by a tier-1 model",
                    models("trailer", commit).join(", ")
                ));
            }
            body.push_str(&line);
            body.push('\n');
        }
    }
    body.push_str(&format!(
        "\nCoverage: {}.\nA review is no gate: the record says what was merged either way.\n\
         Written by the Privateer runtime from its merge record.\n",
        coverage_words(text("coverage"))
    ));
    capped(body)
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
    use serde_json::json;

    fn redact() -> Redact {
        Redact {
            names: vec!["engineering".into(), "laptop-7".into()],
            paths: vec!["/home/captain".into()],
        }
    }

    fn assert_clean(text: &str, absent: &[&str], kept: &[&str]) {
        let clean = sanitize(text, &redact());
        for absent in absent {
            assert!(!clean.contains(absent), "{absent} in {clean}");
        }
        for kept in kept {
            assert!(clean.contains(kept), "{kept} missing from {clean}");
        }
        assert!(!clean.contains('\n'), "{clean}");
    }

    #[test]
    fn a_comment_is_masked_and_names_no_fleet_detail() {
        assert_clean(
            "the token ghp_4fK9xQ2mZ7pL1vB8nR3tY6wE0aS5dC2hJ8 leaks; see \
             /home/captain/ships/engineering/.privateer/jobs/x.log and /srv/other/thing \
             from errand-0a1b2c3d-4e5f-6789-abcd-ef0123456789 on Engineering at laptop-7. \
             src/lib.rs:12 is fine.",
            &[
                "ghp_4fK9xQ2mZ7pL1vB8nR3tY6wE0aS5dC2hJ8",
                "/home/captain",
                "/srv/other",
                "errand-0a1b",
                "Engineering ",
                "laptop-7",
            ],
            &["src/lib.rs:12", "[fleet]", "[path]"],
        );
        assert!(id_shaped("(job-0a1b2c3d-4e5f-6789-abcd-ef0123456789)"));
        assert!(!id_shaped("src/lib.rs:12") && !id_shaped("2026-10-22"));
        // The marker names a digest, never the review id.
        let marker = marker("review-0a1b2c3d");
        assert!(!marker.contains("0a1b2c3d") && marker.starts_with("<!-- privateer-review "));
    }

    #[test]
    fn ids_inside_path_like_words_are_dropped() {
        // The transcript mask hides most of these already; the id pass
        // drops whatever it leaves.
        let uuid = "0a1b2c3d-4e5f-6789-abcd-ef0123456789";
        for (word, clean) in [
            (format!("jobs/{uuid}/stdout.log"), "jobs/[id]/stdout.log"),
            (format!("focus=task:errand-{uuid},"), "focus=task:[id],"),
            (format!("wake-{uuid}.json"), "[id].json"),
            (format!("fleet-{uuid}"), "fleet-[id]"),
            ("crew-0a1b2c3d4e5f6789-abcd".to_owned(), "[id]"),
        ] {
            assert_eq!(without_ids(&word), clean, "{word}");
            assert!(!sanitize(&word, &redact()).contains("0a1b2c3d"), "{word}");
        }
        // A commit sha and a date are not ids.
        let sha = "e".repeat(40);
        assert_eq!(without_ids(&sha), sha);
        assert_eq!(sanitize("2026-10-22", &redact()), "2026-10-22");
    }

    #[test]
    fn a_ship_name_joined_by_hyphens_is_dropped() {
        assert_eq!(
            sanitize("fleet-engineering and engineering-tools", &redact()),
            "fleet-[fleet] and [fleet]-tools"
        );
        assert_eq!(sanitize("Laptop-7.local", &redact()), "[fleet].local");
        // Inside a longer word it is not the name.
        assert_eq!(sanitize("reengineering", &redact()), "reengineering");
    }

    #[test]
    fn an_absolute_path_after_a_prefix_is_dropped() {
        for (text, clean) in [
            ("--home=/srv/x/y", "--home=[path]"),
            ("file:/opt/a/b", "file:[path]"),
            ("file:///opt/a", "file:[path]"),
            ("[log](/srv/a/b)", "[log]([path])"),
            ("`/srv/a/b`,", "`[path]`,"),
            ("C:\\Users\\captain\\x.log", "[path]"),
            ("at=C:\\Users\\x", "at=[path]"),
            ("\\\\server\\share\\x", "[path]"),
            ("~/ships/x", "[path]"),
            ("/home/captain", "[path]"),
            ("x=/home/captain/a", "x=[path]"),
        ] {
            assert_eq!(sanitize(text, &redact()), clean, "{text}");
        }
        // A URL, a relative path and a lone root folder stay.
        for kept in [
            "https://example.invalid/a/b",
            "src/lib.rs:12",
            "/tmp",
            "1/2/3",
        ] {
            assert_eq!(sanitize(kept, &redact()), kept);
        }
    }

    #[test]
    fn a_path_that_goes_on_past_a_space_is_dropped_whole() {
        assert_eq!(
            sanitize(
                "read \"/srv/my ships/a b/x.log\" then src/lib.rs",
                &redact()
            ),
            "read \"[path] \" then src/lib.rs"
        );
        assert_eq!(
            sanitize("read /srv/my\\ ships/x.log, then src/lib.rs", &redact()),
            "read [path] , then src/lib.rs"
        );
        assert_eq!(
            sanitize("see `/srv/a b/c` and src/lib.rs", &redact()),
            "see `[path] ` and src/lib.rs"
        );
    }

    #[test]
    fn the_prs_own_repository_is_never_blanked() {
        let redact = Redact {
            names: vec!["privateer".into(), "acme".into(), "engineering".into()],
            paths: vec![],
        }
        .keeping("Acme/Privateer");
        assert_eq!(
            sanitize("privateer's parser and acme's, not engineering", &redact),
            "privateer's parser and acme's, not [fleet]"
        );
    }

    #[test]
    fn text_is_masked_before_it_is_clipped() {
        let token = "ghp_4fK9xQ2mZ7pL1vB8nR3tY6wE0aS5dC2hJ8";
        let text = format!("leaks {token} here");
        // A cut through the token would leave a prefix the mask no longer
        // recognises; masking first leaves none of it.
        let cut = "leaks ghp_4fK9xQ2mZ7".chars().count();
        let clean = masked_clip(&text, cut, &redact());
        assert!(!clean.contains("ghp_4fK9"), "{clean}");
        assert!(!sanitize(&clip(&text, cut), &redact()).contains(token));
    }

    #[test]
    fn a_long_comment_cuts_the_resolved_findings_first() {
        let finding = |n: usize| Finding {
            severity: "must-fix".into(),
            file: "src/lib.rs".into(),
            line: n as u64,
            text: "x".repeat(390),
        };
        let review = json!({"id": "review-1", "verdict": "approve", "head_sha": "a".repeat(40),
            "kind": "errand", "route_model": "claude-opus-5-5", "route_name": "opus",
            "route_tier": 1, "findings": []});
        let resolved: Vec<Resolved> = (0..400)
            .map(|n| Resolved {
                finding: finding(n),
                found_at: "b".repeat(40),
                resolved_by: "c".repeat(40),
            })
            .collect();
        let body = review_body(&review, &resolved, &redact());
        assert!(body.chars().count() <= COMMENT_CHARS, "{}", body.len());
        assert!(
            body.contains("more left out for the comment limit"),
            "{body}"
        );
        assert!(body.contains("Written by the Privateer runtime"));
        assert!(body.contains("the head the first mate named"));
        assert!(body.starts_with(&marker("review-1")));
        // Whatever else is too long is cut at the limit, marker first.
        let long = capped(format!(
            "{}\n{}",
            marker("m"),
            "y".repeat(COMMENT_CHARS * 2)
        ));
        assert_eq!(long.chars().count(), COMMENT_CHARS);
        assert!(long.starts_with(&marker("m")));
    }
}
