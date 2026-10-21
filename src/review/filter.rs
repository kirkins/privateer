//! What a review's reply may leave behind: its verdict and its structured
//! findings, filtered where the record is stored, so a post carries only
//! filtered records.
//!
//! The verdict is read from the reply's last non-empty line alone, matched
//! exactly; anything else is `none`. A finding is one line, `FINDING
//! <severity> <path>:<line> <text>`, whose path names a file inside the
//! review's worktree. Everything else in the reply is the reviewer's working
//! and is not kept. A reply is refused outright, keeping nothing, when it
//! carries a secret or names a path outside the worktree: the runtime never
//! stores, and so never posts, such a review.
//!
//! A secret is a stored credential value whole, any 16-byte piece of one
//! (one line of a multi-line key is such a piece), the same pieces in
//! base64 or hex, or a token of a common shape the runtime never stored
//! (`ghp_`, `gho_`, `github_pat_`, `sk-ant-`, `AKIA`, a PEM header), such
//! as the forge login of the ship user's own `gh`. A path is any absolute,
//! `file:` or home-relative (`~`, `~user`, `$HOME`) path wherever it stands
//! in a word (`HOME=/x`, `{/x}`, `//x`). Only the worktree and the review
//! folder's own `diff.patch`, `files.txt` and `pr.md` may be named, and a
//! kept finding names them relative, so the record never carries the
//! captain's folder layout. Past those, the text may not carry the runtime's
//! `$HOME` value, `/home/`, `/root/` or `/etc/` anywhere, so a path glued to
//! other text (`x:/home/…`, `@/home/…`) is refused too. Zero-width and other
//! invisible format characters are taken out of the reply before any of
//! this, so one cannot hide a word's start. A relative path in the text, `../x` included,
//! is the reviewer quoting code (`include_str!("../x")`) and passes; only a
//! finding's own path must stay inside the worktree, so a finding whose
//! path climbs out with `..` is refused.
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// Severities a finding may carry, in the order a summary lists them.
pub const SEVERITIES: [&str; 3] = ["must-fix", "should-fix", "nit"];
/// Findings kept from one reply, and the characters kept of each.
const MAX_FINDINGS: usize = 50;
const FINDING_CHARS: usize = 500;
/// Stored credentials shorter than this are not searched for: a value that
/// short matches ordinary prose and would refuse honest reviews.
const MIN_SECRET: usize = 8;
/// A piece of a stored value this long, found anywhere in a reply, is a
/// leak of that value.
const PIECE: usize = 16;
/// The review folder's own files, which a reviewer may name.
pub const FOLDER_FILES: [&str; 3] = ["diff.patch", "files.txt", "pr.md"];
/// Token prefixes of common shapes, each needing this many token characters
/// after it to count.
const TOKEN_PREFIXES: [&str; 8] = [
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "sk-ant-",
    "glpat-",
];
const TOKEN_TAIL: usize = 16;

/// A reviewer's finding, as stored: its severity, its file relative to the
/// worktree root, its line, and its text.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    pub severity: String,
    pub file: String,
    pub line: u64,
    pub text: String,
}

/// What a reply may name: the worktree, and the review folder it sits in for
/// that folder's own files; and the runtime's home folder, which it may not.
#[derive(Clone, Copy, Debug)]
pub struct Allowed<'a> {
    pub tree: &'a Path,
    pub folder: &'a Path,
    pub home: Option<&'a Path>,
}

impl Allowed<'_> {
    fn names(&self, path: &Path) -> bool {
        path.starts_with(self.tree) || FOLDER_FILES.iter().any(|f| path == self.folder.join(f))
    }
    /// Text with the worktree and the review folder written relative.
    fn relative(&self, text: &str) -> String {
        let tree = self.tree.to_string_lossy();
        let folder = self.folder.to_string_lossy();
        text.replace(&format!("{tree}/"), "")
            .replace(tree.as_ref(), ".")
            .replace(&format!("{folder}/"), "")
    }
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

/// The structured findings of a reply, or why the reply is refused.
/// `secrets` are the stored credential values.
pub fn findings(
    reply: &str,
    allowed: Allowed,
    secrets: &[Vec<u8>],
) -> Result<Vec<Finding>, String> {
    let reply = &visible(reply);
    if let Some(why) = secret(reply, secrets) {
        return Err(why);
    }
    if let Some(path) = outside_path(reply, allowed) {
        return Err(format!(
            "the reply names a path outside the worktree: {}",
            clip(&path, 120)
        ));
    }
    if let Some(folder) = private_folder(reply, allowed) {
        return Err(format!(
            "the reply names a path outside the worktree: it carries {folder}"
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
        // A finding on the review folder's own diff or text names no file
        // of the change: it is dropped, not refused.
        let Some(file) = inside(file, allowed).map_err(|path| {
            format!(
                "a finding names a path outside the worktree: {}",
                clip(&path, 120)
            )
        })?
        else {
            continue;
        };
        let text = words
            .next()
            .unwrap_or_default()
            .trim_start_matches([':', '-', '—', ' '])
            .trim();
        if kept.len() < MAX_FINDINGS {
            kept.push(Finding {
                severity,
                file,
                line: line_no,
                text: clip(&allowed.relative(text), FINDING_CHARS),
            });
        }
    }
    Ok(kept)
}

/// Why a reply carries a secret, if it does: a stored value or a piece of
/// one, raw or encoded, or a token of a common shape.
fn secret(reply: &str, secrets: &[Vec<u8>]) -> Option<String> {
    let bytes = reply.as_bytes();
    let mut pieces: HashSet<&[u8]> = HashSet::new();
    let mut encoded: Vec<Vec<u8>> = vec![];
    for secret in secrets.iter().filter(|s| s.len() >= MIN_SECRET) {
        if secret.len() < PIECE {
            if bytes.windows(secret.len()).any(|w| w == secret.as_slice()) {
                return Some("the reply contains a stored credential value".to_owned());
            }
            continue;
        }
        encoded.extend(encodings(secret));
    }
    for secret in secrets.iter().filter(|s| s.len() >= PIECE) {
        pieces.extend(secret.windows(PIECE));
    }
    for encoding in &encoded {
        pieces.extend(encoding.windows(PIECE));
    }
    if bytes.len() >= PIECE && bytes.windows(PIECE).any(|w| pieces.contains(w)) {
        return Some("the reply contains a stored credential value, or a piece of one".to_owned());
    }
    token_shape(reply).map(|shape| format!("the reply contains what looks like a {shape}"))
}

/// A stored value as base64 (standard and URL-safe, at each of the three
/// byte alignments it can take inside a longer encoded text, less the last
/// group that alignment changes) and as hex.
fn encodings(secret: &[u8]) -> Vec<Vec<u8>> {
    const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut found = vec![];
    for shift in 0..3 {
        let part = &secret[shift.min(secret.len())..];
        for alphabet in [STANDARD, URL] {
            let mut text = vec![];
            for chunk in part.chunks(3) {
                let n = chunk.iter().fold(0u32, |n, b| (n << 8) | u32::from(*b))
                    << (8 * (3 - chunk.len()));
                for i in 0..=chunk.len() {
                    text.push(alphabet[((n >> (18 - 6 * i)) & 63) as usize]);
                }
            }
            text.truncate(text.len().saturating_sub(4));
            found.push(text);
        }
    }
    let hex = |upper: bool| -> Vec<u8> {
        secret
            .iter()
            .flat_map(|b| {
                let s = if upper {
                    format!("{b:02X}")
                } else {
                    format!("{b:02x}")
                };
                s.into_bytes()
            })
            .collect()
    };
    found.push(hex(false));
    found.push(hex(true));
    found
}

/// The kind of token a reply carries in a common shape, if any.
fn token_shape(reply: &str) -> Option<&'static str> {
    let token_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    for prefix in TOKEN_PREFIXES {
        for (at, _) in reply.match_indices(prefix) {
            let before = reply[..at].chars().next_back();
            if before.is_some_and(|c| c.is_ascii_alphanumeric()) {
                continue;
            }
            let tail = reply[at + prefix.len()..]
                .chars()
                .take_while(|c| token_char(*c))
                .count();
            if tail >= TOKEN_TAIL {
                return Some("forge or API token");
            }
        }
    }
    for (at, _) in reply.match_indices("AKIA") {
        let before = reply[..at].chars().next_back();
        let tail: String = reply[at + 4..].chars().take(17).collect();
        let key = tail
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            .count();
        if !before.is_some_and(|c| c.is_ascii_alphanumeric()) && key == 16 {
            return Some("cloud access key");
        }
    }
    for (at, _) in reply.match_indices("-----BEGIN") {
        let rest = &reply[at + "-----BEGIN".len()..];
        let label: String = rest
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || *c == ' ')
            .collect();
        if rest[label.len()..].starts_with("-----") || label.trim().is_empty() {
            return Some("PEM key or certificate");
        }
    }
    None
}

/// A finding's file as a path relative to the worktree root; None for one of
/// the review folder's own files; or the path itself when it leaves the
/// worktree. An absolute path inside the worktree is made relative; `..`, a
/// home-relative path or any other absolute path is outside.
fn inside(file: &str, allowed: Allowed) -> Result<Option<String>, String> {
    let path = Path::new(file);
    let relative = if path.is_absolute() {
        let normal = normalize(path);
        if FOLDER_FILES
            .iter()
            .any(|f| normal == allowed.folder.join(f))
        {
            return Ok(None);
        }
        path.strip_prefix(allowed.tree)
            .map_err(|_| file.to_owned())?
    } else {
        path
    };
    let normal = relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if file.is_empty() || file.starts_with('~') || !normal || relative.as_os_str().is_empty() {
        return Err(file.to_owned());
    }
    Ok(Some(relative.to_string_lossy().into_owned()))
}

/// An absolute path with `.` and `..` resolved lexically and repeated
/// slashes collapsed; `..` never climbs past the root.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::Normal(part) => normal.push(part),
            _ => {}
        }
    }
    normal
}

/// The first path the text names outside what it may name. Words are split
/// on whitespace and on the quotes, brackets, braces and operators that
/// wrap a path in prose or code, so a path inside `HOME=/x` or `{/x}` is a
/// word of its own. A URL is not a path, except a `file:` one, and a
/// relative path is not one either: `findings` holds a finding's own path
/// to the worktree.
fn outside_path(text: &str, allowed: Allowed) -> Option<String> {
    let wrapping = |c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '`' | '"'
                    | '\''
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '<'
                    | '>'
                    | '{'
                    | '}'
                    | ','
                    | ';'
                    | '='
                    | '|'
                    | '&'
                    | '*'
            )
    };
    text.split(wrapping).find_map(|raw| {
        if raw.is_empty() {
            return None;
        }
        let word = raw.trim_end_matches(['.', ':', '!', '?']);
        let word = word.split_once(':').map_or(word, |(path, rest)| {
            if !path.is_empty()
                && rest
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == ':' || c == '-')
            {
                path
            } else {
                word
            }
        });
        let outside = |path: &str| -> bool {
            !path.is_empty() && path != "/" && !allowed.names(&normalize(Path::new(path)))
        };
        let lower = word.to_ascii_lowercase();
        let named = if let Some(rest) = lower.strip_prefix("file:") {
            // file:/x, file:///x and file://host/x all name /x.
            let rest = &word[word.len() - rest.len()..];
            let rest = rest
                .strip_prefix("//")
                .map_or(rest, |r| r.find('/').map_or("", |at| &r[at..]));
            outside(if rest.is_empty() { "/" } else { rest }) || rest.is_empty()
        } else if word.contains("://") {
            false
        } else if let Some(rest) = word.strip_prefix('~') {
            rest.is_empty()
                || rest.starts_with('/')
                || rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '+' || c == '-')
        } else if let Some(rest) = word.strip_prefix('$') {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && (rest[name.len()..].starts_with('/')
                    || matches!(name.as_str(), "HOME" | "USERPROFILE")
                    || name.starts_with("XDG_"))
        } else {
            word.starts_with('/') && word.len() > 1 && outside(word)
        };
        named.then(|| word.to_owned())
    })
}

/// Whether a character is invisible formatting: zero-width spaces and
/// joiners, direction marks and overrides, word joiners and invisible
/// operators, variation selectors, the byte order mark, soft hyphens, tag
/// characters and the other format (`Cf`) characters, and blank fillers.
fn invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{0600}'..='\u{0605}' | '\u{06DD}' | '\u{070F}'
        | '\u{0890}'..='\u{0891}' | '\u{08E2}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}'
        | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}'
        | '\u{FFF0}'..='\u{FFFB}' | '\u{110BD}' | '\u{110CD}' | '\u{13430}'..='\u{1343F}'
        | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}' | '\u{E0000}'..='\u{E0FFF}')
}

/// The reply with its invisible formatting taken out.
fn visible(reply: &str) -> String {
    reply.chars().filter(|c| !invisible(*c)).collect()
}

/// A home or system folder the text carries anywhere, glued to other text
/// or not, once the worktree's and the review folder's own paths are
/// written relative: the runtime's `$HOME`, `/home/`, `/root/` or `/etc/`.
fn private_folder(text: &str, allowed: Allowed) -> Option<&'static str> {
    let rest = allowed.relative(text);
    let home = allowed
        .home
        .map(|h| h.to_string_lossy().trim_end_matches('/').to_owned())
        .filter(|h| h.len() > 1);
    if home.is_some_and(|h| rest.contains(&h)) {
        return Some("the runtime's $HOME");
    }
    ["/home/", "/root/", "/etc/"]
        .into_iter()
        .find(|folder| rest.contains(folder))
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

    const TREE: &str = "/srv/reviews/r1/tree";
    const FOLDER: &str = "/srv/reviews/r1";
    const HOME: &str = "/srv/captain";

    fn allowed() -> Allowed<'static> {
        Allowed {
            tree: Path::new(TREE),
            folder: Path::new(FOLDER),
            home: Some(Path::new(HOME)),
        }
    }

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
        let reply = "Looked at it.\n\
            - FINDING must-fix src/a.rs:12 the lock is dropped early\n\
            FINDING nit /srv/reviews/r1/tree/src/b.rs:3: a typo, see /srv/reviews/r1/tree/src/c.rs:4\n\
            FINDING bogus src/c.rs:1 ignored\n\
            FINDING should-fix src/d.rs ignored without a line\n\
            FINDING nit /srv/reviews/r1/diff.patch:9 hunk header, dropped\n\
            VERDICT: CHANGES_REQUESTED";
        let kept = findings(reply, allowed(), &[]).unwrap();
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
                    text: "a typo, see src/c.rs:4".into()
                },
            ]
        );
        // No kept text carries the captain's folder layout.
        assert!(!format!("{kept:?}").contains("/srv/"));
    }

    #[test]
    fn every_way_of_naming_a_path_outside_is_refused() {
        for reply in [
            "FINDING nit ../outside.rs:1 escapes",
            "FINDING nit /etc/passwd:1 outside",
            "see /home/captain/.ssh/id_ed25519 for the key",
            "the token lives in ~/.config/gh/hosts.yml",
            "read /srv/reviews/r1/tree/../../secrets.txt",
            "HOME=/home/kirkins/.ssh",
            "file:///etc/passwd",
            "file:/etc/passwd",
            "{/etc/passwd}",
            "//etc/passwd",
            "~kirkins/notes",
            "~",
            "$HOME/.config/gh/hosts.yml",
            "${HOME}/.ssh",
            "it reads $HOME",
            "$XDG_STATE_HOME",
            "FINDING must-fix src/../../escape.rs:3 climbs out",
            "FINDING nit ..:1 the folder above",
            "/srv/reviews/r1/other.txt",
            "/srv/reviews/r1/treehouse/x",
        ] {
            let refused = findings(reply, allowed(), &[]).unwrap_err();
            assert!(
                refused.contains("outside the worktree"),
                "{reply}: {refused}"
            );
        }
        // A URL, prose, a path inside the worktree, and the review folder's
        // own files are not refused.
        for reply in [
            "https://github.com/a/b/pull/3 and /srv/reviews/r1/tree/x.rs",
            "the diff is /srv/reviews/r1/diff.patch, and /srv/reviews/r1/pr.md: fine.",
            "and/or, ~5 minutes, $5, src/../lib.rs, ... and done.",
            "the cost is $PATH-free",
        ] {
            assert!(findings(reply, allowed(), &[]).is_ok(), "{reply}");
        }
    }

    #[test]
    fn a_home_or_system_folder_glued_to_other_text_or_hidden_is_refused() {
        for reply in [
            "x:/home/kirkins/.ssh/id_rsa",
            "@/home/kirkins/.ssh/id_rsa",
            "see \u{200B}/home/kirkins/.ssh/id_rsa",
            "see /\u{200D}home/kirkins/.ssh",
            "cat\u{2060}/etc/shadow",
            "key=/root/.aws/credentials",
            "prefix/etc/passwd",
            "../../home/kirkins/notes",
            "it reads srv:/srv/captain/notes",
            "it reads X/srv/captain",
            "\u{FEFF}FINDING nit src/a.rs:1 see q/etc/x",
        ] {
            let refused = findings(reply, allowed(), &[]).unwrap_err();
            assert!(
                refused.contains("outside the worktree"),
                "{reply:?}: {refused}"
            );
        }
        // The worktree and review folder's own paths are written relative
        // first, so a worktree under a home folder is not refused, and an
        // invisible character is gone from the kept text.
        let under = Allowed {
            tree: Path::new("/home/captain/ships/a/reviews/r1/tree"),
            folder: Path::new("/home/captain/ships/a/reviews/r1"),
            home: Some(Path::new("/home/captain")),
        };
        let kept = findings(
            "FINDING nit /home/captain/ships/a/reviews/r1/tree/src/a.rs:2 a typo\u{200B} here, \
             see /home/captain/ships/a/reviews/r1/diff.patch\nVERDICT: APPROVE",
            under,
            &[],
        )
        .unwrap();
        assert_eq!(kept[0].file, "src/a.rs");
        assert_eq!(kept[0].text, "a typo here, see diff.patch");
        assert!(findings("the /home/captain/ships/a folder", under, &[]).is_err());
        assert!(findings("an etc/ folder, a home/ page", allowed(), &[]).is_ok());
    }

    #[test]
    fn a_relative_path_quoted_in_text_passes_and_only_a_findings_path_must_stay_inside() {
        // Quoted code climbs with `..` relative to its own file: the reply
        // and its finding are kept.
        let reply = "src/transcript.rs reads include_str!(\"../tests/fixtures/x.jsonl\").\n\
            the text says ../../.. and a/../.. and src/../../escape.rs somewhere\n\
            FINDING nit src/bundled.rs:8 `include_str!(\"../plugins/x.py\")` is fine\n\
            VERDICT: APPROVE";
        let kept = findings(reply, allowed(), &[]).unwrap();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].file, "src/bundled.rs");
        assert!(kept[0].text.contains("../plugins/x.py"), "{kept:?}");
        // The same climb in a finding's own path is refused.
        for reply in [
            "FINDING nit ../x:1 outside",
            "FINDING nit src/../../x.rs:1 outside",
        ] {
            let refused = findings(reply, allowed(), &[]).unwrap_err();
            assert!(
                refused.contains("a finding names a path outside the worktree"),
                "{reply}: {refused}"
            );
        }
    }

    #[test]
    fn a_stored_value_whole_in_part_or_encoded_is_refused() {
        let key = b"-----KEY-----\nAAAAB3NzaC1yc2EAAAADAQABAAABgQC7\nZZZZsecondlineofthekeyZZZZ\n"
            .to_vec();
        let token = b"forge-token-value-123456".to_vec();
        let secrets = vec![key, token.clone(), b"short-12".to_vec(), b"abc".to_vec()];
        let refused = |reply: &str| findings(reply, allowed(), &secrets).unwrap_err();
        assert!(refused("it printed forge-token-value-123456").contains("stored credential"));
        // Part of a value, and one line of a multi-line key.
        assert!(refused("prefix token-value-12345 suffix").contains("piece of one"));
        assert!(refused("ZZZZsecondlineofthekeyZZZZ").contains("piece of one"));
        // Base64 at each alignment, URL-safe, and hex.
        for prefix in ["", "x", "xy"] {
            let mut text = prefix.as_bytes().to_vec();
            text.extend(&token);
            text.extend(b" trailing");
            let encoded = String::from_utf8(encodings(&text)[0].clone()).unwrap();
            // A whole-text standard encoding with no shift.
            assert!(
                refused(&format!("blob: {encoded}")).contains("piece of one"),
                "{prefix}: {encoded}"
            );
        }
        let hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
        assert!(refused(&hex).contains("piece of one"));
        // A short stored value is matched whole; a tiny one not at all.
        assert!(refused("short-12").contains("stored credential"));
        assert!(findings("abc", allowed(), &secrets).is_ok());
    }

    #[test]
    fn tokens_of_common_shapes_are_refused_though_never_stored() {
        for reply in [
            "gho_16C7e42F292c6912E7710c838347Ae178B4a",
            "token=ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "github_pat_11ABCDEFG0123456789_abcdefghijklmnop",
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
            "AKIAIOSFODNN7EXAMPLE",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "-----BEGIN",
        ] {
            assert!(
                findings(reply, allowed(), &[])
                    .unwrap_err()
                    .contains("looks like a"),
                "{reply}"
            );
        }
        // The bare prefixes in prose are not tokens.
        assert!(findings("the filter matches ghp_ and AKIA prefixes", allowed(), &[]).is_ok());
    }
}
