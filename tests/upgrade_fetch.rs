//! `privateer upgrade fetch`: a fake `gh` (PRIVATEER_GH) serves canned run,
//! job and artifact JSON and a zip built here from copied test binaries; a
//! fake `cargo` (PRIVATEER_CARGO) builds over a fixture Git repository with
//! an `origin`. Every run has its own HOME, data directory and registry; no
//! test reaches the network, the installed Privateer or a service manager.
mod support;

use privateer::releases::fetch::{SUMS, machine_platform};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use support::BIN;

const C: &str = "3333333333333333333333333333333333333333";
const OLD: &str = "1111111111111111111111111111111111111111";
const REPO: &str = "kirkins/privateer";
const BROKER: &str = env!("CARGO_BIN_EXE_privateer-mcp-broker");

const FAKE_GH: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys
d = pathlib.Path(os.environ["FAKE_GH"])
with open(d / "calls.log", "a") as f:
    f.write(json.dumps(sys.argv[1:]) + "\n")
endpoint = sys.argv[-1]
routes = json.loads((d / "routes.json").read_text())
if sys.argv[1] != "api" or endpoint not in routes:
    sys.stderr.write("gh: Not Found (HTTP 404) " + endpoint + "\n")
    sys.exit(1)
sys.stdout.buffer.write((d / routes[endpoint]).read_bytes())
"#;

const FAKE_CARGO: &str = r#"#!/bin/sh
set -e
[ "$1" = build ] || exit 2
[ -f Cargo.toml ] || { echo "not run in the exported source" >&2; exit 3; }
if [ -n "$FAKE_CARGO_FAIL" ]; then echo "error: could not compile privateer" >&2; exit 101; fi
out="$CARGO_TARGET_DIR/release"
mkdir -p "$out"
cp "$FAKE_PRIVATEER" "$out/privateer"
cp "$FAKE_BROKER" "$out/privateer-mcp-broker"
printf '#!/bin/sh\nexit 0\n' > "$out/privateer-dashboard"
chmod +x "$out/privateer-dashboard"
"#;

fn platform() -> Option<&'static str> {
    let output = Command::new("uname").arg("-sm").output().unwrap();
    machine_platform(&String::from_utf8_lossy(&output.stdout))
}

fn job_name(platform: &str) -> &'static str {
    match platform {
        "macos-arm64" => "macOS arm64 release",
        "linux-x86_64" => "Linux x86_64 release",
        _ => "Linux aarch64 release",
    }
}

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn sha256(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(root, &path, out);
        } else {
            out.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
}

/// SHA256SUMS over every file but itself, as the layout script writes it.
fn write_sums(root: &Path) {
    let mut names = Vec::new();
    walk(root, root, &mut names);
    names.retain(|n| n != SUMS);
    names.sort();
    let text: String = names
        .iter()
        .map(|n| format!("{}  {n}\n", sha256(&root.join(n))))
        .collect();
    fs::write(root.join(SUMS), text).unwrap();
}

/// A complete release tree at `parent/<commit>`, from copied test binaries.
fn tree(parent: &Path, commit: &str) -> PathBuf {
    let root = parent.join(commit);
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::copy(BIN, root.join("bin/privateer")).unwrap();
    fs::copy(BROKER, root.join("bin/privateer-mcp-broker")).unwrap();
    executable(&root.join("bin/privateer-dashboard"), "#!/bin/sh\nexit 0\n");
    let plugins = root.join("share/privateer/plugins/pi");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        plugins.join("manifest.json"),
        json!({"schema_version":1,"id":"pi","executable":["python3","adapter.py"]}).to_string(),
    )
    .unwrap();
    write_sums(&root);
    root
}

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("gh")).unwrap();
        fs::create_dir_all(root.join("user")).unwrap();
        executable(&root.join("gh/gh"), FAKE_GH);
        executable(&root.join("cargo"), FAKE_CARGO);
        fs::write(root.join("gh/routes.json"), "{}").unwrap();
        Self { _dir: dir, root }
    }

    fn data(&self) -> PathBuf {
        self.root.join("data/privateer")
    }

    fn release(&self, commit: &str) -> PathBuf {
        self.data().join("releases").join(commit)
    }

    /// An existing release selected by `current`, which fetch must not move.
    fn select_old(&self) {
        let old = self.release(OLD);
        fs::create_dir_all(old.join("bin")).unwrap();
        symlink(&old, self.data().join("current")).unwrap();
    }

    fn current(&self) -> PathBuf {
        fs::read_link(self.data().join("current")).unwrap()
    }

    fn command(&self) -> Command {
        let mut command = support::command(BIN);
        command
            .current_dir(&self.root)
            .env("HOME", self.root.join("user"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_GH", self.root.join("gh/gh"))
            .env("FAKE_GH", self.root.join("gh"))
            .env("PRIVATEER_GLIBC", "glibc 2.39")
            .env("PRIVATEER_CARGO", self.root.join("cargo"))
            .env("FAKE_PRIVATEER", BIN)
            .env("FAKE_BROKER", BROKER);
        command
    }

    fn fetch(&self, args: &[&str], tweak: impl FnOnce(&mut Command)) -> Output {
        let mut command = self.command();
        command.args(["upgrade", "fetch"]).args(args);
        tweak(&mut command);
        command.output().unwrap()
    }

    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("gh/calls.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn staging_is_empty(&self) -> bool {
        fs::read_dir(self.data().join("staging"))
            .map(|mut d| d.next().is_none())
            .unwrap_or(true)
    }

    /// Serve one push run on main whose release job succeeded, with an
    /// artifact holding `tarball` (zipped here) under the given digest.
    fn serve(&self, platform: &str, tarball: &Path, edit: impl FnOnce(&mut Value)) {
        let gh = self.root.join("gh");
        let inner = gh.join(format!("privateer-{platform}.tar.gz"));
        fs::copy(tarball, &inner).unwrap();
        let zipped = Command::new(support::python())
            .current_dir(&gh)
            .args(["-I", "-m", "zipfile", "-c", "artifact.zip"])
            .arg(inner.file_name().unwrap())
            .status()
            .unwrap();
        assert!(zipped.success());
        let name = format!("privateer-{platform}-{C}");
        let mut world = json!({
            "runs": {"workflow_runs": [{"id": 7, "head_sha": C, "event": "push", "head_branch": "main",
                "repository": {"full_name": REPO}, "head_repository": {"full_name": REPO}}]},
            "jobs": {"jobs": [{"name": "test (rest)", "conclusion": "failure"},
                {"name": job_name(platform), "conclusion": "success"}]},
            "artifacts": {"artifacts": [{"id": 9, "name": name, "expired": false,
                "digest": format!("sha256:{}", sha256(&gh.join("artifact.zip")))}]},
        });
        edit(&mut world);
        for key in ["runs", "jobs", "artifacts"] {
            fs::write(gh.join(format!("{key}.json")), world[key].to_string()).unwrap();
        }
        let routes = json!({
            format!("repos/{REPO}/actions/runs?head_sha={C}&event=push&branch=main&per_page=100"): "runs.json",
            format!("repos/{REPO}/actions/runs/7/jobs?per_page=100"): "jobs.json",
            format!("repos/{REPO}/actions/runs/7/artifacts?name={name}"): "artifacts.json",
            format!("repos/{REPO}/actions/artifacts/9/zip"): "artifact.zip",
        });
        fs::write(gh.join("routes.json"), routes.to_string()).unwrap();
    }

    /// Lay out a tree for `C` (altered by `edit`), tar it and serve it.
    fn serve_tree(&self, platform: &str, edit: impl FnOnce(&Path)) {
        let parent = self.root.join("artifact-tree");
        let root = tree(&parent, C);
        edit(&root);
        let tarball = self.root.join("release.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(&parent)
            .arg(C)
            .status()
            .unwrap();
        assert!(status.success());
        self.serve(platform, &tarball, |_| {});
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn reply(output: &Output) -> Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn fetch_publishes_a_checked_artifact_and_switches_nothing() {
    let Some(platform) = platform() else { return };
    let world = World::new();
    world.select_old();
    world.serve_tree(platform, |_| {});

    // Preparing a release is the operator's.
    let refused = world.fetch(&[C], |c| {
        c.env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "infra");
    });
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("operator"),
        "{}",
        stderr(&refused)
    );
    assert!(world.calls().is_empty());

    let fetched = reply(&world.fetch(&[C], |_| {}));
    assert_eq!(fetched["from"], "artifact");
    assert_eq!(fetched["switched"], false);
    assert_eq!(fetched["platform"], platform);
    assert_eq!(fetched["artifact"]["run_id"], 7);
    let release = world.release(C);
    assert_eq!(fetched["release"], release.to_string_lossy().as_ref());
    assert_eq!(
        fs::read(release.join("bin/privateer")).unwrap(),
        fs::read(BIN).unwrap()
    );
    assert!(release.join(SUMS).is_file());
    assert_eq!(world.current(), world.release(OLD), "current moved");
    assert!(world.staging_is_empty(), "staging left behind");
    let endpoints: Vec<String> = world
        .calls()
        .iter()
        .map(|c| {
            c.as_array()
                .unwrap()
                .last()
                .unwrap()
                .as_str()
                .unwrap()
                .into()
        })
        .collect();
    assert_eq!(endpoints.len(), 4, "{endpoints:?}");
    assert!(endpoints[3].ends_with("/actions/artifacts/9/zip"));

    // The published release runs as itself.
    let status = support::command(release.join("bin/privateer"))
        .args(["upgrade", "status"])
        .env("HOME", world.root.join("user"))
        .output()
        .unwrap();
    assert_eq!(reply(&status)["running_commit"], C);

    // Fetching again reuses it without the network, bytecode caches and all.
    let cache = release.join("share/privateer/plugins/pi/__pycache__");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("adapter.cpython-312.pyc"), "cache").unwrap();
    let again = reply(&world.fetch(&[C], |_| {}));
    assert_eq!(again["from"], "existing");
    assert_eq!(world.calls().len(), 4);

    // A release changed at rest is refused, not overwritten.
    let manifest = release.join("share/privateer/plugins/pi/manifest.json");
    fs::write(&manifest, "{}").unwrap();
    let tampered = world.fetch(&[C], |_| {});
    assert!(!tampered.status.success());
    let message = stderr(&tampered);
    assert!(message.contains("not an intact release"), "{message}");
    assert!(message.contains(&*release.to_string_lossy()), "{message}");
    assert_eq!(fs::read_to_string(&manifest).unwrap(), "{}");
    assert_eq!(world.current(), world.release(OLD));
}

/// Each bad artifact is refused before anything is published, with a
/// reason, and leaves no staging behind.
#[test]
fn fetch_refuses_bad_artifacts_and_publishes_nothing() {
    let Some(platform) = platform() else { return };
    type Setup = Box<dyn Fn(&World)>;
    let cases: Vec<(&str, Setup, &str)> = vec![
        (
            "wrong digest",
            Box::new(|w| {
                w.serve_tree(platform, |_| {});
                let tarball = w.root.join("release.tar.gz");
                w.serve(platform, &tarball, |v| {
                    v["artifacts"]["artifacts"][0]["digest"] =
                        json!(format!("sha256:{}", "0".repeat(64)))
                });
            }),
            "does not match its digest",
        ),
        (
            "expired",
            Box::new(|w| {
                w.serve_tree(platform, |_| {});
                let tarball = w.root.join("release.tar.gz");
                w.serve(platform, &tarball, |v| {
                    v["artifacts"]["artifacts"][0]["expired"] = json!(true)
                });
            }),
            "has expired",
        ),
        (
            "a pull request's run",
            Box::new(|w| {
                w.serve_tree(platform, |_| {});
                let tarball = w.root.join("release.tar.gz");
                w.serve(platform, &tarball, |v| {
                    v["runs"]["workflow_runs"][0]["event"] = json!("pull_request")
                });
            }),
            "no push run on main",
        ),
        (
            "a fork's run",
            Box::new(|w| {
                w.serve_tree(platform, |_| {});
                let tarball = w.root.join("release.tar.gz");
                w.serve(platform, &tarball, |v| {
                    v["runs"]["workflow_runs"][0]["head_repository"]["full_name"] =
                        json!("someone/privateer")
                });
            }),
            "no push run on main",
        ),
        (
            "a failed release job",
            Box::new(|w| {
                w.serve_tree(platform, |_| {});
                let tarball = w.root.join("release.tar.gz");
                w.serve(platform, &tarball, |v| {
                    v["jobs"]["jobs"][1]["conclusion"] = json!("failure")
                });
            }),
            "job is failure",
        ),
        (
            "a missing binary",
            Box::new(|w| {
                w.serve_tree(platform, |root| {
                    fs::remove_file(root.join("bin/privateer-mcp-broker")).unwrap();
                    write_sums(root);
                })
            }),
            "has no bin/privateer-mcp-broker",
        ),
        (
            "a file outside SHA256SUMS",
            Box::new(|w| {
                w.serve_tree(platform, |root| {
                    fs::write(root.join("bin/extra"), "x").unwrap();
                })
            }),
            "bin/extra is not in SHA256SUMS",
        ),
        (
            "a file changed after SHA256SUMS",
            Box::new(|w| {
                w.serve_tree(platform, |root| {
                    fs::write(root.join("share/privateer/plugins/pi/manifest.json"), "{}").unwrap();
                })
            }),
            "does not match SHA256SUMS",
        ),
        (
            "a link",
            Box::new(|w| {
                w.serve_tree(platform, |root| {
                    symlink("/etc", root.join("share/etc")).unwrap();
                })
            }),
            "not a plain file or directory",
        ),
        (
            "the wrong platform",
            Box::new(|w| {
                w.serve_tree(platform, |root| {
                    let bin = root.join("bin/privateer");
                    fs::write(&bin, b"\x7fELF\x02\x01\x01 not this machine's image").unwrap();
                    write_sums(root);
                })
            }),
            "the staged privateer upgrade status",
        ),
        (
            "a path outside the release",
            Box::new(|w| {
                // tar itself will not write such a member, so Python does.
                let tarball = w.root.join("evil.tar.gz");
                let script = "import io, sys, tarfile\n\
                    t = tarfile.open(sys.argv[1], 'w:gz')\n\
                    d = tarfile.TarInfo(sys.argv[2]); d.type = tarfile.DIRTYPE; t.addfile(d)\n\
                    f = tarfile.TarInfo(sys.argv[2] + '/../../evil'); f.size = 1\n\
                    t.addfile(f, io.BytesIO(b'x')); t.close()\n";
                let status = Command::new(support::python())
                    .args(["-I", "-c", script])
                    .arg(&tarball)
                    .arg(C)
                    .status()
                    .unwrap();
                assert!(status.success());
                w.serve(platform, &tarball, |_| {});
            }),
            "outside its one",
        ),
    ];
    for (case, setup, expected) in cases {
        let world = World::new();
        world.select_old();
        setup(&world);
        let output = world.fetch(&[C], |_| {});
        let message = stderr(&output);
        assert!(!output.status.success(), "{case}: fetched");
        assert!(message.contains(expected), "{case}: {message}");
        assert!(!world.release(C).exists(), "{case}: published");
        assert!(world.staging_is_empty(), "{case}: staging left behind");
        assert_eq!(world.current(), world.release(OLD), "{case}: current moved");
        for escaped in ["evil", "data/evil", "data/privateer/evil"] {
            assert!(
                !world.root.join(escaped).exists(),
                "{case}: wrote {escaped}"
            );
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// An `origin` with two commits on main, and a checkout of it holding a
/// commit only on a side branch plus uncommitted edits.
fn fixture_repo(root: &Path) -> (PathBuf, [String; 3]) {
    let origin = root.join("origin");
    fs::create_dir_all(origin.join("plugins/pi")).unwrap();
    git(&origin, &["init", "--quiet"]);
    fs::write(origin.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
    fs::write(origin.join("README.md"), "fixture\n").unwrap();
    fs::write(
        origin.join("plugins/pi/manifest.json"),
        json!({"schema_version":1,"id":"pi","executable":["python3","adapter.py"]}).to_string(),
    )
    .unwrap();
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "--quiet", "-m", "first"]);
    let first = git(&origin, &["rev-parse", "HEAD"]);
    fs::write(origin.join("README.md"), "fixture, second\n").unwrap();
    git(&origin, &["commit", "--quiet", "-am", "second"]);
    let second = git(&origin, &["rev-parse", "HEAD"]);
    let checkout = root.join("checkout");
    git(
        root,
        &[
            "clone",
            "--quiet",
            origin.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    git(&checkout, &["checkout", "--quiet", "-b", "side"]);
    fs::write(checkout.join("README.md"), "side\n").unwrap();
    git(&checkout, &["commit", "--quiet", "-am", "side"]);
    let side = git(&checkout, &["rev-parse", "HEAD"]);
    fs::write(checkout.join("README.md"), "uncommitted\n").unwrap();
    fs::write(checkout.join("notes.txt"), "untracked\n").unwrap();
    (checkout, [first, second, side])
}

#[test]
fn source_build_of_a_main_commit_leaves_the_checkout_and_current_untouched() {
    let world = World::new();
    world.select_old();
    let (checkout, [first, second, side]) = fixture_repo(&world.root);
    let source = checkout.to_str().unwrap();
    let before = git(&checkout, &["status", "--porcelain"]);

    // Only main is released.
    let output = world.fetch(&[&side, "--build", "--source", source], |_| {});
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("is not on origin/main"),
        "{}",
        stderr(&output)
    );
    assert!(!world.release(&side).exists());

    // A failing build publishes nothing and moves nothing.
    let output = world.fetch(&[&first, "--build", "--source", source], |c| {
        c.env("FAKE_CARGO_FAIL", "1");
    });
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("could not compile"),
        "{}",
        stderr(&output)
    );
    assert!(!world.release(&first).exists());
    assert_eq!(world.current(), world.release(OLD));
    assert!(world.staging_is_empty());

    // The build is laid out by the shared script, with SHA256SUMS.
    let built = reply(&world.fetch(&[&first, "--build", "--source", source], |_| {}));
    assert_eq!(built["from"], "build");
    assert_eq!(built["build_reason"], "--build");
    let release = world.release(&first);
    let sums = fs::read_to_string(release.join(SUMS)).unwrap();
    assert!(
        sums.contains("  share/privateer/plugins/pi/manifest.json\n"),
        "{sums}"
    );
    assert!(sums.contains("  bin/privateer\n"), "{sums}");
    assert_eq!(world.current(), world.release(OLD), "current moved");
    assert!(world.staging_is_empty());

    // Without an artifact for its commit, a machine builds from --source.
    let gh = world.root.join("gh");
    fs::write(
        gh.join("runs.json"),
        json!({"workflow_runs": []}).to_string(),
    )
    .unwrap();
    let query =
        format!("repos/{REPO}/actions/runs?head_sha={second}&event=push&branch=main&per_page=100");
    fs::write(
        gh.join("routes.json"),
        json!({query: "runs.json"}).to_string(),
    )
    .unwrap();
    let built = reply(&world.fetch(&[&second, "--source", source], |_| {}));
    assert_eq!(built["from"], "build");
    if platform().is_some() {
        assert!(
            built["build_reason"]
                .as_str()
                .unwrap()
                .contains("no push run on main"),
            "{built}"
        );
    }
    // The checkout's working tree and branch are as they were.
    assert_eq!(git(&checkout, &["status", "--porcelain"]), before);
    assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), side);
    assert_eq!(
        fs::read_to_string(checkout.join("README.md")).unwrap(),
        "uncommitted\n"
    );
    assert_eq!(world.current(), world.release(OLD));
}

/// A glibc below the floor CI checked builds from source and never asks
/// GitHub; without a source it is refused, saying why.
#[cfg(target_os = "linux")]
#[test]
fn an_old_glibc_needs_a_source_build() {
    let world = World::new();
    let (checkout, [first, second, _]) = fixture_repo(&world.root);
    let old_glibc = |c: &mut Command| {
        c.env("PRIVATEER_GLIBC", "glibc 2.17");
    };
    let output = world.fetch(&[&first], old_glibc);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(
        message.contains("glibc 2.17 is older than the 2.39"),
        "{message}"
    );
    assert!(message.contains("--source"), "{message}");
    let built = reply(&world.fetch(&[&first, "--source", checkout.to_str().unwrap()], old_glibc));
    assert_eq!(built["from"], "build");
    assert!(built["build_reason"].as_str().unwrap().contains("2.17"));
    assert!(
        world.calls().is_empty(),
        "asked GitHub: {:?}",
        world.calls()
    );
    // Musl, or no getconf at all, reports no glibc.
    let output = world.fetch(&[&second], |c| {
        c.env("PRIVATEER_GLIBC", "");
    });
    assert!(
        stderr(&output).contains("no glibc reported"),
        "{}",
        stderr(&output)
    );
}

/// CI publishes each platform's artifact from the job fetch looks for, laid
/// out by the shared script.
#[test]
fn ci_publishes_every_platform_through_the_layout_script() {
    let workflow =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/ci.yml"))
            .unwrap();
    for platform in ["macos-arm64", "linux-x86_64", "linux-aarch64"] {
        assert!(
            workflow.contains(&format!(
                "- name: {}\n            platform: {platform}\n",
                job_name(platform)
            )),
            "ci.yml does not release {platform}"
        );
    }
    assert!(workflow.contains("os: ubuntu-24.04-arm"));
    assert!(workflow.contains("name: privateer-${{ matrix.platform }}-${{ github.sha }}"));
    assert!(
        workflow
            .contains("scripts/release-layout.sh \"$GITHUB_SHA\" target/release plugins releases")
    );
}
