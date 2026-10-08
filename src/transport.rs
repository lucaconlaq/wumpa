use std::{
    io::{Read, Seek, SeekFrom},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::{
    Result,
    config::Connection,
    protocol::{self, MAX_MESSAGE, Request, Response},
};

struct Ssh(Child);
impl Drop for Ssh {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Send one request using the transport's default deadline, without cancellation.
pub fn request(connection: &Connection, request: &Request) -> Result<Response> {
    cancellable_request(connection, request, &AtomicBool::new(false))
}

/// Send one request, polling cancellation while waiting for a response.
/// Metadata uses 20-second SSH / ten-second local deadlines. Clone requests
/// allow the server's 30-minute deadline plus cleanup time. Session requests use
/// cancellable nonblocking input kept open until completion; metadata SSH uses
/// file-backed stdin. Local connection and writes have two-second timeouts.
pub fn cancellable_request(
    connection: &Connection,
    request: &Request,
    cancelled: &AtomicBool,
) -> Result<Response> {
    let cloning = matches!(request, Request::Clone { .. });
    let timeout = if cloning {
        protocol::CLONE_TIMEOUT + Duration::from_secs(10)
    } else {
        Duration::from_secs(20)
    };
    let deadline = Instant::now() + timeout;
    check_deadline(cancelled, deadline)?;
    match request {
        Request::Add { url } => {
            crate::repository::folder_name(url, None)?;
        }
        Request::PrepareClone {
            version,
            url,
            folder_name,
            ..
        }
        | Request::Clone {
            version,
            url,
            folder_name,
            ..
        } => {
            if *version != protocol::HELPER_VERSION {
                return Err("incompatible clone protocol; update both client and server".into());
            }
            crate::repository::folder_name(url, folder_name.as_deref())?;
        }
        Request::List => {}
    }
    let response: Response = match connection {
        Connection::Local { port } => {
            let address = SocketAddr::from((Ipv4Addr::LOCALHOST, *port));
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
            stream.set_read_timeout(Some(Duration::from_millis(100)))?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            protocol::write_message(request, &mut stream)?;
            let deadline = if cloning {
                deadline
            } else {
                Instant::now() + Duration::from_secs(10)
            };
            let mut bytes = Vec::new();
            loop {
                check_deadline(cancelled, deadline)?;
                let mut chunk = [0; 4096];
                match stream.read(&mut chunk) {
                    Ok(0) => return Err("server closed the connection without a response".into()),
                    Ok(n) => {
                        bytes.extend_from_slice(&chunk[..n]);
                        if bytes.len() as u64 > MAX_MESSAGE {
                            return Err("server response is too large".into());
                        }
                        if bytes.contains(&b'\n') {
                            break;
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            protocol::read_message(bytes.as_slice())?
        }
        Connection::Ssh { host, port }
            if matches!(
                request,
                Request::PrepareClone { .. } | Request::Clone { .. }
            ) =>
        {
            session_ssh(host, *port, request, cancelled, deadline)?
        }
        Connection::Ssh { host, port } => {
            crate::parse_host(host)?;
            // Never let SSH prompts or diagnostics corrupt the alternate screen.
            // Authenticate/accept host keys with ordinary ssh before using Wumpa.
            let input = request_file(request)?;
            check_deadline(cancelled, deadline)?;
            let mut output = tempfile::tempfile()?;
            let mut errors = tempfile::tempfile()?;
            let mut ssh = Ssh(Command::new("ssh")
                .args([
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "StrictHostKeyChecking=yes",
                    "-o",
                    "ConnectTimeout=10",
                    "-W",
                ])
                .arg(format!("127.0.0.1:{port}"))
                .arg("--")
                .arg(host)
                .stdin(Stdio::from(input))
                .stdout(Stdio::from(output.try_clone()?))
                .stderr(Stdio::from(errors.try_clone()?))
                .spawn()?);
            let status = wait_ssh(&mut ssh, &output, &errors, cancelled, deadline)?;
            if !status.success() {
                errors.seek(SeekFrom::Start(0))?;
                let mut message = String::new();
                errors.take(65536).read_to_string(&mut message)?;
                return Err(format!(
                    "SSH failed: {}. Check `ssh {host}` in a terminal first.",
                    message.trim()
                )
                .into());
            }
            output.seek(SeekFrom::Start(0))?;
            protocol::read_message(output)?
        }
    };
    if matches!(
        request,
        Request::PrepareClone { .. } | Request::Clone { .. }
    ) && response.preflight.as_ref().map(|value| value.version) != Some(protocol::HELPER_VERSION)
    {
        return Err(
            "missing or incompatible clone helper/daemon response; update both client and server"
                .into(),
        );
    }
    if let Some(error) = &response.error {
        return Err(error.clone().into());
    }
    Ok(response)
}

/// Fixed shell text only: all request data travels over stdin, never interpolation.
#[cfg(unix)]
const HELPER_COMMAND: &str = "if [ -x \"$HOME/.local/bin/wumpa\" ]; then exec \"$HOME/.local/bin/wumpa\" clone-helper; elif command -v wumpa >/dev/null 2>&1; then exec wumpa clone-helper; else printf '%s\\n' 'Wumpa clone helper missing; install/update Wumpa on the server' >&2; exit 127; fi";

#[cfg(unix)]
fn session_ssh(
    host: &str,
    port: u16,
    request: &Request,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<Response> {
    session_ssh_command(
        Command::new("ssh"),
        host,
        port,
        request,
        cancelled,
        deadline,
    )
}

#[cfg(unix)]
fn session_ssh_command(
    mut command: Command,
    host: &str,
    port: u16,
    request: &Request,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<Response> {
    use std::{
        io::Write,
        os::{fd::OwnedFd, unix::net::UnixStream},
    };

    check_deadline(cancelled, deadline)?;
    crate::parse_host(host)?;
    let (Request::PrepareClone {
        version,
        url,
        folder_name,
        ..
    }
    | Request::Clone {
        version,
        url,
        folder_name,
        ..
    }) = request
    else {
        return Err("expected clone preflight request".into());
    };
    let input = protocol::HelperRequest {
        version: *version,
        port,
        clone: matches!(request, Request::Clone { .. }),
        url: url.clone(),
        folder_name: folder_name.clone(),
    };
    let mut bytes = Vec::new();
    protocol::write_message(&input, &mut bytes)?;
    let (mut writer, reader) = UnixStream::pair()?;
    writer.set_nonblocking(true)?;
    let mut output = tempfile::tempfile()?;
    let mut errors = tempfile::tempfile()?;
    let mut ssh = Ssh(command
        .args([
            "-A",
            "-T",
            // A shared master may forward a different session's agent.
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
        ])
        .arg(host)
        .arg(HELPER_COMMAND)
        .stdin(Stdio::from(OwnedFd::from(reader)))
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(errors.try_clone()?))
        .spawn()?);
    while !bytes.is_empty() {
        check_deadline(cancelled, deadline)?;
        if output.metadata()?.len() > MAX_MESSAGE || errors.metadata()?.len() > 65536 {
            return Err("SSH output exceeded its size limit".into());
        }
        match writer.write(&bytes) {
            Ok(0) => return Err("SSH helper closed its input".into()),
            Ok(count) => {
                bytes.drain(..count);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(format!(
                    "SSH helper input failed: {error}; install/update Wumpa on the server"
                )
                .into());
            }
        }
    }
    // Keep writer alive: closing stdin signals session cancellation to the helper.
    let status = wait_ssh(&mut ssh, &output, &errors, cancelled, deadline)?;
    drop(writer);
    if !status.success() {
        errors.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        errors.take(65536).read_to_end(&mut bytes)?;
        return Err(format!("SSH clone helper failed: {}. Verify SSH access and install/update Wumpa on the server.", crate::output::clean(&String::from_utf8_lossy(&bytes))).into());
    }
    output.seek(SeekFrom::Start(0))?;
    protocol::read_message(output).map_err(|error| {
        format!("invalid clone helper response: {error}; update both client and server").into()
    })
}

#[cfg(not(unix))]
fn session_ssh(
    _host: &str,
    _port: u16,
    _request: &Request,
    _cancelled: &AtomicBool,
    _deadline: Instant,
) -> Result<Response> {
    Err("SSH clone preflight is supported on macOS and Linux only".into())
}

fn request_file(request: &Request) -> Result<std::fs::File> {
    let mut input = tempfile::tempfile()?;
    protocol::write_message(request, &mut input)?;
    input.seek(SeekFrom::Start(0))?;
    Ok(input)
}

fn wait_ssh(
    ssh: &mut Ssh,
    output: &std::fs::File,
    errors: &std::fs::File,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<std::process::ExitStatus> {
    loop {
        check_deadline(cancelled, deadline)?;
        if output.metadata()?.len() > MAX_MESSAGE || errors.metadata()?.len() > 65536 {
            return Err("SSH output exceeded its size limit".into());
        }
        if let Some(status) = ssh.0.try_wait()? {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn check_deadline(cancelled: &AtomicBool, deadline: Instant) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("request cancelled".into());
    }
    if Instant::now() >= deadline {
        return Err("request timed out".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_urls_before_connecting() {
        let connection = Connection::Ssh {
            host: "unused".into(),
            port: 7432,
        };
        for url in [String::new(), "x".repeat(4097), "a\u{1b}b".into()] {
            let error = request(&connection, &Request::Add { url }).err().unwrap();
            assert!(error.to_string().contains("repository URL must be"));
        }
    }

    #[test]
    fn rejects_non_ssh_urls_before_connecting() {
        let connection = Connection::Ssh {
            host: "unused".into(),
            port: 7432,
        };
        for url in ["https://host/app.git", "git@host:app.git", "file:///app"] {
            let error = request(&connection, &Request::Add { url: url.into() })
                .err()
                .unwrap();
            assert!(error.to_string().contains("use an ssh:// repository URL"));
        }
    }

    #[test]
    fn preflight_rejects_unversioned_or_outdated_daemons() {
        use std::io::Write;
        use std::net::TcpListener;

        for preflight in [
            serde_json::Value::Null,
            serde_json::json!({"version": 99, "destination": null}),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let connection = Connection::Local {
                port: listener.local_addr().unwrap().port(),
            };
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let _: Request = protocol::read_message(&mut stream).unwrap();
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({"repositories": [], "error": null, "preflight": preflight})
                )
                .unwrap();
            });
            let error = request(
                &connection,
                &Request::PrepareClone {
                    version: protocol::HELPER_VERSION,
                    url: "ssh://host/app.git".into(),
                    folder_name: None,
                    agent_socket: None,
                },
            )
            .err()
            .unwrap();
            assert!(error.to_string().contains("update both client and server"));
            server.join().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn ssh_clone_selects_execution_without_serializing_the_clients_agent_path() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("request");
        let mut command = Command::new("sh");
        command.args(["-c", r#"IFS= read -r request; printf '%s\n' "$request" > "$1"; printf '%s\n' '{"repositories":[],"error":null,"preflight":{"version":2,"destination":"/projects/review"}}'"#, "mock-ssh"]).arg(&input);
        let response = session_ssh_command(
            command,
            "test-host",
            7432,
            &Request::Clone {
                version: protocol::HELPER_VERSION,
                url: "ssh://host/app.git".into(),
                folder_name: Some("review".into()),
                agent_socket: Some("/client/must-not-be-forwarded".into()),
            },
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(3),
        )
        .unwrap();
        let text = std::fs::read_to_string(input).unwrap();
        let request: protocol::HelperRequest = protocol::read_message(text.as_bytes()).unwrap();
        assert!(request.clone);
        assert_eq!(request.port, 7432);
        assert_eq!(request.folder_name.as_deref(), Some("review"));
        assert!(!text.contains("agent_socket"));
        assert!(!text.contains("/client/"));
        assert_eq!(
            response.preflight.unwrap().destination.as_deref(),
            Some(std::path::Path::new("/projects/review"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn stalled_session_input_is_cancellable_and_child_is_reaped() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("pid");
        for cancel in [false, true] {
            let cancelled = Arc::new(AtomicBool::new(false));
            let worker_cancelled = cancelled.clone();
            let setter = thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                worker_cancelled.store(cancel, Ordering::Relaxed);
            });
            let mut command = Command::new("sh");
            command
                .args(["-c", "echo $$ > \"$1\"; exec sleep 30", "mock-ssh"])
                .arg(&pid_path);
            let started = Instant::now();
            let error = session_ssh_command(
                command,
                "unused",
                7432,
                &Request::PrepareClone {
                    version: protocol::HELPER_VERSION,
                    // Bypass URL validation to exercise a socket-full write.
                    url: "x".repeat(500_000),
                    folder_name: None,
                    agent_socket: None,
                },
                &cancelled,
                started + Duration::from_millis(200),
            )
            .err()
            .unwrap();
            setter.join().unwrap();
            assert_eq!(
                error.to_string(),
                if cancel {
                    "request cancelled"
                } else {
                    "request timed out"
                }
            );
            assert!(started.elapsed() < Duration::from_secs(2));
            let pid: i32 = std::fs::read_to_string(&pid_path)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            // SAFETY: signal zero only checks whether this test's child still exists.
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        }
    }

    #[test]
    fn request_file_is_rewound_and_newline_terminated() {
        let input = request_file(&Request::List).unwrap();
        assert!(matches!(
            protocol::read_message(input).unwrap(),
            Request::List
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unread_ssh_input_does_not_block_timeout_or_cancellation() {
        // Larger than a pipe buffer: the child deliberately never reads stdin.
        let input = request_file(&Request::Add {
            url: "x".repeat(500_000),
        })
        .unwrap();
        for cancel in [false, true] {
            let output = tempfile::tempfile().unwrap();
            let errors = tempfile::tempfile().unwrap();
            let mut ssh = Ssh(Command::new("sh")
                .args(["-c", "exec sleep 30"])
                .stdin(Stdio::from(input.try_clone().unwrap()))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap());
            let started = Instant::now();
            let error = wait_ssh(
                &mut ssh,
                &output,
                &errors,
                &AtomicBool::new(cancel),
                started + Duration::from_millis(100),
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                if cancel {
                    "request cancelled"
                } else {
                    "request timed out"
                }
            );
            drop(ssh);
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }
}
