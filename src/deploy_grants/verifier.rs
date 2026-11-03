//! Dormant evidence helper: no deployment authority, ledger writes or adapter
//! invocation. See the 2b-i contract in docs/design/standing-deploy-grants.md.
mod config;
mod process;

use crate::store::{Store, identifier, private_dirs, reject_symlinks};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const ARTIFACT_LIMIT: u64 = 512 * 1024 * 1024;
fn digest(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}
fn hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    task_id: String,
    commit: String,
    artifact_path: PathBuf,
    artifact_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    id: String,
    run_id: String,
    name: String,
    commit: String,
    app_id: String,
    workflow_path: String,
    event: String,
    kind: String,
    status: String,
    conclusion: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attestation {
    id: String,
    mode: String,
    repository_id: String,
    commit: String,
    artifact_sha256: String,
    app_id: String,
    workflow_path: String,
    trusted_build_policy: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    schema_version: u32,
    repository_id: String,
    branch_ref: String,
    commit: String,
    branch_tip: String,
    artifact_sha256: String,
    checks: Vec<Check>,
    attestation: Attestation,
}

fn matched(
    evidence: &Evidence,
    rule: &super::ArtifactRule,
    request: &Request,
    policy: &str,
) -> Result<()> {
    ensure!(
        evidence.schema_version == 1
            && evidence.repository_id == rule.repository_id
            && evidence.branch_ref == rule.branch_ref
            && evidence.commit == request.commit
            && evidence.branch_tip == request.commit
            && evidence.artifact_sha256 == request.artifact_sha256,
        "verifier artifact, branch tip or repository mismatch"
    );
    ensure!(
        evidence.checks.len() == rule.required_checks.len() && evidence.checks.len() <= 32,
        "verifier check set mismatch"
    );
    let mut names = std::collections::HashSet::new();
    let mut ids = std::collections::HashSet::new();
    for check in &evidence.checks {
        identifier(&check.id)?;
        identifier(&check.run_id)?;
        ensure!(
            names.insert(&check.name)
                && ids.insert(&check.id)
                && rule.required_checks.contains(&check.name),
            "ambiguous or unexpected verifier check"
        );
        ensure!(
            check.commit == request.commit
                && check.app_id == rule.app_id
                && check.workflow_path == rule.workflow_path
                && check.event == rule.event
                && check.kind == "check_run"
                && check.status == "completed"
                && check.conclusion == "success",
            "incomplete, failed or untrusted verifier check"
        );
    }
    let attestation = &evidence.attestation;
    super::text(&attestation.id, "attestation identity", 512)?;
    ensure!(
        attestation.mode == "ci_attestation"
            && attestation.repository_id == rule.repository_id
            && attestation.commit == request.commit
            && attestation.artifact_sha256 == request.artifact_sha256
            && attestation.app_id == rule.app_id
            && attestation.workflow_path == rule.workflow_path
            && attestation.trusted_build_policy == policy,
        "artifact attestation mismatch"
    );
    Ok(())
}

fn directory(path: &Path) -> Result<()> {
    reject_symlinks(path)?;
    private_dirs(path)?;
    let meta = fs::metadata(path)?;
    ensure!(
        meta.is_dir()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.permissions().mode() & 0o077 == 0,
        "runtime artifact directory must be private and owned"
    );
    Ok(())
}

fn custody(home: &Path, request: &Request) -> Result<Value> {
    let artifacts = home.join("deployment/artifacts");
    directory(&artifacts)?;
    let mut source = config::regular(&request.artifact_path, false)?;
    let mut copy = tempfile::NamedTempFile::new_in(&artifacts)?;
    let (mut hash, mut size, mut buffer) = (Sha256::new(), 0, [0; 65_536]);
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        ensure!(size <= ARTIFACT_LIMIT, "artifact exceeds limit");
        hash.update(&buffer[..n]);
        copy.write_all(&buffer[..n])?;
    }
    ensure!(
        format!("{:x}", hash.finalize()) == request.artifact_sha256,
        "artifact bytes do not match attestation digest"
    );
    copy.as_file()
        .set_permissions(fs::Permissions::from_mode(0o400))?;
    copy.as_file().sync_all()?;
    let path = artifacts.join(&request.artifact_sha256);
    if let Err(error) = copy.persist_noclobber(&path) {
        ensure!(
            error.error.kind() == std::io::ErrorKind::AlreadyExists,
            "cannot retain verified artifact"
        );
        let (hash, stored_size) = config::hash_file(config::regular(&path, true)?, ARTIFACT_LIMIT)?;
        ensure!(
            hash == request.artifact_sha256 && stored_size == size,
            "stored artifact was changed"
        );
    }
    File::open(&artifacts)?.sync_all()?;
    Ok(json!({"path":path,"sha256":request.artifact_sha256,"size_bytes":size}))
}

/// Captain-only preparation aid. This does not create policy or consent and is
/// intentionally not registered in RPC, CLI, door, relay or MCP dispatch.
pub fn inspect(store: &Store, environment: &str) -> Result<Value> {
    super::captain(store, "deployment verifier configuration inspection")?;
    Ok(config::load(store, environment)?.bindings())
}

/// Verify and retain bytes, never consume authority. Checks task ownership,
/// deploy_nonproduction and the remaining cross-revision budget. Consumption
/// still needs resource admission and writer-transaction authority/age checks.
pub fn verify(store: &Store, grant_id: &str, args: Value) -> Result<Value> {
    verify_with(
        store,
        grant_id,
        args,
        |environment| config::load(store, environment),
        Duration::from_secs(30),
    )
    .map(|proof| proof.value)
}

/// Sealed proof: only a successful verifier run can construct one. Not
/// serializable/deserializable and never accepted from an RPC caller.
#[derive(Debug)]
pub(super) struct Verified {
    value: Value,
}
impl Verified {
    pub(super) fn value(&self) -> &Value {
        &self.value
    }
}

pub(super) fn recheck(store: &Store, proof: &Verified) -> Result<()> {
    recheck_with(store, proof, |e| config::load(store, e), config::unpaused)
}

fn recheck_with(
    store: &Store,
    proof: &Verified,
    load: impl FnOnce(&str) -> Result<config::Snapshot>,
    pause: impl FnOnce(&config::Snapshot) -> Result<()>,
) -> Result<()> {
    let value = proof.value();
    let now = Store::now_ms();
    let verified = value["verified_ms"]
        .as_i64()
        .context("missing verification time")?;
    ensure!(
        verified <= now && now.saturating_sub(verified) <= 300_000,
        "deployment evidence is stale or future-dated"
    );
    let grant = super::known(store, crate::store::required(value, "grant_id")?)?;
    ensure!(
        grant == value["grant_snapshot"],
        "deployment grant changed after verification"
    );
    ensure!(
        super::consumption_authority(store, &grant, crate::store::required(value, "task_id")?)?
            == value["authority"],
        "deployment task or charter changed after verification"
    );
    let snapshot = load(crate::store::required(value, "environment")?)?;
    ensure!(
        !snapshot.configuration.production
            && snapshot.bindings() == value["bindings"]
            && json!(snapshot.configuration.installation_id) == value["installation_id"]
            && json!(snapshot.configuration.home) == value["home"],
        "deployment configuration changed after verification"
    );
    pause(&snapshot)
}

fn verify_with(
    store: &Store,
    grant_id: &str,
    args: Value,
    load: impl Fn(&str) -> Result<config::Snapshot>,
    timeout: Duration,
) -> Result<Verified> {
    let request: Request =
        serde_json::from_value(args).context("invalid artifact verification request")?;
    ensure!(
        hex(&request.commit, &[40, 64]) && hex(&request.artifact_sha256, &[64]),
        "full lowercase commit and artifact digests required"
    );
    let grant = super::known(store, grant_id)?;
    let authority = super::consumption_authority(store, &grant, &request.task_id)?;
    ensure!(
        grant["state"] == "active" && grant["production"] == false,
        "only active nonproduction grants can be verified"
    );
    let grantee = crate::store::required(&grant, "grantee")?;
    super::hosted_grantee(store, grantee)?;
    let environment = crate::store::required(&grant, "environment")?;
    let rule: super::ArtifactRule = serde_json::from_value(grant["artifact_rule"].clone())?;
    ensure!(
        rule.provenance_mode == "ci_attestation",
        "runtime_build verification is not implemented"
    );
    let snapshot = load(environment).context("captain deployment configuration unavailable")?;
    let bindings = snapshot.bindings();
    ensure!(
        !snapshot.configuration.production
            && bindings["verifier"] == grant["verifier"]
            && bindings["adapter"] == grant["adapter"],
        "deployment classification or tool binding mismatch"
    );
    directory(&store.home().join("deployment"))?;
    let work = tempfile::Builder::new()
        .prefix("verify-")
        .tempdir_in(store.home().join("deployment"))?;
    let tool = config::prepare(&snapshot, work.path())?;
    let evidence = process::invoke(
        &tool,
        work.path(),
        &json!({
        "schema_version":1,"artifact_rule":grant["artifact_rule"],"commit":request.commit,
        "artifact_sha256":request.artifact_sha256,"trusted_build_policy":snapshot.configuration.trusted_build_policy}),
        timeout,
    )?;
    // Freshness starts at the verifier response, not after a potentially slow
    // artifact copy or subsequent authority rechecks.
    let verified_ms = Store::now_ms();
    let evidence: Evidence = serde_json::from_value(evidence)
        .map_err(|_| anyhow::anyhow!("invalid verifier evidence schema; diagnostics withheld"))?;
    matched(
        &evidence,
        &rule,
        &request,
        &snapshot.configuration.trusted_build_policy,
    )?;
    let artifact = custody(store.home(), &request)?;
    config::check_prepared(&snapshot, &tool)?;
    let current = load(environment)?;
    ensure!(
        current.bindings() == bindings,
        "deployment configuration changed during verification"
    );
    ensure!(
        super::consumption_authority(store, &grant, &request.task_id)? == authority,
        "deployment task or charter changed during verification"
    );
    ensure!(
        super::known(store, grant_id)? == grant,
        "deployment grant changed during verification"
    );
    let evidence = serde_json::to_value(evidence)?;
    Ok(Verified {
        value: json!({"grant_id":grant_id,"task_id":request.task_id,"authority":authority,"grant_snapshot":grant,"grant_revision":grant["revision"],"environment":environment,
        "installation_id":snapshot.configuration.installation_id,"home":snapshot.configuration.home,
        "rule_digest":digest(&grant["artifact_rule"]),"bindings":bindings,"artifact":artifact,
        "commit":request.commit,"evidence_digest":digest(&evidence),"evidence":evidence,
        "verified_ms":verified_ms,"deployment_authorized":false}),
    })
}

#[cfg(test)]
mod tests;
