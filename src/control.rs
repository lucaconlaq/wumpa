//! Local-only, read-only control handshake. This transport never loads configuration.

use std::path::Path;

use crate::Result;

/// Require explicit absolute instance selection on every platform.
pub fn validate_path(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err("--socket must be an absolute socket path".into());
    }
    Ok(())
}

#[cfg(unix)]
pub use unix::Listener;

#[cfg(not(unix))]
pub struct Listener;

#[cfg(not(unix))]
impl Listener {
    pub fn bind(_path: &Path) -> Result<Self> {
        Err("Unix control sockets require Linux or macOS".into())
    }

    pub fn check(&mut self) -> Result<()> {
        Err("Unix control sockets require Linux or macOS".into())
    }
}

#[cfg(not(unix))]
pub fn install_shutdown_handlers() -> Result<()> {
    Err("Unix control sockets require Linux or macOS".into())
}

#[cfg(not(unix))]
pub fn shutting_down() -> bool {
    false
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::{CStr, CString},
        fs::{self, File, Metadata, OpenOptions},
        io::{self, Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::{
                ffi::OsStrExt,
                fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
                net::{UnixListener, UnixStream},
            },
        },
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use serde::{Deserialize, Serialize};

    use crate::{Result, protocol};

    const VERSION: u32 = 1;
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
    const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);
    static SHUTDOWN: AtomicBool = AtomicBool::new(false);

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
    enum Request {
        Handshake { version: u32 },
    }

    /// Identity of one daemon run at a stable canonical instance path.
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Handshake {
        pub version: u32,
        pub socket: PathBuf,
        pub run_id: String,
    }

    fn user() -> u32 {
        // SAFETY: geteuid reads the process identity without modifying state.
        unsafe { libc::geteuid() }
    }

    fn owned_mode(metadata: &Metadata, uid: u32, mode: u32) -> Result<()> {
        if metadata.uid() != uid || metadata.mode() & 0o7777 != mode {
            return Err(format!("control endpoint must be user-owned with mode {mode:04o}").into());
        }
        Ok(())
    }

    fn canonical_path(path: &Path) -> Result<PathBuf> {
        super::validate_path(path)?;
        let parent = fs::canonicalize(path.parent().ok_or("socket has no parent")?)?;
        let metadata = fs::metadata(&parent)?;
        owned_mode(&metadata, user(), 0o700)?;
        if !metadata.is_dir() {
            return Err("socket parent is not a directory".into());
        }
        let canonical = parent.join(path.file_name().ok_or("socket has no name")?);
        if canonical.to_str().is_none() {
            return Err("canonical control socket path must be valid UTF-8".into());
        }
        Ok(canonical)
    }

    fn socket_metadata(path: &Path) -> Result<Metadata> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            return Err("control endpoint is not a socket (symlinks are not allowed)".into());
        }
        owned_mode(&metadata, user(), 0o600)?;
        Ok(metadata)
    }

    fn same_identity(a: &Metadata, b: &Metadata) -> bool {
        a.dev() == b.dev() && a.ino() == b.ino()
    }

    fn random_id() -> io::Result<String> {
        let mut random = [0; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    fn entry_metadata(directory: &File, name: &CStr) -> io::Result<libc::stat> {
        // SAFETY: stat is initialized by fstatat, using a live directory and name.
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                &mut metadata,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(metadata)
    }

    // Device and inode widths differ between Linux and macOS.
    #[allow(clippy::unnecessary_cast)]
    fn matches_socket(metadata: &libc::stat, identity: &Metadata) -> bool {
        metadata.st_mode & libc::S_IFMT == libc::S_IFSOCK
            && metadata.st_uid == user()
            && metadata.st_dev as u64 == identity.dev()
            && metadata.st_ino as u64 == identity.ino()
    }

    fn rename_entry(
        source_directory: &File,
        source: &CStr,
        target_directory: &File,
        target: &CStr,
        exclusive: bool,
    ) -> io::Result<()> {
        let result = if exclusive {
            #[cfg(target_os = "linux")]
            // SAFETY: all descriptors and strings are valid for renameat2.
            unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    source_directory.as_raw_fd(),
                    source.as_ptr(),
                    target_directory.as_raw_fd(),
                    target.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            }
            #[cfg(target_os = "macos")]
            // SAFETY: all descriptors and strings are valid for renameatx_np.
            unsafe {
                libc::renameatx_np(
                    source_directory.as_raw_fd(),
                    source.as_ptr(),
                    target_directory.as_raw_fd(),
                    target.as_ptr(),
                    libc::RENAME_EXCL,
                ) as libc::c_long
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "exclusive rename is unsupported",
            ));
        } else {
            // SAFETY: descriptors and strings remain live for this synchronous call.
            unsafe {
                libc::renameat(
                    source_directory.as_raw_fd(),
                    source.as_ptr(),
                    target_directory.as_raw_fd(),
                    target.as_ptr(),
                ) as libc::c_long
            }
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Capture before checking: never unlink an entry at the public endpoint.
    /// The callback lets tests replace the endpoint at the former race window.
    fn remove_socket(
        directory: &File,
        path: &Path,
        identity: &Metadata,
        before_capture: impl FnOnce(),
    ) -> Result<()> {
        let name = CString::new(path.file_name().ok_or("missing socket name")?.as_bytes())?;
        match entry_metadata(directory, &name) {
            Ok(metadata) if matches_socket(&metadata, identity) => {}
            Ok(_) => return Err("control endpoint changed; not removed".into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let quarantine_name = CString::new(format!(".wumpa-control-cleanup-{}", random_id()?))?;
        let quarantine_path = path
            .parent()
            .ok_or("missing socket parent")?
            .join(quarantine_name.to_str()?);
        // SAFETY: the directory descriptor and random name are valid.
        if unsafe { libc::mkdirat(directory.as_raw_fd(), quarantine_name.as_ptr(), 0o700) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let result = (|| -> Result<()> {
            // SAFETY: openat uses a live directory and name; flags reject symlinks.
            let raw = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    quarantine_name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error().into());
            }
            // SAFETY: raw is a new descriptor owned exclusively by this File.
            let quarantine = unsafe { File::from_raw_fd(raw) };
            let captured = c"endpoint";
            before_capture();
            rename_entry(directory, &name, &quarantine, captured, false)?;
            let metadata = entry_metadata(&quarantine, captured).map_err(|error| {
                format!(
                    "cannot inspect captured endpoint; preserved at {}: {error}",
                    quarantine_path.join("endpoint").display()
                )
            })?;
            if !matches_socket(&metadata, identity) {
                let restore = rename_entry(&quarantine, captured, directory, &name, true);
                return Err(match restore {
                    Ok(()) => "control endpoint changed during cleanup; replacement restored".into(),
                    Err(error) => format!("control endpoint changed during cleanup; replacement preserved at {}: {error}",
                        quarantine_path.join("endpoint").display()).into(),
                });
            }
            // The public name is now independent of this captured inode.
            // SAFETY: unlinkat targets only the verified entry in our private directory.
            if unsafe { libc::unlinkat(quarantine.as_raw_fd(), captured.as_ptr(), 0) } != 0 {
                return Err(format!(
                    "socket preserved at {}: {}",
                    quarantine_path.join("endpoint").display(),
                    io::Error::last_os_error()
                )
                .into());
            }
            Ok(())
        })();
        // Never recursively clean this directory: it may contain an unexpected
        // replacement. rmdir only succeeds when it is empty.
        // SAFETY: unlinkat targets the new directory, and only if it is empty.
        let removed = unsafe {
            libc::unlinkat(
                directory.as_raw_fd(),
                quarantine_name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        };
        if result.is_ok() && removed != 0 {
            return Err(format!(
                "cannot remove empty cleanup directory {}: {}",
                quarantine_path.display(),
                io::Error::last_os_error()
            )
            .into());
        }
        result
    }

    /// Bound retries for shared descriptor/resource pressure on either listener.
    pub fn accept_backoff(
        error: io::Error,
        failing_since: &mut Option<Instant>,
        retry_budget: Duration,
    ) -> io::Result<Duration> {
        match error.kind() {
            io::ErrorKind::WouldBlock => {
                *failing_since = None;
                return Ok(Duration::from_millis(10));
            }
            io::ErrorKind::Interrupted => return Ok(Duration::ZERO),
            _ => {}
        }
        if !matches!(
            error.raw_os_error(),
            Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM | libc::ECONNABORTED)
        ) {
            return Err(error);
        }
        let start = *failing_since.get_or_insert_with(|| {
            crate::output::error(format_args!("listener accept failed; retrying: {error}"));
            Instant::now()
        });
        if start.elapsed() >= retry_budget {
            return Err(error);
        }
        Ok(Duration::from_millis(50))
    }

    fn wait_for_connection(
        mut accept: impl FnMut() -> io::Result<UnixStream>,
        stopping: &AtomicBool,
        retry_budget: Duration,
    ) -> io::Result<Option<UnixStream>> {
        let mut failing_since = None;
        while !stopping.load(Ordering::Relaxed) {
            match accept() {
                Ok(stream) => return Ok(Some(stream)),
                Err(error) => {
                    thread::sleep(accept_backoff(error, &mut failing_since, retry_budget)?)
                }
            }
        }
        Ok(None)
    }

    fn endpoint_lock(directory: &File, path: &Path) -> Result<File> {
        let mut name = path
            .file_name()
            .ok_or("missing socket name")?
            .to_os_string();
        name.push(".lock");
        let name = CString::new(name.as_bytes())?;
        // SAFETY: openat uses a live directory/name; no symlinks are followed.
        let raw = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
                0o600,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: raw is a new descriptor owned exclusively by this File.
        let file = unsafe { File::from_raw_fd(raw) };
        let metadata = file.metadata()?;
        owned_mode(&metadata, user(), 0o600)?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err("unsafe control endpoint lock".into());
        }
        // SAFETY: flock acts on a live file descriptor and does not access memory.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("control endpoint is already locked by another daemon".into());
        }
        // Keep the lock file permanently: unlinking it would split the lock domain.
        Ok(file)
    }

    fn connect(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
        // SAFETY: zero is a valid initial representation of sockaddr_un.
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        #[cfg(target_os = "macos")]
        {
            address.sun_len = std::mem::size_of_val(&address) as u8;
        }
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid socket path",
            ));
        }
        for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
            *destination = *source as libc::c_char;
        }
        // SAFETY: socket returns an independent descriptor, immediately owned below.
        let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a newly allocated, uniquely owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let stream = UnixStream::from(fd);
        // SAFETY: fcntl only changes descriptor flags on this live, private socket.
        if unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        stream.set_nonblocking(true)?;
        // SAFETY: address points to an initialized sockaddr_un of the supplied size.
        let result = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(error);
            }
            let deadline = Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "control connect timed out",
                    ));
                }
                let mut poll = libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: poll points to one initialized entry with a live descriptor.
                let result =
                    unsafe { libc::poll(&mut poll, 1, remaining.as_millis().max(1) as i32) };
                if result > 0 {
                    if let Some(error) = stream.take_error()? {
                        return Err(error);
                    }
                    break;
                }
                if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        stream.set_nonblocking(false)?;
        Ok(stream)
    }

    fn peer(stream: &UnixStream) -> Result<()> {
        #[cfg(target_os = "linux")]
        let uid = {
            // SAFETY: zero initializes ucred; getsockopt fills the supplied buffer.
            let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
            let mut size = std::mem::size_of_val(&credentials) as libc::socklen_t;
            // SAFETY: all pointers refer to valid, appropriately sized buffers.
            let result = unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut credentials as *mut libc::ucred).cast(),
                    &mut size,
                )
            };
            if result != 0 || size as usize != std::mem::size_of_val(&credentials) {
                return Err("cannot verify control peer credentials".into());
            }
            credentials.uid
        };
        #[cfg(target_os = "macos")]
        let uid = {
            let mut uid = 0;
            let mut gid = 0;
            // SAFETY: getpeereid writes to two valid user/group ID buffers.
            if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
                return Err("cannot verify control peer credentials".into());
            }
            uid
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return Err("control peer verification is unsupported on this platform".into());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if uid != user() {
            return Err("control peer must be the same OS user".into());
        }
        Ok(())
    }

    struct Exchange<'a> {
        stream: &'a mut UnixStream,
        deadline: Instant,
    }

    impl Exchange<'_> {
        fn remaining(&self) -> io::Result<Duration> {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "control exchange timed out",
                ));
            }
            Ok(remaining)
        }
    }

    impl Read for Exchange<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.stream.set_read_timeout(Some(self.remaining()?))?;
            self.stream.read(bytes)
        }
    }

    impl Write for Exchange<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.stream.set_write_timeout(Some(self.remaining()?))?;
            self.stream.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.remaining()?;
            self.stream.flush()
        }
    }

    /// Internal client helper; no TCP fallback or configuration access.
    #[allow(dead_code)]
    pub fn handshake(path: &Path) -> Result<Handshake> {
        let path = canonical_path(path)?;
        let before = socket_metadata(&path)?;
        let mut stream = connect(&path, CONNECT_TIMEOUT)?;
        let deadline = Instant::now() + EXCHANGE_TIMEOUT;
        peer(&stream)?;
        if !same_identity(&before, &socket_metadata(&path)?) {
            return Err("control endpoint changed during connection".into());
        }
        let mut exchange = Exchange {
            stream: &mut stream,
            deadline,
        };
        protocol::write_message(&Request::Handshake { version: VERSION }, &mut exchange)?;
        let response: Handshake = protocol::read_message(&mut exchange)?;
        if response.version != VERSION
            || response.socket != path
            || response.run_id.len() != 32
            || !response.run_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("incompatible control handshake".into());
        }
        Ok(response)
    }

    /// Own the endpoint lock, listener worker, and identity-checked cleanup.
    pub struct Listener {
        path: PathBuf,
        identity: Metadata,
        _lock: File,
        directory: File,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<io::Result<()>>>,
    }

    impl Listener {
        /// Bind an explicitly selected private endpoint, recovering only verified stale sockets.
        pub fn bind(path: &Path) -> Result<Self> {
            let path = canonical_path(path)?;
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path.parent().ok_or("missing socket parent")?)?;
            owned_mode(&directory.metadata()?, user(), 0o700)?;
            let lock = endpoint_lock(&directory, &path)?;
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    let identity = socket_metadata(&path)?;
                    match connect(&path, CONNECT_TIMEOUT) {
                        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                            if !same_identity(&identity, &socket_metadata(&path)?) {
                                return Err("control endpoint changed during stale check".into());
                            }
                            remove_socket(&directory, &path, &identity, || {})?;
                        }
                        _ => {
                            return Err(
                                "control endpoint is active or cannot be verified stale".into()
                            );
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let listener = UnixListener::bind(&path)?;
            let identity = fs::symlink_metadata(&path)?;
            let stop = Arc::new(AtomicBool::new(false));
            let mut owned = Self {
                path,
                identity,
                _lock: lock,
                directory,
                stop,
                worker: None,
            };
            fs::set_permissions(&owned.path, fs::Permissions::from_mode(0o600))?;
            socket_metadata(&owned.path)?;
            listener.set_nonblocking(true)?;
            let response = Arc::new(Handshake {
                version: VERSION,
                socket: owned.path.clone(),
                run_id: random_id()?,
            });
            let stopping = owned.stop.clone();
            owned.worker = Some(
                thread::Builder::new()
                    .name("control-listener".into())
                    .spawn(move || {
                        let active = Arc::new(AtomicUsize::new(0));
                        while !stopping.load(Ordering::Relaxed) {
                            match wait_for_connection(
                                || listener.accept().map(|(stream, _)| stream),
                                &stopping,
                                Duration::from_secs(10),
                            )? {
                                Some(mut stream) => {
                                    if active.load(Ordering::Relaxed) >= 32 {
                                        continue;
                                    }
                                    active.fetch_add(1, Ordering::Relaxed);
                                    let count = active.clone();
                                    let response = response.clone();
                                    let result = thread::Builder::new()
                                        .name("control-handshake".into())
                                        .spawn(move || {
                                            let deadline = Instant::now() + EXCHANGE_TIMEOUT;
                                            let _ = (|| -> Result<()> {
                                                stream.set_nonblocking(false)?;
                                                peer(&stream)?;
                                                let mut exchange = Exchange {
                                                    stream: &mut stream,
                                                    deadline,
                                                };
                                                let Request::Handshake { version } =
                                                    protocol::read_message(&mut exchange)?;
                                                if version != VERSION {
                                                    return Err(
                                                        "incompatible control version".into()
                                                    );
                                                }
                                                protocol::write_message(
                                                    response.as_ref(),
                                                    &mut exchange,
                                                )
                                            })(
                                            );
                                            count.fetch_sub(1, Ordering::Relaxed);
                                        });
                                    if let Err(error) = result {
                                        active.fetch_sub(1, Ordering::Relaxed);
                                        crate::output::error(format_args!(
                                            "cannot start control handler: {error}"
                                        ));
                                    }
                                }
                                None => break,
                            }
                        }
                        Ok(())
                    })?,
            );
            Ok(owned)
        }

        /// Propagate listener failure so the daemon cannot silently lose control.
        pub fn check(&mut self) -> Result<()> {
            if self
                .worker
                .as_ref()
                .is_some_and(|worker| worker.is_finished())
            {
                let worker = self.worker.take().ok_or("missing control worker")?;
                match worker.join() {
                    Ok(Err(error)) => {
                        return Err(format!("control listener failed: {error}").into());
                    }
                    Ok(Ok(())) => return Err("control listener stopped unexpectedly".into()),
                    Err(_) => return Err("control listener panicked".into()),
                }
            }
            Ok(())
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            if let Err(error) = remove_socket(&self.directory, &self.path, &self.identity, || {}) {
                crate::output::error(format_args!("control socket cleanup failed: {error}"));
            }
        }
    }

    extern "C" fn shutdown_signal(_: libc::c_int) {
        SHUTDOWN.store(true, Ordering::Relaxed);
    }

    /// Request orderly endpoint cleanup on foreground Ctrl-C or service stop.
    pub fn install_shutdown_handlers() -> Result<()> {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            // SAFETY: the handler only stores to a lock-free atomic boolean.
            if unsafe { libc::signal(signal, shutdown_signal as *const () as libc::sighandler_t) }
                == libc::SIG_ERR
            {
                return Err(io::Error::last_os_error().into());
            }
        }
        Ok(())
    }

    /// Whether SIGINT or SIGTERM requested orderly daemon shutdown.
    pub fn shutting_down() -> bool {
        SHUTDOWN.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::symlink;

        fn directory() -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            dir
        }

        #[test]
        fn aliases_instances_restart_and_permissions() {
            let dir = directory();
            let alias = dir.path().join("alias");
            symlink(dir.path(), &alias).unwrap();
            let path = dir.path().join("control");
            let other_path = dir.path().join("other");
            let listener = Listener::bind(&alias.join("control")).unwrap();
            let other = Listener::bind(&other_path).unwrap();
            let first = handshake(&path).unwrap();
            fs::create_dir(dir.path().join("child")).unwrap();
            let second = handshake(&alias.join("child/../control")).unwrap();
            assert_eq!(first.socket, fs::canonicalize(&path).unwrap());
            assert_eq!(first.run_id, second.run_id);
            let independent = handshake(&other_path).unwrap();
            assert_ne!(first.socket, independent.socket);
            assert_ne!(first.run_id, independent.run_id);
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
            drop(listener);
            assert!(!path.exists());
            let restarted = Listener::bind(&path).unwrap();
            let next = handshake(&path).unwrap();
            assert_eq!(first.socket, next.socket);
            assert_ne!(first.run_id, next.run_id);
            drop(restarted);
            drop(other);
        }

        #[test]
        fn paths_directory_modes_and_ownership_fail_closed() {
            let dir = directory();
            assert!(Listener::bind(Path::new("relative")).is_err());
            assert!(handshake(&dir.path().join("missing")).is_err());
            let metadata = fs::metadata(dir.path()).unwrap();
            assert!(owned_mode(&metadata, user().wrapping_add(1), 0o700).is_err());
            for mode in [0o755, 0o770, 0o1700] {
                fs::set_permissions(dir.path(), fs::Permissions::from_mode(mode)).unwrap();
                assert!(Listener::bind(&dir.path().join("control")).is_err());
            }
        }

        #[test]
        fn lock_collisions_and_stale_recovery() {
            let dir = directory();
            let path = dir.path().join("control");
            let listener = Listener::bind(&path).unwrap();
            let identity = fs::symlink_metadata(&path).unwrap();
            let attempt = path.clone();
            assert!(
                thread::spawn(move || Listener::bind(&attempt).is_err())
                    .join()
                    .unwrap()
            );
            assert!(same_identity(
                &identity,
                &fs::symlink_metadata(&path).unwrap()
            ));
            assert!(handshake(&path).is_ok());
            drop(listener);
            let external = UnixListener::bind(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(Listener::bind(&path).is_err());
            drop(external);
            let recovered = Listener::bind(&path).unwrap();
            assert!(handshake(&path).is_ok());
            drop(recovered);
        }

        #[test]
        fn never_removes_files_symlinks_or_replacement_socket() {
            let dir = directory();
            let path = dir.path().join("control");
            fs::write(&path, b"keep").unwrap();
            assert!(Listener::bind(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), b"keep");
            fs::remove_file(&path).unwrap();
            let target = dir.path().join("target");
            fs::write(&target, b"target").unwrap();
            symlink(&target, &path).unwrap();
            assert!(Listener::bind(&path).is_err());
            assert!(handshake(&path).is_err());
            assert!(
                fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            fs::remove_file(&path).unwrap();
            let listener = Listener::bind(&path).unwrap();
            fs::remove_file(&path).unwrap();
            let replacement = UnixListener::bind(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let identity = fs::symlink_metadata(&path).unwrap();
            drop(listener);
            assert!(same_identity(
                &identity,
                &fs::symlink_metadata(&path).unwrap()
            ));
            drop(replacement);
        }

        fn exchange(path: &Path, bytes: &[u8]) -> Vec<u8> {
            let mut stream = UnixStream::connect(path).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(6)))
                .unwrap();
            let _ = stream.write_all(bytes);
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response);
            response
        }

        #[test]
        fn capture_race_restores_files_symlinks_and_live_sockets() {
            let dir = directory();
            let directory = File::open(dir.path()).unwrap();
            let path = dir.path().join("control");
            let target = dir.path().join("target");
            fs::write(&target, b"target").unwrap();
            for kind in ["file", "symlink", "socket"] {
                let original = UnixListener::bind(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                let identity = fs::symlink_metadata(&path).unwrap();
                let mut replacement = None;
                let result = remove_socket(&directory, &path, &identity, || {
                    fs::remove_file(&path).unwrap();
                    match kind {
                        "file" => fs::write(&path, b"keep").unwrap(),
                        "symlink" => symlink(&target, &path).unwrap(),
                        "socket" => {
                            replacement = Some(UnixListener::bind(&path).unwrap());
                            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                        }
                        _ => unreachable!(),
                    }
                });
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("replacement restored")
                );
                match kind {
                    "file" => assert_eq!(fs::read(&path).unwrap(), b"keep"),
                    "symlink" => assert_eq!(fs::read_link(&path).unwrap(), target),
                    "socket" => {
                        assert!(UnixStream::connect(&path).is_ok());
                    }
                    _ => unreachable!(),
                }
                assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".wumpa-control-cleanup-")
                }));
                drop(replacement);
                drop(original);
                fs::remove_file(&path).unwrap();
            }
        }

        #[test]
        fn exclusive_restore_never_overwrites_a_new_endpoint() {
            let dir = directory();
            let directory = File::open(dir.path()).unwrap();
            let source = c"captured";
            let target = c"endpoint";
            fs::write(dir.path().join("captured"), b"preserve").unwrap();
            fs::write(dir.path().join("endpoint"), b"newer").unwrap();
            assert!(rename_entry(&directory, source, &directory, target, true).is_err());
            assert_eq!(fs::read(dir.path().join("captured")).unwrap(), b"preserve");
            assert_eq!(fs::read(dir.path().join("endpoint")).unwrap(), b"newer");
        }

        #[test]
        fn cleanup_uses_the_open_directory_after_parent_replacement() {
            let dir = directory();
            let parent = dir.path().join("runtime");
            fs::create_dir(&parent).unwrap();
            let directory = File::open(&parent).unwrap();
            let path = parent.join("control");
            let original = UnixListener::bind(&path).unwrap();
            let identity = fs::symlink_metadata(&path).unwrap();
            let moved = dir.path().join("moved");
            fs::rename(&parent, &moved).unwrap();
            fs::create_dir(&parent).unwrap();
            fs::write(&path, b"keep").unwrap();
            remove_socket(&directory, &path, &identity, || {}).unwrap();
            assert!(!moved.join("control").exists());
            assert_eq!(fs::read(&path).unwrap(), b"keep");
            drop(original);
        }

        #[test]
        fn non_utf8_canonical_paths_fail_before_creating_runtime_files() {
            use std::ffi::OsStr;
            let dir = directory();
            let path = dir.path().join(OsStr::from_bytes(b"control-\xff"));
            assert!(
                Listener::bind(&path)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("UTF-8")
            );
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
            let parent = dir.path().join(OsStr::from_bytes(b"runtime-\xff"));
            fs::create_dir(&parent).unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
            let alias = dir.path().join("alias");
            symlink(&parent, &alias).unwrap();
            assert!(
                Listener::bind(&alias.join("control"))
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("UTF-8")
            );
            assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        }

        #[test]
        fn accepted_stream_waits_for_delayed_and_fragmented_requests() {
            let dir = directory();
            let path = dir.path().join("control");
            let _listener = Listener::bind(&path).unwrap();
            let mut stream = UnixStream::connect(&path).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut writer = stream.try_clone().unwrap();
            let sending = thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                writer.write_all(b"{\"action\":\"handshake\",").unwrap();
                thread::sleep(Duration::from_millis(100));
                writer.write_all(b"\"version\":1}\n").unwrap();
            });
            let response: Handshake = protocol::read_message(&mut stream).unwrap();
            assert_eq!(response.version, VERSION);
            sending.join().unwrap();
        }

        #[test]
        fn accept_retries_pressure_but_fatal_and_persistent_errors_propagate() {
            let stopping = AtomicBool::new(false);
            let (stream, _peer) = UnixStream::pair().unwrap();
            let mut stream = Some(stream);
            let mut calls = 0;
            let result = wait_for_connection(
                || {
                    calls += 1;
                    if calls == 1 {
                        Err(io::Error::from_raw_os_error(libc::EMFILE))
                    } else {
                        Ok(stream.take().unwrap())
                    }
                },
                &stopping,
                Duration::from_secs(1),
            );
            assert!(result.unwrap().is_some());
            assert_eq!(calls, 2);
            assert!(
                wait_for_connection(
                    || Err(io::Error::from_raw_os_error(libc::EINVAL)),
                    &stopping,
                    Duration::from_secs(1)
                )
                .is_err()
            );
            let start = Instant::now();
            assert!(
                wait_for_connection(
                    || Err(io::Error::from_raw_os_error(libc::EMFILE)),
                    &stopping,
                    Duration::from_millis(20)
                )
                .is_err()
            );
            assert!(start.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn supervision_reports_listener_failure() {
            let dir = directory();
            let path = dir.path().join("control");
            let mut listener = Listener::bind(&path).unwrap();
            listener.stop.store(true, Ordering::Relaxed);
            listener.worker.take().unwrap().join().unwrap().unwrap();
            listener.stop.store(false, Ordering::Relaxed);
            listener.worker = Some(thread::spawn(|| {
                Err(io::Error::from_raw_os_error(libc::EINVAL))
            }));
            let deadline = Instant::now() + Duration::from_secs(1);
            while !listener.worker.as_ref().unwrap().is_finished() {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
            assert!(
                listener
                    .check()
                    .unwrap_err()
                    .to_string()
                    .contains("control listener failed")
            );
        }

        #[test]
        fn malformed_oversized_incompatible_and_one_exchange() {
            let dir = directory();
            let path = dir.path().join("control");
            let _listener = Listener::bind(&path).unwrap();
            for input in [
                b"not json\n".as_slice(),
                b"{\"action\":\"handshake\",\"version\":2}\n",
                b"{\"action\":\"list\"}\n",
            ] {
                assert!(exchange(&path, input).is_empty());
            }
            let mut oversized = vec![b' '; protocol::MAX_MESSAGE as usize];
            oversized.push(b'\n');
            assert!(exchange(&path, &oversized).is_empty());
            let response = exchange(&path, b"{\"action\":\"handshake\",\"version\":1}\n{\"action\":\"handshake\",\"version\":1}\n");
            assert_eq!(response.iter().filter(|&&byte| byte == b'\n').count(), 1);
            assert!(protocol::read_message::<Handshake>(response.as_slice()).is_ok());
        }

        #[test]
        fn client_rejects_incompatible_responses_and_socket_modes() {
            let dir = directory();
            let path = dir.path().join("mock");
            let mock = UnixListener::bind(&path).unwrap();
            assert!(handshake(&path).is_err());
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let thread = thread::spawn(move || {
                let (mut stream, _) = mock.accept().unwrap();
                let _: Request = protocol::read_message(&mut stream).unwrap();
                stream
                    .write_all(b"{\"version\":2,\"socket\":\"/wrong\",\"run_id\":\"bad\"}\n")
                    .unwrap();
            });
            assert!(handshake(&path).is_err());
            thread.join().unwrap();
        }

        #[test]
        fn total_deadline_does_not_reset_with_incoming_bytes() {
            let (mut reader, mut writer) = UnixStream::pair().unwrap();
            let sending = thread::spawn(move || {
                for _ in 0..20 {
                    if writer.write_all(b" ").is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            });
            let start = Instant::now();
            let mut exchange = Exchange {
                stream: &mut reader,
                deadline: start + Duration::from_millis(100),
            };
            assert!(protocol::read_message::<Request>(&mut exchange).is_err());
            assert!(start.elapsed() < Duration::from_millis(350));
            drop(reader);
            sending.join().unwrap();
        }

        #[test]
        fn blocked_writes_obey_the_exchange_deadline() {
            let (mut writer, _reader) = UnixStream::pair().unwrap();
            let start = Instant::now();
            let mut exchange = Exchange {
                stream: &mut writer,
                deadline: start + Duration::from_millis(100),
            };
            assert!(
                exchange
                    .write_all(&vec![b' '; protocol::MAX_MESSAGE as usize])
                    .is_err()
            );
            assert!(start.elapsed() < Duration::from_secs(2));
        }

        #[test]
        fn client_slow_response_has_overall_five_second_deadline() {
            let dir = directory();
            let path = dir.path().join("mock");
            let mock = UnixListener::bind(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let sending = thread::spawn(move || {
                let (mut stream, _) = mock.accept().unwrap();
                let _: Request = protocol::read_message(&mut stream).unwrap();
                for _ in 0..30 {
                    if stream.write_all(b" ").is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(250));
                }
            });
            let start = Instant::now();
            assert!(handshake(&path).is_err());
            assert!(start.elapsed() >= Duration::from_secs(4));
            assert!(start.elapsed() < Duration::from_secs(7));
            sending.join().unwrap();
        }

        #[test]
        fn server_slow_input_has_overall_five_second_deadline() {
            let dir = directory();
            let path = dir.path().join("control");
            let _listener = Listener::bind(&path).unwrap();
            let mut stream = UnixStream::connect(&path).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut sending = stream.try_clone().unwrap();
            let sender = thread::spawn(move || {
                for _ in 0..30 {
                    if sending.write_all(b" ").is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(250));
                }
            });
            let start = Instant::now();
            let mut byte = [0];
            loop {
                match stream.read(&mut byte) {
                    Ok(0) => break,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        assert!(start.elapsed() < Duration::from_secs(7));
                    }
                    Err(_) => break,
                    Ok(_) => panic!("unexpected response"),
                }
            }
            assert!(start.elapsed() >= Duration::from_secs(4));
            assert!(start.elapsed() < Duration::from_secs(7));
            sender.join().unwrap();
        }
    }
}

#[cfg(unix)]
pub use unix::{accept_backoff, install_shutdown_handlers, shutting_down};
