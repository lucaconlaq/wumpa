//! Client-side creation handoff. Environments are captured on the server, not sent over TCP.

use std::{path::Path, process::Command};

use crate::{Result, config::Connection, sessions::SessionName};

/// Build an interactive create-and-attach handoff, or a named detached creation.
/// Detached callers supply a name (empty means default) and never read child stdin.
pub fn create(
    connection: &Connection,
    checkout: &Path,
    name: Option<&str>,
    attach: bool,
) -> Result<Command> {
    let path = checkout
        .to_str()
        .ok_or("Agent creation requires a UTF-8 checkout path.")?;
    if !checkout.is_absolute() || path.chars().any(char::is_control) {
        return Err(
            "Agent creation requires an absolute checkout path without control characters.".into(),
        );
    }
    if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
        SessionName::try_from(name.to_owned())?;
    }
    if !attach && name.is_none() {
        return Err("Detached creation requires an explicit name or empty default name.".into());
    }
    let port = match connection {
        Connection::Local { port } | Connection::Ssh { port, .. } => *port,
    };
    match connection {
        Connection::Local { .. } => {
            let mut command = Command::new(std::env::current_exe()?);
            command
                .current_dir(checkout)
                .args(["agent-create", "--port", &port.to_string()]);
            if let Some(name) = name {
                command.args(["--name", name]);
            }
            if !attach {
                command
                    .arg("--no-attach")
                    .stdin(std::process::Stdio::null());
            }
            Ok(command)
        }
        Connection::Ssh { host, .. } => {
            crate::parse_host(host)?;
            if host.chars().any(char::is_control) {
                return Err("Invalid SSH destination.".into());
            }
            let quote = |value: &str| format!("'{}'", value.replace('\'', "'\"'\"'"));
            let mut action = format!("wumpa agent-create --port {port}");
            if let Some(name) = name {
                action.push_str(&format!(" --name {}", quote(name)));
            }
            if !attach {
                action.push_str(" --no-attach");
            }
            let mut command = Command::new("ssh");
            if attach {
                command.arg("-t");
            } else {
                command.stdin(std::process::Stdio::null());
            }
            let remote = format!("cd {} && exec {action}", quote(path));
            let remote = if attach {
                // Match an interactive SSH session's PATH (for example, zshrc).
                // Start in the checkout so startup hooks can load project tools.
                // Keep expansion in sh even when sshd uses a non-POSIX shell,
                // and restore the checkout if startup hooks change directory.
                let shell = format!(
                    "cd {} && exec \"${{SHELL:-/bin/sh}}\" -i -c {}",
                    quote(path),
                    quote(&remote)
                );
                format!("exec /bin/sh -c {}", quote(&shell))
            } else {
                remote
            };
            // The configured port is the daemon port, not sshd's port.
            command.args(["--", host, &remote]);
            Ok(command)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_creation_quotes_paths_and_names_and_uses_daemon_port() {
        let connection = Connection::Ssh {
            host: "dev".into(),
            port: 8123,
        };
        let command = create(
            &connection,
            Path::new("/repo's tree"),
            Some("it's $(touch nope)"),
            false,
        )
        .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "--",
                "dev",
                "cd '/repo'\"'\"'s tree' && exec wumpa agent-create --port 8123 --name 'it'\"'\"'s $(touch nope)' --no-attach"
            ]
        );
        let command = create(&connection, Path::new("/tree"), None, true).unwrap();
        assert_eq!(command.get_args().next().unwrap(), "-t");
    }

    #[cfg(unix)]
    #[test]
    fn interactive_creation_loads_project_shell_path_and_preserves_quoted_arguments() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("repo's $(touch injected) tree");
        let bin = dir.path().join("bin");
        fs::create_dir(&checkout).unwrap();
        fs::write(checkout.join(".project-tools"), "").unwrap();
        fs::create_dir(&bin).unwrap();
        let write_executable = |path: &Path, script: &str| {
            fs::write(path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        };
        write_executable(
            &bin.join("wumpa"),
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$@\"\nexec test-agent\n",
        );
        write_executable(&bin.join("test-agent"), "#!/bin/sh\necho agent-found\n");
        let shell = dir.path().join("interactive shell");
        write_executable(
            &shell,
            "#!/bin/sh\n[ \"$1\" = -i ] && [ \"$2\" = -c ] || exit 91\n[ -f .project-tools ] || exit 92\nexport PATH=\"$TEST_BIN:$PATH\"\ncd /\nexec /bin/sh -c \"$3\"\n",
        );
        fs::write(
            dir.path().join(".zshrc"),
            "[ -f .project-tools ] || exit 92\nexport PATH=\"$TEST_BIN:$PATH\"\ncd /\n",
        )
        .unwrap();
        let connection = Connection::Ssh {
            host: "dev".into(),
            port: 8123,
        };
        let name = "it's $(touch injected); a name";
        let command = create(&connection, &checkout, Some(name), true).unwrap();
        let remote = command.get_args().last().unwrap();
        let mut shells = vec![shell];
        if Path::new("/bin/zsh").exists() {
            shells.push("/bin/zsh".into());
        }
        for shell in shells {
            let output = Command::new("/bin/sh")
                .args([std::ffi::OsStr::new("-c"), remote])
                .current_dir(dir.path())
                .env("SHELL", &shell)
                .env("ZDOTDIR", dir.path())
                .env("TEST_BIN", &bin)
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}: {}",
                shell.display(),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!(
                    "{}\nagent-create\n--port\n8123\n--name\n{name}\nagent-found\n",
                    checkout.display()
                )
            );
            assert!(!dir.path().join("injected").exists());
            assert!(!checkout.join("injected").exists());
        }
    }

    #[test]
    fn local_creation_has_an_explicit_directory_and_default_name() {
        let command = create(
            &Connection::Local { port: 7432 },
            Path::new("/tree"),
            Some(""),
            false,
        )
        .unwrap();
        assert_eq!(command.get_current_dir(), Some(Path::new("/tree")));
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "agent-create",
                "--port",
                "7432",
                "--name",
                "",
                "--no-attach"
            ]
        );
        assert!(
            create(
                &Connection::Local { port: 7432 },
                Path::new("relative"),
                None,
                true
            )
            .is_err()
        );
    }
}
