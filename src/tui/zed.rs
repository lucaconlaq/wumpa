//! Launch the local Zed CLI for a remote checkout without blocking the dashboard.

use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use crate::{Result, config::Connection};

/// Build Zed's SSH URL using the SSH destination, not the daemon's TCP port.
pub(super) fn target(connection: &Connection, checkout: &Path) -> Result<String> {
    let Connection::Ssh { host, .. } = connection else {
        return Err(
            "Opening Zed over SSH requires an SSH connection; select an SSH server.".into(),
        );
    };
    let path = checkout
        .to_str()
        .ok_or("Zed requires a UTF-8 checkout path")?;
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return Err("Zed requires an absolute checkout path without control characters".into());
    }
    // Reuse strict SSH authority validation, separately from the checkout path.
    // Paths may contain spaces, Unicode, and URI delimiters and are encoded below.
    if host.contains(['/', '?', '#', '%']) {
        return Err("Invalid SSH destination for Zed; use an SSH alias or user@host".into());
    }
    crate::repository::folder_name(&format!("ssh://{host}/checkout"), None)
        .map_err(|_| "Invalid SSH destination for Zed; use an SSH alias or user@host")?;
    let mut url = format!("ssh://{host}");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            url.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(url, "%{byte:02X}")?;
        }
    }
    Ok(url)
}

pub(super) struct Launch {
    child: Child,
    deadline: Instant,
}

impl Launch {
    /// Start a CLI handoff. Zed manages its own GUI and SSH authentication prompts.
    pub(super) fn start(target: &str) -> Result<Self> {
        Self::spawn(Command::new("zed"), target)
    }

    fn spawn(mut command: Command, target: &str) -> Result<Self> {
        let child = command
            .arg("--")
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    "Zed CLI not found. In Zed, run 'cli: install', then ensure zed is on PATH."
                        .to_owned()
                } else {
                    format!("Could not launch Zed: {error}")
                }
            })?;
        Ok(Self {
            child,
            deadline: Instant::now() + Duration::from_secs(30),
        })
    }

    /// Poll only the CLI handoff; success does not assert that SSH has connected.
    pub(super) fn poll(&mut self) -> Option<Result<()>> {
        match self.child.try_wait() {
            Ok(Some(status)) if status.success() => Some(Ok(())),
            Ok(Some(status)) => Some(Err(format!("Zed CLI exited with {status}. Check Zed's installation and remote connection settings.").into())),
            Err(error) => Some(Err(format!("Could not check Zed CLI: {error}").into())),
            Ok(None) if Instant::now() >= self.deadline => Some(Err("Zed CLI handoff timed out. Check Zed; it may already have opened.".into())),
            Ok(None) => None,
        }
    }
}

impl Drop for Launch {
    fn drop(&mut self) {
        // Reap only our CLI child. Never signal Zed's GUI or its SSH processes.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_target_uses_the_alias_and_encodes_the_remote_checkout() {
        let connection = Connection::Ssh {
            host: "user@dev-host".into(),
            port: 7432,
        };
        assert_eq!(
            target(&connection, Path::new("/home/user/repo")).unwrap(),
            "ssh://user@dev-host/home/user/repo"
        );
        assert_eq!(
            target(&connection, Path::new("/projects/my repo#1?%/café")).unwrap(),
            "ssh://user@dev-host/projects/my%20repo%231%3F%25/caf%C3%A9"
        );
        let ipv6 = Connection::Ssh {
            host: "user@[::1]".into(),
            port: 7433,
        };
        assert_eq!(
            target(&ipv6, Path::new("/repo")).unwrap(),
            "ssh://user@[::1]/repo"
        );
        assert!(target(&Connection::Local { port: 7432 }, Path::new("/repo")).is_err());
        for path in ["relative", "~/repo", "/repo\n", "/repo\0"] {
            assert!(target(&connection, Path::new(path)).is_err());
        }
        for host in [
            "-option",
            "host/path",
            "user:password@host",
            "host?query",
            "host#fragment",
        ] {
            assert!(
                target(
                    &Connection::Ssh {
                        host: host.into(),
                        port: 7432
                    },
                    Path::new("/repo")
                )
                .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn launcher_passes_one_url_argument_reports_failures_and_reaps() {
        let dir = tempfile::tempdir().unwrap();
        let args = dir.path().join("args");
        let mut command = Command::new("sh");
        command
            .args(["-c", "printf '%s\\n' \"$@\" > \"$TEST_ARGS\"", "mock-zed"])
            .env("TEST_ARGS", &args);
        let url = "ssh://dev-host/projects/my%20repo";
        let mut launch = Launch::spawn(command, url).unwrap();
        loop {
            if let Some(result) = launch.poll() {
                result.unwrap();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(args).unwrap(),
            format!("--\n{url}\n")
        );
        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]);
        let mut launch = Launch::spawn(command, url).unwrap();
        loop {
            if let Some(result) = launch.poll() {
                assert!(result.is_err());
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let mut launch = Launch::spawn(command, url).unwrap();
        let pid = launch.child.id();
        assert!(launch.poll().is_none());
        launch.deadline = Instant::now();
        assert!(
            launch
                .poll()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        drop(launch);
        // SAFETY: signal zero queries only the test child's existence.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        let command = Command::new(dir.path().join("missing-zed"));
        assert!(
            Launch::spawn(command, url)
                .err()
                .unwrap()
                .to_string()
                .contains("not found")
        );
    }
}
