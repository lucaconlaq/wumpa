use std::{
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    Result,
    config::{self, Repository, ServerConfig},
    protocol::{Request, Response, read_message, write_message},
    repository,
};

struct DeadlineReader<'a> {
    stream: &'a mut TcpStream,
    deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request timed out",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        // Preserve any bytes after the newline for clone cancellation checks.
        if bytes.is_empty() {
            return Ok(0);
        }
        self.stream.read(&mut bytes[..1])
    }
}

fn apply(request: Request, path: &Path, config: &mut ServerConfig) -> Result<Option<PathBuf>> {
    if let Request::PrepareClone {
        version,
        url,
        folder_name,
        agent_socket,
    } = &request
    {
        if *version != crate::protocol::HELPER_VERSION {
            return Err("incompatible clone protocol; update both client and server".into());
        }
        // Never fall back to the daemon's agent when the helper supplied a path.
        // Preflight validates the endpoint only; cloning passes it exclusively
        // through the Git child's environment, never through persisted metadata.
        let _socket = match agent_socket {
            Some(socket) => crate::agent::validate(socket)?,
            None => crate::agent::from_environment()?,
        };
        return repository::destination(config, url, folder_name.as_deref()).map(Some);
    }
    if let Request::Add { url } = request {
        // Preflight only: cloning will repeat this check and publish atomically.
        repository::destination(config, &url, None)?;
        let url = url.trim();
        if config.repositories.iter().any(|entry| entry.url == url) {
            return Err("repository is already saved".into());
        }
        if config.repositories.len() >= 100 {
            return Err("this prototype supports at most 100 repositories".into());
        }
        let mut next = config.clone();
        next.repositories.push(Repository {
            url: url.into(),
            checkout_path: None,
        });
        config::save(path, &next)?;
        *config = next;
    }
    Ok(None)
}

/// Serve bounded loopback requests with a five-second message-read deadline.
/// Persist configuration before optionally publishing detached startup readiness.
pub fn serve(port: u16, socket: &Path, ready_file: Option<&Path>) -> Result<()> {
    let shared_config = Arc::new(Mutex::new(ServerConfig::default()));
    let mut control = crate::control::Listener::bind_with_config(socket, shared_config.clone())?;
    crate::control::install_shutdown_handlers()?;
    let control_socket = socket.canonicalize()?;
    let path = config::path("server")?;
    let mut config: ServerConfig = config::load(&path)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    config.initialize(std::env::var_os("HOME").as_deref())?;
    config::save(&path, &config)?;
    let ready = crate::daemon::Ready {
        pid: std::process::id(),
        port: listener.local_addr()?.port(),
        repositories: config.repositories.len(),
    };
    let mut log_name = path.as_os_str().to_os_string();
    log_name.push(".log");
    let log_path = std::path::PathBuf::from(log_name);

    for entry in std::fs::read_dir(config.repository_dir.as_ref().unwrap())? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".wumpa-clone-")
        {
            crate::output::error(format_args!(
                "unregistered clone staging path: {}; inspect manually, not automatically removed",
                entry.path().display()
            ));
        }
    }
    *shared_config
        .lock()
        .map_err(|_| "configuration lock poisoned")? = config;
    let config = shared_config;
    crate::daemon::banner(&ready, &path, ready_file.map(|_| log_path.as_path()));
    std::io::stdout().flush()?;
    if let Some(ready_file) = ready_file {
        config::save(ready_file, &ready)?;
    }
    let cloning = Arc::new(Mutex::new(()));
    let active = Arc::new(AtomicUsize::new(0));
    listener.set_nonblocking(true)?;
    #[cfg(unix)]
    let mut failing_since = None;
    while !crate::control::shutting_down() {
        control.check()?;
        let stream = match listener.accept() {
            Ok((stream, _)) => {
                #[cfg(unix)]
                {
                    failing_since = None;
                }
                stream.set_nonblocking(false)?;
                stream
            }
            Err(error) => {
                #[cfg(unix)]
                {
                    std::thread::sleep(crate::control::accept_backoff(
                        error,
                        &mut failing_since,
                        Duration::from_secs(10),
                    )?);
                    continue;
                }
                #[cfg(not(unix))]
                return Err(error.into());
            }
        };
        if active.load(Ordering::Relaxed) >= 32 {
            drop(stream);
            continue;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let active = active.clone();
        let config = config.clone();
        let cloning = cloning.clone();
        let path = path.clone();
        let sessions = control.session_snapshot_reader();
        let control_socket = control_socket.clone();
        std::thread::Builder::new()
            .name("request".into())
            .spawn(move || {
                struct Active(Arc<AtomicUsize>);
                impl Drop for Active {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::Relaxed);
                    }
                }
                let _active = Active(active);
                let result = (|| -> Result<()> {
                    let mut stream = stream;
                    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                    let reader = DeadlineReader {
                        stream: &mut stream,
                        deadline: Instant::now() + Duration::from_secs(5),
                    };
                    let request = read_message::<Request>(reader);
                    let preflight = matches!(
                        &request,
                        Ok(Request::PrepareClone { .. } | Request::Clone { .. })
                    );
                    let (destination, error) = match request
                        .and_then(|request| handle(request, &path, &config, &cloning, &stream))
                    {
                        Ok(destination) => (destination, None),
                        Err(error) => (None, Some(error.to_string())),
                    };
                    let config = config.lock().map_err(|_| "configuration lock poisoned")?;
                    let mut response = Response {
                        control_socket: Some(control_socket),
                        repository_dir: config.repository_dir.clone(),
                        home_dir: std::env::var_os("HOME")
                            .map(PathBuf::from)
                            .filter(|path| path.is_absolute()),
                        checkouts: config.repositories.clone(),
                        repositories: config
                            .repositories
                            .iter()
                            .map(|repository| repository.url.clone())
                            .collect(),
                        error,
                        sessions: None,
                        worktrees: Vec::new(),
                        preflight: preflight.then_some(crate::protocol::Preflight {
                            version: crate::protocol::HELPER_VERSION,
                            destination,
                        }),
                    };
                    drop(config);
                    response.worktrees = crate::worktrees::discover(&response.checkouts);
                    response.sessions = Some(sessions().remote());
                    if let Some(snapshot) = response.sessions.as_mut() {
                        for agent in &mut snapshot.sessions {
                            if let Some(path) = response
                                .checkouts
                                .iter()
                                .filter_map(|repository| repository.checkout_path.as_ref())
                                .find(|path| {
                                    path.canonicalize()
                                        .is_ok_and(|canonical| canonical == agent.checkout)
                                })
                            {
                                agent.checkout = path.clone();
                            }
                        }
                        snapshot.limit();
                    }
                    write_message(&response, &mut stream)
                })();
                if let Err(error) = result {
                    crate::output::error(format_args!("request failed: {error}"));
                }
            })?;
    }
    Ok(())
}

fn handle(
    request: Request,
    path: &Path,
    config: &Mutex<ServerConfig>,
    cloning: &Mutex<()>,
    stream: &TcpStream,
) -> Result<Option<PathBuf>> {
    let Request::Clone {
        version,
        url,
        folder_name,
        agent_socket,
    } = request
    else {
        // Reserve metadata capacity and URL/path names while a clone is running.
        let _reservation = if matches!(request, Request::Add { .. }) {
            Some(
                cloning
                    .try_lock()
                    .map_err(|_| "a clone is already running; retry later")?,
            )
        } else {
            None
        };
        return apply(
            request,
            path,
            &mut *config.lock().map_err(|_| "configuration lock poisoned")?,
        );
    };
    if version != crate::protocol::HELPER_VERSION {
        return Err("incompatible clone protocol; update both client and server".into());
    }
    let _reservation = cloning
        .try_lock()
        .map_err(|_| "a clone is already running; retry later")?;
    let socket = match agent_socket {
        Some(socket) => crate::agent::validate(&socket)?,
        None => crate::agent::from_environment()?,
    };
    let destination = repository::destination(
        &*config.lock().map_err(|_| "configuration lock poisoned")?,
        &url,
        folder_name.as_deref(),
    )?;
    let root = destination.parent().ok_or("missing repository root")?;
    let temporary = tempfile::Builder::new()
        .prefix(".wumpa-clone-")
        .tempdir_in(root)?;
    let checkout = temporary.path().join("checkout");
    stream.set_nonblocking(true)?;
    let cancelled = || match stream.peek(&mut [0]) {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
        // EOF, unexpected extra input, or a broken connection cancel the operation.
        _ => true,
    };
    let result = (|| -> Result<()> {
        crate::clone::run(url.trim(), &checkout, &socket, cancelled)?;
        if cancelled() {
            return Err("clone cancelled".into());
        }
        let mut config = config.lock().map_err(|_| "configuration lock poisoned")?;
        // Repeat validation immediately before the atomic no-clobber publication.
        repository::destination(&config, &url, folder_name.as_deref())?;
        persist_checkout(path, &mut config, url.trim(), &checkout, &destination)
    })();
    let blocking = stream.set_nonblocking(false);
    let staging_path = temporary.path().to_path_buf();
    let cleanup = temporary.close();
    if let Err(error) = cleanup {
        return Err(format!(
            "{}; staging cleanup failed under {}: {error}",
            result
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "clone saved".into()),
            staging_path.display()
        )
        .into());
    }
    blocking?;
    result?;
    Ok(Some(destination))
}

fn persist_checkout(
    path: &Path,
    config: &mut ServerConfig,
    url: &str,
    checkout: &Path,
    destination: &Path,
) -> Result<()> {
    let mut next = config.clone();
    if let Some(entry) = next
        .repositories
        .iter_mut()
        .find(|entry| entry.url.trim() == url)
    {
        entry.checkout_path = Some(destination.to_path_buf());
    } else {
        next.repositories.push(Repository {
            url: url.into(),
            checkout_path: Some(destination.to_path_buf()),
        });
    }
    let identity = std::fs::symlink_metadata(checkout)?;
    crate::clone::publish(checkout, destination)?;
    if let Err(error) = config::save(path, &next) {
        if let Err(cleanup) = crate::clone::rollback(destination, &identity) {
            return Err(format!("configuration save failed: {error}; rollback failed: {cleanup}; leftover checkout: {}", destination.display()).into());
        }
        return Err(format!("configuration save failed; checkout rolled back: {error}").into());
    }
    *config = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_persists_and_rejects_duplicates_and_empty_urls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        let mut config = ServerConfig::default();
        config.initialize(Some(dir.path().as_os_str())).unwrap();
        let root = config.repository_dir.clone();
        apply(
            Request::Add {
                url: " ssh://git@example.com/app.git ".into(),
            },
            &path,
            &mut config,
        )
        .unwrap();
        assert_eq!(
            config::load::<ServerConfig>(&path).unwrap().repositories,
            config.repositories
        );
        for url in [
            "ssh://git@example.com/app.git",
            " ",
            "https://example.com/app.git",
        ] {
            assert!(apply(Request::Add { url: url.into() }, &path, &mut config).is_err());
        }
        assert_eq!(config.repositories.len(), 1);
        assert_eq!(config.repository_dir, root);
        assert_eq!(
            config::load::<ServerConfig>(&path).unwrap().repository_dir,
            root
        );
        assert!(config.repositories[0].checkout_path.is_none());
    }

    #[test]
    fn rejected_adds_leave_config_and_existing_paths_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        let mut config = ServerConfig::default();
        config.initialize(Some(dir.path().as_os_str())).unwrap();
        config::save(&path, &config).unwrap();
        let before = std::fs::read(&path).unwrap();
        let checkout = dir.path().join("app");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::write(checkout.join("keep"), "contents").unwrap();
        for url in [
            "ssh://host/app.git",
            "ssh://host/app.git\n",
            "ssh://user:secret@host/other.git",
            "https://host/other.git",
            "git@host:other.git",
        ] {
            assert!(apply(Request::Add { url: url.into() }, &path, &mut config).is_err());
            assert!(config.repositories.is_empty());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        assert_eq!(
            std::fs::read_to_string(checkout.join("keep")).unwrap(),
            "contents"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preflight_validates_each_agent_without_persisting_or_changing_environment() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        let mut config = ServerConfig::default();
        config.initialize(Some(dir.path().as_os_str())).unwrap();
        config::save(&path, &config).unwrap();
        let before = std::fs::read(&path).unwrap();
        let environment = std::env::var_os("SSH_AUTH_SOCK");
        for name in ["first", "second"] {
            let socket = dir.path().join(name);
            let listener = UnixListener::bind(&socket).unwrap();
            let request = || Request::PrepareClone {
                version: crate::protocol::HELPER_VERSION,
                url: "ssh://host/app.git".into(),
                folder_name: None,
                agent_socket: Some(socket.clone()),
            };
            assert_eq!(
                apply(request(), &path, &mut config).unwrap(),
                Some(config.repository_dir.as_ref().unwrap().join("app"))
            );
            drop(listener);
            std::fs::remove_file(&socket).unwrap();
            assert!(apply(request(), &path, &mut config).is_err());
        }
        assert_eq!(std::env::var_os("SSH_AUTH_SOCK"), environment);
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(config.repositories.is_empty());
    }

    #[test]
    fn failed_save_does_not_change_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = ServerConfig::default();
        config.initialize(Some(dir.path().as_os_str())).unwrap();
        assert!(
            apply(
                Request::Add {
                    url: "ssh://host/app.git".into()
                },
                dir.path(),
                &mut config
            )
            .is_err()
        );
        assert!(config.repositories.is_empty());
    }

    #[test]
    fn trickling_input_cannot_extend_the_deadline() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let sender = std::thread::spawn(move || {
            for _ in 0..100 {
                if client.write_all(b" ").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let started = Instant::now();
        let result = read_message::<Request>(DeadlineReader {
            stream: &mut stream,
            deadline: started + Duration::from_millis(100),
        });
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(750));
        drop(stream);
        sender.join().unwrap();
    }
}
