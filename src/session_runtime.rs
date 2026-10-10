//! Instance-owned tmux execution and non-secret creation outcomes.
//!
//! Linux agents run under a persistent subreaper. Directory descriptors pin the
//! checkout objects across daemon restarts, and the runner owns all descendants.
//! Platforms without verified descendant containment fail closed at creation.

use crate::{Result, checkout, config::ServerConfig, sessions::*};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const OPERATION_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(target_os = "linux")]
const MAX_RECORDS: usize = 4096;
const MAX_LIVE: usize = 64;

#[cfg(target_os = "linux")]
fn supported_directory_fs(magic: u32) -> bool {
    // Only local directory-inode lifetimes covered by descriptor pinning.
    // NFS/FUSE/9p may recycle server-side inode identities despite an open FD.
    // ext2/3/4, XFS, Btrfs, tmpfs, and OverlayFS directory identities.
    matches!(
        magic,
        0xef53 | 0x58465342 | 0x9123683e | 0x01021994 | 0x794c7630
    )
}

#[cfg(target_os = "linux")]
fn directory_identity_supported(file: &std::fs::File) -> Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: statfs is a plain C output structure; fstatfs receives a live FD
    // and valid writable storage. Only read the result after successful return.
    let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut filesystem) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(supported_directory_fs(filesystem.f_type as u32))
}

#[cfg(target_os = "linux")]
fn validate_creation_filesystems(
    checkout: &checkout::Checkout,
) -> std::result::Result<(), Failure> {
    use std::os::unix::fs::OpenOptionsExt;
    for observed in [
        &checkout.root,
        &checkout.git_directory,
        &checkout.common_directory,
    ] {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&observed.path)
            .map_err(|_| Failure::CheckoutUnavailable)?;
        if !directory_identity_supported(&file).map_err(|_| Failure::CheckoutUnavailable)? {
            return Err(Failure::UnsupportedFilesystem);
        }
    }
    Ok(())
}

/// Internal discovery result; errors are distinct from an empty list.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub error: Option<String>,
    pub supported: bool,
    /// Verified local integration descriptors; never serialized remotely.
    #[cfg(target_os = "linux")]
    pub integrations: Vec<(SessionId, crate::agent_integration::Metadata)>,
    /// Projected from the independent subscriber cache at read time.
    pub activity: Vec<(SessionId, crate::agent_activity::Status)>,
}

/// Public session summary without local instance/attachment or launch secrets.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub id: SessionId,
    pub checkout: PathBuf,
    pub label: String,
    pub state: State,
    #[serde(
        default,
        skip_serializing_if = "crate::agent_activity::Activity::is_unknown"
    )]
    pub activity: crate::agent_activity::Activity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi_session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi_session_id: Option<String>,
}

impl Summary {
    /// Current Pi name or the original Wumpa label for unnamed/unsupported agents.
    pub fn display_name(&self) -> &str {
        self.pi_session_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.label)
    }

    /// Process failures/transitions remain authoritative; only Running uses Pi activity.
    pub fn display_state(&self) -> &'static str {
        use crate::agent_activity::Activity;
        match self.state {
            State::Starting => "Starting",
            State::Stopping => "Stopping",
            State::CleanupFailed => "Cleanup failed",
            State::Running => match self.activity {
                Activity::Working => "Running",
                Activity::WaitingForInput => "Waiting for input",
                Activity::Unknown => "Unknown",
            },
        }
    }
}

/// Backward-compatible optional remote snapshot, separate from local responses.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemoteSnapshot {
    pub sessions: Vec<Summary>,
    pub error: Option<String>,
    pub supported: bool,
}

impl Snapshot {
    pub fn remote(self) -> RemoteSnapshot {
        let mut snapshot = RemoteSnapshot {
            supported: self.supported,
            error: self.error,
            sessions: self
                .sessions
                .into_iter()
                .map(|session| Summary {
                    id: session.id.clone(),
                    checkout: session.checkout.root.path,
                    label: session.label,
                    state: session.state,
                    activity: self
                        .activity
                        .iter()
                        .find(|(id, _)| *id == session.id)
                        .map(|(_, status)| status.activity)
                        .unwrap_or_default(),
                    pi_session_name: self
                        .activity
                        .iter()
                        .find(|(id, _)| *id == session.id)
                        .and_then(|(_, status)| status.pi_session_name.clone()),
                    pi_session_id: self
                        .activity
                        .iter()
                        .find(|(id, _)| *id == session.id)
                        .and_then(|(_, status)| status.pi_session_id.clone()),
                })
                .collect(),
        };
        snapshot.limit();
        snapshot
    }
}

impl RemoteSnapshot {
    /// Display-only shortest unique prefixes, at least five hex characters.
    /// Compare all distinct IDs on this server, including other checkouts.
    /// Full identities remain unchanged in summaries and attachment requests.
    pub fn display_ids(&self) -> std::collections::BTreeMap<String, String> {
        let mut ids = self
            .sessions
            .iter()
            .map(|session| String::from(session.id.clone()))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        let mut labels = std::collections::BTreeMap::new();
        for (index, id) in ids.iter().enumerate() {
            let mut length = 5;
            // In sorted order, only the adjacent IDs can share the longest prefix.
            for neighbor in index
                .checked_sub(1)
                .and_then(|previous| ids.get(previous))
                .into_iter()
                .chain(ids.get(index + 1))
            {
                let shared = id
                    .bytes()
                    .zip(neighbor.bytes())
                    .take_while(|(left, right)| left == right)
                    .count();
                length = length.max(shared + 1);
            }
            // SessionId validates exactly 32 ASCII hex characters.
            labels.insert(id.clone(), id[..length.min(id.len())].to_owned());
        }
        labels
    }

    /// Transport/discovery loss never implies idle; keep explicitly cached names.
    pub fn invalidate_activity(&mut self) {
        for session in &mut self.sessions {
            session.activity = crate::agent_activity::Activity::Unknown;
        }
    }

    /// Preserve a previously known name only when a server has no verified Pi
    /// identity yet. A snapshot with a Pi identity and no name is an actual clear.
    pub fn retain_cached_names(&mut self, previous: &Self) {
        for session in &mut self.sessions {
            if session.activity.is_unknown()
                && session.pi_session_id.is_none()
                && session.pi_session_name.is_none()
            {
                if let Some(old) = previous.sessions.iter().find(|old| old.id == session.id) {
                    session.pi_session_name = old.pi_session_name.clone();
                    session.pi_session_id = old.pi_session_id.clone();
                }
            }
        }
    }

    /// Agent metadata must not consume the repository browsing response budget.
    pub fn limit(&mut self) {
        if serde_json::to_vec(self).map_or(true, |bytes| bytes.len() > 128 * 1024) {
            self.sessions.clear();
            self.error = Some("Agent snapshot exceeds its discovery size budget".into());
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    creation: CreationRecord,
    session: Session,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pi: Option<crate::agent_integration::Metadata>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeIdentity {
    instance: PathBuf,
    device: u64,
    inode: u64,
}

/// Only the private one-shot launch channel carries secrets.
#[cfg(target_os = "linux")]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    instance: PathBuf,
    session_id: SessionId,
    checkout: checkout::Checkout,
    command: crate::config::AgentCommand,
    integration: crate::config::AgentIntegration,
    label: String,
    environment: crate::session_environment::Environment,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum RunnerRequest {
    Status { session_id: SessionId },
    Stop { session_id: SessionId },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerStatus {
    instance: PathBuf,
    session_id: SessionId,
    checkout: CheckoutAssociation,
    state: State,
    stopped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pi: Option<crate::agent_integration::Metadata>,
}

/// Generate opaque IDs with OS randomness, without a dependency or time-based IDs.
pub fn random_id() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn id_text(id: &SessionId) -> String {
    id.clone().into()
}
fn request_text(id: &CreationId) -> String {
    id.clone().into()
}
fn backend_name(id: &SessionId) -> String {
    format!("wumpa-{}", id_text(id))
}

/// Stable tmux location derived from the canonical control socket path.
/// A too-long Unix path fails explicitly; there is no alternate-instance fallback.
pub fn tmux_socket(instance: &Path) -> Result<PathBuf> {
    let name = instance
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid instance path")?;
    Ok(instance
        .with_file_name(format!(".{name}.sessions"))
        .join("tmux.sock"))
}

/// One manager per canonical control endpoint; creation/reconciliation serialize.
pub struct Manager {
    instance: PathBuf,
    directory: PathBuf,
    socket: PathBuf,
    backend_supported: bool,
    #[cfg(unix)]
    directory_anchor: std::fs::File,
}

impl Manager {
    /// Stable adjacent runtime directory, verified instead of following symlinks.
    pub fn new(instance: &Path) -> Result<Self> {
        let name = instance
            .file_name()
            .ok_or("instance has no file name")?
            .to_str()
            .ok_or("instance path is not UTF-8")?;
        let directory = instance.with_file_name(format!(".{name}.sessions"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, MetadataExt};
            match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            let metadata = std::fs::symlink_metadata(&directory)?;
            // SAFETY: geteuid only reads effective identity.
            if !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o7777 != 0o700
            {
                return Err(
                    "session runtime must be a private owned directory, not a symlink".into(),
                );
            }
        }
        #[cfg(not(unix))]
        return Err("agent execution requires Unix".into());
        let identity = directory.join("instance.json");
        match std::fs::symlink_metadata(&identity) {
            Ok(_) => {
                let saved: PathBuf = read_private(&identity)?;
                if saved != instance {
                    return Err("session runtime belongs to a different instance".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                save_private(&identity, &instance)?
            }
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        let directory_anchor = {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let anchor = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&directory)?;
            #[cfg(target_os = "linux")]
            if !directory_identity_supported(&anchor)? {
                return Err("session runtime requires supported local directory identities".into());
            }
            let observed = anchor.metadata()?;
            let marker = instance.with_file_name(format!(".{name}.session-identity.json"));
            match std::fs::symlink_metadata(&marker) {
                Ok(_) => {
                    let expected: RuntimeIdentity = read_private(&marker)?;
                    if expected.instance != instance
                        || expected.device != observed.dev()
                        || expected.inode != observed.ino()
                    {
                        return Err("session runtime was moved/replaced; inspect surviving agents before recovery".into());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => save_private(
                    &marker,
                    &RuntimeIdentity {
                        instance: instance.into(),
                        device: observed.dev(),
                        inode: observed.ino(),
                    },
                )?,
                Err(error) => return Err(error.into()),
            }
            anchor
        };
        let mut manager = Self {
            instance: instance.into(),
            socket: directory.join("tmux.sock"),
            directory,
            backend_supported: false,
            #[cfg(unix)]
            directory_anchor,
        };
        manager.backend_supported = cfg!(target_os = "linux") && manager.probe_backend();
        Ok(manager)
    }

    fn verify_runtime(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let current = std::fs::symlink_metadata(&self.directory)?;
            let pinned = self.directory_anchor.metadata()?;
            if !current.is_dir()
                || current.dev() != pinned.dev()
                || current.ino() != pinned.ino()
                || current.uid() != pinned.uid()
                || current.mode() & 0o7777 != 0o700
            {
                return Err("session runtime changed; refusing reassociation".into());
            }
        }
        Ok(())
    }

    fn probe_backend(&self) -> bool {
        if std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))
            .is_err()
        {
            return false;
        }
        let Ok(version) = self.command(&["-V"], Instant::now() + Duration::from_secs(1)) else {
            return false;
        };
        let Ok(version) = std::str::from_utf8(&version) else {
            return false;
        };
        let Some(version) = version.strip_prefix("tmux ") else {
            return false;
        };
        let Some((major, minor)) = version.trim().split_once('.') else {
            return false;
        };
        let minor = minor
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        match (major.parse::<u32>(), minor.parse::<u32>()) {
            (Ok(major), Ok(minor)) => major > 3 || (major == 3 && minor >= 2),
            _ => false,
        }
    }

    fn record_path(&self, run: &str, id: &CreationId) -> Result<PathBuf> {
        if run.len() != 32 || !run.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("invalid originating run ID".into());
        }
        Ok(self
            .directory
            .join(format!("{run}-{}.json", request_text(id))))
    }

    fn agent_socket(&self, id: &SessionId) -> PathBuf {
        self.directory.join(format!("a-{}.sock", id_text(id)))
    }

    fn runner(&self, id: &SessionId, stop: bool, deadline: Instant) -> Result<RunnerStatus> {
        #[cfg(unix)]
        {
            let request = if stop {
                RunnerRequest::Stop {
                    session_id: id.clone(),
                }
            } else {
                RunnerRequest::Status {
                    session_id: id.clone(),
                }
            };
            crate::control::local_request(
                &self.agent_socket(id),
                &request,
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(3)),
            )
            .or_else(|error| {
                // Only the supervisor writes this after verified descendant cleanup.
                let status: RunnerStatus =
                    read_private(&self.directory.join(format!("a-{}.done", id_text(id))))?;
                if status.stopped && status.session_id == *id && status.instance == self.instance {
                    Ok(status)
                } else {
                    Err(error)
                }
            })
        }
        #[cfg(not(unix))]
        Err("agent execution requires Unix".into())
    }

    fn tmux(&self, args: &[std::ffi::OsString], deadline: Instant) -> Result<Vec<u8>> {
        if Instant::now() >= deadline {
            return Err("tmux operation deadline expired".into());
        }
        use std::{
            io::{Read, Seek, SeekFrom},
            process::{Command, Stdio},
        };
        #[cfg(unix)]
        {
            match std::fs::symlink_metadata(&self.socket) {
                Ok(_) => {
                    crate::control::tmux_socket_metadata(&self.socket)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let mut output = tempfile::tempfile()?;
        let mut command = Command::new("tmux");
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .args(["-u", "-S"])
            .arg(&self.socket)
            .args(args)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|_| "tmux is unavailable")?;
        loop {
            if output.metadata()?.len() > 128 * 1024 || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("tmux discovery/operation exceeded its budget".into());
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err("tmux operation failed".into());
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        output.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        output.take(128 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 128 * 1024 {
            return Err("tmux output exceeded its budget".into());
        }
        Ok(bytes)
    }

    fn command(&self, args: &[&str], deadline: Instant) -> Result<Vec<u8>> {
        self.tmux(
            &args
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>(),
            deadline,
        )
    }

    fn owned(&self, deadline: Instant) -> Result<bool> {
        if !self.socket.try_exists()? {
            return Ok(false);
        }
        #[cfg(unix)]
        if crate::control::recover_stale_tmux_socket(&self.socket)? {
            return Ok(false);
        }
        let instance = self.command(&["show-options", "-gqv", "@wumpa-instance"], deadline)?;
        if instance.strip_suffix(b"\n") != Some(self.instance.as_os_str().as_encoded_bytes()) {
            return Err("tmux server ownership cannot be verified".into());
        }
        Ok(true)
    }

    fn discovered(&self, deadline: Instant) -> Result<Vec<(String, SessionId, String)>> {
        if !self.owned(deadline)? {
            return Ok(Vec::new());
        }
        let output = self.command(
            &[
                "list-sessions",
                "-F",
                "#{session_name}|#{@wumpa-id}|#{@wumpa-record}",
            ],
            deadline,
        )?;
        let text = std::str::from_utf8(&output)?;
        let mut result = Vec::new();
        for line in text.lines() {
            let mut fields = line.split('|');
            let name = fields.next().ok_or("invalid tmux session metadata")?;
            let id = fields.next().ok_or("invalid tmux session metadata")?;
            let record = fields.next().ok_or("missing tmux recovery record")?;
            if fields.next().is_some()
                || record.len() != 70
                || !record.ends_with(".json")
                || !record.as_bytes()[..65]
                    .iter()
                    .all(|byte| byte.is_ascii_hexdigit() || *byte == b'-')
                || record.as_bytes()[32] != b'-'
            {
                return Err("invalid tmux recovery record tag".into());
            }
            let id = SessionId::try_from(id.to_owned())
                .map_err(|_| "unowned session in dedicated server")?;
            if name != backend_name(&id) {
                return Err("session backend identity mismatch".into());
            }
            result.push((name.into(), id, record.into()));
        }
        if result.len() > MAX_LIVE {
            return Err("session discovery capacity exceeded".into());
        }
        Ok(result)
    }

    #[cfg(target_os = "linux")]
    fn record_count(&self, deadline: Instant) -> Result<usize> {
        let mut count = 0;
        for entry in std::fs::read_dir(&self.directory)? {
            if Instant::now() >= deadline {
                return Err("creation record scan timed out".into());
            }
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name == "instance.json" || !name.ends_with(".json") {
                continue;
            }
            if !entry.file_type()?.is_file() {
                return Err("unsafe creation record entry".into());
            }
            count += 1;
            if count >= MAX_RECORDS {
                return Ok(count);
            }
        }
        Ok(count)
    }

    /// Reconcile without a dashboard. Live identity is proven by the owned runner's
    /// pinned descriptors, not by trusting stale on-disk device/inode observations.
    pub fn snapshot(&mut self, deadline: Instant) -> Snapshot {
        if self.verify_runtime().is_err() {
            return Snapshot {
                sessions: Vec::new(),
                supported: false,
                error: Some("Agent runtime moved/replaced; manual recovery required".into()),
                ..Default::default()
            };
        }
        if !self.backend_supported {
            self.backend_supported = cfg!(target_os = "linux") && self.probe_backend();
            if !self.backend_supported {
                return Snapshot::default();
            }
        }
        #[cfg(target_os = "linux")]
        let mut integrations = Vec::new();
        let result = (|| -> Result<Vec<Session>> {
            let live = self.discovered(deadline)?;
            let mut sessions = Vec::new();
            for (name, id, record_name) in live {
                if Instant::now() >= deadline {
                    return Err("session refresh timed out".into());
                }
                let path = self.directory.join(record_name);
                let mut record: Record = read_private(&path)?;
                if record.session.id != id
                    || record.creation.instance != self.instance
                    || record.session.instance != self.instance
                    || record.creation.checkout != record.session.checkout
                    || self.record_path(
                        &record.creation.originating_run_id,
                        &record.creation.request_id,
                    )? != path
                {
                    return Err("owned session recovery metadata mismatch".into());
                }
                let status = self.runner(&id, false, deadline)?;
                if status.instance != self.instance
                    || status.session_id != id
                    || status.checkout != record.session.checkout
                {
                    return Err("runner ownership/checkout identity mismatch".into());
                }
                if status.stopped {
                    self.command(&["kill-session", "-t", &name], deadline)?;
                    continue;
                }
                if record.pi != status.pi {
                    record.pi = status.pi.clone();
                    save_private(&path, &record)?;
                }
                // Backend membership and runner identity agree; recover a lost reply.
                if matches!(record.creation.outcome, CreationOutcome::InProgress) {
                    record.creation.outcome = CreationOutcome::Created {
                        session_id: id.clone(),
                    };
                    save_private(&path, &record)?;
                }
                record.session.state = status.state;
                // Confirm replacement using successful isolated Git discovery only.
                // Access/discovery failures are unknown and never terminate agents.
                if let Ok(current) = checkout::observe(&record.session.checkout.root.path, deadline)
                {
                    if CheckoutAssociation::from(&current) != record.session.checkout {
                        let stopped = self.runner(&id, true, deadline)?;
                        if !stopped.stopped {
                            return Err("checkout replacement termination failed".into());
                        }
                        let _ = self.command(&["kill-session", "-t", &name], deadline);
                        continue;
                    }
                }
                #[cfg(target_os = "linux")]
                if let Some(metadata) = record.pi {
                    integrations.push((id, metadata));
                }
                sessions.push(record.session);
            }
            Ok(sessions)
        })();
        match result {
            Ok(sessions) => Snapshot {
                sessions,
                #[cfg(target_os = "linux")]
                integrations,
                error: None,
                supported: cfg!(target_os = "linux"),
                ..Default::default()
            },
            Err(_) => Snapshot { sessions: Vec::new(), error: Some("Agent discovery/reconciliation unavailable; existing agents were not reassociated".into()), supported: cfg!(target_os = "linux"), ..Default::default() },
        }
    }

    /// Stop a verified owned agent before removing its backend session.
    pub fn delete_agent(&mut self, id: &SessionId, deadline: Instant) -> Result<()> {
        self.verify_runtime()?;
        let snapshot = self.snapshot(deadline);
        if snapshot.error.is_some() || !snapshot.supported {
            return Err("cannot verify agents; deletion aborted".into());
        }
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.id == *id)
            .ok_or("agent is no longer available")?;
        let status = self.runner(id, true, deadline)?;
        if !status.stopped
            || status.instance != self.instance
            || status.checkout != session.checkout
        {
            return Err("agent termination could not be verified".into());
        }
        self.command(&["kill-session", "-t", &backend_name(id)], deadline)?;
        Ok(())
    }

    /// Hold the manager mutex through removal. Stop failures abort deletion;
    /// the callback must itself use replacement-safe filesystem deletion.
    pub fn with_checkout_removal(
        &mut self,
        checkout: &checkout::Checkout,
        remove: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        let association = CheckoutAssociation::from(checkout);
        let snapshot = self.snapshot(deadline);
        if !snapshot.supported || snapshot.error.is_some() {
            return Err("cannot verify running agents; removal aborted".into());
        }
        let owned = snapshot
            .sessions
            .into_iter()
            .filter(|session| session.checkout == association)
            .collect::<Vec<_>>();
        if !owned.is_empty() {
            crate::output::info(
                "Removing checkout",
                format!("force-stopping {} owned agents before removal", owned.len()),
            );
        }
        for session in owned {
            let status = self.runner(&session.id, true, deadline)?;
            if !status.stopped || status.instance != self.instance || status.checkout != association
            {
                return Err(
                    "owned agent termination could not be verified; removal aborted".into(),
                );
            }
            let _ = self.command(
                &["kill-session", "-t", &backend_name(&session.id)],
                deadline,
            );
        }
        let current = checkout::observe(&checkout.root.path, deadline)?;
        if CheckoutAssociation::from(&current) != association {
            return Err("checkout changed before removal; removal aborted".into());
        }
        remove()
    }

    /// Validate run, current registration, and checkout before session operations.
    pub fn dispatch(
        &mut self,
        request: LocalRequest,
        run: &str,
        config: &ServerConfig,
        deadline: Instant,
    ) -> LocalResponse {
        let result = (|| -> std::result::Result<LocalResponse, Failure> {
            request.validate_run(run)?;
            self.verify_runtime()
                .map_err(|_| Failure::BackendUnavailable)?;
            let observations = match &request.operation {
                Operation::List { observations }
                | Operation::Create { observations, .. }
                | Operation::RetryCreate { observations, .. }
                | Operation::Attach { observations, .. } => observations,
            };
            let checkout = checkout::validate(
                &observations.directory.path,
                observations,
                &config.repositories,
                deadline,
            )
            .map_err(|_| Failure::CheckoutUnavailable)?;
            let association = CheckoutAssociation::from(&checkout);
            match request.operation {
                Operation::List { .. } => {
                    let snapshot = self.snapshot(deadline);
                    if snapshot.error.is_some() || !snapshot.supported {
                        return Err(Failure::BackendUnavailable);
                    }
                    Ok(LocalResponse::Listed {
                        sessions: snapshot
                            .sessions
                            .into_iter()
                            .filter(|session| session.checkout == association)
                            .collect(),
                    })
                }
                Operation::Attach { session_id, .. } => {
                    let snapshot = self.snapshot(deadline);
                    if snapshot.error.is_some() || !snapshot.supported {
                        return Err(Failure::BackendUnavailable);
                    }
                    let session = snapshot
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .ok_or(Failure::SessionNotFound)?;
                    if session.checkout != association {
                        return Err(Failure::OwnershipMismatch);
                    }
                    if session.state != State::Running {
                        return Err(Failure::SessionNotRunning);
                    }
                    Ok(LocalResponse::Attached {
                        attachment: Attachment::Tmux {
                            socket: self.socket.clone(),
                            session: backend_name(&session_id),
                        },
                    })
                }
                Operation::RetryCreate {
                    request_id,
                    originating_run_id,
                    ..
                } => {
                    let path = self
                        .record_path(&originating_run_id, &request_id)
                        .map_err(|_| Failure::RequestConflict)?;
                    let snapshot = self.snapshot(deadline);
                    if snapshot.error.is_some() {
                        return Err(Failure::OutcomeUnknown);
                    }
                    let record = read_private::<Record>(&path).ok();
                    if record.as_ref().is_some_and(|record| {
                        matches!(record.creation.outcome, CreationOutcome::InProgress)
                    }) {
                        return Err(Failure::OutcomeUnknown);
                    }
                    let session_id = resolve_retry(
                        &self.instance,
                        &originating_run_id,
                        &request_id,
                        &association,
                        record.as_ref().map(|record| &record.creation),
                    )?;
                    Ok(LocalResponse::Created { session_id })
                }
                Operation::Create {
                    request_id,
                    environment,
                    name,
                    ..
                } => {
                    #[cfg(not(target_os = "linux"))]
                    {
                        let _ = (request_id, environment, name);
                        Err(Failure::UnsupportedPlatform)
                    }
                    #[cfg(target_os = "linux")]
                    {
                        if !self.backend_supported {
                            return Err(Failure::BackendUnavailable);
                        }
                        validate_creation_filesystems(&checkout)?;
                        let path = self
                            .record_path(run, &request_id)
                            .map_err(|_| Failure::RequestConflict)?;
                        if path.try_exists().map_err(|_| Failure::OutcomeUnknown)? {
                            if self.snapshot(deadline).error.is_some() {
                                return Err(Failure::OutcomeUnknown);
                            }
                            let record: Record =
                                read_private(&path).map_err(|_| Failure::OutcomeUnknown)?;
                            if matches!(record.creation.outcome, CreationOutcome::InProgress) {
                                return Err(Failure::OutcomeUnknown);
                            }
                            let id = resolve_retry(
                                &self.instance,
                                run,
                                &request_id,
                                &association,
                                Some(&record.creation),
                            )?;
                            return Ok(LocalResponse::Created { session_id: id });
                        }
                        if self
                            .record_count(deadline)
                            .map_err(|_| Failure::OutcomeUnknown)?
                            >= MAX_RECORDS
                        {
                            return Err(Failure::CapacityExceeded);
                        }
                        let snapshot = self.snapshot(deadline);
                        if snapshot.error.is_some() || !snapshot.supported {
                            return Err(Failure::BackendUnavailable);
                        }
                        let live = self
                            .discovered(deadline)
                            .map_err(|_| Failure::BackendUnavailable)?;
                        if live.len() >= MAX_LIVE {
                            return Err(Failure::CapacityExceeded);
                        }
                        let prepared = environment
                            .prepare(&checkout.directory.path)
                            .map_err(Failure::from)?;
                        prepared
                            .command(&config.agent_command, &checkout.root.path)
                            .map_err(Failure::from)?;
                        let id =
                            SessionId::try_from(random_id().map_err(|_| Failure::LaunchFailed)?)
                                .map_err(|_| Failure::LaunchFailed)?;
                        let mut record = Record {
                            creation: CreationRecord {
                                instance: self.instance.clone(),
                                originating_run_id: run.into(),
                                request_id,
                                checkout: association.clone(),
                                outcome: CreationOutcome::InProgress,
                            },
                            session: Session {
                                id: id.clone(),
                                instance: self.instance.clone(),
                                checkout: association,
                                label: name.map(String::from).unwrap_or_else(|| {
                                    Path::new(&config.agent_command.arguments()[0])
                                        .file_name()
                                        .and_then(|name| name.to_str())
                                        .unwrap_or("agent")
                                        .into()
                                }),
                                state: State::Starting,
                            },
                            pi: None,
                        };
                        save_private(&path, &record).map_err(|_| Failure::LaunchFailed)?;
                        let launch = Launch {
                            instance: self.instance.clone(),
                            session_id: id.clone(),
                            checkout,
                            command: config.agent_command.clone(),
                            integration: config.agent_integration,
                            label: record.session.label.clone(),
                            environment,
                        };
                        match self.launch(
                            &launch,
                            live.is_empty() && !self.socket.exists(),
                            &path,
                            deadline,
                        ) {
                            Ok(pi) => {
                                record.pi = pi;
                                record.creation.outcome = CreationOutcome::Created {
                                    session_id: id.clone(),
                                };
                                record.session.state = State::Running;
                                save_private(&path, &record)
                                    .map_err(|_| Failure::OutcomeUnknown)?;
                                Ok(LocalResponse::Created { session_id: id })
                            }
                            Err(_) => {
                                // Never kill tmux around a running agent without runner-confirmed termination.
                                if self
                                    .runner(&id, true, deadline)
                                    .is_ok_and(|status| status.stopped)
                                {
                                    let _ = self.command(
                                        &["kill-session", "-t", &backend_name(&id)],
                                        deadline,
                                    );
                                    record.creation.outcome = CreationOutcome::Failed {
                                        failure: Failure::LaunchFailed,
                                    };
                                    let _ = save_private(&path, &record);
                                    Err(Failure::LaunchFailed)
                                } else {
                                    // A lost response may mean launch succeeded; leave its key reserved.
                                    Err(Failure::OutcomeUnknown)
                                }
                            }
                        }
                    }
                }
            }
        })();
        result.unwrap_or_else(|failure| LocalResponse::Failed { failure })
    }

    #[cfg(target_os = "linux")]
    fn launch(
        &self,
        launch: &Launch,
        new_server: bool,
        record_path: &Path,
        deadline: Instant,
    ) -> Result<Option<crate::agent_integration::Metadata>> {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let channel = self
            .directory
            .join(format!("l-{}.sock", id_text(&launch.session_id)));
        let listener = UnixListener::bind(&channel)?;
        std::fs::set_permissions(&channel, std::fs::Permissions::from_mode(0o600))?;
        struct Channel(PathBuf, std::fs::Metadata);
        impl Drop for Channel {
            fn drop(&mut self) {
                if let Err(error) = crate::control::cleanup_socket(&self.0, &self.1) {
                    crate::output::error(format_args!(
                        "agent launch channel cleanup failed: {error}"
                    ));
                }
            }
        }
        let _channel = Channel(channel.clone(), std::fs::symlink_metadata(&channel)?);
        listener.set_nonblocking(true)?;
        let name = backend_name(&launch.session_id);
        let args = vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            name.clone().into(),
            "-c".into(),
            launch.checkout.root.path.as_os_str().into(),
            std::env::current_exe()?.into_os_string(),
            "agent-runner".into(),
            "--channel".into(),
            channel.into_os_string(),
            "--id".into(),
            id_text(&launch.session_id).into(),
        ];
        self.tmux(&args, deadline)?;
        if new_server {
            self.tmux(
                &[
                    "set-option".into(),
                    "-g".into(),
                    "@wumpa-instance".into(),
                    self.instance.as_os_str().into(),
                ],
                deadline,
            )?;
        }
        self.command(
            &[
                "set-option",
                "-t",
                &name,
                "@wumpa-id",
                &id_text(&launch.session_id),
            ],
            deadline,
        )?;
        self.command(
            &[
                "set-option",
                "-t",
                &name,
                "@wumpa-record",
                record_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or("invalid recovery record path")?,
            ],
            deadline,
        )?;
        self.command(
            &["set-option", "-t", &name, "remain-on-exit", "off"],
            deadline,
        )?;
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    crate::control::verify_peer(&stream)?;
                    let mut exchange = crate::control::Exchange {
                        stream: &mut stream,
                        deadline,
                    };
                    let hello: SessionId = crate::protocol::read_message(&mut exchange)?;
                    if hello != launch.session_id {
                        return Err("launch peer identity mismatch".into());
                    }
                    crate::protocol::write_message(launch, &mut exchange)?;
                    let ready: RunnerStatus = crate::protocol::read_message(&mut exchange)?;
                    if ready.instance != self.instance
                        || ready.session_id != launch.session_id
                        || ready.checkout != CheckoutAssociation::from(&launch.checkout)
                        || ready.state != State::Running
                    {
                        return Err("agent launch did not confirm readiness".into());
                    }
                    return Ok(ready.pi);
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return Err("agent launch channel unavailable".into()),
            }
        }
    }
}

fn save_private<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    crate::config::save(path, value)?;
    // Commit the rename/directory entry before any launch side effect.
    std::fs::File::open(path.parent().ok_or("record has no parent")?)?.sync_all()?;
    Ok(())
}

fn read_private<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    use std::io::Read;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(unix)]
    {
        // SAFETY: geteuid only reads identity.
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o600
        {
            return Err("unsafe session record".into());
        }
    }
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err("session record is too large".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(target_os = "linux")]
mod runner {
    use super::*;
    use std::{
        fs::{File, OpenOptions},
        io,
        os::{
            fd::AsRawFd,
            unix::{
                fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
                net::{UnixListener, UnixStream},
                process::CommandExt,
            },
        },
        process::Child,
    };

    static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    extern "C" fn stopping(_: libc::c_int) {
        STOP.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    struct Anchors(Vec<(File, checkout::Observation)>);
    impl Anchors {
        fn new(checkout: &checkout::Checkout) -> Result<Self> {
            let mut anchors = Vec::new();
            for observation in [
                &checkout.root,
                &checkout.git_directory,
                &checkout.common_directory,
            ] {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                    .open(&observation.path)?;
                if !directory_identity_supported(&file)? {
                    return Err("checkout filesystem has unsupported directory identities".into());
                }
                let metadata = file.metadata()?;
                if metadata.dev() != observation.device || metadata.ino() != observation.inode {
                    return Err("checkout changed before launch".into());
                }
                anchors.push((file, observation.clone()));
            }
            Ok(Self(anchors))
        }
        fn removed(&self) -> bool {
            self.0
                .iter()
                .any(|(_, observed)| match std::fs::metadata(&observed.path) {
                    Ok(metadata) => {
                        metadata.dev() != observed.device || metadata.ino() != observed.inode
                    }
                    Err(error) => error.kind() == io::ErrorKind::NotFound,
                })
        }
    }

    /// Kill/reap direct children repeatedly. As a subreaper, orphaned descendants
    /// (including double-forked/setsid children) become our direct children. Their
    /// PIDs cannot be reused before this process reaps them.
    fn terminate(child: &mut Child) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let children = std::fs::read_to_string(format!(
                "/proc/self/task/{}/children",
                std::process::id()
            ))?;
            for pid in children.split_whitespace() {
                let pid: i32 = pid.parse()?;
                // SAFETY: these are our unreaped direct children. SIGCHLD is
                // defaulted and this sole thread reaps only after the entire list
                // has been signalled, so their PIDs cannot be reused in this loop.
                // Do not signal queried group IDs: a child could leave a group
                // between getpgid and kill, allowing an unrelated group ID reuse.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
            let _ = child.try_wait();
            loop {
                let mut status = 0;
                // SAFETY: waitpid reaps only this process's children.
                let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                if pid > 0 {
                    continue;
                }
                if pid == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                    return Ok(());
                }
                break;
            }
            if Instant::now() >= deadline {
                return Err("owned child termination could not be confirmed".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn run(channel: &Path, id: SessionId) -> Result<()> {
        // SAFETY: signal installs a handler that only updates an atomic flag.
        // Hangup/termination never bypass owned-descendant cleanup.
        unsafe {
            // Explicitly prevent inherited SIG_IGN/automatic child reaping:
            // unreaped direct-child PID ownership is the termination proof.
            if libc::signal(libc::SIGCHLD, libc::SIG_DFL) == libc::SIG_ERR {
                return Err("cannot establish owned-child reaping".into());
            }
            for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
                if libc::signal(signal, stopping as *const () as libc::sighandler_t)
                    == libc::SIG_ERR
                {
                    return Err("cannot install agent cleanup signals".into());
                }
            }
        }
        // SAFETY: prctl affects only this process's descendant adoption policy.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
            return Err("agent descendant containment unavailable".into());
        }
        // Require procfs before launching; never downgrade to process-group-only cleanup.
        std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))?;
        let socket = channel
            .parent()
            .ok_or("launch channel has no parent")?
            .join(format!("a-{}.sock", id_text(&id)));
        // Pin the runtime directory too: replacing/removing it cannot permit inode
        // reuse while this supervisor and its agent still exist.
        let _runtime_anchor = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(channel.parent().ok_or("missing runtime directory")?)?;
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let identity = std::fs::symlink_metadata(&socket)?;
        struct Endpoint(PathBuf, std::fs::Metadata);
        impl Drop for Endpoint {
            fn drop(&mut self) {
                if let Err(error) = crate::control::cleanup_socket(&self.0, &self.1) {
                    crate::output::error(format_args!(
                        "agent supervisor endpoint cleanup failed: {error}"
                    ));
                }
            }
        }
        let _endpoint = Endpoint(socket, identity);
        let mut stream = UnixStream::connect(channel)?;
        crate::control::verify_peer(&stream)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exchange = crate::control::Exchange {
            stream: &mut stream,
            deadline,
        };
        crate::protocol::write_message(&id, &mut exchange)?;
        let launch: Launch = crate::protocol::read_message(&mut exchange)?;
        if launch.session_id != id {
            return Err("launch identity mismatch".into());
        }
        let anchors = Anchors::new(&launch.checkout)?;
        if anchors.removed() {
            return Err("checkout moved before launch".into());
        }
        let prepared = launch
            .environment
            .prepare(&launch.checkout.directory.path)?;
        if STOP.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("launch cancelled by supervisor termination".into());
        }
        let mut pi_resource = if launch.integration == crate::config::AgentIntegration::Pi {
            crate::agent_integration::Resource::new(
                channel.parent().ok_or("missing runtime directory")?,
                &id,
            )
            .ok()
        } else {
            None
        };
        let mut command = if let Some(resource) = &pi_resource {
            let executable =
                crate::config::AgentCommand::try_from(vec![launch.command.arguments()[0].clone()])?;
            let mut command = prepared.command(&executable, &launch.checkout.root.path)?;
            resource.configure(&mut command, &launch.command, &launch.label, &id);
            command
        } else {
            prepared.command(&launch.command, &launch.checkout.root.path)?
        };
        if pi_resource.is_none() {
            // Reserved integration variables cannot be supplied by callers to
            // disabled/unsupported agents or failed materializations.
            command.env_remove("WUMPA_ACTIVITY_SOCKET");
            command.env_remove("WUMPA_SESSION_ID");
        }
        // tmux's newly allocated pane supplies fresh terminal bookkeeping.
        for name in ["TERM", "TMUX", "TMUX_PANE"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let root_fd = anchors.0[0].0.as_raw_fd();
        command.process_group(0);
        // SAFETY: fchdir is async-signal-safe and the pinned descriptor remains live
        // through spawn. Execute in the original object, never its replacement.
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(root_fd) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // Give the agent's group the pane terminal before it can read.
                // Otherwise a separate child group would be stopped by SIGTTIN.
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                if libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpid()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|_| "agent execution failed")?;
        if let Some(resource) = pi_resource.as_mut() {
            resource.launched();
        }
        let status = RunnerStatus {
            instance: launch.instance,
            session_id: id,
            checkout: CheckoutAssociation::from(&launch.checkout),
            state: State::Running,
            stopped: false,
            pi: pi_resource
                .as_ref()
                .map(crate::agent_integration::Resource::metadata),
        };
        // Disconnect/lost launch acknowledgement must not destroy a live agent.
        let _ = crate::protocol::write_message(&status, &mut exchange);
        drop(stream);
        let mut stopped = false;
        let mut cleanup_failed = false;
        let mut next_git = Instant::now();
        loop {
            if !stopped
                && (STOP.load(std::sync::atomic::Ordering::Relaxed)
                    || cleanup_failed
                    || anchors.removed()
                    || child.try_wait()?.is_some())
            {
                match terminate(&mut child) {
                    Ok(()) => stopped = true,
                    Err(_) => cleanup_failed = true,
                }
            }
            if !stopped && Instant::now() >= next_git {
                if let Ok(current) = checkout::observe(
                    &status.checkout.root.path,
                    Instant::now() + Duration::from_millis(500),
                ) {
                    if CheckoutAssociation::from(&current) != status.checkout {
                        match terminate(&mut child) {
                            Ok(()) => stopped = true,
                            Err(_) => cleanup_failed = true,
                        }
                    }
                }
                next_git = Instant::now() + Duration::from_secs(1);
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let result = (|| -> Result<bool> {
                        crate::control::verify_peer(&stream)?;
                        let mut exchange = crate::control::Exchange {
                            stream: &mut stream,
                            deadline: Instant::now() + Duration::from_secs(3),
                        };
                        let request: RunnerRequest = crate::protocol::read_message(&mut exchange)?;
                        let (requested, stop) = match request {
                            RunnerRequest::Status { session_id } => (session_id, false),
                            RunnerRequest::Stop { session_id } => (session_id, true),
                        };
                        if requested != status.session_id {
                            return Err("runner identity mismatch".into());
                        }
                        if stop && !stopped {
                            match terminate(&mut child) {
                                Ok(()) => stopped = true,
                                Err(_) => cleanup_failed = true,
                            }
                        }
                        let response = RunnerStatus {
                            instance: status.instance.clone(),
                            session_id: status.session_id.clone(),
                            checkout: status.checkout.clone(),
                            state: if cleanup_failed {
                                State::CleanupFailed
                            } else {
                                State::Running
                            },
                            stopped,
                            pi: status.pi.clone(),
                        };
                        crate::protocol::write_message(&response, &mut exchange)?;
                        Ok(stop && stopped)
                    })();
                    if result.unwrap_or(false) {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    // Losing runner control cannot orphan its agent tree.
                    match terminate(&mut child) {
                        Ok(()) => stopped = true,
                        Err(_) => cleanup_failed = true,
                    }
                }
            }
            if stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(resource) = pi_resource.as_mut() {
            resource.terminated();
        }
        drop(pi_resource);
        // Preserve completion evidence before removing the whole owned session.
        // Extra windows/panes must not retain an exited agent indefinitely.
        let completed = RunnerStatus {
            stopped: true,
            ..status
        };
        let directory = channel.parent().ok_or("missing runtime directory")?;
        let pinned_directory =
            PathBuf::from(format!("/proc/self/fd/{}", _runtime_anchor.as_raw_fd()));
        save_private(
            &pinned_directory.join(format!("a-{}.done", id_text(&completed.session_id))),
            &completed,
        )?;
        let manager = Manager {
            instance: completed.instance.clone(),
            directory: directory.to_path_buf(),
            socket: tmux_socket(&completed.instance)?,
            backend_supported: true,
            directory_anchor: _runtime_anchor,
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        manager.verify_runtime()?;
        if manager.owned(deadline)? {
            manager.command(
                &["kill-session", "-t", &backend_name(&completed.session_id)],
                deadline,
            )?;
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};

    #[test]
    fn remote_or_unknown_filesystems_cannot_claim_pinned_inode_lifetimes() {
        for magic in [0xef53, 0x58465342, 0x9123683e, 0x01021994, 0x794c7630] {
            assert!(supported_directory_fs(magic));
        }
        for magic in [0x6969, 0x65735546, 0x01021997, 0] {
            // NFS, FUSE, 9p, unknown
            assert!(!supported_directory_fs(magic));
        }
    }

    #[test]
    fn runtime_replacement_is_not_silently_reassociated() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let instance = directory.path().join("control.sock");
        let manager = Manager::new(&instance).unwrap();
        let old = directory.path().join("old-runtime");
        std::fs::rename(&manager.directory, &old).unwrap();
        std::fs::create_dir(&manager.directory).unwrap();
        std::fs::set_permissions(&manager.directory, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert!(manager.verify_runtime().is_err());
        assert!(Manager::new(&instance).is_err());
        assert!(old.join("instance.json").exists());
    }

    #[test]
    fn private_records_reject_symlinks_and_remote_budget_is_explicit() {
        let directory = tempfile::tempdir().unwrap();
        let actual = directory.path().join("actual.json");
        save_private(&actual, &"test").unwrap();
        let alias = directory.path().join("alias.json");
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        assert!(read_private::<String>(&alias).is_err());
        let mut remote = RemoteSnapshot {
            supported: true,
            error: None,
            sessions: vec![Summary {
                id: SessionId::try_from("a".repeat(32)).unwrap(),
                label: "pi".into(),
                checkout: format!("/{}", "x".repeat(128 * 1024)).into(),
                state: State::Running,
                activity: Default::default(),
                pi_session_name: None,
                pi_session_id: None,
            }],
        };
        remote.limit();
        assert!(remote.error.is_some());
        assert!(remote.sessions.is_empty());
    }

    #[test]
    fn display_ids_are_minimal_unique_server_wide_and_never_change_full_identities() {
        let summary = |id: String, checkout: &str| Summary {
            id: SessionId::try_from(id).unwrap(),
            checkout: checkout.into(),
            label: "Same name".into(),
            state: State::Running,
            activity: Default::default(),
            pi_session_name: None,
            pi_session_id: None,
        };
        let first = format!("abcde0{}", "0".repeat(26));
        let second = format!("abcde10{}", "0".repeat(25));
        let third = format!("abcde11{}", "0".repeat(25));
        let separate = "f".repeat(32);
        let mut snapshot = RemoteSnapshot {
            sessions: vec![
                summary(first.clone(), "/one"),
                summary(third.clone(), "/two"),
                summary(second.clone(), "/three"),
                summary(separate.clone(), "/one"),
            ],
            supported: true,
            error: None,
        };
        let serialized = serde_json::to_vec(&snapshot).unwrap();
        let labels = snapshot.display_ids();
        assert_eq!(labels[&first], "abcde0");
        assert_eq!(labels[&second], "abcde10");
        assert_eq!(labels[&third], "abcde11");
        assert_eq!(labels[&separate], "fffff");
        assert_eq!(serde_json::to_vec(&snapshot).unwrap(), serialized);
        snapshot.sessions.reverse();
        assert_eq!(snapshot.display_ids(), labels);
        snapshot
            .sessions
            .retain(|session| String::from(session.id.clone()) == first);
        snapshot.sessions.push(snapshot.sessions[0].clone());
        assert_eq!(snapshot.display_ids()[&first], "abcde"); // Duplicate identity is not a collision.
        snapshot.sessions.clear();
        assert!(snapshot.display_ids().is_empty());
        let almost_same = format!("{}b", "a".repeat(31));
        let same_prefix = "a".repeat(32);
        snapshot.sessions = vec![
            summary(almost_same.clone(), "/one"),
            summary(same_prefix.clone(), "/one"),
        ];
        assert_eq!(snapshot.display_ids()[&almost_same], almost_same);
        assert_eq!(snapshot.display_ids()[&same_prefix], same_prefix);
    }

    #[test]
    fn activity_summaries_are_backward_compatible_and_names_clear_only_authoritatively() {
        use crate::agent_activity::Activity;
        let mut legacy: Summary = serde_json::from_value(serde_json::json!({
            "id":"a".repeat(32), "checkout":"/repo", "label":"original", "state":"running"
        }))
        .unwrap();
        assert_eq!(legacy.activity, Activity::Unknown);
        assert_eq!(legacy.display_state(), "Unknown");
        assert_eq!(legacy.display_name(), "original");
        legacy.activity = Activity::WaitingForInput;
        assert_eq!(legacy.display_state(), "Waiting for input");
        legacy.activity = Activity::Working;
        assert_eq!(legacy.display_state(), "Running");
        legacy.pi_session_name = Some("Pi renamed".into());
        legacy.pi_session_id = Some("conversation".into());
        let previous = RemoteSnapshot {
            sessions: vec![legacy.clone()],
            supported: true,
            error: None,
        };
        let mut current = previous.clone();
        current.sessions[0].pi_session_id = None;
        current.sessions[0].pi_session_name = None;
        current.invalidate_activity();
        current.retain_cached_names(&previous);
        assert_eq!(current.sessions[0].display_name(), "Pi renamed");
        assert_eq!(current.sessions[0].display_state(), "Unknown");
        current.sessions[0].pi_session_id = Some("new-conversation".into());
        current.sessions[0].pi_session_name = None;
        current.retain_cached_names(&previous);
        assert_eq!(current.sessions[0].display_name(), "original");
        for (state, label) in [
            (State::Starting, "Starting"),
            (State::Stopping, "Stopping"),
            (State::CleanupFailed, "Cleanup failed"),
        ] {
            current.sessions[0].state = state;
            assert_eq!(current.sessions[0].display_state(), label);
        }
        let serialized = serde_json::to_value(&previous).unwrap();
        assert_eq!(serialized["sessions"][0]["activity"], "working");
        assert!(serialized["sessions"][0].get("instance").is_none());
        assert!(serialized["sessions"][0].get("directory").is_none());
    }

    #[test]
    fn unsafe_record_types_fail_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("instance.json");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a valid C pathname in a private test directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        assert!(read_private::<String>(&fifo).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(read_private::<String>(directory.path()).is_err());
    }

    #[test]
    fn unsupported_discovery_aborts_managed_removal() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut manager = Manager::new(&directory.path().join("control.sock")).unwrap();
        let root = directory.path().join("checkout");
        std::fs::create_dir(&root).unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .arg("init")
                .output()
                .unwrap()
                .status
                .success()
        );
        let checkout = checkout::observe(&root, Instant::now() + Duration::from_secs(3)).unwrap();
        // Simulate unavailable backend while recovery records/agents may survive.
        manager.backend_supported = false;
        std::fs::write(&manager.socket, "unsafe endpoint").unwrap();
        let snapshot = manager.snapshot(Instant::now() + Duration::from_secs(3));
        assert!(!snapshot.supported);
        assert!(snapshot.error.is_none());
        let mut removed = false;
        assert!(
            manager
                .with_checkout_removal(&checkout, || {
                    removed = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!removed);
    }

    #[test]
    fn unverified_termination_aborts_managed_removal_callback() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut manager = Manager::new(&directory.path().join("control.sock")).unwrap();
        assert!(
            manager.backend_supported,
            "runtime tests require tmux >= 3.2"
        );
        let root = directory.path().join("checkout");
        std::fs::create_dir(&root).unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .arg("init")
                .output()
                .unwrap()
                .status
                .success()
        );
        let checkout = checkout::observe(&root, Instant::now() + Duration::from_secs(3)).unwrap();
        let id = SessionId::try_from("a".repeat(32)).unwrap();
        let request_id = CreationId::try_from("b".repeat(32)).unwrap();
        let path = manager.record_path(&"c".repeat(32), &request_id).unwrap();
        let session = Session {
            id: id.clone(),
            instance: manager.instance.clone(),
            checkout: CheckoutAssociation::from(&checkout),
            label: "test".into(),
            state: State::Running,
        };
        save_private(
            &path,
            &Record {
                creation: CreationRecord {
                    instance: manager.instance.clone(),
                    originating_run_id: "c".repeat(32),
                    request_id,
                    checkout: session.checkout.clone(),
                    outcome: CreationOutcome::Created {
                        session_id: id.clone(),
                    },
                },
                session,
                pi: None,
            },
        )
        .unwrap();
        let name = backend_name(&id);
        manager
            .command(
                &["new-session", "-d", "-s", &name, "sleep", "30"],
                Instant::now() + Duration::from_secs(3),
            )
            .unwrap();
        struct Backend(PathBuf);
        impl Drop for Backend {
            fn drop(&mut self) {
                let _ = std::process::Command::new("tmux")
                    .arg("-S")
                    .arg(&self.0)
                    .arg("kill-server")
                    .output();
            }
        }
        let _backend = Backend(manager.socket.clone());
        manager
            .tmux(
                &[
                    "set-option".into(),
                    "-g".into(),
                    "@wumpa-instance".into(),
                    manager.instance.as_os_str().into(),
                ],
                Instant::now() + Duration::from_secs(3),
            )
            .unwrap();
        manager
            .command(
                &["set-option", "-t", &name, "@wumpa-id", &id_text(&id)],
                Instant::now() + Duration::from_secs(3),
            )
            .unwrap();
        manager
            .command(
                &[
                    "set-option",
                    "-t",
                    &name,
                    "@wumpa-record",
                    path.file_name().unwrap().to_str().unwrap(),
                ],
                Instant::now() + Duration::from_secs(3),
            )
            .unwrap();
        let socket = manager.agent_socket(&id);
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let instance = manager.instance.clone();
        let association = CheckoutAssociation::from(&checkout);
        let worker = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut exchange = crate::control::Exchange {
                    stream: &mut stream,
                    deadline: Instant::now() + Duration::from_secs(3),
                };
                let _: RunnerRequest = crate::protocol::read_message(&mut exchange).unwrap();
                crate::protocol::write_message(
                    &RunnerStatus {
                        instance: instance.clone(),
                        session_id: id.clone(),
                        checkout: association.clone(),
                        state: State::CleanupFailed,
                        stopped: false,
                        pi: None,
                    },
                    &mut exchange,
                )
                .unwrap();
            }
        });
        let mut removed = false;
        assert!(
            manager
                .with_checkout_removal(&checkout, || {
                    removed = true;
                    Ok(())
                })
                .is_err()
        );
        worker.join().unwrap();
        assert!(!removed);
        assert!(root.exists());
    }
}

/// Internal tmux pane supervisor; never reads server configuration or shell startup.
pub fn run_agent(channel: &Path, id: &str) -> Result<()> {
    let id = SessionId::try_from(id.to_owned())?;
    #[cfg(target_os = "linux")]
    {
        runner::run(channel, id)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (channel, id);
        Err("verified agent descendant containment currently requires Linux".into())
    }
}
