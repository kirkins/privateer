//! One bounded upload per installation. Caller strings never become paths:
//! the only on-disk upload name is `upgrade-upload/incoming`; its opaque id
//! is compared with the server-generated UUID in its manifest.
use super::*;
use sha2::{Digest, Sha256};
use std::io::Read;

pub(super) const CHUNK: usize = 192 * 1024;
const MAX_TOTAL: u64 = 1 << 30;
const IDLE_MS: i64 = 30 * 60 * 1000;

fn directory() -> Result<(PathBuf, fs::File)> {
    let root = data()?.join("upgrade-upload");
    crate::store::private_dirs(&root)?;
    let guard = lock(&root.join("lock"))?;
    let incoming = root.join("incoming");
    if incoming.exists() {
        let modified = fs::metadata(&incoming)?.modified()?;
        if modified.elapsed().unwrap_or_default().as_millis() > IDLE_MS as u128 {
            fs::remove_dir_all(&incoming)?;
        }
    }
    Ok((incoming, guard))
}

fn manifest(dir: &Path, id: &str) -> Result<Value> {
    valid_id(id)?;
    let state: Value = serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
    ensure!(state["id"] == id, "unknown upload id");
    Ok(state)
}

pub(super) fn encode(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        text.push(HEX[(b >> 4) as usize] as char);
        text.push(HEX[(b & 15) as usize] as char);
    }
    text
}

fn decode(text: &str) -> Result<Vec<u8>> {
    ensure!(
        !text.is_empty() && text.len() <= CHUNK * 2 && text.len().is_multiple_of(2),
        "chunk exceeds size bound or has odd/empty hex"
    );
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |b: u8| -> Result<u8> {
                match b {
                    b'0'..=b'9' => Ok(b - b'0'),
                    b'a'..=b'f' => Ok(b - b'a' + 10),
                    _ => bail!("invalid chunk hex"),
                }
            };
            Ok(digit(pair[0])? * 16 + digit(pair[1])?)
        })
        .collect()
}

pub(super) fn receive(args: &Value) -> Result<Value> {
    let (dir, _guard) = directory()?;
    let action = args["action"].as_str().context("chunk action required")?;
    if action == "begin" {
        ensure!(
            !dir.exists(),
            "an upload is already active; abort it or wait for its idle expiry"
        );
        let commit = args["commit"].as_str().context("commit required")?;
        ensure!(super::super::valid_commit(commit), "invalid commit");
        let platform = args["platform"].as_str().context("platform required")?;
        ensure!(
            fetch::host_platform().ok() == Some(platform),
            "upload platform does not match this machine"
        );
        let digest = args["digest"]
            .as_str()
            .context("GitHub sha256 digest required")?;
        ensure!(
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "invalid sha256 digest"
        );
        let total = args["total"].as_u64().context("total required")?;
        ensure!(
            total > 0 && total <= MAX_TOTAL,
            "upload total exceeds 1 GiB bound (or is zero)"
        );
        crate::store::private_dirs(&dir)?;
        let id = uuid::Uuid::new_v4().to_string();
        let result = (|| -> Result<Value> {
            fs::File::create(dir.join("artifact.zip"))?;
            save(
                &dir.join("manifest.json"),
                &json!({"id":id,"commit":commit,"platform":platform,"digest":digest,"total":total,"offset":0,"complete":false}),
            )?;
            Ok(json!({"id":id,"chunk_bytes":CHUNK,"max_total":MAX_TOTAL}))
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&dir);
        }
        return result;
    }
    let id = args["id"].as_str().context("upload id required")?;
    // A stale or bogus id must not destroy a different active transfer.
    let mut state = manifest(&dir, id)?;
    let result = (|| -> Result<Value> {
        match action {
            "abort" => {
                fs::remove_dir_all(&dir)?;
                Ok(json!({"aborted":true}))
            }
            "append" => {
                ensure!(state["complete"] != true, "upload already complete");
                let offset = args["offset"].as_u64().context("offset required")?;
                ensure!(
                    state["offset"].as_u64() == Some(offset),
                    "out-of-order chunk offset"
                );
                let bytes = decode(args["hex"].as_str().context("chunk hex required")?)?;
                let next = offset
                    .checked_add(bytes.len() as u64)
                    .context("offset overflow")?;
                ensure!(
                    next <= state["total"].as_u64().context("total")?,
                    "chunk exceeds declared total"
                );
                let mut file = fs::OpenOptions::new()
                    .append(true)
                    .open(dir.join("artifact.zip"))?;
                ensure!(
                    file.metadata()?.len() == offset,
                    "partial file length differs from offset"
                );
                file.write_all(&bytes)?;
                file.sync_data()?;
                state["offset"] = json!(next);
                save(&dir.join("manifest.json"), &state)?;
                Ok(json!({"id":id,"offset":next}))
            }
            "finish" => {
                ensure!(state["offset"] == state["total"], "upload incomplete");
                let mut file = fs::File::open(dir.join("artifact.zip"))?;
                let mut hash = Sha256::new();
                std::io::copy(&mut file, &mut hash)?;
                ensure!(
                    state["digest"].as_str() == Some(&format!("{:x}", hash.finalize())),
                    "assembled artifact does not match GitHub digest"
                );
                state["complete"] = json!(true);
                save(&dir.join("manifest.json"), &state)?;
                Ok(json!({"id":id,"complete":true}))
            }
            _ => bail!("unknown chunk action"),
        }
    })();
    // Malformed, corrupt, out-of-order and failed writes cannot leave a
    // resumable poisoned transfer consuming disk space.
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

pub(super) fn take(id: &str, commit: &str) -> Result<(fetch::Staging, Value)> {
    let (dir, _guard) = directory()?;
    let state = manifest(&dir, id)?;
    ensure!(
        state["commit"] == commit && state["complete"] == true,
        "upload is not complete for this commit"
    );
    let staging = fetch::Staging(
        data()?
            .join("staging")
            .join(uuid::Uuid::new_v4().to_string()),
    );
    crate::store::private_dirs(staging.0.parent().context("staging")?)?;
    fs::rename(&dir, &staging.0)?;
    Ok((staging, state))
}

pub(super) fn push(name: &str, commit: &str, platform: &str) -> Result<Option<String>> {
    let Some(artifact) = fetch::for_push(commit, platform)? else {
        return Ok(None);
    };
    let total = fs::metadata(&artifact.zip)?.len();
    let started = call(
        name,
        "upgrade.chunk",
        json!({"action":"begin","commit":commit,"platform":platform,"digest":artifact.digest,"total":total}),
    )?;
    let id = started["id"].as_str().context("upload id")?.to_owned();
    let result = (|| -> Result<()> {
        let mut file = fs::File::open(&artifact.zip)?;
        let mut buf = vec![0; CHUNK];
        let mut offset = 0u64;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let reply = call(
                name,
                "upgrade.chunk",
                json!({"action":"append","id":id,"offset":offset,"hex":encode(&buf[..n])}),
            )?;
            offset += n as u64;
            ensure!(
                reply["offset"].as_u64() == Some(offset),
                "chunk acknowledgement has wrong offset"
            );
        }
        call(name, "upgrade.chunk", json!({"action":"finish","id":id}))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = call(name, "upgrade.chunk", json!({"action":"abort","id":id}));
    }
    result?;
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_chunks_are_bounded_and_round_trip_every_byte() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
        assert_eq!(decode(&"00".repeat(CHUNK)).unwrap().len(), CHUNK);
        for bad in [
            "".into(),
            "0".into(),
            "GG".into(),
            "é".into(),
            "00".repeat(CHUNK + 1),
        ] {
            assert!(decode(&bad).is_err());
        }
    }

    #[test]
    fn ids_are_canonical_opaque_uuids_not_paths() {
        assert!(valid_id("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").is_ok());
        for bad in [
            "../../outside",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
            "",
            "/tmp/upload",
        ] {
            assert!(valid_id(bad).is_err(), "accepted {bad:?}");
        }
    }
}
