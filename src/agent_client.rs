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
            // The configured port is the daemon port, not sshd's port.
            command.args(["--", host, &format!("cd {} && exec {action}", quote(path))]);
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
