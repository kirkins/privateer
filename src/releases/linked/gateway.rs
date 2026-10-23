use super::*;

fn commit(args: &Value) -> Result<&str> {
    let commit = args["commit"].as_str().context("commit is required")?;
    ensure!(
        super::super::valid_commit(commit),
        "commit must be a full lowercase sha"
    );
    Ok(commit)
}

pub(crate) fn serve(command: &str, args: &Value) -> Result<Value> {
    match command {
        "upgrade.probe" => {
            let commit = commit(args)?;
            let release = Release::new(&data()?, commit)?;
            let exists = fs::symlink_metadata(&release.root).is_ok();
            let checked = fetch::existing(&release.root, "exists");
            let uname = fetch::probe("uname", &["-sm"]);
            let glibc = std::env::var("PRIVATEER_GLIBC")
                .ok()
                .or_else(|| fetch::probe("getconf", &["GNU_LIBC_VERSION"]));
            let gh = std::env::var("PRIVATEER_GH").unwrap_or_else(|_| "gh".into());
            Ok(
                json!({"commit":commit,"platform":fetch::host_platform().ok(),
                "uname":uname,"glibc":glibc,"current_commit":super::super::current()?.map(|r| r.commit),
                "exists":exists,"verified":checked.is_ok(),"verification_error":checked.err().map(|e| e.to_string()),
                "gh":fetch::probe(&gh, &["--version"]).is_some()}),
            )
        }
        "upgrade.prepare" => {
            let commit = commit(args)?;
            if let Some(upload) = args["upload"].as_str() {
                valid_id(upload)?;
            }
            spawn("prepare", json!({"commit":commit,"upload":args["upload"]}))
        }
        "upgrade.apply" => apply::detached(&apply::Options {
            commit: Some(commit(args)?.into()),
            dry_run: false,
        }),
        "upgrade.rollback" => apply::detached(&apply::Options {
            commit: None,
            dry_run: false,
        }),
        "upgrade.progress" => {
            let id = args["id"].as_str().context("id required")?;
            match args["kind"].as_str().unwrap_or("apply") {
                "apply" => apply::progress(id),
                kind @ ("prepare" | "linked") => progress(kind, id),
                _ => bail!("invalid progress kind"),
            }
        }
        "upgrade.chunk" => transfer::receive(args),
        _ => bail!("unknown gateway command {command}"),
    }
}

pub(super) fn prepare(state: &Value) -> Result<Value> {
    let commit = commit(state)?;
    let release = Release::new(&data()?, commit)?;
    if let Some(upload) = state["upload"].as_str() {
        let (staging, manifest) = transfer::take(upload, commit)?;
        let platform = manifest["platform"].as_str().context("upload platform")?;
        let digest = manifest["digest"].as_str().context("upload digest")?;
        let report = fetch::receive(
            commit,
            platform,
            digest,
            &staging.0.join("artifact.zip"),
            &staging.0,
        )?;
        fetch::existing(&release.root, "prepared")?;
        return Ok(report);
    }
    let mut args = vec![commit.to_owned()];
    // This is the remote operator's environment, never a caller-supplied path.
    if let Ok(source) = std::env::var("PRIVATEER_UPGRADE_SOURCE") {
        args.extend(["--source".into(), source]);
    }
    let gh = std::env::var("PRIVATEER_GH").unwrap_or_else(|_| "gh".into());
    if fs::symlink_metadata(&release.root).is_err() && fetch::probe(&gh, &["--version"]).is_none() {
        ensure!(
            args.len() > 1,
            "no gh or pushed artifact; configure PRIVATEER_UPGRADE_SOURCE on this machine"
        );
        args.push("--build".into());
    }
    let report = fetch::fetch(&fetch::Options::parse(&args)?)?;
    fetch::existing(&release.root, "prepared")?;
    Ok(report)
}
