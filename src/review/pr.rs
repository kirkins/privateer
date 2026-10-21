//! The pull request a review or a merge check reads, read by the runtime
//! itself, never taken from the ship, and the local checkout it is read in.
//!
//! The PR is read through the forge adapter (`crate::forges::call`,
//! `pr.view`) with the reviewer account's token, for the repository's entry
//! in the captain's forges.json: its forge, API, protected branch and
//! required checks come from there, never from the clone, whose remote and
//! git config a ship can change.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long one forge read or git fetch may take.
const READ_TIMEOUT: Duration = Duration::from_secs(120);
/// Changed files the prompt lists; the rest are in files.txt.
const LISTED_FILES: usize = 100;

/// The local clone of a PR's repository: its folder, its origin's host
/// (none for a local path) and its `owner/name`.
#[derive(Clone, Debug)]
pub struct Clone {
    pub dir: PathBuf,
    pub host: Option<String>,
    pub repo: String,
}

/// A pull request as the forge reports it now.
#[derive(Clone, Debug, Default)]
pub struct Pr {
    pub state: String,
    pub base: String,
    pub base_sha: String,
    pub head_sha: String,
    pub head_ref: String,
    pub title: String,
    pub body: String,
    pub url: String,
    /// Whether the required checks are green at the head.
    pub checks_ok: bool,
    /// The checks that are not, or why none could be read.
    pub checks_detail: String,
    /// The merge commit, once merged, when the forge names it.
    pub merge_sha: Option<String>,
}

/// The files a review folder holds beside its worktree.
pub struct Files {
    pub tree: PathBuf,
    pub diff: PathBuf,
    pub stats: PathBuf,
    pub pr_md: PathBuf,
    /// `git diff --name-status` of the change, bounded for the prompt.
    pub listing: String,
    /// Symlinks in the worktree that led outside it, cut to plain files.
    pub cut: Vec<String>,
}

/// Run a command with a bounded wait, its stdin given and its stdout and
/// stderr read whole.
pub fn run(command: Command, input: Option<&[u8]>) -> Result<std::process::Output> {
    run_until(command, input, None)
}

/// `run`, stopped at `until` when that comes before the usual bound: a
/// caller with a time budget (the posting tick) gives its end.
pub fn run_until(
    mut command: Command,
    input: Option<&[u8]>,
    until: Option<Instant>,
) -> Result<std::process::Output> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("start a forge or git read")?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .context("child stdin")?
            .write_all(input)?;
    }
    let mut stdout = child.stdout.take().context("child stdout")?;
    let mut stderr = child.stderr.take().context("child stderr")?;
    let out = std::thread::spawn(move || {
        let mut bytes = vec![];
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = vec![];
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let bound = Instant::now() + READ_TIMEOUT;
    let budget = until.filter(|u| *u < bound);
    let until = budget.unwrap_or(bound);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > until {
            let _ = child.kill();
            let _ = child.wait();
            if budget.is_some() {
                bail!("a forge call ran past this tick's time budget");
            }
            bail!("a forge or git read took longer than {READ_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(std::process::Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// `git -C dir <args>`, prompting for nothing and running no configured
/// external program; its trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.ext.allow=never",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let output = run(command, None)?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.first().copied().unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The host and `owner/name` a remote address names: scp-like, URL or a
/// local path, whose last two segments are taken.
fn repo_of_url(url: &str) -> Option<(Option<String>, String)> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?.split(':').next()?;
        (Some(host.to_ascii_lowercase()), path.to_owned())
    } else if !url.starts_with('/')
        && let Some((authority, path)) = url.split_once(':')
    {
        let host = authority.rsplit('@').next()?;
        (Some(host.to_ascii_lowercase()), path.to_owned())
    } else {
        (None, url.to_owned())
    };
    let mut parts = path.rsplit('/').filter(|p| !p.is_empty());
    let name = parts.next()?;
    let owner = parts.next()?;
    Some((host, format!("{owner}/{name}")))
}

/// The clone of the PR's repository among the charter repository itself and
/// its `projects/` folders, matched by its origin's `owner/name`. With no
/// repository named, the only clone there is.
pub fn locate(charter_repo: &Path, requested: Option<&str>) -> Result<Clone> {
    let mut candidates = vec![charter_repo.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(charter_repo.join("projects")) {
        let mut projects: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        projects.sort();
        candidates.extend(projects);
    }
    let mut clones = vec![];
    for dir in candidates {
        if !dir.join(".git").exists() {
            continue;
        }
        let Ok(url) = git(&dir, &["remote", "get-url", "origin"]) else {
            continue;
        };
        let Some((host, repo)) = repo_of_url(&url) else {
            continue;
        };
        clones.push(Clone { dir, host, repo });
    }
    match requested {
        Some(repo) => clones
            .into_iter()
            .find(|c| c.repo.eq_ignore_ascii_case(repo))
            .with_context(|| {
                format!(
                    "no clone of {repo} in {} or its projects/ folder",
                    charter_repo.display()
                )
            }),
        None => {
            ensure!(
                clones.len() == 1,
                "pass repo (owner/name): {} has {} clones to choose from",
                charter_repo.display(),
                clones.len()
            );
            Ok(clones.remove(0))
        }
    }
}

fn sha(value: &Value, what: &str) -> Result<String> {
    let sha = value.as_str().unwrap_or_default();
    ensure!(
        sha.len() == 40
            && sha
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "the forge reported no valid {what}"
    );
    Ok(sha.to_owned())
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// The PR as the forge reports it now, its `required` checks judged at the
/// head (none judged, and `checks_ok` false, when `required` is empty), the
/// read stopped at `until` when one is given.
pub fn view(
    forge: &crate::forges::Repo,
    number: u64,
    required: &[String],
    until: Option<Instant>,
) -> Result<Pr> {
    let reply = crate::forges::call_until(
        forge,
        &forge.reviewer,
        "pr.view",
        json!({"pr": number, "required_checks": required}),
        until,
    )
    .with_context(|| format!("read #{number} in {}", forge.key))?;
    let failing: Vec<String> = reply["checks_not_success"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(Pr {
        state: if reply["merged"] == true {
            "merged".to_owned()
        } else {
            text(&reply["state"]).to_ascii_lowercase()
        },
        base: text(&reply["base"]),
        base_sha: sha(&reply["base_sha"], "base sha")?,
        head_sha: sha(&reply["head_sha"], "head sha")?,
        head_ref: text(&reply["head_ref"]),
        title: text(&reply["title"]),
        body: text(&reply["body"]),
        url: text(&reply["url"]),
        checks_ok: reply["required_checks_ok"] == true && !required.is_empty(),
        checks_detail: if failing.is_empty() {
            String::new()
        } else {
            format!("not green: {}", failing.join(", "))
        },
        merge_sha: reply["merge_sha"]
            .as_str()
            .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned),
    })
}

/// Fetch commits the clone lacks from origin: by sha, then the PR's head
/// ref, which GitHub and Forgejo both publish.
pub fn fetch(clone: &Clone, number: u64, shas: &[&str]) -> Result<()> {
    let present = |sha: &str| {
        git(
            &clone.dir,
            &["cat-file", "-e", &format!("{sha}^{{commit}}")],
        )
        .is_ok()
    };
    let missing: Vec<&str> = shas.iter().copied().filter(|s| !present(s)).collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut args = vec!["fetch", "--no-tags", "--quiet", "origin"];
    args.extend(missing.iter().copied());
    let by_sha = git(&clone.dir, &args);
    if missing.iter().any(|s| !present(s)) {
        let pull = format!("refs/pull/{number}/head");
        git(
            &clone.dir,
            &["fetch", "--no-tags", "--quiet", "origin", &pull],
        )?;
    }
    for sha in missing {
        ensure!(
            present(sha),
            "cannot fetch {sha} from origin: {}",
            by_sha
                .as_ref()
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// The tree a commit records: two heads with one tree hold the same files.
pub fn tree_of(dir: &Path, sha: &str) -> Result<String> {
    git(dir, &["rev-parse", &format!("{sha}^{{tree}}")])
}

/// When the merge-base of base and head was committed, in milliseconds. It
/// is on the protected branch, so its time is trustworthy.
pub fn merge_base_ms(dir: &Path, base: &str, head: &str) -> Result<i64> {
    let merge_base = git(dir, &["merge-base", base, head])?;
    let seconds: i64 = git(dir, &["show", "-s", "--format=%ct", &merge_base])?
        .parse()
        .context("merge-base commit time")?;
    Ok(seconds * 1000)
}

fn set_writable(path: &Path, writable: bool) {
    let Ok(info) = std::fs::symlink_metadata(path) else {
        return;
    };
    if info.file_type().is_symlink() {
        return;
    }
    if info.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        if writable {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
        }
        for entry in entries.flatten() {
            set_writable(&entry.path(), writable);
        }
    }
    let mode = info.permissions().mode();
    let mode = if writable {
        mode | 0o200
    } else {
        mode & !0o222
    };
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

/// Replace each symlink in the worktree that leaves it (or leads nowhere)
/// with a plain file saying so: the reviewer's file tools follow links, and a
/// PR may carry one to the ship user's secrets. The paths cut, relative.
fn cut_outward_links(tree: &Path, dir: &Path, cut: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let info = std::fs::symlink_metadata(&path)?;
        if info.file_type().is_symlink() {
            let inward = std::fs::canonicalize(&path).is_ok_and(|t| t.starts_with(tree));
            if !inward {
                std::fs::remove_file(&path)?;
                std::fs::write(
                    &path,
                    "(a symlink leading outside the worktree, cut by the review runtime)\n",
                )?;
                cut.push(
                    path.strip_prefix(tree)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        } else if info.is_dir() && path.file_name().is_none_or(|n| n != ".git") {
            cut_outward_links(tree, &path, cut)?;
        }
    }
    Ok(())
}

/// The review folder: a detached, read-only worktree at the head, the diff
/// from the merge-base, its file stats and the PR's text.
pub fn prepare(clone: &Clone, folder: &Path, live: &Pr, number: u64) -> Result<Files> {
    std::fs::create_dir_all(folder)?;
    let tree = folder.join("tree");
    let range = format!("{}...{}", live.base_sha, live.head_sha);
    git(
        &clone.dir,
        &[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            tree.to_str().context("review folder path is not UTF-8")?,
            &live.head_sha,
        ],
    )?;
    let mut cut = vec![];
    cut_outward_links(&std::fs::canonicalize(&tree)?, &tree, &mut cut)?;
    let diff_args = [
        "-c",
        "diff.external=",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
    ];
    let mut patch = diff_args.to_vec();
    patch.push(&range);
    let diff = folder.join("diff.patch");
    std::fs::write(&diff, git(&clone.dir, &patch)? + "\n")?;
    let mut stat = diff_args.to_vec();
    stat.extend(["--stat=200", &range]);
    let stats = folder.join("files.txt");
    std::fs::write(&stats, git(&clone.dir, &stat)? + "\n")?;
    let mut names = diff_args.to_vec();
    names.extend(["--name-status", &range]);
    let all = git(&clone.dir, &names)?;
    let count = all.lines().count();
    let mut listing: String = all
        .lines()
        .take(LISTED_FILES)
        .collect::<Vec<_>>()
        .join("\n");
    if count > LISTED_FILES {
        listing.push_str(&format!(
            "\n(and {} more, in files.txt)",
            count - LISTED_FILES
        ));
    }
    let pr_md = folder.join("pr.md");
    std::fs::write(
        &pr_md,
        format!(
            "# #{number}: {}\n\n{}\n\nBase {} at {}; head {} at {}.\n\n{}\n",
            live.title, live.url, live.base, live.base_sha, live.head_ref, live.head_sha, live.body
        ),
    )?;
    set_writable(&tree, false);
    Ok(Files {
        tree,
        diff,
        stats,
        pr_md,
        listing,
        cut,
    })
}

/// Remove a review's worktree, keeping its diff and text beside it.
pub fn remove(clone_dir: Option<&Path>, folder: &Path) {
    let tree = folder.join("tree");
    if !tree.exists() {
        return;
    }
    set_writable(&tree, true);
    let removed = clone_dir.is_some_and(|dir| {
        tree.to_str()
            .is_some_and(|t| git(dir, &["worktree", "remove", "--force", t]).is_ok())
    });
    if !removed {
        let _ = std::fs::remove_dir_all(&tree);
        if let Some(dir) = clone_dir {
            let _ = git(dir, &["worktree", "prune"]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_names_its_host_and_repository() {
        for (url, host, repo) in [
            (
                "git@github.com:kirkins/privateer.git",
                Some("github.com"),
                "kirkins/privateer",
            ),
            (
                "https://github.com/kirkins/privateer",
                Some("github.com"),
                "kirkins/privateer",
            ),
            (
                "https://user@git.example.net:3000/hermes/app.git/",
                Some("git.example.net"),
                "hermes/app",
            ),
            ("/tmp/remotes/acme/widget.git", None, "acme/widget"),
        ] {
            let (h, r) = repo_of_url(url).unwrap();
            assert_eq!(h.as_deref(), host, "{url}");
            assert_eq!(r, repo, "{url}");
        }
    }
}
