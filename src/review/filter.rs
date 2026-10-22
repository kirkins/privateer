//! What a review's reply may leave behind: its verdict and its structured
//! findings, filtered where the record is stored, so a later post carries
//! only filtered records.
//!
//! The verdict is read from the reply's last non-empty line alone, matched
//! exactly; anything else is `none`. A finding is one line, `FINDING
//! <severity> <path>:<line> <text>`, whose path names a file inside the
//! review's worktree. Everything else in the reply is the reviewer's working
//! and is not kept. A reply is refused outright, keeping nothing, when it
//! carries a stored credential value or names a path outside the worktree:
//! the runtime never stores, and so never posts, such a review.
use std::path::{Component, Path};

/// Severities a finding may carry, in the order a summary lists them.
pub const SEVERITIES: [&str; 3] = ["must-fix", "should-fix", "nit"];
/// Findings kept from one reply, and the characters kept of each.
const MAX_FINDINGS: usize = 50;
const FINDING_CHARS: usize = 500;
/// Stored credentials shorter than this are not searched for: a value that
/// short matches ordinary prose and would refuse honest reviews.
const MIN_SECRET: usize = 8;

/// A reviewer's finding, as stored: its severity, its file relative to the
/// worktree root, its line, and its text.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    pub severity: String,
    pub file: String,
    pub line: u64,
    pub text: String,
}

/// The verdict a reply's last non-empty line gives: `approve`,
/// `changes_requested` or `none`.
pub fn verdict(reply: &str) -> &'static str {
    match reply.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
        Some("VERDICT: APPROVE") => "approve",
        Some("VERDICT: CHANGES_REQUESTED") => "changes_requested",
        _ => "none",
    }
}

/// The structured findings of a reply, or why the reply is refused. `tree` is
/// the worktree the reviewer read; `secrets` are the stored credential values.
pub fn findings(reply: &str, tree: &Path, secrets: &[Vec<u8>]) -> Result<Vec<Finding>, String> {
    scan(reply, tree, secrets, true)
}

/// The structured findings of an errand's review, which ran in the ship's
/// folder rather than a worktree of its own, so its prose may name any path:
/// a stored credential value still refuses the reply, but a finding outside
/// `tree` (the PR's clone) is dropped rather than refusing the rest. What is
/// kept is masked and cleared of fleet details again before it is posted.
pub fn findings_lenient(
    reply: &str,
    tree: &Path,
    secrets: &[Vec<u8>],
) -> Result<Vec<Finding>, String> {
    scan(reply, tree, secrets, false)
}

fn scan(
    reply: &str,
    tree: &Path,
    secrets: &[Vec<u8>],
    strict: bool,
) -> Result<Vec<Finding>, String> {
    for secret in secrets.iter().filter(|s| s.len() >= MIN_SECRET) {
        if reply
            .as_bytes()
            .windows(secret.len())
            .any(|window| window == secret.as_slice())
        {
            return Err("the reply contains a stored credential value".to_owned());
        }
    }
    if strict && let Some(path) = outside_path(reply, tree) {
        return Err(format!(
            "the reply names a path outside the worktree: {}",
            clip(&path, 120)
        ));
    }
    let mut kept = vec![];
    for line in reply.lines() {
        let line = line
            .trim()
            .trim_start_matches(['-', '*'])
            .trim_start()
            .trim_matches('`');
        let Some(rest) = line.strip_prefix("FINDING ") else {
            continue;
        };
        let mut words = rest.trim_start().splitn(3, char::is_whitespace);
        let severity = words
            .next()
            .unwrap_or_default()
            .trim_matches(['[', ']'])
            .to_ascii_lowercase();
        if !SEVERITIES.contains(&severity.as_str()) {
            continue;
        }
        let location = words
            .next()
            .unwrap_or_default()
            .trim_matches('`')
            .trim_end_matches([':', ',']);
        let Some((file, line_no)) = location.rsplit_once(':') else {
            continue;
        };
        let Ok(line_no) = line_no.parse::<u64>() else {
            continue;
        };
        let file = match inside(file, tree) {
            Ok(file) => file,
            Err(_) if !strict => continue,
            Err(path) => {
                return Err(format!(
                    "a finding names a path outside the worktree: {}",
                    clip(&path, 120)
                ));
            }
        };
        let text = words
            .next()
            .unwrap_or_default()
            .trim_start_matches([':', '-', '—', ' '])
            .trim();
        if kept.len() < MAX_FINDINGS {
            // Masked before it is cut, so a token across the cut is never
            // kept in part.
            kept.push(Finding {
                severity,
                file,
                line: line_no,
                text: clip(&crate::transcript::mask_text(text), FINDING_CHARS),
            });
        }
    }
    Ok(kept)
}

/// A finding's file as a path relative to the worktree root, or the path
/// itself when it leaves the worktree. An absolute path inside the worktree
/// is made relative; `..`, a home-relative path or any other absolute path
/// is outside.
fn inside(file: &str, tree: &Path) -> Result<String, String> {
    let path = Path::new(file);
    let relative = if path.is_absolute() {
        path.strip_prefix(tree).map_err(|_| file.to_owned())?
    } else {
        path
    };
    let normal = relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if file.is_empty() || file.starts_with('~') || !normal || relative.as_os_str().is_empty() {
        return Err(file.to_owned());
    }
    Ok(relative.to_string_lossy().into_owned())
}

/// The first absolute or home-relative path the text names outside the
/// worktree. Words are split on whitespace and on the quotes, brackets and
/// punctuation that wrap a path in prose; a URL's path is not a path here.
fn outside_path(text: &str, tree: &Path) -> Option<String> {
    let wrapping = |c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | ';'
            )
    };
    text.split(wrapping).find_map(|word| {
        let word = word.trim_end_matches(['.', ':', '!', '?']);
        let word = word.split_once(':').map_or(word, |(path, rest)| {
            if rest
                .chars()
                .all(|c| c.is_ascii_digit() || c == ':' || c == '-')
            {
                path
            } else {
                word
            }
        });
        let named = (word.starts_with('/') && word.len() > 1 && !word.starts_with("//"))
            || word.starts_with("~/")
            || word == "~";
        let path = Path::new(word);
        let escapes = path.components().any(|c| c == Component::ParentDir);
        (named && (escapes || !path.starts_with(tree))).then(|| word.to_owned())
    })
}

/// At most `limit` characters of `text`.
pub fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    format!("{}…", text.chars().take(limit).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_verdict_is_the_last_non_empty_line_matched_exactly() {
        assert_eq!(verdict("ok\nVERDICT: APPROVE\n\n"), "approve");
        assert_eq!(verdict("VERDICT: CHANGES_REQUESTED"), "changes_requested");
        assert_eq!(verdict("VERDICT: APPROVE\nbut one more thing"), "none");
        assert_eq!(verdict("VERDICT: approve"), "none");
        assert_eq!(verdict("Final VERDICT: APPROVE"), "none");
        assert_eq!(verdict(""), "none");
    }

    #[test]
    fn findings_keep_structure_inside_the_worktree_and_refuse_the_rest() {
        let tree = Path::new("/srv/reviews/r1/tree");
        let reply = "Looked at it.\n\
            - FINDING must-fix src/a.rs:12 the lock is dropped early\n\
            FINDING nit /srv/reviews/r1/tree/src/b.rs:3: a typo\n\
            FINDING bogus src/c.rs:1 ignored\n\
            FINDING should-fix src/d.rs ignored without a line\n\
            VERDICT: CHANGES_REQUESTED";
        let kept = findings(reply, tree, &[]).unwrap();
        assert_eq!(
            kept,
            vec![
                Finding {
                    severity: "must-fix".into(),
                    file: "src/a.rs".into(),
                    line: 12,
                    text: "the lock is dropped early".into()
                },
                Finding {
                    severity: "nit".into(),
                    file: "src/b.rs".into(),
                    line: 3,
                    text: "a typo".into()
                },
            ]
        );
        for reply in [
            "FINDING nit ../outside.rs:1 escapes",
            "FINDING nit /etc/passwd:1 outside",
            "see /home/captain/.ssh/id_ed25519 for the key",
            "the token lives in ~/.config/gh/hosts.yml",
            "read /srv/reviews/r1/tree/../../secrets.txt",
        ] {
            let refused = findings(reply, tree, &[]).unwrap_err();
            assert!(
                refused.contains("outside the worktree"),
                "{reply}: {refused}"
            );
        }
        // A URL's path and a path inside the worktree are not refused.
        assert!(
            findings(
                "https://github.com/a/b/pull/3 and /srv/reviews/r1/tree/x.rs",
                tree,
                &[]
            )
            .is_ok()
        );
        let refused = findings(
            "it printed hunter2-secret-value",
            tree,
            &[b"hunter2-secret-value".to_vec()],
        )
        .unwrap_err();
        assert!(refused.contains("stored credential value"), "{refused}");
        // A value too short to search for is not one.
        assert!(findings("abc", tree, &[b"abc".to_vec()]).is_ok());
    }
}
