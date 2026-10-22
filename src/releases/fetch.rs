//! `privateer upgrade fetch COMMIT`: prepare the immutable `releases/COMMIT`
//! of this installation, from the CI artifact built for this platform (its
//! sha256 checked against the digest GitHub reports) or, where CI builds
//! nothing usable, from a source build of a commit on main. It switches
//! nothing: `current`, launchers, service units and running processes are
//! untouched. A release is staged and checked under `staging/<id>/` and only
//! then renamed into `releases/`, so `releases/COMMIT` is complete or absent.
use super::{BINARIES, Release, installation, running_image, valid_commit};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The repository whose CI publishes release artifacts.
pub const DEFAULT_REPO: &str = "kirkins/privateer";
/// The oldest glibc a CI Linux release runs on (it is built on Ubuntu 24.04).
pub const GLIBC_FLOOR: (u32, u32) = (2, 39);
pub const SUMS: &str = "SHA256SUMS";
/// The workflow whose push runs on main publish releases.
pub const WORKFLOW: &str = ".github/workflows/ci.yml";
/// The layout CI runs from scripts/, built in so a source build of any
/// commit is laid out and checked the same way.
const LAYOUT: &str = include_str!("../../scripts/release-layout.sh");
const USAGE: &str =
    "usage: privateer upgrade fetch COMMIT [--build] [--source CHECKOUT] [--repo OWNER/NAME]";
const API_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const BUILD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const SMOKE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_JSON: usize = 16 << 20;
const MAX_ARTIFACT: u64 = 1 << 30;
const MAX_SUMS: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub commit: String,
    pub repo: String,
    /// `--repo` was given, so a source checkout's origin must name it too.
    pub repo_given: bool,
    /// Build from source even where CI has an artifact.
    pub build: bool,
    /// The checkout a source build exports the commit from. Without it, a
    /// machine CI builds nothing for is refused rather than built.
    pub source: Option<PathBuf>,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut args = args.iter();
        let commit = args.next().context(USAGE)?.clone();
        ensure!(
            valid_commit(&commit),
            "upgrade fetch takes a full lowercase commit, not {commit:?}"
        );
        let mut options = Self {
            commit,
            repo: DEFAULT_REPO.into(),
            repo_given: false,
            build: false,
            source: None,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--build" => options.build = true,
                "--source" => {
                    let source = PathBuf::from(args.next().context(USAGE)?);
                    ensure!(source.is_absolute(), "--source must be an absolute path");
                    options.source = Some(source);
                }
                "--repo" => {
                    options.repo = args.next().context(USAGE)?.clone();
                    options.repo_given = true;
                }
                _ => bail!("{USAGE}"),
            }
        }
        ensure!(valid_repo(&options.repo), "--repo must be OWNER/NAME");
        ensure!(
            !options.build || options.source.is_some(),
            "--build needs --source CHECKOUT"
        );
        Ok(options)
    }
}

fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let part = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && !p.starts_with('.')
                && p.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
    };
    part(parts.next()) && part(parts.next()) && parts.next().is_none()
}

/// The CI platform `uname -sm` names, if CI builds one for it.
pub fn machine_platform(uname: &str) -> Option<&'static str> {
    match uname.split_whitespace().collect::<Vec<_>>()[..] {
        ["Darwin", "arm64"] => Some("macos-arm64"),
        ["Linux", "x86_64"] => Some("linux-x86_64"),
        ["Linux", "aarch64" | "arm64"] => Some("linux-aarch64"),
        _ => None,
    }
}

/// `getconf GNU_LIBC_VERSION` output ("glibc 2.39") as (major, minor).
pub fn glibc_version(reported: &str) -> Option<(u32, u32)> {
    let version = reported.trim().strip_prefix("glibc ")?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.trim().parse().ok()?;
    Some((major, minor))
}

/// The artifact platform this machine can run, or why it needs a build.
pub fn platform(uname: &str, glibc: Option<&str>) -> std::result::Result<&'static str, String> {
    let Some(platform) = machine_platform(uname) else {
        return Err(format!("CI builds no release for {}", uname.trim()));
    };
    if platform.starts_with("linux-") {
        let (major, minor) = GLIBC_FLOOR;
        match glibc.and_then(glibc_version) {
            Some(found) if found >= GLIBC_FLOOR => {}
            Some((a, b)) => {
                return Err(format!(
                    "glibc {a}.{b} is older than the {major}.{minor} CI releases need"
                ));
            }
            None => {
                return Err(format!(
                    "no glibc reported (musl, or no getconf); CI releases need glibc {major}.{minor}"
                ));
            }
        }
    }
    Ok(platform)
}

fn probe(program: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command.args(args);
    run(command, API_TIMEOUT, None)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// This machine's platform. `PRIVATEER_GLIBC` stands in for getconf's
/// report in tests ("glibc 2.17", or empty for none).
fn host_platform() -> std::result::Result<&'static str, String> {
    let uname = probe("uname", &["-sm"]).ok_or("uname -sm failed")?;
    let glibc = match std::env::var("PRIVATEER_GLIBC") {
        Ok(value) => Some(value).filter(|v| !v.is_empty()),
        Err(_) => probe("getconf", &["GNU_LIBC_VERSION"]),
    };
    platform(&uname, glibc.as_deref())
}

fn program(variable: &str, default: &str) -> std::ffi::OsString {
    std::env::var_os(variable).unwrap_or_else(|| default.into())
}

/// Run `command` in its own process group with a deadline. On timeout the
/// whole group is killed, so a build's compilers do not outlive it. Stdout
/// goes to `stdout` when given, else is kept up to MAX_JSON; the tail of
/// stderr is kept for the error.
fn run(mut command: Command, limit: Duration, stdout: Option<fs::File>) -> Result<Output> {
    command
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    let capture = stdout.is_none();
    command.stdout(match stdout {
        Some(file) => Stdio::from(file),
        None => Stdio::piped(),
    });
    let mut child = command
        .spawn()
        .map_err(|e| anyhow!("cannot run {:?}: {e}", command.get_program()))?;
    let out = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = (&mut pipe).take(MAX_JSON as u64).read_to_end(&mut bytes);
            let _ = std::io::copy(&mut pipe, &mut std::io::sink());
            bytes
        })
    });
    let mut pipe = child.stderr.take().context("child stderr")?;
    let err = std::thread::spawn(move || {
        let mut tail = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&chunk[..n]);
            if tail.len() > 16384 {
                tail.drain(..tail.len() - 8192);
            }
        }
        tail
    });
    let until = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > until {
            // SAFETY: the child leads its own process group.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            bail!(
                "{:?} took longer than {limit:?} and was stopped",
                command.get_program()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    // A daemon the command left behind (a compiler cache) may hold its
    // pipes open; what was read by then is enough.
    let settled = Instant::now() + Duration::from_secs(5);
    let join = |reader: std::thread::JoinHandle<Vec<u8>>| {
        while !reader.is_finished() && Instant::now() < settled {
            std::thread::sleep(Duration::from_millis(10));
        }
        if reader.is_finished() {
            reader.join().unwrap_or_default()
        } else {
            Vec::new()
        }
    };
    let stdout = out.map(join).unwrap_or_default();
    ensure!(
        !capture || stdout.len() < MAX_JSON,
        "{:?} wrote more than {MAX_JSON} bytes",
        command.get_program()
    );
    Ok(Output {
        status,
        stdout,
        stderr: join(err),
    })
}

fn checked(command: Command, limit: Duration, what: &str) -> Result<Output> {
    let output = run(command, limit, None).map_err(|e| anyhow!("{what} could not run: {e:#}"))?;
    ensure!(
        output.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}

fn gh_json(endpoint: &str) -> Result<Value> {
    let mut command = Command::new(program("PRIVATEER_GH", "gh"));
    command
        .args(["api", "-H", "Accept: application/vnd.github+json", endpoint])
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1");
    let output = checked(command, API_TIMEOUT, &format!("gh api {endpoint}"))?;
    serde_json::from_slice(&output.stdout).with_context(|| format!("gh api {endpoint}: not JSON"))
}

#[derive(Debug)]
struct Artifact {
    run_id: u64,
    id: u64,
    name: String,
    digest: String,
}

/// What the API has for a commit: an artifact, or why there is none (no
/// such run, the job did not succeed, the artifact expired). A malformed
/// answer, or a release job still running, is an error, never a reason to
/// build instead.
enum Lookup {
    Found(Artifact),
    Missing(String),
}

/// The CI job that builds each platform's release.
fn job_name(platform: &str) -> String {
    let (os, arch) = platform.split_once('-').unwrap_or((platform, ""));
    let os = if os == "macos" { "macOS" } else { "Linux" };
    format!("{os} {arch} release")
}

fn lookup(repo: &str, commit: &str, platform: &str) -> Result<Lookup> {
    let runs = gh_json(&format!(
        "repos/{repo}/actions/runs?head_sha={commit}&event=push&branch=main&per_page=100"
    ))?;
    let full_name = |run: &Value, key: &str| run[key]["full_name"].as_str() == Some(repo);
    // A push to main in this repository running its CI workflow; never a
    // pull request's or a fork's run, or another workflow's.
    let mut candidates: Vec<&Value> = runs["workflow_runs"]
        .as_array()
        .context("gh api: no workflow_runs")?
        .iter()
        .filter(|run| {
            run["head_sha"].as_str() == Some(commit)
                && run["event"].as_str() == Some("push")
                && run["head_branch"].as_str() == Some("main")
                && run["path"].as_str() == Some(WORKFLOW)
                && full_name(run, "repository")
                && full_name(run, "head_repository")
        })
        .collect();
    candidates.sort_by_key(|run| std::cmp::Reverse(run["id"].as_u64()));
    let job = job_name(platform);
    let name = format!("privateer-{platform}-{commit}");
    let mut missing = format!("no push run on main of {WORKFLOW} in {repo} for {commit}");
    let mut building = false;
    for run in candidates {
        let run_id = run["id"].as_u64().context("gh api: a run without an id")?;
        let jobs = gh_json(&format!(
            "repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"
        ))?;
        let found = jobs["jobs"]
            .as_array()
            .context("gh api: no jobs")?
            .iter()
            .find(|j| j["name"].as_str() == Some(&job));
        // A release job not yet listed (it waits on others) or not yet
        // completed will have an artifact: never build in its place.
        let finished = |status: &Value| status.as_str() == Some("completed");
        let running = match found {
            Some(found) => !finished(&found["status"]) || found["conclusion"].is_null(),
            None => !finished(&run["status"]),
        };
        if running {
            building = true;
            continue;
        }
        let conclusion = found.map(|j| j["conclusion"].as_str().unwrap_or("unknown"));
        if conclusion != Some("success") {
            missing = format!(
                "run {run_id}'s {job} job is {}",
                conclusion.unwrap_or("absent")
            );
            continue;
        }
        let artifacts = gh_json(&format!(
            "repos/{repo}/actions/runs/{run_id}/artifacts?name={name}"
        ))?;
        let Some(found) = artifacts["artifacts"]
            .as_array()
            .context("gh api: no artifacts")?
            .iter()
            .find(|a| a["name"].as_str() == Some(&name))
        else {
            missing = format!("run {run_id} has no artifact {name}");
            continue;
        };
        if found["expired"] != false {
            missing = format!("artifact {name} of run {run_id} has expired");
            continue;
        }
        let digest = found["digest"]
            .as_str()
            .and_then(|d| d.strip_prefix("sha256:"))
            .filter(|d| d.len() == 64 && d.bytes().all(|c| c.is_ascii_hexdigit()))
            .with_context(|| format!("artifact {name} reports no sha256 digest"))?;
        return Ok(Lookup::Found(Artifact {
            run_id,
            id: found["id"]
                .as_u64()
                .context("gh api: an artifact without an id")?,
            name,
            digest: digest.to_ascii_lowercase(),
        }));
    }
    ensure!(
        !building,
        "CI is still building {commit} for {platform}; try again when it finishes, or pass --build to build from source"
    );
    Ok(Lookup::Missing(missing))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Download the artifact's zip, check it against the API's digest, and
/// unpack its one tarball into `out` (which must be empty).
fn unpack_artifact(
    repo: &str,
    artifact: &Artifact,
    platform: &str,
    commit: &str,
    work: &Path,
    out: &Path,
) -> Result<()> {
    let zip = work.join("artifact.zip");
    let mut command = Command::new(program("PRIVATEER_GH", "gh"));
    command
        .args([
            "api",
            &format!("repos/{repo}/actions/artifacts/{}/zip", artifact.id),
        ])
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1");
    let output = run(command, DOWNLOAD_TIMEOUT, Some(fs::File::create(&zip)?))?;
    ensure!(
        output.status.success(),
        "downloading {} failed: {}",
        artifact.name,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let size = fs::metadata(&zip)?.len();
    ensure!(
        size <= MAX_ARTIFACT,
        "{} is larger than {MAX_ARTIFACT} bytes",
        artifact.name
    );
    let digest = sha256_file(&zip)?;
    ensure!(
        digest == artifact.digest,
        "{} does not match its digest: GitHub reports sha256:{}, the download is sha256:{digest}",
        artifact.name,
        artifact.digest
    );
    let unzipped = work.join("unzipped");
    fs::create_dir(&unzipped)?;
    let mut command = Command::new("python3");
    command
        .args(["-I", "-m", "zipfile", "-e"])
        .arg(&zip)
        .arg(&unzipped);
    checked(command, SMOKE_TIMEOUT, "unzipping the artifact")?;
    let tarball = format!("privateer-{platform}.tar.gz");
    let names: Vec<_> = fs::read_dir(&unzipped)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()?;
    ensure!(
        names == [std::ffi::OsString::from(&tarball)],
        "{} must hold only {tarball}",
        artifact.name
    );
    let tarball = unzipped.join(tarball);
    ensure!(
        fs::symlink_metadata(&tarball)?.is_file(),
        "{} is not a file",
        tarball.display()
    );
    untar(&tarball, &artifact.name, commit, out)
}

/// A tarball member is `<commit>` or under it: relative, no `..`.
pub fn member_ok(name: &str, commit: &str) -> bool {
    let path = Path::new(name.strip_prefix("./").unwrap_or(name));
    let mut components = path.components();
    components.next() == Some(Component::Normal(commit.as_ref()))
        && components.all(|c| matches!(c, Component::Normal(_)))
}

fn untar(tarball: &Path, label: &str, commit: &str, out: &Path) -> Result<()> {
    let mut list = Command::new("tar");
    list.arg("-tzf").arg(tarball);
    let listing = checked(list, SMOKE_TIMEOUT, "listing the release tarball")?;
    let listing = String::from_utf8(listing.stdout).context("tarball names are not UTF-8")?;
    for name in listing.lines() {
        ensure!(
            member_ok(name, commit),
            "{label} holds {name:?}, outside its one {commit}/ directory"
        );
    }
    let mut extract = Command::new("tar");
    extract.arg("-xzf").arg(tarball).arg("-C").arg(out);
    checked(extract, SMOKE_TIMEOUT, "unpacking the release tarball")?;
    Ok(())
}

/// Every file in `root`, relative, refusing anything but plain files and
/// directories (no links, devices, or files hard-linked elsewhere).
fn files(root: &Path) -> Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(root.join(&dir))? {
            let entry = entry?;
            let relative = dir.join(entry.file_name());
            let meta = fs::symlink_metadata(entry.path())?;
            let shown = relative.display();
            if meta.is_dir() {
                pending.push(relative);
            } else if meta.is_file() {
                ensure!(
                    meta.nlink() == 1,
                    "{shown} in {} is hard-linked",
                    root.display()
                );
                found.insert(
                    relative
                        .to_str()
                        .with_context(|| format!("{shown} is not UTF-8"))?
                        .to_owned(),
                );
            } else {
                bail!(
                    "{} holds {shown}, which is not a plain file or directory",
                    root.display()
                );
            }
        }
    }
    Ok(found)
}

/// Parse SHA256SUMS: `<64 hex>  <relative path>` (or ` *` for binary mode).
pub fn parse_sums(text: &str) -> Result<BTreeMap<String, String>> {
    let mut sums = BTreeMap::new();
    for line in text.lines() {
        let (digest, name) = line
            .split_at_checked(64)
            .with_context(|| format!("bad {SUMS} line {line:?}"))?;
        let name = name
            .strip_prefix("  ")
            .or_else(|| name.strip_prefix(" *"))
            .with_context(|| format!("bad {SUMS} line {line:?}"))?;
        ensure!(
            digest.bytes().all(|c| c.is_ascii_hexdigit())
                && !name.is_empty()
                && Path::new(name)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "bad {SUMS} line {line:?}"
        );
        ensure!(
            sums.insert(name.to_owned(), digest.to_ascii_lowercase())
                .is_none(),
            "{SUMS} lists {name} twice"
        );
    }
    Ok(sums)
}

/// Check a release's layout and its SHA256SUMS, which must match exactly:
/// every listed file, and nothing else. The bundled plugins never write
/// bytecode beside themselves, so a `__pycache__` in a release was put there.
pub fn verify(root: &Path) -> Result<()> {
    let shown = root.display();
    for name in BINARIES {
        let path = root.join("bin").join(name);
        let meta =
            fs::symlink_metadata(&path).with_context(|| format!("{shown} has no bin/{name}"))?;
        ensure!(
            meta.is_file() && meta.permissions().mode() & 0o111 != 0,
            "{shown}: bin/{name} is not an executable file"
        );
    }
    let plugins = root.join("share/privateer/plugins");
    let manifests = fs::read_dir(&plugins)
        .with_context(|| format!("{shown} has no share/privateer/plugins"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("manifest.json").is_file())
        .count();
    ensure!(manifests > 0, "{shown} has no plugin manifests");
    let sums_path = root.join(SUMS);
    let meta =
        fs::symlink_metadata(&sums_path).with_context(|| format!("{shown} has no {SUMS}"))?;
    ensure!(
        meta.is_file() && meta.len() <= MAX_SUMS,
        "{shown}: {SUMS} is not a small file"
    );
    let sums = parse_sums(&fs::read_to_string(&sums_path)?)?;
    let mut present = files(root)?;
    present.remove(SUMS);
    for name in sums.keys() {
        ensure!(present.contains(name), "{shown}: {name} is missing");
    }
    if let Some(name) = present.iter().find(|n| !sums.contains_key(*n)) {
        bail!("{shown}: {name} is not in {SUMS}");
    }
    for (name, digest) in &sums {
        ensure!(
            &sha256_file(&root.join(name))? == digest,
            "{shown}: {name} does not match {SUMS}"
        );
    }
    Ok(())
}

/// The environment a staged release runs in for its checks: a throwaway
/// HOME, configuration, fleet registry and ledger, no service manager,
/// nothing Privateer-specific from the caller, and no Python bytecode
/// written into the release.
fn sandboxed(program: impl AsRef<std::ffi::OsStr>, sandbox: &Path) -> Command {
    let mut command = Command::new(program);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("PRIVATEER_") {
            command.env_remove(name);
        }
    }
    command
        .current_dir(sandbox)
        .env("HOME", sandbox.join("user"))
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("PRIVATEER_FLEET", sandbox.join("fleet.sqlite"))
        .env("PRIVATEER_RESOURCE_HOME", sandbox.join("ledger"))
        .env("PRIVATEER_SERVICE", "none")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_SOCKET_PATH");
    command
}

/// Run the staged release before anything can move to it: it must know
/// itself as `commit`, initialise and read a throwaway home, and every
/// binary must load. A wrong-platform build fails here.
fn smoke(root: &Path, commit: &str, sandbox: &Path) -> Result<()> {
    fs::create_dir_all(sandbox.join("user"))?;
    let bin = |name: &str| root.join("bin").join(name);
    let mut status = sandboxed(bin("privateer"), sandbox);
    status.args(["upgrade", "status"]);
    let output = checked(status, SMOKE_TIMEOUT, "the staged privateer upgrade status")?;
    let reported: Value = serde_json::from_slice(&output.stdout)
        .context("the staged privateer upgrade status is not JSON")?;
    ensure!(
        reported["running_commit"].as_str() == Some(commit),
        "the staged privateer reports itself as {}, not {commit}",
        reported["running_commit"]
    );
    let home = sandbox.join("home");
    for command in ["init", "status"] {
        let mut run = sandboxed(bin("privateer"), sandbox);
        run.arg("--home").arg(&home).args([command, "--json", "{}"]);
        checked(
            run,
            SMOKE_TIMEOUT,
            &format!("the staged privateer {command}"),
        )?;
    }
    let mut dashboard = sandboxed(bin("privateer-dashboard"), sandbox);
    dashboard.arg("--help");
    checked(dashboard, SMOKE_TIMEOUT, "the staged privateer-dashboard")?;
    let mut broker = sandboxed(bin("privateer-mcp-broker"), sandbox);
    broker.arg("--version");
    checked(broker, SMOKE_TIMEOUT, "the staged privateer-mcp-broker")?;
    Ok(())
}

fn git(dir: &Path, args: &[&str], limit: Duration) -> Result<Output> {
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
    checked(command, limit, &format!("git {}", args.join(" ")))
}

/// Whether a remote URL names `repo` (OWNER/NAME) as its last two path
/// segments, in any of Git's URL forms, a `.git` suffix aside.
pub fn origin_names(url: &str, repo: &str) -> bool {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let mut segments = url.rsplit(['/', ':']);
    let (Some(name), Some(owner)) = (segments.next(), segments.next()) else {
        return false;
    };
    format!("{owner}/{name}").eq_ignore_ascii_case(repo)
}

/// The machine's one source build: every build shares `<data>/build/target`
/// (incremental reuse), so two at once could lay out each other's binaries.
/// Held from the checkout's fetch through the layout; a second build is
/// refused, naming the holder.
fn build_lock(data: &Path, commit: &str) -> Result<fs::File> {
    use std::io::{Seek, Write};
    use std::os::fd::AsRawFd;
    let dir = data.join("build");
    fs::create_dir_all(&dir)?;
    let path = dir.join("lock");
    let mut lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let mut holder = String::new();
        let _ = lock.read_to_string(&mut holder);
        let holder = holder.trim();
        bail!(
            "another source build holds {} ({}); try again when it finishes",
            path.display(),
            if holder.is_empty() {
                "holder unknown"
            } else {
                holder
            }
        );
    }
    lock.set_len(0)?;
    lock.rewind()?;
    write!(lock, "pid {} building {commit}", std::process::id())?;
    Ok(lock)
}

/// Build `commit` from `checkout` into `out/<commit>`. The checkout is the
/// operator's choice: its origin is trusted for main (and, with `--repo`,
/// must name that repository). The commit must be on that origin's main; it
/// is exported with `git archive`, so the checkout's working tree and index
/// are never touched, and built with `cargo` (`PRIVATEER_CARGO` in tests)
/// under the build lock.
fn build(options: &Options, commit: &str, data: &Path, work: &Path, out: &Path) -> Result<()> {
    let checkout = options.source.as_deref().context("--source CHECKOUT")?;
    if options.repo_given {
        let url = git(checkout, &["remote", "get-url", "origin"], API_TIMEOUT)
            .map_err(|e| anyhow!("the source checkout has no origin: {e:#}"))?;
        let url = String::from_utf8_lossy(&url.stdout).trim().to_owned();
        ensure!(
            origin_names(&url, &options.repo),
            "the source checkout's origin {url} is not {}; build from a checkout of it, or drop --repo",
            options.repo
        );
    }
    let _lock = build_lock(data, commit)?;
    git(
        checkout,
        &[
            "fetch",
            "--quiet",
            "origin",
            "+refs/heads/main:refs/remotes/origin/main",
        ],
        DOWNLOAD_TIMEOUT,
    )
    .map_err(|e| anyhow!("cannot refresh origin/main in the source checkout: {e:#}"))?;
    git(
        checkout,
        &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        API_TIMEOUT,
    )
    .with_context(|| format!("{commit} is not a commit in {}", checkout.display()))?;
    git(
        checkout,
        &[
            "merge-base",
            "--is-ancestor",
            commit,
            "refs/remotes/origin/main",
        ],
        API_TIMEOUT,
    )
    .with_context(|| format!("{commit} is not on origin/main; only main is released"))?;
    let archive = work.join("source.tar");
    let archive_arg = archive.to_str().context("staging path is not UTF-8")?;
    git(
        checkout,
        &["archive", "--format=tar", "-o", archive_arg, commit],
        DOWNLOAD_TIMEOUT,
    )?;
    let source = work.join("source");
    fs::create_dir(&source)?;
    let mut extract = Command::new("tar");
    extract.arg("-xf").arg(&archive).arg("-C").arg(&source);
    checked(extract, SMOKE_TIMEOUT, "unpacking the source")?;
    let target = data.join("build/target");
    fs::create_dir_all(&target)?;
    let mut cargo = Command::new(program("PRIVATEER_CARGO", "cargo"));
    cargo
        .args(["build", "--release", "--locked", "--features", "dashboard"])
        .current_dir(&source)
        .env("CARGO_TARGET_DIR", &target);
    checked(cargo, BUILD_TIMEOUT, "the source build")?;
    let script = work.join("release-layout.sh");
    fs::write(&script, LAYOUT)?;
    let sandbox = work.join("layout");
    fs::create_dir_all(sandbox.join("user"))?;
    let mut layout = sandboxed("sh", &sandbox);
    layout
        .arg(&script)
        .arg(commit)
        .arg(target.join("release"))
        .arg(source.join("plugins"))
        .arg(out);
    checked(layout, SMOKE_TIMEOUT, "laying out the release")?;
    Ok(())
}

/// An existing `releases/<commit>`: a real directory (never a link to one)
/// whose SHA256SUMS checks out. `when` says how it was found.
fn existing(root: &Path, when: &str) -> Result<()> {
    let shown = root.display();
    let meta = fs::symlink_metadata(root).with_context(|| format!("{shown} is missing"))?;
    ensure!(
        meta.is_dir(),
        "{shown} {when} but is not a directory (a link or a file); nothing was overwritten"
    );
    verify(root).map_err(|e| {
        anyhow!("{shown} {when} but is not an intact release ({e:#}); nothing was overwritten")
    })
}

/// Removes a staging directory however staging ends.
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn python_present() -> bool {
    probe("python3", &["-I", "-c", "import zipfile"]).is_some()
}

/// Prepare `releases/<commit>`, switching nothing.
pub fn fetch(options: &Options) -> Result<Value> {
    let running = Release::from_executable(&running_image()?);
    let data = installation(running.as_ref())?;
    let release = Release::new(&data, &options.commit)?;
    let commit = &release.commit;
    let report = |from: &str, extra: Value| {
        let mut out = json!({"commit": commit, "release": release.root, "from": from,
            "switched": false});
        if let (Value::Object(out), Value::Object(extra)) = (&mut out, extra) {
            out.extend(extra);
        }
        out
    };
    // Releases never change: one already here is reused only if it checks out.
    if fs::symlink_metadata(&release.root).is_ok() {
        existing(&release.root, "exists")?;
        return Ok(report("existing", json!({})));
    }
    ensure!(
        python_present(),
        "python3 is not on PATH; the bundled plugins need it"
    );
    let platform = host_platform();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let staging = Staging(data.join("staging").join(&id));
    fs::create_dir_all(&staging.0)?;
    fs::set_permissions(&staging.0, fs::Permissions::from_mode(0o700))?;
    let out = staging.0.join("releases");
    fs::create_dir(&out)?;
    let build_reason = if options.build {
        Some("--build".to_owned())
    } else {
        match &platform {
            Err(reason) => Some(reason.clone()),
            Ok(platform) => match lookup(&options.repo, commit, platform)? {
                Lookup::Missing(reason) => Some(reason),
                Lookup::Found(artifact) => {
                    unpack_artifact(&options.repo, &artifact, platform, commit, &staging.0, &out)?;
                    let extra = json!({"platform": platform, "artifact": {
                        "repo": options.repo, "run_id": artifact.run_id, "id": artifact.id,
                        "name": artifact.name, "digest": format!("sha256:{}", artifact.digest)}});
                    return publish(&release, &out, &staging.0, report("artifact", extra));
                }
            },
        }
    };
    let reason = build_reason.unwrap_or_default();
    let Some(source) = &options.source else {
        bail!("{commit} needs a source build ({reason}); pass --source CHECKOUT to build it");
    };
    build(options, commit, &data, &staging.0, &out)?;
    let extra = json!({"platform": platform.ok(), "build_reason": reason, "source": source});
    publish(&release, &out, &staging.0, report("build", extra))
}

/// Check and smoke the staged release, then rename it into `releases/`. A
/// release that appeared meanwhile is reused if intact; nothing is replaced.
fn publish(release: &Release, out: &Path, work: &Path, report: Value) -> Result<Value> {
    let names: Vec<_> = fs::read_dir(out)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()?;
    ensure!(
        names == [std::ffi::OsString::from(&release.commit)],
        "the release must unpack to exactly one {}/ directory",
        release.commit
    );
    let staged = out.join(&release.commit);
    ensure!(
        fs::symlink_metadata(&staged)?.is_dir(),
        "{} is not a directory",
        staged.display()
    );
    verify(&staged)?;
    smoke(&staged, &release.commit, &work.join("smoke"))?;
    let releases = release.root.parent().context("releases directory")?;
    fs::create_dir_all(releases)?;
    if let Err(error) = fs::rename(&staged, &release.root) {
        if fs::symlink_metadata(&release.root).is_err() {
            bail!("cannot publish the staged release: {error}");
        }
        existing(&release.root, "appeared while staging")?;
        let mut report = report;
        report["from"] = json!("existing");
        return Ok(report);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn platforms_and_glibc_floor() {
        let new = Some("glibc 2.39");
        assert_eq!(platform("Darwin arm64", None), Ok("macos-arm64"));
        assert_eq!(platform("Linux x86_64", new), Ok("linux-x86_64"));
        assert_eq!(
            platform("Linux aarch64\n", Some("glibc 2.41")),
            Ok("linux-aarch64")
        );
        assert!(
            platform("Darwin x86_64", None)
                .unwrap_err()
                .contains("no release")
        );
        assert!(
            platform("Linux x86_64", Some("glibc 2.35"))
                .unwrap_err()
                .contains("2.35")
        );
        assert!(platform("Linux x86_64", None).unwrap_err().contains("musl"));
        assert_eq!(glibc_version("glibc 2.39\n"), Some((2, 39)));
        assert_eq!(glibc_version("glibc 10.1"), Some((10, 1)));
        assert_eq!(glibc_version("musl"), None);
        assert_eq!(job_name("macos-arm64"), "macOS arm64 release");
        assert_eq!(job_name("linux-aarch64"), "Linux aarch64 release");
    }

    #[test]
    fn tar_members_stay_in_the_commit_directory() {
        for ok in [
            C,
            "0123456789abcdef0123456789abcdef01234567/",
            &format!("{C}/bin/privateer"),
            &format!("./{C}/SHA256SUMS"),
        ] {
            assert!(member_ok(ok, C), "{ok}");
        }
        for bad in [
            "/etc/passwd",
            "..",
            &format!("{C}/../x"),
            "other/x",
            "",
            &format!("/{C}/x"),
        ] {
            assert!(!member_ok(bad, C), "{bad}");
        }
    }

    #[test]
    fn sums_parse_strictly() {
        let digest = "a".repeat(64);
        let sums = parse_sums(&format!("{digest}  bin/privateer\n{digest} *share/x\n")).unwrap();
        assert_eq!(sums.len(), 2);
        assert!(parse_sums(&format!("{digest}  ../x\n")).is_err());
        assert!(parse_sums(&format!("{digest}  /x\n")).is_err());
        assert!(parse_sums(&format!("{digest} x\n")).is_err());
        assert!(parse_sums(&format!("{digest}  x\n{digest}  x\n")).is_err());
        assert!(parse_sums("short  x\n").is_err());
    }

    #[test]
    fn options_parse() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let parsed = Options::parse(&args(&[C])).unwrap();
        assert_eq!(parsed.repo, DEFAULT_REPO);
        assert!(!parsed.build && parsed.source.is_none());
        let parsed =
            Options::parse(&args(&[C, "--build", "--source", "/src", "--repo", "a/b"])).unwrap();
        assert!(parsed.build && parsed.repo_given);
        assert_eq!(parsed.repo, "a/b");
        assert!(!Options::parse(&args(&[C])).unwrap().repo_given);
        assert!(Options::parse(&args(&["main"])).is_err());
        assert!(Options::parse(&args(&[C, "--build"])).is_err());
        assert!(Options::parse(&args(&[C, "--source", "rel"])).is_err());
        assert!(Options::parse(&args(&[C, "--repo", "a/../b"])).is_err());
        assert!(Options::parse(&args(&[C, "--repo", "a"])).is_err());
    }

    #[test]
    fn an_origin_url_names_its_repository() {
        for url in [
            "https://github.com/owner/name.git",
            "https://github.com/owner/name",
            "https://github.com/Owner/Name/",
            "git@github.com:owner/name.git",
            "ssh://git@github.com/owner/name.git",
            "/srv/git/owner/name.git",
        ] {
            assert!(origin_names(url, "owner/name"), "{url}");
        }
        for url in [
            "https://github.com/someone/name.git",
            "https://github.com/owner/name-fork",
            "https://github.com/owner/name/other",
            "git@github.com:name.git",
            "/srv/origin",
            "",
        ] {
            assert!(!origin_names(url, "owner/name"), "{url}");
        }
    }
}
