//! Opening and closing Xcode windows: the configured helper commands, run
//! with the broker's minimal environment, in `/`, with a deadline; and the
//! record of the windows the broker opened, so that it, or a broker started
//! after it, closes exactly those.
use crate::{sys, upstream};
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

/// A helper that runs longer is killed, with its whole process group.
const HELPER_DEADLINE: Duration = Duration::from_secs(30);
/// The most of a helper's stderr that is read.
const HELPER_OUTPUT: u64 = 64 * 1024;
/// Executables a helper may not be: a shell would run whatever it is told.
const SHELLS: [&str; 12] = [
    "sh", "bash", "zsh", "dash", "ksh", "mksh", "fish", "csh", "tcsh", "env", "busybox", "xargs",
];

/// A window the broker opened.
#[derive(Clone, PartialEq, Eq)]
pub struct Opened {
    pub root: PathBuf,
    pub project: PathBuf,
    /// Learned once a window list shows it.
    pub tab: Option<String>,
    pub opened_ms: u64,
}

impl Opened {
    pub fn json(&self) -> Value {
        json!({"root": self.root, "project": self.project, "tab": self.tab, "opened_ms": self.opened_ms})
    }
}

/// A helper argv as `--window-open` and `--window-close` give it: a JSON
/// array whose first element is an absolute, normalized path that is not a
/// shell.
pub fn helper_argv(text: &str) -> Result<Vec<String>, String> {
    let argv: Vec<String> =
        serde_json::from_str(text).map_err(|_| format!("{text} is not a JSON array of strings"))?;
    let valid = (1..=32).contains(&argv.len())
        && argv
            .iter()
            .all(|a| a.len() <= 4096 && !a.chars().any(char::is_control));
    if !valid {
        return Err("a window helper has 1 to 32 arguments without control characters".into());
    }
    let program = Path::new(&argv[0]);
    let normalized = program.is_absolute()
        && program.components().all(|c| {
            matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
        && !argv[0].ends_with('/');
    if !normalized {
        return Err("a window helper must start with an absolute, normalized path".into());
    }
    let base = program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if SHELLS.contains(&base.as_str()) {
        return Err(format!("a window helper may not be {base}"));
    }
    Ok(argv)
}

/// Run `argv` with `project` appended. Its first stderr line, escaped, is
/// the error when it exits non-zero; past the deadline its group is killed.
/// Never called with the broker's state locked.
pub fn run(argv: &[String], project: &Path) -> Result<(), String> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .arg(project)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    upstream::minimal_env(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run {}: {}", argv[0], error.kind()))?;
    let pid = child.id();
    let (sender, received) = sync_channel(1);
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut text = String::new();
            let mut stderr = stderr;
            let _ = (&mut stderr).take(HELPER_OUTPUT).read_to_string(&mut text);
            let _ = sender.send(text);
            // Drain the rest so a chatty helper never blocks on a full pipe.
            let _ = io::copy(&mut stderr, &mut io::sink());
        });
    }
    let deadline = Instant::now() + HELPER_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                sys::signal_group(pid, libc::SIGKILL);
                let _ = child.wait();
                return Err(format!(
                    "{} did not finish within {} s",
                    argv[0],
                    HELPER_DEADLINE.as_secs()
                ));
            }
            Err(error) => return Err(format!("lost {}: {}", argv[0], error.kind())),
        }
    };
    if status.success() {
        return Ok(());
    }
    let text = received
        .recv_timeout(Duration::from_secs(1))
        .unwrap_or_default();
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    Err(format!(
        "{} exited with {status}: {}",
        argv[0],
        sys::quoted(first, 200)
    ))
}

/// The record `run/NAME.windows.json`, read through the same checks as every
/// broker file. Missing or unreadable means none.
pub fn load(path: &Path) -> Vec<Opened> {
    let Ok(file) = sys::open_checked(path, OpenOptions::new().read(true)) else {
        return vec![];
    };
    let mut bytes = vec![];
    if file.take(1 << 20).read_to_end(&mut bytes).is_err() {
        return vec![];
    }
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return vec![];
    };
    value["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| {
            let root = PathBuf::from(w["root"].as_str()?);
            let project = PathBuf::from(w["project"].as_str()?);
            (root.is_absolute() && project.is_absolute()).then(|| Opened {
                root,
                project,
                tab: w["tab"].as_str().map(str::to_owned),
                opened_ms: w["opened_ms"].as_u64().unwrap_or(0),
            })
        })
        .collect()
}

pub fn save(path: &Path, windows: &[Opened]) -> io::Result<()> {
    let list: Vec<Value> = windows.iter().map(Opened::json).collect();
    sys::write_atomic(path, json!({"windows": list}).to_string().as_bytes())
}
