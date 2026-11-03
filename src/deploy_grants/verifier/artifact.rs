//! Hash retained bytes without a home writer lock; recheck only identity and
//! metadata under that lock. An uploader must still hash its actual stream.
use super::{ARTIFACT_LIMIT, config};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::{fs::File, os::unix::fs::MetadataExt, path::Path, sync::Arc};

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
#[derive(Debug)]
pub(super) struct Seal {
    file: Arc<File>,
    stamp: Stamp,
    sha256: String,
}
impl Seal {
    pub(super) fn capture(artifact: &Value) -> Result<Self> {
        let file = config::regular(Path::new(crate::store::required(artifact, "path")?), true)?;
        let stamp = Stamp::read(&file)?;
        let (sha256, size) = config::hash_file(file.try_clone()?, ARTIFACT_LIMIT)?;
        ensure!(
            stamp == Stamp::read(&file)?
                && artifact["sha256"].as_str() == Some(&sha256)
                && artifact["size_bytes"].as_u64() == Some(size),
            "retained deployment artifact changed"
        );
        Ok(Self {
            file: Arc::new(file),
            stamp,
            sha256,
        })
    }

    pub(super) fn check(&self, artifact: &Value) -> Result<()> {
        let current = config::regular(Path::new(crate::store::required(artifact, "path")?), true)?;
        ensure!(
            self.stamp == Stamp::read(&self.file)?
                && self.stamp == Stamp::read(&current)?
                && artifact["sha256"].as_str() == Some(&self.sha256)
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
