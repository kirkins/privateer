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
                .take(1_048_577)
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
                    bytes.len() <= 1_048_576,
                    "command output exceeds 1 MiB; action outcome uncertain, refresh before retrying"
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
