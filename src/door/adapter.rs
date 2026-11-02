//! Where a door's adapter comes from. A door registered with a release's
//! bundled adapter follows the release that runs it, so an upgrade restarts
//! it on the new adapter; any other path (a custom adapter, a checkout or a
//! worktree) stays exactly as registered, and is reported when it is not the
//! bundled adapter of the same id.
use super::{Door, Manifest, load_manifest_in};
use std::fs;
use std::path::{Path, PathBuf};

/// The bundled plugin a manifest path names when it lies in a release's own
/// `share/privateer/plugins/<id>/manifest.json`, read literally (a pruned
/// release) or through `current`. Choosing such a path chose the bundled
/// adapter, never one release's copy of it.
fn bundled_id(path: &Path) -> Option<String> {
    let id_in = |path: &Path| -> Option<String> {
        if path.file_name()? != "manifest.json" {
            return None;
        }
        let plugin = path.parent()?;
        let plugins = plugin.parent()?;
        let root = ["plugins", "privateer", "share"]
            .iter()
            .try_fold(plugins, |dir, name| {
                (dir.file_name()? == *name).then_some(dir.parent()?)
            })?;
        crate::releases::Release::from_executable(&root.join("bin/privateer"))?;
        let id = plugin.file_name()?.to_str()?;
        crate::store::identifier(id).ok()?;
        Some(id.to_owned())
    };
    id_in(path).or_else(|| id_in(&fs::canonicalize(path).ok()?))
}

/// The adapter manifest to load for a registered path: the same bundled
/// adapter in `plugins` (the running release's) when the path names a
/// release's bundled one and `plugins` has it, else the path itself.
pub(super) fn follow(path: &Path, plugins: Option<&Path>) -> PathBuf {
    if let (Some(id), Some(plugins)) = (bundled_id(path), plugins) {
        let bundled = plugins.join(id).join("manifest.json");
        if bundled.is_file() {
            return bundled;
        }
    }
    path.to_path_buf()
}

fn same(a: &Path, b: &Path) -> bool {
    a == b || matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// Why `door` does not run `plugins`' bundled adapter of its id, with the
/// command that re-registers it on that adapter; None when it does, or when
/// its adapter is not a bundled one at all.
pub(super) fn drift(door: &Door, manifest: &Manifest, plugins: &Path) -> Option<String> {
    let bundled = plugins.join(&manifest.id).join("manifest.json");
    if !bundled.is_file() || same(&manifest.source, &bundled) {
        return None;
    }
    let mut fix = format!(
        "privateer door add {} --manifest {}",
        door.name,
        bundled.display()
    );
    if let Some(config) = &door.config {
        fix.push_str(&format!(" --config {}", config.display()));
    }
    if let Some(ship) = &door.recipient {
        fix.push_str(&format!(" --to {ship}"));
    }
    let grants = if door.protocol == 2 {
        " (its client grants are revoked: the adapter source changes)"
    } else {
        ""
    };
    Some(format!(
        "its {} adapter runs from {}, not the bundled {}, so upgrades do not reach it; unless that is deliberate, re-register it to follow them{grants}: {fix}",
        manifest.id,
        manifest.source.display(),
        bundled.display(),
    ))
}

/// Every registered door whose adapter would not come from `plugins` (a
/// release's bundled plugins), as `door:NAME: why` lines.
pub(crate) fn drifted(plugins: &Path) -> anyhow::Result<Vec<String>> {
    let mut lines = Vec::new();
    for door in super::doors()? {
        let note = match load_manifest_in(&door.manifest, Some(plugins)) {
            Ok(manifest) => {
                drift(&door, &manifest, plugins).map(|why| format!("adapter drift: {why}"))
            }
            Err(error) => Some(format!("adapter not checked: {error:#}")),
        };
        if let Some(note) = note {
            lines.push(format!("door:{}: {note}", door.name));
        }
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_release_bundle_follows_the_running_plugins() {
        let temp = tempfile::tempdir().unwrap();
        let commit = "1111111111111111111111111111111111111111";
        let old = temp.path().join(format!(
            "releases/{commit}/share/privateer/plugins/telegram"
        ));
        let running = temp.path().join("running/plugins");
        let checkout = temp.path().join("checkout/plugins/telegram");
        for dir in [&old, &running.join("telegram"), &checkout] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("manifest.json"), "{}").unwrap();
        }
        std::os::unix::fs::symlink(format!("releases/{commit}"), temp.path().join("current"))
            .unwrap();
        let followed = running.join("telegram/manifest.json");
        for registered in [
            old.join("manifest.json"),
            temp.path()
                .join("current/share/privateer/plugins/telegram/manifest.json"),
        ] {
            assert_eq!(follow(&registered, Some(&running)), followed);
            assert_eq!(follow(&registered, None), registered);
        }
        // A pruned release still names the bundled adapter.
        fs::remove_dir_all(temp.path().join("releases")).unwrap();
        let pruned = old.join("manifest.json");
        assert_eq!(follow(&pruned, Some(&running)), followed);
        // A checkout or custom path is a deliberate choice: it stays.
        let custom = checkout.join("manifest.json");
        assert_eq!(follow(&custom, Some(&running)), custom);
    }
}
