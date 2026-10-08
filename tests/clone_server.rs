#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    daemon: Process,
    port: u16,
    _agent: UnixListener,
}

fn script(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "condition timed out");
        thread::sleep(Duration::from_millis(20));
    }
}

impl Fixture {
    fn new(fake_git: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("bin")).unwrap();
        fs::create_dir(root.join("repos")).unwrap();
        fs::write(
            root.join("server.json"),
            json!({
                "repository_dir": root.join("repos"),
                "repositories": [{"url": "ssh://host/saved.git"}]
            })
            .to_string(),
        )
        .unwrap();
        if fake_git {
            script(
                &root.join("bin/git"),
                r#"#!/bin/sh
if [ "$1" = "-C" ]; then
    printf 'worktree %s\000branch refs/heads/main\000\000' "$2"
    exit 0
fi
printf '%s\n' "$@" > "$TEST_ROOT/args"
printf '%s\n' "$SSH_AUTH_SOCK" "$GIT_ALLOW_PROTOCOL" "$GIT_TERMINAL_PROMPT" "$GIT_SSH_COMMAND" > "$TEST_ROOT/environment"
for arg do previous=$last; last=$arg; done
mkdir -p "$last/.git"
case "$previous" in
  *slow.git)
    (while :; do printf x >> "$TEST_ROOT/heartbeat"; sleep 0.05; done) &
    echo ready > "$TEST_ROOT/running"
    wait;;
  *fail.git) printf '\033[31mdenied' >&2; exit 1;;
  *race.git) mkdir "$TEST_ROOT/repos/race"; echo keep > "$TEST_ROOT/repos/race/keep";;
esac
echo checkout > "$last/contents"
"#,
            );
        } else {
            // Real Git speaks its local upload-pack protocol through this fake
            // SSH executable. No credentials, agent signing, or network is used.
            script(
                &root.join("bin/ssh"),
                r#"#!/bin/sh
for arg do last=$arg; done
exec /bin/sh -c "$last"
"#,
            );
        }
        let agent = UnixListener::bind(root.join("agent")).unwrap();
        let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let daemon = Self::start(root, port);
        Self {
            dir,
            daemon,
            port,
            _agent: agent,
        }
    }

    fn start(root: &Path, port: u16) -> Process {
        let ready = root.join("ready");
        let _ = fs::remove_file(&ready);
        let working_dir = root.join("server-cwd");
        fs::create_dir_all(&working_dir).unwrap();
        let mut child = Process(
            Command::new(env!("CARGO_BIN_EXE_wumpa"))
                .current_dir(&working_dir)
                .args(["serve", "--port", &port.to_string(), "--ready-file"])
                .arg(&ready)
                .env("WUMPA_SERVER_CONFIG", root.join("server.json"))
                .env("SSH_AUTH_SOCK", root.join("agent"))
                .env("TEST_ROOT", root)
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        root.join("bin").display(),
                        std::env::var("PATH").unwrap()
                    ),
                )
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        wait(|| {
            assert!(child.0.try_wait().unwrap().is_none());
            ready.exists()
        });
        child
    }

    fn send(&self, request: Value) -> TcpStream {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writeln!(stream, "{request}").unwrap();
        stream
    }

    fn request(&self, request: Value) -> Value {
        response(self.send(request))
    }

    fn clone_request(&self, name: &str) -> Value {
        json!({"action": "clone", "version": 2, "url": format!("ssh://host/{name}.git")})
    }

    fn staging_empty(&self) -> bool {
        fs::read_dir(self.dir.path().join("repos"))
            .unwrap()
            .all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".wumpa-clone-")
            })
    }
}

fn response(stream: TcpStream) -> Value {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn saved_entry_clone_persists_across_restart_and_agents_are_scoped() {
    let mut fixture = Fixture::new(true);
    let first = fixture.request(fixture.clone_request("saved"));
    assert!(first["error"].is_null(), "{first}");
    assert_eq!(first["checkouts"].as_array().unwrap().len(), 1);
    let root = fixture.dir.path();
    let destination = root.join("repos/saved").canonicalize().unwrap();
    assert_eq!(
        first["checkouts"][0]["checkout_path"],
        destination.to_str().unwrap()
    );
    assert!(destination.join("contents").exists());
    let second_socket = root.join("second-agent");
    let _agent = UnixListener::bind(&second_socket).unwrap();
    let mut request = fixture.clone_request("second");
    request["agent_socket"] = json!(second_socket);
    let second = fixture.request(request);
    assert!(second["error"].is_null(), "{second}");
    let environment = fs::read_to_string(root.join("environment")).unwrap();
    assert!(environment.starts_with(second_socket.canonicalize().unwrap().to_str().unwrap()));
    assert!(environment.contains("\nssh\n0\n"));
    assert!(environment.contains("StrictHostKeyChecking=yes"));
    assert!(environment.contains("IdentityAgent=SSH_AUTH_SOCK"));
    let args = fs::read_to_string(root.join("args")).unwrap();
    assert!(args.contains("--no-recurse-submodules\n--template=\n--\nssh://host/second.git\n"));
    assert!(
        !fs::read_to_string(root.join("server.json"))
            .unwrap()
            .contains("agent")
    );
    assert!(
        fixture.request(fixture.clone_request("saved"))["error"]
            .as_str()
            .unwrap()
            .contains("already cloned")
    );
    fixture.daemon.0.kill().unwrap();
    fixture.daemon.0.wait().unwrap();
    fixture.daemon = Fixture::start(root, fixture.port);
    let list = fixture.request(json!({"action": "list"}));
    assert_eq!(list["checkouts"], second["checkouts"]);
    assert!(fixture.staging_empty());
}

#[test]
fn running_clone_keeps_lists_responsive_rejects_concurrency_and_cancels_tree() {
    let fixture = Fixture::new(true);
    let running = fixture.send(fixture.clone_request("slow"));
    wait(|| fixture.dir.path().join("running").exists());
    let started = Instant::now();
    let list = fixture.request(json!({"action": "list"}));
    assert!(list["error"].is_null());
    assert!(started.elapsed() < Duration::from_secs(1));
    for name in ["slow", "different"] {
        assert!(
            fixture.request(fixture.clone_request(name))["error"]
                .as_str()
                .unwrap()
                .contains("already running")
        );
    }
    drop(running);
    wait(|| fixture.staging_empty());
    let heartbeat = fixture.dir.path().join("heartbeat");
    let length = fs::metadata(&heartbeat).unwrap().len();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        fs::metadata(&heartbeat).unwrap().len(),
        length,
        "descendant survived cancellation"
    );
    assert!(!fixture.dir.path().join("repos/slow").exists());
    assert!(fixture.request(fixture.clone_request("after"))["error"].is_null());
}

#[test]
fn failures_and_destination_races_leave_no_registration_or_owned_staging() {
    let fixture = Fixture::new(true);
    let before = fs::read(fixture.dir.path().join("server.json")).unwrap();
    for name in ["fail", "race"] {
        let result = fixture.request(fixture.clone_request(name));
        let error = result["error"].as_str().unwrap();
        assert!(!error.contains('\x1b'));
        assert_eq!(
            fs::read(fixture.dir.path().join("server.json")).unwrap(),
            before
        );
        assert!(fixture.staging_empty());
    }
    assert!(!fixture.dir.path().join("repos/fail").exists());
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("repos/race/keep")).unwrap(),
        "keep\n"
    );
    // Replace only the config path to force an atomic-save error after Git exits.
    fs::remove_file(fixture.dir.path().join("server.json")).unwrap();
    fs::create_dir(fixture.dir.path().join("server.json")).unwrap();
    let failure = fixture.request(fixture.clone_request("rollback"));
    assert!(
        failure["error"].as_str().unwrap().contains("rolled back"),
        "{failure}"
    );
    assert!(!fixture.dir.path().join("repos/rollback").exists());
    assert!(fixture.staging_empty());
    assert_eq!(failure["checkouts"].as_array().unwrap().len(), 1);
}

#[test]
fn helper_executes_clone_and_stdin_eof_cancels_the_git_tree() {
    let fixture = Fixture::new(true);
    for name in ["helper", "slow"] {
        let mut child = Process(
            Command::new(env!("CARGO_BIN_EXE_wumpa"))
                .arg("clone-helper")
                .env("SSH_AUTH_SOCK", fixture.dir.path().join("agent"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut input = child.0.stdin.take().unwrap();
        writeln!(
            input,
            "{}",
            json!({"version": 2, "port": fixture.port, "clone": true,
            "url": format!("ssh://host/{name}.git")})
        )
        .unwrap();
        if name == "slow" {
            wait(|| fixture.dir.path().join("running").exists());
            drop(input);
            wait(|| child.0.try_wait().unwrap().is_some());
            wait(|| fixture.staging_empty());
            assert!(!fixture.dir.path().join("repos/slow").exists());
        } else {
            wait(|| child.0.try_wait().unwrap().is_some());
            let mut line = String::new();
            BufReader::new(child.0.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let result: Value = serde_json::from_str(&line).unwrap();
            assert!(result["error"].is_null(), "{result}");
            assert!(fixture.dir.path().join("repos/helper/contents").exists());
            drop(input);
        }
    }
}

#[test]
fn real_git_clones_disposable_repository_through_offline_ssh_double() {
    let fixture = Fixture::new(false);
    // Cloning must not depend on the long-lived server's inherited directory.
    fs::remove_dir(fixture.dir.path().join("server-cwd")).unwrap();
    let origin = fixture.dir.path().join("origin");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    git(&["init", origin.to_str().unwrap()]);
    fs::write(origin.join("hello"), "offline fixture").unwrap();
    git(&["-C", origin.to_str().unwrap(), "add", "hello"]);
    git(&[
        "-C",
        origin.to_str().unwrap(),
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-m",
        "fixture",
    ]);
    let result = fixture.request(json!({"action": "clone", "version": 2,
        "url": format!("ssh://host{}", origin.display()), "folder_name": "actual"}));
    assert!(result["error"].is_null(), "{result}");
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("repos/actual/hello")).unwrap(),
        "offline fixture"
    );
    let checkout = fixture.dir.path().join("repos/actual");
    let linked = fixture.dir.path().join("external worktree");
    git(&[
        "-C",
        checkout.to_str().unwrap(),
        "worktree",
        "add",
        "-b",
        "feature",
        linked.to_str().unwrap(),
    ]);
    let snapshot = fixture.request(json!({"action": "list"}));
    let groups = snapshot["worktrees"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert!(groups[0]["error"].is_null());
    let entries = groups[0]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|entry| entry["branch"] == "feature"
        && entry["path"] == linked.canonicalize().unwrap().to_str().unwrap()));
    fs::remove_dir_all(&linked).unwrap();
    let snapshot = fixture.request(json!({"action": "list"}));
    assert!(
        snapshot["worktrees"][0]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["prunable"] == true)
    );
    assert!(fixture.staging_empty());
}
