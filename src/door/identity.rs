//! Path-free MCP build diagnostics. Compare the selected files to the bundle
//! compiled into this core, not to a mutable `current` symlink or version label.
use super::{Door, load_manifest};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

// Keep in sync with plugins/mcp/identity.py. Include every runtime module and
// the dependency lock; private config, venvs, caches and docs are not identity.
const FILES: &[(&str, &[u8])] = &[
    (
        "argument_errors.py",
        include_bytes!("../../plugins/mcp/argument_errors.py"),
    ),
    ("door.py", include_bytes!("../../plugins/mcp/door.py")),
    (
        "file_transfer.py",
        include_bytes!("../../plugins/mcp/file_transfer.py"),
    ),
    (
        "identity.py",
        include_bytes!("../../plugins/mcp/identity.py"),
    ),
    ("launch.py", include_bytes!("../../plugins/mcp/launch.py")),
    (
        "oauth_http.py",
        include_bytes!("../../plugins/mcp/oauth_http.py"),
    ),
    (
        "request_log.py",
        include_bytes!("../../plugins/mcp/request_log.py"),
    ),
    (
        "requirements.lock",
        include_bytes!("../../plugins/mcp/requirements.lock"),
    ),
    ("tools.py", include_bytes!("../../plugins/mcp/tools.py")),
];
const MANIFEST: &[u8] = include_bytes!("../../plugins/mcp/manifest.json");
const FILE_LIMIT: u64 = 1024 * 1024;

fn add(hash: &mut Sha256, name: &str, bytes: &[u8]) {
    hash.update((name.len() as u64).to_be_bytes());
    hash.update(name.as_bytes());
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn read(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(FILE_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= FILE_LIMIT, "identity file too large");
    Ok(bytes)
}

fn bundled_revision() -> String {
    let mut hash = Sha256::new();
    for (name, bytes) in FILES {
        add(&mut hash, name, bytes);
    }
    format!("{:x}", hash.finalize())
}

fn revision(root: &Path) -> Result<(String, bool)> {
    let mut hash = Sha256::new();
    let mut complete = true;
    for (name, _) in FILES {
        match read(&root.join(name)) {
            Ok(bytes) => add(&mut hash, name, &bytes),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                // Older bundles can predate a module. Absence is positive
                // mismatch evidence, unlike a permission or I/O failure.
                hash.update((name.len() as u64).to_be_bytes());
                hash.update(name.as_bytes());
                hash.update(u64::MAX.to_be_bytes());
                complete = false;
            }
            Err(error) => return Err(error),
        }
    }
    Ok((format!("{:x}", hash.finalize()), complete))
}

/// Recognize only the exact release plugin layout. Never report a path or an
/// arbitrary manifest-supplied version (which could contain private values).
fn adapter_release(source: &Path) -> Option<String> {
    let root = source.parent()?.ancestors().nth(4)?;
    let release = crate::releases::Release::from_executable(&root.join("bin/privateer"))?;
    (release.plugins().join("mcp/manifest.json") == source).then_some(release.commit)
}

pub(super) fn registered(door: &Door) -> Value {
    let core_commit = crate::releases::running_image()
        .ok()
        .and_then(|p| crate::releases::Release::from_executable(&p))
        .map(|r| r.commit);
    registered_with_core(door, core_commit)
}

fn registered_with_core(door: &Door, core_commit: Option<String>) -> Value {
    let mut value = json!({"schema":"door-identity.v1", "observation":"registered_files",
        "core":{"version":env!("CARGO_PKG_VERSION"), "release_commit":core_commit,
            "adapter_revision":bundled_revision(), "manifest_revision":format!("{:x}", Sha256::digest(MANIFEST))},
        "adapter":null, "adapter_drift":null, "identity_status":"unavailable"});
    // Fail closed diagnostically, without reflecting I/O errors or file content.
    let observed = (|| -> Result<Value> {
        let manifest = load_manifest(&door.manifest)?;
        ensure!(manifest.id == "mcp", "not the bundled MCP adapter");
        let root = manifest.source.parent().context("manifest parent")?;
        let (revision, complete) = revision(root)?;
        Ok(json!({"revision":revision, "complete":complete,
            "manifest_revision":format!("{:x}", Sha256::digest(read(&manifest.source)?)),
            "release_commit":adapter_release(&manifest.source)}))
    })();
    if let Ok(adapter) = observed {
        let different_release = core_commit.as_ref().is_some_and(|commit| {
            adapter["release_commit"]
                .as_str()
                .is_some_and(|other| other != commit)
        });
        value["adapter_drift"] = json!(
            adapter["complete"] != true
                || different_release
                || adapter["revision"] != value["core"]["adapter_revision"]
                || adapter["manifest_revision"] != value["core"]["manifest_revision"]
        );
        value["identity_status"] = json!(if adapter["complete"] == true {
            "ok"
        } else {
            "incomplete"
        });
        value["adapter"] = adapter;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_manifest_reports_content_and_release_drift_without_private_data() {
        let temp = tempfile::tempdir().unwrap();
        let old = "1111111111111111111111111111111111111111";
        let new = "2222222222222222222222222222222222222222";
        let root = temp
            .path()
            .join(format!("releases/{old}/share/privateer/plugins/mcp"));
        std::fs::create_dir_all(&root).unwrap();
        for (name, bytes) in FILES {
            std::fs::write(root.join(name), bytes).unwrap();
        }
        std::fs::write(root.join("manifest.json"), MANIFEST).unwrap();
        let guard = temp.path().join("scoped-manifest.json");
        std::fs::write(
            &guard,
            json!({"schema_version":1,"kind":"front-door-scoped",
            "door_protocol":2,"adapter_manifest":root.join("manifest.json")})
            .to_string(),
        )
        .unwrap();
        let door = Door {
            name: "alpha".into(),
            manifest: guard,
            config: None,
            recipient: None,
            protocol: 2,
        };
        let matching = registered_with_core(&door, Some(old.into()));
        assert_eq!(matching["adapter_drift"], false);
        assert_eq!(
            matching["adapter"]["revision"],
            matching["core"]["adapter_revision"]
        );
        let different_release = registered_with_core(&door, Some(new.into()));
        assert_eq!(different_release["adapter_drift"], true);
        assert_eq!(different_release["adapter"]["release_commit"], old);
        assert_eq!(different_release["core"]["release_commit"], new);
        std::fs::write(root.join("tools.py"), b"# older adapter").unwrap();
        assert_eq!(registered_with_core(&door, None)["adapter_drift"], true);
        // Manifest-only edits are independently visible, but never reflected.
        let tools = FILES.iter().find(|(name, _)| *name == "tools.py").unwrap();
        std::fs::write(root.join("tools.py"), tools.1).unwrap();
        let mut manifest: Value = serde_json::from_slice(MANIFEST).unwrap();
        manifest["version"] = json!("private-canary-secret");
        std::fs::write(root.join("manifest.json"), manifest.to_string()).unwrap();
        let changed = registered_with_core(&door, None);
        assert_eq!(changed["adapter_drift"], true);
        assert_eq!(
            changed["adapter"]["revision"],
            changed["core"]["adapter_revision"]
        );
        let text = changed.to_string();
        assert!(!text.contains("private-canary-secret"));
        assert!(!text.contains(&temp.path().display().to_string()));
        std::fs::remove_file(root.join("door.py")).unwrap();
        let missing = registered_with_core(&door, None);
        assert_eq!(missing["identity_status"], "incomplete");
        assert_eq!(missing["adapter_drift"], true);
        assert_eq!(missing["adapter"]["complete"], false);
        std::fs::remove_file(root.join("manifest.json")).unwrap();
        let unavailable = registered_with_core(&door, None);
        assert_eq!(unavailable["identity_status"], "unavailable");
        assert!(unavailable["adapter_drift"].is_null());
    }

    #[test]
    fn release_identity_requires_exact_layout() {
        let commit = "1111111111111111111111111111111111111111";
        assert_eq!(
            adapter_release(Path::new(&format!(
                "/alpha/releases/{commit}/share/privateer/plugins/mcp/manifest.json"
            ))),
            Some(commit.into())
        );
        assert_eq!(adapter_release(Path::new("/alpha/mcp/manifest.json")), None);
    }
}
