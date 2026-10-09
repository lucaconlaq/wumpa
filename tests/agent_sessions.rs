#![cfg(target_os = "linux")]

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpListener},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "condition timed out");
        thread::sleep(Duration::from_millis(25));
    }
}
fn request(socket: &Path, value: Value) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(22)))
        .unwrap();
    writeln!(stream, "{value}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|error| panic!("invalid response {line:?}: {error}"))
}
fn observation(path: &Path) -> Value {
    let path = path.canonicalize().unwrap();
    let metadata = path.metadata().unwrap();
    json!({"path": path, "device": metadata.dev(), "inode": metadata.ino()})
}
struct Fixture {
    dir: tempfile::TempDir,
    repo: PathBuf,
    socket: PathBuf,
    child: Child,
    run: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let repo = dir.path().join("checkout space");
        fs::create_dir(&repo).unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .arg("init")
                .output()
                .unwrap()
                .status
                .success()
        );
        let agent = dir.path().join("fake-agent");
        fs::write(
            &agent,
            "#!/bin/sh\nprintf '%s' \"$TOKEN\" > launched-token\nread input\nprintf '%s' \"$input\" > typed-input\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(dir.path().join("server.json"), serde_json::to_vec(&json!({
            "repository_dir": dir.path(), "repositories": [{"url":"git@github.com:test/repo.git", "checkout_path":repo}],
            "agent_command": [agent]
        })).unwrap()).unwrap();
        let socket = dir.path().join("c.sock");
        let child = Self::start(dir.path(), &socket);
        let run = request(&socket, json!({"action":"handshake", "version":1}))["run_id"]
            .as_str()
            .unwrap()
            .into();
        Self {
            dir,
            repo,
            socket,
            child,
            run,
        }
    }
    fn start(directory: &Path, socket: &Path) -> Child {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let ready = directory.join("ready");
        let _ = fs::remove_file(&ready);
        let mut child = Command::new(env!("CARGO_BIN_EXE_wumpa"))
            .args(["serve", "--socket"])
            .arg(socket)
            .args(["--port", &port.to_string(), "--ready-file"])
            .arg(&ready)
            .env("WUMPA_SERVER_CONFIG", directory.join("server.json"))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        wait(|| {
            assert!(child.try_wait().unwrap().is_none());
            ready.exists()
        });
        child
    }
    fn observations(&self) -> Value {
        let git_path = |flag| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["rev-parse", "--path-format=absolute", flag])
                .output()
                .unwrap();
            assert!(output.status.success());
            PathBuf::from(
                String::from_utf8(output.stdout.strip_suffix(b"\n").unwrap().to_vec()).unwrap(),
            )
        };
        json!({"directory":observation(&self.repo), "root":observation(&git_path("--show-toplevel")),
            "git_directory":observation(&git_path("--absolute-git-dir")),
            "common_directory":observation(&git_path("--git-common-dir"))})
    }
    fn operation(&self, operation: Value) -> Value {
        request(
            &self.socket,
            json!({"action":"sessions", "request": {
                "version":1, "run_id":self.run, "operation":operation
            }}),
        )
    }
    fn create(&self, key: &str) -> Value {
        self.operation(json!({"action":"create", "request_id":key,
        "observations":self.observations(), "environment":[
            {"name":STANDARD.encode("TOKEN"), "value":STANDARD.encode("caller-secret-test")},
            {"name":STANDARD.encode("PATH"), "value":STANDARD.encode(std::env::var("PATH").unwrap())}
        ]}))
    }
    fn list(&self) -> Value {
        self.operation(json!({"action":"list", "observations":self.observations()}))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Stop owned runners before killing the isolated test server. Never touch the
        // user's tmux server, even when an assertion failed.
        let runtime = self.dir.path().join(".c.sock.sessions");
        if let Ok(entries) = fs::read_dir(&runtime) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(id) = name
                    .strip_prefix("a-")
                    .and_then(|name| name.strip_suffix(".sock"))
                {
                    if let Ok(mut stream) = UnixStream::connect(entry.path()) {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(4)));
                        let _ = writeln!(stream, "{}", json!({"action":"stop", "session_id":id}));
                        let mut line = String::new();
                        let _ = BufReader::new(stream).read_line(&mut line);
                    }
                }
            }
        }
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(runtime.join("tmux.sock"))
            .arg("kill-server")
            .output();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn agent_exit_removes_extra_windows_and_preserves_retry_outcome() {
    let fixture = Fixture::new();
    fs::write(
        fixture.dir.path().join("fake-agent"),
        "#!/bin/sh\nwhile [ ! -f exit-agent ]; do sleep 0.1; done\n",
    )
    .unwrap();
    let key = "e".repeat(32);
    let created = fixture.create(&key);
    assert_eq!(created["status"], "created", "{created}");
    // Use the dedicated instance socket, never the developer's tmux server.
    let socket = fixture.dir.path().join(".c.sock.sessions/tmux.sock");
    let name = format!("wumpa-{}", created["session_id"].as_str().unwrap());
    assert!(
        Command::new("tmux")
            .arg("-S")
            .arg(&socket)
            .args(["new-window", "-d", "-t", &name, "sleep", "60"])
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::write(fixture.repo.join("exit-agent"), "exit").unwrap();
    wait(|| {
        fixture.list()["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions.is_empty())
    });
    assert!(
        !Command::new("tmux")
            .arg("-S")
            .arg(&socket)
            .args(["has-session", "-t", &name])
            .output()
            .unwrap()
            .status
            .success()
    );
    let id = created["session_id"].as_str().unwrap();
    wait(|| {
        fixture
            .dir
            .path()
            .join(format!(".c.sock.sessions/a-{id}.done"))
            .exists()
    });
    let completion: Value = serde_json::from_slice(
        &fs::read(
            fixture
                .dir
                .path()
                .join(format!(".c.sock.sessions/a-{id}.done")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(completion["stopped"], true);
    assert_eq!(completion["session_id"], created["session_id"]);
    let retry = fixture.create(&key);
    assert_eq!(retry["session_id"], created["session_id"]);
}

#[test]
fn fifo_instance_record_does_not_hang_handshake_or_shutdown() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let mut fixture = Fixture::new();
    fixture.child.kill().unwrap();
    fixture.child.wait().unwrap();
    let record = fixture.dir.path().join(".c.sock.sessions/instance.json");
    fs::remove_file(&record).unwrap();
    let name = CString::new(record.as_os_str().as_bytes()).unwrap();
    // SAFETY: name points to a valid private test pathname.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    fixture.child = Fixture::start(fixture.dir.path(), &fixture.socket);
    let hello = request(&fixture.socket, json!({"action":"handshake", "version":1}));
    assert!(hello["run_id"].is_string(), "{hello}");
    // SAFETY: this PID is the unreaped daemon child owned by this fixture.
    assert_eq!(
        unsafe { libc::kill(fixture.child.id() as i32, libc::SIGTERM) },
        0
    );
    wait(|| fixture.child.try_wait().unwrap().is_some());
}

#[test]
fn stale_runs_and_missing_executables_fail_without_launching() {
    let fixture = Fixture::new();
    let stale = request(
        &fixture.socket,
        json!({"action":"sessions", "request": {
            "version":1, "run_id":"not-the-current-run", "operation": {"action":"create", "request_id":"5".repeat(32),
            "observations":fixture.observations(), "environment":[]}
        }}),
    );
    assert_eq!(stale["failure"], "run_changed");
    fs::set_permissions(
        fixture.dir.path().join("fake-agent"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let unavailable = fixture.create(&"6".repeat(32));
    assert_eq!(unavailable["failure"], "agent_unavailable");
    assert_eq!(fixture.list()["sessions"].as_array().unwrap().len(), 0);
}

#[test]
fn detached_descendants_are_force_stopped_after_external_replacement() {
    let fixture = Fixture::new();
    fs::write(fixture.dir.path().join("fake-agent"), "#!/bin/sh\nsetsid sh -c 'echo $$ > detached-pid; while :; do sleep 1; done' </dev/null >/dev/null 2>&1 &\necho $$ > agent-pid\nwhile :; do sleep 1; done\n").unwrap();
    let created = fixture.create(&"b".repeat(32));
    assert_eq!(created["status"], "created", "{created}");
    wait(|| {
        fs::read_to_string(fixture.repo.join("detached-pid"))
            .is_ok_and(|value| value.trim().parse::<u32>().is_ok())
            && fs::read_to_string(fixture.repo.join("agent-pid"))
                .is_ok_and(|value| value.trim().parse::<u32>().is_ok())
    });
    let descendant = fs::read_to_string(fixture.repo.join("detached-pid"))
        .unwrap()
        .trim()
        .to_owned();
    let agent = fs::read_to_string(fixture.repo.join("agent-pid"))
        .unwrap()
        .trim()
        .to_owned();
    let moved = fixture.dir.path().join("old-checkout");
    fs::rename(&fixture.repo, &moved).unwrap();
    fs::create_dir(&fixture.repo).unwrap();
    fs::write(fixture.repo.join("replacement-is-untouched"), "keep").unwrap();
    wait(|| {
        !Path::new(&format!("/proc/{descendant}")).exists()
            && !Path::new(&format!("/proc/{agent}")).exists()
    });
    assert_eq!(
        fs::read_to_string(fixture.repo.join("replacement-is-untouched")).unwrap(),
        "keep"
    );
}

#[test]
fn agent_exit_removes_output_and_allows_new_creation_without_retry_duplication() {
    let fixture = Fixture::new();
    fs::write(
        fixture.dir.path().join("fake-agent"),
        "#!/bin/sh\nprintf 'not-retained-output'\nexit 0\n",
    )
    .unwrap();
    let first = fixture.create(&"c".repeat(32));
    assert_eq!(first["status"], "created", "{first}");
    wait(|| {
        fixture.list()["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions.is_empty())
    });
    assert_eq!(fixture.create(&"c".repeat(32)), first);
    let second = fixture.create(&"d".repeat(32));
    assert_eq!(second["status"], "created", "{second}");
    assert_ne!(first["session_id"], second["session_id"]);
    for entry in fs::read_dir(fixture.dir.path().join(".c.sock.sessions"))
        .unwrap()
        .flatten()
    {
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            assert!(
                !fs::read_to_string(entry.path())
                    .unwrap()
                    .contains("not-retained-output")
            );
        }
    }
}

#[test]
fn attachment_failure_preserves_agents_and_other_tmux_is_refused_before_creation() {
    let fixture = Fixture::new();
    let refused = Command::new(env!("CARGO_BIN_EXE_wumpa"))
        .args(["agent", "--socket"])
        .arg(&fixture.socket)
        .current_dir(&fixture.repo)
        .env("TMUX", "/unrelated/socket,123,0")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("different tmux"));
    assert_eq!(fixture.list()["sessions"].as_array().unwrap().len(), 0);
    let failed_attach = Command::new(env!("CARGO_BIN_EXE_wumpa"))
        .args(["agent", "--socket"])
        .arg(&fixture.socket)
        .current_dir(&fixture.repo)
        .env_remove("TMUX")
        .env("TOKEN", "cli-test")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!failed_attach.status.success());
    assert_eq!(fixture.list()["sessions"].as_array().unwrap().len(), 1);
}

#[test]
fn instances_registering_the_same_checkout_do_not_share_sessions() {
    let first = Fixture::new();
    let mut second = Fixture::new();
    let path = second.dir.path().join("server.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["repositories"][0]["checkout_path"] = json!(first.repo);
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    second.child.kill().unwrap();
    second.child.wait().unwrap();
    second.child = Fixture::start(second.dir.path(), &second.socket);
    second.run = request(&second.socket, json!({"action":"handshake", "version":1}))["run_id"]
        .as_str()
        .unwrap()
        .into();
    second.repo = first.repo.clone();
    let a = first.create(&"e".repeat(32));
    let b = second.create(&"f".repeat(32));
    assert_eq!(a["status"], "created", "{a}");
    assert_eq!(b["status"], "created", "{b}");
    assert_ne!(a["session_id"], b["session_id"]);
    assert_eq!(first.list()["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(second.list()["sessions"].as_array().unwrap().len(), 1);
    let foreign = second.operation(json!({"action":"attach", "session_id":a["session_id"], "observations":second.observations()}));
    assert_eq!(foreign["failure"], "session_not_found");
}

#[test]
fn multiple_agents_use_separate_environments_and_resume_cancel_do_not_create() {
    let fixture = Fixture::new();
    fs::write(
        fixture.dir.path().join("fake-agent"),
        "#!/bin/sh\nprintf '%s' \"$TOKEN\" > \"token-$TMUX_PANE\"\nwhile :; do sleep 1; done\n",
    )
    .unwrap();
    let first = fixture.create(&"4".repeat(32));
    assert_eq!(first["status"], "created", "{first}");
    let cli = |choice: &str, token: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_wumpa"))
            .args(["agent", "--socket"])
            .arg(&fixture.socket)
            .current_dir(&fixture.repo)
            .env_remove("TMUX")
            .env("TOKEN", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(choice.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    assert!(!cli("n\n", "second-test-token").status.success()); // No TTY: attachment fails, not launch.
    let tokens = || {
        fs::read_dir(&fixture.repo)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("token-%"))
            .map(|entry| fs::read_to_string(entry.path()).unwrap())
            .collect::<Vec<_>>()
    };
    wait(|| {
        let values = tokens();
        values.contains(&"caller-secret-test".into())
            && values.contains(&"second-test-token".into())
    });
    assert_eq!(fixture.list()["sessions"].as_array().unwrap().len(), 2);
    assert!(cli("q\n", "must-not-change-resume").status.success());
    assert!(!cli("1\n", "must-not-change-resume").status.success());
    assert_eq!(fixture.list()["sessions"].as_array().unwrap().len(), 2);
    assert!(!tokens().contains(&"must-not-change-resume".into()));
    let global = Command::new("tmux")
        .arg("-S")
        .arg(fixture.dir.path().join(".c.sock.sessions/tmux.sock"))
        .args(["show-environment", "-g", "TOKEN"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&global.stdout).contains("test-token"));
    assert!(!String::from_utf8_lossy(&global.stdout).contains("caller-secret-test"));
}

#[test]
fn linked_checkout_subdirectories_and_aliases_launch_at_the_checkout_root() {
    let mut fixture = Fixture::new();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&fixture.repo)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial"
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    let linked = fixture.dir.path().join("linked worktree");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&fixture.repo)
            .args(["worktree", "add", "-b", "feature"])
            .arg(&linked)
            .output()
            .unwrap()
            .status
            .success()
    );
    let sub = linked.join("sub");
    fs::create_dir(&sub).unwrap();
    let alias = fixture.dir.path().join("alias");
    std::os::unix::fs::symlink(&sub, &alias).unwrap();
    fixture.repo = alias;
    let created = fixture.create(&"1".repeat(32));
    assert_eq!(created["status"], "created", "{created}");
    wait(|| linked.join("launched-token").exists());
    let list = fixture.list();
    assert_eq!(
        list["sessions"][0]["checkout"]["root"]["path"],
        json!(linked.canonicalize().unwrap())
    );
}

#[test]
fn transient_git_access_failures_do_not_terminate_agents() {
    // SAFETY: geteuid only reads identity. Root bypasses the permission failure.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let fixture = Fixture::new();
    let created = fixture.create(&"2".repeat(32));
    assert_eq!(created["status"], "created", "{created}");
    let socket = fixture.dir.path().join(".c.sock.sessions").join(format!(
        "a-{}.sock",
        created["session_id"].as_str().unwrap()
    ));
    let git = fixture.repo.join(".git");
    struct Restore(PathBuf, fs::Permissions);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, self.1.clone());
        }
    }
    let restore = Restore(git.clone(), git.metadata().unwrap().permissions());
    fs::set_permissions(&git, fs::Permissions::from_mode(0o000)).unwrap();
    thread::sleep(Duration::from_millis(1500));
    let status = request(
        &socket,
        json!({"action":"status", "session_id":created["session_id"]}),
    );
    drop(restore);
    assert_eq!(status["stopped"], false);
}

#[test]
fn remote_snapshots_are_read_only_and_do_not_contain_attachment_or_environment() {
    use std::net::TcpStream;
    let fixture = Fixture::new();
    let created = fixture.create(&"3".repeat(32));
    assert_eq!(created["status"], "created", "{created}");
    let ready: Value =
        serde_json::from_slice(&fs::read(fixture.dir.path().join("ready")).unwrap()).unwrap();
    let tcp = |value: Value| {
        let mut stream =
            TcpStream::connect((Ipv4Addr::LOCALHOST, ready["port"].as_u64().unwrap() as u16))
                .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        writeln!(stream, "{value}").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    let rejected = tcp(
        json!({"action":"sessions", "request":{"version":1,"run_id":fixture.run,"operation":{"action":"list","observations":fixture.observations()}}}),
    );
    assert!(rejected["error"].is_string());
    wait(|| {
        tcp(json!({"action":"list"}))["sessions"]["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions.len() == 1)
    });
    let response = tcp(json!({"action":"list"}));
    let summary = &response["sessions"]["sessions"][0];
    assert_eq!(summary["id"], created["session_id"]);
    for field in ["instance", "attachment", "environment", "socket"] {
        assert!(summary.get(field).is_none());
    }
    assert!(!response.to_string().contains("caller-secret-test"));
}

#[test]
fn creates_retries_recovers_and_removes_sessions_on_checkout_move() {
    let mut fixture = Fixture::new();
    let first = fixture.create(&"a".repeat(32));
    assert_eq!(first["status"], "created", "{first}");
    wait(|| {
        fs::read_to_string(fixture.repo.join("launched-token"))
            .is_ok_and(|value| value == "caller-secret-test")
    });
    assert_eq!(
        fs::read_to_string(fixture.repo.join("launched-token")).unwrap(),
        "caller-secret-test"
    );
    let status = Command::new("tmux")
        .arg("-S")
        .arg(fixture.dir.path().join(".c.sock.sessions/tmux.sock"))
        .args([
            "send-keys",
            "-t",
            &format!("wumpa-{}", first["session_id"].as_str().unwrap()),
            "typed-through-pane",
            "Enter",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    wait(|| {
        fs::read_to_string(fixture.repo.join("typed-input"))
            .is_ok_and(|value| value == "typed-through-pane")
    });
    assert_eq!(
        fs::read_to_string(fixture.repo.join("typed-input")).unwrap(),
        "typed-through-pane"
    );
    assert_eq!(fixture.create(&"a".repeat(32)), first);
    let list = fixture.list();
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1, "{list}");
    let attach = fixture.operation(json!({"action":"attach", "session_id":first["session_id"], "observations":fixture.observations()}));
    assert_eq!(attach["status"], "attached", "{attach}");
    let original_run = fixture.run.clone();
    fixture.child.kill().unwrap();
    fixture.child.wait().unwrap();
    // Simulate a crash after launch acknowledgement but before outcome persistence.
    let record_path = fixture
        .dir
        .path()
        .join(".c.sock.sessions")
        .join(format!("{original_run}-{}.json", "a".repeat(32)));
    let mut record: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    record["creation"]["outcome"] = json!({"status":"in_progress"});
    record["session"]["state"] = json!("starting");
    fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    fixture.child = Fixture::start(fixture.dir.path(), &fixture.socket);
    fixture.run = request(&fixture.socket, json!({"action":"handshake", "version":1}))["run_id"]
        .as_str()
        .unwrap()
        .into();
    assert_ne!(fixture.run, original_run);
    let retry = fixture.operation(json!({"action":"retry_create", "request_id":"a".repeat(32),
        "originating_run_id":original_run, "observations":fixture.observations()}));
    assert_eq!(retry, first);
    let unknown = fixture.operation(json!({"action":"retry_create", "request_id":"0".repeat(32),
        "originating_run_id":original_run, "observations":fixture.observations()}));
    assert_eq!(unknown["failure"], "outcome_unknown");
    assert_eq!(fixture.list()["sessions"].as_array().unwrap().len(), 1);
    let records = fs::read_dir(fixture.dir.path().join(".c.sock.sessions")).unwrap();
    for entry in records
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
    {
        assert!(
            !fs::read_to_string(entry.path())
                .unwrap()
                .contains("caller-secret-test")
        );
    }
    let agent_socket = fixture
        .dir
        .path()
        .join(".c.sock.sessions")
        .join(format!("a-{}.sock", first["session_id"].as_str().unwrap()));
    fs::rename(&fixture.repo, fixture.dir.path().join("moved-checkout")).unwrap();
    wait(|| !agent_socket.exists());
}
