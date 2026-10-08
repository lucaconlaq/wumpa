#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    process::Command,
    time::Duration,
};

struct Detached(u32);
impl Drop for Detached {
    fn drop(&mut self) {
        // SAFETY: this PID was returned by the daemon launched in this test.
        unsafe {
            libc::kill(self.0 as libc::pid_t, libc::SIGTERM);
        }
    }
}

#[test]
fn detached_server_survives_launcher_and_uses_a_new_session() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("server.json");
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let output = Command::new(env!("CARGO_BIN_EXE_wumpa"))
        .args(["serve", "-d", "--port", &port.to_string()])
        .env("WUMPA_SERVER_CONFIG", &config)
        .env("HOME", directory.path())
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let pid: u32 = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("PID").map(str::trim))
        .unwrap()
        .parse()
        .unwrap();
    let _daemon = Detached(pid);
    assert!(stdout.contains("WUMPA"));
    assert!(stdout.contains("Listening · detached"));
    assert!(stdout.contains(&format!("kill {pid}")));
    assert!(!stdout.contains('\u{1b}'));
    assert!(!stdout.contains("(w)"));
    // SAFETY: getsid only queries the session ID of the child process.
    assert_eq!(
        unsafe { libc::getsid(pid as libc::pid_t) },
        pid as libc::pid_t
    );

    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(b"{\"action\":\"add\",\"url\":\"ssh://git@example.com/detached.git\"}\n")
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(response["error"].is_null());
    assert_eq!(
        response["repositories"][0],
        "ssh://git@example.com/detached.git"
    );
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(saved.contains("detached.git"));
    let log = std::fs::read_to_string(directory.path().join("server.json.log")).unwrap();
    assert!(log.contains("WUMPA"));
    assert!(!log.contains('\u{1b}'));
}

#[test]
fn occupied_port_is_reported_as_startup_failure() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let output = Command::new(env!("CARGO_BIN_EXE_wumpa"))
        .args(["serve", "--detach", "--port", &port.to_string()])
        .env("WUMPA_SERVER_CONFIG", directory.path().join("server.json"))
        .env("HOME", directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("server failed to start"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Listening"));
}
