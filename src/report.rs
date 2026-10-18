//! Reports for the captain, and the captain's annotations on them.
//!
//! A ship hands the captain a report on mail addressed to the captain, as
//! `payload.report`: inline as `{title, format, content}` while the whole
//! message fits mail's 64 KiB, or else stored first with `report.put` and
//! referenced as `{report_id}`, which the runtime fills in with the stored
//! title, format and size. `report.put` takes the content inline or, past what
//! one argument carries, as `content_file`, read by the runtime. `report.get` is the operator's: it finds a stored
//! report in any fleet home on this machine and returns it as stored. HTML is
//! returned as is; the portal that shows it sandboxes it.
//!
//! The captain answers with `payload.annotations` on a reply (or a notice in
//! the thread): each one anchors a comment to a quote in the report (with
//! optional prefix and suffix to place it) or to a selector. Reports and
//! annotations are checked for shape and size only, and no error repeats
//! their text.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::path::PathBuf;

const FORMATS: &[&str] = &["markdown", "html"];
/// A stored report's content: what the captain's portal will show.
const MAX_CONTENT_BYTES: usize = 256 * 1024;
const MAX_TITLE_CHARS: usize = 200;
/// Stored reports kept per ship; the oldest beyond this are removed.
const KEEP_PER_SHIP: i64 = 200;
const MAX_ANNOTATIONS: usize = 100;
const MAX_COMMENT_CHARS: usize = 4096;
/// A quote, prefix, suffix or selector an annotation is anchored by.
const MAX_ANCHOR_CHARS: usize = 2000;

pub fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS reports(id TEXT PRIMARY KEY, author TEXT NOT NULL,
           title TEXT, format TEXT NOT NULL, content TEXT NOT NULL, created_ms INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS reports_author ON reports(author, created_ms);",
    )?;
    Ok(())
}

/// A report's title: required, since the captain's portal opens none without one.
fn title(report: &Value) -> Result<Option<&str>> {
    match report.get("title").filter(|v| !v.is_null()) {
        None => bail!("report title must be a string of 1 to {MAX_TITLE_CHARS} characters"),
        Some(title) => {
            let title = title
                .as_str()
                .filter(|t| !t.trim().is_empty() && t.chars().count() <= MAX_TITLE_CHARS)
                .with_context(|| {
                    format!("report title must be a string of 1 to {MAX_TITLE_CHARS} characters")
                })?;
            Ok(Some(title))
        }
    }
}

fn format(report: &Value) -> Result<&str> {
    report
        .get("format")
        .and_then(Value::as_str)
        .filter(|f| FORMATS.contains(f))
        .context("report format must be markdown or html")
}

fn content(report: &Value) -> Result<&str> {
    report
        .get("content")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .context("report content must be a non-empty string")
}

/// A ship stores a report too large to send inline, in its own home.
fn put(store: &Store, args: &Value) -> Result<Value> {
    let author = required(args, "from")?;
    ensure!(
        author != crate::mail::CAPTAIN && author != crate::mail::CREW,
        "reports are put by a ship, for the captain"
    );
    match crate::mail::ship(store, author)? {
        Some(ship) if ship["location"] == "here" => {}
        Some(_) => bail!("ship {author} is not hosted by this home"),
        None => bail!("unknown ship {author}; add it with ship.put"),
    }
    let title = title(args)?;
    let format = format(args)?;
    let from_file;
    let content = match args.get("content_file").filter(|v| !v.is_null()) {
        Some(path) => {
            ensure!(
                args.get("content").is_none(),
                "give content or content_file, not both"
            );
            from_file = read_content(path)?;
            from_file.as_str()
        }
        None => content(args)?,
    };
    ensure!(
        !content.is_empty() && content.len() <= MAX_CONTENT_BYTES,
        "report content must be 1 byte to 256 KiB"
    );
    let id = format!("report-{}", uuid::Uuid::new_v4());
    let now = Store::now_ms();
    let tx = store.write_transaction()?;
    tx.execute(
        "INSERT INTO reports(id,author,title,format,content,created_ms) VALUES(?1,?2,?3,?4,?5,?6)",
        params![id, author, title, format, content, now],
    )?;
    // Bounded per ship: the oldest reports beyond the cap go.
    let pruned = tx.execute(
        "DELETE FROM reports WHERE author=?1 AND id NOT IN
           (SELECT id FROM reports WHERE author=?1 ORDER BY created_ms DESC, rowid DESC LIMIT ?2)",
        params![author, KEEP_PER_SHIP],
    )?;
    tx.commit()?;
    let reference = reference(&id, title, format, content.len());
    store.audit(
        "report.put",
        &json!({"id": id, "author": author, "bytes": content.len(), "pruned": pruned}),
    )?;
    Ok(
        json!({"report_id": id, "bytes": content.len(), "created_ms": now,
        "report": reference}),
    )
}

/// A report's content from a file the ship wrote: argv, `--file` and RPC
/// lines are all smaller than the largest report.
fn read_content(path: &Value) -> Result<String> {
    use std::io::Read;
    let path = std::path::Path::new(
        path.as_str()
            .context("content_file must be an absolute path")?,
    );
    ensure!(path.is_absolute(), "content_file must be an absolute path");
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot open content_file {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "content_file must be a regular file"
    );
    let mut bytes = vec![];
    file.take(MAX_CONTENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_CONTENT_BYTES,
        "report content must be 1 byte to 256 KiB"
    );
    String::from_utf8(bytes)
        .ok()
        .context("content_file must be UTF-8 text")
}

/// What mail carries for a stored report.
fn reference(id: &str, title: Option<&str>, format: &str, bytes: usize) -> Value {
    let mut reference = json!({"report_id": id, "format": format, "bytes": bytes});
    if let Some(title) = title {
        reference["title"] = json!(title);
    }
    reference
}

struct Stored {
    author: String,
    title: Option<String>,
    format: String,
    content: String,
    created_ms: i64,
}

fn stored(store: &Store, id: &str) -> Result<Option<Stored>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT author,title,format,content,created_ms FROM reports WHERE id=?1",
            [id],
            |r| {
                Ok(Stored {
                    author: r.get(0)?,
                    title: r.get(1)?,
                    format: r.get(2)?,
                    content: r.get(3)?,
                    created_ms: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// The captain reads a stored report, from whichever fleet home holds it.
fn get(store: &Store, args: &Value) -> Result<Value> {
    ensure!(
        *store.caller() == Caller::Operator,
        "only the captain reads reports with report.get"
    );
    let id = required(args, "id")?;
    identifier(id)?;
    if let Some(found) = here(Some(store), id)? {
        return Ok(found);
    }
    // A ship on a linked machine keeps its reports there.
    if let Some(found) = crate::machines::report_from_linked(id)? {
        return Ok(found);
    }
    bail!("no report {id} in the fleet")
}

/// A stored report from this machine's homes, for a machine that links to
/// this one and asks over the gateway. It never asks further.
pub(crate) fn serve(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    identifier(id)?;
    here(None, id)?.with_context(|| format!("no report {id} on this machine"))
}

/// The report `id` from `store`'s home first, then every other fleet home on
/// this machine.
fn here(store: Option<&Store>, id: &str) -> Result<Option<Value>> {
    let mut homes: Vec<PathBuf> = store.map(|s| s.home().to_path_buf()).into_iter().collect();
    for (_, home) in crate::captain::fleet()? {
        if !homes.iter().any(|h| same_dir(h, &home)) {
            homes.push(home);
        }
    }
    for home in homes {
        let found = match store {
            Some(store) if home == store.home() => stored(store, id)?,
            _ => stored(&Store::open(&home)?, id)?,
        };
        if let Some(report) = found {
            let mut result = json!({"id": id, "ship": report.author,
                "format": report.format, "bytes": report.content.len(),
                "created_ms": report.created_ms, "content": report.content});
            if let Some(title) = report.title {
                result["title"] = json!(title);
            }
            return Ok(Some(result));
        }
    }
    Ok(None)
}

fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Check `payload.report` on mail to the captain, and fill a stored report's
/// reference with its title, format and size. A report is inline
/// `{title, format, content}` or a reference `{report_id}` to a report the
/// sender put in this home. Mail to anyone else keeps its payload as given.
pub(crate) fn check_report(store: &Store, from: &str, to: &str, payload: &mut Value) -> Result<()> {
    if to != crate::mail::CAPTAIN {
        return Ok(());
    }
    let Some(report) = payload.get("report") else {
        return Ok(());
    };
    let report = report
        .as_object()
        .context("payload.report must be an object")?;
    if let Some(id) = report.get("report_id") {
        let id = id
            .as_str()
            .context("payload.report.report_id must be a string")?;
        identifier(id)?;
        only(report, &["report_id", "title", "format", "bytes"])?;
        let stored = stored(store, id)?
            .filter(|r| r.author == from)
            .with_context(|| {
                format!("{from} has no report {id} in this home; put it with report.put")
            })?;
        payload["report"] = reference(
            id,
            stored.title.as_deref(),
            &stored.format,
            stored.content.len(),
        );
        return Ok(());
    }
    only(report, &["title", "format", "content"])?;
    let report = Value::Object(report.clone());
    title(&report)?;
    format(&report)?;
    content(&report)?;
    Ok(())
}

fn only(object: &Map<String, Value>, keys: &[&str]) -> Result<()> {
    ensure!(
        object.keys().all(|k| keys.contains(&k.as_str())),
        "payload.report takes only {}",
        keys.join(", ")
    );
    Ok(())
}

/// Check the captain's `payload.annotations`: at most 100, each
/// `{anchor: {quote, prefix?, suffix?} | {selector, quote?}, comment}`. Errors name
/// the annotation by position and never repeat its text.
pub(crate) fn check_annotations(from: &str, kind: &str, payload: &Value) -> Result<()> {
    let Some(annotations) = payload.get("annotations") else {
        return Ok(());
    };
    ensure!(
        from == crate::mail::CAPTAIN && matches!(kind, "reply" | "notice"),
        "payload.annotations are the captain's, on a reply or a notice"
    );
    let annotations = annotations
        .as_array()
        .context("payload.annotations must be an array")?;
    ensure!(
        annotations.len() <= MAX_ANNOTATIONS,
        "payload.annotations holds at most {MAX_ANNOTATIONS}"
    );
    let bounded = |value: Option<&Value>, max: usize| {
        value
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty() && s.chars().count() <= max)
    };
    for (index, annotation) in annotations.iter().enumerate() {
        let at = format!("payload.annotations[{index}]");
        let annotation = annotation
            .as_object()
            .with_context(|| format!("{at} must be an object"))?;
        ensure!(
            annotation
                .keys()
                .all(|k| matches!(k.as_str(), "anchor" | "comment")),
            "{at} takes only anchor and comment"
        );
        ensure!(
            bounded(annotation.get("comment"), MAX_COMMENT_CHARS),
            "{at}.comment must be a string of 1 to {MAX_COMMENT_CHARS} characters"
        );
        let anchor = annotation
            .get("anchor")
            .and_then(Value::as_object)
            .with_context(|| format!("{at}.anchor must be an object"))?;
        // A block the captain taps is its selector, with its text as the quote.
        let keys: &[&str] = if anchor.contains_key("selector") {
            &["selector", "quote"]
        } else {
            &["quote", "prefix", "suffix"]
        };
        ensure!(
            anchor.keys().all(|k| keys.contains(&k.as_str())),
            "{at}.anchor is {{quote, prefix?, suffix?}} or {{selector, quote?}}"
        );
        let first = if anchor.contains_key("selector") {
            "selector"
        } else {
            "quote"
        };
        ensure!(
            bounded(anchor.get(first), MAX_ANCHOR_CHARS),
            "{at}.anchor.{first} must be a string of 1 to {MAX_ANCHOR_CHARS} characters"
        );
        if first == "selector"
            && let Some(quote) = anchor.get("quote")
        {
            ensure!(
                bounded(Some(quote), MAX_ANCHOR_CHARS),
                "{at}.anchor.quote must be a string of 1 to {MAX_ANCHOR_CHARS} characters"
            );
        }
        for optional in ["prefix", "suffix"] {
            if let Some(value) = anchor.get(optional) {
                ensure!(
                    bounded(Some(value), MAX_ANCHOR_CHARS),
                    "{at}.anchor.{optional} must be a string of 1 to {MAX_ANCHOR_CHARS} characters"
                );
            }
        }
    }
    Ok(())
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "report.put" => put(store, &args),
        "report.get" => get(store, &args),
        _ => bail!("unknown report command {command}"),
    }
}
