//! Hash retained bytes without a home writer lock; recheck only identity and
//! metadata under that lock. An uploader must still hash its actual stream.
use super::{ARTIFACT_LIMIT, config};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::{
    fs::File,
    os::unix::fs::{FileExt, MetadataExt},
    path::Path,
    sync::Arc,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    mode: u32,
    uid: u32,
    links: u64,
}
impl Stamp {
    fn read(file: &File) -> Result<Self> {
        let m = file.metadata()?;
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
            mode: m.mode(),
            uid: m.uid(),
            links: m.nlink(),
        })
    }
}

/// Holds the inode open so deletion/recreation cannot reuse its identity. Never
/// deserialized from evidence JSON. Same-account writers are not a sandbox:
/// this seal narrows the transaction check, not the eventual upload check.
/// Filesystems with coarse ctime granularity cannot detect every same-tick,
/// same-size rewrite restoring mtime; consumers must hash their actual stream.
#[derive(Clone, Debug)]
pub(super) struct Seal {
    file: Arc<File>,
    stamp: Stamp,
    sha256: String,
}
impl Seal {
    pub(super) fn capture(artifact: &Value) -> Result<Self> {
        let seal = Self::input(
            Path::new(crate::store::required(artifact, "path")?),
            true,
            ARTIFACT_LIMIT,
        )?;
        ensure!(
            artifact["sha256"].as_str() == Some(seal.digest())
                && artifact["size_bytes"].as_u64() == Some(seal.stamp.size),
            "retained deployment artifact changed"
        );
        Ok(seal)
    }

    pub(super) fn input(path: &Path, private: bool, limit: u64) -> Result<Self> {
        let file = config::regular(path, private)?;
        let stamp = Stamp::read(&file)?;
        let (sha256, _) = config::hash_file(file.try_clone()?, limit)?;
        ensure!(
            stamp == Stamp::read(&file)?,
            "deployment input changed while hashing"
        );
        Ok(Self {
            file: Arc::new(file),
            stamp,
            sha256,
        })
    }

    pub(super) fn digest(&self) -> &str {
        &self.sha256
    }

    /// Inspect the SAME held inode that supplied the executable digest, never
    /// an earlier path open that may name different bytes by hash time.
    pub(super) fn check_executable(&self) -> Result<()> {
        ensure!(self.stamp.mode & 0o111 != 0, "tool is not executable");
        let mut prefix = [0; 2];
        self.file.read_exact_at(&mut prefix, 0)?;
        ensure!(
            &prefix != b"#!",
            "pin the resolved interpreter and script explicitly, not a shebang executable"
        );
        ensure!(
            self.stamp == Stamp::read(&self.file)?,
            "deployment executable changed since hashing"
        );
        Ok(())
    }

    pub(super) fn check_input(&self, path: &Path, private: bool) -> Result<()> {
        let current = config::regular(path, private)?;
        ensure!(
            self.stamp == Stamp::read(&self.file)? && self.stamp == Stamp::read(&current)?,
            "deployment input changed since hashing"
        );
        Ok(())
    }

    pub(super) fn check(&self, artifact: &Value) -> Result<()> {
        self.check_input(Path::new(crate::store::required(artifact, "path")?), true)?;
        ensure!(
            artifact["sha256"].as_str() == Some(&self.sha256)
                && artifact["size_bytes"].as_u64() == Some(self.stamp.size),
            "retained deployment artifact changed since hashing"
        );
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn repeat(&self) -> Self {
        Self {
            file: Arc::clone(&self.file),
            stamp: self.stamp.clone(),
            sha256: self.sha256.clone(),
        }
    }
}
