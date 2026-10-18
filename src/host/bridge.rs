//! One bridge per machine fleet, maintained by host orchestration, never by
//! dashboard observation. The guard serializes homes; the direct dashboard
//! holds a separate kernel lease for its entire lifetime. Pending launch tokens
//! are fenced under the guard before replacement. No time-based death inference,
//! label adoption, input replay, process signalling or pane closing.
use crate::{
    herdr::{Binding, Client, Pane, View},
    store::{Caller, Store},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

const TOKEN: &str = "PRIVATEER_BRIDGE_TOKEN";

#[derive(Clone, Serialize, Deserialize)]
struct Owner {
    home: PathBuf,
    database: (u64, u64),
    directory: (u64, u64),
    origin: String,
    ship: String,
    // The kernel boot that wrote this claim. No lease outlives a reboot, and
    // device numbers need not survive one (btrfs subvolumes, APFS volumes).
    // Absent on claims written before boots were recorded. Kept on the owner,
    // which older binaries read without refusing unknown fields, so a claim
    // stamped here never wedges an older release.
    #[serde(default)]
    boot: Option<String>,
}
impl Owner {
    fn read(store: &Store) -> Result<Self> {
        store.caller().require_operator("hosted bridge")?;
        let ship = crate::presence::folder_ship(store).context("unknown hosting home")?;
        let enabled: bool = store.connection().query_row(
            "SELECT json_extract(record,'$.location')='here' AND json_extract(record,'$.first_mate.host')='herdr' FROM ships WHERE name=?1",
            [&ship], |r| r.get(0))?;
        ensure!(enabled, "hosting is off");
        let origin = store.connection().query_row(
            "SELECT value FROM metadata WHERE key='runtime_origin'",
            [],
            |r| r.get(0),
        )?;
        Ok(Self {
            home: store.home().into(),
            database: identity(&store.home().join("privateer.sqlite"))?,
            directory: {
                let m = fs::metadata(store.home())?;
                (m.dev(), m.ino())
            },
            origin,
            ship,
            boot: boot(),
        })
    }
    fn validate(&self) -> Result<()> {
        // No foreign Store::open/CREATE/migrate/repair, including adoption and
        // manual entry from outside the winning home.
        let store = Store::open_readonly(&self.home, Caller::Operator)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        store
            .connection()
            .progress_handler(1_000, Some(move || std::time::Instant::now() >= deadline));
        let now = Self::read(&store)?;
        ensure!(
            now.database == self.database
                && now.directory == self.directory
                && now.origin == self.origin
                && now.ship == self.ship,
            "bridge hosting home changed"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    registry: (u64, u64),
    guard: (u64, u64),
    live: (u64, u64),
    owner: Owner,
    token: String,
    binding: Option<Binding>,
    // A lost create acknowledgement cannot be repaired from labels. Retain
    // that uncertainty, rather than issuing another create on a timer.
    uncertain: bool,
    adopted: bool,
    failures: i64,
    retry_ms: i64,
}

#[cfg(target_os = "linux")]
fn boot() -> Option<String> {
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(boot.trim().to_owned()).filter(|b| !b.is_empty())
}
#[cfg(target_os = "macos")]
fn boot() -> Option<String> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    let raw = buf.get(..len).filter(|_| rc == 0)?;
    let raw = raw.split(|&b| b == 0).next()?;
    Some(String::from_utf8(raw.to_vec()).ok()?).filter(|b| !b.is_empty())
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn boot() -> Option<String> {
    None
}
/// The same inode under another device number: the filesystem was mounted
/// again, not a different file swapped in under the path (a new inode).
fn renumbered(then: (u64, u64), now: (u64, u64)) -> bool {
    then.0 != now.0 && then.1 == now.1
}

fn identity(path: &Path) -> Result<(u64, u64)> {
    crate::store::reject_symlinks(path)?;
    let m = fs::metadata(path)?;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o022 == 0
            && m.nlink() == 1,
        "bridge state must be an owned, non-writable regular file"
    );
    Ok((m.dev(), m.ino()))
}
fn file(path: &Path, create: bool) -> Result<File> {
    crate::store::reject_symlinks(path)?;
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let m = f.metadata()?;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.nlink() == 1,
        "unsafe bridge coordination file"
    );
    ensure!(
        identity(path)? == (m.dev(), m.ino()),
        "bridge file replaced"
    );
    Ok(f)
}
fn try_lock(f: &File) -> Result<bool> {
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    ensure!(
        e.kind() == std::io::ErrorKind::WouldBlock,
        "bridge lock: {e}"
    );
    Ok(false)
}

struct Coordination {
    dir: PathBuf,
    registry_path: PathBuf,
    registry: (u64, u64),
    _guard: File,
}
impl Coordination {
    fn open(create: bool) -> Result<Self> {
        Self::open_with_wait(create, false)
    }
    fn open_with_wait(create: bool, wait: bool) -> Result<Self> {
        let registry_path = crate::fleet::registry_path()?;
        ensure!(
            registry_path.is_absolute(),
            "fleet registry must be absolute"
        );
        let registry = identity(&registry_path)?;
        let mut name = registry_path
            .file_name()
            .context("fleet filename")?
            .to_os_string();
        name.push(".bridge");
        let dir = registry_path.with_file_name(name);
        crate::store::reject_symlinks(&dir)?;
        if create {
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        let m = fs::metadata(&dir)?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "unsafe bridge coordination directory"
        );
        let guard = file(&dir.join("guard"), create)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !try_lock(&guard)? {
            ensure!(
                wait && std::time::Instant::now() < deadline,
                "bridge reconciliation busy"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let out = Self {
            dir,
            registry_path,
            registry,
            _guard: guard,
        };
        out.validate()?;
        Ok(out)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            identity(&self.registry_path)? == self.registry,
            "fleet registry replaced"
        );
        // Detect directory/guard replacement before any side effect.
        let m = self._guard.metadata()?;
        ensure!(
            identity(&self.dir.join("guard"))? == (m.dev(), m.ino()),
            "bridge guard replaced"
        );
        Ok(())
    }
    fn owner(&self, owner: &Owner) -> Result<()> {
        self.validate()?;
        owner.validate()?;
        let conn = rusqlite::Connection::open_with_flags(
            &self.registry_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        conn.busy_timeout(std::time::Duration::ZERO)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        conn.progress_handler(1_000, Some(move || std::time::Instant::now() >= deadline));
        let active: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND home=?2 AND retired_ms IS NULL)",
            rusqlite::params![owner.ship, owner.home.to_string_lossy()],
            |r| r.get(0),
        )?;
        ensure!(active, "bridge hosting home is not an active fleet member");
        Ok(())
    }
    /// None also for a claim its writer's boot has ended: its lease cannot be
    /// held now, and callers still reclaim only while the live lease is free.
    fn read(&self) -> Result<Option<Claim>> {
        let path = self.dir.join("claim.json");
        if !path.try_exists()? {
            return Ok(None);
        }
        let mut raw = Vec::new();
        file(&path, false)?.take(65_537).read_to_end(&mut raw)?;
        ensure!(raw.len() <= 65_536, "bridge claim exceeds 64 KiB");
        let claim: Claim = serde_json::from_slice(&raw)?;
        ensure!(
            uuid::Uuid::parse_str(&claim.token).is_ok(),
            "bridge claim scope changed"
        );
        if let (Some(then), Some(now)) = (&claim.owner.boot, boot())
            && *then != now
        {
            return Ok(None);
        }
        let guard = identity(&self.dir.join("guard"))?;
        if claim.registry == self.registry && claim.guard == guard {
            return Ok(Some(claim));
        }
        // Within one boot a changed identity is a replaced file. A claim from
        // before boots were recorded proves an earlier boot only when every
        // file kept its inode on one renumbered device.
        ensure!(
            claim.owner.boot.is_none()
                && renumbered(claim.registry, self.registry)
                && renumbered(claim.guard, guard)
                && identity(&self.dir.join("live")).is_ok_and(|live| renumbered(claim.live, live)),
            "bridge claim scope changed"
        );
        Ok(None)
    }
    fn save(&self, claim: &Claim) -> Result<()> {
        self.validate()?;
        ensure!(
            claim.guard == identity(&self.dir.join("guard"))?
                && claim.live == identity(&self.dir.join("live"))?,
            "bridge coordination incarnation changed"
        );
        // One bounded staging file, also after an interrupted durable rename.
        let next = self.dir.join("claim.next");
        let mut f = file(&next, true)?;
        f.set_len(0)?;
        f.write_all(&serde_json::to_vec(claim)?)?;
        f.sync_all()?;
        crate::store::reject_symlinks(&self.dir.join("claim.json"))?;
        fs::rename(next, self.dir.join("claim.json"))?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
    fn live(&self, claim: Option<&Claim>) -> Result<(File, bool)> {
        // A removed/replaced lease PATH is not proof that a dashboard holding
        // the old open inode exited. Never recreate it for an existing claim.
        let f = file(&self.dir.join("live"), claim.is_none())?;
        let m = f.metadata()?;
        ensure!(
            claim.is_none_or(|c| c.live == (m.dev(), m.ino())),
            "bridge lifetime lease incarnation changed"
        );
        let free = try_lock(&f)?;
        Ok((f, free))
    }
}

fn matches(binding: &Binding, view: &View, pane: &Pane) -> bool {
    binding.matches(view, pane)
        && binding.pane.agent == pane.agent
        && binding.pane.agent_session == pane.agent_session
}

/// Called only by a dispatcher with local hosting enabled. All homes in this
/// fleet contend on the same owned guard, regardless of their current home.
/// Returns bounded status, not an audit entry on every idle tick.
pub(crate) fn maintain(store: &Store) -> Result<Value> {
    if !matches!(store.caller(), Caller::Operator) {
        return Ok(Value::Null);
    }
    // Do not even create claim files for hosting-off/unknown local identity or
    // an installation without its own dashboard. PATH wrappers are not hosts.
    let owner = match Owner::read(store) {
        Ok(o) => o,
        Err(_) => return Ok(Value::Null),
    };
    let Some(binary) = crate::cli::hosted_dashboard_binary() else {
        return Ok(Value::Null);
    };
    let coord = Coordination::open(true)?;
    let mut previous = coord.read()?;
    let (_live, free) = coord.live(previous.as_ref())?;
    // A lease held now is held on this boot. Record it on a claim from before
    // boots were recorded, so the next reboot retires the claim, not wedges it.
    if let Some(claim) = previous
        .as_mut()
        .filter(|c| !free && c.adopted && c.owner.boot.is_none())
    {
        claim.owner.boot = boot();
        if claim.owner.boot.is_some() {
            coord.save(claim)?;
        }
    }
    if let Some(claim) = previous.as_ref() {
        if let Some(binding) = &claim.binding {
            let socket = super::socket().context("no Herdr socket configured")?;
            ensure!(socket == binding.socket, "bridge socket scope changed");
            let mut client = Client::connect(&socket)?;
            let view = client.snapshot()?;
            ensure!(
                view.socket_identity == binding.socket_identity,
                "bridge socket incarnation changed"
            );
            if let Some(pane) = view
                .panes
                .iter()
                .find(|p| p.pane_id == binding.pane.pane_id)
            {
                ensure!(matches(binding, &view, pane), "bridge endpoint replaced");
                if !free && claim.adopted {
                    coord.owner(&claim.owner)?;
                    coord.validate()?;
                    // Presentation recovery is host work, not observation.
                    client.tag(&pane.workspace_id, "bridge", &claim.owner.ship)?;
                    client
                        .arrange_with_mates(&crate::herdr::sidebar::mates(store.connection())?)?;
                    return Ok(json!({"status":"live","pane":pane.pane_id}));
                }
                return Ok(json!({"status":"awaiting dashboard or pane exit"}));
            }
            // Even a complete missing-pane snapshot is not child death. A live
            // dashboard keeps custody until its own kernel lease disappears.
            if !free {
                return Ok(json!({"status":"live dashboard; pane missing"}));
            }
            ensure!(
                !view
                    .panes
                    .iter()
                    .any(|p| p.terminal_id == binding.pane.terminal_id
                        || p.workspace_id == binding.pane.workspace_id
                        || p.tab_id == binding.pane.tab_id),
                "bridge location has an unexpected occupant"
            );
        } else {
            ensure!(
                !claim.uncertain,
                "bridge create outcome unknown; retained for recovery"
            );
            ensure!(free, "unbound live bridge lease; retained for recovery");
        }
        if claim.retry_ms > Store::now_ms() {
            return Ok(json!({"status":"backoff","retry_ms":claim.retry_ms}));
        }
    } else {
        ensure!(free, "live bridge without claim; retained for recovery");
    }
    // Recheck local lifecycle while the machine guard and home's write lock
    // are held. No foreign writable store and no provider/harness launch.
    let tx = store.write_transaction()?;
    let owner_now = Owner::read(store)?;
    ensure!(
        owner_now.origin == owner.origin
            && owner_now.database == owner.database
            && owner_now.directory == owner.directory,
        "hosting home changed"
    );
    coord.owner(&owner_now)?;
    let failures = previous
        .take()
        .map_or(1, |c| c.failures.saturating_add(1).clamp(1, 7));
    let mut claim = Claim {
        registry: coord.registry,
        guard: identity(&coord.dir.join("guard"))?,
        live: identity(&coord.dir.join("live"))?,
        owner: owner_now,
        token: uuid::Uuid::new_v4().to_string(),
        binding: None,
        uncertain: false,
        adopted: false,
        failures,
        retry_ms: Store::now_ms() + super::delay(failures),
    };
    // Durable backoff also covers missing/unreachable Herdr, before host I/O.
    // Replacing this token fences delayed dashboard adoption under the guard.
    coord.save(&claim)?;
    let socket = super::socket().context("no Herdr socket configured")?;
    let mut client = Client::connect(&socket)?;
    let observed = client.snapshot()?;
    let socket_identity = observed.socket_identity;
    let token = claim.token.clone();
    let launched = client.bridge_space(
        store.home().parent().context("ship folder")?,
        &binary,
        store.home(),
        &coord.registry_path,
        &token,
        |pane| {
            claim.binding = pane.map(|pane| Binding {
                socket: socket.clone(),
                socket_identity,
                pane: pane.clone(),
            });
            claim.uncertain = pane.is_none();
            coord.save(&claim)
        },
    );
    let pane = match launched {
        Ok(pane) => pane,
        Err(error) => {
            if claim.binding.is_none()
                && error
                    .downcast_ref::<crate::herdr::Refusal>()
                    .is_some_and(|r| r.method == "workspace.create")
            {
                // An explicit rejection is not an unknown/lost acknowledgement.
                claim.uncertain = false;
                coord.save(&claim)?;
            }
            return Err(error);
        }
    };
    // Do not steal focus, including first creation and restart repair.
    let _ = client
        .tag(&pane.workspace_id, "bridge", &claim.owner.ship)
        .and_then(|()| {
            client.arrange_with_mates(&crate::herdr::sidebar::mates(store.connection())?)
        });
    tx.commit()?;
    Ok(json!({"status":"launched","pane":pane.pane_id}))
}

/// Only bare operator entry calls this. Failure is deliberately not label-based
/// adoption: its caller retains ordinary inline dashboard/onboarding fallback.
pub(crate) fn focus(home: Option<&Path>) -> Result<bool> {
    Caller::from_env(None, None)?.require_operator("bridge focus")?;
    if let Some(home) = home {
        Owner::read(&Store::open_readonly(home, Caller::Operator)?)?;
    }
    let coord = Coordination::open(false)?;
    let Some(claim) = coord.read()? else {
        return Ok(false);
    };
    let (_lease, free) = coord.live(Some(&claim))?;
    ensure!(!free && claim.adopted, "bridge has no live dashboard lease");
    coord.owner(&claim.owner)?;
    let binding = claim.binding.context("bridge endpoint unknown")?;
    ensure!(
        super::socket().as_ref() == Some(&binding.socket),
        "bridge socket scope changed"
    );
    coord.validate()?;
    Client::connect(&binding.socket)?.focus_bridge(&binding)?;
    Ok(true)
}

/// A lease held by the DIRECT dashboard process, not a wrapper supervising a
/// child. It is never timed out or explicitly revoked by another dispatcher.
pub struct Lease {
    _file: File,
}

/// Handshake before rendering. Ordinary explicit dashboards have no token and
/// remain entirely read-only. A delayed launch cannot adopt a replaced claim.
pub fn adopt() -> Result<Option<Lease>> {
    Caller::from_env(None, None)?.require_operator("dashboard")?;
    let Ok(token) = std::env::var(TOKEN) else {
        return Ok(None);
    };
    let coord = Coordination::open_with_wait(false, true)?;
    let mut claim = coord.read()?.context("missing bridge launch claim")?;
    ensure!(
        claim.token == token && !claim.adopted,
        "stale bridge launch token"
    );
    let (lease, free) = coord.live(Some(&claim))?;
    ensure!(free, "another dashboard holds the bridge lease");
    coord.owner(&claim.owner)?;
    let binding = Binding::capture()?.context("bridge requires exact Herdr caller identity")?;
    let reserved = claim
        .binding
        .as_ref()
        .context("bridge endpoint not recorded")?;
    ensure!(
        binding.socket == reserved.socket
            && binding.socket_identity == reserved.socket_identity
            && binding.pane.agent.is_none()
            && binding.pane.agent_session == reserved.pane.agent_session
            && binding.pane.pane_id == reserved.pane.pane_id
            && binding.pane.terminal_id == reserved.pane.terminal_id
            && binding.pane.workspace_id == reserved.pane.workspace_id
            && binding.pane.tab_id == reserved.pane.tab_id,
        "bridge caller endpoint changed"
    );
    coord.validate()?;
    claim.binding = Some(binding);
    claim.adopted = true;
    claim.failures = 0;
    coord.save(&claim)?;
    Ok(Some(Lease { _file: lease }))
}

/// Presence of a token requests fleet-only observation. Adoption must still
/// succeed before any dashboard work; this function grants no ownership.
pub fn requested() -> bool {
    std::env::var_os(TOKEN).is_some()
}
