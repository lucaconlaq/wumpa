use clap::{Parser, Subcommand};
use std::process::{Command, ExitCode};

mod agent;
mod checkout;
mod client;
mod clone;
mod config;
mod control;
mod daemon;
mod helper;
mod output;
mod protocol;
mod repository;
mod server;
mod session_cli;
mod session_environment;
mod session_runtime;
mod sessions;
mod transport;
mod tui;
mod worktrees;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Use plain text menus instead of interactive terminal interfaces
    #[arg(long, global = true)]
    plain: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the local-only server (use SSH forwarding for remote access)
    Serve {
        /// Absolute control socket in an existing private (0700) directory
        #[arg(long)]
        socket: std::path::PathBuf,
        #[arg(long, default_value_t = 7432, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        /// Detach into the background, logging beside the server config
        #[arg(short = 'd', long)]
        detach: bool,
        #[arg(long, hide = true, conflicts_with = "detach")]
        ready_file: Option<std::path::PathBuf>,
    },
    /// Create or attach a coding agent in a registered checkout (Linux servers only)
    Agent {
        /// Absolute local control socket; no TCP/config fallback
        #[arg(long)]
        socket: std::path::PathBuf,
        /// Recover an uncertain creation without launching a duplicate (RUN:REQUEST)
        #[arg(long)]
        retry: Option<String>,
    },
    /// Internal SSH entry point for attachment to an existing agent
    #[command(hide = true)]
    AgentAttach {
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        #[arg(long)]
        session: String,
    },
    /// Internal persistent agent supervisor
    #[command(hide = true)]
    AgentRunner {
        #[arg(long)]
        channel: std::path::PathBuf,
        #[arg(long)]
        id: String,
    },
    /// Check SSH-agent handoff and a clone destination without cloning or saving
    CheckClone {
        /// SSH server alias/destination; omit to reach the local daemon
        #[arg(long, value_parser = parse_host)]
        host: Option<String>,
        #[arg(long, default_value_t = 7432, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        #[arg(long)]
        url: String,
        #[arg(long)]
        folder: Option<String>,
    },
    /// Internal session-aware SSH relay
    #[command(hide = true)]
    CloneHelper,
    /// Inspect remote Git worktrees
    Worktrees {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
}

#[derive(Subcommand)]
enum WorktreeCommand {
    /// List a repository's worktrees over SSH
    List {
        /// SSH destination, including aliases from ~/.ssh/config
        #[arg(long, value_parser = parse_host)]
        host: String,
        /// Absolute path to an existing remote Git checkout
        #[arg(long, value_parser = parse_repo)]
        repo: String,
    },
}

fn parse_host(value: &str) -> std::result::Result<String, String> {
    if value.is_empty() || value.starts_with('-') || value.chars().any(char::is_whitespace) {
        return Err("expected an SSH destination such as my-server or user@host".into());
    }
    Ok(value.into())
}

fn parse_repo(value: &str) -> std::result::Result<String, String> {
    if !value.starts_with('/') || value.contains('\0') {
        return Err("expected an absolute remote path, such as /projects/app".into());
    }
    Ok(value.into())
}

fn remote_command(repo: &str) -> String {
    // SSH passes the command to a remote shell; local argument separation alone
    // does not protect paths containing spaces, quotes, or shell metacharacters.
    let quoted_repo = format!("'{}'", repo.replace('\'', "'\"'\"'"));
    format!("git -C {quoted_repo} worktree list")
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None if cli.plain => client::run(),
        None => tui::run(),
        Some(Commands::Serve {
            socket,
            port,
            detach,
            ready_file,
        }) => {
            control::validate_path(&socket)?;
            if detach {
                daemon::detach(port, &socket)
            } else {
                server::serve(port, &socket, ready_file.as_deref())
            }
        }
        Some(Commands::Agent { socket, retry }) => {
            session_cli::run(&socket, retry.as_deref(), cli.plain)
        }
        Some(Commands::AgentAttach { port, session }) => {
            session_cli::attach_remote(port, sessions::SessionId::try_from(session)?)
        }
        Some(Commands::AgentRunner { channel, id }) => session_runtime::run_agent(&channel, &id),
        Some(Commands::CloneHelper) => helper::run(),
        Some(Commands::CheckClone {
            host,
            port,
            url,
            folder,
        }) => {
            let connection = match host {
                Some(host) => config::Connection::Ssh { host, port },
                None => config::Connection::Local { port },
            };
            let response = transport::request(
                &connection,
                &protocol::Request::PrepareClone {
                    version: protocol::HELPER_VERSION,
                    url,
                    folder_name: folder,
                    agent_socket: None,
                },
            )?;
            let destination = response
                .preflight
                .and_then(|value| value.destination)
                .ok_or("clone preflight returned no destination; update both client and server")?;
            output::info("Destination", destination.display());
            output::hint(
                "Preflight passed. Agent socket validated; no clone or key approval attempted.",
            );
            Ok(())
        }
        Some(Commands::Worktrees {
            command: WorktreeCommand::List { host, repo },
        }) => {
            let status = Command::new("ssh")
                .arg("--")
                .arg(host)
                .arg(remote_command(&repo))
                .status()?;
            if status.success() {
                Ok(())
            } else {
                Err("SSH/Git command failed".into())
            }
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            output::error(error);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_accepts_plain_before_or_after_the_subcommand() {
        for arguments in [
            vec![
                "wumpa",
                "--plain",
                "agent",
                "--socket",
                "/private/control.sock",
            ],
            vec![
                "wumpa",
                "agent",
                "--plain",
                "--socket",
                "/private/control.sock",
            ],
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            assert!(cli.plain);
            assert!(matches!(cli.command, Some(Commands::Agent { .. })));
        }
    }

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_worktree_list() {
        let cli = Cli::try_parse_from([
            "wumpa",
            "worktrees",
            "list",
            "--host",
            "dev",
            "--repo",
            "/projects/app",
        ])
        .unwrap();
        let Some(Commands::Worktrees {
            command: WorktreeCommand::List { host, repo },
        }) = cli.command
        else {
            panic!("expected worktrees list")
        };
        assert_eq!(host, "dev");
        assert_eq!(repo, "/projects/app");
    }

    #[test]
    fn requires_host_and_repo() {
        assert!(Cli::try_parse_from(["wumpa", "worktrees", "list"]).is_err());
    }

    #[test]
    fn rejects_invalid_destinations_and_relative_paths() {
        for host in ["", "-oProxyCommand=evil", "host with spaces"] {
            assert!(parse_host(host).is_err());
        }
        for repo in ["", "projects/app", "~/app", "/app\0"] {
            assert!(parse_repo(repo).is_err());
        }
    }

    #[test]
    fn quotes_remote_paths() {
        assert_eq!(
            remote_command("/projects/my app"),
            "git -C '/projects/my app' worktree list"
        );
        assert_eq!(
            remote_command("/projects/user's $(app);"),
            "git -C '/projects/user'\"'\"'s $(app);' worktree list"
        );
    }
}
