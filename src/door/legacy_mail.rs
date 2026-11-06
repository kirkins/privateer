//! Mailbox selection for trusted v1 adapters. Scoped v2 routing is separate.

use super::{Session, members};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::path::PathBuf;

fn correlated_home<F>(
    members: &[(String, PathBuf)],
    to: &str,
    correlation: &str,
    mut find: F,
) -> Result<Option<PathBuf>>
where
    F: FnMut(&str, &PathBuf, &str) -> Result<Option<Value>>,
{
    let mut found = None;
    for (ship, home) in members {
        let Some(original) = find(ship, home, correlation)? else {
            continue;
        };
        let captain = crate::mail::CAPTAIN;
        if !((original["from"] == to && original["to"] == captain)
            || (original["from"] == captain && original["to"] == to))
        {
            continue;
        }
        ensure!(
            found.as_ref().is_none_or(|previous| previous == home),
            "correlated message {correlation} with {to} is held in multiple mailboxes"
        );
        found = Some(home.clone());
    }
    Ok(found)
}

impl Session<'_> {
    pub(super) fn send_home(&mut self, to: &str, mail: &Value) -> Result<PathBuf> {
        self.legacy_skipped_homes.clear();
        let default = self.home(to)?;
        let Some(correlation) = mail["correlation"].as_str() else {
            return Ok(default);
        };
        let members = members()?;
        // A local ship's own home is authoritative. For a remote ship, its
        // captain conversation may have arrived in any local home, rather
        // than the current liaison's mailbox.
        if members.iter().any(|(ship, _)| ship == to) {
            return Ok(default);
        }
        // An unavailable store cannot establish a match. Search the remaining
        // homes; unreadable mail within an opened store is still an error.
        let mut skipped = Vec::new();
        let found = correlated_home(&members, to, correlation, |ship, home, id| {
            let store = match self.store(home) {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("privateer door: skipped unavailable member home {ship}: {error:#}");
                    skipped.push(ship.to_owned());
                    return Ok(None);
                }
            };
            crate::mail::find(store, id)
        })?;
        self.legacy_skipped_homes = skipped;
        // Preserve existing gateway behavior when no local copy is retained;
        // mail.send still validates the correlation and the answer itself.
        Ok(found.unwrap_or(default))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn unavailable_home_is_skipped_and_ambiguity_is_refused() {
        let members = vec![
            ("broken".into(), PathBuf::from("broken")),
            ("first".into(), PathBuf::from("first")),
            ("second".into(), PathBuf::from("second")),
        ];
        let find = |_ship: &str, home: &PathBuf, _id: &str| -> Result<Option<Value>> {
            if home == Path::new("broken") {
                return Ok(None); // Store::open failure is skipped by the caller.
            }
            if home == Path::new("first") {
                return Ok(Some(json!({"from":"remote", "to":crate::mail::CAPTAIN})));
            }
            Ok(None)
        };
        assert_eq!(
            correlated_home(&members, "remote", "mail-1", find).unwrap(),
            Some(PathBuf::from("first"))
        );

        let ambiguous = |_ship: &str, home: &PathBuf, _id: &str| -> Result<Option<Value>> {
            if home == Path::new("broken") {
                return Ok(None);
            }
            Ok(Some(json!({"from":"remote", "to":crate::mail::CAPTAIN})))
        };
        let error = correlated_home(&members, "remote", "mail-1", ambiguous).unwrap_err();
        assert!(error.to_string().contains("multiple mailboxes"));
    }

    #[test]
    fn store_open_failure_on_a_member_home_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let broken = temp.path().join("broken");
        std::fs::create_dir(&broken).unwrap();
        // Store::open reaches the real database-open path and fails on this
        // malformed database, while the following member has a usable store.
        std::fs::write(broken.join("privateer.sqlite"), b"not a sqlite database").unwrap();
        let usable = temp.path().join("usable");
        let store = crate::store::Store::open(&usable).unwrap();
        crate::mail::init(&store).unwrap();
        let members = vec![
            ("broken-home".into(), broken.clone()),
            ("usable-home".into(), usable.clone()),
        ];
        let mut skipped = Vec::new();
        let found = correlated_home(&members, "remote", "mail-1", |ship, home, id| {
            let store = crate::store::Store::open(home);
            let store = match store {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("privateer door: skipped unavailable member home {ship}: {error:#}");
                    skipped.push(ship.to_owned());
                    return Ok(None);
                }
            };
            if home == &usable {
                assert_eq!(id, "mail-1");
                return Ok(Some(json!({"from":"remote", "to":crate::mail::CAPTAIN})));
            }
            let _ = store;
            Ok(None)
        })
        .unwrap();
        assert_eq!(found, Some(usable));
        assert_eq!(skipped, ["broken-home"]);
    }
}
