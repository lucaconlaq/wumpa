//! Observational Pi subscriptions, isolated from agent control and Git discovery.

use serde::{Deserialize, Serialize};

/// Session-level work, separate from the runner's process lifecycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Working,
    WaitingForInput,
    #[default]
    Unknown,
}

impl Activity {
    pub fn is_unknown(&self) -> bool {
        *self == Self::Unknown
    }
}

/// Remote-safe cached data. Names are display-only and must be escaped.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub pi_session_id: Option<String>,
    pub pi_session_name: Option<String>,
    pub activity: Activity,
}

#[cfg(target_os = "linux")]
pub use linux::Hub;

#[cfg(target_os = "linux")]
mod linux {
    use super::{Activity, Status};
    use crate::{
        Result, agent_integration::Metadata, session_runtime::Snapshot, sessions::SessionId,
    };
    use serde::Deserialize;
    use std::{
        collections::BTreeMap,
        fs::OpenOptions,
        io::{Read, Write},
        os::unix::fs::{MetadataExt, OpenOptionsExt},
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    const MAX_WORKERS: usize = 64;
    const MAX_FRAME: usize = 8192;
    const MAX_NAME: usize = 512;
    const MAX_SEQUENCE: u64 = 9_007_199_254_740_991;
    const STALE: Duration = Duration::from_secs(15);
    const HANDSHAKE: Duration = Duration::from_secs(3);
    const IO_TIMEOUT: Duration = Duration::from_millis(100);

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Frame {
        #[serde(rename = "type")]
        kind: String,
        version: u32,
        generation: String,
        sequence: u64,
        wumpa_session_id: SessionId,
        pi_session_id: String,
        // Value intentionally requires the key, even when its value is null.
        pi_session_name: serde_json::Value,
        activity: Activity,
    }

    impl Frame {
        fn parse(bytes: &[u8], id: &SessionId) -> Result<Self> {
            if bytes.len() > MAX_FRAME {
                return Err("oversized activity frame".into());
            }
            let frame: Self = serde_json::from_slice(bytes)?;
            let uuid = frame.generation.as_bytes();
            if frame.kind != "status"
                || frame.version != 1
                || frame.wumpa_session_id != *id
                || frame.sequence == 0
                || frame.sequence > MAX_SEQUENCE
                || uuid.len() != 36
                || !uuid.iter().enumerate().all(|(i, byte)| {
                    if matches!(i, 8 | 13 | 18 | 23) {
                        *byte == b'-'
                    } else {
                        byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)
                    }
                })
                || frame.pi_session_id.is_empty()
                || frame.pi_session_id.len() > 1024
                || frame.pi_session_id.chars().count() > 256
                || frame.pi_session_id.chars().any(char::is_control)
                || frame.activity == Activity::Unknown
            {
                return Err("invalid activity snapshot".into());
            }
            match &frame.pi_session_name {
                serde_json::Value::Null => {}
                serde_json::Value::String(name) if name.chars().count() <= MAX_NAME => {}
                _ => return Err("invalid activity name".into()),
            }
            Ok(frame)
        }

        fn status(&self) -> Status {
            Status {
                pi_session_id: Some(self.pi_session_id.clone()),
                pi_session_name: self.pi_session_name.as_str().map(str::to_owned),
                activity: self.activity,
            }
        }
    }

    /// LF framing only. Retained buffer and every record are strictly bounded.
    #[derive(Default)]
    struct Framing(Vec<u8>);
    impl Framing {
        fn feed(
            &mut self,
            bytes: &[u8],
            mut record: impl FnMut(&[u8]) -> Result<()>,
        ) -> Result<()> {
            for byte in bytes {
                if *byte == b'\n' {
                    record(&self.0)?;
                    self.0.clear();
                } else {
                    if self.0.len() == MAX_FRAME {
                        return Err("oversized activity buffer".into());
                    }
                    self.0.push(*byte);
                }
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct Cached {
        status: Status,
        generation: Option<String>,
        sequence: u64,
        received: Option<Instant>,
        connected: bool,
        authorized: bool,
        terminated: bool,
    }
    impl Cached {
        fn status(&self, now: Instant) -> Status {
            let mut status = self.status.clone();
            if !self.authorized
                || !self.connected
                || self
                    .received
                    .is_none_or(|received| now.saturating_duration_since(received) >= STALE)
            {
                status.activity = Activity::Unknown;
            }
            status
        }
        fn invalidate(&mut self) {
            self.connected = false;
        }
        fn accept(&mut self, frame: &Frame, now: Instant) {
            if !self.authorized {
                return;
            }
            // A full verified snapshot replaces the name, including clears and
            // conversation switches; silence never clears a cached name.
            self.status = frame.status();
            self.generation = Some(frame.generation.clone());
            self.sequence = frame.sequence;
            self.received = Some(now);
            self.connected = true;
        }
    }

    struct Worker {
        metadata: Metadata,
        cache: Arc<Mutex<Cached>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }
    impl Worker {
        fn stop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
        fn join(mut self) {
            self.stop();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// At most 64 workers, independent sockets, bounded reads/backoff and cache.
    /// No manager mutex, Git subprocess, agent signal, or endpoint deletion here.
    pub struct Hub {
        instance: PathBuf,
        runtime: PathBuf,
        workers: Mutex<BTreeMap<String, Worker>>,
        reconciliation: Mutex<()>,
    }
    impl Hub {
        pub fn new(instance: &Path) -> Self {
            let runtime = crate::session_runtime::tmux_socket(instance)
                .ok()
                .and_then(|socket| socket.parent().map(Path::to_path_buf))
                .unwrap_or_default();
            Self {
                instance: instance.into(),
                runtime,
                workers: Mutex::new(BTreeMap::new()),
                reconciliation: Mutex::new(()),
            }
        }

        /// Only confirmed observations authorize freshness. Missing membership
        /// invalidates, but does not discard identity until runner completion.
        pub fn reconcile(&self, snapshot: &Snapshot) {
            let Ok(_reconciliation) = self.reconciliation.lock() else {
                return;
            };
            let Ok(mut workers) = self.workers.lock() else {
                return;
            };
            let mut known = if snapshot.error.is_none() && snapshot.supported {
                snapshot
                    .integrations
                    .iter()
                    .filter(|(_, metadata)| {
                        metadata.version == 1
                            && metadata.directory.parent() == Some(self.runtime.as_path())
                    })
                    .map(|(id, metadata)| (String::from(id.clone()), (id, metadata)))
                    .collect::<BTreeMap<_, _>>()
            } else {
                BTreeMap::new()
            };
            let mut retired = Vec::new();
            let mut previous = BTreeMap::new();
            let finished = workers
                .iter()
                .filter_map(|(key, worker)| {
                    worker
                        .cache
                        .lock()
                        .ok()
                        .filter(|cache| cache.terminated)
                        .map(|_| key.clone())
                })
                .collect::<Vec<_>>();
            for key in finished {
                known.remove(&key);
                if let Some(mut worker) = workers.remove(&key) {
                    worker.stop();
                    retired.push(worker);
                }
            }
            for (key, worker) in workers.iter_mut() {
                if let Ok(mut cache) = worker.cache.lock() {
                    cache.authorized = known.contains_key(key);
                    if !cache.authorized {
                        cache.invalidate();
                    }
                }
            }
            let changed = known
                .iter()
                .filter_map(|(key, (_, metadata))| {
                    workers
                        .get(key)
                        .filter(|worker| worker.metadata != **metadata)
                        .map(|_| key.clone())
                })
                .collect::<Vec<_>>();
            for key in changed {
                if let Some(mut worker) = workers.remove(&key) {
                    worker.stop();
                    let status = worker
                        .cache
                        .lock()
                        .map(|cache| cache.status.clone())
                        .unwrap_or_default();
                    previous.insert(key, status);
                    retired.push(worker);
                }
            }
            drop(workers);
            // Retire before spawning replacements: the live thread count never
            // exceeds the cap, even if every descriptor changes at once.
            for worker in retired {
                worker.join();
            }
            let Ok(mut workers) = self.workers.lock() else {
                return;
            };
            for (key, (id, metadata)) in known {
                if workers
                    .get(&key)
                    .is_some_and(|worker| worker.metadata == *metadata)
                {
                    continue;
                }
                let old_status = previous.remove(&key).unwrap_or_default();
                if workers.len() >= MAX_WORKERS {
                    continue;
                }
                let cache = Arc::new(Mutex::new(Cached {
                    status: old_status,
                    authorized: true,
                    ..Default::default()
                }));
                let stop = Arc::new(AtomicBool::new(false));
                let state = cache.clone();
                let stopping = stop.clone();
                let instance = self.instance.clone();
                let runtime = self.runtime.clone();
                let id = id.clone();
                let descriptor = metadata.clone();
                if let Ok(thread) =
                    thread::Builder::new()
                        .name("pi-activity".into())
                        .spawn(move || {
                            subscribe(&instance, &runtime, &id, &descriptor, &state, &stopping);
                        })
                {
                    workers.insert(
                        key,
                        Worker {
                            metadata: metadata.clone(),
                            cache,
                            stop,
                            thread: Some(thread),
                        },
                    );
                }
            }
        }

        /// Project only remote-safe state, reading monotonic freshness on demand.
        pub fn apply(&self, snapshot: &mut Snapshot) {
            let Ok(workers) = self.workers.lock() else {
                return;
            };
            let now = Instant::now();
            snapshot.activity = snapshot
                .sessions
                .iter()
                .filter_map(|session| {
                    let key = String::from(session.id.clone());
                    let worker = workers.get(&key)?;
                    let cache = worker.cache.lock().ok()?;
                    Some((session.id.clone(), cache.status(now)))
                })
                .collect();
        }
    }
    impl Drop for Hub {
        fn drop(&mut self) {
            if let Ok(workers) = self.workers.get_mut() {
                for worker in workers.values_mut() {
                    worker.stop();
                }
                for (_, worker) in std::mem::take(workers) {
                    worker.join();
                }
            }
        }
    }

    fn completed(runtime: &Path, instance: &Path, id: &SessionId) -> bool {
        let result = (|| -> Result<bool> {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(runtime.join(format!("a-{}.done", String::from(id.clone()))))?;
            let metadata = file.metadata()?;
            // SAFETY: geteuid only reads the effective identity.
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o7777 != 0o600
                || metadata.len() > 64 * 1024
            {
                return Ok(false);
            }
            let mut bytes = Vec::new();
            file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 64 * 1024 {
                return Ok(false);
            }
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            Ok(value["stopped"] == true
                && value["session_id"] == String::from(id.clone())
                && value["instance"].as_str() == instance.to_str())
        })();
        result.unwrap_or(false)
    }

    fn pause(stop: &AtomicBool, duration: Duration) {
        let deadline = Instant::now() + duration;
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn subscribe(
        instance: &Path,
        runtime: &Path,
        id: &SessionId,
        metadata: &Metadata,
        cache: &Mutex<Cached>,
        stop: &AtomicBool,
    ) {
        let path = metadata
            .directory
            .join(format!("a-{}.activity.sock", String::from(id.clone())));
        let mut backoff = Duration::from_millis(100);
        while !stop.load(Ordering::Relaxed) {
            if completed(runtime, instance, id) {
                if let Ok(mut cache) = cache.lock() {
                    cache.terminated = true;
                    cache.invalidate();
                }
                return;
            }
            let result = connection(instance, runtime, &path, id, cache, stop);
            if let Ok(mut cache) = cache.lock() {
                cache.invalidate();
            }
            if result.is_ok() {
                backoff = Duration::from_millis(100);
            }
            pause(stop, backoff);
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
    }

    fn connection(
        instance: &Path,
        runtime: &Path,
        path: &Path,
        id: &SessionId,
        cache: &Mutex<Cached>,
        stop: &AtomicBool,
    ) -> Result<()> {
        let (mut stream, directory, socket) = crate::control::activity_connection(path)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        writeln!(
            stream,
            "{}",
            serde_json::json!({"type":"subscribe", "version":1, "wumpa_session_id":id})
        )?;
        let mut framing = Framing::default();
        let mut bytes = [0; 4096];
        let mut last = Instant::now();
        let mut handshake = true;
        let mut generation: Option<String> = None;
        let mut sequence = 0;
        let mut window = Instant::now();
        let mut count = 0;
        let mut next_identity = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now.duration_since(last) >= if handshake { HANDSHAKE } else { STALE } {
                return Err("activity heartbeat expired".into());
            }
            if now >= next_identity {
                if completed(runtime, instance, id) {
                    if let Ok(mut cache) = cache.lock() {
                        cache.terminated = true;
                        cache.invalidate();
                    }
                    return Ok(());
                }
                crate::control::verify_activity_endpoint(path, &directory, &socket)?;
                next_identity = now + Duration::from_secs(1);
            }
            match stream.read(&mut bytes) {
                Ok(0) => return Err("activity connection closed".into()),
                Ok(size) => framing.feed(&bytes[..size], |bytes| {
                    if window.elapsed() >= Duration::from_secs(1) {
                        window = Instant::now();
                        count = 0;
                    }
                    count += 1;
                    if count > 64 {
                        return Err("activity rate limit".into());
                    }
                    let frame = Frame::parse(bytes, id)?;
                    if generation
                        .as_ref()
                        .is_some_and(|generation| *generation != frame.generation)
                        || frame.sequence <= sequence
                    {
                        return Err("unordered activity snapshot".into());
                    }
                    if handshake {
                        let old = cache.lock().map_err(|_| "activity cache unavailable")?;
                        if old.generation.as_ref() == Some(&frame.generation)
                            && frame.sequence <= old.sequence
                        {
                            return Err("replayed activity snapshot".into());
                        }
                    }
                    generation = Some(frame.generation.clone());
                    sequence = frame.sequence;
                    last = Instant::now();
                    handshake = false;
                    cache
                        .lock()
                        .map_err(|_| "activity cache unavailable")?
                        .accept(&frame, last);
                    Ok(())
                })?,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn id() -> SessionId {
            SessionId::try_from("a".repeat(32)).unwrap()
        }
        fn frame(sequence: u64) -> serde_json::Value {
            serde_json::json!({"type":"status","version":1,"generation":"12345678-1234-1234-1234-123456789abc","sequence":sequence,
                "wumpa_session_id":id(),"pi_session_id":"conversation","pi_session_name":"name","activity":"working"})
        }
        fn fixture() -> (
            tempfile::TempDir,
            Hub,
            Snapshot,
            std::os::unix::net::UnixListener,
            PathBuf,
        ) {
            use std::os::unix::fs::PermissionsExt;
            let directory = tempfile::tempdir().unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let instance = directory.path().join("c.sock");
            let runtime = directory.path().join(".c.sock.sessions");
            let resource = runtime.join("pi-test");
            std::fs::create_dir_all(&resource).unwrap();
            for path in [&runtime, &resource] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let path = resource.join(format!("a-{}.activity.sock", String::from(id())));
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            listener.set_nonblocking(true).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let observed = crate::checkout::Observation {
                path: "/repo".into(),
                device: 1,
                inode: 2,
            };
            let snapshot = Snapshot {
                sessions: vec![crate::sessions::Session {
                    id: id(),
                    instance: instance.clone(),
                    checkout: crate::sessions::CheckoutAssociation {
                        root: observed.clone(),
                        git_directory: observed.clone(),
                        common_directory: observed,
                    },
                    label: "fallback".into(),
                    state: crate::sessions::State::Running,
                }],
                integrations: vec![(
                    id(),
                    Metadata {
                        version: 1,
                        directory: resource,
                    },
                )],
                supported: true,
                ..Default::default()
            };
            (directory, Hub::new(&instance), snapshot, listener, path)
        }
        fn wait(mut predicate: impl FnMut() -> bool, timeout: Duration) {
            let deadline = Instant::now() + timeout;
            while !predicate() {
                assert!(Instant::now() < deadline, "activity condition timed out");
                thread::sleep(Duration::from_millis(10));
            }
        }
        fn peer(listener: &std::os::unix::net::UnixListener) -> std::os::unix::net::UnixStream {
            use std::io::BufRead;
            let mut accepted = None;
            wait(
                || match listener.accept() {
                    Ok((stream, _)) => {
                        accepted = Some(stream);
                        true
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
                    Err(error) => panic!("{error}"),
                },
                Duration::from_secs(5),
            );
            let stream = accepted.unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(&stream)
                .read_line(&mut line)
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap(),
                serde_json::json!({"type":"subscribe","version":1,"wumpa_session_id":id()})
            );
            stream
        }
        fn projected(hub: &Hub, snapshot: &Snapshot) -> crate::session_runtime::Summary {
            let mut value = snapshot.clone();
            hub.apply(&mut value);
            value.remote().sessions.remove(0)
        }
        fn send(stream: &mut std::os::unix::net::UnixStream, value: &serde_json::Value) {
            writeln!(stream, "{value}").unwrap();
        }

        #[test]
        fn pushed_updates_reconnect_ordering_and_unknown_reconciliation_preserve_identity() {
            let (_directory, hub, snapshot, listener, path) = fixture();
            hub.reconcile(&snapshot);
            let mut stream = peer(&listener);
            let mut initial = frame(1);
            initial["pi_session_name"] = serde_json::json!("Name \u{1b}[2J\u{2028}");
            // Split a frame across many writes, including multibyte Unicode.
            let bytes = format!("{initial}\n").into_bytes();
            for bytes in bytes.chunks(7) {
                stream.write_all(bytes).unwrap();
            }
            wait(
                || projected(&hub, &snapshot).activity == Activity::Working,
                Duration::from_secs(5),
            );
            let summary = projected(&hub, &snapshot);
            assert!(crate::output::clean(summary.display_name()).contains("\\u{2028}"));
            assert!(
                !serde_json::to_string(&summary)
                    .unwrap()
                    .contains(path.parent().unwrap().to_str().unwrap())
            );
            let unknown = Snapshot {
                error: Some("temporary discovery failure".into()),
                supported: true,
                ..Default::default()
            };
            hub.reconcile(&unknown);
            assert_eq!(hub.workers.lock().unwrap().len(), 1);
            assert_eq!(projected(&hub, &snapshot).activity, Activity::Unknown);
            assert_eq!(
                projected(&hub, &snapshot).pi_session_name,
                summary.pi_session_name
            );
            send(&mut stream, &frame(2)); // Unverified observations cannot refresh the cache.
            thread::sleep(Duration::from_millis(50));
            assert_eq!(
                projected(&hub, &snapshot).pi_session_name,
                summary.pi_session_name
            );
            hub.reconcile(&snapshot);
            send(&mut stream, &frame(3));
            wait(
                || projected(&hub, &snapshot).pi_session_name.as_deref() == Some("name"),
                Duration::from_secs(5),
            );
            send(&mut stream, &frame(3)); // Duplicate sequence must disconnect.
            wait(
                || projected(&hub, &snapshot).activity == Activity::Unknown,
                Duration::from_secs(5),
            );
            drop(stream);
            let mut replay = peer(&listener);
            send(&mut replay, &frame(1)); // Reconnect cannot replay the same generation.
            let mut byte = [0];
            assert_eq!(replay.read(&mut byte).unwrap(), 0);
            let mut replacement = peer(&listener);
            let mut next = frame(1);
            next["generation"] = serde_json::json!("abcdef12-1234-1234-1234-123456789abc");
            next["pi_session_id"] = serde_json::json!("new-conversation");
            next["pi_session_name"] = serde_json::Value::Null;
            next["activity"] = serde_json::json!("waiting_for_input");
            send(&mut replacement, &next);
            wait(
                || projected(&hub, &snapshot).activity == Activity::WaitingForInput,
                Duration::from_secs(5),
            );
            assert_eq!(projected(&hub, &snapshot).display_name(), "fallback");
            assert_eq!(
                projected(&hub, &snapshot).pi_session_id.as_deref(),
                Some("new-conversation")
            );
            let before = Instant::now();
            drop(hub);
            assert!(before.elapsed() < Duration::from_secs(2));
            assert!(path.exists()); // Observers never delete agent resources.
        }

        #[test]
        fn handshake_and_heartbeat_expire_without_inventing_idle() {
            let (_directory, hub, snapshot, listener, _path) = fixture();
            hub.reconcile(&snapshot);
            let mut stalled = peer(&listener);
            let mut byte = [0];
            assert_eq!(stalled.read(&mut byte).unwrap(), 0); // Three-second handshake deadline.
            let mut stream = peer(&listener);
            send(&mut stream, &frame(1));
            wait(
                || projected(&hub, &snapshot).activity == Activity::Working,
                Duration::from_secs(5),
            );
            wait(
                || projected(&hub, &snapshot).activity == Activity::Unknown,
                STALE + Duration::from_secs(3),
            );
            assert_eq!(
                projected(&hub, &snapshot).pi_session_name.as_deref(),
                Some("name")
            );
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
            let mut recovered = peer(&listener);
            send(&mut recovered, &frame(2));
            wait(
                || projected(&hub, &snapshot).activity == Activity::Working,
                Duration::from_secs(5),
            );
        }

        #[test]
        fn listener_replacement_security_and_verified_completion() {
            use std::os::unix::fs::PermissionsExt;
            let (_directory, hub, snapshot, listener, path) = fixture();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
            assert!(crate::control::activity_connection(&path).is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let alias = path.with_extension("alias");
            std::os::unix::fs::symlink(&path, &alias).unwrap();
            assert!(crate::control::activity_connection(&alias).is_err());
            hub.reconcile(&snapshot);
            let mut stream = peer(&listener);
            send(&mut stream, &frame(1));
            wait(
                || projected(&hub, &snapshot).activity == Activity::Working,
                Duration::from_secs(5),
            );
            std::fs::rename(&path, path.with_extension("old")).unwrap();
            let replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
            replacement.set_nonblocking(true).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let mut fresh = peer(&replacement);
            let mut next = frame(1);
            next["generation"] = serde_json::json!("abcdef12-1234-1234-1234-123456789abc");
            send(&mut fresh, &next);
            wait(
                || {
                    hub.workers
                        .lock()
                        .unwrap()
                        .values()
                        .next()
                        .unwrap()
                        .cache
                        .lock()
                        .unwrap()
                        .generation
                        .as_deref()
                        == Some("abcdef12-1234-1234-1234-123456789abc")
                },
                Duration::from_secs(5),
            );
            let completed_path = hub.runtime.join(format!("a-{}.done", String::from(id())));
            std::fs::write(
                &completed_path,
                serde_json::to_vec(
                    &serde_json::json!({"stopped":true,"session_id":id(),"instance":hub.instance}),
                )
                .unwrap(),
            )
            .unwrap();
            std::fs::set_permissions(&completed_path, std::fs::Permissions::from_mode(0o600))
                .unwrap();
            wait(
                || {
                    hub.workers
                        .lock()
                        .unwrap()
                        .values()
                        .next()
                        .unwrap()
                        .cache
                        .lock()
                        .unwrap()
                        .terminated
                },
                Duration::from_secs(5),
            );
            hub.reconcile(&Snapshot {
                supported: true,
                ..Default::default()
            });
            assert!(hub.workers.lock().unwrap().is_empty());
            assert!(path.exists());
        }

        #[test]
        fn worker_cap_malformed_peers_and_input_rate_are_bounded() {
            let (_directory, hub, mut snapshot, listener, _path) = fixture();
            hub.reconcile(&snapshot);
            let mut stream = peer(&listener);
            send(&mut stream, &frame(1));
            wait(
                || projected(&hub, &snapshot).activity == Activity::Working,
                Duration::from_secs(5),
            );
            stream.write_all(b"invalid json\n").unwrap();
            wait(
                || projected(&hub, &snapshot).activity == Activity::Unknown,
                Duration::from_secs(5),
            );
            drop(stream);
            let mut rapid = peer(&listener);
            let mut burst = Vec::new();
            for sequence in 2..75 {
                burst.extend_from_slice(format!("{}\n", frame(sequence)).as_bytes());
            }
            let _ = rapid.write_all(&burst);
            wait(
                || {
                    hub.workers
                        .lock()
                        .unwrap()
                        .values()
                        .next()
                        .unwrap()
                        .cache
                        .lock()
                        .unwrap()
                        .sequence
                        >= 2
                },
                Duration::from_secs(5),
            );
            wait(
                || projected(&hub, &snapshot).activity == Activity::Unknown,
                Duration::from_secs(5),
            );
            for number in 1..100 {
                snapshot.integrations.push((
                    SessionId::try_from(format!("{number:032x}")).unwrap(),
                    snapshot.integrations[0].1.clone(),
                ));
            }
            hub.reconcile(&snapshot);
            assert_eq!(hub.workers.lock().unwrap().len(), MAX_WORKERS);
            let before = Instant::now();
            drop(hub);
            assert!(before.elapsed() < Duration::from_secs(3));
        }

        #[test]
        fn strict_frames_and_lf_byte_bounds() {
            let valid = serde_json::to_vec(&frame(1)).unwrap();
            assert!(Frame::parse(&valid, &id()).is_ok());
            for (field, value) in [
                ("version", serde_json::json!(2)),
                ("sequence", serde_json::json!(0)),
                ("activity", serde_json::json!("unknown")),
                ("pi_session_name", serde_json::json!("x".repeat(513))),
                ("wumpa_session_id", serde_json::json!("b".repeat(32))),
                ("generation", serde_json::json!("invalid")),
            ] {
                let mut invalid = frame(1);
                invalid[field] = value;
                assert!(Frame::parse(&serde_json::to_vec(&invalid).unwrap(), &id()).is_err());
            }
            let mut missing = frame(1);
            missing.as_object_mut().unwrap().remove("pi_session_name");
            assert!(Frame::parse(&serde_json::to_vec(&missing).unwrap(), &id()).is_err());
            assert!(Frame::parse(&[0xff], &id()).is_err());
            let mut framing = Framing::default();
            let mut records = Vec::new();
            for byte in b"one\ntwo\n" {
                framing
                    .feed(&[*byte], |record| {
                        records.push(record.to_vec());
                        Ok(())
                    })
                    .unwrap();
            }
            assert_eq!(records, [b"one".to_vec(), b"two".to_vec()]);
            framing
                .feed("\u{2028}".as_bytes(), |_| panic!("not LF"))
                .unwrap();
            assert_eq!(framing.0.len(), 3);
            let mut framing = Framing::default();
            framing.feed(&vec![b'x'; MAX_FRAME], |_| Ok(())).unwrap();
            assert!(framing.feed(b"x", |_| Ok(())).is_err());
            framing
                .feed(b"\n", |record| {
                    assert_eq!(record.len(), MAX_FRAME);
                    Ok(())
                })
                .unwrap();
        }
        #[test]
        fn monotonic_freshness_and_cached_names_survive_disconnect_but_not_clear_or_switch() {
            let now = Instant::now();
            let parsed = Frame::parse(&serde_json::to_vec(&frame(1)).unwrap(), &id()).unwrap();
            let mut cache = Cached {
                authorized: true,
                ..Default::default()
            };
            cache.accept(&parsed, now);
            assert_eq!(cache.status(now).activity, Activity::Working);
            assert_eq!(cache.status(now + STALE).activity, Activity::Unknown);
            cache.invalidate();
            assert_eq!(cache.status(now).pi_session_name.as_deref(), Some("name"));
            assert_eq!(cache.status(now).activity, Activity::Unknown);
            let mut replacement = frame(2);
            replacement["pi_session_id"] = serde_json::json!("replacement");
            replacement["pi_session_name"] = serde_json::Value::Null;
            cache.accept(
                &Frame::parse(&serde_json::to_vec(&replacement).unwrap(), &id()).unwrap(),
                now,
            );
            assert_eq!(cache.status(now).pi_session_name, None);
            cache.authorized = false;
            cache.invalidate();
            cache.accept(&parsed, now);
            assert_eq!(cache.status(now).pi_session_name, None);
        }
    }
}
