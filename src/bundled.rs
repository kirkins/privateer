//! Durable references to bundled harness manifests. Custom paths are never
//! redirected merely because their manifest declares a familiar harness id.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::{Path, PathBuf};

const MANIFESTS: &[(&str, &str)] = &[
    ("agy", include_str!("../plugins/agy/manifest.json")),
    ("claude", include_str!("../plugins/claude/manifest.json")),
    ("codex", include_str!("../plugins/codex/manifest.json")),
    ("cursor", include_str!("../plugins/cursor/manifest.json")),
    ("devin", include_str!("../plugins/devin/manifest.json")),
    (
        "gateway-pi",
        include_str!("../plugins/gateway-pi/manifest.json"),
    ),
    ("gemini", include_str!("../plugins/gemini/manifest.json")),
    ("grok", include_str!("../plugins/grok/manifest.json")),
    ("kimi", include_str!("../plugins/kimi/manifest.json")),
    ("muse", include_str!("../plugins/muse/manifest.json")),
    ("omp", include_str!("../plugins/omp/manifest.json")),
    (
        "opencode",
        include_str!("../plugins/opencode/manifest.json"),
    ),
    (
        "opencode-v2",
        include_str!("../plugins/opencode-v2/manifest.json"),
    ),
    ("pi", include_str!("../plugins/pi/manifest.json")),
    (
        "pi-signed",
        include_str!("../plugins/pi-signed/manifest.json"),
    ),
    ("rovo", include_str!("../plugins/rovo/manifest.json")),
];

/// The bundled harness manifests, by harness id, as shipped.
pub(crate) fn manifests() -> &'static [(&'static str, &'static str)] {
    MANIFESTS
}

fn known(id: &str) -> bool {
    MANIFESTS.iter().any(|(name, _)| *name == id)
}

pub(crate) fn resolve(reference: &str) -> Result<PathBuf> {
    match reference.strip_prefix("bundled:") {
        Some(id) => {
            ensure!(known(id), "unknown bundled harness {id}");
            Ok(crate::cli::plugins_dir()?.join(id).join("manifest.json"))
        }
        None => Ok(PathBuf::from(reference)),
    }
}

/// Only migrate standard `plugins/<id>/manifest.json` paths with the bundled
/// contents, or paths in a recognizable immutable release. A custom manifest
/// (including one with a bundled id) remains pinned to its original path.
/// Missing unidentifiable checkout paths are left alone rather than guessed.
pub(crate) fn normalize(reference: &str) -> Result<String> {
    if let Some(id) = reference.strip_prefix("bundled:") {
        ensure!(known(id), "unknown bundled harness {id}");
        return Ok(reference.into());
    }
    let path = Path::new(reference);
    let candidate = || -> Option<&str> {
        if !path.is_absolute() || path.file_name()? != "manifest.json" {
            return None;
        }
        let harness = path.parent()?;
        if harness.parent()?.file_name()? != "plugins" {
            return None;
        }
        let id = harness.file_name()?.to_str()?;
        known(id).then_some(id)
    };
    if let Some(id) = candidate() {
        let release_path = path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .and_then(Path::parent)
            .and_then(Path::parent)
            .is_some_and(|root| {
                crate::releases::Release::from_executable(&root.join("bin/privateer")).is_some()
            });
        let checkout_path = path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .is_some_and(|root| {
                root == Path::new(env!("CARGO_MANIFEST_DIR"))
                    || (root.join(".git").exists()
                        && root.join("Cargo.toml").is_file()
                        && root.join("plugins/native.py").is_file())
            });
        // `requires` came later, so a manifest from an earlier release that
        // lacks it is still the bundled one.
        let unrequired = |mut value: Value| {
            if let Some(fields) = value.as_object_mut() {
                fields.remove("requires");
            }
            value
        };
        let bundled_contents = std::fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .map(unrequired)
            .is_some_and(|actual| {
                MANIFESTS.iter().any(|(name, raw)| {
                    *name == id
                        && serde_json::from_str::<Value>(raw)
                            .ok()
                            .map(unrequired)
                            .as_ref()
                            == Some(&actual)
                })
            });
        if ((checkout_path || release_path) && bundled_contents) || (release_path && !path.exists())
        {
            return Ok(format!("bundled:{id}"));
        }
    }
    Ok(reference.into())
}

pub(crate) fn harness(reference: &str) -> Option<String> {
    reference
        .strip_prefix("bundled:")
        .map(str::to_owned)
        .or_else(|| {
            Path::new(reference)
                .parent()?
                .file_name()?
                .to_str()
                .map(str::to_owned)
        })
}

/// Transactional, idempotent migration performed when a home opens. It only
/// changes first_mate.manifest; options, configs, and provider pins are intact.
pub(crate) fn migrate(store: &crate::store::Store) -> Result<()> {
    let tx = store.write_transaction()?;
    let records = {
        let mut statement = tx.prepare("SELECT name, record FROM ships")?;
        statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, raw) in records {
        let mut record: Value = serde_json::from_str(&raw).context("corrupt ship record")?;
        if let Some(reference) = record["first_mate"]["manifest"].as_str() {
            let normalized = normalize(reference)?;
            if normalized != reference {
                record["first_mate"]["manifest"] = Value::String(normalized);
                tx.execute(
                    "UPDATE ships SET record=?1 WHERE name=?2",
                    rusqlite::params![record.to_string(), name],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn no_bundled_manifest_overrides_the_host_crew_memory() {
        // A declared figure outranks the host's default, so the bundled
        // manifests leave crew memory to the host.
        for (id, raw) in super::manifests() {
            let manifest: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert!(
                manifest.get("memory_bytes").is_none(),
                "{id} declares memory"
            );
        }
    }
}
