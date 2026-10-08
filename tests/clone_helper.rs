#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn input(port: u16) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "version": 2, "port": port, "url": "ssh://git@host/team/app.git", "folder_name": null
        })
    )
}

fn helper(socket: Option<&Path>) -> Process {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wumpa"));
    command
        .arg("clone-helper")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.env_remove("SSH_AUTH_SOCK");
    if let Some(socket) = socket {
        command.env("SSH_AUTH_SOCK", socket);
    }
    Process(command.spawn().unwrap())
}

fn finish(child: &mut Process) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "helper did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    let mut text = String::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn helper_rejects_missing_agents_versions_and_bad_framing() {
    for (message, expected) in [
        (input(7432), "SSH_AUTH_SOCK is missing"),
        (
            "{\"version\":99,\"port\":7432,\"url\":\"ssh://host/app\",\"folder_name\":null}\n"
                .into(),
            "incompatible clone helper",
        ),
        (
            "{\"version\":1,\"port\":7432,\"url\":\"ssh://host/app\",\"agent_socket\":\"/tmp/foreign\"}\n".into(),
            "unknown field",
        ),
        ("not json\n".into(), "expected"),
        ("{}".into(), "invalid or oversized"),
    ] {
        let mut child = helper(None);
        let mut stdin = child.0.stdin.take().unwrap();
        stdin.write_all(message.as_bytes()).unwrap();
        drop(stdin);
        let response = finish(&mut child);
        assert_eq!(response["preflight"]["version"], 2);
        assert!(
            response["error"].as_str().unwrap().contains(expected),
            "{response}"
        );
    }
}

#[test]
fn helper_uses_each_sessions_agent_and_drops_daemon_connection_on_disconnect() {
    let dir = tempfile::tempdir().unwrap();
    for cancel in [false, true] {
        let socket = dir.path().join(if cancel {
            "second-agent"
        } else {
            "first-agent"
        });
        let _agent = UnixListener::bind(&socket).unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut child = helper(Some(&socket));
        let mut stdin = child.0.stdin.take().unwrap();
        stdin
            .write_all(input(listener.local_addr().unwrap().port()).as_bytes())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream).read_line(&mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["action"], "prepare_clone");
        assert_eq!(
            request["agent_socket"],
            std::fs::canonicalize(&socket).unwrap().to_str().unwrap()
        );
        if cancel {
            drop(stdin);
            assert_eq!(
                stream.read(&mut [0]).unwrap(),
                0,
                "daemon connection must close"
            );
            assert!(
                finish(&mut child)["error"]
                    .as_str()
                    .unwrap()
                    .contains("cancelled")
            );
        } else {
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "repositories": [], "error": null,
                    "preflight": {"version": 2, "destination": "/projects/app"}
                })
            )
            .unwrap();
            let response = finish(&mut child);
            assert_eq!(response["preflight"]["destination"], "/projects/app");
            assert!(response["error"].is_null());
            assert!(!response.to_string().contains("agent_socket"));
            drop(stdin);
        }
    }
}

#[test]
fn ssh_preflight_uses_fixed_helper_command_and_does_not_save_metadata() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let bin = dir.path().join("bin");
    let home = dir.path().join("remote home");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(home.join(".local/bin")).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_wumpa"), home.join(".local/bin/wumpa")).unwrap();
    let ssh = bin.join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$TEST_ARGS\"\nfor arg do command=$arg; done\nexport SSH_AUTH_SOCK=\"$TEST_REMOTE_AGENT\"\nexec /bin/sh -c \"$command\"\n").unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("agent");
    let _agent = UnixListener::bind(&socket).unwrap();
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let ready = dir.path().join("ready.json");
    let config = dir.path().join("server.json");
    let mut daemon = Process(
        Command::new(env!("CARGO_BIN_EXE_wumpa"))
            .args(["serve", "--socket"])
            .arg(dir.path().join("control.sock"))
            .args(["--port", &port.to_string(), "--ready-file"])
            .arg(&ready)
            .env("WUMPA_SERVER_CONFIG", &config)
            .env("HOME", &home)
            .env_remove("SSH_AUTH_SOCK")
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
    let before = std::fs::read(&config).unwrap();
    // A local caller's socket must not silently replace the daemon's environment.
    let local = Command::new(env!("CARGO_BIN_EXE_wumpa"))
        .args([
            "check-clone",
            "--port",
            &port.to_string(),
            "--url",
            "ssh://git@host/team/app.git",
        ])
        .env("SSH_AUTH_SOCK", &socket)
        .output()
        .unwrap();
    assert!(!local.status.success());
    assert!(String::from_utf8_lossy(&local.stderr).contains("SSH_AUTH_SOCK is missing"));
    let args = dir.path().join("args");
    for (remove_helper, expected) in [(false, true), (true, false)] {
        if remove_helper {
            std::fs::remove_file(home.join(".local/bin/wumpa")).unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_wumpa"))
            .args([
                "check-clone",
                "--host",
                "test-host",
                "--port",
                &port.to_string(),
                "--url",
                "ssh://git@host/team/app.git",
                "--folder",
                "chosen",
            ])
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", &home)
            .env("SSH_AUTH_SOCK", "/invalid-local-agent")
            .env("TEST_REMOTE_AGENT", &socket)
            .env("TEST_ARGS", &args)
            .output()
            .unwrap();
        assert_eq!(output.status.success(), expected, "{output:?}");
        if expected {
            assert!(String::from_utf8_lossy(&output.stdout).contains("chosen"));
            assert!(!home.join("chosen").exists());
        } else {
            assert!(String::from_utf8_lossy(&output.stderr).contains("helper missing"));
        }
        assert_eq!(std::fs::read(&config).unwrap(), before);
        let arguments = std::fs::read_to_string(&args).unwrap();
        assert!(arguments.lines().any(|arg| arg == "-A"));
        assert!(arguments.lines().any(|arg| arg == "-T"));
        assert!(arguments.lines().any(|arg| arg == "ControlPath=none"));
        assert!(!arguments.lines().any(|arg| arg == "-W"));
        assert!(!arguments.contains("ssh://git@host/team/app.git"));
        assert!(!arguments.contains("invalid-local-agent"));
    }
}
