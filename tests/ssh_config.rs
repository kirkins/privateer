//! Real OpenSSH parsing only: no network, sshd, keys, or writes to /etc.
#![cfg(target_os = "linux")]
mod support;

use serde_json::{Value, json};
use std::ffi::CStr;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Output;

fn successful(output: Output) -> Output {
    assert!(
        output.status.success(),
        "status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn real_openssh_default_include_rejected_in_namespace_but_per_link_config_works() {
    let root = support::tempdir();
    let root = root.path();
    let located = support::command("/bin/sh")
        .args(["-c", "command -v ssh"])
        .output()
        .unwrap();
    if !located.status.success() {
        assert_ne!(
            std::env::var("PRIVATEER_SANDBOX_QUALIFICATION").as_deref(),
            Ok("1"),
            "qualification requires OpenSSH"
        );
        eprintln!("skipped: OpenSSH ssh is unavailable on PATH");
        return;
    }
    let ssh = PathBuf::from(String::from_utf8(located.stdout).unwrap().trim());
    let version = successful(support::command(&ssh).arg("-V").output().unwrap());
    assert!(String::from_utf8_lossy(&version.stderr).contains("OpenSSH"));
    let Some(mut prefix) = support::sandbox_prefix(root) else {
        return;
    };

    // Bind an existing root-owned regular OpenSSH config onto a synthetic
    // include, then hide the real system config. The host file is only read.
    // Unlike a copy, the read-only bind retains the root ownership that the
    // namespace remaps. The host-side parse also qualifies its real content.
    let system = Path::new("/etc/ssh/ssh_config");
    let metadata = fs::metadata(system).expect("OpenSSH system config fixture source");
    assert!(metadata.is_file());
    assert_eq!(metadata.uid(), 0);
    let include = root.join("beta.include");
    fs::copy(system, &include).unwrap();
    let user = root.join("user");
    fs::create_dir_all(user.join(".ssh")).unwrap();
    fs::set_permissions(&user, fs::Permissions::from_mode(0o700)).unwrap();
    let default_config = user.join(".ssh/config");
    fs::write(&default_config, format!("Include {}\n", include.display())).unwrap();
    let explicit = root.join("alpha.conf");
    // Privateer's command-line strict setting must win even over this file.
    fs::write(&explicit, "Host *\n    StrictHostKeyChecking no\n").unwrap();
    fs::set_permissions(&explicit, fs::Permissions::from_mode(0o600)).unwrap();
    successful(
        support::command(&ssh)
            .args(["-G", "-F"])
            .arg(&default_config)
            .arg("beta")
            .output()
            .unwrap(),
    );
    successful(
        support::command(&ssh)
            .args(["-G", "-F"])
            .arg(system)
            .arg("beta")
            .output()
            .unwrap(),
    );

    let passwd_home = unsafe {
        let entry = libc::getpwuid(libc::getuid());
        assert!(!entry.is_null());
        PathBuf::from(CStr::from_ptr((*entry).pw_dir).to_str().unwrap())
    };
    assert!(passwd_home.is_absolute());
    assert!(
        !root.starts_with(&passwd_home),
        "fixture must survive masking passwd home"
    );
    let empty_system = root.join("empty-system");
    fs::write(&empty_system, "").unwrap();
    let runtime = root.join("privateer");
    fs::write(&runtime, "").unwrap();
    for (source, target) in [
        (system, include.as_path()),
        (Path::new(support::BIN), runtime.as_path()),
        (empty_system.as_path(), system),
        (user.as_path(), passwd_home.as_path()),
    ] {
        prefix.extend(["--ro-bind".into(), source.into(), target.into()]);
    }
    let sandbox = || {
        let mut command = support::command(&prefix[0]);
        command
            .args(&prefix[1..])
            .env("HOME", &passwd_home)
            .env("PRIVATEER_FLEET", root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", root.join("ledger"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"));
        command
    };
    let ownership = successful(
        sandbox()
            .arg("/usr/bin/stat")
            .args(["-c", "%u"])
            .arg(&include)
            .output()
            .unwrap(),
    );
    let uid: u32 = String::from_utf8(ownership.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(uid, 0);
    assert_ne!(uid, unsafe { libc::getuid() });
    let rejected = sandbox().arg(&ssh).args(["-G", "beta"]).output().unwrap();
    assert_eq!(rejected.status.code(), Some(255));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains(&format!(
        "Bad owner or permissions on {}",
        include.display()
    )));

    // Exercise Privateer's actual -F argv and error classification, with a
    // shim that only adds -G. A successful parse returns a protocol envelope;
    // the complete OpenSSH effective config is retained for assertions.
    let shim = root.join("ssh-parse");
    let dump = root.join("effective-config");
    fs::write(&shim, format!("#!/bin/sh\n'{}' -G \"$@\" > '{}'\nstatus=$?\n[ \"$status\" = 0 ] || exit \"$status\"\nread -r request\nprintf '%s\\n' '{{\"id\":\"alpha\",\"ok\":true,\"result\":{{}}}}'\n", ssh.display(), dump.display())).unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.join("ship");
    let mut endpoint = json!({"name":"beta", "host":"beta", "executable":"/alpha/privateer", "home":"/alpha/home"});
    let configure = |endpoint: &Value| {
        successful(
            support::isolated(root)
                .args(["--home"])
                .arg(&home)
                .args(["peer.put", "--json", &endpoint.to_string()])
                .output()
                .unwrap(),
        );
    };
    let call = || {
        sandbox()
            .arg(&runtime)
            .arg("--home")
            .arg(&home)
            .args([
                "peer.call",
                "--json",
                &json!({"name":"beta","command":"status","id":"alpha"}).to_string(),
            ])
            .env("PRIVATEER_SSH_EXECUTABLE", &shim)
            .output()
            .unwrap()
    };
    configure(&endpoint);
    let rejected = call();
    assert!(!rejected.status.success());
    let diagnostic = String::from_utf8_lossy(&rejected.stderr);
    for expected in [include.to_str().unwrap(), "ssh_config", "job.start"] {
        assert!(diagnostic.contains(expected), "{diagnostic}");
    }
    endpoint["ssh_config"] = json!(explicit);
    configure(&endpoint);
    successful(call());
    let effective = fs::read_to_string(dump).unwrap();
    assert!(effective.lines().any(|line| line == "stricthostkeychecking true" || line == "stricthostkeychecking yes"), "{effective}");
    assert!(effective.lines().any(|line| line == "batchmode yes"));
    assert!(effective.lines().any(|line| line == "connecttimeout 10"));
}
