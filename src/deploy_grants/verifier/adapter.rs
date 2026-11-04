//! Immutable adapter inputs derived only from a fresh verifier snapshot.
use super::config::Snapshot;
use crate::jobs::deployment_admission::containment::{SealedExecutable, SealedInput};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{ffi::CString, fs::File};

pub(crate) struct PreparedAdapter {
    executable: SealedExecutable,
    inputs: Vec<SealedInput>,
    args: Vec<CString>,
    env: Vec<CString>,
    binding: Value,
}

impl PreparedAdapter {
    pub(super) fn prepare(snapshot: &Snapshot) -> Result<Self> {
        let tool = &snapshot.configuration.adapter;
        let source = snapshot.input(&tool.executable)?;
        let executable = SealedExecutable::copy(source.file(), source.digest())?;
        let mut inputs = Vec::new();
        let mut digests = Vec::new();
        let mut args = vec![CString::new(tool.id.as_str())?];
        for path in &tool.files {
            let source = snapshot.input(path)?;
            inputs.push(SealedInput::copy(
                source.file(),
                source.digest(),
                8 * 1024 * 1024,
            )?);
            digests.push(source.digest());
        }
        for arg in &tool.args {
            args.push(CString::new(
                if let Some(index) = tool
                    .files
                    .iter()
                    .position(|p| p.as_os_str() == arg.as_str())
                {
                    format!("/proc/self/fd/{}", 7 + index)
                } else {
                    arg.clone()
                },
            )?);
        }
        let mut env = vec![CString::new("PRIVATEER_ADAPTER_PROTOCOL=1")?];
        let mut config_digest = None;
        if let Some(path) = &tool.config {
            let source = snapshot.input(path)?;
            env.push(CString::new(format!(
                "PRIVATEER_ADAPTER_CONFIG=/proc/self/fd/{}",
                7 + inputs.len()
            ))?);
            inputs.push(SealedInput::copy(source.file(), source.digest(), 65_536)?);
            config_digest = Some(source.digest());
        }
        ensure!(inputs.len() <= 17, "too many sealed adapter inputs");
        let binding = json!({"adapter":snapshot.bindings()["adapter"],
            "executable_sha256":source.digest(), "files_sha256":digests,
            "config_sha256":config_digest});
        Ok(Self {
            executable,
            inputs,
            args,
            env,
            binding,
        })
    }

    pub(crate) fn executable(&self) -> &SealedExecutable {
        &self.executable
    }
    pub(crate) fn args(&self) -> &[CString] {
        &self.args
    }
    pub(crate) fn env(&self) -> &[CString] {
        &self.env
    }
    pub(crate) fn files(&self) -> impl Iterator<Item = &File> {
        self.inputs.iter().map(SealedInput::file)
    }
    pub(crate) fn binding(&self) -> &Value {
        &self.binding
    }
}
