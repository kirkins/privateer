//! Herdr saves its spaces and restores them when its server starts again, as
//! after a reboot: the same workspace and pane ids, but a new terminal holding
//! a bare shell where Privateer's `exec privateer ...` ran. Privateer then
//! opens a fresh space for the same first mate, crew or bridge, so each
//! restart left a dead copy of every space for the captain to close by hand.
//!
//! Each space Privateer opens is recorded here with its terminal. On a pass
//! where Herdr answers, a recorded pane that Herdr still lists under a new
//! terminal is a restored copy: its space is closed if nothing else moved in
//! (the space's only pane, at a bare shell, still in the folder it was opened
//! in); otherwise the captain has made it theirs and it is left alone. Either
//! way the record is dropped, as it is for a space that is gone. Ownership
//! comes from this record alone, never from labels or metadata tokens.
use crate::herdr::{Client, Pane};
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use serde_json::{Value, json};
use std::path::Path;

pub(crate) fn init(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS herdr_spaces(socket TEXT NOT NULL, workspace_id TEXT NOT NULL,
           pane_id TEXT NOT NULL, terminal_id TEXT NOT NULL, role TEXT NOT NULL, ship TEXT NOT NULL,
           folder TEXT NOT NULL, recorded_ms INTEGER NOT NULL, PRIMARY KEY(socket, workspace_id));",
    )?;
    Ok(())
}

/// Record a space this home just opened. Best effort: an unrecorded space
/// still runs, and is only left for the captain after a restart.
pub(crate) fn record(
    store: &Store,
    socket: &Path,
    pane: &Pane,
    role: &str,
    ship: &str,
    folder: &Path,
) {
    let recorded = (|| -> Result<()> {
        let tx = store.write_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO herdr_spaces(socket,workspace_id,pane_id,terminal_id,role,ship,folder,recorded_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![socket.to_string_lossy(), pane.workspace_id, pane.pane_id, pane.terminal_id,
                role, ship, folder.to_string_lossy(), Store::now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(error) = recorded {
        eprintln!(
            "privateer: could not record Herdr space {}: {error:#}",
            pane.workspace_id
        );
    }
}

struct Space {
    workspace: String,
    pane: String,
    terminal: String,
    role: String,
    ship: String,
    folder: String,
}

/// Close the restored copies of this home's spaces. Best effort and quiet
/// while Herdr is unreachable; returns what it closed or left.
pub(crate) fn sweep(store: &Store) -> Vec<Value> {
    sweep_spaces(store).unwrap_or_else(|error| vec![json!({"error": format!("{error:#}")})])
}

fn sweep_spaces(store: &Store) -> Result<Vec<Value>> {
    let Some(socket) = super::socket() else {
        return Ok(vec![]);
    };
    let spaces = {
        let mut statement = store.connection().prepare(
            "SELECT workspace_id,pane_id,terminal_id,role,ship,folder FROM herdr_spaces
             WHERE socket=?1 ORDER BY recorded_ms",
        )?;
        statement
            .query_map([socket.to_string_lossy()], |r| {
                Ok(Space {
                    workspace: r.get(0)?,
                    pane: r.get(1)?,
                    terminal: r.get(2)?,
                    role: r.get(3)?,
                    ship: r.get(4)?,
                    folder: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if spaces.is_empty() {
        return Ok(vec![]);
    }
    let Ok(mut client) = Client::connect(&socket) else {
        return Ok(vec![]);
    };
    let Ok(view) = client.snapshot() else {
        return Ok(vec![]);
    };
    let mut out = vec![];
    for space in spaces {
        let listed = view
            .panes
            .iter()
            .find(|p| p.pane_id == space.pane && p.workspace_id == space.workspace);
        if listed.is_some_and(|p| p.terminal_id == space.terminal) {
            continue;
        }
        if listed.is_some() {
            let alone = view
                .panes
                .iter()
                .filter(|p| p.workspace_id == space.workspace)
                .count()
                == 1;
            // A failed read is temporary: keep the record for the next pass.
            let unused = match alone.then(|| client.process_info(&space.pane)) {
                Some(Err(_)) => continue,
                Some(Ok(info)) => bare_shell_in(&info, Path::new(&space.folder)),
                None => false,
            };
            let kind = if unused {
                // A failed close is temporary too: keep the record for the next pass.
                if client.close_workspace(&space.workspace).is_err() {
                    continue;
                }
                "host.restored_closed"
            } else {
                "host.restored_left"
            };
            let entry = json!({"workspace": space.workspace, "pane": space.pane,
                "role": space.role, "ship": space.ship, "closed": unused});
            store.audit(kind, &entry)?;
            out.push(entry);
        }
        let tx = store.write_transaction()?;
        tx.execute(
            "DELETE FROM herdr_spaces WHERE socket=?1 AND workspace_id=?2 AND pane_id=?3 AND terminal_id=?4",
            params![socket.to_string_lossy(), space.workspace, space.pane, space.terminal],
        )?;
        tx.commit()?;
    }
    Ok(out)
}

const SHELLS: [&str; 6] = ["sh", "bash", "zsh", "fish", "dash", "ksh"];
const HARNESSES: [&str; 5] = ["claude", "pi", "codex", "opencode", "agy"];

/// Nothing runs in the pane but its shell, and the shell is where the space
/// was opened: no one has used the restored copy. A `privateer` process or a
/// harness is never a bare shell, even at the shell's pid: Privateer starts
/// its panes with `exec`, which replaces the shell.
fn bare_shell_in(info: &Value, folder: &Path) -> bool {
    let Some(shell) = info["shell_pid"].as_u64() else {
        return false;
    };
    let login = std::env::var("SHELL").ok();
    let login = login
        .as_deref()
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str());
    let processes = info["foreground_processes"].as_array();
    info["foreground_process_group_id"].as_u64() == Some(shell)
        && processes.is_some_and(|all| {
            !all.is_empty()
                && all.iter().all(|p| {
                    let names = process_names(p);
                    p["pid"].as_u64() == Some(shell)
                        && names.iter().any(|name| {
                            SHELLS.contains(name) || login.is_some_and(|login| login == *name)
                        })
                        && !names
                            .iter()
                            .any(|name| name.starts_with("privateer") || HARNESSES.contains(name))
                })
                && all.iter().all(|p| {
                    p["cwd"]
                        .as_str()
                        .is_some_and(|cwd| same_folder(Path::new(cwd), folder))
                })
        })
}

/// A process's name and argv[0], by base name, without a login shell's dash.
fn process_names(process: &Value) -> Vec<&str> {
    [process["name"].as_str(), process["argv"][0].as_str()]
        .into_iter()
        .flatten()
        .filter_map(|name| Path::new(name.trim_start_matches('-')).file_name())
        .filter_map(|name| name.to_str())
        .collect()
}

fn same_folder(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}
