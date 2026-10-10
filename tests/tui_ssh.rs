//! Exercise dashboard SSH handoff in a private PTY, without network access.

#![cfg(unix)]

use std::{
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    process::{Child, Command},
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

fn wait_for(master: &mut File, screen: &mut Vec<u8>, needle: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut answered = screen
        .windows(4)
        .filter(|bytes| *bytes == b"\x1b[6n")
        .count();
    while !screen.windows(needle.len()).any(|bytes| bytes == needle) {
        assert!(
            Instant::now() < deadline,
            "PTY output did not contain {needle:?}"
        );
        let mut buffer = [0; 8192];
        match master.read(&mut buffer) {
            Ok(0) => panic!("PTY closed before expected output"),
            Ok(count) => screen.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("PTY read failed: {error}"),
        }
        let queries = screen
            .windows(4)
            .filter(|bytes| *bytes == b"\x1b[6n")
            .count();
        while answered < queries {
            master.write_all(b"\x1b[1;1R").unwrap();
            answered += 1;
        }
    }
}

#[test]
fn ssh_and_failure_prompt_have_visible_cursor_then_dashboard_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = dir.path().join("ssh");
    fs::write(&ssh, r#"#!/bin/sh
if [ "$1" = "-T" ]; then
    printf '%s\n' '{"repositories":["ssh://host/fixture-repo.git"],"checkouts":[{"url":"ssh://host/fixture-repo.git","checkout_path":"/tmp"}],"error":null}'
else
    printf '\nSSH_TEST_READY\n'
    exit 2
fi
"#).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let config = dir.path().join("client.json");
    fs::write(&config, r#"{"servers":[{"name":"Fixture","type":"ssh","host":"mock","port":7432}],"last_server":"Fixture"}"#).unwrap();
    let (mut master_fd, mut slave_fd) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: descriptors and size are valid output/input pointers; null uses
    // default terminal attributes and omits the optional device-name output.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
            )
        },
        0
    );
    // SAFETY: openpty returned distinct owned descriptors.
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    // SAFETY: fcntl operates on the live master descriptor.
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(flags, -1);
    assert_ne!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        -1
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_wumpa"));
    command
        .env("PATH", dir.path())
        .env("WUMPA_CLIENT_CONFIG", &config)
        .env("TERM", "xterm-256color")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    // SAFETY: only async-signal-safe syscalls run between fork and exec, creating
    // a controlling terminal for this isolated test child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = Process(command.spawn().unwrap());
    let mut screen = Vec::new();
    wait_for(&mut master, &mut screen, b"fixture-repo");
    master.write_all(b"\r").unwrap();
    wait_for(&mut master, &mut screen, b"Press Enter to return to Wumpa.");
    let marker = screen
        .windows(14)
        .position(|bytes| bytes == b"SSH_TEST_READY")
        .unwrap();
    let visible = |bytes: &[u8]| {
        bytes
            .windows(6)
            .rfind(|bytes| *bytes == b"\x1b[?25h" || *bytes == b"\x1b[?25l")
            == Some(b"\x1b[?25h".as_slice())
    };
    assert!(visible(&screen[..marker]), "SSH inherited a hidden cursor");
    assert!(
        visible(&screen),
        "failure acknowledgement has a hidden cursor"
    );
    screen.clear();
    master.write_all(b"\n").unwrap();
    wait_for(&mut master, &mut screen, b"\x1b[?1049h");
    wait_for(&mut master, &mut screen, b"fixture-repo");
    master.write_all(b"q").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "dashboard did not exit");
        // Drain the rest of the redraw so a full PTY buffer cannot block the
        // dashboard before it reads the quit key.
        let mut buffer = [0; 8192];
        match master.read(&mut buffer) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("PTY read failed: {error}"),
        }
        thread::sleep(Duration::from_millis(10));
    }
}
