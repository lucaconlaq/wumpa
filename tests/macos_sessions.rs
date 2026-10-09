#![cfg(target_os = "macos")]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixStream,
    },
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(socket: &Path, request: Value) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(stream, "{request}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn observation(path: &Path) -> Value {
    let path = path.canonicalize().unwrap();
    let metadata = path.metadata().unwrap();
    json!({"path": path, "device": metadata.dev(), "inode": metadata.ino()})
}

#[test]
fn macos_rejects_agent_operations_without_launching_and_keeps_browsing_available() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let repo = dir.path().join("repo");
    assert!(
        Command::new("git")
            .arg("init")
            .arg(&repo)
            .output()
            .unwrap()
            .status
            .success()
    );
    let config = dir.path().join("server.json");
    let launched = dir.path().join("agent-launched");
    fs::write(
        &config,
        json!({
            "repository_dir": dir.path(),
            "agent_command": ["/usr/bin/touch", launched],
            "repositories": [{"url": "ssh://host/repo.git", "checkout_path": repo}]
        })
        .to_string(),
    )
    .unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(daemon.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let before = fs::read(&config).unwrap();
    let handshake = request(&socket, json!({"action": "handshake", "version": 1}));
    let observations = json!({
        "directory": observation(&repo), "root": observation(&repo),
        "git_directory": observation(&repo.join(".git")),
        "common_directory": observation(&repo.join(".git"))
    });
    for operation in [
        json!({"action": "list", "observations": observations}),
        json!({"action": "create", "observations": observations,
            "request_id": "a".repeat(32), "environment": []}),
        json!({"action": "retry_create", "observations": observations,
            "request_id": "a".repeat(32), "originating_run_id": handshake["run_id"]}),
        json!({"action": "attach", "observations": observations, "session_id": "b".repeat(32)}),
    ] {
        let mut envelope = json!({"action": "sessions", "request": {
            "version": 1, "run_id": handshake["run_id"], "operation": operation
        }});
        assert_eq!(
            request(&socket, envelope.clone()),
            json!({"status": "failed", "failure": "unsupported_platform"})
        );
        envelope["request"]["run_id"] = json!("stale-run");
        assert_eq!(
            request(&socket, envelope),
            json!({"status": "failed", "failure": "run_changed"})
        );
    }

    let output = Command::new(env!("CARGO_BIN_EXE_wumpa"))
        .args(["agent", "--plain", "--socket"])
        .arg(&socket)
        .current_dir(&repo)
        .env_remove("TMUX")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("Agent sessions require a Linux server"),
        "{error}"
    );
    assert!(error.contains("Repository browsing and cloning remain available"));
    assert!(!error.contains("BackendUnavailable"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Session name"));

    let mut tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    writeln!(tcp, "{}", json!({"action": "list"})).unwrap();
    let mut line = String::new();
    BufReader::new(tcp).read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert!(response["error"].is_null());
    assert_eq!(response["repositories"], json!(["ssh://host/repo.git"]));
    assert_eq!(response["sessions"]["supported"], false);
    assert_eq!(response["sessions"]["sessions"], json!([]));
    assert!(response["sessions"]["error"].is_null());
    assert_eq!(fs::read(&config).unwrap(), before);
    assert!(!launched.exists());
    assert!(!dir.path().join(".control.sock.sessions").exists());
    assert!(
        !dir.path()
            .join(".control.sock.session-identity.json")
            .exists()
    );
}
