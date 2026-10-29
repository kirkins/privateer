//! The pull request a review or a merge check reads, read by the runtime
//! itself, never taken from the ship, and the local checkout it is read in.
//!
//! GitHub is reached through the ship user's own `gh` login: `gh pr view`
//! for the PR and `gh pr checks` for its checks at the head, `gh api` for a
//! review's comment and `gh pr merge` at the exact head. Another forge uses
//! the captain's `permissions.set` commands under `forges.<forge>`:
//! `read_pr` for `pr.view` (including checks), `merge_pr` for `pr.merge`,
//! and `publish_review` for `pr.comment`. Each runs as `<command> <op>`
//! with JSON on stdin, as `plugins/forge.py` does. A debug build may name a fake
//! adapter in `PRIVATEER_FORGE_ADAPTER`, run as `<adapter> <op>` the same
//! way; a release build ignores the variable, so a ship cannot swap the
//! forge by its environment.
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
const LISTED_FILES: usize = 200;

/// The local clone of a PR's repository: its folder, its forge kind and its
/// `owner/name`.
#[derive(Clone, Debug)]
pub struct Clone {
    pub dir: PathBuf,
    pub forge: String,
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
}

/// The files a review folder holds beside its worktree.
pub struct Files {
    pub tree: PathBuf,
    pub diff: PathBuf,
    pub stats: PathBuf,
    pub pr_md: PathBuf,
    /// `git diff --name-status` of the change, bounded for the prompt.
    pub listing: String,
}

/// Run a command with a bounded wait, its stdin given and its stdout and
/// stderr read whole.
fn run(mut command: Command, input: Option<&[u8]>) -> Result<std::process::Output> {
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
    let until = Instant::now() + READ_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > until {
            let _ = child.kill();
            let _ = child.wait();
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
        let forge = git(&dir, &["config", "--get", "privateer.forge"])
            .ok()
            .filter(|f| !f.is_empty())
            .or_else(|| (host.as_deref() == Some("github.com")).then(|| "github".to_owned()))
            .unwrap_or_default();
        clones.push(Clone { dir, forge, repo });
    }
    let found = match requested {
        Some(repo) => clones
            .into_iter()
            .find(|c| c.repo.eq_ignore_ascii_case(repo))
            .with_context(|| {
                format!(
                    "no clone of {repo} in {} or its projects/ folder",
                    charter_repo.display()
                )
            })?,
        None => {
            ensure!(
                clones.len() == 1,
                "pass repo (owner/name): {} has {} clones to choose from",
                charter_repo.display(),
                clones.len()
            );
            clones.remove(0)
        }
    };
    ensure!(
        !found.forge.is_empty(),
        "the forge of {} is unknown: set it with git config privateer.forge github",
        found.repo
    );
    Ok(found)
}

/// The branch the charter's PRs merge into: origin's default branch.
pub fn default_branch(dir: &Path) -> Result<String> {
    if let Ok(head) = git(
        dir,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        return Ok(head.strip_prefix("origin/").unwrap_or(&head).to_owned());
    }
    let advertised = git(dir, &["ls-remote", "--symref", "origin", "HEAD"])?;
    advertised
        .lines()
        .find_map(|line| {
            line.strip_prefix("ref: refs/heads/")
                .and_then(|s| s.split_whitespace().next())
                .map(str::to_owned)
        })
        .context("cannot determine origin's default branch")
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

/// The fake adapter a debug build may be given.
fn test_adapter() -> Option<Vec<String>> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var("PRIVATEER_FORGE_ADAPTER")
        .ok()
        .filter(|a| !a.is_empty())
        .map(|a| vec![a])
}

/// Run `<argv> <op>` with the request on stdin; its JSON reply, which must
/// say `ok`.
fn adapter_call(argv: &[String], op: &str, request: &Value, what: &str) -> Result<Value> {
    let (program, rest) = argv
        .split_first()
        .context("the forge adapter names no command")?;
    let mut command = Command::new(program);
    command.args(rest).arg(op);
    let output = run(command, Some(request.to_string().as_bytes()))?;
    let reply: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("the forge adapter's {op} reply is not JSON"))?;
    ensure!(
        reply["ok"] == true,
        "the forge adapter could not {what}: {}",
        reply["error"].as_str().unwrap_or("no reason given")
    );
    Ok(reply)
}

/// The command the captain configured for a forge's action: its words, a
/// trailing ` *` (as a tool rule ends) dropped. Not a shell line: no quoting.
fn configured(forge: &str, action: &str) -> Result<(Vec<String>, Option<String>)> {
    let policy = crate::permissions::load()?;
    let entry = &policy["forges"][forge];
    let rule = entry[action].as_str().with_context(|| {
        format!(
            "no {action} command is configured for the forge {forge}: set it with \
             permissions.set {{\"forges\": {{\"{forge}\": {{\"{action}\": \"<command>\"}}}}}}"
        )
    })?;
    let rule = rule.strip_suffix(" *").unwrap_or(rule);
    let rule = rule
        .strip_prefix("Bash(")
        .and_then(|r| r.strip_suffix(')'))
        .unwrap_or(rule);
    let argv: Vec<String> = rule.split_whitespace().map(str::to_owned).collect();
    ensure!(
        !argv.is_empty(),
        "the {action} command for {forge} is empty"
    );
    Ok((argv, entry["api"].as_str().map(str::to_owned)))
}

/// Post one comment on the PR, unless one carrying `marker` is already
/// there; the comment's forge id.
pub fn comment(clone: &Clone, number: u64, marker: &str, body: &str) -> Result<String> {
    let request = json!({"forge": clone.forge, "repo": clone.repo, "pr": number,
        "marker": marker, "body": body});
    let posted = |argv: &[String], request: &Value| -> Result<String> {
        let reply = adapter_call(argv, "pr.comment", request, "post the comment")?;
        Ok(match &reply["comment_id"] {
            Value::String(id) => id.clone(),
            Value::Null => String::new(),
            id => id.to_string(),
        })
    };
    if let Some(adapter) = test_adapter() {
        return posted(&adapter, &request);
    }
    if clone.forge == "github" {
        return comment_gh(clone, number, marker, body);
    }
    let (argv, api) = configured(&clone.forge, "publish_review")?;
    let mut request = request;
    if let Some(api) = api {
        request["api"] = json!(api);
    }
    posted(&argv, &request)
}

fn comment_gh(clone: &Clone, number: u64, marker: &str, body: &str) -> Result<String> {
    let path = format!("repos/{}/issues/{number}/comments", clone.repo);
    // The marker is the runtime's own hex digest: nothing to escape.
    let select = format!(".[] | select((.body // \"\") | contains(\"{marker}\")) | .id");
    let (ok, stdout, stderr) = gh(&["api", "--paginate", &path, "--jq", &select])?;
    ensure!(
        ok,
        "gh could not read the comments on #{number} in {}: {stderr}",
        clone.repo
    );
    if let Some(id) = String::from_utf8_lossy(&stdout).split_whitespace().next() {
        return Ok(id.to_owned());
    }
    let mut command = Command::new("gh");
    command
        .args(["api", "--method", "POST", &path, "--input", "-"])
        .env("GH_PROMPT_DISABLED", "1");
    let output = run(command, Some(json!({"body": body}).to_string().as_bytes()))
        .context("run gh to post the review comment")?;
    ensure!(
        output.status.success(),
        "gh could not comment on #{number} in {}: {}",
        clone.repo,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let posted: Value =
        serde_json::from_slice(&output.stdout).context("gh api did not reply JSON")?;
    Ok(posted["id"].to_string())
}

/// Merge the PR at exactly `head`, so a push since it was read makes the
/// forge refuse; the merge commit, when the forge names it.
pub fn merge(clone: &Clone, number: u64, head: &str) -> Result<Option<String>> {
    let adapter = test_adapter();
    if adapter.is_some() || clone.forge != "github" {
        let (argv, api) = match adapter {
            Some(argv) => (argv, None),
            None => configured(&clone.forge, "merge_pr")?,
        };
        let mut request = json!({"forge": clone.forge, "repo": clone.repo, "pr": number,
            "head_sha": head, "merge_method": "merge"});
        if let Some(api) = api {
            request["api"] = json!(api);
        }
        let reply = adapter_call(&argv, "pr.merge", &request, "merge")?;
        ensure!(
            reply["merged"] != false,
            "the forge adapter did not merge #{number}"
        );
        return Ok(reply["merge_sha"].as_str().map(str::to_owned));
    }
    let n = number.to_string();
    let (ok, _, stderr) = gh(&[
        "pr",
        "merge",
        &n,
        "--repo",
        &clone.repo,
        "--merge",
        "--match-head-commit",
        head,
    ])?;
    ensure!(ok, "gh could not merge #{n} in {}: {stderr}", clone.repo);
    let (ok, stdout, _) = gh(&[
        "pr",
        "view",
        &n,
        "--repo",
        &clone.repo,
        "--json",
        "mergeCommit",
        "--jq",
        ".mergeCommit.oid",
    ])?;
    let sha = String::from_utf8_lossy(&stdout).trim().to_owned();
    Ok((ok && !sha.is_empty()).then_some(sha))
}

/// The PR as the forge reports it now.
pub fn view(clone: &Clone, number: u64) -> Result<Pr> {
    if let Some(adapter) = test_adapter() {
        return view_adapter(&adapter, None, clone, number);
    }
    if clone.forge == "github" {
        return view_gh(clone, number);
    }
    let (argv, api) = configured(&clone.forge, "read_pr")?;
    view_adapter(&argv, api, clone, number)
}

fn view_adapter(argv: &[String], api: Option<String>, clone: &Clone, number: u64) -> Result<Pr> {
    let mut request = json!({"forge": clone.forge, "repo": clone.repo, "pr": number});
    if clone.forge != "github" {
        request["discover_checks"] = json!(true);
    }
    if let Some(api) = api {
        request["api"] = json!(api);
    }
    let reply = adapter_call(argv, "pr.view", &request, &format!("read #{number}"))?;
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
        state: text(&reply["state"]).to_ascii_lowercase(),
        base: text(&reply["base"]),
        base_sha: sha(&reply["base_sha"], "base sha")?,
        head_sha: sha(&reply["head_sha"], "head sha")?,
        head_ref: text(&reply["head_ref"]),
        title: text(&reply["title"]),
        body: text(&reply["body"]),
        url: text(&reply["url"]),
        checks_ok: reply["required_checks_ok"] == true,
        checks_detail: if failing.is_empty() {
            String::new()
        } else {
            format!("not green: {}", failing.join(", "))
        },
    })
}

fn gh(args: &[&str]) -> Result<(bool, Vec<u8>, String)> {
    let mut command = Command::new("gh");
    command.args(args).env("GH_PROMPT_DISABLED", "1");
    let output = run(command, None).context("run gh to read the pull request")?;
    Ok((
        output.status.success(),
        output.stdout,
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

fn view_gh(clone: &Clone, number: u64) -> Result<Pr> {
    let n = number.to_string();
    let (ok, stdout, stderr) = gh(&[
        "pr",
        "view",
        &n,
        "--repo",
        &clone.repo,
        "--json",
        "state,baseRefName,baseRefOid,headRefName,headRefOid,title,body,url",
    ])?;
    ensure!(ok, "gh could not read #{n} in {}: {stderr}", clone.repo);
    let pr: Value = serde_json::from_slice(&stdout).context("gh pr view did not reply JSON")?;
    // The required checks, as the forge's branch protection names them; with
    // none required, every check at the head.
    let checks = |required: bool| -> Result<Option<Vec<Value>>> {
        let mut args = vec!["pr", "checks", &n, "--repo", &clone.repo];
        if required {
            args.push("--required");
        }
        args.extend(["--json", "name,bucket"]);
        let (_, stdout, _) = gh(&args)?;
        Ok(serde_json::from_slice::<Vec<Value>>(&stdout)
            .ok()
            .filter(|c| !c.is_empty()))
    };
    let found = match checks(true)? {
        Some(found) => Some(found),
        None => checks(false)?,
    };
    let (checks_ok, checks_detail) = match found {
        None => (false, "no checks reported at the head".to_owned()),
        Some(found) => {
            let failing: Vec<String> = found
                .iter()
                .filter(|c| !matches!(c["bucket"].as_str(), Some("pass" | "skipping")))
                .map(|c| format!("{} ({})", text(&c["name"]), text(&c["bucket"])))
                .collect();
            (
                failing.is_empty(),
                if failing.is_empty() {
                    String::new()
                } else {
                    format!("not green: {}", failing.join(", "))
                },
            )
        }
    };
    Ok(Pr {
        state: text(&pr["state"]).to_ascii_lowercase(),
        base: text(&pr["baseRefName"]),
        base_sha: sha(&pr["baseRefOid"], "base sha")?,
        head_sha: sha(&pr["headRefOid"], "head sha")?,
        head_ref: text(&pr["headRefName"]),
        title: text(&pr["title"]),
        body: text(&pr["body"]),
        url: text(&pr["url"]),
        checks_ok,
        checks_detail,
    })
}

/// Fetch commits the clone lacks from origin: by sha, then, on GitHub, the
/// PR's head ref.
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
    if clone.forge == "github" && missing.iter().any(|s| !present(s)) {
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
