//! One block, one ask: a harness permission denial becomes one typed captain
//! decision, and the task it stopped waits on it. See
//! docs/design/one-block-one-ask.md.
//!
//! A denial is read from the job's own log when the job settles, and each
//! denied tool use becomes the narrowest allow rule and a short, masked
//! preview. The ask, the park and the notices are written in the settling
//! transaction, so they commit with the job's status.
use crate::mail::CAPTAIN;
use crate::store::Store;
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Distinct rules (or tools) one ask carries; the rest are counted.
pub(crate) const MAX_DENIALS: usize = 4;
/// What the card shows of a denied input, after masking.
const PREVIEW_CHARS: usize = 160;
/// How much of the end of a log settlement reads for the denial line. The
/// line holds at most four inputs of about 4 KB each.
const DENIAL_TAIL_BYTES: u64 = 64 * 1024;
const MASK: &str = "•••";
/// What an ask names when the denied tool use could not be read.
pub(crate) const UNIDENTIFIED: &str = "denied tool not identified";
/// What a card shows of a refusal's reason, after masking.
const REASON_CHARS: usize = 160;
/// How long after its refusal an ask says when the denial was refused.
const LATE_MS: i64 = 5 * 60_000;
/// The reason a block ask is withdrawn with when its own session got past
/// it: a later call of the same tool with the same input succeeded.
pub(crate) const PASSED: &str = "the session got past this block";
/// Senders of open requests to a blocked ship who are told about its ask.
const MAX_REQUESTERS: usize = 5;
/// The reason a covering policy change ends an ask with.
pub(crate) const POLICY_CHANGED: &str = "policy changed";
/// The reason a policy change ends an ask with when it grants nothing the ask
/// named: its denials name no rule, so it ends for one retry.
pub(crate) const POLICY_CHANGED_UNGRANTED: &str =
    "policy changed, but nothing this ask named was granted; retry once";
/// The reason a change to its ship's standing authorizations ends an ask
/// offering one with: whether the new wording covers it cannot be read, and
/// auto mode reads it only from its next launch.
pub(crate) const POLICY_CHANGED_AUTHORIZATIONS: &str = "policy changed: this ship's standing authorizations changed, but whether they cover this ask cannot be told; relaunch, then retry once";
/// An open block ask, in a query's WHERE clause.
const OPEN_ASK: &str = "recipient='captain' AND kind IN ('request','decision') AND state='open'
    AND json_extract(payload,'$.type')='tool_permission'";

pub(crate) fn init(store: &Store) -> Result<()> {
    // The rules the captain chose to do without, per task: a later denial of
    // only these opens no new ask.
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS permission_declined(ship TEXT NOT NULL, task_id TEXT NOT NULL,
           rule TEXT NOT NULL, policy_hash TEXT NOT NULL, ms INTEGER NOT NULL,
           PRIMARY KEY(ship, task_id, rule));",
    )?;
    Ok(())
}

/// The denials `claude_turn.py` printed as its `native.permission_blocked`
/// line, read from the end of a job's stdout log. A missing log, or no such
/// line, reads as none: the job then reports as an ordinary failure.
pub(crate) fn logged_denials(log: &Path) -> Vec<Value> {
    let read = || -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(log)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(DENIAL_TAIL_BYTES)))?;
        let mut bytes = vec![];
        file.take(DENIAL_TAIL_BYTES).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    let Ok(bytes) = read() else {
        return vec![];
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .filter(|line| line.contains("native.permission_blocked"))
        .find_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            (value["type"] == "native.permission_blocked")
                .then(|| value["denials"].as_array().cloned())
                .flatten()
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d["tool_name"].is_string())
        .collect()
}

fn shell_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/+-@".contains(c))
}

/// The narrowest allow rule that would have let this tool use run, or None
/// when none can be derived safely (the captain then gets no Allow option).
/// `base` is the folder relative paths are made relative to.
pub(crate) fn narrow_rule(tool: &str, input: &Value, base: Option<&Path>) -> Option<String> {
    let rule = match tool {
        "Bash" => {
            let command = input["command"].as_str()?.trim();
            // A compound command does not say which part was denied.
            if command.is_empty() || command.chars().any(|c| ";&|<>$`(){}\\\n'\"*?".contains(c)) {
                return None;
            }
            let mut words = command.split_whitespace();
            let first = words.next()?;
            // An assignment or a word the shell would expand is not a prefix.
            if !shell_word(first) || first.contains('=') || secret_shaped(first) {
                return None;
            }
            match words.next() {
                Some(second)
                    if shell_word(second)
                        && !second.starts_with('-')
                        && !second.contains('/')
                        && !second.contains('.')
                        // The rule is shown unmasked, so a key is no prefix.
                        && !secret_shaped(second) =>
                {
                    format!("Bash({first} {second} *)")
                }
                _ => format!("Bash({first} *)"),
            }
        }
        "Edit" | "Write" | "Read" | "NotebookEdit" => {
            let path = input["file_path"]
                .as_str()
                .or_else(|| input["notebook_path"].as_str())?;
            let path = Path::new(path);
            if path.components().any(|c| c.as_os_str() == "..") || path.to_str()?.contains('*') {
                return None;
            }
            match base.and_then(|base| path.strip_prefix(base).ok()) {
                Some(inside) if !inside.as_os_str().is_empty() => {
                    format!("{tool}(./{})", inside.to_str()?)
                }
                // Claude reads `//path` as absolute from the filesystem root.
                _ if path.is_absolute() => format!("{tool}(/{})", path.to_str()?),
                _ => format!("{tool}(./{})", path.to_str()?.trim_start_matches("./")),
            }
        }
        "WebFetch" => {
            let url = input["url"].as_str()?;
            let rest = url.split_once("://")?.1;
            let host = rest.split(['/', '?', '#']).next()?;
            let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
            let host = host.split(':').next()?;
            if host.is_empty()
                || !host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
            {
                return None;
            }
            format!("WebFetch(domain:{})", host.to_ascii_lowercase())
        }
        // A tool whose input has no prefix to narrow by is allowed by name.
        _ if !tool.is_empty()
            && tool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
            && !matches!(tool, "Bash" | "Edit" | "Write" | "Read" | "unknown") =>
        {
            tool.to_owned()
        }
        _ => return None,
    };
    // A rule must be one the fleet policy accepts, and never every use of a tool.
    let valid = rule.len() <= 512
        && !rule.starts_with('-')
        && !rule.chars().any(char::is_control)
        && !rule.ends_with("(*)")
        && !rule.ends_with("( *)");
    valid.then_some(rule)
}

/// A path with each secret-shaped part hidden alone, so a digit in
/// `/home/me/app2/src/main.rs` leaves it readable while a token pasted in as
/// one of its parts is still hidden. A word with `+` or `=` reads as base64,
/// not a path, and is judged whole.
fn masked_path(word: &str) -> Option<String> {
    let rooted = ["/", "./", "../", "~/"].iter().any(|p| word.starts_with(p));
    if !rooted || word.matches('/').count() < 2 || word.contains(['+', '=']) {
        return None;
    }
    Some(
        word.split('/')
            .map(|part| if secret_shaped(part) { MASK } else { part })
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// Whether a run of characters looks like a key or token: long, and mixing
/// letters with digits.
pub(crate) fn secret_shaped(word: &str) -> bool {
    let body: String = word
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "+/=_-".contains(*c))
        .collect();
    body.len() >= 20
        && body.len() * 10 >= word.len() * 9
        && body.chars().any(|c| c.is_ascii_digit())
        && body.chars().any(|c| c.is_ascii_alphabetic())
}

pub(crate) fn sensitive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "key", "token", "secret", "pass", "auth", "cred", "cookie", "session",
    ]
    .iter()
    .any(|word| name.contains(word))
}

/// Mask values that look secret: long hex or base64 runs, `NAME=value`
/// where the name sounds sensitive, the word after `Bearer` or `token`, and
/// credentials in a URL. What is left is for a person to recognise the tool
/// use, not to rebuild it.
pub(crate) fn mask(text: &str) -> String {
    let mut out: Vec<String> = vec![];
    let mut hide_next = false;
    for word in text.split_inclusive(char::is_whitespace) {
        let trimmed = word.trim_end();
        let tail = &word[trimmed.len()..];
        let lowered = trimmed.to_ascii_lowercase();
        let keyword = matches!(
            lowered.trim_matches(|c: char| !c.is_ascii_alphabetic()),
            "bearer" | "token" | "basic" | "authorization"
        ) || lowered.ends_with("authorization:");
        // In `Authorization: Bearer <token>` the scheme is shown and the
        // token after it hidden.
        let masked = if keyword {
            hide_next = true;
            trimmed.to_owned()
        } else if hide_next && !trimmed.is_empty() {
            hide_next = false;
            MASK.to_owned()
        } else if let Some((name, _)) = trimmed.split_once('=')
            && sensitive_name(name)
        {
            format!("{name}={MASK}")
        } else if let Some((scheme, rest)) = trimmed.split_once("://")
            && let Some((_, host)) = rest.split_once('@')
        {
            format!("{scheme}://{MASK}@{host}")
        } else if let Some(path) = masked_path(trimmed) {
            path
        } else if secret_shaped(trimmed) {
            MASK.to_owned()
        } else {
            trimmed.to_owned()
        };
        out.push(format!("{masked}{tail}"));
    }
    out.concat()
}

/// A tool input with every value under a sensitive key hidden and the other
/// strings masked, for tools whose preview is their input as JSON.
fn mask_json(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let shown = if sensitive_name(key) && !value.is_object() {
                        json!(MASK)
                    } else {
                        mask_json(value)
                    };
                    (key.clone(), shown)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(mask_json).collect()),
        Value::String(text) => json!(mask(text)),
        other => other.clone(),
    }
}

/// Text on one line: control characters read as spaces.
fn flat(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// A short, masked preview of a denied tool use, at most 160 characters.
pub(crate) fn preview(tool: &str, input: &Value) -> String {
    let raw = match tool {
        "Bash" => input["command"].as_str(),
        "Edit" | "Write" | "Read" => input["file_path"].as_str(),
        "NotebookEdit" => input["notebook_path"].as_str(),
        "WebFetch" => input["url"].as_str(),
        _ => None,
    };
    // A heredoc's or here-string's contents are what it feeds a program,
    // often a password: the preview ends where it starts.
    let raw = raw.map(|raw| match (tool, raw.find("<<")) {
        ("Bash", Some(at)) => format!("{}<<…", &raw[..at]),
        _ => raw.to_owned(),
    });
    let masked = match raw {
        Some(raw) => mask(&flat(&raw)),
        None => flat(&mask_json(input).to_string()),
    };
    if masked.chars().count() <= PREVIEW_CHARS {
        masked
    } else {
        format!(
            "{}…",
            masked.chars().take(PREVIEW_CHARS - 1).collect::<String>()
        )
    }
}

/// The denial an ask carries when the denied tool use could not be read: it
/// names no tool, so it offers no Allow.
pub(crate) fn unidentified() -> Value {
    json!({"tool_name": "", "tool_input": {}, "unidentified": true})
}

/// The length of a shell word's redirection operator, such as the `2>&` of
/// `2>&1`, the `>` of `>file` or the `<` of `<file`. None when the word is
/// no redirection.
fn redirection(word: &str) -> Option<usize> {
    let digits = word.len() - word.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let rest = &word[digits..];
    if !(rest.starts_with(['<', '>']) || rest.starts_with("&>")) {
        return None;
    }
    let mut operator = rest.len() - rest.trim_start_matches(['<', '>', '&']).len();
    // `>|` clobbers: its bar is the operator's, not a pipe.
    if rest[..operator].ends_with('>') && rest[operator..].starts_with('|') {
        operator += 1;
    }
    Some(digits + operator)
}

/// What of a redirection's target still separates commands, such as the `;`
/// of `>/dev/null;`.
fn separator_tail(target: &str) -> &str {
    target
        .find(|c: char| ";&|()`".contains(c))
        .map_or("", |at| &target[at..])
}

/// The programs a shell command runs, at most four: the first word of each
/// part, without assignments, paths or anything that looks like a key. Only
/// the first line is read, up to any heredoc: what follows is a program's
/// input, not a program.
fn programs(command: &str) -> Vec<String> {
    let line = command.split(['\n', '\r']).next().unwrap_or_default();
    let line = line.split("<<").next().unwrap_or_default();
    let mut words = vec![];
    let mut target = false;
    for word in line.split_whitespace() {
        if std::mem::take(&mut target) {
            words.push(separator_tail(word));
            continue;
        }
        match redirection(word) {
            Some(operator) if operator == word.len() => target = true,
            Some(operator) => words.push(separator_tail(&word[operator..])),
            None => words.push(word),
        }
    }
    let line = words.join(" ");
    let mut found: Vec<String> = vec![];
    for part in line.split(|c: char| ";&|()`".contains(c)) {
        let Some(word) = part.split_whitespace().find(|w| !w.contains('=')) else {
            continue;
        };
        let word = word.rsplit('/').next().unwrap_or(word);
        if shell_word(word)
            && !word.starts_with('-')
            && word.len() <= 32
            && !secret_shaped(word)
            && !found.iter().any(|f| f == word)
        {
            found.push(word.to_owned());
        }
    }
    found.truncate(4);
    found
}

/// A classifier reason's label, `[Credential Materialization]` read as
/// `Credential Materialization`, when it is a short plain label.
fn label(reason: &str) -> Option<&str> {
    let inside = reason.strip_prefix('[')?.split_once(']')?.0.trim();
    (!inside.is_empty()
        && inside.len() <= 60
        && inside
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " -/".contains(c)))
    .then_some(inside)
}

/// A standing authorization the captain could record instead of an Allow
/// rule, in plain words from the programs a command runs and the reason it
/// was refused. Its words are plain shell words and a plain label, so it
/// quotes safely in a command line; the captain edits it before keeping it.
fn authorization(command: &str, reason: Option<&str>, classifier: bool) -> String {
    let programs = programs(command);
    let what = match programs.as_slice() {
        [] => "the shell commands".to_owned(),
        [one] => format!("{one} commands"),
        [rest @ .., last] => format!("{} and {last} commands", rest.join(", ")),
    };
    let why = match reason.and_then(label) {
        Some(label) if classifier => {
            format!(", even when the auto-mode check flags them as {label}")
        }
        _ if classifier => ", even when the auto-mode check refuses them".to_owned(),
        _ => ", including multi-step ones".to_owned(),
    };
    format!("Running {what} its assigned work needs{why}.")
}

/// The ask's denial records: one per distinct rule (or authorization, or
/// tool, when no rule can be derived), at most four, and how many more there
/// were. A Bash denial no prefix rule covers, or one the auto-mode classifier
/// refused, gets no Allow rule: an allow rule would not satisfy the
/// classifier, so it carries a standing authorization to offer instead.
pub(crate) fn records(denials: &[Value], base: Option<&Path>) -> (Vec<Value>, usize) {
    let mut seen = std::collections::BTreeSet::new();
    let mut records = vec![];
    for denial in denials {
        if denial["unidentified"] == true {
            if seen.insert("unidentified".to_owned()) {
                let mut record = json!({"tool": "", "rule": null, "preview": UNIDENTIFIED,
                    "source": "denial", "unidentified": true});
                transcript_ref(&mut record, denial);
                records.push(record);
            }
            continue;
        }
        let tool = denial["tool_name"].as_str().unwrap_or_default();
        let input = &denial["tool_input"];
        let classifier = denial["classifier"] == true;
        let reason = denial["reason"]
            .as_str()
            .map(|reason| clip(&mask(&flat(reason)), REASON_CHARS))
            .filter(|reason| !reason.trim().is_empty());
        let derived = narrow_rule(tool, input, base);
        let authorize = (tool == "Bash" && (derived.is_none() || classifier)).then(|| {
            authorization(
                input["command"].as_str().unwrap_or_default(),
                reason.as_deref(),
                classifier,
            )
        });
        let rule = if authorize.is_some() { None } else { derived };
        let key = rule
            .clone()
            .or_else(|| authorize.as_ref().map(|a| format!("authorize:{a}")))
            .unwrap_or_else(|| format!("tool:{tool}"));
        if !seen.insert(key) {
            continue;
        }
        let mut record = json!({"tool": tool, "rule": rule, "preview": preview(tool, input),
            "source": "denial"});
        if let Some(reason) = reason {
            record["reason"] = json!(reason);
        }
        if let Some(authorize) = authorize {
            record["authorize"] = json!(authorize);
        }
        transcript_ref(&mut record, denial);
        // What a captain approval's command prefix is matched against, and
        // whether the card shows the whole command, byte for byte: not
        // clipped, cut at a heredoc, flattened or masked.
        if tool == "Bash" && classifier {
            record["classifier"] = json!(true);
            let command = input["command"].as_str();
            if let Some(head) = command.and_then(crate::captain_approvals::command_head) {
                record["head"] = json!(head);
            }
            if command.is_some_and(|command| record["preview"] == command) {
                record["whole"] = json!(true);
            }
        }
        records.push(record);
    }
    let more = records.len().saturating_sub(MAX_DENIALS);
    records.truncate(MAX_DENIALS);
    (records, more)
}

/// Where a denial read from a pane's transcript is recorded: its tool use's
/// id and when it was refused. Neither holds any of its input.
fn transcript_ref(record: &mut Value, denial: &Value) {
    if let Some(id) = denial["tool_use_id"].as_str().filter(|id| !id.is_empty()) {
        record["tool_use_id"] = json!(id);
    }
    if let Some(ms) = denial["denied_ms"].as_i64() {
        record["denied_ms"] = json!(ms);
    }
}

/// The standing authorization an ask offers: its records' suggestions, one
/// line of at most 500 characters, as the fleet policy keeps them.
fn authorization_text(records: &[Value]) -> Option<String> {
    let mut texts: Vec<&str> = records
        .iter()
        .filter_map(|d| d["authorize"].as_str())
        .collect();
    texts.dedup();
    // Its words are ASCII, so 498 characters and the ellipsis fit 500 bytes.
    (!texts.is_empty()).then(|| clip(&texts.join(" "), 498))
}

fn hex(text: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(text))
}

/// What each denial would be allowed by: its rule, else its authorization,
/// else its tool's name. An authorization is the ship's own, so its name
/// carries the ship: no other ship's ask shares its fleet key.
fn names(denials: &[Value], ship: &str) -> Vec<String> {
    let mut names: Vec<String> = denials
        .iter()
        .map(|d| {
            d["rule"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    d["authorize"]
                        .as_str()
                        .map(|a| format!("authorize:{ship}:{a}"))
                })
                .unwrap_or_else(|| format!("tool:{}", d["tool"].as_str().unwrap_or("")))
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The rules an ask lists, for its card and notices.
pub(crate) fn rules_text(block: &Value) -> String {
    let denials = block["denials"].as_array().cloned().unwrap_or_default();
    let mut text = denials
        .iter()
        .map(|d| {
            let tool = d["tool"].as_str().filter(|t| !t.is_empty());
            match (d["rule"].as_str(), d["reason"].as_str()) {
                (Some(rule), _) => rule.to_owned(),
                _ if d["unidentified"] == true => format!("a tool ({UNIDENTIFIED})"),
                (None, Some(reason)) => {
                    format!("{} ({})", tool.unwrap_or("a tool"), clip(reason, 60))
                }
                (None, None) => format!("{} (no narrower rule)", tool.unwrap_or("a tool")),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(more) = block["more"].as_u64().filter(|m| *m > 0) {
        text.push_str(&format!(" and {more} more"));
    }
    text
}

pub(crate) fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(chars - 1).collect::<String>())
    }
}

/// A hash of one ship's standing authorizations in a fleet policy.
fn authorizations_hash(policy: &Value, ship: &str) -> String {
    hex(policy["authorized"][ship].to_string())
}

/// Whether a policy change ends an ask: when the policy grants it (see
/// `grants`), when its ship's standing authorizations changed (see
/// `reauthorized`), or, for an ask with any other denial or from before asks
/// listed their denials, on any policy change. Only the first grants
/// anything; the others end so the step is retried once, and a retry that is
/// denied again asks anew.
pub(crate) fn covered(policy: &Value, policy_hash: &str, ask: &Value) -> bool {
    grants(policy, ask)
        || reauthorized(policy, ask)
        || (!known(ask) && ask["policy_hash"].as_str() != Some(policy_hash))
}

/// What a policy change that ends an ask did for what it asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// The policy lets every denial run.
    Granted,
    /// Its ship's standing authorizations changed; whether they cover it
    /// cannot be told.
    Reauthorized,
    /// Nothing it named was granted; whether the change covers it cannot be
    /// told.
    Ungranted,
}

impl Outcome {
    /// The outcome of a policy change that ends `ask`.
    fn of(policy: &Value, ask: &Value) -> Self {
        if grants(policy, ask) {
            Self::Granted
        } else if reauthorized(policy, ask) {
            Self::Reauthorized
        } else {
            Self::Ungranted
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::Granted => POLICY_CHANGED,
            Self::Reauthorized => POLICY_CHANGED_AUTHORIZATIONS,
            Self::Ungranted => POLICY_CHANGED_UNGRANTED,
        }
    }

    fn from_reason(reason: &str) -> Option<Self> {
        [Self::Granted, Self::Reauthorized, Self::Ungranted]
            .into_iter()
            .find(|o| o.reason() == reason)
    }

    /// What a ship's notice payload says was granted.
    fn granted(self) -> Value {
        match self {
            Self::Granted => json!(true),
            Self::Reauthorized => json!("unknown"),
            Self::Ungranted => json!(false),
        }
    }
}

/// Whether the coverage of every denial of an ask can be read from a policy:
/// each names a derived rule, or a standing authorization the ask recorded
/// its ship's authorizations for.
fn known(ask: &Value) -> bool {
    let block = &ask["block"];
    let seen = block["authorized"].is_string();
    block["denials"].as_array().is_some_and(|d| {
        !d.is_empty()
            && d.iter()
                .all(|d| d["rule"].is_string() || (seen && d["authorize"].is_string()))
    })
}

/// Whether the policy now lets every denial of an ask run: `yolo`, or each
/// denial a derived rule present exactly or by its tool's name. A denial an
/// ask offers a standing authorization for is never granted this way: see
/// `reauthorized`.
pub(crate) fn grants(policy: &Value, ask: &Value) -> bool {
    if policy["mode"] == "yolo" {
        return true;
    }
    let allowed = allowed(policy);
    known(ask) && denials(ask).all(|d| d["rule"].is_string() && rule_allowed(&allowed, d))
}

/// Whether an ask offering a standing authorization ends because its own
/// ship's authorizations changed, never another ship's, with every derived
/// rule it also names allowed. Whether the new authorizations cover it
/// cannot be read from their wording, and auto mode reads them only from
/// the ship's next launch, so this grants nothing the runtime can confirm.
fn reauthorized(policy: &Value, ask: &Value) -> bool {
    if !known(ask) {
        return false;
    }
    let block = &ask["block"];
    let seen = block["authorized"].as_str();
    let ship = block["ship"].as_str().unwrap_or_default();
    let changed = seen != Some(&authorizations_hash(policy, ship));
    let allowed = allowed(policy);
    changed
        && denials(ask).any(|d| !d["rule"].is_string())
        && denials(ask).all(|d| !d["rule"].is_string() || rule_allowed(&allowed, d))
}

fn denials(ask: &Value) -> impl Iterator<Item = &Value> {
    ask["block"]["denials"].as_array().into_iter().flatten()
}

fn allowed(policy: &Value) -> Vec<&str> {
    policy["claude_allow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Whether a denial's derived rule is allowed, exactly or by its tool's name.
fn rule_allowed(allowed: &[&str], denial: &Value) -> bool {
    let rule = denial["rule"].as_str().unwrap_or_default();
    allowed.contains(&rule) || allowed.contains(&denial["tool"].as_str().unwrap_or_default())
}

/// The open block ask a ship raised for a task, if any.
pub(crate) fn open_for_task(store: &Store, ship: &str, task: &str) -> Result<Option<Value>> {
    let id: Option<String> = store
        .connection()
        .query_row(
            &format!("SELECT id FROM mail WHERE sender=?1 AND task_id=?2 AND {OPEN_ASK} ORDER BY seq DESC LIMIT 1"),
            params![ship, task],
            |r| r.get(0),
        )
        .optional()?;
    match id {
        Some(id) => crate::mail::find(store, &id),
        None => Ok(None),
    }
}

/// Fleet homes on this machine other than `here`, for lookups that span
/// ships. A registry that cannot be read means a home with no fleet.
pub(crate) fn other_homes(here: &Path) -> Vec<(String, PathBuf)> {
    let Ok(conn) = crate::fleet::registry() else {
        return vec![];
    };
    crate::fleet::members(&conn)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.home.is_dir() && m.home != here)
        .map(|m| (m.name, m.home))
        .collect()
}

/// Whether an ask's denials may join another ship's ask: only when each one,
/// counted or not, is a derived allow rule. A denial no rule narrows (a
/// compound command, an unknown or unidentified tool) is named by its tool
/// alone, which says nothing about what either ship ran, and an
/// authorization is the asking ship's own.
fn shared_denials(denials: &[Value], more: u64) -> bool {
    more == 0 && !denials.is_empty() && denials.iter().all(|d| d["rule"].is_string())
}

/// Whether a block ask's payload may be attached to another ship's ask. Any
/// other payload may.
pub(crate) fn may_join(payload: &Value) -> bool {
    payload["type"] != "tool_permission"
        || payload["block"]["denials"]
            .as_array()
            .is_some_and(|d| shared_denials(d, payload["block"]["more"].as_u64().unwrap_or(0)))
}

/// An open primary ask for the same denial under the same policy, raised by
/// another ship in this home or another home on this machine.
fn same_denial(store: &Store, ship: &str, fleet_key: &str) -> Result<Option<(String, String)>> {
    let query = format!(
        "SELECT id, sender FROM mail WHERE {OPEN_ASK} AND sender<>?1
         AND json_extract(payload,'$.block.fleet_key')=?2
         AND json_extract(payload,'$.attached_to') IS NULL ORDER BY seq LIMIT 1"
    );
    let find = |store: &Store| -> Result<Option<(String, String)>> {
        Ok(store
            .connection()
            .query_row(&query, params![ship, fleet_key], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?)
    };
    if let Some(found) = find(store)? {
        return Ok(Some(found));
    }
    for (_, home) in other_homes(store.home()) {
        if let Ok(other) = Store::open_readonly(&home, crate::store::Caller::Operator)
            && let Ok(Some(found)) = find(&other)
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Where a job ran: its working directory, if it recorded one.
fn job_folder(job: &Value) -> Option<PathBuf> {
    job["cwd"].as_str().map(PathBuf::from)
}

/// Settle a job's permission denials, in the transaction that ends the job,
/// inside a savepoint: any error rolls back to it, and the job settles with
/// its ordinary outcome instead. Returns what the crew outcome should say.
pub(crate) fn settle(store: &Store, job: &Value) -> Option<Value> {
    let conn = store.connection();
    conn.execute_batch("SAVEPOINT block_ask").ok()?;
    match settle_inner(store, job) {
        Ok(block) => {
            let _ = conn.execute_batch("RELEASE block_ask");
            block
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK TO block_ask; RELEASE block_ask");
            eprintln!(
                "privateer: job {} was denied a tool, but its captain ask could not be written: {error:#}",
                job["id"].as_str().unwrap_or("?")
            );
            None
        }
    }
}

fn settle_inner(store: &Store, job: &Value) -> Result<Option<Value>> {
    let job_id = job["id"].as_str().context("job id")?;
    let incarnation = job["incarnation"].as_str().context("job incarnation")?;
    let log = store
        .home()
        .join("jobs")
        .join(job_id)
        .join(incarnation)
        .join("stdout.log");
    let denials = logged_denials(&log);
    if denials.is_empty() {
        return Ok(None);
    }
    let task_id = job["task_id"].as_str().context("job task")?;
    let task = store.task(task_id)?;
    let ship = task["owner"].as_str().context("task owner")?.to_owned();
    let scope = job_scope(store, job_id)?;
    let (records, more) = records(&denials, job_folder(job).as_deref());
    raise(
        store,
        &ship,
        task_id,
        &task,
        Some(job_id),
        scope,
        records,
        more,
    )
    .map(Some)
}

/// Who a job's denial stopped: the first mate's own wake, an errand, or a
/// crew. Only a wake's ask may offer a standing authorization.
pub(crate) fn job_scope(store: &Store, job_id: &str) -> Result<&'static str> {
    Ok(
        if store.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM wakes WHERE job_id=?1)",
            [job_id],
            |r| r.get::<_, bool>(0),
        )? {
            "wake"
        } else if crate::errand::is_errand(store, job_id)? {
            "errand"
        } else {
            "crew"
        },
    )
}

/// Open (or find) the block ask for a task's denials, park the task when a
/// crew was denied, and tell the ship's requesters. Runs in the caller's
/// transaction. Returns `{ask, rules, duplicate, declined}`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn raise(
    store: &Store,
    ship: &str,
    task_id: &str,
    task: &Value,
    job_id: Option<&str>,
    scope: &str,
    records: Vec<Value>,
    more: usize,
) -> Result<Value> {
    let policy_hash = crate::permissions::policy_hash(store)?;
    let names = names(&records, ship);
    let block_text = rules_text(&json!({"denials": records, "more": more}));
    // The captain already chose to do without these rules for this task.
    let declined: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM permission_declined WHERE ship=?1 AND task_id=?2 AND rule IN (SELECT value FROM json_each(?3))",
        params![ship, task_id, serde_json::to_string(&names)?],
        |r| r.get(0),
    )?;
    if declined as usize == names.len() {
        return Ok(json!({"ask": null, "rules": block_text, "declined": true, "duplicate": false}));
    }
    if let Some(open) = open_for_task(store, ship, task_id)? {
        let ask = open["id"].as_str().unwrap_or_default();
        // A crew that hits an errand's ask parks the task on it too.
        if scope == "crew" && open["payload"]["block"]["scope"] == "errand" {
            store.connection().execute(
                "UPDATE mail SET payload=json_set(payload,'$.block.scope','crew') WHERE id=?1",
                [ask],
            )?;
            let reason = format!("tool permission: {}", rules_text(&open["payload"]["block"]));
            crate::scheduling::park_on_ask(store, task_id, ask, &reason)?;
        }
        // Another source's denial joins it: the pane's session getting past
        // its own refusals says nothing of that one, so the ask stays open.
        if scope != "pane" {
            crate::automode::hold(store, ask)?;
        }
        return Ok(
            json!({"ask": ask, "rules": rules_text(&open["payload"]["block"]),
            "declined": false, "duplicate": true}),
        );
    }
    let episode: i64 = store.connection().query_row(
        "SELECT COUNT(*) FROM mail WHERE sender=?1 AND recipient='captain' AND task_id=?2 AND state <> 'open' AND json_extract(payload,'$.type')='tool_permission'",
        params![ship, task_id],
        |r| r.get(0),
    )?;
    let id = format!(
        "permissions-{}",
        hex(format!("{ship}\0{task_id}\0{policy_hash}\0{episode}"))
    );
    let fleet_key = hex(format!("{}\0{policy_hash}", names.join("\0")));
    let key = hex(format!(
        "{ship}\0{task_id}\0{}\0{policy_hash}",
        names.join("\0")
    ));
    let title = task["title"].as_str().unwrap_or(task_id);
    let rules: Vec<&str> = records.iter().filter_map(|d| d["rule"].as_str()).collect();
    let all_derived = rules.len() == records.len();
    let harness = "Claude";
    let question = clip(&format!("{ship}: {harness} was denied {block_text}"), 200);
    let who = match scope {
        "crew" => "Its crew",
        "errand" => "Its errand",
        _ => "Its first mate",
    };
    let wait = if scope == "errand" {
        "Nothing retries it until you answer."
    } else {
        "The task waits until you answer or the fleet policy covers it (/yolo on also clears it)."
    };
    let offer = crate::captain_approvals::offer(
        store,
        ship,
        scope,
        job_id,
        &records,
        more,
        Store::now_ms(),
    )?;
    let ignored = offer
        .why
        .as_deref()
        .map(|why| format!(" {why}"))
        .unwrap_or_default();
    let why = clip(
        &format!("{who} stopped at this tool, working on {title}. {wait}{ignored}"),
        500,
    );
    let mut options = vec![];
    if all_derived {
        options.push(json!({"id": "allow", "label": clip(&format!("Allow {}", rules.join(", ")), 80),
            "what_happens": clip(&format!("Adds {} to this machine's fleet policy for every Claude ship; every task blocked on it resumes.", rules.join(", ")), 400)}));
    }
    // A crew or an errand never reads the ship's standing authorizations.
    let authorize = authorization_text(&records).filter(|_| !matches!(scope, "crew" | "errand"));
    if let Some(text) = &authorize {
        options.push(json!({"id": "authorize", "label": clip(&format!("Authorize: {text}"), 80),
            "what_happens": clip(&format!("Records the standing authorization '{text}' for {ship}; its auto-mode check reads it from its next launch (`privateer host relaunch {ship}`). Answer with your own wording to record that instead."), 400)}));
    }
    options.extend(offer.options);
    options.push(json!({"id": "without", "label": "Continue without it",
        "what_happens": "The task resumes; its first mate is told the tool stays denied."}));
    options.push(json!({"id": "stop", "label": "Stop the task",
        "what_happens": "The task is held; its owner is told."}));
    let now = Store::now_ms();
    let previews: Vec<String> = records
        .iter()
        .map(|d| {
            let preview = d["preview"].as_str().unwrap_or("");
            let shown = match d["tool"].as_str().filter(|t| !t.is_empty()) {
                Some(tool) => format!("{tool}: {preview}"),
                None => preview.to_owned(),
            };
            let shown = match d["reason"].as_str() {
                Some(reason) => format!("{shown} (refused: {reason})"),
                None => shown,
            };
            // A pane's denial is seen when its screen shows it, which can be
            // well after the refusal: say when it was refused.
            match d["denied_ms"].as_i64().filter(|ms| now - ms >= LATE_MS) {
                Some(ms) => format!(
                    "{shown} (refused at {}, {} before this ask)",
                    crate::routes::utc_time(ms),
                    crate::activation::idle_duration(now - ms)
                ),
                None => shown,
            }
        })
        .collect();
    let mut block = json!({"key": key, "fleet_key": fleet_key, "ship": ship, "task_id": task_id,
        "job_id": job_id, "scope": scope, "harness": "claude", "denials": records, "more": more});
    if let Some(text) = &authorize {
        block["authorize"] = json!(text);
        let policy = crate::permissions::for_store(store)?;
        block["authorized"] = json!(authorizations_hash(&policy, ship));
    }
    if !offer.approvals.is_empty() {
        block["approvals"] = json!(offer.approvals);
    }
    if let Some(approve) = offer.approve {
        block["approve"] = approve;
    }
    let mut payload = json!({"type": "tool_permission", "policy_hash": policy_hash, "block": block,
        "decision": {"question": question, "why": why,
            "blocked": [clip(&format!("{task_id}: {title}"), 200)], "free_text_ok": true,
            "group": "tool-permission", "options": options}});
    let primary = if shared_denials(&records, more as u64) {
        same_denial(store, ship, &fleet_key)?
    } else {
        None
    };
    if let Some((primary, primary_ship)) = &primary {
        payload["about"] = json!(primary);
        payload["attached_to"] = json!(primary);
        payload["attached_ship"] = json!(primary_ship);
    }
    crate::mail::typed_payload("decision", &mut payload)?;
    let job_line = job_id.map(|j| format!(" Job: {j}.")).unwrap_or_default();
    // No narrower rule usually means Claude's auto-mode check refused it,
    // which reads the captain's standing authorizations, not tool rules.
    let remedy = if all_derived {
        "`privateer permissions allow 'Tool(rule)'` or `privateer permissions yolo`".to_owned()
    } else if let Some(text) = &authorize {
        format!(
            "`privateer permissions authorize {ship} '{text}'` (edit the wording first) or `privateer permissions yolo`"
        )
    } else {
        format!(
            "`privateer permissions authorize {ship} 'what you allow'` or `privateer permissions yolo`"
        )
    };
    let body = format!(
        "{question}\n{why}\nDenied: {}.{job_line}\nThe captain can also run {remedy}.",
        previews.join("; ")
    );
    crate::mail::record_runtime(
        store,
        &json!({"id": id, "from": ship, "to": CAPTAIN, "kind": "decision", "task_id": task_id,
            "body": body, "payload": payload, "urgency": "normal"}),
    )?;
    let reason = format!("tool permission: {block_text}");
    if scope == "crew" {
        crate::scheduling::park_on_ask(store, task_id, &id, &reason)?;
    }
    tell_requesters(store, ship, &id, &block_text)?;
    store.audit(
        "permissions.ask",
        &json!({"id": id, "ship": ship, "task_id": task_id,
        "job_id": job_id, "scope": scope, "attached_to": primary.map(|p| p.0)}),
    )?;
    Ok(json!({"ask": id, "rules": block_text, "declined": false, "duplicate": false}))
}

/// Tell the senders of open requests to a blocked ship that the captain has
/// its ask, so they need not relay the block, and give a later relay a
/// correlation to cite. Low notices: they wake nobody.
fn tell_requesters(store: &Store, ship: &str, ask: &str, rules: &str) -> Result<()> {
    let mut statement = store.connection().prepare(
        "SELECT sender, MIN(id) FROM mail WHERE recipient=?1 AND kind IN ('request','decision')
         AND state='open' AND sender NOT IN ('crew','captain') GROUP BY sender ORDER BY MIN(seq) LIMIT ?2",
    )?;
    let requesters = statement
        .query_map(params![ship, MAX_REQUESTERS as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (sender, request) in requesters {
        crate::mail::record_runtime(
            store,
            &json!({"id": format!("block-{}", hex(format!("{ask}\0{sender}"))), "from": ship,
                "to": sender, "kind": "notice", "correlation": request, "urgency": "low",
                "body": format!("{ship} is blocked on captain decision {ask} ({rules}); the captain has it. If you add anything, cite about: {ask}."),
                "payload": {"about": ask, "block_notice": true}}),
        )?;
    }
    Ok(())
}

/// End the open block asks in this home that the current policy covers, and
/// keep the rest open under the new policy's hash. Returns the ended ids.
pub(crate) fn sweep(store: &Store) -> Result<Vec<String>> {
    // Most passes find no open ask: they take no write lock.
    let any: bool = store.connection().query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM mail WHERE {OPEN_ASK})"),
        [],
        |r| r.get(0),
    )?;
    if !any {
        end_orphans(store)?;
        return Ok(vec![]);
    }
    let policy = crate::permissions::for_store(store)?;
    let policy_hash = crate::permissions::policy_hash(store)?;
    let tx = store.write_transaction()?;
    let ids = {
        let mut statement = store.connection().prepare(&format!(
            "SELECT id FROM mail WHERE {OPEN_ASK} AND COALESCE(delivery,'') <> 'received'"
        ))?;
        statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut ended = vec![];
    for id in ids {
        let Some(ask) = crate::mail::find(store, &id)? else {
            continue;
        };
        if covered(&policy, &policy_hash, &ask["payload"]) {
            end_by_policy(store, &policy, &ask)?;
            ended.push((id, ask["from"].as_str().unwrap_or_default().to_owned()));
        } else if ask["payload"]["policy_hash"].as_str() != Some(&policy_hash) {
            store.connection().execute(
                "UPDATE mail SET payload=json_set(payload,'$.policy_hash',?2) WHERE id=?1",
                params![id, policy_hash],
            )?;
        }
    }
    tx.commit()?;
    for (id, ship) in &ended {
        release_attachments(store, id, ship)?;
        crate::mail::end_attachments(store, id, POLICY_CHANGED)?;
    }
    end_orphans(store)?;
    Ok(ended.into_iter().map(|(id, _)| id).collect())
}

/// End an open block ask a policy change covers, in the caller's
/// transaction, and tell an unscheduled task's owner what it means (see
/// `ended_text`).
fn end_by_policy(store: &Store, policy: &Value, ask: &Value) -> Result<()> {
    let id = ask["id"].as_str().unwrap_or_default();
    let outcome = Outcome::of(policy, &ask["payload"]);
    crate::mail::end_request(store, ask, "withdrawn", outcome.reason())?;
    let task = ask["task_id"].as_str().unwrap_or_default();
    if !task.is_empty() && !crate::scheduling::scheduled(store, task)? {
        let owner = ask["from"].as_str().unwrap_or_default();
        crate::mail::record_crew(
            store.connection(),
            &json!({"id": format!("block-cleared-{}", hex(id)), "to": owner, "kind": "notice",
                "task_id": task, "urgency": "normal",
                "body": format!("{} Task {task}.", ended_text(ask, outcome)),
                "payload": {"about": id, "block_cleared": true, "granted": outcome.granted()}}),
        )?;
    }
    Ok(())
}

/// What a block ask that a policy change ended means for the ship that
/// asked, in plain words: carry on only when the policy grants what it
/// asked for; otherwise the runtime cannot tell, so retry once (after a
/// relaunch, when its ship's authorizations changed) and ask again if
/// denied again.
fn ended_text(ask: &Value, outcome: Outcome) -> String {
    let id = ask["id"].as_str().unwrap_or_default();
    let block = &ask["payload"]["block"];
    let rules = rules_text(block);
    match outcome {
        Outcome::Granted => format!(
            "The fleet policy now allows what {id} asked for ({rules}); the captain's decision is no longer needed. Carry on."
        ),
        Outcome::Reauthorized => {
            let ship = block["ship"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("<ship>");
            format!(
                "{id} ended because your ship's standing authorizations changed, but the runtime can't tell whether they cover this request ({rules}): nothing is known to be granted. Auto mode reads them only at launch, so first ask for a relaunch (`privateer host relaunch {ship}`). Retry the step once after it; if it is denied again, report it with `permissions.blocked` so the captain gets a new ask."
            )
        }
        Outcome::Ungranted => format!(
            "{id} ended because the fleet policy changed, but the runtime can't tell whether the new policy covers this ({rules}): nothing was granted for this request. Retry the step once; if it is denied again, report it with `permissions.blocked` so the captain gets a new ask."
        ),
    }
}

/// Why a task parked on a block ask resumes, when a policy change ended the
/// ask or its own session got past it; None for any other ending.
pub(crate) fn resumed_by_policy(request: &Value) -> Option<String> {
    if request["state"] != "withdrawn" || request["payload"]["type"] != "tool_permission" {
        return None;
    }
    let reason = request["end_reason"].as_str()?;
    if reason.starts_with(PASSED) {
        let id = request["id"].as_str().unwrap_or_default();
        return Some(format!("{id} was withdrawn because {reason}. Carry on."));
    }
    Outcome::from_reason(reason).map(|outcome| ended_text(request, outcome))
}

/// Withdraw an open block ask its own session got past, with `reason`
/// (starting with `PASSED`), when `still` says, in the same transaction,
/// that nothing changed since the caller read it. Another ship's ask
/// attached to it is detached, so the captain sees it as its own card: that
/// ship may still be blocked. Whether it was withdrawn.
pub(crate) fn withdraw_passed(
    store: &Store,
    ask: &Value,
    reason: &str,
    still: impl FnOnce() -> Result<bool>,
) -> Result<bool> {
    let id = ask["id"].as_str().context("ask id")?;
    let ship = ask["from"].as_str().unwrap_or_default();
    let tx = store.write_transaction()?;
    let open = crate::mail::find(store, id)?.is_some_and(|ask| ask["state"] == "open") && still()?;
    if open {
        detach_passed(store, id, ship, reason)?;
        crate::mail::end_request(store, ask, "withdrawn", reason)?;
        store.audit(
            "permissions.passed",
            &json!({"ask": id, "ship": ship, "task_id": ask["task_id"], "reason": reason}),
        )?;
    }
    tx.commit()?;
    if open {
        detach_passed_elsewhere(store, id, ship, reason)?;
    }
    Ok(open)
}

/// Detach the open asks other ships raised that are attached to `primary`,
/// an ask its own session got past, in this home and the caller's
/// transaction.
fn detach_passed(store: &Store, primary: &str, ship: &str, reason: &str) -> Result<()> {
    let detached = store.connection().execute(
        &format!(
            "UPDATE mail SET payload=json_remove(payload,'$.attached_to','$.attached_ship','$.about')
             WHERE {OPEN_ASK} AND COALESCE(delivery,'') <> 'received'
             AND json_extract(payload,'$.attached_to')=?1 AND sender<>?2"
        ),
        params![primary, ship],
    )?;
    if detached > 0 {
        store.audit(
            "permissions.detached",
            &json!({"from": primary, "reason": reason, "asks": detached}),
        )?;
    }
    Ok(())
}

/// `detach_passed` in every other fleet home on this machine. A home that
/// cannot be opened now detaches its own when its sweep finds the ask ended
/// (`end_orphans`).
fn detach_passed_elsewhere(store: &Store, primary: &str, ship: &str, reason: &str) -> Result<()> {
    for (_, home) in other_homes(store.home()) {
        let Ok(other) = Store::open(&home) else {
            continue;
        };
        let tx = other.write_transaction()?;
        detach_passed(&other, primary, ship, reason)?;
        tx.commit()?;
    }
    Ok(())
}

/// Before a primary that a policy change ended takes its attachments with
/// it, settle each other ship's block ask attached to it on its own: one the
/// policy covers in its own home ends there as that home's sweep would end
/// it; one it does not cover is detached, so the captain sees it as its own
/// card instead of losing it to a change made for another ship.
fn release_attachments(store: &Store, primary: &str, primary_ship: &str) -> Result<()> {
    let mut homes = vec![store.home().to_path_buf()];
    homes.extend(other_homes(store.home()).into_iter().map(|(_, h)| h));
    for home in homes {
        let Ok(other) = Store::open(&home) else {
            continue;
        };
        let policy = crate::permissions::for_store(&other)?;
        let policy_hash = crate::permissions::policy_hash(&other)?;
        let tx = other.write_transaction()?;
        let rows: Vec<String> = {
            let mut statement = other.connection().prepare(&format!(
                "SELECT id FROM mail WHERE {OPEN_ASK} AND COALESCE(delivery,'') <> 'received'
                 AND json_extract(payload,'$.attached_to')=?1 AND sender<>?2"
            ))?;
            statement
                .query_map(params![primary, primary_ship], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for row in rows {
            let Some(ask) = crate::mail::find(&other, &row)? else {
                continue;
            };
            if covered(&policy, &policy_hash, &ask["payload"]) {
                end_by_policy(&other, &policy, &ask)?;
            } else {
                other.connection().execute(
                    "UPDATE mail SET payload=json_remove(payload,'$.attached_to','$.attached_ship','$.about')
                     WHERE id=?1 AND state='open'",
                    [&row],
                )?;
                other.audit(
                    "permissions.detached",
                    &json!({"ask": row, "from": primary, "reason": POLICY_CHANGED}),
                )?;
            }
        }
        tx.commit()?;
    }
    Ok(())
}

/// End the items in this home still attached to an ask that ended without
/// an answer (a task closed, a withdrawal elsewhere). An answered ask's
/// items are answered by its fan-out, so they are left to it.
fn end_orphans(store: &Store) -> Result<()> {
    let rows: Vec<(String, String)> = {
        let mut statement = store.connection().prepare(
            "SELECT id, json_extract(payload,'$.attached_to') FROM mail WHERE recipient='captain'
             AND state='open' AND kind IN ('request','decision') AND COALESCE(delivery,'') <> 'received'
             AND json_extract(payload,'$.attached_to') IS NOT NULL",
        )?;
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut primaries = std::collections::BTreeSet::new();
    for (_, primary) in rows {
        if let Some(ask) = find_ask(store, &primary)?
            && !matches!(ask["state"].as_str(), Some("open" | "answered"))
            && primaries.insert(primary.clone())
        {
            let reason = ask["end_reason"]
                .as_str()
                .filter(|r| !r.is_empty())
                .or(ask["state"].as_str())
                .unwrap_or("closed")
                .to_owned();
            // A policy change ended it, maybe in another home between its
            // commit and that home's release: other ships' asks go first.
            let ship = ask["from"].as_str().unwrap_or_default();
            if Outcome::from_reason(&reason).is_some() {
                release_attachments(store, &primary, ship)?;
            } else if reason.starts_with(PASSED) {
                // Its own session got past it, maybe while this home could
                // not be opened: other ships' asks here may still be blocked.
                let tx = store.write_transaction()?;
                detach_passed(store, &primary, ship, &reason)?;
                tx.commit()?;
                detach_passed_elsewhere(store, &primary, ship, &reason)?;
            }
            crate::mail::end_attachments(store, &primary, &reason)?;
        }
    }
    Ok(())
}

/// Sweep every fleet home on this machine after a policy change.
pub(crate) fn sweep_fleet() -> Vec<String> {
    let mut ended = vec![];
    let homes: Vec<PathBuf> = match crate::fleet::registry() {
        Ok(conn) => crate::fleet::members(&conn)
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.home.is_dir())
            .map(|m| m.home)
            .collect(),
        Err(_) => vec![],
    };
    for home in homes {
        if let Ok(store) = Store::open(&home)
            && let Ok(mut done) = sweep(&store)
        {
            ended.append(&mut done);
        }
    }
    ended
}

/// Whether an open block ask pauses its whole ship: a first mate's own
/// denial, or one from before asks were scoped, that the policy does not
/// yet cover. A crew's or an errand's ask holds only its task.
pub(crate) fn pausing_ask(store: &Store, ship: &str) -> Result<Option<(i64, Option<String>)>> {
    let policy = crate::permissions::for_store(store)?;
    let policy_hash = crate::permissions::policy_hash(store)?;
    let mut statement = store.connection().prepare(&format!(
        "SELECT seq, task_id, payload FROM mail WHERE sender=?1 AND {OPEN_ASK}
         AND COALESCE(json_extract(payload,'$.block.scope'),'wake')='wake' ORDER BY seq DESC"
    ))?;
    let rows = statement
        .query_map([ship], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (seq, task, payload) in rows {
        let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        if !covered(&policy, &policy_hash, &payload) {
            return Ok(Some((seq, task)));
        }
    }
    Ok(None)
}

/// The open block ask an ended job raised or re-hit: the ask its own denial
/// line names, by the job or by the task it worked on.
pub(crate) fn ask_for_job(store: &Store, job_id: &str, log: &Path) -> Result<Option<Value>> {
    if logged_denials(log).is_empty() {
        return Ok(None);
    }
    let task: Option<(String, String)> = store
        .connection()
        .query_row(
            "SELECT t.owner, t.id FROM jobs j JOIN tasks t
               ON t.id = json_extract(j.record,'$.task_id') WHERE j.id=?1",
            [job_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match task {
        Some((ship, task)) => open_for_task(store, &ship, &task),
        None => Ok(None),
    }
}

/// Bearings lines for a ship's open block asks.
pub(crate) fn bearings_lines(store: &Store, ship: &str) -> Result<Vec<String>> {
    let mut statement = store.connection().prepare(&format!(
        "SELECT id, task_id, payload FROM mail WHERE sender=?1 AND {OPEN_ASK} ORDER BY seq LIMIT 10"
    ))?;
    let rows = statement
        .query_map([ship], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(id, task, payload)| {
            let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            format!(
                "Waiting on captain decision {id} ({}) for {}; do not ask the captain again.",
                rules_text(&payload["block"]),
                task.as_deref().unwrap_or("this ship")
            )
        })
        .collect())
}

/// An open crew block ask on a task, which `agent.start` refuses to launch
/// over unless the launch replaces it.
pub(crate) fn crew_block(store: &Store, task: &str) -> Result<Option<(String, String)>> {
    let row: Option<(String, String)> = store
        .connection()
        .query_row(
            &format!(
                "SELECT id, payload FROM mail WHERE task_id=?1 AND {OPEN_ASK}
              AND json_extract(payload,'$.block.scope')='crew' ORDER BY seq DESC LIMIT 1"
            ),
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(id, payload)| {
        let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        (id, rules_text(&payload["block"]))
    }))
}

/// Correlation hops followed to find a message's topic.
const MAX_HOPS: usize = 8;

/// Whether a stored message is a block ask itself.
fn is_ask(message: &Value) -> bool {
    message["payload"]["type"] == "tool_permission"
        && matches!(message["kind"].as_str(), Some("request" | "decision"))
        && message["to"] == CAPTAIN
}

/// Whether a message in this home is a block ask (or its desk copy).
pub(crate) fn is_ask_id(store: &Store, id: &str) -> bool {
    crate::mail::find(store, id)
        .ok()
        .flatten()
        .is_some_and(|m| is_ask(&m))
}

/// A block ask by id, in this home or another fleet home on this machine.
fn find_ask(store: &Store, id: &str) -> Result<Option<Value>> {
    if let Some(found) = crate::mail::find(store, id)?.filter(is_ask) {
        return Ok(Some(found));
    }
    for (_, home) in other_homes(store.home()) {
        if let Ok(other) = Store::open_readonly(&home, crate::store::Caller::Operator)
            && let Ok(Some(found)) = crate::mail::find(&other, id)
            && is_ask(&found)
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// The open ask a topic resolves to, and the ship that raised it: an ask
/// attached to another resolves to that one.
pub(crate) fn open_primary(store: &Store, topic: &str) -> Result<Option<(String, String)>> {
    let mut id = topic.to_owned();
    for _ in 0..2 {
        let Some(ask) = find_ask(store, &id)? else {
            return Ok(None);
        };
        if ask["state"] != "open" {
            return Ok(None);
        }
        match ask["payload"]["attached_to"].as_str() {
            Some(primary) if primary != id => id = primary.to_owned(),
            _ => {
                return Ok(Some((
                    id,
                    ask["payload"]["block"]["ship"]
                        .as_str()
                        .or(ask["from"].as_str())
                        .unwrap_or_default()
                        .to_owned(),
                )));
            }
        }
    }
    Ok(None)
}

/// The rules an ask lists, for a sender's note.
pub(crate) fn rules_of(store: &Store, ask: &str) -> Option<String> {
    find_ask(store, ask)
        .ok()
        .flatten()
        .map(|ask| rules_text(&ask["payload"]["block"]))
}

/// An open primary for a delivered block ask's denial, at the desk.
pub(crate) fn fleet_key_primary(
    store: &Store,
    from: &str,
    payload: &Value,
) -> Result<Option<String>> {
    // A copy from before asks with no derived rule stopped joining carries
    // the same key as before: its denials decide.
    if payload["type"] != "tool_permission" || !may_join(payload) {
        return Ok(None);
    }
    let Some(key) = payload["block"]["fleet_key"].as_str() else {
        return Ok(None);
    };
    Ok(same_denial(store, from, key)?.map(|(id, _)| id))
}

/// The captain decision a message is about, resolved by the runtime in the
/// sender's home: an explicit `about`, then the correlation chain, then
/// (for notices only) the task and its parents.
pub(crate) fn topic(
    store: &Store,
    about: Option<&str>,
    correlation: Option<&str>,
    task_id: Option<&str>,
    kind: &str,
) -> Result<Option<String>> {
    if let Some(about) = about {
        let known = find_ask(store, about)?.is_some()
            || store.connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM mail WHERE json_extract(payload,'$.about')=?1)",
                [about],
                |r| r.get::<_, bool>(0),
            )?;
        anyhow::ensure!(known, "about names no captain decision known here: {about}");
        return Ok(Some(about.to_owned()));
    }
    let mut hop = correlation.map(str::to_owned);
    for _ in 0..MAX_HOPS {
        let Some(id) = hop else { break };
        let Some(message) = crate::mail::find(store, &id)? else {
            break;
        };
        if is_ask(&message) && message["payload"]["block"].is_object() {
            return Ok(Some(id));
        }
        if let Some(about) = message["payload"]["about"].as_str() {
            return Ok(Some(about.to_owned()));
        }
        hop = message["correlation"].as_str().map(str::to_owned);
    }
    // A request or decision on the blocked task may be another question;
    // only a notice is taken to be about the block by its task alone.
    if kind == "notice"
        && let Some(task) = task_id
    {
        let open: Option<String> = store
            .connection()
            .query_row(
                &format!(
                    "WITH RECURSIVE chain(id, depth) AS (SELECT ?1, 0 UNION
                       SELECT t.parent, chain.depth + 1 FROM tasks t JOIN chain ON t.id = chain.id
                       WHERE t.parent IS NOT NULL AND chain.depth < 16)
                     SELECT id FROM mail WHERE task_id IN (SELECT id FROM chain) AND {OPEN_ASK}
                     ORDER BY seq LIMIT 1"
                ),
                [task],
                |r| r.get(0),
            )
            .optional()?;
        return Ok(open);
    }
    Ok(None)
}

/// Refuse an `allow` or `authorize` answer that is not the operator's own: a
/// relay, a bridge or an agent must never widen fleet permissions.
pub(crate) fn check_answer(thread: &Value, payload: &Value) -> Result<()> {
    let widens = matches!(
        payload["answer"]["option_id"].as_str(),
        Some("allow" | "authorize")
    ) || approves(payload);
    if !is_ask(thread) || !widens {
        return Ok(());
    }
    if payload["operator"] == true {
        // The wording is checked before the reply commits, so a refusal is
        // the captain's to see, not a failure after "answered".
        if payload["answer"]["option_id"] == "authorize" {
            wording(thread, payload)?;
        }
        return Ok(());
    }
    let rules = rules_text(&thread["payload"]["block"]);
    if approves(payload) {
        anyhow::bail!(
            "An approval must come from the captain directly; ask the captain to tap it on the card ({rules})"
        )
    }
    if payload["answer"]["option_id"] == "authorize" {
        anyhow::bail!(
            "Authorize must come from the captain directly; ask the captain to tap Authorize or run `privateer permissions authorize SHIP 'TEXT'` ({rules})"
        )
    }
    anyhow::bail!(
        "Allow must come from the captain directly; ask the captain to tap Allow or run `privateer permissions allow '<rule>'` ({rules})"
    )
}

/// Whether an answer chooses a captain approval: `approve` or one of the
/// `as_approved_N` options.
fn approves(payload: &Value) -> bool {
    payload["answer"]["option_id"]
        .as_str()
        .is_some_and(|id| id == "approve" || id.starts_with("as_approved_"))
}

/// The authorization an `authorize` answer records: the captain's own
/// wording, when the answer carries one, else the ask's suggestion. Either is
/// one line of at most 500 bytes, as the fleet policy keeps it.
fn wording(ask: &Value, payload: &Value) -> Result<String> {
    let option_label = ask["payload"]["decision"]["options"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o["id"] == "authorize")
        .and_then(|o| o["label"].as_str());
    let text = payload["answer"]["text"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty() && Some(*t) != option_label)
        .or(ask["payload"]["block"]["authorize"].as_str())
        .context("the ask offers no authorization; answer with the wording to record")?;
    anyhow::ensure!(
        text.len() <= 500 && !text.chars().any(char::is_control),
        "an authorization is one line of at most 500 characters; shorten the wording or run `privateer permissions authorize SHIP 'TEXT'`"
    );
    Ok(text.to_owned())
}

/// Apply the captain's answer to a block ask that lives in this home, then,
/// where the captain wrote it (`fan`), send it on to the asks and requests
/// attached to it. A delivered answer is only applied: the desk that wrote it
/// fans it out, so no attached item is answered twice. Called after the
/// reply commits; a failure is logged, never undoes the reply.
pub(crate) fn answered(store: &Store, request: &str, reply: &Value, fan: bool) {
    let result = (|| -> Result<()> {
        let Some(ask) = crate::mail::find(store, request)?.filter(is_ask) else {
            return Ok(());
        };
        // A desk copy is answered at the desk; the ask's own home applies it.
        if ask["delivery"] != "received" && ask["payload"]["block"].is_object() {
            apply(store, &ask, reply)?;
        }
        if fan && reply["payload"].get("fanned_from").is_none() {
            fan_out(store, &ask, reply)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!(
            "privateer: the captain's answer to {request} was recorded but not fully applied: {error:#}"
        );
        let _ = store.audit(
            "permissions.answer_failed",
            &json!({"request": request, "error": format!("{error:#}")}),
        );
    }
}

fn apply(store: &Store, ask: &Value, reply: &Value) -> Result<()> {
    let block = &ask["payload"]["block"];
    let ship = ask["from"].as_str().unwrap_or_default();
    let task = ask["task_id"].as_str().unwrap_or_default();
    let records = block["denials"].as_array().cloned().unwrap_or_default();
    match reply["payload"]["answer"]["option_id"].as_str() {
        Some("allow") if reply["payload"]["operator"] == true => {
            let rules: Vec<&str> = records.iter().filter_map(|d| d["rule"].as_str()).collect();
            for rule in &rules {
                crate::permissions::set(
                    &crate::store::Caller::Operator,
                    &json!({"claude_allow_add": rule}),
                )?;
            }
            store.audit(
                "permissions.allowed",
                &json!({"ask": ask["id"], "rules": rules, "reply": reply["id"]}),
            )?;
        }
        Some("authorize") if reply["payload"]["operator"] == true => {
            let text = wording(ask, &reply["payload"])?;
            crate::permissions::set(
                &crate::store::Caller::Operator,
                &json!({"authorize": {"ship": ship, "text": text}}),
            )?;
            store.audit(
                "permissions.authorized",
                &json!({"ask": ask["id"], "ship": ship, "text": text, "reply": reply["id"]}),
            )?;
        }
        Some("without") => {
            let policy_hash = crate::permissions::policy_hash(store)?;
            let tx = store.write_transaction()?;
            for name in names(&records, ship) {
                tx.execute(
                    "INSERT OR REPLACE INTO permission_declined(ship,task_id,rule,policy_hash,ms) VALUES(?1,?2,?3,?4,?5)",
                    params![ship, task, name, policy_hash, Store::now_ms()],
                )?;
            }
            tx.commit()?;
        }
        Some("stop") if !task.is_empty() => {
            let tx = store.write_transaction()?;
            crate::scheduling::hold(store, task, "the captain stopped it at a tool permission")?;
            tx.commit()?;
        }
        _ => {}
    }
    Ok(())
}

/// Send the captain's answer to every open item attached to `ask`, in the
/// home that holds it. Typed decisions with the same options get the same
/// option; others get its text. The operator marker is kept.
fn fan_out(store: &Store, ask: &Value, reply: &Value) -> Result<()> {
    let id = ask["id"].as_str().unwrap_or_default();
    let options = |m: &Value| -> Vec<String> {
        let mut ids: Vec<String> = m["payload"]["decision"]["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| o["id"].as_str().map(str::to_owned))
            .collect();
        ids.sort();
        ids
    };
    let answer = reply["payload"].get("answer").cloned();
    let label = answer
        .as_ref()
        .and_then(|a| a["option_id"].as_str())
        .and_then(|chosen| {
            ask["payload"]["decision"]["options"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|o| o["id"] == chosen)
                .and_then(|o| o["label"].as_str())
        });
    let operator = reply["payload"]["operator"] == true;
    // An authorization or a captain approval is the asking ship's alone.
    let authorizes = answer
        .as_ref()
        .is_some_and(|a| a["option_id"] == "authorize")
        || approves(&reply["payload"]);
    let mut homes = vec![store.home().to_path_buf()];
    homes.extend(other_homes(store.home()).into_iter().map(|(_, h)| h));
    for home in homes {
        let Ok(other) = Store::open(&home) else {
            continue;
        };
        let rows: Vec<String> = {
            let mut statement = other.connection().prepare(
                "SELECT id FROM mail WHERE recipient='captain' AND state='open'
                 AND kind IN ('request','decision') AND json_extract(payload,'$.attached_to')=?1",
            )?;
            statement
                .query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for row in rows {
            let Some(attached) = crate::mail::find(&other, &row)? else {
                continue;
            };
            // Another ship's ask is detached, so the captain sees it on its
            // own.
            if authorizes && is_ask(&attached) && attached["from"] != ask["from"] {
                other.connection().execute(
                    "UPDATE mail SET payload=json_remove(payload,'$.attached_to','$.attached_ship','$.about')
                     WHERE id=?1 AND state='open'",
                    [&row],
                )?;
                other.audit(
                    "permissions.detached",
                    &json!({"ask": row, "from": id, "reply": reply["id"]}),
                )?;
                continue;
            }
            let mut payload = json!({"fanned_from": reply["id"]});
            if let Some(answer) = &answer
                && attached["kind"] == "decision"
                && options(&attached) == options(ask)
            {
                payload["answer"] = answer.clone();
            }
            let body = match (label, reply["body"].as_str()) {
                (Some(label), Some(body)) if body != label => format!("{label}: {body}"),
                (Some(label), _) => label.to_owned(),
                (None, Some(body)) => body.to_owned(),
                (None, None) => "Answered".to_owned(),
            };
            let sent = crate::mail::send_copy(
                &other,
                &json!({"from": CAPTAIN, "to": attached["from"], "kind": "reply",
                    "correlation": row, "body": body, "payload": payload}),
                operator,
            );
            if let Err(error) = sent {
                eprintln!("privateer: could not pass the captain's answer to {row}: {error:#}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(command: &str) -> Option<String> {
        narrow_rule("Bash", &json!({"command": command}), None)
    }

    #[test]
    fn a_bash_rule_is_the_command_and_its_subcommand_only() {
        assert_eq!(
            bash("apify run actor/x --input a").as_deref(),
            Some("Bash(apify run *)")
        );
        assert_eq!(
            bash("curl -sS https://example.com/ -o /dev/null").as_deref(),
            Some("Bash(curl *)")
        );
        assert_eq!(bash("make ./target").as_deref(), Some("Bash(make *)"));
        assert_eq!(bash("ls").as_deref(), Some("Bash(ls *)"));
        // A key in the command never reaches the unmasked rule.
        assert_eq!(
            bash("some-cli sk-proj-abc123def456ghi789jkl012 --x").as_deref(),
            Some("Bash(some-cli *)")
        );
        assert_eq!(bash("ghp_abcdefghij1234567890klmn status"), None);
        for compound in [
            "cd x && apify run",
            "echo $HOME",
            "a | b",
            "FOO=1 apify run",
            "rm -rf *",
            "echo 'x'",
            "",
        ] {
            assert_eq!(bash(compound), None, "{compound}");
        }
    }

    #[test]
    fn file_rules_are_relative_inside_the_folder_and_absolute_outside() {
        let base = Path::new("/ships/web");
        let rule =
            |tool: &str, path: &str| narrow_rule(tool, &json!({"file_path": path}), Some(base));
        assert_eq!(
            rule("Edit", "/ships/web/src/a.rs").as_deref(),
            Some("Edit(./src/a.rs)")
        );
        assert_eq!(
            rule("Write", "/etc/hosts").as_deref(),
            Some("Write(//etc/hosts)")
        );
        assert_eq!(rule("Read", "/ships/web/../x"), None);
        assert_eq!(
            narrow_rule(
                "WebFetch",
                &json!({"url": "https://u:p@Docs.Example.com:8443/a?b"}),
                None
            )
            .as_deref(),
            Some("WebFetch(domain:docs.example.com)")
        );
        assert_eq!(
            narrow_rule("mcp__apify__run", &json!({}), None).as_deref(),
            Some("mcp__apify__run")
        );
        assert_eq!(narrow_rule("Bash", &json!({}), None), None);
        assert_eq!(narrow_rule("Edit", &json!({}), None), None);
    }

    #[test]
    fn previews_mask_secret_shaped_values_and_stay_short() {
        let planted = "deadbeef0123456789abcdef0123456789abcdef";
        let shown = preview(
            "Bash",
            &json!({"command": format!(
                "curl -H 'Authorization: Bearer {planted}' https://bob:hunter2@x.io API_KEY={planted} --token abc"
            )}),
        );
        assert!(!shown.contains(planted), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("abc"), "{shown}");
        assert!(shown.starts_with("curl -H"), "{shown}");
        assert!(shown.contains("API_KEY=•••"), "{shown}");
        let short = preview(
            "Bash",
            &json!({"command": "curl -H 'Authorization: Bearer abc12' x.io"}),
        );
        assert_eq!(short, "curl -H 'Authorization: Bearer ••• x.io");
        let tool = preview(
            "mcp__apify__run",
            &json!({"actor": "x", "apiKey": "short1", "headers": {"cookie": "c=1"}}),
        );
        assert!(!tool.contains("short1") && !tool.contains("c=1"), "{tool}");
        assert!(tool.contains(r#""actor":"x""#), "{tool}");
        assert_eq!(
            preview("NotebookEdit", &json!({"notebook_path": "/w/a.ipynb"})),
            "/w/a.ipynb"
        );
        // A path with a digit stays readable, and only a random or
        // token-shaped part of it is hidden.
        assert_eq!(
            preview(
                "Edit",
                &json!({"file_path": "/home/me/ships/app2/scratch/plan.json"})
            ),
            "/home/me/ships/app2/scratch/plan.json"
        );
        assert_eq!(
            preview(
                "Read",
                &json!({"file_path": format!("/var/folders/zz/zyxvpxvq6csfxvn_n0000gn/T/{planted}/a.rs")})
            ),
            "/var/folders/zz/•••/T/•••/a.rs"
        );
        let base64 = format!("cat /{planted}/x+y==");
        assert_eq!(preview("Bash", &json!({"command": base64})), "cat •••");
        let long = preview("Bash", &json!({"command": "echo ".repeat(100)}));
        assert_eq!(long.chars().count(), PREVIEW_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn records_keep_one_per_rule_and_count_the_rest() {
        let denial =
            |command: &str| json!({"tool_name": "Bash", "tool_input": {"command": command}});
        let denials: Vec<Value> = ["a x", "a x 2", "b", "c", "d", "e", "f | g"]
            .iter()
            .map(|c| denial(c))
            .collect();
        let (records, more) = records(&denials, None);
        assert_eq!(records.len(), MAX_DENIALS);
        assert_eq!(more, 2);
        assert_eq!(records[0]["rule"], "Bash(a x *)");
    }

    #[test]
    fn a_script_or_classifier_denial_offers_an_authorization_not_a_rule() {
        let denial = |command: &str, reason: Option<&str>, classifier: bool| {
            json!({"tool_name": "Bash", "tool_input": {"command": command},
                "reason": reason, "classifier": classifier})
        };
        let (found, _) = records(
            &[
                denial(
                    "cd /srv && SECRET_KEY=abc ./deploy.sh | tee log",
                    None,
                    false,
                ),
                // A simple command the classifier refused is still no rule.
                denial(
                    "kubectl delete pod web-1",
                    Some("[Interfere With Workloads]"),
                    true,
                ),
                denial(
                    "cargo publish --dry-run",
                    Some("This command requires approval"),
                    false,
                ),
            ],
            None,
        );
        assert!(found[0]["rule"].is_null());
        assert_eq!(
            found[0]["authorize"],
            "Running cd, deploy.sh and tee commands its assigned work needs, including multi-step ones."
        );
        assert!(found[1]["rule"].is_null());
        assert_eq!(
            found[1]["authorize"],
            "Running kubectl commands its assigned work needs, even when the auto-mode check flags them as Interfere With Workloads."
        );
        assert_eq!(found[2]["rule"], "Bash(cargo publish *)");
        assert!(found[2].get("authorize").is_none());
        assert_eq!(found[2]["reason"], "This command requires approval");
        let text = rules_text(&json!({"denials": found}));
        assert_eq!(
            text,
            "Bash (no narrower rule), Bash ([Interfere With Workloads]), Bash(cargo publish *)"
        );
        // A key in the command reaches neither the preview nor the suggestion.
        let (keyed, _) = records(
            &[denial(
                "deadbeef0123456789abcdef0123456789 | sh",
                Some("[Credential Leakage]"),
                true,
            )],
            None,
        );
        assert!(
            !keyed[0].to_string().contains("deadbeef0123"),
            "{}",
            keyed[0]
        );
        assert_eq!(
            keyed[0]["authorize"],
            "Running sh commands its assigned work needs, even when the auto-mode check flags them as Credential Leakage."
        );
    }

    #[test]
    fn a_heredoc_or_redirection_never_reaches_the_suggestion_or_preview() {
        let (found, _) = records(
            &[json!({"tool_name": "Bash", "classifier": true,
                "reason": "[Credential Materialization]",
                "tool_input": {"command": "docker login --password-stdin <<EOF\nS3cr3tPass\nEOF"}})],
            None,
        );
        assert!(!found[0].to_string().contains("S3cr3tPass"), "{}", found[0]);
        assert_eq!(found[0]["preview"], "docker login --password-stdin <<…");
        assert_eq!(
            found[0]["authorize"],
            "Running docker commands its assigned work needs, even when the auto-mode check flags them as Credential Materialization."
        );
        assert_eq!(
            programs("make deploy >deploy.log 2>&1 && curl -s x > /tmp/out; wc -l < /tmp/out"),
            ["make", "curl", "wc"]
        );
        assert_eq!(
            programs("make 2>&1|tee log; ls >|out&&pwd"),
            ["make", "tee", "ls", "pwd"]
        );
        assert_eq!(programs("cat <<<\"$TOKEN\" | login"), ["cat"]);
        assert_eq!(programs("echo one\nsecret-program two"), ["echo"]);
    }

    #[test]
    fn an_unidentified_denial_names_no_tool_and_no_rule() {
        let (unread, _) = records(&[unidentified()], None);
        assert_eq!(unread[0]["tool"], "");
        assert!(unread[0]["rule"].is_null());
        assert_eq!(unread[0]["preview"], UNIDENTIFIED);
        assert_eq!(
            rules_text(&json!({"denials": unread})),
            "a tool (denied tool not identified)"
        );
        // A ship that reports the tool as unknown gets no rule for it either.
        assert_eq!(narrow_rule("unknown", &json!({}), None), None);
    }

    #[test]
    fn only_the_operator_may_answer_allow_or_authorize() {
        let ask = json!({"kind": "decision", "to": CAPTAIN,
            "payload": {"type": "tool_permission",
                "block": {"denials": [], "authorize": "Running make commands."}}});
        for option in [
            "allow",
            "authorize",
            "approve",
            "as_approved_1",
            "as_approved_2",
        ] {
            let relayed = json!({"answer": {"option_id": option}, "relayed_by": "liaison"});
            assert!(check_answer(&ask, &relayed).is_err(), "{option}");
            let own = json!({"answer": {"option_id": option}, "operator": true});
            assert!(check_answer(&ask, &own).is_ok(), "{option}");
        }
        let without = json!({"answer": {"option_id": "without"}});
        assert!(check_answer(&ask, &without).is_ok());
    }

    #[test]
    fn an_authorize_ask_is_covered_by_its_own_ships_authorizations_only() {
        let before = json!({"authorized": {"storefront": ["Running deploys."]}});
        let ask = json!({"policy_hash": "old", "block": {"ship": "liaison",
            "authorized": authorizations_hash(&before, "liaison"),
            "denials": [{"tool": "Bash", "rule": null, "authorize": "Running make commands."}]}});
        let other = json!({"authorized": {"storefront": ["Running deploys.", "More."]}});
        assert!(!covered(&other, "new", &ask));
        // Its own ship's change ends it, but other wording may not cover
        // it: the runtime cannot tell, so it grants nothing.
        let own = json!({"authorized": {"storefront": ["Running deploys."],
            "liaison": ["Running make for deploys."]}});
        assert!(covered(&own, "new", &ask));
        assert!(!grants(&own, &ask));
        assert_eq!(Outcome::of(&own, &ask), Outcome::Reauthorized);
        let yolo = json!({"mode": "yolo"});
        assert!(covered(&yolo, "new", &ask));
        assert_eq!(Outcome::of(&yolo, &ask), Outcome::Granted);
        // A derived rule it also names must be allowed as well.
        let mut mixed = ask.clone();
        mixed["block"]["denials"]
            .as_array_mut()
            .unwrap()
            .push(json!({"tool": "Bash", "rule": "Bash(make *)"}));
        assert!(!covered(&own, "old", &mixed));
        let mut allowing = own.clone();
        allowing["claude_allow"] = json!(["Bash(make *)"]);
        assert_eq!(Outcome::of(&allowing, &mixed), Outcome::Reauthorized);
        // Without the hash (a crew's ask, or an older one) any change ends it.
        let mut older = ask.clone();
        older["block"].as_object_mut().unwrap().remove("authorized");
        assert!(covered(&other, "new", &older));
        assert_eq!(Outcome::of(&other, &older), Outcome::Ungranted);
    }

    #[test]
    fn only_asks_whose_every_denial_is_a_derived_rule_join_another_ships() {
        let ask = |denials: Value, more: u64| json!({"type": "tool_permission", "block": {"denials": denials, "more": more}});
        assert!(may_join(&ask(
            json!([{"tool": "Bash", "rule": "Bash(kubectl apply *)"}]),
            0
        )));
        for refused in [
            ask(json!([{"tool": "Bash", "rule": null}]), 0),
            ask(json!([{"tool": "unknown", "rule": null}]), 0),
            ask(json!([{"tool": "", "rule": null, "unidentified": true}]), 0),
            ask(
                json!([{"tool": "Bash", "rule": null, "authorize": "Running make."}]),
                0,
            ),
            ask(
                json!([{"tool": "Bash", "rule": "Bash(make *)"}, {"tool": "Bash", "rule": null}]),
                0,
            ),
            // What it counted but does not list may name no rule.
            ask(json!([{"tool": "Bash", "rule": "Bash(make *)"}]), 1),
            ask(json!([]), 0),
            json!({"type": "tool_permission", "block": {}}),
        ] {
            assert!(!may_join(&refused), "{refused}");
        }
        // Other captain mail joins by its topic as before.
        assert!(may_join(&json!({"about": "permissions-x"})));
    }

    #[test]
    fn a_policy_change_ends_a_no_rule_ask_without_granting_it() {
        let policy = json!({"claude_allow": ["Bash(apify run *)"]});
        let ask = |denials: Value| json!({"policy_hash": "old", "block": {"denials": denials}});
        let derived = ask(json!([{"tool": "Bash", "rule": "Bash(apify run *)"}]));
        assert!(grants(&policy, &derived) && covered(&policy, "new", &derived));
        let other = ask(json!([{"tool": "Bash", "rule": "Bash(make *)"}]));
        assert!(!grants(&policy, &other) && !covered(&policy, "new", &other));
        let none = ask(json!([{"tool": "unknown", "rule": null}]));
        assert!(!grants(&policy, &none));
        assert!(covered(&policy, "new", &none));
        assert!(!covered(&policy, "old", &none));
        assert!(grants(&json!({"mode": "yolo"}), &none));
        let ended = |reason: &str| {
            json!({"id": "permissions-1", "state": "withdrawn", "end_reason": reason,
                "payload": {"type": "tool_permission", "block": {"denials": [{"tool": "unknown", "rule": null}]}}})
        };
        let ungranted = resumed_by_policy(&ended(POLICY_CHANGED_UNGRANTED)).unwrap();
        assert!(ungranted.contains("nothing was granted"), "{ungranted}");
        assert!(
            ungranted.contains("can't tell whether the new policy covers this"),
            "{ungranted}"
        );
        assert!(!ungranted.contains("does not cover"), "{ungranted}");
        assert!(ungranted.contains("permissions.blocked"), "{ungranted}");
        let granted = resumed_by_policy(&ended(POLICY_CHANGED)).unwrap();
        assert!(granted.contains("now allows"), "{granted}");
        // A ship whose authorizations changed resumes too, told to relaunch.
        let mut reauthorized = ended(POLICY_CHANGED_AUTHORIZATIONS);
        reauthorized["payload"]["block"]["ship"] = json!("art");
        let reauthorized = resumed_by_policy(&reauthorized).unwrap();
        for absent in ["now allows", "Carry on"] {
            assert!(!reauthorized.contains(absent), "{reauthorized}");
        }
        for present in [
            "can't tell",
            "privateer host relaunch art",
            "Retry the step once",
            "permissions.blocked",
        ] {
            assert!(reauthorized.contains(present), "{reauthorized}");
        }
        assert!(
            resumed_by_policy(&ended("merged ask permissions-2 ended: policy changed")).is_none()
        );
    }

    #[test]
    fn the_captains_authorize_wording_is_checked_before_it_is_answered() {
        let ask = json!({"kind": "decision", "to": CAPTAIN,
            "payload": {"type": "tool_permission", "block": {"denials": [],
                "authorize": "Running make commands its assigned work needs."},
                "decision": {"options": [{"id": "authorize",
                    "label": "Authorize: Running make commands its assigned work needs."}]}}});
        let answer = |text: &str| json!({"answer": {"option_id": "authorize", "text": text}, "operator": true});
        for refused in [
            "x".repeat(501),
            "Running make.\nAnd anything else.".to_owned(),
            "Running make\tcommands.".to_owned(),
        ] {
            let error = check_answer(&ask, &answer(&refused)).unwrap_err();
            assert!(error.to_string().contains("one line"), "{error}");
        }
        let own = "Running make for deploys.";
        check_answer(&ask, &answer(own)).unwrap();
        assert_eq!(wording(&ask, &answer(own)).unwrap(), own);
        // A tap, or a reply that only repeats the label, takes the suggestion.
        for tap in [
            "",
            "Authorize: Running make commands its assigned work needs.",
        ] {
            assert_eq!(
                wording(&ask, &answer(tap)).unwrap(),
                "Running make commands its assigned work needs."
            );
        }
    }

    #[test]
    fn the_denial_line_is_read_from_the_end_of_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("stdout.log");
        assert!(logged_denials(&log).is_empty());
        let line = json!({"type": "native.permission_blocked", "denials": [
            {"tool_name": "Bash", "tool_input": {"command": "curl x"}}, {"bad": 1}]});
        std::fs::write(
            &log,
            format!("{}\n{line}\n{{\"type\":\"other\"}}\n", "x".repeat(200_000)),
        )
        .unwrap();
        let denials = logged_denials(&log);
        assert_eq!(denials.len(), 1);
        assert_eq!(denials[0]["tool_input"]["command"], "curl x");
    }
}
