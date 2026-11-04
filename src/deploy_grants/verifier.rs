//! Dormant evidence helper: no deployment authority, ledger writes or adapter
//! invocation. See the 2b-i contract in docs/design/standing-deploy-grants.md.
#[cfg(target_os = "linux")]
pub(crate) mod adapter;
mod artifact;
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
    artifact: artifact::Seal,
    inputs: config::Snapshot,
}
impl Verified {
    #[cfg(target_os = "linux")]
    pub(super) fn adapter(&self) -> Result<adapter::PreparedAdapter> {
        adapter::PreparedAdapter::prepare(&self.inputs)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn check_pause(&self) -> Result<()> {
        config::unpaused(&self.inputs)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn artifact_file(&self) -> Result<File> {
        Ok(self.artifact.file().try_clone()?)
    }

    pub(super) fn value(&self) -> &Value {
        &self.value
    }
}

pub(super) fn recheck(store: &Store, proof: &Verified) -> Result<()> {
    recheck_with(
        store,
        proof,
        |e, s| config::recheck(store, e, s),
        config::unpaused,
    )
}

fn recheck_with(
    store: &Store,
    proof: &Verified,
    load: impl FnOnce(&str, &config::Snapshot) -> Result<config::Snapshot>,
    pause: impl FnOnce(&config::Snapshot) -> Result<()>,
) -> Result<()> {
    recheck_attempt(store, proof, None, load, pause)
}

fn fresh(value: &Value) -> Result<()> {
    let now = Store::now_ms();
    let verified = value["verified_ms"]
        .as_i64()
        .context("missing verification time")?;
    ensure!(
        verified <= now && now.saturating_sub(verified) <= 300_000,
        "deployment evidence is stale or future-dated"
    );
    Ok(())
}

fn recheck_attempt(
    store: &Store,
    proof: &Verified,
    used: Option<&super::ledger::Recorded>,
    load: impl FnOnce(&str, &config::Snapshot) -> Result<config::Snapshot>,
    pause: impl FnOnce(&config::Snapshot) -> Result<()>,
) -> Result<()> {
    let value = proof.value();
    if let Some(used) = used {
        used.matches(value)?;
    }
    fresh(value)?;
    let grant = super::known(store, crate::store::required(value, "grant_id")?)?;
    ensure!(
        grant == value["grant_snapshot"],
        "deployment grant changed after verification"
    );
    ensure!(
        super::deployment_authority(
            store,
            &grant,
            crate::store::required(value, "task_id")?,
            used
        )? == value["authority"],
        "deployment task or charter changed after verification"
    );
    let snapshot = load(crate::store::required(value, "environment")?, &proof.inputs)?;
    ensure!(
        !snapshot.configuration.production
            && snapshot.bindings() == value["bindings"]
            && json!(snapshot.configuration.installation_id) == value["installation_id"]
            && json!(snapshot.configuration.home) == value["home"],
        "deployment configuration changed after verification"
    );
    // The full retained-byte hash ran BEFORE the writer lock. Bind it to the
    // still-open inode and size/mtime/ctime/mode under this lock: no 512 MiB
    // read can stall other lifecycle writers past their busy_timeout here.
    // The eventual uploader must also hash the bytes it actually streams.
    proof.artifact.check(&value["artifact"])?;
    pause(&snapshot)?;
    // Configuration reads can be slow. Do not accidentally extend evidence
    // lifetime by checking its age only before those bounded reads.
    fresh(value)
}

fn verify_with(
    store: &Store,
    grant_id: &str,
    args: Value,
    load: impl Fn(&str) -> Result<config::Snapshot>,
    timeout: Duration,
) -> Result<Verified> {
    verify_attempt(store, grant_id, args, None, load, timeout)
}

// Only the internal claim path may reverify against an already consumed use.
// The request comes from immutable runtime custody, never from retry arguments.
pub(super) fn verify_claim(store: &Store, used: &super::ledger::Recorded) -> Result<Verified> {
    verify_claim_with(
        store,
        used,
        |e| config::load(store, e),
        Duration::from_secs(30),
    )
}

fn verify_claim_with(
    store: &Store,
    used: &super::ledger::Recorded,
    load: impl Fn(&str) -> Result<config::Snapshot>,
    timeout: Duration,
) -> Result<Verified> {
    let value = used.value();
    verify_attempt(
        store,
        crate::store::required(value, "grant_id")?,
        json!({"task_id":value["task_id"],"commit":value["commit"],
            "artifact_path":value["artifact"]["path"],"artifact_sha256":value["artifact"]["sha256"]}),
        Some(used),
        load,
        timeout,
    )
}

pub(super) fn recheck_claim(
    store: &Store,
    proof: &Verified,
    used: &super::ledger::Recorded,
) -> Result<()> {
    recheck_attempt(
        store,
        proof,
        Some(used),
        |e, s| config::recheck(store, e, s),
        config::unpaused,
    )
}

fn verify_attempt(
    store: &Store,
    grant_id: &str,
    args: Value,
    used: Option<&super::ledger::Recorded>,
    load: impl Fn(&str) -> Result<config::Snapshot>,
    timeout: Duration,
) -> Result<Verified> {
    ensure!(
        store.connection().is_autocommit(),
        "deployment verification and artifact hashing must run outside a transaction"
    );
    let request: Request =
        serde_json::from_value(args).context("invalid artifact verification request")?;
    ensure!(
        hex(&request.commit, &[40, 64]) && hex(&request.artifact_sha256, &[64]),
        "full lowercase commit and artifact digests required"
    );
    let grant = super::known(store, grant_id)?;
    let authority = super::deployment_authority(store, &grant, &request.task_id, used)?;
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
        .permissions(fs::Permissions::from_mode(0o700))
        .prefix("verify-")
        .tempdir_in(store.home().join("deployment"))?;
    let tool = config::prepare(
        &snapshot.configuration,
        &snapshot.configuration.verifier,
        &snapshot.verifier_sha256,
        work.path(),
    )?;
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
    config::check_prepared(
        &snapshot.configuration,
        &snapshot.configuration.verifier,
        &snapshot.verifier_sha256,
        &tool,
    )?;
    let current = load(environment)?;
    ensure!(
        current.bindings() == bindings,
        "deployment configuration changed during verification"
    );
    ensure!(
        super::deployment_authority(store, &grant, &request.task_id, used)? == authority,
        "deployment task or charter changed during verification"
    );
    ensure!(
        super::known(store, grant_id)? == grant,
        "deployment grant changed during verification"
    );
    let evidence = serde_json::to_value(evidence)?;
    let artifact_seal = artifact::Seal::capture(&artifact)?;
    let proof = Verified {
        artifact: artifact_seal,
        inputs: current,
        value: json!({"grant_id":grant_id,"task_id":request.task_id,"authority":authority,"grant_snapshot":grant,"grant_revision":grant["revision"],"environment":environment,
        "installation_id":snapshot.configuration.installation_id,"home":snapshot.configuration.home,
        "rule_digest":digest(&grant["artifact_rule"]),"bindings":bindings,"artifact":artifact,
        "commit":request.commit,"evidence_digest":digest(&evidence),"evidence":evidence,
        "verified_ms":verified_ms,"deployment_authorized":false}),
    };
    if let Some(used) = used {
        used.matches(proof.value())?;
    }
    Ok(proof)
}

#[cfg(test)]
mod tests;
