//! The push credentials a crew's payload does not get (crew-role.md, layer D).
//!
//! A crew fetches and commits; its first mate pushes its branches and opens
//! and merges PRs. So the crew's launch environment holds no git credential
//! helper, no gh token or config, and no SSH identity or agent. Fetching over
//! a remote that needs a key fails for a crew too; that is accepted, because
//! the first mate fetches.
use std::path::Path;
use std::process::Command;

/// Git's command-scope config, which `GIT_CONFIG_COUNT` and friends set. Git
/// reads it after every file, so the empty `credential.helper` resets the
/// helper list built from global, repository and URL-scoped entries alike.
/// Checked with real git: a global helper and a `credential.<url>.helper` both
/// stop answering.
const CONFIG_COUNT: &str = "GIT_CONFIG_COUNT";
/// Environment a crew payload must not carry: gh's tokens, the SSH agent and
/// SSH's own askpass.
const CREW_UNSET: [&str; 6] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "SSH_AUTH_SOCK",
    "SSH_ASKPASS",
];
/// Offers no key and no agent, reads no `~/.ssh/config`, and so cannot
/// authenticate to any host.
const CREW_SSH: &str =
    "ssh -o IdentitiesOnly=yes -o IdentityFile=/dev/null -o IdentityAgent=none -F /dev/null";

/// Strips a crew payload's push credentials. `gh_config_dir` is a directory
/// the caller created empty for this one launch; gh reads its config from it
/// and finds no token.
///
/// The git config entries are appended after any the starter already exported,
/// so they still come last and reset its helper list.
pub(crate) fn scrub_push_credentials(cmd: &mut Command, gh_config_dir: &Path) {
    // The count the starter exported; the launch inherits it from this process.
    let base = std::env::var(CONFIG_COUNT)
        .ok()
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0);
    scrub_with_base(cmd, gh_config_dir, base);
}

fn scrub_with_base(cmd: &mut Command, gh_config_dir: &Path, base: usize) {
    cmd.env(CONFIG_COUNT, (base + 1).to_string())
        .env(format!("GIT_CONFIG_KEY_{base}"), "credential.helper")
        .env(format!("GIT_CONFIG_VALUE_{base}"), "")
        .env("GIT_SSH_COMMAND", CREW_SSH)
        // An empty GIT_ASKPASS, unlike an unset one, stops git falling back to
        // core.askPass and SSH_ASKPASS; with no terminal prompt either, git
        // has no way to ask for a credential.
        .env("GIT_ASKPASS", "")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_CONFIG_DIR", gh_config_dir);
    for name in CREW_UNSET {
        cmd.env_remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn env_of(cmd: &Command, name: &str) -> Option<Option<String>> {
        cmd.get_envs()
            .find(|(key, _)| *key == OsStr::new(name))
            .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }

    #[test]
    fn a_crew_payload_gets_no_push_credential() {
        let mut cmd = Command::new("true");
        cmd.env("GH_TOKEN", "t");
        scrub_with_base(&mut cmd, Path::new("/jobs/x/gh"), 0);
        assert_eq!(env_of(&cmd, "GIT_CONFIG_COUNT"), Some(Some("1".into())));
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_KEY_0"),
            Some(Some("credential.helper".into()))
        );
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_VALUE_0"),
            Some(Some(String::new()))
        );
        assert_eq!(
            env_of(&cmd, "GH_CONFIG_DIR"),
            Some(Some("/jobs/x/gh".into()))
        );
        assert_eq!(env_of(&cmd, "GIT_SSH_COMMAND"), Some(Some(CREW_SSH.into())));
        assert_eq!(env_of(&cmd, "GIT_ASKPASS"), Some(Some(String::new())));
        assert_eq!(env_of(&cmd, "GIT_TERMINAL_PROMPT"), Some(Some("0".into())));
        for name in CREW_UNSET {
            assert_eq!(env_of(&cmd, name), Some(None), "{name} is removed");
        }
    }

    /// The starter's entries are kept, and the empty helper comes after them.
    #[test]
    fn the_empty_helper_follows_the_starters_git_config() {
        let mut cmd = Command::new("true");
        scrub_with_base(&mut cmd, Path::new("/jobs/x/gh"), 2);
        assert_eq!(env_of(&cmd, "GIT_CONFIG_COUNT"), Some(Some("3".into())));
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_KEY_2"),
            Some(Some("credential.helper".into()))
        );
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_VALUE_2"),
            Some(Some(String::new()))
        );
        assert_eq!(env_of(&cmd, "GIT_CONFIG_KEY_0"), None);
    }
}
