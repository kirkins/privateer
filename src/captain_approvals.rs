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
use rusqlite::{Connection, OptionalExtension, params};
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
    "nice", "time", "watch", "ssh", "coproc", "!",
];
/// Characters that make a shell line more than one simple command.
const METACHARACTERS: &str = ";&|<>()$`\\";
/// Characters bash quotes, globs, expands or comments with, so the word it
/// runs is not the word written.
const EXPANSIONS: &str = "'\"*?[]{}~#";
/// The most words and characters of a command a prefix is matched against.
const MAX_HEAD_WORDS: usize = 16;
const MAX_HEAD_CHARS: usize = 200;
/// How recent an approval or a reply must be to act on in one tap.
const ONE_TAP_MS: i64 = 86_400_000;
/// Shells and interpreters, refused alone or running inline code.
const INTERPRETERS: &[&str] = &[
    "bash", "sh", "zsh", "dash", "ksh", "fish", "node", "perl", "ruby",
];
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
/// None turns a yes into a question or a no: "you good", "that ok" and
/// "this fine" ask, and "y" reads as "why", so those are left out. "you",
/// "it" and "for" are plain only where `plain_run` allows them.
const PLAIN_WORDS: &[&str] = &[
    "yes", "yep", "yeah", "ok", "okay", "k", "sure", "go", "ahead", "do", "please", "approve",
    "approved", "proceed", "lgtm", "ship", "thanks", "thank", "the", "fine", "good", "now",
];
/// The words of which a free-text reply needs one, unless it retypes the
/// whole action.
const YES_WORDS: &[&str] = &[
    "yes", "yep", "yeah", "ok", "okay", "k", "sure", "go", "approve", "approved", "proceed",
    "lgtm", "ship", "do", "fine", "good",
];
/// Words that, straight after "it", make a dropped-"is" question of it.
const ASKS_AFTER_IT: &[&str] = &["ok", "okay", "k", "sure", "fine", "good"];
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

/// The words bash runs for a line holding one simple command, split as bash
/// splits them, on spaces and tabs. None when the line holds a shell
/// operator, redirection, quoting, glob, expansion or comment, or anything
/// but printable ASCII, spaces and tabs: blank, invisible and lookalike
/// characters are too many to list, so no word written is a different
/// word run or a word the captain did not see.
fn shell_words(line: &str) -> Option<Vec<&str>> {
    let plain = line.chars().all(|c| {
        (c.is_ascii_graphic() || c == ' ' || c == '\t')
            && !METACHARACTERS.contains(c)
            && !EXPANSIONS.contains(c)
    });
    plain.then(|| line.split([' ', '\t']).filter(|w| !w.is_empty()).collect())
}

/// Whether an interpreter's argument makes it run code written inline
/// rather than a script: `-c`, `-e` or `--eval`, alone or in a flag cluster
/// (`bash -lc`, `python3 -Sc`, `node -pe`), node's `-p` or `--print`, and
/// python's `-m`.
fn inline_code(program: &str, word: &str) -> bool {
    if matches!(word, "--eval" | "--print" | "--command") {
        return true;
    }
    let Some(flags) = word.strip_prefix('-').filter(|f| !f.starts_with('-')) else {
        return false;
    };
    let letters: String = flags
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    letters.contains(['c', 'e'])
        || (program == "node" && letters.contains('p'))
        || (program.starts_with("python") && letters.contains('m'))
}

pub(crate) fn check_prefix(prefix: &str) -> Result<()> {
    let Some(words) = shell_words(prefix) else {
        anyhow::bail!(
            "approves.command_prefix must be plain words of one simple command, separated by spaces, without ; & | < > ( ) $ ` \\ ' \" * ? [ ] {{ }} ~ #"
        )
    };
    let first = words.first().copied().unwrap_or_default();
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
        // The script comes first: an option before it (`bash -o errexit`,
        // `perl -E`, `python3 -W ignore`) leaves room for inline code after.
        let rest = &words[1..];
        ensure!(
            rest.first().is_some_and(|w| !w.starts_with('-'))
                && !rest.iter().any(|w| inline_code(program, w)),
            "approves.command_prefix cannot be just a shell or interpreter, or give it options before its script; name the script it runs first"
        );
    }
    Ok(())
}

/// The words of a Bash command a prefix is matched against, when it is one
/// simple command whose words are the words bash runs (see `shell_words`):
/// a program first rather than an assignment, at most 16 words and 200
/// characters, and nothing a card would mask or that looks like a key.
pub(crate) fn command_head(command: &str) -> Option<Vec<String>> {
    let words = shell_words(command)?;
    (command.trim_matches([' ', '\t']).chars().count() <= MAX_HEAD_CHARS
        && crate::block_ask::mask(command) == command
        && (1..=MAX_HEAD_WORDS).contains(&words.len())
        && !words[0].contains('=')
        && !words.iter().any(|w| crate::block_ask::secret_shaped(w)))
    .then(|| words.into_iter().map(str::to_owned).collect())
}

/// Whether a command prefix covers a command's head: its words are the
/// head's first words, each exactly, so `make release` covers `make release
/// --dry-run` but never `make releasex` or `sudo make release`.
pub(crate) fn covers(prefix: &str, head: &[String]) -> bool {
    shell_words(prefix).is_some_and(|words| {
        !words.is_empty()
            && words.len() <= head.len()
            && words.iter().zip(head).all(|(p, h)| p == h)
    })
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

/// Whether every word of `run` is plain. "you" is plain only right after
/// "thank" ("yes thank you", never "you good" or "do you"), and "it" only
/// right after a yes word ("do it", "ship it"; never "it ok", which asks)
/// or in "go for it", the only place "for" is plain. Each word is judged
/// by its neighbours inside `run` alone, so the words of an action never
/// make the plain words beside it plain.
fn plain_run(run: &[String]) -> bool {
    let at = |i: usize| run.get(i).map(String::as_str);
    (0..run.len()).all(|i| {
        let before = i.checked_sub(1).and_then(at);
        match run[i].as_str() {
            "you" => before == Some("thank"),
            "for" => before == Some("go") && at(i + 1) == Some("it"),
            // "sure it ok" drops an "is" and asks, as "it ok" does.
            "it" => {
                before.is_some_and(|w| w == "for" || YES_WORDS.contains(&w))
                    && !at(i + 1).is_some_and(|w| ASKS_AFTER_IT.contains(&w))
            }
            word => PLAIN_WORDS.contains(&word),
        }
    })
}

/// Whether `said` holds `own` in full and in order, its plain words
/// included, as one unbroken run with nothing but plain words before and
/// after it.
fn retyped(said: &[String], own: &[String]) -> bool {
    let Some(last) = said.len().checked_sub(own.len()) else {
        return false;
    };
    (0..=last).any(|lead| {
        let tail = lead + own.len();
        said[lead..tail] == own[..] && plain_run(&said[..lead]) && plain_run(&said[tail..])
    })
}

/// Whether a free-text reply that passed `hedged` is a plain yes to
/// `action`: either only plain words, one of them a yes word, or the
/// action retyped (`retyped`): "ok, Deploy build 55 to staging and prod,
/// thanks". Any shorter reply approves nothing, since a plain word can be a
/// verb: "merge PR 12" of "Merge PR 12 ship to prod", "live" of "Go live on
/// prod" or "Tag v1.2 to production" of "Tag v1.2 ship to production".
fn plain_yes(text: &str, action: &str) -> bool {
    let said = words(text);
    if plain_run(&said) {
        return said.iter().any(|w| YES_WORDS.contains(&w.as_str()));
    }
    retyped(&said, &words(action))
}

/// Whether the text sent with a chosen approving option holds only plain
/// words, or the option's label retyped (`retyped`), its plain words
/// included; the tap is the yes, so a part of the label ("prod" beside
/// "Deploy to staging and prod", "13" beside "Merge PR 12 and 13", "Tag
/// v1.2 to production" beside "Tag v1.2 ship to production") would narrow
/// it.
fn plain_beside(text: &str, label: &str) -> bool {
    let said = words(text);
    plain_run(&said) || retyped(&said, &words(label))
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
/// a cross or a check mark beside a word could reverse it. Every combining
/// mark counts (a strike or a slash drawn over a word), as do a tilde (a
/// strike in chat markup) and the letters that look like punctuation (`ǀ`,
/// `ǃ`, `ʻ`, `ˈ`).
fn pictograph(c: char) -> bool {
    matches!(u32::from(c),
        0x7e | 0xa9 | 0xac | 0xae | 0xd7 | 0x200d | 0x203c | 0x2e38 | 0x3030 | 0x303d | 0xff5e
        | 0x1c0..=0x1c3
        | 0x2b9..=0x2ff
        | 0x300..=0x36f
        | 0xfbe..=0xfbf
        | 0x1ab0..=0x1aff
        | 0x1dc0..=0x1dff
        | 0x2020..=0x2021
        | 0x20d0..=0x20ff
        | 0x2100..=0x214f
        | 0x2190..=0x2bff
        | 0x3200..=0x32ff
        | 0xfe00..=0xfe0f
        | 0xfe20..=0xfe2f
        | 0x10102
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
    door_expiry: Option<i64>,
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
    let ttl = step_ttl(approves);
    if action.is_empty() {
        return skip("the request names no action, so nothing was approved");
    }
    let mut result = insert(
        store,
        &Grant {
            ship,
            task,
            request,
            reply_id,
            action,
            prefix: approves["command_prefix"].as_str(),
            ttl,
            expires_ms: door_expiry,
            chat_door: payload["chat_door"] == true,
            source,
            renews: None,
        },
        now,
    )?;
    if payload["chat_door"] == true
        && let Some(record) = &mut result
    {
        record["answered_via"] = json!("chat-door");
        record["provenance"] = payload["provenance"].clone();
    }
    Ok(result)
}

/// The effective lifetime of an ordinary declared step approval, shared by
/// native answers and the grant displayed before door consent.
pub(crate) fn step_ttl(approves: &Value) -> u64 {
    approves["ttl_secs"]
        .as_u64()
        .unwrap_or(DEFAULT_TTL_SECS)
        .clamp(1, MAX_TTL_SECS)
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
    /// Only a consumed door challenge fixes expiry before the native reply.
    expires_ms: Option<i64>,
    source: &'a str,
    chat_door: bool,
    /// The expired approval a block card's tap renews.
    renews: Option<&'a str>,
}

/// Native admission checks shared with door preparation. Call again in the
/// answer's write transaction: capacity may change after preparation.
pub(crate) fn admission(
    store: &Store,
    ship: &str,
    sentence: &str,
    now: i64,
) -> Result<Option<String>> {
    if live(store, ship, now)?.len() >= MAX_LIVE {
        return Ok(Some(format!(
            "{ship} already holds {MAX_LIVE} live captain approvals, so nothing was approved"
        )));
    }
    if sentence.chars().count() > MAX_SENTENCE_CHARS {
        return Ok(Some(
            "sentence too long for auto mode with this reply id, so nothing was approved; ask again"
                .to_owned(),
        ));
    }
    Ok(None)
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
        expires_ms,
        source,
        chat_door,
        renews,
    } = *grant;
    let skip = |why: &str| skipped(store, ship, request, reply_id, why);
    let id = format!("approval-{}", uuid::Uuid::new_v4().simple());
    let expires_ms = expires_ms.unwrap_or(now + ttl as i64 * 1000);
    let request_id = request["id"].as_str().unwrap_or_default();
    let render = if chat_door { chat_sentence } else { sentence };
    let told = render(
        reply_id,
        request_id,
        ship,
        task,
        &crate::routes::utc_time(expires_ms),
        action,
        prefix,
    );
    if let Some(why) = admission(store, ship, &told, now)? {
        return skip(&why);
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
    if chat_door {
        audit["answered_via"] = json!("chat-door");
    }
    if source == "block" {
        audit["ask"] = request["id"].clone();
        audit["command_prefix"] = json!(prefix);
    }
    if let Some(old) = renews {
        audit["renews"] = json!(old);
    }
    store.audit("permissions.approval_recorded", &audit)?;
    let mut result = json!({"recorded": true, "id": id, "action": action,
        "expires_ms": expires_ms, "applies": applies, "note": note(applies, action)});
    if chat_door {
        result["sentence"] = json!(told);
    }
    Ok(Some(result))
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
                expires_ms: None,
                chat_door: false,
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
    // Live or lapsed, an approval given more than 24 h ago is asked again.
    if now - created_ms >= ONE_TAP_MS {
        return skip("the approval is older than 24 h, so nothing was approved; ask again");
    }
    if expires_ms > now {
        // A legacy row may still be live but unsafe to tell auto mode. Never
        // claim coverage unless the next launch would actually carry it.
        if !current_set(store, ship, now)?
            .iter()
            .any(|(live_id, _)| live_id == id)
        {
            return skip(
                "the approval cannot be carried at launch, so nothing was approved; ask again",
            );
        }
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
            expires_ms: None,
            chat_door: false,
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn offer(
    store: &Store,
    ship: &str,
    task: &str,
    request: &str,
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
    // Share launch's carry check: a live legacy row may be stored but skipped
    // by told(). Neither offer it nor claim that auto mode ignored it.
    let current = current_set(store, ship, now)?;
    let can_carry = |row: &Row| current.iter().any(|(id, _)| id == &row.id);
    // Auto mode refused this with a covering approval in its session.
    let ignored = live(store, ship, now)?.into_iter().rev().find(|row| {
        known && in_session(row) && can_carry(row) && covers_all(row.prefix.as_deref())
    });
    let resume = if scope == "pane" {
        "The session restarts at its next idle point with it; nothing to relaunch."
    } else {
        "The ship's next wake carries it; nothing to relaunch."
    };
    let mut recommended = ignored.is_some();
    for row in recent(store, ship, now)?
        .into_iter()
        .filter(|row| known && !in_session(row) && (row.expires_ms <= now || can_carry(row)))
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
    if let Some((action, prefix)) = step(store, ship, task, request, records, &heads, now)? {
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

/// The exact step a block card offers to approve: "Run" and its refused
/// commands, and, for one command, its words as the prefix. None when the
/// card shows any command less than whole (clipped, cut at a heredoc,
/// flattened or masked), so the captain never approves what they did not
/// see; when any command is more than one simple command or its words would
/// not pass as a prefix, so a quoted or wrapped program or an interpreter's
/// inline code is never one tap away; or when the step would not pass as a
/// ship's own `approves` would.
#[allow(clippy::too_many_arguments)]
fn step(
    store: &Store,
    ship: &str,
    task: &str,
    request: &str,
    records: &[Value],
    heads: &[Option<Vec<String>>],
    now: i64,
) -> Result<Option<(String, Option<String>)>> {
    if !records.iter().all(|d| d["whole"] == true) {
        return Ok(None);
    }
    let previews: Vec<String> = records
        .iter()
        .map(|d| format!("`{}`", d["preview"].as_str().unwrap_or_default()))
        .collect();
    let action = format!("Run {}", previews.join("; "));
    if action.chars().count() > MAX_ACTION_CHARS
        || action.chars().any(char::is_control)
        || lint(store, ship, "the step", &action).is_err()
    {
        return Ok(None);
    }
    let mut prefixes = vec![];
    for head in heads {
        let Some(head) = head else {
            return Ok(None);
        };
        let prefix = head.join(" ");
        if prefix.chars().count() > MAX_PREFIX_CHARS
            || check_prefix(&prefix).is_err()
            || lint(store, ship, "the prefix", &prefix).is_err()
        {
            return Ok(None);
        }
        prefixes.push(prefix);
    }
    let prefix = match prefixes.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    };
    // Check the complete sentence with a minted reply's full length, not
    // just the independent action/prefix bounds. Explicitly longer reply ids
    // remain subject to insertion-time admission.
    let text = sentence(
        &"r".repeat(MINTED_REPLY_CHARS),
        request,
        ship,
        task,
        LONGEST_UTC,
        &action,
        prefix.as_deref(),
    );
    if admission(store, ship, &text, now)?.is_some() {
        return Ok(None);
    }
    Ok(Some((action, prefix)))
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
    ship: String,
    task: String,
    request: String,
    reply: String,
    pub(crate) action: String,
    pub(crate) prefix: Option<String>,
    pub(crate) created_ms: i64,
    pub(crate) expires_ms: i64,
    revoked_ms: Option<i64>,
    pub(crate) source: String,
}

/// Whether `ship` owns `task` and it is open under an active charter.
pub(crate) fn open_task(store: &Store, ship: &str, task: &str) -> bool {
    task_open(store.connection(), ship, task)
}

/// `open_task` read from the tables alone, so a read-only open sees what the
/// runtime sees: the ship owns the task, it is not done or cancelled, no
/// task above it is cancelled, and its charter is active. Anything unreadable
/// is not open.
fn task_open(conn: &Connection, ship: &str, task: &str) -> bool {
    let read = |id: &str| {
        conn.query_row(
            "SELECT owner,status,parent,charter_id FROM tasks WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
    };
    let Ok(Some((owner, status, mut parent, charter))) = read(task) else {
        return false;
    };
    if owner != ship || matches!(status.as_str(), "done" | "cancelled") {
        return false;
    }
    // Tasks nest a few levels; the bound only stops a corrupt cycle.
    for _ in 0..1000 {
        let Some(id) = parent else {
            let data: Option<String> = conn
                .query_row("SELECT data FROM charters WHERE id=?1", [&charter], |r| {
                    r.get(0)
                })
                .optional()
                .ok()
                .flatten();
            return data
                .and_then(|d| serde_json::from_str::<Value>(&d).ok())
                .is_some_and(|c| c["active"] == true);
        };
        let Ok(Some((_, status, next, _))) = read(&id) else {
            return false;
        };
        if status == "cancelled" {
            return false;
        }
        parent = next;
    }
    false
}

/// Where an approval stands at `now`. This is the one rule launches, list,
/// show and the dashboard read: `revoked` once the captain revokes it,
/// `closed` once its task is done, cancelled or handed to another owner or
/// its charter is inactive, `expired` past its expiry, and otherwise `live`.
pub(crate) fn state(conn: &Connection, row: &Row, now: i64) -> &'static str {
    if row.revoked_ms.is_some() {
        "revoked"
    } else if !task_open(conn, &row.ship, &row.task) {
        "closed"
    } else if row.expires_ms <= now {
        "expired"
    } else {
        "live"
    }
}

/// The approvals matching `condition` (SQL on `captain_approvals`), in
/// `order`.
fn select(
    conn: &Connection,
    condition: &str,
    args: impl rusqlite::Params,
    order: &str,
) -> Result<Vec<Row>> {
    let mut statement = conn.prepare(&format!(
        "SELECT id,ship,task_id,request_id,reply_id,action,command_prefix,created_ms,expires_ms,
           revoked_ms,source
         FROM captain_approvals WHERE {condition} ORDER BY {order}"
    ))?;
    let rows = statement
        .query_map(args, |r| {
            Ok(Row {
                id: r.get(0)?,
                ship: r.get(1)?,
                task: r.get(2)?,
                request: r.get(3)?,
                reply: r.get(4)?,
                action: r.get(5)?,
                prefix: r.get(6)?,
                created_ms: r.get(7)?,
                expires_ms: r.get(8)?,
                revoked_ms: r.get(9)?,
                source: r.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// A ship's live approvals: not revoked, not expired, and on a task the ship
/// still owns, open under an active charter.
pub(crate) fn live(store: &Store, ship: &str, now: i64) -> Result<Vec<Row>> {
    live_in(store.connection(), ship, now)
}

/// `live` on a plain connection, such as the dashboard's read-only one.
pub(crate) fn live_in(conn: &Connection, ship: &str, now: i64) -> Result<Vec<Row>> {
    Ok(select(
        conn,
        "ship=?1 AND revoked_ms IS NULL AND expires_ms>?2",
        params![ship, now],
        "created_ms, id",
    )?
    .into_iter()
    .filter(|row| state(conn, row, now) == "live")
    .collect())
}

/// A ship's live approvals read from a home no runtime opened: one from
/// before captain approvals has none, and nothing is created or migrated.
#[cfg_attr(not(feature = "dashboard"), allow(dead_code))]
pub(crate) fn live_read(conn: &Connection, ship: &str, now: i64) -> Result<Vec<Row>> {
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='captain_approvals')",
        [],
        |r| r.get(0),
    )?;
    if !present {
        return Ok(vec![]);
    }
    live_in(conn, ship, now)
}

/// The approvals a block card may offer to use again in one tap: created in
/// the last 24 h, not revoked, on a task the ship still owns and is open,
/// newest first. An expired one is renewed by the tap.
pub(crate) fn recent(store: &Store, ship: &str, now: i64) -> Result<Vec<Row>> {
    let conn = store.connection();
    Ok(select(
        conn,
        "ship=?1 AND revoked_ms IS NULL AND created_ms>?2",
        params![ship, now - ONE_TAP_MS],
        "created_ms DESC, id",
    )?
    .into_iter()
    .filter(|row| matches!(state(conn, row, now), "live" | "expired"))
    .collect())
}

/// The one sentence the classifier reads for an approval. Never omit a command
/// prefix to meet the policy's length bound: admission must refuse an oversized
/// sentence, and launch reads must skip it, rather than widen the grant.
pub(crate) fn sentence(
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
        Some(prefix) => format!("{plain} Only commands that begin exactly with: {prefix}"),
        None => plain,
    }
}

/// The exact sentence displayed by chat prepare and carried into bearings and
/// auto mode. Keep the marker in the head, outside the action/prefix clauses.
/// It participates in native sentence-length admission.
pub(crate) fn chat_sentence(
    reply: &str,
    request: &str,
    ship: &str,
    task: &str,
    until: &str,
    action: &str,
    prefix: Option<&str>,
) -> String {
    sentence(reply, request, ship, task, until, action, prefix).replacen(
        "The captain approved this in reply ",
        "The captain approved this via the chat door in reply ",
        1,
    )
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
        let text = match stored_chat_sentence(store.connection(), &row.id)? {
            Some(text) => {
                // Older renderers could persist a sentence without its prefix
                // clause. Do not repair consent after the fact or launch it as
                // an unscoped grant: the exact stored text must be safe as-is.
                if row.prefix.is_some()
                    && text
                        != chat_sentence(
                            &row.reply,
                            &row.request,
                            ship,
                            &row.task,
                            &crate::routes::utc_time(row.expires_ms),
                            &row.action,
                            row.prefix.as_deref(),
                        )
                {
                    if audit {
                        store.audit(
                            "permissions.approval_skipped",
                            &json!({"id": row.id, "ship": ship, "why": "stored chat sentence does not match scoped approval"}),
                        )?;
                    }
                    continue;
                }
                text
            }
            None => sentence(
                &row.reply,
                &row.request,
                ship,
                &row.task,
                &crate::routes::utc_time(row.expires_ms),
                &row.action,
                row.prefix.as_deref(),
            ),
        };
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

/// Time left, coarse: "3 h 20 min", "12 min".
pub(crate) fn left(ms: i64) -> String {
    let minutes = (ms / 60_000).max(1);
    if minutes >= 60 {
        format!("{} h {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    }
}

// Persist exactly what prepare showed, atomically with the approval. Provenance
// alone is not a marker: older chat approvals must not be relabeled after consent.
// Reading an older home neither creates a table nor migrates its sentences.
fn stored_chat_sentence(conn: &Connection, id: &str) -> Result<Option<String>> {
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='captain_approval_door_sentences')",
        [], |r| r.get(0),
    )?;
    if !present {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT sentence FROM captain_approval_door_sentences WHERE approval_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Bearings lines for a ship's live approvals, and how to ask for one.
pub(crate) fn bearings_lines(store: &Store, ship: &str) -> Result<Vec<String>> {
    let now = Store::now_ms();
    let mut lines: Vec<String> = live(store, ship, now)?
        .into_iter()
        .map(|row| {
            let suffix = if stored_chat_sentence(store.connection(), &row.id)?.is_some() {
                " (via chat door)"
            } else {
                ""
            };
            Ok(format!(
                "Captain approval {} ({} left): {}{suffix}",
                row.id,
                left(row.expires_ms - now),
                row.action
            ))
        })
        .collect::<Result<_>>()?;
    lines.push(
        "Free-text replies count only to requests that carry approves; ask with approves when you need a step approved."
            .into(),
    );
    Ok(lines)
}

/// The ship a list or show may read: the operator any, or every ship when
/// none is named; an agent only its own, named.
fn reader_ship<'a>(store: &Store, ship: Option<&'a str>) -> Result<Option<&'a str>> {
    match store.caller() {
        Caller::Operator => Ok(ship),
        Caller::Agent { actor } => {
            let ship = ship.with_context(|| {
                format!("an agent lists only its own captain approvals; pass ship {actor}")
            })?;
            ensure!(
                ship == actor,
                "agent {actor} cannot read {ship}'s captain approvals"
            );
            Ok(Some(ship))
        }
    }
}

/// Rows as list and show give them: each with its `state` and, when live,
/// the `sentence` a launch now tells auto mode.
fn described(store: &Store, rows: &[Row], now: i64) -> Result<Vec<Value>> {
    let conn = store.connection();
    let mut told: std::collections::BTreeMap<&str, Vec<(String, String)>> = Default::default();
    let mut out = vec![];
    // Older homes need no observation-time migration. Check once per batch,
    // rather than querying sqlite_master for every approval row.
    let has_provenance: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='captain_approval_door_provenance')",
        [], |r| r.get(0))?;
    for row in rows {
        let state = state(conn, row, now);
        let sentence = if state == "live" {
            let set = match told.entry(row.ship.as_str()) {
                std::collections::btree_map::Entry::Occupied(set) => set.into_mut(),
                std::collections::btree_map::Entry::Vacant(set) => {
                    set.insert(current_set(store, &row.ship, now)?)
                }
            };
            set.iter()
                .find(|(id, _)| *id == row.id)
                .map(|(_, sentence)| sentence.clone())
        } else {
            None
        };
        out.push(json!({"id": row.id, "ship": row.ship, "task_id": row.task,
            "request_id": row.request, "reply_id": row.reply, "action": row.action,
            "command_prefix": row.prefix, "created_ms": row.created_ms,
            "expires_ms": row.expires_ms, "revoked_ms": row.revoked_ms, "source": row.source,
            "live": state == "live", "state": state, "sentence": sentence}));
        // A companion native row retains core-owned provenance even after
        // reply/history pruning.
        if has_provenance {
            let provenance: Option<String> = conn
                .query_row(
                    "SELECT provenance FROM captain_approval_door_provenance WHERE approval_id=?1",
                    [&row.id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(provenance) = provenance {
                let record = out.last_mut().expect("row just inserted");
                record["answered_via"] = json!("chat-door");
                record["provenance"] = serde_json::from_str(&provenance)?;
            }
        }
    }
    Ok(out)
}

fn list(store: &Store, args: &Value) -> Result<Value> {
    let named = match args.get("ship").filter(|v| !v.is_null()) {
        Some(_) => Some(required(args, "ship")?),
        None => None,
    };
    let ship = reader_ship(store, named)?;
    if let Some(ship) = ship {
        crate::store::identifier(ship)?;
    }
    let live_only = match args.get("live_only").filter(|v| !v.is_null()) {
        Some(flag) => flag.as_bool().context("live_only must be true or false")?,
        None => false,
    };
    let now = Store::now_ms();
    let order = "created_ms DESC, id LIMIT 100";
    let rows = if live_only {
        select(
            store.connection(),
            "(?1 IS NULL OR ship=?1) AND revoked_ms IS NULL AND expires_ms>?2",
            params![ship, now],
            order,
        )?
    } else {
        select(store.connection(), "(?1 IS NULL OR ship=?1)", [ship], order)?
    };
    let approvals: Vec<Value> = described(store, &rows, now)?
        .into_iter()
        .filter(|row| !live_only || row["state"] == "live")
        .collect();
    Ok(json!({"ship": ship, "approvals": approvals}))
}

fn show(store: &Store, args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let row = select(store.connection(), "id=?1", [id], "id")?
        .into_iter()
        .next()
        .with_context(|| format!("no captain approval {id}"))?;
    if let Caller::Agent { actor } = store.caller() {
        ensure!(*actor == row.ship, "captain approval {id} is not yours");
    }
    let now = Store::now_ms();
    let mut shown = described(store, std::slice::from_ref(&row), now)?.remove(0);
    let conn = store.connection();
    shown["applies_at"] = json!(conn.query_row(
        "SELECT applies_at FROM captain_approvals WHERE id=?1",
        [id],
        |r| r.get::<_, Option<String>>(0)
    )?);
    let mut carried = vec![];
    let mut statement = conn.prepare(
        "SELECT pid, recorded_ms, approvals_ids FROM hosted_panes WHERE ship=?1 ORDER BY pid",
    )?;
    let panes = statement
        .query_map([&row.ship], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (pid, launched_ms, ids) in panes {
        let ids: Vec<String> = ids
            .and_then(|ids| serde_json::from_str(&ids).ok())
            .unwrap_or_default();
        if ids.iter().any(|carried| carried == id) {
            carried.push(json!({"pid": pid, "launched_ms": launched_ms}));
        }
    }
    shown["carried_by"] = json!(carried);
    Ok(shown)
}

/// When a revoke reaches the ship's session.
fn revoke_note(applies: &str, ship: &str) -> String {
    match applies {
        "relaunch" => format!("stops at {ship}'s next idle point"),
        "wake" => "next wake no longer carries it".to_owned(),
        _ => "next launch no longer carries it".to_owned(),
    }
}

fn revoke(store: &Store, args: &Value) -> Result<Value> {
    store.caller().require_operator("captain_approval.revoke")?;
    let id = required(args, "id")?;
    let row: Option<(String, Option<i64>)> = store
        .connection()
        .query_row(
            "SELECT ship, revoked_ms FROM captain_approvals WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((ship, revoked)) = row else {
        bail!("no captain approval {id}");
    };
    if let Some(revoked_ms) = revoked {
        return Ok(json!({"id": id, "ship": ship, "revoked_ms": revoked_ms, "already": true}));
    }
    let now = Store::now_ms();
    store.connection().execute(
        "UPDATE captain_approvals SET revoked_ms=?2 WHERE id=?1 AND revoked_ms IS NULL",
        params![id, now],
    )?;
    store.audit("permissions.approval_revoked", &json!({"id": id}))?;
    let applies = crate::host::upgrade::approval_applies(store, &ship)?;
    Ok(
        json!({"id": id, "ship": ship, "revoked_ms": now, "already": false,
        "applies": applies, "note": revoke_note(applies, &ship)}),
    )
}

/// The argument keys of each command here; see `crate::Accepted`.
pub(crate) const ACCEPTED: crate::Accepted = &[
    ("captain_approval.list", &["ship", "live_only"]),
    ("captain_approval.revoke", &["id"]),
    ("captain_approval.show", &["id"]),
];

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "captain_approval.list" => list(store, &args),
        "captain_approval.show" => show(store, &args),
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

const USAGE: &str = "usage: privateer permissions approvals [list [SHIP] | show ID | revoke ID]";

/// The captain's terminal verbs: `privateer permissions approvals [list
/// [SHIP] | show ID | revoke ID]`, printing what the JSON commands answer.
pub fn cli(home: Option<std::path::PathBuf>, args: &[String], caller: &Caller) -> Result<()> {
    let (command, request) = match args {
        [] => ("captain_approval.list", json!({})),
        [verb] if verb == "list" => ("captain_approval.list", json!({})),
        [verb, ship] if verb == "list" => ("captain_approval.list", json!({"ship": ship})),
        [verb, id] if verb == "show" => ("captain_approval.show", json!({"id": id})),
        [verb, id] if verb == "revoke" => ("captain_approval.revoke", json!({"id": id})),
        _ => bail!("{USAGE}"),
    };
    if command == "captain_approval.revoke" {
        caller.require_operator(command)?;
    }
    let home = home.context(
        "not inside a ship folder (no .privateer here or above); run `privateer init`, or pass --home",
    )?;
    let mut store = Store::open_as(&home, caller.clone())?;
    let result = crate::dispatch(&mut store, command, request)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_marker_is_explicit_and_native_sentence_is_unchanged() {
        let native = sentence(
            "reply",
            "ask",
            "alpha",
            "task",
            "later",
            "Submit build 55",
            Some("make release"),
        );
        assert_eq!(
            native,
            "The captain approved this in reply reply to request ask, for ship alpha only, on task task, until later: Submit build 55 Only commands that begin exactly with: make release"
        );
        let chat = chat_sentence(
            "reply",
            "ask",
            "alpha",
            "task",
            "later",
            "Submit build 55",
            Some("make release"),
        );
        assert!(chat.starts_with("The captain approved this via the chat door in reply "));
        // The action and command-prefix clauses stay byte-identical to native.
        assert_eq!(
            chat.split_once("until ").unwrap().1,
            native.split_once("until ").unwrap().1
        );
        let clause = " Only commands that begin exactly with: ";
        assert_eq!(
            chat.split_once(clause).unwrap().1,
            native.split_once(clause).unwrap().1
        );
    }

    fn approval_test_store(task: &str) -> (tempfile::TempDir, Store) {
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
            json!({"id":task,"charter_id":"project","owner":"alpha","title":"Deliver"}),
        )
        .unwrap();
        (temp, store)
    }

    #[test]
    fn legacy_scoped_chat_sentences_and_oversized_rows_never_launch_or_claim_coverage() {
        let (_temp, store) = approval_test_store("task");
        let now = Store::now_ms();
        let expires = now + 3_600_000;
        store.connection().execute_batch(
            "CREATE TABLE captain_approval_door_sentences(
                approval_id TEXT PRIMARY KEY REFERENCES captain_approvals(id), sentence TEXT NOT NULL);",
        ).unwrap();
        for id in ["legacy", "correct", "oversized"] {
            let action = if id == "oversized" {
                "x".repeat(600)
            } else {
                "Run make release".into()
            };
            store
                .connection()
                .execute(
                    "INSERT INTO captain_approvals(id,ship,task_id,request_id,reply_id,action,
                    command_prefix,created_ms,expires_ms,source)
                 VALUES(?1,'alpha','task','ask-'||?1,'reply-'||?1,?2,'make release',?3,?4,'reply')",
                    params![id, action, now - 60_000, expires],
                )
                .unwrap();
            if id != "oversized" {
                // Simulate a #565-era stored sentence that omitted the clause,
                // alongside a correctly persisted scoped chat sentence.
                let text = chat_sentence(
                    &format!("reply-{id}"),
                    &format!("ask-{id}"),
                    "alpha",
                    "task",
                    &crate::routes::utc_time(expires),
                    &action,
                    (id == "correct").then_some("make release"),
                );
                store
                    .connection()
                    .execute(
                        "INSERT INTO captain_approval_door_sentences VALUES(?1,?2)",
                        params![id, text],
                    )
                    .unwrap();
            }
        }
        let read = current_set(&store, "alpha", now).unwrap();
        let launch = launch_set(&store, "alpha", now).unwrap();
        assert_eq!(read, launch);
        assert_eq!(launch.len(), 1, "{launch:?}");
        assert_eq!(launch[0].0, "correct");
        assert!(
            launch[0]
                .1
                .ends_with(" Only commands that begin exactly with: make release")
        );
        let stored = stored_chat_sentence(store.connection(), "correct")
            .unwrap()
            .unwrap();
        assert_eq!(launch[0].1, stored);
        let audited: bool = store.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM audit WHERE kind='permissions.approval_skipped'
             AND json_extract(payload,'$.id')='legacy'
             AND json_extract(payload,'$.why')='stored chat sentence does not match scoped approval')",
            [], |r| r.get(0),
        ).unwrap();
        assert!(audited);

        let records = [json!({"tool":"Bash","classifier":true,"whole":true,
            "preview":"make release","head":["make","release"]})];
        let offered = offer(
            &store,
            "alpha",
            "task",
            "block-ask",
            "pane",
            None,
            &records,
            0,
            now,
        )
        .unwrap();
        assert_eq!(offered.approvals, vec!["correct"]);
        assert_eq!(offered.options[0]["id"], "as_approved_1");
        assert_eq!(offered.options[0]["recommended"], true);
        assert!(offered.why.is_none());

        // Even a previously offered card must recheck what will actually launch.
        for id in ["legacy", "oversized", "correct"] {
            let ask = json!({"id":"block-ask","from":"alpha","kind":"decision",
                "task_id":"task","state":"open","payload":{
                    "type":"tool_permission","block":{"approvals":[id]},
                    "decision":{"options":[{"id":"as_approved_1","label":"As approved"}]}}});
            let answer = record(
                &store,
                &ask,
                "tap",
                "",
                &json!({"operator":true,"answer":{"option_id":"as_approved_1"}}),
                now,
                None,
            )
            .unwrap()
            .unwrap();
            assert_eq!(answer["recorded"], false, "{answer}");
            if id == "correct" {
                assert_eq!(answer["covered_by"], id, "{answer}");
            } else {
                assert!(answer.get("covered_by").is_none(), "{answer}");
                assert!(answer.get("applies").is_none(), "{answer}");
                assert!(
                    answer["why"]
                        .as_str()
                        .unwrap()
                        .contains("cannot be carried at launch"),
                    "{answer}"
                );
            }
        }

        // A wake created after these approvals appears to have carried them.
        // Only a row that launch can actually carry may explain the refusal.
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id,record) VALUES('wake',?1)",
                [json!({"created_ms":now}).to_string()],
            )
            .unwrap();
        let offered = offer(
            &store,
            "alpha",
            "task",
            "block-ask",
            "wake",
            Some("wake"),
            &records,
            0,
            now,
        )
        .unwrap();
        assert!(offered.approvals.is_empty());
        assert!(
            offered
                .why
                .as_deref()
                .unwrap()
                .contains("approval correct ")
        );
        store
            .connection()
            .execute(
                "UPDATE captain_approvals SET revoked_ms=?1 WHERE id='correct'",
                [now],
            )
            .unwrap();
        let offered = offer(
            &store,
            "alpha",
            "task",
            "block-ask",
            "wake",
            Some("wake"),
            &records,
            0,
            now,
        )
        .unwrap();
        assert!(offered.approvals.is_empty());
        assert!(offered.why.is_none());
        assert_eq!(offered.options[0]["id"], "approve");
        assert_eq!(offered.options[0]["recommended"], true);
    }

    #[test]
    fn block_cards_still_offer_short_native_approvals_and_expired_renewals() {
        let (_temp, store) = approval_test_store("task");
        let now = Store::now_ms();
        for expires in [now + 3_600_000, now - 1] {
            store
                .connection()
                .execute(
                    "INSERT OR REPLACE INTO captain_approvals(id,ship,task_id,request_id,reply_id,
                    action,command_prefix,created_ms,expires_ms,source)
                 VALUES('ordinary','alpha','task','ask','reply','Run make release',
                    'make release',?1,?2,'reply')",
                    params![now - 60_000, expires],
                )
                .unwrap();
            let offered = offer(
                &store,
                "alpha",
                "task",
                "block-ask",
                "pane",
                None,
                &[json!({"tool":"Bash","classifier":true,"head":["make","release"]})],
                0,
                now,
            )
            .unwrap();
            assert_eq!(offered.approvals, vec!["ordinary"]);
            assert_eq!(offered.options[0]["id"], "as_approved_1");
        }
    }

    #[test]
    fn block_cards_offer_only_steps_whose_complete_sentence_passes_admission() {
        let task = format!("task-{}", "x".repeat(155));
        let (_temp, store) = approval_test_store(&task);
        let request = format!("permissions-{}", "0".repeat(64));
        let now = Store::now_ms();
        for (command, available) in [
            ("make release".to_owned(), true),
            (format!("make {}", "x".repeat(115)), false),
        ] {
            let head: Vec<&str> = command.split_whitespace().collect();
            let action = format!("Run `{command}`");
            assert!(action.chars().count() <= MAX_ACTION_CHARS);
            assert!(command.chars().count() <= MAX_PREFIX_CHARS);
            let text = sentence(
                &"r".repeat(MINTED_REPLY_CHARS),
                &request,
                "alpha",
                &task,
                LONGEST_UTC,
                &action,
                Some(&command),
            );
            assert_eq!(
                admission(&store, "alpha", &text, now).unwrap().is_none(),
                available
            );
            let offer = offer(&store, "alpha", &task, &request, "pane", None,
                &[json!({"tool":"Bash","classifier":true,"whole":true,"preview":command,"head":head})],
                0, now).unwrap();
            assert_eq!(offer.approve.is_some(), available);
            assert_eq!(
                offer.options.iter().any(|o| o["id"] == "approve"),
                available
            );
        }
    }

    #[test]
    fn declared_step_ttl_defaults_and_clamps_identically_for_native_and_door_answers() {
        for (approves, expected) in [
            (json!({}), DEFAULT_TTL_SECS),
            (json!({"ttl_secs":null}), DEFAULT_TTL_SECS),
            (json!({"ttl_secs":"invalid"}), DEFAULT_TTL_SECS),
            (json!({"ttl_secs":0}), 1),
            (json!({"ttl_secs":60}), 60),
            (json!({"ttl_secs":MAX_TTL_SECS + 1}), MAX_TTL_SECS),
        ] {
            assert_eq!(step_ttl(&approves), expected);
        }
    }

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
            "Submit build 55 to TestFlight",
            "yes",
            "ok do it",
            "Go ahead",
            "Approved",
            "lgtm",
            "yes please",
            "K",
        ] {
            assert!(plain_yes(yes, submit), "{yes}");
        }
        for loose in [
            "n",
            "Y",
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
            "ok, submit it",
            "submit it",
            "Submit build 55",
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
            "Deploy build 55 to staging and prod",
            "deploy build 55 to staging and prod, thanks",
            "yes",
        ] {
            assert!(plain_yes(yes, deploy), "{yes}");
        }
    }

    #[test]
    fn a_shorter_reply_is_no_yes() {
        // The opener of a two-step action approves neither step.
        let diff = "Show me the diff, then merge PR 12";
        for opener in [
            "show me the diff",
            "Show me the diff, thanks",
            "ok show me the diff",
            "show me",
        ] {
            assert!(!plain_yes(opener, diff), "{opener}");
        }
        assert!(plain_yes("show me the diff, then merge PR 12", diff));
        assert!(plain_yes("yes", diff));
        // Leaving out a step, a clause or even a destination approves
        // nothing: "and prod" and "and merge PR 12" read alike word by word.
        for (action, short) in [
            ("Submit build 55 to TestFlight", "submit it"),
            ("Submit build 55 to TestFlight", "ok, submit build"),
            ("Deploy build 55 to staging and prod", "deploy build"),
            ("Deploy build 55 to staging and prod", "Deploy build 55"),
            ("Deploy build 55 to staging and prod", "ok deploy"),
            (
                "Deploy build 55 to staging and merge PR 12",
                "Deploy build 55",
            ),
            ("Merge PR 340 and deploy to staging", "merge PR 340"),
            ("Merge PR 340, then deploy to staging", "merge PR 340"),
            (
                "Deploy build 55 to staging before wiping prod",
                "Deploy build 55",
            ),
            ("Deploy build 55 with a database reset", "Deploy build 55"),
            ("Merge PR 12 after deleting main", "Merge PR 12"),
            ("Deploy build 55 to staging, wipe prod", "Deploy build 55"),
            ("Deploy build 55 to staging; wipe prod", "Deploy build 55"),
            ("Deploy build 55 to staging - wipe prod", "Deploy build 55"),
            ("Deploy build 55 to staging & prod", "Deploy build 55"),
            ("Deploy build 55 to staging (wipe prod)", "Deploy build 55"),
            // A second step needs no scope word.
            (
                "Merge PR 12 into main deleting the release branch",
                "merge PR 12",
            ),
            ("Push the fix to main skipping CI", "push the fix"),
            (
                "Deploy build 55 to staging next merge PR 12",
                "Deploy build 55",
            ),
            ("Deploy build 55 to wipe prod", "Deploy build 55"),
            (
                "Deploy build 55 to staging as well as prod",
                "Deploy build 55",
            ),
            ("Deploy build 55 to staging 'n' prod", "Deploy build 55"),
            ("Deploy build 55 to staging / prod", "Deploy build 55"),
            ("Deploy build 55 to staging--wipe", "Deploy build 55"),
            ("Deploy build 55 to staging...wipe", "Deploy build 55"),
            ("Deploy build 55 to staging/-wipe", "Deploy build 55"),
            ("Deploy build 55 to staging'n'prod", "Deploy build 55"),
            ("Deploy build 55 to staging_and_prod", "Deploy build 55"),
            ("Deploy build 55 to staging-then-wipe", "Deploy build 55"),
            ("Deploy build 55 to staging+prod", "Deploy build 55"),
            ("Deploy build 55 to staging\u{200b}prod", "Deploy build 55"),
            ("Deploy build 55 to it", "Deploy build 55"),
            ("Deploy build 55 into", "Deploy build 55"),
            ("Merge PR #12 into main", "merge PR #12"),
            // A destination left out is a no too.
            ("Submit build 55 to TestFlight", "Submit build 55"),
            ("Submit build 55 to TestFlight.", "ok, submit build 55"),
            ("Deploy v1.2.3 to staging-eu", "deploy v1.2.3"),
            ("Merge PR 12 into main", "merge PR 12"),
            ("Push feature-x to origin/main", "push feature-x"),
            ("Run the migration on db:primary", "run the migration"),
            (
                "Submit build\u{a0}55 to\u{3000}TestFlight",
                "submit build 55",
            ),
            ("Submit build 55\u{2028}to TestFlight", "submit build 55"),
        ] {
            assert!(!plain_yes(short, action), "{short} / {action}");
            assert!(plain_yes(action, action), "{action}");
        }
        // Any Unicode space is a space in a full retype.
        assert!(plain_yes(
            "Submit build 55 to TestFlight",
            "Submit build\u{a0}55 to\u{3000}TestFlight"
        ));
    }

    #[test]
    fn only_a_bare_yes_or_the_whole_action_approves() {
        // A shorter reply is a no, whatever it leaves out: a plain word can
        // be a verb ("ship", "go", "do"), so the action is compared whole.
        let wrong: Vec<_> = [
            ("Merge PR 12 ship to prod", "merge PR 12"),
            ("Merge PR 12 go to prod", "merge PR 12"),
            ("Tag v1.2 ship to production", "tag v1.2"),
            ("Do the migration on db:primary", "the migration"),
            ("Go live on prod", "live"),
            ("Tag v1.2 ship to production", "Tag v1.2 to production"),
            ("Push the fix to delete", "push the fix"),
            ("Deploy build 55 to staging.wipe-prod", "Deploy build 55"),
            ("Submit build 55 to TestFlight", "Submit build 55"),
            ("Deploy build 55 to staging and prod", "Deploy build 55"),
            ("Merge PR 12 into main", "merge PR 12"),
            ("Show me the diff, then merge PR 12", "show me the diff"),
        ]
        .iter()
        .filter(|(action, short)| {
            plain_yes(short, action) || !plain_yes(action, action) || !plain_yes("yes", action)
        })
        .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
        // Plain words may sit around the whole action, never inside it.
        let deploy = "Deploy build 55 to staging and prod";
        for whole in [
            "ok, Deploy build 55 to staging and prod, thanks",
            "yes please: deploy build 55 to staging and prod",
            "Deploy build 55 to staging and prod now",
        ] {
            assert!(plain_yes(whole, deploy), "{whole}");
        }
        for broken in [
            "Deploy build 55 to staging ok and prod",
            "Deploy build 55 to staging and prod and wipe",
            "ok deploy build 55 to staging",
        ] {
            assert!(!plain_yes(broken, deploy), "{broken}");
        }
        // The action's own plain words are part of it.
        let migrate = "Do the migration on db:primary";
        assert!(plain_yes("ok, do the migration on db:primary", migrate));
        assert!(plain_yes("Go live on prod", "Go live on prod"));
        assert!(!plain_yes("go live", "Go live on prod"));
        // A bare yes still needs a yes word.
        assert!(!plain_yes("thanks", deploy));
        assert!(!plain_yes("please", deploy));
    }

    #[test]
    fn a_word_that_asks_leaves_no_bare_yes() {
        // "you", "that" and "this" turn a yes into a question with no
        // question mark ("you good", "that ok"), and "y" reads as "why".
        let deploy = "Deploy build 55 to staging and prod";
        let wrong: Vec<_> = [
            "you good",
            "you ok",
            "that ok",
            "this fine",
            "do you",
            "y",
            "y do it",
            "ok you",
        ]
        .into_iter()
        .filter(|asked| plain_yes(asked, deploy))
        .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
        for yes in ["ok do it", "yes please", "go ahead", "ship it now"] {
            assert!(plain_yes(yes, deploy), "{yes}");
        }
    }

    #[test]
    fn you_is_plain_only_after_thank() {
        let deploy = "Deploy build 55 to staging and prod";
        for yes in [
            "yes thank you",
            "ok, thank you",
            "Yes. Thank you!",
            "thank you, yes",
            "Deploy build 55 to staging and prod, thank you",
            "thank you: deploy build 55 to staging and prod",
        ] {
            assert!(plain_yes(yes, deploy), "{yes}");
        }
        let wrong: Vec<_> = [
            "you good",
            "do you",
            "thank you",
            "you yes",
            "yes you",
            "thank you you yes",
            "Deploy build 55 to staging and prod you",
        ]
        .into_iter()
        .filter(|asked| plain_yes(asked, deploy))
        .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
        // An action that holds "thank you" is retyped whole, its "you"
        // included; the plain words beside it never shorten it.
        let thank = "Send the thank you note";
        assert!(plain_yes("ok, send the thank you note", thank));
        assert!(!plain_yes("send the thank", thank));
        assert!(!plain_yes("send the note", thank));
        assert!(!plain_yes("ok send the thank you", thank));
        // The same check holds beside a tapped option.
        assert!(plain_beside("thank you", "Ship it"));
        assert!(plain_beside("yes, ship it, thank you", "Ship it"));
        assert!(!plain_beside("you sure", "Ship it"));
    }

    #[test]
    fn it_is_plain_only_after_a_yes_word() {
        // "it ok" drops an "is" and asks, as "that ok" does.
        let deploy = "Deploy build 55 to staging and prod";
        let wrong: Vec<_> = [
            "it ok",
            "it good",
            "it fine",
            "it's ok",
            "is it ok",
            "thanks it",
            "please it",
            "the it",
            "it",
            "for it",
            "go for",
            "ok for it",
            "yes for now",
            "Deploy build 55 to staging and prod it",
            "sure it ok",
            "sure it fine",
            "ok it good",
            "yes it ok",
            "good it ok",
            "k it fine",
            "do it ok",
        ]
        .into_iter()
        .filter(|asked| plain_yes(asked, deploy))
        .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
        for yes in [
            "do it",
            "ship it",
            "yes it",
            "approve it",
            "ok do it",
            "go for it",
            "yes, go for it",
            "please do it now",
            "ok, deploy build 55 to staging and prod, ship it",
        ] {
            assert!(plain_yes(yes, deploy), "{yes}");
        }
        assert!(!plain_beside("it ok", "Ship it"));
        assert!(!plain_beside("sure it ok", "Ship it"));
        assert!(plain_beside("go for it", "Ship it"));
    }

    #[test]
    fn a_combining_mark_or_a_punctuation_like_letter_is_a_symbol() {
        let wrong: Vec<_> = [
            "ok\u{301}",
            "ok\u{36f}",
            "ok\u{1ab0}",
            "ok\u{1aff}",
            "ok\u{1dc0}",
            "ok\u{1dff}",
            "ok\u{fe20}",
            "ok\u{fe2f}",
            "yes \u{2021}",
            "ok \u{fbf}",
            "ok \u{2e38}",
            "yes\u{1c0} merge",
            "yes \u{1c3}",
            "\u{2bb}ok",
            "ok\u{2b9}",
            "ok\u{2ff}",
            // A tilde strikes a word through in chat markup.
            "~~Deploy build 55 to staging~~",
            "~ok~",
            "ok\u{ff5e}",
        ]
        .iter()
        .filter(|shown| !(worded(shown) && pictured(shown)))
        .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    #[test]
    fn beside_a_chosen_option_only_plain_and_label_words_pass() {
        for ok in ["thanks", "Ship it", "yes, ship it now"] {
            assert!(plain_beside(ok, "Ship it"), "{ok}");
        }
        for loose in ["after CI", "Submit build 55", "for staging", "staging"] {
            assert!(!plain_beside(loose, "Ship it"), "{loose}");
        }
        // Past plain words, the text is the label's other words in full and
        // in order, so a part of the label never narrows it.
        let both = "Deploy to staging and prod";
        for narrow in [
            "to staging",
            "deploy to staging",
            "staging and",
            "deploy and prod",
            "prod and staging",
            "and prod to staging deploy",
            "yes, deploy to staging",
            "prod",
            "staging",
            "deploy prod",
            "deploy",
        ] {
            assert!(!plain_beside(narrow, both), "{narrow}");
        }
        assert!(!plain_beside("13", "Merge PR 12 and 13"));
        // The label's plain words count too: a plain verb left out of it is
        // a step left out, and a plain word inside it is a break.
        let wrong: Vec<_> = [
            ("Tag v1.2 ship to production", "Tag v1.2 to production"),
            ("Merge PR 12 ship to prod", "merge PR 12 to prod"),
            ("Tag v1.2, ship to prod", "tag v1.2 to prod"),
            (
                "Deploy to staging and prod",
                "deploy to staging ok and prod",
            ),
        ]
        .iter()
        .filter(|(label, short)| plain_beside(short, label) || !plain_beside(label, label))
        .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
        for whole in [
            "Deploy to staging and prod",
            "yes, deploy to staging and prod, thanks",
            "ok",
            "thanks",
        ] {
            assert!(plain_beside(whole, both), "{whole}");
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
            "× merge",
            "yes ×",
            "ok ⨯",
            "¬ merge",
            "\u{10102} yes",
            "yes ✓",
            "ok\u{20e0}",
            "ok\u{20d2}",
            "k\u{336}",
            "ok\u{334}",
            "ok\u{338}",
            "yes \u{2020}",
            "ok \u{fbe}",
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
            // Quoted, globbed, braced, tilded or commented: bash runs
            // another word than the one written.
            "'sudo' make release",
            "su''do make",
            "\"sudo\" make release",
            "{sudo,} make",
            "/usr/bin/sud? make",
            "/usr/bin/su* make",
            "[s]udo make",
            "~/bin/deploy",
            "make #release",
            // Whitespace bash does not split on, and invisible characters.
            "make\u{a0}release",
            "make\u{2003}release",
            "make\u{200b} release",
            "make\u{202e}esaeler",
            "make\u{b}release",
            "make\u{3164}release",
            "make\u{2800}release",
            "make\u{ad}release",
            "make\u{61c}release",
            "make\u{e0041}release",
            "m\u{430}ke release",
            // Interpreters running inline code, however the flag is spelled.
            "bash \"-c\" x",
            "bash -lc x",
            "bash -c x",
            "sh -ec x",
            "zsh -xc x",
            "python3 -Sc x",
            "python3 -c x",
            "python3 -m http",
            "python3 -mhttp x",
            "node -pe x",
            "node -e1 x",
            "node -p x",
            "node --print x",
            "node --eval x",
            "perl -e x",
            "ruby -e x",
            "bash",
            "bash -l",
            // Options before the script leave room for inline code after.
            "bash -o errexit",
            "bash -x scripts/release.sh",
            "perl -E x",
            "python3 -W ignore",
            "python3 -u tools/ship.py",
            "coproc make",
            "! make",
        ] {
            assert!(check_prefix(bad).is_err(), "{bad}");
        }
        for good in [
            "make release",
            "./scripts/ship.sh --prod",
            "cargo publish",
            "make\trelease",
            "bash scripts/release.sh",
            "python3 tools/ship.py --dry-run",
            "make release-2.0 v=1",
        ] {
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
            "'sudo' make release",
            "su''do make",
            "{sudo,} make",
            "/usr/bin/sud? make",
            "\"sudo\" make release",
            "make\u{a0}release",
            "make\u{85}release",
            "make release\u{2028}rm x",
            "make\u{200b} release",
            "make\u{3164}release",
            "m\u{430}ke release",
            "make ~/release",
            "make release # now",
        ] {
            assert_eq!(command_head(compound), None, "{compound}");
        }
        assert_eq!(command_head("make\trelease").unwrap(), ["make", "release"]);
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
        // Only spaces and tabs split words, in a prefix as in a head.
        assert!(!covers("make\u{a0}release", &head("make release")));
        assert!(!covers("'make' release", &head("make release")));
        assert_eq!(command_head("make\u{a0}release --dry-run"), None);
        // A compound command has no head, so no prefix covers it.
        assert_eq!(command_head("make  release; rm x"), None);
    }

    #[test]
    fn a_sentence_keeps_its_prefix_even_when_it_does_not_fit() {
        let told = |action: &str, prefix| {
            sentence(
                "r1",
                "q1",
                "alpha",
                "task",
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
        // The unscoped sentence fits, but adding the prefix crosses the bound.
        // Keep the restriction intact so admission/launch can fail closed.
        let action = "x".repeat(MAX_SENTENCE_CHARS - told("", None).chars().count());
        let unscoped = told(&action, None);
        assert_eq!(unscoped.chars().count(), MAX_SENTENCE_CHARS);
        let long = told(&action, Some("make release"));
        assert_eq!(
            long,
            format!("{unscoped} Only commands that begin exactly with: make release")
        );
        assert!(long.chars().count() > MAX_SENTENCE_CHARS);
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

    /// List reads every row through `state`, the rule `live` and so every
    /// launch reads: each kind of row says so, and a live row's sentence is
    /// the one a launch carries.
    #[test]
    fn list_states_agree_with_what_a_launch_carries() {
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
            "charter.put",
            json!({"id":"old","objective":"Deliver","repo":repo,"min_free_bytes":0}),
        )
        .unwrap();
        for (task, owner, charter) in [
            ("main", "apps", "project"),
            ("over", "apps", "project"),
            ("theirs", "infra", "project"),
            ("gone", "apps", "project"),
            ("moved", "apps", "project"),
            ("stale", "apps", "old"),
        ] {
            crate::dispatch(
                &mut store,
                "task.create",
                json!({"id":task,"charter_id":charter,"owner":owner,"title":"Deliver"}),
            )
            .unwrap();
        }
        crate::dispatch(
            &mut store,
            "task.create",
            json!({"id":"child","parent":"gone","charter_id":"project","owner":"apps","title":"Deliver"}),
        )
        .unwrap();
        // Closed four ways: done, a cancelled ancestor, handed to another
        // ship, and a revoked charter.
        for update in [
            "UPDATE tasks SET status='done' WHERE id='over'",
            "UPDATE tasks SET status='cancelled' WHERE id='gone'",
            "UPDATE tasks SET owner='infra' WHERE id='moved'",
        ] {
            store.connection().execute(update, []).unwrap();
        }
        crate::dispatch(&mut store, "charter.revoke", json!({"id":"old"})).unwrap();
        let now = Store::now_ms();
        let hour = 3_600_000;
        for (id, ship, task, created, expires, revoked) in [
            ("a-live", "apps", "main", now - hour, now + hour, None),
            (
                "a-expired",
                "apps",
                "main",
                now - 3 * hour,
                now - hour,
                None,
            ),
            (
                "a-revoked",
                "apps",
                "main",
                now - 2 * hour,
                now + hour,
                Some(now - hour),
            ),
            ("a-closed", "apps", "over", now - 2 * hour, now + hour, None),
            (
                "a-ancestor",
                "apps",
                "child",
                now - 2 * hour,
                now + hour,
                None,
            ),
            (
                "a-handed",
                "apps",
                "moved",
                now - 2 * hour,
                now + hour,
                None,
            ),
            (
                "a-charter",
                "apps",
                "stale",
                now - 2 * hour,
                now + hour,
                None,
            ),
            (
                "i-live",
                "infra",
                "theirs",
                now - 2 * hour,
                now + hour,
                None,
            ),
        ] {
            store
                .connection()
                .execute(
                    "INSERT INTO captain_approvals(id,ship,task_id,request_id,reply_id,action,
                       command_prefix,created_ms,expires_ms,revoked_ms,source)
                     VALUES(?1,?2,?3,?1,'r-'||?1,'Run make release','make release',?4,?5,?6,'reply')",
                    params![id, ship, task, created, expires, revoked],
                )
                .unwrap();
        }
        let listed = list(&store, &json!({})).unwrap();
        assert_eq!(listed["ship"], Value::Null);
        let rows = listed["approvals"].as_array().unwrap();
        let state_of = |id: &str| {
            let row = rows.iter().find(|r| r["id"] == id).unwrap();
            (
                row["state"].as_str().unwrap(),
                row["sentence"].clone(),
                row["live"].clone(),
            )
        };
        assert_eq!(rows.len(), 8, "{listed}");
        assert_eq!(state_of("a-expired").0, "expired");
        assert_eq!(state_of("a-revoked").0, "revoked");
        for id in ["a-closed", "a-ancestor", "a-handed", "a-charter"] {
            assert_eq!(state_of(id).0, "closed", "{id}");
        }
        for (id, ship) in [("a-live", "apps"), ("i-live", "infra")] {
            let (state, sentence, live_flag) = state_of(id);
            assert_eq!((state, live_flag), ("live", json!(true)));
            let carried = launch_set(&store, ship, now).unwrap();
            assert_eq!(carried.len(), 1);
            assert_eq!((carried[0].0.as_str(), json!(carried[0].1)), (id, sentence));
        }
        for id in [
            "a-expired",
            "a-revoked",
            "a-closed",
            "a-ancestor",
            "a-handed",
            "a-charter",
        ] {
            assert_eq!(state_of(id).1, Value::Null, "{id}");
            assert_eq!(state_of(id).2, json!(false), "{id}");
        }
        // `live` keeps exactly the rows `state` calls live, here and later.
        for at in [now, now - 2 * hour + 1, now + 2 * hour] {
            let conn = store.connection();
            let every = select(conn, "1", [], "id").unwrap();
            let by_state: Vec<String> = every
                .iter()
                .filter(|row| state(conn, row, at) == "live")
                .map(|row| row.id.clone())
                .collect();
            let mut by_live: Vec<String> = ["apps", "infra"]
                .iter()
                .flat_map(|ship| live(&store, ship, at).unwrap())
                .map(|row| row.id)
                .collect();
            by_live.sort();
            assert_eq!(by_state, by_live, "at {at}");
        }
        let only = list(&store, &json!({"ship":"apps","live_only":true})).unwrap();
        let ids: Vec<&Value> = only["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| &r["id"])
            .collect();
        assert_eq!(ids, [&json!("a-live")]);
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
        // The dashboard's open cannot migrate it, so refuses it up front.
        #[cfg(feature = "dashboard")]
        {
            let refused = Store::open_existing_action(&home, crate::store::Caller::Operator);
            assert!(
                refused
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("incompatible existing action schema")
            );
        }
        let store = Store::open(&home).unwrap();
        #[cfg(feature = "dashboard")]
        Store::open_existing_action(&home, crate::store::Caller::Operator).unwrap();
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
            None,
        );
        assert_eq!(outcome.unwrap(), None);
        let count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    /// Only the captain's own tap on a block card, in the home holding the
    /// ask, records or reuses an approval, and only one given within 24 h.
    #[test]
    fn a_block_cards_tap_approves_only_from_the_captain_and_within_24_hours() {
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
        let now = Store::now_ms();
        for (id, created_ms) in [
            ("approval-old", now - 25 * 3_600_000),
            ("approval-new", now - 3_600_000),
        ] {
            store
                .connection()
                .execute(
                    "INSERT INTO captain_approvals(id,ship,task_id,request_id,reply_id,action,
                       command_prefix,created_ms,expires_ms,source)
                     VALUES(?1,'apps','main','q-'||?1,'r-'||?1,'Run make release','make release',?2,?3,'reply')",
                    params![id, created_ms, now + 3_600_000],
                )
                .unwrap();
        }
        let ask = json!({"id": "ask-1", "from": "apps", "kind": "decision", "task_id": "main",
            "state": "open", "payload": {"type": "tool_permission", "block": {
                "approve": {"action": "Run `make release`", "command_prefix": "make release"},
                "approvals": ["approval-old", "approval-new"]},
            "decision": {"options": [
                {"id": "as_approved_1", "label": "As approved: Run make release"},
                {"id": "as_approved_2", "label": "As approved: Run make release"},
                {"id": "approve", "label": "Approve for 24 h: Run `make release`"}]}}});
        let tap = |reply: &str, option: &str, extra: Value| {
            let mut payload = json!({"answer": {"option_id": option}});
            payload.as_object_mut().unwrap().extend(
                extra
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
            record(&store, &ask, reply, "", &payload, now, None)
                .unwrap()
                .unwrap()
        };
        let count = || -> i64 {
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0))
                .unwrap()
        };
        for (reply, option, extra) in [
            ("reply-1", "approve", json!({})),
            ("reply-2", "approve", json!({"operator": false})),
            (
                "reply-3",
                "approve",
                json!({"operator": true, "fanned_from": "reply-0"}),
            ),
            (
                "reply-4",
                "as_approved_2",
                json!({"fanned_from": "reply-0"}),
            ),
        ] {
            let outcome = tap(reply, option, extra);
            assert_eq!(outcome["recorded"], false, "{reply}: {outcome}");
            assert!(
                outcome["why"]
                    .as_str()
                    .unwrap()
                    .contains("only the captain's own reply approves"),
                "{reply}: {outcome}"
            );
        }
        assert_eq!(count(), 2);
        // Still live, but given 25 h ago: asked again, not reused.
        let old = tap("reply-5", "as_approved_1", json!({"operator": true}));
        assert_eq!(old["recorded"], false, "{old}");
        assert!(old.get("covered_by").is_none(), "{old}");
        assert!(
            old["why"].as_str().unwrap().contains("older than 24 h"),
            "{old}"
        );
        // Given an hour ago, it is reused; nothing new is recorded.
        let new = tap("reply-6", "as_approved_2", json!({"operator": true}));
        assert_eq!(new["covered_by"], "approval-new", "{new}");
        assert_eq!(count(), 2);
        // The captain's own approve records one row bound to the ask.
        let own = tap("reply-7", "approve", json!({"operator": true}));
        assert_eq!(own["recorded"], true, "{own}");
        assert_eq!(count(), 3);
    }
}
