//! Captain approvals: one step a ship asked the captain to OK, declared on
//! its request as `payload.approves`, recorded from the captain's own reply,
//! and told to Claude's auto-mode classifier at the ship's next first-mate
//! launch.
//!
//! The action is frozen in the request the captain read, and the stored body
//! says what a yes grants. A row is bound to its ship, its task and its
//! expiry, reaches `autoMode.allow` only, and dies with its task. Nothing a
//! ship sends after the request can create, widen, extend or move one.
//!
//! Every ship runs as the same OS user, so this is a workflow control that
//! keeps a confused agent from approving itself, not isolation: an agent that
//! writes the database directly is not stopped here.
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const DEFAULT_TTL_SECS: u64 = 86_400;
pub(crate) const MAX_TTL_SECS: u64 = 604_800;
/// Live approvals one ship may hold, as `tool_permissions.policy` accepts.
pub(crate) const MAX_LIVE: usize = 8;
const MAX_ACTION_CHARS: usize = 300;
const MAX_PREFIX_CHARS: usize = 120;
/// The longest sentence `tool_permissions.policy` accepts.
const MAX_SENTENCE_CHARS: usize = 600;
/// The longest `until` a sentence prints, and the length of the reply id a
/// captain's door mints, for the send-time worst case.
const LONGEST_UTC: &str = "9999-12-31 23:59 UTC";
const MINTED_REPLY_CHARS: usize = 36;

const POLICY_WORDS: &[&str] = &[
    "permission",
    "setting",
    ".claude",
    "charter",
    "automode",
    "allowedtools",
];
const CREDENTIAL_WORDS: &[&str] = &[
    ".ssh", ".aws", ".netrc", "id_rsa", ".env", "keychain", "token", "password",
];
const BROAD_WORDS: &[&str] = &[
    "anything",
    "everything",
    "all commands",
    "any command",
    "whatever",
    "always",
    "from now on",
];
/// Words of the sentence that wraps an action, which the action cannot restate.
const FRAMING_WORDS: &[&str] = &["authorized", "authorised", "captain", "approved", "expire"];
/// Commands that run whatever follows them.
const WRAPPERS: &[&str] = &[
    "env", "sudo", "xargs", "eval", "doas", "su", "exec", "command", "builtin", "nohup", "timeout",
    "nice", "time", "watch", "ssh",
];
/// Characters that make a shell line more than one simple command.
const METACHARACTERS: &str = ";&|<>()$`\\";
/// The most words and characters of a command a prefix is matched against.
const MAX_HEAD_WORDS: usize = 16;
const MAX_HEAD_CHARS: usize = 200;
/// How recent an approval or a reply must be to act on in one tap.
const ONE_TAP_MS: i64 = 86_400_000;
/// Shells and interpreters, refused alone or running inline code.
const INTERPRETERS: &[&str] = &["bash", "sh", "zsh", "node"];
/// Words that make a free-text reply something other than a plain yes.
const REFUSALS: &[&str] = &[
    "no",
    "not",
    "don't",
    "dont",
    "do not",
    "never",
    "stop",
    "wait",
    "hold",
    "later",
    "cancel",
    "deny",
    "decline",
    "reject",
    "instead",
    "unless",
    "except",
    "but",
    "only",
    "first",
    "if",
    "nope",
    "nah",
    "cannot",
    "can't",
    "won't",
    "until",
    "pause",
    "abort",
    "refuse",
    "maybe",
    "never mind",
];
/// Refusals matched as the start of a word, so "Denied" and "Waiting" count.
const REFUSAL_STEMS: &[&str] = &[
    "deny",
    "deni",
    "reject",
    "declin",
    "cancel",
    "stop",
    "refus",
    "wait",
    "hold",
    "paus",
    "abort",
    "postpon",
    "veto",
    "skip",
    "negative",
    "nevermind",
];
/// Words a plain yes may hold besides the words of the action it answers.
const PLAIN_WORDS: &[&str] = &[
    "yes", "y", "yep", "yeah", "ok", "okay", "k", "sure", "go", "ahead", "do", "it", "please",
    "approve", "approved", "proceed", "lgtm", "ship", "thanks", "thank", "you", "the", "that",
    "this", "fine", "good", "now",
];
/// The words of which a free-text reply needs one, unless it is made only of
/// the action's own words.
const YES_WORDS: &[&str] = &[
    "yes", "y", "yep", "yeah", "ok", "okay", "k", "sure", "go", "approve", "approved", "proceed",
    "lgtm", "ship", "do", "fine", "good",
];
const NOT_PLAIN: &str = "reply was not a plain yes; ask again with a decision";
const PICTURED: &str =
    "reply carries a symbol or emoji, so nothing was approved; ask again with a decision";

/// One line of text within `max` characters, trimmed, with no control
/// characters. Errors name the field, never the value.
fn line(value: &Value, field: &str, max: usize) -> Result<String> {
    let text = value
        .as_str()
        .with_context(|| format!("{field} must be a string"))?
        .trim();
    let len = text.chars().count();
    ensure!(
        (1..=max).contains(&len) && !text.chars().any(char::is_control),
        "{field} must be one line of 1 to {max} characters"
    );
    Ok(text.to_owned())
}

/// Whether `name` stands alone in `text`, not inside a longer word.
fn names(text: &str, name: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    let (text, name) = (text.to_lowercase(), name.to_lowercase());
    text.match_indices(&name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(word)
            && !text[at + name.len()..].chars().next().is_some_and(word)
    })
}

/// The ships this machine knows besides `ship`: the fleet registry's, when
/// one exists, and this home's directory.
fn other_ships(store: &Store, ship: &str) -> Result<Vec<String>> {
    let mut ships: Vec<String> = {
        let mut statement = store.connection().prepare("SELECT name FROM ships")?;
        statement
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    // Read an existing registry only: a send must not create one.
    if crate::fleet::registry_path().is_ok_and(|p| p.is_file())
        && let Ok(conn) = crate::fleet::registry()
    {
        ships.extend(crate::fleet::members(&conn)?.into_iter().map(|m| m.name));
    }
    ships.retain(|name| name != ship);
    ships.sort();
    ships.dedup();
    Ok(ships)
}

/// The coarse send-time lint. The captain's reading is the real check.
pub(crate) fn lint(store: &Store, ship: &str, field: &str, text: &str) -> Result<()> {
    let lower = text.to_lowercase();
    if let Some(other) = other_ships(store, ship)?
        .into_iter()
        .find(|other| names(text, other))
    {
        bail!("{field} names ship {other}; an approval covers the asking ship's own work only");
    }
    ensure!(
        !POLICY_WORDS.iter().any(|w| lower.contains(w)),
        "{field} mentions policy or settings; ask the captain for those with permissions, not an approval"
    );
    ensure!(
        !CREDENTIAL_WORDS.iter().any(|w| lower.contains(w)),
        "{field} mentions credentials; an approval never covers them"
    );
    ensure!(
        !BROAD_WORDS.iter().any(|w| lower.contains(w)),
        "{field} is too broad; name the one step the captain is approving"
    );
    ensure!(
        !FRAMING_WORDS.iter().any(|w| lower.contains(w)),
        "{field} restates the approval's own framing; name the step itself"
    );
    Ok(())
}

pub(crate) fn check_prefix(prefix: &str) -> Result<()> {
    ensure!(
        !prefix.chars().any(|c| METACHARACTERS.contains(c)),
        "approves.command_prefix must be one simple command, without ; & | < > ( ) $ ` or \\"
    );
    let mut words = prefix.split_whitespace();
    let first = words.next().unwrap_or_default();
    ensure!(
        !first.is_empty() && !first.contains('='),
        "approves.command_prefix must start with the program it runs, not an assignment"
    );
    let program = first.rsplit('/').next().unwrap_or(first);
    ensure!(
        !WRAPPERS.contains(&program),
        "approves.command_prefix cannot start with {program}, which runs any command"
    );
    let interpreter = INTERPRETERS.contains(&program) || program.starts_with("python");
    if interpreter {
        let rest: Vec<&str> = words.collect();
        ensure!(
            rest.iter().any(|w| !w.starts_with('-'))
                && !rest
                    .iter()
                    .any(|w| matches!(*w, "-c" | "-e" | "--eval" | "-m")),
            "approves.command_prefix cannot be just a shell or interpreter; name the script it runs"
        );
    }
    Ok(())
}

/// The words of a Bash command a prefix is matched against, when it is one
/// simple command: one line, no shell operator, redirection or expansion, a
/// program first rather than an assignment, at most 16 words and 200
/// characters, and nothing a card would mask or that looks like a key.
pub(crate) fn command_head(command: &str) -> Option<Vec<String>> {
    let simple = !command
        .chars()
        .any(|c| (c.is_control() && c != '\t') || METACHARACTERS.contains(c))
        && command.trim().chars().count() <= MAX_HEAD_CHARS
        && crate::block_ask::mask(command) == command;
    let words: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
    (simple
        && (1..=MAX_HEAD_WORDS).contains(&words.len())
        && !words[0].contains('=')
        && !words.iter().any(|w| crate::block_ask::secret_shaped(w)))
    .then_some(words)
}

/// Whether a command prefix covers a command's head: its words are the
/// head's first words, each exactly, so `make release` covers `make release
/// --dry-run` but never `make releasex` or `sudo make release`.
pub(crate) fn covers(prefix: &str, head: &[String]) -> bool {
    let words: Vec<&str> = prefix.split_whitespace().collect();
    !words.is_empty() && words.len() <= head.len() && words.iter().zip(head).all(|(p, h)| p == h)
}

fn hours(ttl_secs: u64) -> String {
    match ttl_secs {
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

/// Check and normalize `payload.approves` on a message a ship sends to the
/// captain. Returns the line the stored body gains, so every surface shows
/// what a yes grants; None when the message declares nothing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn declare(
    store: &Store,
    id: &str,
    from: &str,
    to: &str,
    kind: &str,
    request_kind: Option<&str>,
    task_id: Option<&str>,
    payload: &mut Value,
) -> Result<Option<String>> {
    let Some(approves) = payload.get("approves").cloned() else {
        return Ok(None);
    };
    ensure!(
        matches!(kind, "request" | "decision"),
        "approves rides a request or a decision"
    );
    ensure!(
        to == crate::mail::CAPTAIN && from != crate::mail::CAPTAIN,
        "approves is a ship's ask to the captain"
    );
    ensure!(
        !crate::mail::is_approval_kind(request_kind),
        "a production approval or privileged command is granted with approval.grant, never through approves"
    );
    ensure!(
        payload["type"] != "tool_permission"
            && ["block", "placement", "needs_hands"]
                .iter()
                .all(|k| payload.get(*k).is_none()),
        "approves cannot ride an ask the runtime raises"
    );
    // A door sends a tapped choice back as plain text, so a ship's own
    // buttons ("Go", "Pass") would each read as the captain's yes.
    ensure!(
        kind != "request" || payload.get("choices").is_none(),
        "use kind decision with an approving option for buttons"
    );
    let fields = approves.as_object().context("approves must be an object")?;
    ensure!(
        fields
            .keys()
            .all(|k| matches!(k.as_str(), "action" | "command_prefix" | "ttl_secs")),
        "approves takes only action, command_prefix and ttl_secs"
    );
    let action = line(
        fields
            .get("action")
            .context("approves.action is required")?,
        "approves.action",
        MAX_ACTION_CHARS,
    )?;
    lint(store, from, "approves.action", &action)?;
    let prefix = match fields.get("command_prefix").filter(|v| !v.is_null()) {
        Some(value) => {
            let prefix = line(value, "approves.command_prefix", MAX_PREFIX_CHARS)?;
            check_prefix(&prefix)?;
            lint(store, from, "approves.command_prefix", &prefix)?;
            Some(prefix)
        }
        None => None,
    };
    let ttl_secs = match fields.get("ttl_secs").filter(|v| !v.is_null()) {
        Some(value) => value
            .as_u64()
            .filter(|s| (1..=MAX_TTL_SECS).contains(s))
            .with_context(|| format!("approves.ttl_secs must be 1..{MAX_TTL_SECS}"))?,
        None => DEFAULT_TTL_SECS,
    };
    let task = task_id.context("approves needs the task_id of the asking ship's open task")?;
    let task_record = store.task(task)?;
    let owner = task_record["owner"].as_str().unwrap_or_default();
    ensure!(
        owner == from,
        "approves needs a task {from} owns; {task} is {owner}'s"
    );
    store
        .active_charter(task)
        .with_context(|| format!("approves needs an open task; {task} is not"))?;
    ensure!(
        live(store, from, Store::now_ms())?.len() < MAX_LIVE,
        "{from} already holds {MAX_LIVE} live captain approvals; finish or let one expire first"
    );
    let worst = sentence(
        &"r".repeat(MINTED_REPLY_CHARS),
        id,
        from,
        task,
        LONGEST_UTC,
        &action,
        prefix.as_deref(),
    );
    ensure!(
        worst.chars().count() <= MAX_SENTENCE_CHARS,
        "approves.action with this request id and task would tell auto mode more than \
         {MAX_SENTENCE_CHARS} characters; shorten the action or the request id"
    );
    let mut normalized = json!({"action": action, "ttl_secs": ttl_secs});
    if let Some(prefix) = prefix {
        normalized["command_prefix"] = json!(prefix);
    }
    payload["approves"] = normalized;
    let grants = format!("{from} do this for {}: {action}", hours(ttl_secs));
    if kind == "decision" {
        let labels: Vec<&str> = payload["decision"]["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|o| o["approves"] == true)
            .filter_map(|o| o["label"].as_str())
            .collect();
        return Ok(Some(format!(
            "Choosing {} lets {grants}",
            labels.join(" or ")
        )));
    }
    Ok(Some(format!("Approving lets {grants}")))
}

/// Check a decision's `approves` option flags: booleans only, only when the
/// decision declares approves, and then on at least one option.
pub(crate) fn check_options(decision: &Value, declared: bool) -> Result<()> {
    let mut marked = 0;
    for option in decision["options"].as_array().into_iter().flatten() {
        if let Some(flag) = option.get("approves") {
            ensure!(
                declared,
                "decision.options approves applies only with payload.approves"
            );
            let approving = flag
                .as_bool()
                .context("decision.options approves must be true or false")?;
            if approving {
                for field in ["label", "what_happens"] {
                    let text = option[field].as_str().unwrap_or_default();
                    ensure!(
                        worded(text),
                        "an option that approves needs words in its {field}"
                    );
                    if let Some(word) = hedged(text) {
                        bail!(
                            "an option that approves cannot read as a refusal; its {field} carries \"{word}\""
                        );
                    }
                }
            }
            marked += usize::from(approving);
        }
    }
    ensure!(
        !declared || marked > 0,
        "a decision with payload.approves marks at least one option approves: true"
    );
    Ok(())
}

/// Whether the text holds at least one word with a letter in it.
fn worded(text: &str) -> bool {
    text.split_whitespace()
        .any(|w| w.chars().any(char::is_alphabetic))
}

/// The lowercased words of `text`, split on anything but letters, digits and
/// an apostrophe inside a word, with every apostrophe form made ASCII.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace(['‘', '’', '`', 'ʼ'], "'")
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Words that narrow or condition what they follow ("to staging", "after
/// CI"), so a reply may hold one only by retyping the whole action.
const SCOPE_WORDS: &[&str] = &[
    "for", "to", "on", "in", "at", "into", "onto", "from", "via", "with", "without", "after",
    "before", "when", "once", "while", "and", "or", "then", "plus", "also", "of", "by",
];

/// The words of `text` that are not plain words.
fn unplain(text: &str) -> Vec<String> {
    words(text)
        .into_iter()
        .filter(|w| !PLAIN_WORDS.contains(&w.as_str()))
        .collect()
}

/// Whether a free-text reply that passed `hedged` is a plain yes to
/// `action`. Past its plain words, the reply is nothing (and then holds a yes
/// word), the action's own words retyped in full, or a leading run of them
/// with no scope word: "Deploy build 55" approves "Deploy build 55 to staging
/// and prod", while "deploy to staging" and "staging ok" narrow it and
/// approve nothing.
fn plain_yes(text: &str, action: &str) -> bool {
    let rest = unplain(text);
    let own = unplain(action);
    if rest.is_empty() {
        return words(text).iter().any(|w| YES_WORDS.contains(&w.as_str()));
    }
    rest == own
        || (own.starts_with(&rest) && !rest.iter().any(|w| SCOPE_WORDS.contains(&w.as_str())))
}

/// Whether the text sent with a chosen approving option holds only plain
/// words and the option's own label words; the tap is the yes, so even the
/// action's words would only narrow it.
fn plain_beside(text: &str, label: &str) -> bool {
    let own = words(label);
    words(text)
        .iter()
        .all(|w| PLAIN_WORDS.contains(&w.as_str()) || own.contains(w))
}

/// Question marks in any script, each a refusal like "?".
const QUESTION_MARKS: &[char] = &[
    '?',
    '\u{ff1f}',
    '\u{fe56}',
    '\u{fe16}',
    '\u{bf}',
    '\u{2047}',
    '\u{2048}',
    '\u{2049}',
    '\u{203d}',
    '\u{2e18}',
    '\u{2e2e}',
    '\u{61f}',
    '\u{37e}',
    '\u{55e}',
    '\u{1367}',
    '\u{1945}',
    '\u{a60f}',
    '\u{11143}',
    '\u{2753}',
    '\u{2754}',
];

/// Whether `c` sits in a Unicode symbol or pictograph range, where a thumb,
/// a cross or a check mark beside a word could reverse it.
fn pictograph(c: char) -> bool {
    matches!(u32::from(c),
        0xa9 | 0xae | 0x200d | 0x203c | 0x20e3 | 0x3030 | 0x303d
        | 0x2100..=0x214f
        | 0x2190..=0x2bff
        | 0x3200..=0x32ff
        | 0xfe00..=0xfe0f
        | 0x1f000..=0x1fbff
        | 0xe0020..=0xe007f)
}

/// Whether the reply holds a symbol or pictograph, which leaves it no plain
/// yes whatever words sit beside it.
fn pictured(text: &str) -> bool {
    text.chars().any(pictograph)
}

/// Whether the reply text holds a word that makes it something other than a
/// plain yes. A false no costs the captain one more ask; a false yes grants
/// what they did not, so this errs toward no.
fn hedged(text: &str) -> Option<String> {
    if let Some(mark) = text.chars().find(|c| QUESTION_MARKS.contains(c)) {
        return Some(mark.to_string());
    }
    let words = words(text);
    REFUSALS
        .iter()
        .find(|refusal| {
            let parts: Vec<&str> = refusal.split(' ').collect();
            words
                .windows(parts.len())
                .any(|w| w.iter().eq(parts.iter()))
        })
        .map(|w| (*w).to_owned())
        .or_else(|| {
            words.iter().find_map(|word| {
                REFUSAL_STEMS
                    .iter()
                    .any(|stem| word.starts_with(stem))
                    .then(|| (*word).to_owned())
            })
        })
}

/// What the captain's reply to `request` approves, recorded in the caller's
/// write transaction at `now`. Only the home holding the original request
/// (not a desk copy) records; elsewhere, and for a request that declares
/// nothing, this returns None. Otherwise the result is the reply's
/// `payload.approval`.
pub(crate) fn record(
    store: &Store,
    request: &Value,
    reply_id: &str,
    body: &str,
    payload: &Value,
    now: i64,
) -> Result<Option<Value>> {
    if request["payload"]["type"] == "tool_permission" {
        return record_block(store, request, reply_id, body, payload, now);
    }
    let Some(approves) = request["payload"].get("approves") else {
        return Ok(None);
    };
    if request["delivery"] == "received" {
        return Ok(None);
    }
    let ship = request["from"].as_str().unwrap_or_default();
    let skip = |why: &str| skipped(store, ship, request, reply_id, why);
    if request["state"] != "open" {
        return skip("the request was closed before this reply, so nothing was approved");
    }
    if payload.get("fanned_from").is_some() || payload["operator"] != true {
        return skip(
            "only the captain's own reply approves; a relay, a bridge or a copy approves nothing",
        );
    }
    let text = reply_text(body, payload);
    let source = if request["kind"] == "decision" {
        let chosen = payload["answer"]["option_id"].as_str();
        let option = request["payload"]["decision"]["options"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|o| chosen.is_some() && o["id"].as_str() == chosen);
        let Some(option) = option.filter(|o| o["approves"] == true) else {
            return skip(
                "the captain did not choose an option that approves; free text on a decision approves nothing",
            );
        };
        if let Some(why) = beside(&text, option["label"].as_str().unwrap_or_default()) {
            return skip(&why);
        }
        "option"
    } else {
        if text.is_empty() {
            return skip("the captain's reply is empty, so nothing was approved; ask again");
        }
        if !worded(&text) {
            return skip("reply had no words; ask again with a decision");
        }
        if pictured(&text) {
            return skip(PICTURED);
        }
        if let Some(word) = hedged(&text) {
            return skip(&format!(
                "the captain's reply carries \"{word}\", so it is not a plain yes and nothing was approved; ask again with a decision"
            ));
        }
        if !plain_yes(&text, approves["action"].as_str().unwrap_or_default()) {
            return skip(NOT_PLAIN);
        }
        "reply"
    };
    let task = request["task_id"].as_str().unwrap_or_default();
    if !open_task(store, ship, task) {
        return skip("the request's task is no longer open, so nothing was approved");
    }
    let action = approves["action"].as_str().unwrap_or_default();
    let ttl = approves["ttl_secs"]
        .as_u64()
        .unwrap_or(DEFAULT_TTL_SECS)
        .clamp(1, MAX_TTL_SECS);
    if action.is_empty() {
        return skip("the request names no action, so nothing was approved");
    }
    insert(
        store,
        &Grant {
            ship,
            task,
            request,
            reply_id,
            action,
            prefix: approves["command_prefix"].as_str(),
            ttl,
            source,
            renews: None,
        },
        now,
    )
}

/// Audit why a reply approved nothing, and say so in its `payload.approval`.
fn skipped(
    store: &Store,
    ship: &str,
    request: &Value,
    reply_id: &str,
    why: &str,
) -> Result<Option<Value>> {
    store.audit(
        "permissions.approval_skipped",
        &json!({"ship": ship, "request": request["id"], "reply": reply_id, "why": why}),
    )?;
    Ok(Some(json!({"recorded": false, "why": why})))
}

/// The reply's words: its body and its answer's text, one per line.
fn reply_text(body: &str, payload: &Value) -> String {
    [body, payload["answer"]["text"].as_str().unwrap_or("")]
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Why the text sent beside a chosen approving option keeps it from
/// approving, if it does. The door may send the chosen label back as the
/// answer's text, which is the tap itself.
fn beside(text: &str, label: &str) -> Option<String> {
    if text.is_empty() || text == label {
        return None;
    }
    if !worded(text) {
        return Some("reply had no words; ask again with a decision".to_owned());
    }
    if pictured(text) {
        return Some(PICTURED.to_owned());
    }
    if let Some(word) = hedged(text) {
        return Some(format!(
            "the captain's words carry \"{word}\", so nothing was approved; ask again"
        ));
    }
    (!plain_beside(text, label)).then(|| NOT_PLAIN.to_owned())
}

/// One approval to record from a captain's reply.
struct Grant<'a> {
    ship: &'a str,
    task: &'a str,
    /// The request or block ask the reply answers.
    request: &'a Value,
    reply_id: &'a str,
    action: &'a str,
    prefix: Option<&'a str>,
    ttl: u64,
    source: &'a str,
    /// The expired approval a block card's tap renews.
    renews: Option<&'a str>,
}

/// Record an approval, unless the ship already holds the most it may or its
/// sentence would be too long for auto mode.
fn insert(store: &Store, grant: &Grant, now: i64) -> Result<Option<Value>> {
    let Grant {
        ship,
        task,
        request,
        reply_id,
        action,
        prefix,
        ttl,
        source,
        renews,
    } = *grant;
    let skip = |why: &str| skipped(store, ship, request, reply_id, why);
    if live(store, ship, now)?.len() >= MAX_LIVE {
        return skip(&format!(
            "{ship} already holds {MAX_LIVE} live captain approvals, so nothing was approved"
        ));
    }
    let id = format!("approval-{}", uuid::Uuid::new_v4().simple());
    let expires_ms = now + ttl as i64 * 1000;
    let request_id = request["id"].as_str().unwrap_or_default();
    let told = sentence(
        reply_id,
        request_id,
        ship,
        task,
        &crate::routes::utc_time(expires_ms),
        action,
        prefix,
    );
    if told.chars().count() > MAX_SENTENCE_CHARS {
        return skip(
            "sentence too long for auto mode with this reply id, so nothing was approved; ask again",
        );
    }
    // Where the ship's session picks the approval up, as the ship is now.
    let applies = crate::host::upgrade::approval_applies(store, ship)?;
    store.connection().execute(
        "INSERT INTO captain_approvals(id,ship,task_id,request_id,reply_id,action,command_prefix,created_ms,expires_ms,source,applies_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            id, ship, task, request_id, reply_id, action, prefix, now, expires_ms, source, applies
        ],
    )?;
    let mut audit = json!({"id": id, "ship": ship, "task": task, "request": request["id"],
        "reply": reply_id, "action": action, "expires_ms": expires_ms, "source": source,
        "applies": applies});
    if source == "block" {
        audit["ask"] = request["id"].clone();
        audit["command_prefix"] = json!(prefix);
    }
    if let Some(old) = renews {
        audit["renews"] = json!(old);
    }
    store.audit("permissions.approval_recorded", &audit)?;
    Ok(Some(json!({"recorded": true, "id": id, "action": action,
        "expires_ms": expires_ms, "applies": applies, "note": note(applies, action)})))
}

/// An approval a block card offered: its task, action, command prefix,
/// creation, expiry and revocation.
type Offered = (String, String, Option<String>, i64, i64, Option<i64>);

/// What the captain's tap on a block card's approve or as-approved option
/// records, in the home holding the ask. `approve` records the step the card
/// named for 24 h. `as_approved_N` uses an approval the card offered again:
/// one still live records nothing and is applied at the session's next safe
/// point; one that expired within the last 24 h is renewed for its own
/// length. Any other answer to a block ask approves nothing here.
fn record_block(
    store: &Store,
    ask: &Value,
    reply_id: &str,
    body: &str,
    payload: &Value,
    now: i64,
) -> Result<Option<Value>> {
    let chosen = payload["answer"]["option_id"].as_str().unwrap_or_default();
    let reuse = chosen
        .strip_prefix("as_approved_")
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| (1..=2).contains(n));
    if (chosen != "approve" && reuse.is_none()) || ask["delivery"] == "received" {
        return Ok(None);
    }
    let ship = ask["from"].as_str().unwrap_or_default();
    let skip = |why: &str| skipped(store, ship, ask, reply_id, why);
    if ask["state"] != "open" {
        return skip("the ask was closed before this reply, so nothing was approved");
    }
    if payload.get("fanned_from").is_some() || payload["operator"] != true {
        return skip(
            "only the captain's own reply approves; a relay, a bridge or a copy approves nothing",
        );
    }
    let label = ask["payload"]["decision"]["options"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o["id"] == chosen)
        .and_then(|o| o["label"].as_str())
        .unwrap_or_default();
    if let Some(why) = beside(&reply_text(body, payload), label) {
        return skip(&why);
    }
    let block = &ask["payload"]["block"];
    let Some(n) = reuse else {
        let task = ask["task_id"].as_str().unwrap_or_default();
        let action = block["approve"]["action"].as_str().unwrap_or_default();
        if action.is_empty() {
            return skip("the ask offers no step to approve, so nothing was approved");
        }
        if !open_task(store, ship, task) {
            return skip("the ask's task is no longer open, so nothing was approved");
        }
        return insert(
            store,
            &Grant {
                ship,
                task,
                request: ask,
                reply_id,
                action,
                prefix: block["approve"]["command_prefix"].as_str(),
                ttl: DEFAULT_TTL_SECS,
                source: "block",
                renews: None,
            },
            now,
        );
    };
    let Some(id) = block["approvals"][n - 1].as_str() else {
        return skip("the ask offers no such approval, so nothing was approved");
    };
    let row: Option<Offered> = store
        .connection()
        .query_row(
            "SELECT task_id,action,command_prefix,created_ms,expires_ms,revoked_ms
             FROM captain_approvals WHERE id=?1 AND ship=?2",
            params![id, ship],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((task, action, prefix, created_ms, expires_ms, revoked)) = row else {
        return skip("the approval the ask offered is gone, so nothing was approved");
    };
    if revoked.is_some() {
        return skip("the approval the ask offered was revoked, so nothing was approved");
    }
    if !open_task(store, ship, &task) {
        return skip("the approval's task is no longer open, so nothing was approved");
    }
    if expires_ms > now {
        // Still live: the session that was refused did not carry it, and
        // its next launch does.
        let applies = crate::host::upgrade::approval_applies(store, ship)?;
        store.connection().execute(
            "UPDATE captain_approvals SET applies_at=?2 WHERE id=?1",
            params![id, applies],
        )?;
        store.audit(
            "permissions.approval_reused",
            &json!({"id": id, "ship": ship, "ask": ask["id"], "reply": reply_id,
                "applies": applies}),
        )?;
        return Ok(Some(
            json!({"recorded": false, "covered_by": id, "applies": applies,
            "note": note(applies, &action)}),
        ));
    }
    if now - created_ms >= ONE_TAP_MS {
        return skip("the approval is older than 24 h, so nothing was approved; ask again");
    }
    let ttl = u64::try_from((expires_ms - created_ms) / 1000)
        .unwrap_or(1)
        .clamp(1, MAX_TTL_SECS);
    insert(
        store,
        &Grant {
            ship,
            task: &task,
            request: ask,
            reply_id,
            action: &action,
            prefix: prefix.as_deref(),
            ttl,
            source: "block",
            renews: Some(id),
        },
        now,
    )
}

/// What a block card offers from captain approvals: its options, the
/// approval ids its as-approved options use, the step its approve option
/// records, and a line for its `why`.
#[derive(Default)]
pub(crate) struct Offer {
    pub(crate) options: Vec<Value>,
    pub(crate) approvals: Vec<String>,
    pub(crate) approve: Option<Value>,
    pub(crate) why: Option<String>,
}

/// What a first mate's block card offers from captain approvals, when every
/// denial it lists is a Bash command the auto-mode classifier refused: up to
/// two recent approvals the refused session did not carry, to use again,
/// and this exact step, to approve for 24 h. A crew's or an errand's card,
/// or one naming any other denial, offers neither.
pub(crate) fn offer(
    store: &Store,
    ship: &str,
    scope: &str,
    job_id: Option<&str>,
    records: &[Value],
    more: usize,
    now: i64,
) -> Result<Offer> {
    let mut offer = Offer::default();
    let refused = more == 0
        && !records.is_empty()
        && records
            .iter()
            .all(|d| d["tool"] == "Bash" && d["classifier"] == true);
    if !matches!(scope, "pane" | "wake") || !refused {
        return Ok(offer);
    }
    let heads: Vec<Option<Vec<String>>> = records
        .iter()
        .map(|d| {
            d["head"].as_array().map(|words| {
                words
                    .iter()
                    .filter_map(|w| w.as_str().map(str::to_owned))
                    .collect()
            })
        })
        .collect();
    let covers_all = |prefix: Option<&str>| {
        prefix.is_some_and(|p| {
            heads
                .iter()
                .all(|h| h.as_ref().is_some_and(|h| covers(p, h)))
        })
    };
    let carried = carried(store, ship, scope, job_id)?;
    let in_session = |row: &Row| match &carried {
        Carried::Ids(ids) => ids.contains(&row.id),
        Carried::Before(at) => row.created_ms < *at && row.expires_ms > *at,
        Carried::Unknown => true,
    };
    let known = !matches!(carried, Carried::Unknown);
    // Auto mode refused this with a covering approval in its session.
    let ignored = live(store, ship, now)?
        .into_iter()
        .rev()
        .find(|row| known && in_session(row) && covers_all(row.prefix.as_deref()));
    let resume = if scope == "pane" {
        "The session restarts at its next idle point with it; nothing to relaunch."
    } else {
        "The ship's next wake carries it; nothing to relaunch."
    };
    let mut recommended = ignored.is_some();
    for row in recent(store, ship, now)?
        .into_iter()
        .filter(|row| known && !in_session(row))
        .take(2)
    {
        let until = crate::routes::utc_time(row.expires_ms);
        let what = if row.expires_ms > now {
            format!(
                "Uses captain approval {} as you gave it, \"{}\", until {until}. {resume}",
                row.id, row.action
            )
        } else {
            let ttl = u64::try_from((row.expires_ms - row.created_ms) / 1000)
                .unwrap_or(1)
                .clamp(1, MAX_TTL_SECS);
            format!(
                "Renews captain approval {} as you gave it, \"{}\", for {} from your tap; it expired {until}. {resume}",
                row.id,
                row.action,
                hours(ttl)
            )
        };
        let mut option = json!({"id": format!("as_approved_{}", offer.approvals.len() + 1),
            "label": crate::block_ask::clip(&format!("As approved: {}", row.action), 80),
            "what_happens": crate::block_ask::clip(&what, 400)});
        if !recommended && covers_all(row.prefix.as_deref()) {
            option["recommended"] = json!(true);
            recommended = true;
        }
        offer.options.push(option);
        offer.approvals.push(row.id);
    }
    if let Some((action, prefix)) = step(store, ship, records, &heads) {
        let only = prefix
            .as_deref()
            .map(|p| format!(", only for commands that begin exactly with {p}"))
            .unwrap_or_default();
        let applies = if scope == "pane" {
            "applies at the session's next idle point; nothing to relaunch"
        } else {
            "applies from the ship's next wake; nothing to relaunch"
        };
        let mut option = json!({"id": "approve",
            "label": crate::block_ask::clip(&format!("Approve for 24 h: {action}"), 80),
            "what_happens": crate::block_ask::clip(&format!(
                "Records your approval for {ship} of \"{action}\"{only}. It holds for 24 h from your tap and {applies}."), 400)});
        if !recommended || ignored.is_some() {
            option["recommended"] = json!(true);
        }
        offer.options.push(option);
        offer.approve = Some(json!({"action": action, "command_prefix": prefix}));
    }
    if let Some(row) = ignored {
        offer.why = Some(format!(
            "Auto mode refused this although captain approval {} ({}) was in its session.",
            row.id,
            crate::block_ask::clip(&row.action, 60)
        ));
    }
    Ok(offer)
}

/// Which approvals the refused session carried.
enum Carried {
    /// A hosted pane's: the ids its launch carried.
    Ids(Vec<String>),
    /// A wake's: those live when its job was created.
    Before(i64),
    /// No session to tell by.
    Unknown,
}

fn carried(store: &Store, ship: &str, scope: &str, job_id: Option<&str>) -> Result<Carried> {
    if scope == "pane" {
        let Some((pid, _, _)) = crate::host::upgrade::live_pane(store, ship)? else {
            return Ok(Carried::Ids(vec![]));
        };
        let ids: Option<String> = store
            .connection()
            .query_row(
                "SELECT approvals_ids FROM hosted_panes WHERE ship=?1 AND pid=?2",
                params![ship, pid],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        return Ok(Carried::Ids(
            ids.and_then(|ids| serde_json::from_str(&ids).ok())
                .unwrap_or_default(),
        ));
    }
    let Some(job) = job_id else {
        return Ok(Carried::Unknown);
    };
    let started: Option<i64> = store
        .connection()
        .query_row(
            "SELECT json_extract(record,'$.created_ms') FROM jobs WHERE id=?1",
            [job],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(started.map_or(Carried::Unknown, Carried::Before))
}

/// The exact step a block card offers to approve: "Run" and the masked
/// previews of its refused commands, and, for one command a prefix can
/// match, that command's words as the prefix. None when either would not
/// pass as a ship's own `approves` would.
fn step(
    store: &Store,
    ship: &str,
    records: &[Value],
    heads: &[Option<Vec<String>>],
) -> Option<(String, Option<String>)> {
    let previews: Vec<String> = records
        .iter()
        .map(|d| format!("`{}`", d["preview"].as_str().unwrap_or_default()))
        .collect();
    let action = format!("Run {}", previews.join("; "));
    if action.chars().count() > MAX_ACTION_CHARS
        || action.chars().any(char::is_control)
        || lint(store, ship, "the step", &action).is_err()
    {
        return None;
    }
    let prefix = match heads {
        [Some(head)] => {
            let prefix = head.join(" ");
            if prefix.chars().count() > MAX_PREFIX_CHARS
                || check_prefix(&prefix).is_err()
                || lint(store, ship, "the prefix", &prefix).is_err()
            {
                return None;
            }
            Some(prefix)
        }
        _ => None,
    };
    Some((action, prefix))
}

/// What the ship is told about when its session picks up an approval.
fn note(applies: &str, action: &str) -> String {
    match applies {
        "relaunch" => "recorded; applies when your session restarts at its next idle point. \
             End this turn without retrying; a notice says when it is applied."
            .to_owned(),
        "wake" => format!("applied from this wake; retry {action} once."),
        _ => "recorded; applies at your next launch.".to_owned(),
    }
}

pub(crate) struct Row {
    pub(crate) id: String,
    task: String,
    request: String,
    reply: String,
    pub(crate) action: String,
    pub(crate) prefix: Option<String>,
    pub(crate) created_ms: i64,
    pub(crate) expires_ms: i64,
}

/// Whether `ship` owns `task` and it is open under an active charter.
fn open_task(store: &Store, ship: &str, task: &str) -> bool {
    store.task(task).is_ok_and(|t| t["owner"] == ship) && store.active_charter(task).is_ok()
}

/// A ship's unrevoked approvals matching `filter` (a condition on
/// `created_ms` and `expires_ms` against `?2`), in `order`, on a task the
/// ship still owns, open under an active charter.
fn rows(store: &Store, ship: &str, filter: &str, order: &str, at: i64) -> Result<Vec<Row>> {
    let mut statement = store.connection().prepare(&format!(
        "SELECT id,task_id,request_id,reply_id,action,command_prefix,created_ms,expires_ms
         FROM captain_approvals WHERE ship=?1 AND revoked_ms IS NULL AND {filter} ORDER BY {order}"
    ))?;
    let rows = statement
        .query_map(params![ship, at], |r| {
            Ok(Row {
                id: r.get(0)?,
                task: r.get(1)?,
                request: r.get(2)?,
                reply: r.get(3)?,
                action: r.get(4)?,
                prefix: r.get(5)?,
                created_ms: r.get(6)?,
                expires_ms: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|row| open_task(store, ship, &row.task))
        .collect())
}

/// A ship's live approvals: not revoked, not expired, and on a task the ship
/// still owns, open under an active charter.
pub(crate) fn live(store: &Store, ship: &str, now: i64) -> Result<Vec<Row>> {
    rows(store, ship, "expires_ms>?2", "created_ms, id", now)
}

/// The approvals a block card may offer to use again in one tap: created in
/// the last 24 h, not revoked, on a task the ship still owns and is open,
/// newest first. An expired one is renewed by the tap.
pub(crate) fn recent(store: &Store, ship: &str, now: i64) -> Result<Vec<Row>> {
    rows(
        store,
        ship,
        "created_ms>?2",
        "created_ms DESC, id",
        now - ONE_TAP_MS,
    )
}

/// The one sentence the classifier reads for an approval. A command prefix
/// is named after the action, unless that would make the sentence longer
/// than a policy accepts; then the plain sentence stands.
fn sentence(
    reply: &str,
    request: &str,
    ship: &str,
    task: &str,
    until: &str,
    action: &str,
    prefix: Option<&str>,
) -> String {
    let plain = format!(
        "The captain approved this in reply {reply} to request {request}, for ship {ship} only, \
         on task {task}, until {until}: {action}"
    );
    match prefix {
        Some(prefix) => {
            let narrowed = format!("{plain} Only commands that begin exactly with: {prefix}");
            if narrowed.chars().count() <= MAX_SENTENCE_CHARS {
                narrowed
            } else {
                plain
            }
        }
        None => plain,
    }
}

/// What the auto-mode classifier is told for a ship's first-mate launch: one
/// sentence per live approval, at most eight. A sentence too long for the
/// launch's policy check is skipped and audited, so one bad row never stops
/// a launch.
pub(crate) fn sentences(store: &Store, ship: &str, now: i64) -> Result<Vec<String>> {
    Ok(launch_set(store, ship, now)?
        .into_iter()
        .map(|(_, sentence)| sentence)
        .collect())
}

/// The approvals a launch now carries, as (id, sentence), in the order told.
pub(crate) fn launch_set(store: &Store, ship: &str, now: i64) -> Result<Vec<(String, String)>> {
    told(store, ship, now, true)
}

/// The approvals a launch now would carry. Read-only: a skipped sentence is
/// audited by the launch, not by every check.
pub(crate) fn current_set(store: &Store, ship: &str, now: i64) -> Result<Vec<(String, String)>> {
    told(store, ship, now, false)
}

/// A digest of the sentences a launch carried. Claude reads its settings only
/// at launch (tests/fixtures/claude/settings-reload.md), so a pane whose
/// digest differs from `current_digest` runs on stale approvals.
pub(crate) fn digest<S: AsRef<str>>(sentences: &[S]) -> String {
    let mut hasher = Sha256::new();
    for sentence in sentences.iter().map(AsRef::as_ref) {
        hasher.update(sentence.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

/// The digest a launch now would carry, read-only.
pub(crate) fn current_digest(store: &Store, ship: &str, now: i64) -> Result<String> {
    Ok(set_digest(&current_set(store, ship, now)?))
}

/// The digest of a launch set's sentences.
pub(crate) fn set_digest(set: &[(String, String)]) -> String {
    let sentences: Vec<&str> = set.iter().map(|(_, sentence)| sentence.as_str()).collect();
    digest(&sentences)
}

fn told(store: &Store, ship: &str, now: i64, audit: bool) -> Result<Vec<(String, String)>> {
    let mut told = vec![];
    for row in live(store, ship, now)? {
        let text = sentence(
            &row.reply,
            &row.request,
            ship,
            &row.task,
            &crate::routes::utc_time(row.expires_ms),
            &row.action,
            row.prefix.as_deref(),
        );
        if text.chars().count() > MAX_SENTENCE_CHARS {
            if audit {
                store.audit(
                    "permissions.approval_skipped",
                    &json!({"id": row.id, "ship": ship, "why": "sentence too long"}),
                )?;
            }
            continue;
        }
        told.push((row.id, text));
        if told.len() == MAX_LIVE {
            break;
        }
    }
    Ok(told)
}

fn left(ms: i64) -> String {
    let minutes = (ms / 60_000).max(1);
    if minutes >= 60 {
        format!("{} h {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    }
}

/// Bearings lines for a ship's live approvals, and how to ask for one.
pub(crate) fn bearings_lines(store: &Store, ship: &str) -> Result<Vec<String>> {
    let now = Store::now_ms();
    let mut lines: Vec<String> = live(store, ship, now)?
        .into_iter()
        .map(|row| {
            format!(
                "Captain approval {} ({} left): {}",
                row.id,
                left(row.expires_ms - now),
                row.action
            )
        })
        .collect();
    lines.push(
        "Free-text replies count only to requests that carry approves; ask with approves when you need a step approved."
            .into(),
    );
    Ok(lines)
}

fn list(store: &Store, args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    crate::store::identifier(ship)?;
    let now = Store::now_ms();
    let live: Vec<String> = live(store, ship, now)?.into_iter().map(|r| r.id).collect();
    let mut statement = store.connection().prepare(
        "SELECT id,task_id,request_id,reply_id,action,command_prefix,created_ms,expires_ms,revoked_ms,source
         FROM captain_approvals WHERE ship=?1 ORDER BY created_ms DESC, id LIMIT 100",
    )?;
    let approvals = statement
        .query_map([ship], |r| {
            Ok(json!({"id": r.get::<_, String>(0)?, "ship": ship, "task_id": r.get::<_, String>(1)?,
                "request_id": r.get::<_, String>(2)?, "reply_id": r.get::<_, String>(3)?,
                "action": r.get::<_, String>(4)?, "command_prefix": r.get::<_, Option<String>>(5)?,
                "created_ms": r.get::<_, i64>(6)?, "expires_ms": r.get::<_, i64>(7)?,
                "revoked_ms": r.get::<_, Option<i64>>(8)?, "source": r.get::<_, String>(9)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|mut row| {
            row["live"] = json!(live.iter().any(|id| row["id"] == id.as_str()));
            row
        })
        .collect::<Vec<_>>();
    Ok(json!({"ship": ship, "approvals": approvals}))
}

fn revoke(store: &Store, args: &Value) -> Result<Value> {
    store.caller().require_operator("captain_approval.revoke")?;
    let id = required(args, "id")?;
    let revoked: Option<Option<i64>> = store
        .connection()
        .query_row(
            "SELECT revoked_ms FROM captain_approvals WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(revoked) = revoked else {
        bail!("no captain approval {id}");
    };
    if let Some(revoked_ms) = revoked {
        return Ok(json!({"id": id, "revoked_ms": revoked_ms, "already": true}));
    }
    let now = Store::now_ms();
    store.connection().execute(
        "UPDATE captain_approvals SET revoked_ms=?2 WHERE id=?1 AND revoked_ms IS NULL",
        params![id, now],
    )?;
    store.audit("permissions.approval_revoked", &json!({"id": id}))?;
    Ok(json!({"id": id, "revoked_ms": now, "already": false}))
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "captain_approval.list" => list(store, &args),
        "captain_approval.revoke" => {
            ensure!(
                *store.caller() == Caller::Operator,
                "captain_approval.revoke is an operator command; agents cannot run it"
            );
            revoke(store, &args)
        }
        _ => bail!("unknown command {command}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_yes_passes_and_a_hedge_names_its_word() {
        for yes in ["Submit build 55", "yes", "Go ahead.", "ok", "Approved"] {
            assert_eq!(hedged(yes), None, "{yes}");
        }
        for (reply, word) in [
            ("no", "no"),
            ("not yet", "not"),
            ("wait for CI", "wait"),
            ("ok?", "?"),
            ("Don’t", "don't"),
            ("yes but only staging", "but"),
            ("do it if CI is green", "if"),
        ] {
            assert_eq!(hedged(reply).as_deref(), Some(word), "{reply}");
        }
        // A word inside a longer one is not a refusal.
        assert_eq!(hedged("notify the butler"), None);
        for refusal in [
            "Denied",
            "Rejected",
            "Declined",
            "Cancelled",
            "Stopped",
            "Refused",
            "Waiting",
            "Veto",
            "Skip it",
            "Postponed",
            "Paused",
            "Holding",
            "Aborted",
            "Negative",
            "Nevermind",
            "never mind",
        ] {
            assert!(hedged(refusal).is_some(), "{refusal}");
        }
        for yes in ["Submit build 55", "yes", "Go ahead", "ok do it", "Approved"] {
            assert!(worded(yes) && hedged(yes).is_none(), "{yes}");
        }
        for wordless in ["👎", "👍", "❌", "✋", "!!", "55"] {
            assert!(!worded(wordless), "{wordless}");
        }
    }

    #[test]
    fn a_reply_is_plain_only_in_yes_words_or_the_actions_own() {
        let submit = "Submit build 55 to TestFlight";
        for yes in [
            "Submit build 55",
            "yes",
            "ok do it",
            "Go ahead",
            "Approved",
            "lgtm",
            "yes please",
            "ok, submit it",
            "submit it",
            "Y",
            "K",
        ] {
            assert!(plain_yes(yes, submit), "{yes}");
        }
        for loose in [
            "n",
            "nvm",
            "nogo",
            "nein",
            "hmm",
            "yes after CI",
            "yes when tests pass",
            "go once green",
            "after lunch",
            "tomorrow",
            "next week",
            "yes for staging",
            "show me the diff",
            "why",
            "what is this",
            "let me check",
            "hang on",
            "thanks",
            "please",
            "TestFlight build 55",
        ] {
            assert!(!plain_yes(loose, submit), "{loose}");
        }
        for dont in ["Don‘t", "Don`t", "Donʼt", "Don’t", "DON'T"] {
            assert_eq!(words(dont), ["don't"], "{dont}");
            assert_eq!(hedged(dont).as_deref(), Some("don't"), "{dont}");
        }
        assert_eq!(words("'ok', ship it!"), ["ok", "ship", "it"]);
    }

    #[test]
    fn a_reply_that_narrows_the_action_is_no_yes() {
        let deploy = "Deploy build 55 to staging and prod";
        for narrow in [
            "yes for staging",
            "yes to staging",
            "staging ok",
            "deploy to staging",
            "deploy build 55 to staging",
            "build 55",
            "deploy prod",
        ] {
            assert!(!plain_yes(narrow, deploy), "{narrow}");
        }
        for yes in [
            "Deploy build 55",
            "Deploy build 55 to staging and prod",
            "deploy build 55 to staging and prod, thanks",
            "yes",
            "ok deploy",
        ] {
            assert!(plain_yes(yes, deploy), "{yes}");
        }
        // Deliberate: naming only the head of a multi-part action approves
        // all of it, as the captain's own "Submit build 55" did.
        assert!(plain_yes(
            "merge PR 340",
            "Merge PR 340 and deploy to staging"
        ));
    }

    #[test]
    fn beside_a_chosen_option_only_plain_and_label_words_pass() {
        for ok in ["thanks", "Ship it", "yes, ship it now"] {
            assert!(plain_beside(ok, "Ship it"), "{ok}");
        }
        for loose in ["after CI", "Submit build 55", "for staging", "staging"] {
            assert!(!plain_beside(loose, "Ship it"), "{loose}");
        }
    }

    #[test]
    fn any_question_mark_or_pictograph_is_no_yes() {
        for asked in [
            "ok?",
            "ok？",
            "ok¿",
            "¿ok",
            "ok⁇",
            "ok⁈",
            "ok⁉",
            "ok؟",
            "ok‽",
            "ok﹖",
            "ok\u{37e}",
            "ok❓",
        ] {
            assert!(hedged(asked).is_some(), "{asked}");
        }
        for shown in [
            "👎 yes",
            "yes 👍",
            "ok ❌",
            "✋ go",
            "yes ✔",
            "ok ☹",
            "go →",
            "yes ⚠️",
            "ok 🛑",
            "yes ™",
        ] {
            assert!(worded(shown) && pictured(shown), "{shown}");
        }
        for plain in [
            "yes",
            "Submit build 55",
            "ok, go ahead!",
            "Don't",
            "déployer",
            "はい",
        ] {
            assert!(!pictured(plain), "{plain}");
        }
    }

    #[test]
    fn an_approving_option_cannot_read_as_a_refusal() {
        let options = |label: &str, what: &str| {
            json!({"options": [
                {"id": "a", "label": label, "what_happens": what, "approves": true},
                {"id": "b", "label": "Hold", "what_happens": "Nothing is submitted"}
            ]})
        };
        for (label, what) in [
            ("Don't submit", "Build 55 goes to TestFlight"),
            ("Skip", "Build 55 goes to TestFlight"),
            ("Ship it", "Build 55 is not submitted"),
            ("👍", "Build 55 goes to TestFlight"),
        ] {
            assert!(
                check_options(&options(label, what), true).is_err(),
                "{label}"
            );
        }
        check_options(&options("Ship it", "Build 55 goes to TestFlight"), true).unwrap();
    }

    #[test]
    fn a_prefix_must_name_more_than_a_shell_or_wrapper() {
        for bad in [
            "bash",
            "sh -c",
            "python3",
            "python3 -c",
            "/usr/bin/env make",
            "sudo make deploy",
            "xargs rm",
            "node -e",
            "python -m http.server",
        ] {
            assert!(check_prefix(bad).is_err(), "{bad}");
        }
        for good in [
            "make deploy",
            "python3 scripts/release.py",
            "bash ./ship.sh",
        ] {
            check_prefix(good).unwrap();
        }
    }

    #[test]
    fn a_prefix_is_one_simple_command_and_no_wrapper() {
        for bad in [
            "make release; rm x",
            "make release && x",
            "make | tee",
            "make > out",
            "make < in",
            "(make)",
            "echo $HOME",
            "echo `id`",
            "make \\\n x",
            "FOO=1 make",
            "doas make",
            "su -c make",
            "exec make",
            "command make",
            "builtin cd",
            "nohup make",
            "timeout 5 make",
            "nice make",
            "time make",
            "watch make",
            "ssh host make",
            "/usr/bin/sudo make",
            " ",
        ] {
            assert!(check_prefix(bad).is_err(), "{bad}");
        }
        for good in ["make release", "./scripts/ship.sh --prod", "cargo publish"] {
            check_prefix(good).unwrap();
        }
    }

    #[test]
    fn a_command_head_is_the_words_of_one_simple_command() {
        assert_eq!(
            command_head("make release --dry-run").unwrap(),
            ["make", "release", "--dry-run"]
        );
        assert_eq!(
            command_head("  make   release ").unwrap(),
            ["make", "release"]
        );
        for compound in [
            "make release; rm x",
            "make  release; rm x",
            "make release && x",
            "make release | tee log",
            "make release > log",
            "make release < in",
            "(make release)",
            "make $TARGET",
            "make `x`",
            "make \\\n release",
            "make release\nrm x",
            "make release\r",
            "TARGET=prod make release",
            "deploy deadbeef0123456789abcdef0123456789abcdef",
            "deploy API_TOKEN=abc",
            "curl -u bob https://bob:pw@x.io",
            "",
            "   ",
        ] {
            assert_eq!(command_head(compound), None, "{compound}");
        }
        let long = format!("make {}", "x ".repeat(16));
        assert_eq!(command_head(&long), None);
        assert_eq!(command_head(&format!("make {}", "x".repeat(200))), None);
    }

    #[test]
    fn a_prefix_covers_only_whole_leading_words() {
        let head = |command: &str| command_head(command).unwrap();
        assert!(covers("make release", &head("make release --dry-run")));
        assert!(covers("make release", &head("make release")));
        assert!(covers("make  release", &head("make release now")));
        for other in [
            "make releasex",
            "./make release",
            "sudo make release",
            "Make release",
            "make",
            "make build release",
        ] {
            assert!(!covers("make release", &head(other)), "{other}");
        }
        assert!(!covers("", &head("make")));
        // A compound command has no head, so no prefix covers it.
        assert_eq!(command_head("make  release; rm x"), None);
    }

    #[test]
    fn a_sentence_names_its_prefix_when_it_fits() {
        let told = |action: &str, prefix| {
            sentence(
                "r1",
                "q1",
                "apps",
                "main",
                "2026-10-21 12:00 UTC",
                action,
                prefix,
            )
        };
        let plain = told("Run `make release`", None);
        assert!(
            plain.ends_with("until 2026-10-21 12:00 UTC: Run `make release`"),
            "{plain}"
        );
        assert_eq!(
            told("Run `make release`", Some("make release")),
            format!("{plain} Only commands that begin exactly with: make release")
        );
        // Too long with the prefix: the plain sentence stands.
        let action = "x".repeat(MAX_SENTENCE_CHARS - plain.chars().count() + 18);
        let long = told(&action, Some("make release"));
        assert_eq!(long, told(&action, None));
        assert!(long.chars().count() <= MAX_SENTENCE_CHARS);
    }

    #[test]
    fn a_ship_name_counts_only_as_a_whole_word() {
        assert!(names("Deploy infra's build", "infra"));
        assert!(names("restart INFRA.", "infra"));
        assert!(!names("Deploy infrastructure", "infra"));
        assert!(!names("Deploy my-infra-box", "infra"));
    }

    /// The digest a launch records matches the read-only check until an
    /// approval is recorded, revoked or expires.
    #[test]
    fn the_approvals_digest_follows_live_approvals() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let mut store = Store::open(&temp.path().join("home")).unwrap();
        crate::dispatch(
            &mut store,
            "charter.put",
            json!({"id":"project","objective":"Deliver","repo":repo,"min_free_bytes":0}),
        )
        .unwrap();
        crate::dispatch(
            &mut store,
            "task.create",
            json!({"id":"main","charter_id":"project","owner":"apps","title":"Deliver"}),
        )
        .unwrap();
        let now = 1_000_000;
        let launched = |store: &Store, at| digest(&sentences(store, "apps", at).unwrap());
        let none = launched(&store, now);
        assert_eq!(none, current_digest(&store, "apps", now).unwrap());
        for (id, expires) in [("a1", now + 60_000), ("a2", now + 120_000)] {
            store
                .connection()
                .execute(
                    "INSERT INTO captain_approvals(id,ship,task_id,request_id,reply_id,action,
                       created_ms,expires_ms,source) VALUES(?1,'apps','main',?1,?1,'Deploy',?2,?3,'reply')",
                    params![id, now, expires],
                )
                .unwrap();
        }
        let both = launched(&store, now);
        assert_ne!(both, none);
        assert_eq!(both, current_digest(&store, "apps", now).unwrap());
        // Another ship's view is untouched.
        assert_eq!(current_digest(&store, "infra", now).unwrap(), none);
        store
            .connection()
            .execute(
                "UPDATE captain_approvals SET revoked_ms=?1 WHERE id='a2'",
                [now],
            )
            .unwrap();
        let one = current_digest(&store, "apps", now).unwrap();
        assert_ne!(one, both);
        assert_ne!(one, none);
        assert_eq!(current_digest(&store, "apps", now + 60_000).unwrap(), none);
    }

    /// Each place an approval applies tells the ship what to do next.
    #[test]
    fn each_approval_applies_note_says_when_to_retry() {
        assert!(note("relaunch", "Deploy").contains("End this turn without retrying"));
        assert_eq!(
            note("wake", "Deploy build 7"),
            "applied from this wake; retry Deploy build 7 once."
        );
        assert_eq!(
            note("next_launch", "Deploy"),
            "recorded; applies at your next launch."
        );
    }

    /// A home from before block cards keeps its approvals and takes a
    /// block card's.
    #[test]
    fn an_older_approvals_table_is_rebuilt_to_take_block_approvals() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let mut store = Store::open(&home).unwrap();
        crate::dispatch(
            &mut store,
            "charter.put",
            json!({"id":"project","objective":"Deliver","repo":repo,"min_free_bytes":0}),
        )
        .unwrap();
        crate::dispatch(
            &mut store,
            "task.create",
            json!({"id":"main","charter_id":"project","owner":"apps","title":"Deliver"}),
        )
        .unwrap();
        store
            .connection()
            .execute_batch(
                "DROP TABLE captain_approvals;
                 CREATE TABLE captain_approvals(id TEXT PRIMARY KEY,ship TEXT NOT NULL,
                   task_id TEXT NOT NULL REFERENCES tasks(id),request_id TEXT NOT NULL,reply_id TEXT NOT NULL UNIQUE,
                   action TEXT NOT NULL,command_prefix TEXT,created_ms INTEGER NOT NULL,expires_ms INTEGER NOT NULL,
                   revoked_ms INTEGER,source TEXT NOT NULL CHECK(source IN ('reply','option')));
                 INSERT INTO captain_approvals(id,ship,task_id,request_id,reply_id,action,created_ms,expires_ms,source)
                   VALUES('a1','apps','main','q1','r1','Deploy',1,2,'reply');",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&home).unwrap();
        let block = "INSERT INTO captain_approvals(id,ship,task_id,request_id,reply_id,action,
            created_ms,expires_ms,source,applies_at) VALUES('a2','apps','main','q2','r2','Run x',1,2,'block','wake')";
        store.connection().execute_batch(block).unwrap();
        let kept: Vec<(String, String)> = store
            .connection()
            .prepare("SELECT id, source FROM captain_approvals ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            kept,
            [("a1".into(), "reply".into()), ("a2".into(), "block".into())]
        );
        // Opened again, it is not rebuilt.
        drop(store);
        Store::open(&home).unwrap();
    }

    #[test]
    fn a_received_copy_records_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("home")).unwrap();
        let request = json!({"id": "r1", "from": "apps", "kind": "request", "delivery": "received",
            "task_id": "t", "payload": {"approves": {"action": "Submit build 55", "ttl_secs": 60}}});
        let outcome = record(
            &store,
            &request,
            "reply-1",
            "yes",
            &json!({"operator": true}),
            1,
        );
        assert_eq!(outcome.unwrap(), None);
        let count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
