//! Pin the service's Python at installation, not a version-manager dispatcher.
use anyhow::{Result, bail};
use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub(crate) const VARIABLE: &str = "PRIVATEER_PYTHON";

pub(crate) fn executable(path: &Path) -> bool {
    let Ok(path_c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
        && unsafe {
            libc::faccessat(
                libc::AT_FDCWD,
                path_c.as_ptr(),
                libc::X_OK,
                libc::AT_EACCESS,
            )
        } == 0
}

/// No shim is executed, even to ask it which interpreter it would choose.
/// Canonicalization can follow a symlink directly to a real interpreter.
pub(crate) fn resolve(path: &OsStr) -> Result<PathBuf> {
    resolve_with(
        path,
        &[
            Path::new("/usr/bin/python3"),
            Path::new("/usr/local/bin/python3"),
            Path::new("/bin/python3"),
        ],
        |candidate| {
            let real = candidate.canonicalize().ok()?;
            executable(&real).then_some(real)
        },
    )
}

/// PATH, standard locations and filesystem inspection are injectable. Relative
/// PATH entries are deliberately excluded: a service runs in another directory.
fn resolve_with(
    path: &OsStr,
    standard: &[&Path],
    inspect: impl Fn(&Path) -> Option<PathBuf>,
) -> Result<PathBuf> {
    let mut managed = None;
    for candidate in std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("python3"))
        .chain(standard.iter().map(|p| p.to_path_buf()))
    {
        let Some(real) = inspect(&candidate).filter(|p| p.is_absolute()) else {
            continue;
        };
        // Check both names: a benign PATH alias can point at a shim, and a
        // shim directory can contain a symlink straight to a real interpreter.
        let Some(real_managed) = manager_path(&real) else {
            continue;
        };
        // A plain PATH alias can resolve through a shim to the manager
        // binary itself. Require an interpreter name for every target, not
        // just candidates visibly inside a shim directory.
        if !real
            .file_name()
            .is_some_and(|s| s.as_bytes().starts_with(b"python"))
        {
            continue;
        }
        if real_managed || manager_path(&candidate) == Some(true) {
            managed.get_or_insert(real);
        } else {
            // Keep stable distro/Homebrew links across minor upgrades. Only
            // escape to the target when the candidate is itself a shim.
            return Ok(if manager_path(&candidate).is_none() {
                real
            } else {
                candidate
            });
        }
    }
    if let Some(real) = managed {
        return Ok(real);
    }
    bail!(
        "cannot install service: no usable python3 found in PATH or standard locations (version-manager shims are skipped)"
    )
}

/// None is a dispatcher/shim, true a prunable managed interpreter, false an
/// ordinary executable. Manager roots are not themselves interpreters.
fn manager_path(path: &Path) -> Option<bool> {
    let parts: Vec<_> = path.iter().filter_map(OsStr::to_str).collect();
    if parts.contains(&"shims") {
        return None;
    }
    for (i, part) in parts.iter().enumerate() {
        if matches!(
            *part,
            "mise" | ".mise" | "asdf" | ".asdf" | "pyenv" | ".pyenv"
        ) {
            return parts[i + 1..]
                .iter()
                .any(|p| matches!(*p, "installs" | "versions"))
                .then_some(true);
        }
    }
    Some(false)
}

/// Missing recordings retain legacy PATH lookup. A pruned recorded interpreter
/// is an explicit, one-line fallback, never an attempt to execute a stale path.
pub(crate) fn program() -> OsString {
    let (program, warning) = program_from(std::env::var_os(VARIABLE).as_deref(), executable);
    static WARNED: std::sync::Once = std::sync::Once::new();
    if let Some(warning) = warning {
        WARNED.call_once(|| eprintln!("{warning}"));
    }
    program
}

fn program_from(
    recorded: Option<&OsStr>,
    usable: impl Fn(&Path) -> bool,
) -> (OsString, Option<String>) {
    match recorded {
        Some(path) if Path::new(path).is_absolute() && usable(Path::new(path)) => {
            (path.into(), None)
        }
        Some(path) => (
            "python3".into(),
            Some(format!(
                "warning: recorded Python {path:?} is missing or not executable; falling back to python3 on PATH"
            )),
        ),
        None => ("python3".into(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn injected_paths_and_files_skip_shims_roots_and_broken_candidates() {
        let files: HashMap<_, _> = [
            ("/alpha/shims/python3", "/alpha/shims/python3"),
            ("/alpha/.mise/bin/python3", "/alpha/.mise/bin/python3"),
            ("/alpha/.asdf/bin/python3", "/alpha/.asdf/bin/python3"),
            ("/alpha/.pyenv/bin/python3", "/alpha/.pyenv/bin/python3"),
            ("/beta/python3", "/alpha/shims/python3"),
            ("/gamma/python3", "/gamma/python3.12"),
        ]
        .into_iter()
        .map(|(a, b)| (PathBuf::from(a), PathBuf::from(b)))
        .collect();
        let inspect = |p: &Path| files.get(p).cloned();
        let path = OsStr::new(
            "relative:/alpha/shims:/alpha/.mise/bin:/alpha/.asdf/bin:/alpha/.pyenv/bin:/missing:/beta:/gamma",
        );
        assert_eq!(
            resolve_with(path, &[], inspect).unwrap(),
            Path::new("/gamma/python3")
        );
        assert!(
            resolve_with(OsStr::new("/alpha/shims:/beta"), &[], inspect)
                .unwrap_err()
                .to_string()
                .contains("no usable python3")
        );
    }

    #[test]
    fn stable_interpreters_precede_prunable_versions_but_managed_is_a_last_resort() {
        let files: HashMap<_, _> = [
            (
                "/alpha/mise/installs/python/3.12/bin/python3",
                "/alpha/mise/installs/python/3.12/bin/python3.12",
            ),
            ("/beta/python3", "/beta/python3.12"),
            ("/gamma/shims/python3", "/beta/python3.12"),
        ]
        .into_iter()
        .map(|(a, b)| (PathBuf::from(a), PathBuf::from(b)))
        .collect();
        let inspect = |p: &Path| files.get(p).cloned();
        let managed = OsStr::new("/alpha/mise/installs/python/3.12/bin");
        assert_eq!(
            resolve_with(managed, &[Path::new("/beta/python3")], inspect).unwrap(),
            Path::new("/beta/python3")
        );
        assert_eq!(
            resolve_with(managed, &[], inspect).unwrap(),
            Path::new("/alpha/mise/installs/python/3.12/bin/python3.12")
        );
        assert_eq!(
            resolve_with(OsStr::new("/gamma/shims"), &[], inspect).unwrap(),
            Path::new("/beta/python3.12")
        );
    }

    #[test]
    fn ordinary_links_keep_stable_names_across_distro_and_homebrew_upgrades() {
        for (candidate, target) in [
            ("/usr/bin/python3", "/usr/bin/python3.12"),
            (
                "/opt/homebrew/bin/python3",
                "/opt/homebrew/Cellar/python@3.12/3.12.7/bin/python3.12",
            ),
            (
                "/var/alpha/bin/python3",
                "/private/var/alpha/bin/python3.12",
            ),
        ] {
            let candidate = Path::new(candidate);
            assert_eq!(
                resolve_with(OsStr::new(""), &[candidate], |p| {
                    (p == candidate).then(|| target.into())
                })
                .unwrap(),
                candidate
            );
        }
    }

    #[test]
    fn ordinary_alias_through_a_shim_to_a_manager_binary_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let alias = dir.path().join("bin");
        let shims = dir.path().join("alpha/shims");
        std::fs::create_dir(&alias).unwrap();
        std::fs::create_dir_all(&shims).unwrap();
        // A manager binary installed outside its manager root (e.g. /usr/bin/mise).
        let manager = dir.path().join("mise-dispatcher");
        std::fs::write(&manager, "fixture").unwrap();
        std::fs::set_permissions(&manager, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&manager, shims.join("python3")).unwrap();
        std::os::unix::fs::symlink(shims.join("python3"), alias.join("python3")).unwrap();
        let inspect = |p: &Path| p.canonicalize().ok().filter(|p| executable(p));
        assert!(resolve_with(alias.as_os_str(), &[], inspect).is_err());
        assert_eq!(
            resolve_with(alias.as_os_str(), &[Path::new("/beta/python3")], |p| {
                if p == Path::new("/beta/python3") {
                    Some("/beta/python3.12".into())
                } else {
                    inspect(p)
                }
            })
            .unwrap(),
            Path::new("/beta/python3")
        );
    }

    #[test]
    fn real_files_require_executable_regular_files_and_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        // Normalize only the fixture root: macOS's temporary directory may
        // live below /var -> /private/var. The interpreter's stable link is
        // intentionally not canonicalized in the expected result.
        let root = dir.path().canonicalize().unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let real = root.join("python3.12");
        std::fs::write(&real, "fixture").unwrap();
        std::os::unix::fs::symlink(&real, bin.join("python3")).unwrap();
        let inspect = |p: &Path| {
            let p = p.canonicalize().ok()?;
            executable(&p).then_some(p)
        };
        assert!(resolve_with(bin.as_os_str(), &[], inspect).is_err());
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            resolve_with(bin.as_os_str(), &[], inspect).unwrap(),
            bin.join("python3")
        );
    }

    #[test]
    fn stable_name_under_a_symlinked_parent_is_an_ordinary_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ordinary = root.join("alpha/bin");
        std::fs::create_dir_all(&ordinary).unwrap();
        let real = ordinary.join("python3.12");
        std::fs::write(&real, "fixture").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink("python3.12", ordinary.join("python3")).unwrap();
        let alias = root.join("beta");
        std::os::unix::fs::symlink(root.join("alpha"), &alias).unwrap();
        let bin = alias.join("bin");
        let candidate = bin.join("python3");
        assert_eq!(candidate.canonicalize().unwrap(), real);
        // Exercise production filesystem inspection, not just injected paths:
        // resolving the parent alias must not pin the versioned target.
        assert_eq!(resolve(bin.as_os_str()).unwrap(), candidate);
    }

    #[test]
    fn pruned_recording_warns_once_per_process() {
        const CHILD: &str = "PRIVATEER_TEST_PYTHON_WARNING";
        if std::env::var_os(CHILD).is_some() {
            for _ in 0..5 {
                assert_eq!(program(), "python3");
            }
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "python::tests::pruned_recording_warns_once_per_process",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(VARIABLE, dir.path().join("missing-python3"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            stderr.matches("falling back to python3 on PATH").count(),
            1,
            "{stderr}"
        );
    }

    #[test]
    fn legacy_and_pruned_recordings_have_explicit_fallback_semantics() {
        assert_eq!(program_from(None, |_| false), ("python3".into(), None));
        assert_eq!(
            program_from(Some(OsStr::new("/alpha/python3")), |_| true),
            ("/alpha/python3".into(), None)
        );
        let (program, warning) =
            program_from(Some(OsStr::new("/alpha/missing\npython3")), |_| false);
        assert_eq!(program, "python3");
        let warning = warning.unwrap();
        assert!(warning.contains("/alpha/missing\\npython3"));
        assert_eq!(warning.lines().count(), 1);
    }
}
