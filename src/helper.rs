//! One-shot remote helper. Its stdin stays open for the lifetime of the SSH session.

use crate::{Result, protocol};

/// Relay a versioned preflight or clone using this SSH session's forwarded agent.
/// Called only by the helper CLI process; its stdin watcher ends at process exit.
#[cfg(unix)]
pub fn run() -> Result<()> {
    use std::{
        io::Read,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    let result = (|| -> Result<protocol::Response> {
        let input: protocol::HelperRequest = protocol::read_message(SessionInput {
            deadline: Instant::now() + Duration::from_secs(5),
        })?;
        if input.version != protocol::HELPER_VERSION {
            return Err("incompatible clone helper; update both client and server".into());
        }
        if input.port == 0 {
            return Err("server port must be between 1 and 65535".into());
        }
        crate::repository::folder_name(&input.url, input.folder_name.as_deref())?;
        let socket = crate::agent::from_environment()?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let watcher_cancelled = cancelled.clone();
        std::thread::Builder::new()
            .name("helper-stdin".into())
            .spawn(move || {
                // EOF means SSH disconnected. Extra input also invalidates this
                // one-request session. Do not keep a daemon operation alive on either.
                let _ = std::io::stdin().read(&mut [0]);
                watcher_cancelled.store(true, Ordering::Relaxed);
            })?;
        let request = if input.clone {
            protocol::Request::Clone {
                version: input.version,
                url: input.url,
                folder_name: input.folder_name,
                agent_socket: Some(socket),
            }
        } else {
            protocol::Request::PrepareClone {
                version: input.version,
                url: input.url,
                folder_name: input.folder_name,
                agent_socket: Some(socket),
            }
        };
        crate::transport::cancellable_request(
            &crate::config::Connection::Local { port: input.port },
            &request,
            &cancelled,
        )
    })();
    let response = match result {
        Ok(response) => response,
        Err(error) => protocol::Response {
            error: Some(error.to_string()),
            preflight: Some(protocol::Preflight {
                version: protocol::HELPER_VERSION,
                destination: None,
            }),
            ..protocol::Response::default()
        },
    };
    protocol::write_message(&response, std::io::stdout())
}

/// Report the unsupported platform clearly.
#[cfg(not(unix))]
pub fn run() -> Result<()> {
    Err("the SSH clone helper is supported on macOS and Linux only".into())
}

#[cfg(unix)]
struct SessionInput {
    deadline: std::time::Instant,
}

#[cfg(unix)]
impl std::io::Read for SessionInput {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            let remaining = self
                .deadline
                .saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "helper input timed out",
                ));
            }
            let mut descriptor = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: descriptor is a valid pollfd for the duration of this call.
            let status =
                unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis().min(100) as i32) };
            if status < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if status > 0 {
                // Never buffer past the message newline: the watcher must see
                // subsequent bytes/EOF even when sent with the initial message.
                // SAFETY: bytes has at least one writable byte; stdin is borrowed.
                let count = unsafe { libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), 1) };
                if count < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                return Ok(count as usize);
            }
        }
    }
}
