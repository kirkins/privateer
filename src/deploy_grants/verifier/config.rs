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

#[derive(Serialize, Deserialize)]
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

/// Nonblocking open prevents a FIFO/device substitution from hanging validation.
/// No reads follow symlinks; private files may contain credentials.
pub(super) fn regular(path: &Path, private: bool) -> Result<File> {
    ensure!(path.is_absolute(), "deployment paths must be absolute");
    reject_symlinks(path)?;
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

fn tool_digest(configuration: &Configuration, tool: &Tool) -> Result<String> {
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
    let mut executable = regular(&tool.executable, false)?;
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
    for file in &tool.files {
        ensure!(seen.insert(file), "duplicate supporting file");
        files.push(hash_file(regular(file, false)?, 8 * 1024 * 1024)?.0);
    }
    let config_sha256 = tool
        .config
        .as_deref()
        .map(|path| bytes(path).map(|b| format!("{:x}", Sha256::digest(b))))
        .transpose()?;
    Ok(super::digest(
        &json!({"configuration":configuration,"tool":tool.id,
        "executable_sha256":executable_sha256,"files":files,"config_sha256":config_sha256}),
    ))
}

fn installation_path() -> Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        .context("user configuration directory unavailable")?;
    Ok(config.join("privateer/deployment-installation.json"))
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
    let verifier_sha256 = tool_digest(&configuration, &configuration.verifier)?;
    let adapter_sha256 = tool_digest(&configuration, &configuration.adapter)?;
    Ok(Snapshot {
        configuration,
        verifier_sha256,
        adapter_sha256,
    })
}
