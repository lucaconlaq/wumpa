use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(path: &Path, port: u16) -> Daemon {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_wumpa"))
            .args(["serve", "--port", &port.to_string(), "--socket"])
            .arg(path.parent().unwrap().join("control.sock"))
            .env("WUMPA_SERVER_CONFIG", path)
            .env("HOME", path.parent().unwrap())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    for _ in 0..100 {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "server exited early"
        );
        if let Ok(mut stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
            // Binding precedes migration; only a response proves startup finished.
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream.write_all(b"{\"action\":\"list\"}\n").unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            let response: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response.get("error"), Some(&serde_json::Value::Null));
            assert!(response["repositories"].is_array());
            return daemon;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("server did not start");
}

fn client(path: &Path, input: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wumpa"))
        .arg("--plain")
        .env("WUMPA_CLIENT_CONFIG", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn invalid_server_root_does_not_rewrite_legacy_config() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = dir.path().join("server.json");
    for config in [
        r#"{"repositories":["https://example.com/app.git"]}"#,
        r#"{"repository_dir":"~/projects","repositories":[]}"#,
    ] {
        std::fs::write(&path, config).unwrap();
        // Another test/process can claim the port between release and exec.
        let output = (0..10)
            .find_map(|_| {
                let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                let port = reservation.local_addr().unwrap().port();
                drop(reservation);
                let output = Command::new(env!("CARGO_BIN_EXE_wumpa"))
                    .args(["serve", "--port", &port.to_string(), "--socket"])
                    .arg(dir.path().join("control.sock"))
                    .env("WUMPA_SERVER_CONFIG", &path)
                    .env_remove("HOME")
                    .output()
                    .unwrap();
                if String::from_utf8_lossy(&output.stderr).contains("Address already in use") {
                    None
                } else {
                    Some(output)
                }
            })
            .expect("could not reserve a server port");
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("repository_dir"),
            "{output:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), config);
    }
}

#[test]
fn legacy_server_migration_survives_restart_without_cloning() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.json");
    std::fs::write(
        &path,
        r#"{"repositories":["https://example.com/legacy.git"]}"#,
    )
    .unwrap();
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let daemon = start(&path, port);
    let migrated = std::fs::read(&path).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&migrated).unwrap();
    assert_eq!(
        saved["repositories"][0]["url"],
        "https://example.com/legacy.git"
    );
    assert!(saved["repositories"][0]["checkout_path"].is_null());
    drop(daemon);
    let _daemon = start(&path, port);
    assert_eq!(std::fs::read(&path).unwrap(), migrated);
    // Runtime discovery initializes asynchronously; only known daemon metadata
    // may appear alongside the configuration/socket/lock, never a cloned folder.
    let names = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert!((3..=5).contains(&names.len()));
    assert!(names.iter().all(|name| matches!(
        name.to_str(),
        Some(
            "server.json"
                | "control.sock"
                | "control.sock.lock"
                | ".control.sock.sessions"
                | ".control.sock.session-identity.json"
        )
    )));
}

#[test]
fn plain_client_escapes_remote_errors() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let path = dir.path().join("client.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "servers": [{"name": "test", "type": "local", "port": port}]
        }))
        .unwrap(),
    )
    .unwrap();
    let responder = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = String::new();
        BufReader::new(&mut stream).read_line(&mut request).unwrap();
        writeln!(
            stream,
            "{}",
            serde_json::json!({"repositories": [], "error": "\u{1b}[2Jremote\nerror"})
        )
        .unwrap();
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_wumpa"))
        .arg("--plain")
        .env("WUMPA_CLIENT_CONFIG", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"1\nq\n").unwrap();
    let output = child.wait_with_output().unwrap();
    responder.join().unwrap();
    assert!(output.status.success());
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(!errors.contains('\u{1b}'));
    assert!(errors.contains("\\u{1b}[2Jremote\\nerror"));
}

#[test]
fn local_client_saves_connection_and_repository_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let server_config = dir.path().join("server.json");
    let client_config = dir.path().join("client.json");
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let daemon = start(&server_config, port);
    // Legacy metadata registration remains a server API, not a client action.
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(b"{\"action\":\"add\",\"url\":\"ssh://git@example.com/app.git\"}\n")
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert!(serde_json::from_str::<serde_json::Value>(&line).unwrap()["error"].is_null());
    let output = client(
        &client_config,
        &format!("a\ntest\nlocal\n{port}\n1\nl\nb\nq\n"),
    );
    assert!(output.contains("Connected to \"test\""));
    assert!(output.contains("ssh://git@example.com/app.git"));
    drop(daemon);

    let _daemon = start(&server_config, port);
    let output = client(&client_config, "1\nl\nb\nq\n");
    assert!(output.contains("ssh://git@example.com/app.git"));
    let server_before = std::fs::read(&server_config).unwrap();
    let saved_server: serde_json::Value = serde_json::from_slice(&server_before).unwrap();
    assert_eq!(
        saved_server["repository_dir"],
        std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap()
    );
    assert_eq!(
        saved_server["repositories"][0]["url"],
        "ssh://git@example.com/app.git"
    );
    assert!(saved_server["repositories"][0]["checkout_path"].is_null());
    let output = client(&client_config, "d\n1\ny\nq\n");
    assert!(output.contains("Connection removed. Server unchanged."));
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&client_config).unwrap()).unwrap();
    assert!(saved["servers"].as_array().unwrap().is_empty());
    assert_eq!(std::fs::read(&server_config).unwrap(), server_before);
    // Re-registering and reconnecting proves the server and repositories survived.
    let output = client(
        &client_config,
        &format!("a\ntest\nlocal\n{port}\n1\nl\nb\nq\n"),
    );
    assert!(output.contains("ssh://git@example.com/app.git"));
    let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(
        (4..=6).contains(&entries.len()),
        "only configs and daemon runtime metadata may exist; no clone"
    );
    assert!(entries.into_iter().all(|entry| matches!(
        entry.unwrap().file_name().to_str(),
        Some(
            "server.json"
                | "client.json"
                | "control.sock"
                | "control.sock.lock"
                | ".control.sock.sessions"
                | ".control.sock.session-identity.json"
        )
    )));
}
