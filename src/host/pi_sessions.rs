//! Where a hosted Pi first mate's sessions are, found the way Pi 1.3 finds
//! them, so a pane can tell its own session from the others in the folder.
//!
//! Errands, wakes and the captain's own `pi` runs in the ship folder save
//! sessions there too, and Pi's `--continue` resumes the newest whoever wrote
//! it, on whatever model it ran. So a pane names its own session instead:
//! it gives Pi an id (`--session-id`, which opens that session or creates it)
//! and records it, and continues only that id. A session counts as written
//! once a file whose header carries that id exists.
//!
//! Pi's session directory is, in order: a `--session-dir` the session passes,
//! `PI_CODING_AGENT_SESSION_DIR`, a `sessionDir` setting (the folder's
//! `.pi/settings.json` over the agent directory's `settings.json`), else the
//! agent directory's `sessions/--<path, dashed>--`. In any directory but that
//! default one, Pi keeps only sessions whose header names the working folder.
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

/// How much of a session file is read for its header line.
const HEADER_SCAN_BYTES: u64 = 64 * 1024;

/// A ship folder's Pi sessions, where Pi keeps them for a launch.
#[derive(Debug, PartialEq)]
pub(crate) struct PiSessions {
    dir: PathBuf,
    /// The working folder a session's header must name, in a directory other
    /// than the folder's default one: the folder as Pi sees it (resolved) and
    /// as given.
    cwd: Option<[PathBuf; 2]>,
}

/// A session's header: the first record, `{"type":"session",...}`.
struct Header {
    id: String,
    cwd: Option<String>,
}

impl PiSessions {
    /// Where a launch in `folder`, passing `passthrough` to Pi, keeps its
    /// sessions, from this process's environment.
    pub(crate) fn of(folder: &Path, passthrough: &[String]) -> Option<Self> {
        Self::resolve(folder, passthrough, &|name| {
            std::env::var_os(name).filter(|v| !v.is_empty())
        })
    }

    pub(crate) fn resolve(
        folder: &Path,
        passthrough: &[String],
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Option<Self> {
        let canonical = folder
            .canonicalize()
            .unwrap_or_else(|_| folder.to_path_buf());
        let home = env("HOME").map(PathBuf::from);
        // `~` and `~/...` name the home directory; a relative path is Pi's
        // working folder's.
        let path = |value: &str| -> Option<PathBuf> {
            let path = match value.strip_prefix('~') {
                Some("") => home.clone()?,
                Some(rest) if rest.starts_with('/') => home.clone()?.join(&rest[1..]),
                _ => PathBuf::from(value),
            };
            Some(canonical.join(path))
        };
        let agent = match env("PI_CODING_AGENT_DIR") {
            Some(dir) => path(&dir.to_string_lossy())?,
            None => home.clone()?.join(".pi/agent"),
        };
        let cwd = canonical.to_string_lossy().into_owned();
        let default = agent.join("sessions").join(format!(
            "--{}--",
            cwd.strip_prefix(['/', '\\'])
                .unwrap_or(&cwd)
                .replace(['/', '\\', ':'], "-")
        ));
        let configured = passed(passthrough, "--session-dir")
            .or_else(|| {
                env("PI_CODING_AGENT_SESSION_DIR").map(|v| v.to_string_lossy().into_owned())
            })
            .or_else(|| {
                [
                    canonical.join(".pi/settings.json"),
                    agent.join("settings.json"),
                ]
                .iter()
                .find_map(|file| setting(file, "sessionDir"))
            });
        Some(match configured.and_then(|dir| path(&dir)) {
            Some(dir) if dir != default => Self {
                dir,
                cwd: Some([canonical, folder.to_path_buf()]),
            },
            _ => Self {
                dir: default,
                cwd: None,
            },
        })
    }

    /// The folder's sessions, with their headers, as Pi lists them.
    fn sessions(&self) -> Vec<(PathBuf, Header)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return vec![];
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|path| path.extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|path| Some((header(&path)?, path)))
            .filter(|(header, _)| match &self.cwd {
                None => true,
                Some(cwds) => header
                    .cwd
                    .as_deref()
                    .is_some_and(|cwd| cwds.iter().any(|c| Path::new(cwd) == c)),
            })
            .map(|(header, path)| (path, header))
            .collect()
    }

    /// Whether the folder has a session with this id, as `--session-id`
    /// opens it.
    pub(crate) fn has(&self, id: &str) -> bool {
        self.sessions().iter().any(|(_, header)| header.id == id)
    }

    /// The id of the session `--continue` resumes, the most recently written,
    /// other than those in `skip`; only an id `--session-id` accepts.
    pub(crate) fn newest(&self, skip: &[String]) -> Option<String> {
        self.sessions()
            .into_iter()
            .filter(|(_, header)| !skip.contains(&header.id) && valid_id(&header.id))
            .filter_map(|(path, header)| {
                Some((std::fs::metadata(path).ok()?.modified().ok()?, header.id))
            })
            .max_by_key(|(at, _)| *at)
            .map(|(_, id)| id)
    }

    /// Whether any session was written at or after `since_ms`, for a fresh
    /// launch recorded before panes named their sessions.
    pub(crate) fn written_since(&self, since_ms: i64) -> bool {
        let since = u128::try_from(since_ms).unwrap_or(0);
        self.sessions().iter().any(|(path, _)| {
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|at| at.as_millis() >= since)
        })
    }
}

/// A new session id for a pane to give Pi.
pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Pi accepts letters, digits, `.`, `_` and `-`, starting and ending with a
/// letter or digit.
fn valid_id(id: &str) -> bool {
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    edge(id.chars().next())
        && edge(id.chars().last())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The last value a flag is given, as `flag value` or `flag=value`.
fn passed(passthrough: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut value = None;
    let mut args = passthrough.iter();
    while let Some(arg) = args.next() {
        if arg == flag {
            value = args.next().cloned();
        } else if let Some(given) = arg.strip_prefix(&prefix) {
            value = Some(given.to_owned());
        }
    }
    value.filter(|v| !v.is_empty())
}

/// A non-empty string setting from a settings file.
fn setting(file: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let settings: serde_json::Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    settings[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// A session file's header: its first record that parses, when that is a
/// `session` record with an id.
fn header(path: &Path) -> Option<Header> {
    let mut reader = BufReader::new(std::fs::File::open(path).ok()?.take(HEADER_SCAN_BYTES));
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            return None;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record: serde_json::Value = serde_json::from_slice(&line).ok()?;
        if record["type"] != "session" {
            return None;
        }
        return Some(Header {
            id: record["id"].as_str()?.to_owned(),
            cwd: record["cwd"].as_str().map(str::to_owned),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn write(dir: &Path, name: &str, id: &str, cwd: Option<&Path>) {
        std::fs::create_dir_all(dir).unwrap();
        let mut header = serde_json::json!({"type":"session","version":3,"id":id});
        if let Some(cwd) = cwd {
            header["cwd"] = serde_json::json!(cwd);
        }
        std::fs::write(dir.join(name), format!("{header}\n")).unwrap();
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        folder: PathBuf,
        env: BTreeMap<&'static str, String>,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let folder = root.join("ship");
            std::fs::create_dir(&folder).unwrap();
            let env = BTreeMap::from([("HOME", root.display().to_string())]);
            Self {
                _temp: temp,
                root,
                folder,
                env,
            }
        }

        fn sessions(&self, passthrough: &[&str]) -> PiSessions {
            let passthrough: Vec<String> = passthrough.iter().map(|&a| a.to_owned()).collect();
            PiSessions::resolve(&self.folder, &passthrough, &|name| {
                self.env.get(name).map(OsString::from)
            })
            .unwrap()
        }

        fn default_dir(&self) -> PathBuf {
            let cwd = self.folder.to_string_lossy();
            self.root.join(".pi/agent/sessions").join(format!(
                "--{}--",
                cwd.trim_start_matches('/').replace(['/', ':'], "-")
            ))
        }
    }

    #[test]
    fn the_default_directory_is_the_folders_own() {
        let fixture = Fixture::new();
        let sessions = fixture.sessions(&[]);
        assert_eq!(
            sessions,
            PiSessions {
                dir: fixture.default_dir(),
                cwd: None
            }
        );
        // The directory is the folder's, so a header without a working folder
        // still counts there, as Pi counts it.
        write(&fixture.default_dir(), "a.jsonl", "own", None);
        assert!(sessions.has("own"));
        assert!(!sessions.has("other"));
    }

    #[test]
    fn a_shared_directory_keeps_only_the_folders_sessions() {
        let mut fixture = Fixture::new();
        let shared = fixture.root.join("shared");
        fixture
            .env
            .insert("PI_CODING_AGENT_SESSION_DIR", shared.display().to_string());
        let sessions = fixture.sessions(&[]);
        assert_eq!(sessions.dir, shared);
        let other = fixture.root.join("other");
        write(&shared, "1.jsonl", "mine", Some(&fixture.folder));
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&shared, "2.jsonl", "theirs", Some(&other));
        write(&shared, "3.jsonl", "nowhere", None);
        assert!(sessions.has("mine"));
        assert!(!sessions.has("theirs") && !sessions.has("nowhere"));
        // Another folder's newer session is not the one --continue resumes.
        assert_eq!(sessions.newest(&[]).as_deref(), Some("mine"));
        assert_eq!(sessions.newest(&["mine".to_owned()]), None);
        // Nor does it count as written.
        let later = crate::store::Store::now_ms() + 60_000;
        assert!(!sessions.written_since(later));
    }

    #[test]
    fn the_session_dir_setting_and_flag_move_the_sessions() {
        let mut fixture = Fixture::new();
        let agent = fixture.root.join(".pi/agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("settings.json"),
            r#"{"sessionDir":"~/global-sessions"}"#,
        )
        .unwrap();
        assert_eq!(
            fixture.sessions(&[]).dir,
            fixture.root.join("global-sessions")
        );
        // The folder's own settings come first, relative to the folder.
        std::fs::create_dir(fixture.folder.join(".pi")).unwrap();
        std::fs::write(
            fixture.folder.join(".pi/settings.json"),
            r#"{"sessionDir":"local-sessions"}"#,
        )
        .unwrap();
        assert_eq!(
            fixture.sessions(&[]).dir,
            fixture.folder.join("local-sessions")
        );
        // The environment over the settings, and the flag over both.
        fixture.env.insert(
            "PI_CODING_AGENT_SESSION_DIR",
            fixture.root.join("env").display().to_string(),
        );
        assert_eq!(fixture.sessions(&[]).dir, fixture.root.join("env"));
        let flag = fixture.root.join("flag");
        let flag = flag.to_str().unwrap();
        assert_eq!(
            fixture.sessions(&["--session-dir", flag]).dir,
            Path::new(flag)
        );
        assert_eq!(
            fixture.sessions(&[&format!("--session-dir={flag}")]).dir,
            Path::new(flag)
        );
        // A configured directory that is the default one is the default one.
        let default = fixture.default_dir();
        assert_eq!(
            fixture.sessions(&["--session-dir", default.to_str().unwrap()]),
            PiSessions {
                dir: default,
                cwd: None
            }
        );
        // PI_CODING_AGENT_DIR moves the agent directory and its settings.
        fixture.env.remove("PI_CODING_AGENT_SESSION_DIR");
        std::fs::remove_file(fixture.folder.join(".pi/settings.json")).unwrap();
        fixture
            .env
            .insert("PI_CODING_AGENT_DIR", "~/agent-two".to_owned());
        assert_eq!(
            fixture.sessions(&[]).dir.parent().unwrap(),
            fixture.root.join("agent-two/sessions")
        );
    }

    #[test]
    fn only_ids_pi_accepts_are_continued() {
        for id in ["01a15b92-aa2a-70a5-b0f2-5c7048260116", "a", "a.b_c-d"] {
            assert!(valid_id(id), "{id}");
        }
        for id in ["", "-a", "a-", "a b", "a/b", "../x"] {
            assert!(!valid_id(id), "{id}");
        }
    }
}
