//! The captain's credentials for tool bundles: one 0600 file per reference in
//! a 0700 directory beside the fleet registry, outside every home, repository
//! and SQLite file. A secret arrives only on stdin, never in argv or JSON
//! arguments; bundles and launch plans name references, and a bounded launch
//! gets only the path of each of its own selected references' files.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MAX_SECRET: usize = 16 * 1024;

/// A stored reference: lowercase words joined by hyphens, as bundles name it.
fn valid_ref(value: &str) -> bool {
    value.len() <= 40
        && value.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn reference(args: &Value, command: &str) -> Result<String> {
    ensure!(
        args.as_object()
            .is_some_and(|v| v.len() == 1 && v.contains_key("ref")),
        "{command} takes only ref; a secret is read from stdin, never from arguments"
    );
    let name = args["ref"].as_str().context("ref must be a string")?;
    ensure!(
        valid_ref(name),
        "a credential ref is lowercase words joined by hyphens, at most 40 characters"
    );
    Ok(name.to_owned())
}

fn directory() -> Result<PathBuf> {
    let path = crate::fleet::registry_path()?.with_extension("credentials");
    ensure!(
        path.is_absolute(),
        "the credential directory must be absolute"
    );
    Ok(path)
}

/// Bundle-path rules on every component: no symlinks and nothing group or
/// world writable, except a root-owned sticky directory such as /tmp.
fn safe_ancestors(path: &Path) -> Result<()> {
    for part in path.ancestors().skip(1) {
        let info = fs::symlink_metadata(part)?;
        let sticky_root = info.is_dir() && info.mode() & 0o1000 != 0 && info.uid() == 0;
        ensure!(
            !info.file_type().is_symlink() && (info.mode() & 0o022 == 0 || sticky_root),
            "unsafe path to the credential directory; inspect it locally"
        );
    }
    Ok(())
}

/// The credential directory, made 0700 when missing and `create` is set.
/// Refused unless it is a real directory this user owns that no one else can
/// enter.
fn checked(create: bool) -> Result<Option<PathBuf>> {
    let dir = directory()?;
    match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == ErrorKind::NotFound && create => {
            fs::create_dir_all(dir.parent().context("credential directory parent")?)?;
            fs::DirBuilder::new().mode(0o700).create(&dir)?;
            // The umask trims the mode a builder asks for; set it exactly.
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    safe_ancestors(&dir)?;
    let info = fs::symlink_metadata(&dir)?;
    ensure!(
        info.is_dir() && info.uid() == unsafe { libc::geteuid() } && info.mode() & 0o777 == 0o700,
        "the credential directory must be a 0700 directory this user owns; inspect it locally"
    );
    Ok(Some(dir))
}

/// The CLI's entry point: a terminal gets a no-echo prompt for `credential.set`
/// so the secret never reaches scrollback; a pipe is read as it always was.
pub fn cli_from_stdin(command: &str, args: &Value) -> Result<Value> {
    if command == "credential.set" && unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        let line = terminal_line()?;
        return cli(command, args, &mut line.as_slice());
    }
    cli(command, args, &mut io::stdin().lock())
}

/// One line typed at the terminal, read with echo off. The terminal's settings
/// go back on every return path that follows the change.
fn terminal_line() -> Result<Vec<u8>> {
    struct Restore(libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
        }
    }
    let mut before: libc::termios = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut before) } == 0,
        "could not read the terminal settings"
    );
    let mut quiet = before;
    quiet.c_lflag &= !libc::ECHO;
    ensure!(
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet) } == 0,
        "could not hide the terminal input"
    );
    let _restore = Restore(before);
    eprint!("Secret (input hidden): ");
    let mut line = Vec::new();
    let mut stdin = io::stdin().lock();
    let mut byte = [0u8];
    // Bounded: one byte past the limit is enough for `set` to refuse it.
    while line.len() <= MAX_SECRET + 1 && stdin.read(&mut byte)? == 1 {
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    eprintln!();
    Ok(line)
}

fn set(args: &Value, input: &mut dyn Read) -> Result<Value> {
    let name = reference(args, "credential.set")?;
    let mut secret = Vec::new();
    input.take(MAX_SECRET as u64 + 1).read_to_end(&mut secret)?;
    ensure!(secret.len() <= MAX_SECRET, "a credential is at most 16 KiB");
    // One trailing newline is the shell's, not the secret's.
    if secret.ends_with(b"\n") {
        secret.pop();
        if secret.ends_with(b"\r") {
            secret.pop();
        }
    }
    ensure!(
        !secret.is_empty(),
        "credential.set reads the secret from stdin; it was empty"
    );
    let dir = checked(true)?.context("credential directory")?;
    let target = dir.join(&name);
    let replaced = fs::symlink_metadata(&target).is_ok();
    // Write beside it, then rename: a launch sees the old or the new file,
    // never a partial one, and the file is 0600 from its first byte.
    let temp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let written = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&secret)?;
        file.sync_all()?;
        fs::rename(&temp, &target)?;
        fs::File::open(&dir)?.sync_all()?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written?;
    Ok(json!({"ref": name, "stored": true, "replaced": replaced}))
}

fn list() -> Result<Value> {
    let mut refs = vec![];
    if let Some(dir) = checked(false)? {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && valid_ref(name)
                && entry.file_type()?.is_file()
            {
                refs.push(name.to_owned());
            }
        }
    }
    refs.sort();
    Ok(json!({"refs": refs}))
}

fn remove(args: &Value) -> Result<Value> {
    let name = reference(args, "credential.remove")?;
    let Some(dir) = checked(false)? else {
        return Ok(json!({"ref": name, "removed": false}));
    };
    let removed = match fs::remove_file(dir.join(&name)) {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    Ok(json!({"ref": name, "removed": removed}))
}

/// `credential.set {"ref"}` with the secret on stdin, `credential.list {}`
/// (refs only) and `credential.remove {"ref"}`: the captain's alone, from
/// the CLI. The caller has already been checked as the operator.
pub fn cli(command: &str, args: &Value, input: &mut dyn Read) -> Result<Value> {
    match command {
        "credential.set" => set(args, input),
        "credential.list" => {
            ensure!(
                args.as_object().is_some_and(|v| v.is_empty()),
                "credential.list takes no arguments"
            );
            list()
        }
        "credential.remove" => remove(args),
        _ => bail!("unknown command {command}"),
    }
}
