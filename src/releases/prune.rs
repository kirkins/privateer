//! `privateer upgrade prune [--keep N] [--dry-run]`: remove old releases
//! from this installation. It keeps the newest N (by when each was
//! published), the one `current` selects, the one a rollback would return
//! to, and every release a live process runs or names; it removes nothing
//! else from `releases/` and touches nothing outside it.
use super::{DEFAULT_KEEP, Release, installation, read_history, running_image, selected};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: privateer upgrade prune [--keep N] [--dry-run]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub keep: usize,
    pub dry_run: bool,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut options = Self {
            keep: DEFAULT_KEEP,
            dry_run: false,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--dry-run" => options.dry_run = true,
                "--keep" => {
                    let keep = args.next().context(USAGE)?;
                    options.keep = keep.parse().ok().filter(|n| *n >= 1).with_context(|| {
                        format!("--keep takes a count of at least 1, not {keep:?}")
                    })?;
                }
                _ => bail!("{USAGE}"),
            }
        }
        Ok(options)
    }
}

/// The release a path lies in, when it is under `releases`.
fn release_of(path: &Path, releases: &[PathBuf]) -> Option<String> {
    releases.iter().find_map(|dir| {
        let rest = path.strip_prefix(dir).ok()?;
        let commit = rest.components().next()?.as_os_str().to_str()?;
        super::valid_commit(commit).then(|| commit.to_owned())
    })
}

/// Every release a live process of this user runs, works in or names: its
/// executable and working directory as the kernel reports them (a plugin's
/// adapter runs from its directory with relative arguments), or a path in
/// its arguments. Arguments are as invoked: Linux reads `/proc`, and
/// elsewhere `ps` reports them.
fn live(releases: &[PathBuf]) -> Result<BTreeMap<String, Vec<u32>>> {
    let mut found: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    let mut note = |commit: Option<String>, pid: i32| {
        if let Some(commit) = commit {
            let pids = found.entry(commit).or_default();
            if !pids.contains(&(pid as u32)) {
                pids.push(pid as u32);
            }
        }
    };
    for pid in super::processes::all().context("inspect processes")? {
        for path in [super::processes::image(pid), super::processes::cwd(pid)]
            .into_iter()
            .flatten()
        {
            note(release_of(&path, releases), pid);
        }
        #[cfg(target_os = "linux")]
        if let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) {
            for arg in cmdline.split(|b| *b == 0) {
                use std::os::unix::ffi::OsStrExt;
                let arg = Path::new(std::ffi::OsStr::from_bytes(arg));
                note(release_of(arg, releases), pid);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = std::process::Command::new("ps")
            .args(["-axww", "-o", "pid=,command="])
            .output()
            .context("cannot run ps to see which releases are in use")?;
        ensure!(
            output.status.success(),
            "ps failed; not pruning without knowing which releases are in use"
        );
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut words = line.split_whitespace();
            let Some(pid) = words.next().and_then(|p| p.parse::<i32>().ok()) else {
                continue;
            };
            for word in words {
                note(release_of(Path::new(word), releases), pid);
            }
        }
    }
    Ok(found)
}

/// Remove a release: renamed out of the way first, so no half-removed
/// `releases/<commit>` is ever seen, then removed through std::fs. Only a
/// real directory directly inside `releases` is touched.
fn remove(releases: &Path, commit: &str) -> Result<()> {
    let root = releases.join(commit);
    let meta = fs::symlink_metadata(&root)?;
    ensure!(
        meta.is_dir() && root.parent() == Some(releases),
        "{} is not a release directory; not removing it",
        root.display()
    );
    let aside = releases.join(format!(".prune-{commit}-{}", uuid::Uuid::new_v4().simple()));
    fs::rename(&root, &aside)?;
    ensure!(
        aside.parent() == Some(releases),
        "prune stays inside releases"
    );
    fs::remove_dir_all(&aside)?;
    Ok(())
}

pub fn prune(options: &Options) -> Result<Value> {
    let image = running_image()?;
    let running = Release::from_executable(&image);
    let data = installation(running.as_ref())?;
    let releases = fs::canonicalize(data.join("releases"))
        .with_context(|| format!("{} has no releases", data.display()))?;
    let _lock = if options.dry_run {
        None
    } else {
        Some(super::adopt::lock(&data)?)
    };
    let mut why: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut reason = |commit: &str, because: String| {
        why.entry(commit.to_owned()).or_default().push(because);
    };
    let current = selected(&data)?;
    if let Some(current) = &current {
        reason(&current.commit, "current".into());
        // The release a rollback would return to.
        let registry = crate::fleet::registry_path()?;
        if let Some(previous) = read_history(&registry, -1)?.into_iter().find_map(|r| {
            (r.kind.as_deref() == Some("upgrade")
                && super::switched(&r)
                && r.to_commit == current.commit)
                .then_some(r.from_commit)
                .flatten()
        }) {
            reason(&previous, "previous".into());
        }
    }
    if let Some(running) = &running {
        reason(&running.commit, "this command runs it".into());
    }
    let mut published: Vec<(std::time::SystemTime, String)> = Vec::new();
    for entry in fs::read_dir(&releases)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let meta = fs::symlink_metadata(entry.path())?;
        if !meta.is_dir() || !super::valid_commit(&name) {
            continue;
        }
        published.push((meta.modified()?, name));
    }
    // Newest first; the name breaks a tie so the order is stable.
    published.sort_by(|a, b| b.cmp(a));
    for (_, commit) in published.iter().take(options.keep) {
        reason(commit, format!("among the newest {}", options.keep));
    }
    let dirs = [releases.clone(), data.join("releases")];
    for (commit, pids) in live(&dirs)? {
        reason(
            &commit,
            format!(
                "in use by pid {}",
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    let mut kept = Vec::new();
    let mut removed = Vec::new();
    for (_, commit) in &published {
        if let Some(because) = why.get(commit) {
            kept.push(json!({"commit": commit, "why": because}));
        } else if options.dry_run {
            removed.push(json!(commit));
        } else {
            // A process may have started from it since the scan.
            if live(&dirs)?.contains_key(commit) {
                kept.push(json!({"commit": commit, "why": ["in use (started meanwhile)"]}));
                continue;
            }
            remove(&releases, commit)?;
            removed.push(json!(commit));
        }
    }
    let unlisted: BTreeSet<&String> = why
        .keys()
        .filter(|c| !published.iter().any(|(_, p)| p == *c))
        .collect();
    let mut report = json!({
        "releases": releases,
        "keep": options.keep,
        "dry_run": options.dry_run,
        "kept": kept,
        // Kept for a reason but not present (a process runs a release
        // already removed by hand, say).
        "absent": unlisted,
    });
    report[if options.dry_run {
        "would_remove"
    } else {
        "removed"
    }] = json!(removed);
    Ok(report)
}
