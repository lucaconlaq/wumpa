#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn serve_requires_absolute_socket_selection() {
    for args in [vec!["serve"], vec!["serve", "--socket", "relative"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_wumpa"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("--socket"));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_recovers_from_descriptor_pressure_without_losing_either_listener() {
    use std::os::unix::process::CommandExt;

    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("control.sock");
    let ready = dir.path().join("ready.json");
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let mut command = Command::new(env!("CARGO_BIN_EXE_wumpa"));
    command
        .args(["serve", "--port", &port.to_string(), "--socket"])
        .arg(&socket)
        .arg("--ready-file")
        .arg(&ready)
        .env("WUMPA_SERVER_CONFIG", dir.path().join("config.json"))
        .env("HOME", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: setrlimit is called only in the forked child, before exec, using
    // a stack-allocated limit. It neither allocates nor accesses shared state.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 24,
                rlim_max: 24,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut daemon = Daemon(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(daemon.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let mut clients = Vec::new();
    for _ in 0..25 {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.write_all(b" ").unwrap();
        clients.push(stream);
    }
    thread::sleep(Duration::from_millis(150));
    assert!(daemon.0.try_wait().unwrap().is_none());
    drop(clients);
    let mut local = UnixStream::connect(&socket).unwrap();
    local
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();
    local
        .write_all(b"{\"action\":\"handshake\",\"version\":1}\n")
        .unwrap();
    let mut line = String::new();
    BufReader::new(local).read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["version"],
        1
    );
    let mut tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
    tcp.write_all(b"{\"action\":\"list\"}\n").unwrap();
    line.clear();
    BufReader::new(tcp).read_line(&mut line).unwrap();
    assert!(serde_json::from_str::<serde_json::Value>(&line).unwrap()["error"].is_null());
}

#[test]
fn handshake_is_read_only_tcp_rejects_it_and_sigterm_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let config = dir.path().join("config.json");
    let socket = dir.path().join("control.sock");
    let ready = dir.path().join("ready.json");
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_wumpa"))
            .args(["serve", "--port", &port.to_string(), "--socket"])
            .arg(&socket)
            .arg("--ready-file")
            .arg(&ready)
            .env("WUMPA_SERVER_CONFIG", &config)
            .env("HOME", dir.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(daemon.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let before = fs::read(&config).unwrap();
    let mut local = UnixStream::connect(&socket).unwrap();
    local
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();
    local
        .write_all(b"{\"action\":\"handshake\",\"version\":1}\n")
        .unwrap();
    let mut line = String::new();
    BufReader::new(local).read_line(&mut line).unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["version"], 1);
    assert_eq!(
        response["socket"],
        fs::canonicalize(&socket).unwrap().to_str().unwrap()
    );
    assert_eq!(response["run_id"].as_str().unwrap().len(), 32);
    assert!(response.get("repositories").is_none());
    assert_eq!(fs::read(&config).unwrap(), before);

    let mut tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
    tcp.write_all(b"{\"action\":\"handshake\",\"version\":1}\n")
        .unwrap();
    line.clear();
    BufReader::new(tcp).read_line(&mut line).unwrap();
    let rejected: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(rejected["error"].is_string());
    assert_eq!(fs::read(&config).unwrap(), before);

    // SAFETY: this PID belongs to the still-live child owned by this test.
    assert_eq!(
        unsafe { libc::kill(daemon.0.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while daemon.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!socket.exists());
    assert_eq!(fs::read(&config).unwrap(), before);
}
