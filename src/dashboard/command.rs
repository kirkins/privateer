//! Explicit argv boundary for operator actions. Outputs, deadlines and process
//! groups are bounded; no shell interprets captain text.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

pub const MAIL_PAGE_LIMIT: usize = 200;
const OUTPUT_LIMIT: usize = MAIL_PAGE_LIMIT * (65_536 + 4096) + 1024;

pub fn binary() -> Result<PathBuf> {
    let sibling = std::env::current_exe()?.with_file_name("privateer");
    Ok(if sibling.is_file() {
        sibling
    } else {
        PathBuf::from("privateer")
    })
}

pub fn existing_home(home: &Path) -> Result<()> {
    ensure!(
        home.is_absolute() && home.is_dir() && home.join("privateer.sqlite").is_file(),
        "ship home is missing: {}",
        home.display()
    );
    crate::store::reject_symlinks(&home.join("privateer.sqlite"))
}

pub fn json(binary: &Path, home: &Path, verb: &str, args: &Value) -> Result<Value> {
    existing_home(home)?;
    let mut cmd = Command::new(binary);
    cmd.arg("--home")
        .arg(home)
        .arg(verb)
        .arg("--json")
        .arg(serde_json::to_string(args)?);
    serde_json::from_slice(&run(cmd)?).context("invalid Privateer JSON response")
}

/// One plain sentence describing a completed operator command.
pub fn summary(verb: &str, value: &Value) -> String {
    let text = |key: &str| {
        let words = value[key].as_str().unwrap_or("").split_whitespace();
        crate::dashboard::model::clean(&words.collect::<Vec<_>>().join(" "), 512)
    };
    match verb {
        "init" => format!("Created {}.", text("ship")),
        "ship.create" => format!(
            "Created {} in {} · dispatcher {}.",
            text("ship"),
            text("folder"),
            text("dispatcher").trim_end_matches('.')
        ),
        "ship.retire" => format!("Retired {}; its folder is kept.", text("ship")),
        "mail.send" => format!(
            "{} sent to {}.",
            match value["kind"].as_str() {
                Some("reply") => "Reply",
                Some("request") => "Question",
                _ => "Message",
            },
            text("to")
        ),
        "service" => text("report"),
        _ => format!("{verb} completed."),
    }
}

pub fn initialize(
    binary: &Path,
    folder: &Path,
    name: &str,
    harness: &str,
    liaison: bool,
) -> Result<Value> {
    crate::store::Caller::from_env(None, None)?.require_operator("dashboard initialization")?;
    crate::store::identifier(name)?;
    ensure!(
        !folder
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "choose a normalized ship folder without .."
    );
    ensure!(
        folder.is_absolute()
            && folder.parent().is_some()
            && Some(folder) != std::env::var_os("HOME").as_deref().map(Path::new),
        "choose a dedicated absolute ship folder"
    );
    crate::store::reject_symlinks(folder)?;
    let home = folder.join(crate::cli::STATE_DIR);
    ensure!(
        !folder.exists() || std::fs::read_dir(folder)?.next().is_none(),
        "choose an empty ship folder"
    );
    std::fs::create_dir_all(folder)?;
    let mut cmd = Command::new(binary);
    cmd.current_dir(folder)
        .env_remove("PRIVATEER_HOME")
        .arg("--home")
        .arg(&home)
        .arg("init")
        .arg("--ship")
        .arg(name)
        .arg("--harness")
        .arg(harness);
    if liaison {
        cmd.arg("--liaison");
    }
    let report = String::from_utf8_lossy(&run(cmd)?).into_owned();
    let home = std::fs::canonicalize(home)?;
    let dispatcher = String::from_utf8_lossy(&service(binary, &home, "start")?).into_owned();
    Ok(
        serde_json::json!({"ship":name,"home":home,"harness":harness,"liaison":liaison,"report":report,"dispatcher":dispatcher}),
    )
}

pub fn service(binary: &Path, home: &Path, action: &str) -> Result<Vec<u8>> {
    existing_home(home)?;
    ensure!(
        matches!(action, "status" | "start" | "stop"),
        "invalid service action"
    );
    let mut cmd = Command::new(binary);
    cmd.arg("--home").arg(home).arg("service").arg(action);
    run(cmd)
}

pub fn run(mut cmd: Command) -> Result<Vec<u8>> {
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("launch command")?;
    let (send, recv) = mpsc::sync_channel(2);
    fn reader(
        mut pipe: impl Read + Send + 'static,
        send: mpsc::SyncSender<(bool, std::io::Result<Vec<u8>>)>,
        error: bool,
    ) {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe
                .by_ref()
                .take((OUTPUT_LIMIT + 1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = send.send((error, result));
        });
    }
    reader(child.stdout.take().unwrap(), send.clone(), false);
    reader(child.stderr.take().unwrap(), send, true);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut reaped = false;
    let mut output = None;
    let mut errors = None;
    let result = (|| -> Result<Vec<u8>> {
        loop {
            while let Ok((error, bytes)) = recv.try_recv() {
                let bytes = bytes?;
                ensure!(
                    bytes.len() <= OUTPUT_LIMIT,
                    "command output exceeds {OUTPUT_LIMIT} bytes; action outcome uncertain, refresh before retrying"
                );
                if error {
                    errors = Some(bytes)
                } else {
                    output = Some(bytes)
                }
            }
            // Do not reap the group leader while descendants hold its pipes.
            if let (Some(_), Some(diagnostic)) = (&output, &errors) {
                if let Some(status) = child.try_wait()? {
                    reaped = true;
                    ensure!(
                        status.success(),
                        "command failed: {}",
                        String::from_utf8_lossy(diagnostic)
                    );
                    return Ok(output.take().unwrap());
                }
            }
            if Instant::now() >= deadline {
                bail!(
                    "command deadline exceeded; action outcome uncertain, refresh before retrying"
                )
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() && !reaped {
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::summary;
    use serde_json::json;

    #[test]
    fn command_results_read_as_sentences() {
        let created = json!({"ship":"builds","folder":"/ships/builds","dispatcher":"not started (PRIVATEER_SERVICE=none)","actions":["inspect"]});
        assert_eq!(
            summary("ship.create", &created),
            "Created builds in /ships/builds · dispatcher not started (PRIVATEER_SERVICE=none)."
        );
        assert_eq!(
            summary(
                "mail.send",
                &json!({"kind":"reply","to":"builds","body":"x"})
            ),
            "Reply sent to builds."
        );
        assert_eq!(
            summary("mail.send", &json!({"kind":"request","to":"apps"})),
            "Question sent to apps."
        );
        assert_eq!(
            summary(
                "service",
                &json!({"report":"Dispatcher for harbor: not running\n(no user service).\n"})
            ),
            "Dispatcher for harbor: not running (no user service)."
        );
        assert_eq!(
            summary("ship.retire", &json!({"ship":"old","retired":true})),
            "Retired old; its folder is kept."
        );
        assert_eq!(summary("init", &json!({"ship":"apps"})), "Created apps.");
    }
}
