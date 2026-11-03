use crate::store::{Store, identifier, reject_symlinks};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Installation {
    id: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Tool {
    pub id: String,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub files: Vec<PathBuf>,
    pub config: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Configuration {
    pub schema_version: u32,
    pub installation_id: String,
    pub home: PathBuf,
    pub environment: String,
    pub production: bool,
    pub trusted_build_policy: String,
    pub verifier: Tool,
    pub adapter: Tool,
}

pub(super) struct Snapshot {
    pub configuration: Configuration,
    pub verifier_sha256: String,
    pub adapter_sha256: String,
}
impl Snapshot {
    pub fn bindings(&self) -> Value {
        json!({"verifier":{"id":self.configuration.verifier.id,"sha256":self.verifier_sha256},
            "adapter":{"id":self.configuration.adapter.id,"sha256":self.adapter_sha256}})
    }
}

fn safe_directory(owner: u32, mode: u32, uid: u32, below_sticky: bool) -> Result<bool> {
    let shared = owner == 0 && mode & 0o1000 != 0;
    ensure!(
        if below_sticky {
            owner == uid && mode & 0o022 == 0
        } else {
            (owner == 0 || owner == uid) && (mode & 0o022 == 0 || shared)
        },
        "deployment ancestor must be owned and not writable by another account"
    );
    Ok(shared)
}

fn ancestors(path: &Path) -> Result<()> {
    let uid = unsafe { libc::geteuid() };
    let mut below_sticky = false;
    let mut parents: Vec<_> = path
        .parent()
        .context("missing deployment parent")?
        .ancestors()
        .collect();
    parents.reverse();
    for parent in parents {
        // reject_symlinks already checked the path, including its narrowly
        // allowed root-owned macOS aliases (/var, /tmp, /etc).
        let meta = fs::metadata(parent)?;
        ensure!(meta.is_dir(), "deployment ancestor must be a directory");
        below_sticky |= safe_directory(meta.uid(), meta.mode(), uid, below_sticky)?;
    }
    Ok(())
}

/// Nonblocking open prevents a FIFO/device substitution from hanging validation.
/// No reads follow symlinks; private files may contain credentials.
pub(super) fn regular(path: &Path, private: bool) -> Result<File> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
        "deployment paths must be absolute without parent traversal"
    );
    reject_symlinks(path)?;
    ancestors(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    let uid = unsafe { libc::geteuid() };
    ensure!(
        meta.is_file()
            && (meta.uid() == uid || (!private && meta.uid() == 0))
            && meta.permissions().mode() & (if private { 0o077 } else { 0o022 }) == 0,
        "deployment input must be a regular owned file with safe permissions"
    );
    Ok(file)
}

fn bytes(path: &Path) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    regular(path, true)?.take(65_537).read_to_end(&mut result)?;
    ensure!(
        result.len() <= 65_536,
        "deployment configuration exceeds limit"
    );
    Ok(result)
}

pub(super) fn hash_file(mut file: File, limit: u64) -> Result<(String, u64)> {
    let mut hash = Sha256::new();
    let mut count = 0;
    let mut buf = [0; 65_536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(count <= limit, "deployment file exceeds limit");
        hash.update(&buf[..n]);
    }
    Ok((format!("{:x}", hash.finalize()), count))
}

fn tool_digest(configuration: &Configuration, tool: &Tool, inputs: &Tool) -> Result<String> {
    identifier(&tool.id)?;
    ensure!(
        tool.args.len() <= 32 && tool.files.len() <= 16,
        "too many tool arguments or files"
    );
    for arg in &tool.args {
        ensure!(
            !arg.is_empty() && arg.len() <= 4096 && !arg.chars().any(char::is_control),
            "invalid fixed tool argument"
        );
        let path = Path::new(arg);
        if path.is_absolute() {
            ensure!(
                tool.files.iter().any(|file| file == path),
                "file arguments must be pinned supporting files"
            );
        } else {
            ensure!(
                !arg.contains('/'),
                "relative file arguments are unsupported"
            );
        }
    }
    let mut executable = regular(&inputs.executable, false)?;
    ensure!(
        executable.metadata()?.permissions().mode() & 0o111 != 0,
        "tool is not executable"
    );
    let mut prefix = [0; 2];
    executable.read_exact(&mut prefix)?;
    ensure!(
        &prefix != b"#!",
        "pin the resolved interpreter and script explicitly, not a shebang executable"
    );
    executable.seek(SeekFrom::Start(0))?;
    let executable_sha256 = hash_file(executable, 64 * 1024 * 1024)?.0;
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for file in &inputs.files {
        ensure!(seen.insert(file), "duplicate supporting file");
        files.push(hash_file(regular(file, false)?, 8 * 1024 * 1024)?.0);
    }
    let config_sha256 = inputs
        .config
        .as_deref()
        .map(|path| bytes(path).map(|b| format!("{:x}", Sha256::digest(b))))
        .transpose()?;
    Ok(super::digest(
        &json!({"configuration":configuration,"tool":tool.id,
        "executable_sha256":executable_sha256,"files":files,"config_sha256":config_sha256}),
    ))
}

/// Execute only private copies of the exact pinned bytes, never a path that
/// was hashed earlier. Preserve layout for explicitly pinned interpreter files.
pub(super) fn prepare(
    configuration: &Configuration,
    source: &Tool,
    expected_digest: &str,
    cwd: &Path,
) -> Result<Tool> {
    let root = cwd.join("pinned");
    let copy = |path: &Path, mode: u32| -> Result<PathBuf> {
        // Validate the source (including parent traversal and ancestors)
        // before its spelling can influence any destination folder creation.
        let mut input = regular(path, false)?;
        let target = root.join(path.strip_prefix("/")?);
        super::directory(target.parent().context("missing tool parent")?)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?;
        let limit = if path == source.executable {
            64 * 1024 * 1024
        } else {
            8 * 1024 * 1024
        };
        let copied = std::io::copy(&mut (&mut input).take(limit + 1), &mut output)?;
        ensure!(copied <= limit, "pinned tool input exceeds limit");
        output.set_permissions(fs::Permissions::from_mode(mode))?;
        Ok(target)
    };
    let mut tool = source.clone();
    tool.executable = copy(&source.executable, 0o500)?;
    tool.files = source
        .files
        .iter()
        .map(|p| copy(p, 0o400))
        .collect::<Result<_>>()?;
    tool.config = source
        .config
        .as_deref()
        .map(|p| copy(p, 0o400))
        .transpose()?;
    tool.args = source
        .args
        .iter()
        .map(|arg| {
            source
                .files
                .iter()
                .position(|p| p.as_os_str() == arg.as_str())
                .map(|i| tool.files[i].to_string_lossy().into_owned())
                .unwrap_or_else(|| arg.clone())
        })
        .collect();
    check_prepared(configuration, source, expected_digest, &tool)?;
    Ok(tool)
}

pub(super) fn check_prepared(
    configuration: &Configuration,
    source: &Tool,
    expected_digest: &str,
    tool: &Tool,
) -> Result<()> {
    ensure!(
        tool_digest(configuration, source, tool)? == expected_digest,
        "pinned deployment tool copy changed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sticky_exception_is_root_only_and_descendants_must_be_owned_and_unwritable() {
        assert!(safe_directory(0, 0o1777, 1000, false).unwrap());
        assert!(safe_directory(0, 0o777, 1000, false).is_err());
        assert!(safe_directory(1001, 0o1777, 1000, false).is_err());
        assert!(safe_directory(1000, 0o770, 1000, true).is_err());
        assert!(safe_directory(0, 0o755, 1000, true).is_err());
        assert!(!safe_directory(1000, 0o700, 1000, true).unwrap());
    }
}

fn installation_path() -> Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        .context("user configuration directory unavailable")?;
    Ok(config.join("privateer/deployment-installation.json"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmergencyPause {
    installation_id: String,
    paused: bool,
}

pub(super) fn unpaused(snapshot: &Snapshot) -> Result<()> {
    unpaused_at(
        snapshot,
        &installation_path()?.with_file_name("deployment-pause.json"),
    )
}

pub(super) fn unpaused_at(snapshot: &Snapshot, path: &Path) -> Result<()> {
    let pause: EmergencyPause = serde_json::from_slice(&bytes(path)?)
        .context("authoritative deployment emergency pause unavailable")?;
    ensure!(
        pause.installation_id == snapshot.configuration.installation_id && !pause.paused,
        "deployment emergency pause active or installation changed"
    );
    Ok(())
}

pub(super) fn load(store: &Store, environment: &str) -> Result<Snapshot> {
    load_at(store, environment, &installation_path()?)
}

pub(super) fn load_at(store: &Store, environment: &str, installation: &Path) -> Result<Snapshot> {
    identifier(environment)?;
    let installation: Installation = serde_json::from_slice(&bytes(installation)?)
        .context("invalid deployment installation configuration")?;
    identifier(&installation.id)?;
    let configuration: Configuration = serde_json::from_slice(&bytes(
        &store
            .home()
            .join("deployment")
            .join(format!("{environment}.json")),
    )?)
    .context("invalid deployment environment configuration")?;
    ensure!(
        configuration.schema_version == 1 && configuration.environment == environment,
        "deployment configuration version or environment mismatch"
    );
    ensure!(
        configuration.installation_id == installation.id
            && configuration.home == fs::canonicalize(store.home())?,
        "deployment installation or home changed; captain reconfiguration and consent required"
    );
    identifier(&configuration.trusted_build_policy)?;
    let verifier_sha256 = tool_digest(
        &configuration,
        &configuration.verifier,
        &configuration.verifier,
    )?;
    let adapter_sha256 = tool_digest(
        &configuration,
        &configuration.adapter,
        &configuration.adapter,
    )?;
    Ok(Snapshot {
        configuration,
        verifier_sha256,
        adapter_sha256,
    })
}
