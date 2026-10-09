//! Open an interactive SSH shell in a selected remote checkout.

use std::{path::Path, process::Command};

use crate::{Result, config::Connection};

pub(super) fn command(connection: &Connection, checkout: &Path) -> Result<Command> {
    // sshd runs commands through the account's login shell, which need not
    // understand POSIX parameter expansion (for example, tcsh or fish).
    remote_command(
        connection,
        checkout,
        "/bin/sh -c 'exec \"${SHELL:-/bin/sh}\" -i'",
    )
}

pub(super) fn attach(
    connection: &Connection,
    checkout: &Path,
    session: &crate::sessions::SessionId,
) -> Result<Command> {
    let port = match connection {
        Connection::Ssh { port, .. } | Connection::Local { port } => port,
    };
    let id: String = session.clone().into();
    remote_command(
        connection,
        checkout,
        &format!("wumpa agent-attach --port {port} --session {id}"),
    )
}

fn remote_command(connection: &Connection, checkout: &Path, action: &str) -> Result<Command> {
    let Connection::Ssh { host, .. } = connection else {
        return Err("SSH requires an SSH connection; select an SSH server.".into());
    };
    crate::parse_host(host)?;
    if host.chars().any(char::is_control) {
        return Err("Invalid SSH destination.".into());
    }
    let path = checkout
        .to_str()
        .ok_or("SSH requires a UTF-8 checkout path.")?;
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return Err("SSH requires an absolute checkout path without control characters.".into());
    }
    // SSH invokes the remote shell, so quote the path even though it is passed
    // as one local argument. Do not start a shell if changing directory fails.
    let quoted = format!("'{}'", path.replace('\'', "'\"'\"'"));
    let remote = format!("cd {quoted} && exec {action}");
    let mut command = Command::new("ssh");
    // The configured port belongs to Wumpa, not sshd. Honor SSH config instead.
    command.args(["-t", "--", host, &remote]);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_uses_alias_and_quotes_checkout_without_daemon_port() {
        let connection = Connection::Ssh {
            host: "user@dev".into(),
            port: 7432,
        };
        let command = command(&connection, Path::new("/projects/it's $(touch nope)")).unwrap();
        assert_eq!(command.get_program(), "ssh");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "-t",
                "--",
                "user@dev",
                "cd '/projects/it'\"'\"'s $(touch nope)' && exec /bin/sh -c 'exec \"${SHELL:-/bin/sh}\" -i'"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_handoff_preserves_directory_and_uses_the_selected_shell() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("it's $(touch injected); a folder");
        fs::create_dir(&checkout).unwrap();
        let shell = dir.path().join("selected shell");
        fs::write(&shell, "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$1\"\n").unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
        let connection = Connection::Ssh {
            host: "dev".into(),
            port: 7432,
        };
        let ssh = command(&connection, &checkout).unwrap();
        let remote = ssh.get_args().last().unwrap();
        // Exercise non-POSIX login shells when available, without requiring
        // extra test dependencies on every supported platform.
        for login in ["/bin/sh", "/bin/tcsh", "/usr/bin/fish"] {
            if !Path::new(login).exists() {
                continue;
            }
            let mut process = Command::new(login);
            if login.ends_with("tcsh") {
                process.arg("-f");
            }
            let output = process
                .current_dir(dir.path())
                .arg("-c")
                .arg(remote)
                .env("SHELL", &shell)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{login}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("{}\n-i\n", checkout.display())
            );
            assert!(!dir.path().join("injected").exists());
        }
        let missing = command(&connection, &dir.path().join("missing")).unwrap();
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(missing.get_args().last().unwrap())
            .env("SHELL", &shell)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "must not launch a shell when cd fails"
        );
    }

    #[test]
    fn attachment_uses_fixed_helper_and_validated_id() {
        let connection = Connection::Ssh {
            host: "dev".into(),
            port: 8123,
        };
        let id = crate::sessions::SessionId::try_from("a".repeat(32)).unwrap();
        let command = attach(&connection, Path::new("/repo's folder"), &id).unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(&args[..3], &["-t", "--", "dev"]);
        assert_eq!(
            args[3],
            format!(
                "cd '/repo'\"'\"'s folder' && exec wumpa agent-attach --port 8123 --session {}",
                "a".repeat(32)
            )
        );
    }

    #[test]
    fn rejects_local_connections_and_invalid_targets() {
        assert!(command(&Connection::Local { port: 7432 }, Path::new("/repo")).is_err());
        for host in ["", "-oProxyCommand=bad", "host\n", "host\0"] {
            let connection = Connection::Ssh {
                host: host.into(),
                port: 7432,
            };
            assert!(command(&connection, Path::new("/repo")).is_err());
        }
        let connection = Connection::Ssh {
            host: "dev".into(),
            port: 7432,
        };
        for path in ["relative", "/repo\n", "/repo\0"] {
            assert!(command(&connection, Path::new(path)).is_err());
        }
    }
}
