use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use crate::{
    Result,
    config::{self, ClientConfig, Connection, Server},
    protocol::{Request, Response},
    transport::{self, request},
};

// A single reader owns stdin for this CLI process. Clone waits can observe
// cancellation and EOF without leaving competing readers behind for later menus.
struct Input(mpsc::Receiver<io::Result<String>>);

impl Input {
    fn new() -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(16);
        thread::Builder::new()
            .name("plain-input".into())
            .spawn(move || {
                for line in io::stdin().lock().lines() {
                    let failed = line.is_err();
                    if sender.send(line).is_err() || failed {
                        break;
                    }
                }
            })?;
        Ok(Self(receiver))
    }

    fn prompt(&self, label: &str) -> Result<Option<String>> {
        print!("{label}");
        io::stdout().flush()?;
        match self.0.recv() {
            Ok(line) => Ok(Some(line?.trim().into())),
            Err(_) => Ok(None),
        }
    }
}

fn new_server(input: &Input) -> Result<Option<Server>> {
    let Some(name) = input.prompt("Connection name: ")? else {
        return Ok(None);
    };
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    let Some(kind) = input.prompt("Connection type [local/ssh]: ")? else {
        return Ok(None);
    };
    let host = match kind.as_str() {
        "local" => None,
        "ssh" => {
            let Some(host) = input.prompt("SSH host (alias or user@host): ")? else {
                return Ok(None);
            };
            Some(crate::parse_host(&host)?)
        }
        _ => return Err("choose local or ssh".into()),
    };
    let Some(port) = input.prompt("Server port [7432]: ")? else {
        return Ok(None);
    };
    let port = if port.is_empty() {
        7432
    } else {
        port.parse::<u16>()?
    };
    if port == 0 {
        return Err("port must be between 1 and 65535".into());
    }
    let connection = match host {
        Some(host) => Connection::Ssh { host, port },
        None => Connection::Local { port },
    };
    Ok(Some(Server { name, connection }))
}

fn show_repositories(response: &Response) {
    let display_ids = response
        .sessions
        .as_ref()
        .map(crate::session_runtime::RemoteSnapshot::display_ids)
        .unwrap_or_default();
    println!("\nRepositories:");
    if let Some(root) = &response.repository_dir {
        crate::output::info("Server root", root.display());
    } else {
        crate::output::hint("Update the server for cloning and checkout metadata.");
    }
    if response.repositories.is_empty() {
        println!("  No repositories yet.");
    }
    if let Some(snapshot) = &response.sessions {
        if let Some(error) = &snapshot.error {
            crate::output::info("Agents unavailable", error);
        } else if !snapshot.supported {
            crate::output::hint("Agent execution/discovery is unsupported on this server.");
        }
    }
    for (index, entry) in response.repository_entries().iter().enumerate() {
        println!("  {}. {:?}", index + 1, entry.url);
        match &entry.checkout_path {
            Some(path) => crate::output::info("Cloned", path.display()),
            None => println!("     Saved — not cloned"),
        }
        if let Some(path) = &entry.checkout_path {
            show_agents(response, path, "     ", &display_ids);
        }
        if let Some(group) = response
            .worktrees
            .iter()
            .find(|group| group.url == entry.url)
        {
            for worktree in &group.entries {
                if Some(&worktree.path) != entry.checkout_path.as_ref() {
                    crate::output::info(
                        "  🌲 Worktree",
                        format_args!(
                            "{} [{}]{}",
                            worktree.path.display(),
                            worktree.branch.as_deref().unwrap_or(if worktree.bare {
                                "bare"
                            } else {
                                "detached"
                            }),
                            if worktree.prunable { " [prunable]" } else { "" }
                        ),
                    );
                    show_agents(response, &worktree.path, "       ", &display_ids);
                }
            }
            if let Some(error) = &group.error {
                crate::output::info("  Worktrees unavailable", error);
            }
        }
    }
}

fn show_agents(
    response: &Response,
    checkout: &std::path::Path,
    indent: &str,
    display_ids: &BTreeMap<String, String>,
) {
    if let Some(snapshot) = &response.sessions {
        for agent in &snapshot.sessions {
            if agent.checkout == checkout {
                let full_id: String = agent.id.clone().into();
                let id = display_ids
                    .get(&full_id)
                    .map(String::as_str)
                    .unwrap_or(&full_id);
                println!(
                    "{indent}└ 🤖 {} · {} · {}",
                    crate::output::clean(agent.display_name()),
                    id,
                    agent.display_state()
                );
            }
        }
    }
}

struct CloneJob {
    cancelled: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for CloneJob {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// Return true to leave the workspace, false to keep its menu open.
fn clone_repository(
    input: &Input,
    server: &Server,
    message: Request,
    response: &mut Response,
) -> Result<bool> {
    println!("Cloning… Approve your SSH key if prompted. [c] Cancel  [b/q] Back");
    io::stdout().flush()?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let connection = server.connection.clone();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("plain-clone".into())
        .spawn(move || {
            let result = transport::cancellable_request(&connection, &message, &worker_cancelled)
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        })?;
    let _job = CloneJob {
        cancelled,
        worker: Some(worker),
    };
    loop {
        match receiver.try_recv() {
            Ok(Ok(next)) => {
                if let Some(path) = next
                    .preflight
                    .as_ref()
                    .and_then(|value| value.destination.as_ref())
                {
                    crate::output::info("Cloned to", path.display());
                }
                *response = next;
                show_repositories(response);
                return Ok(false);
            }
            Ok(Err(error)) => {
                crate::output::error(error);
                return Ok(false);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("clone worker stopped unexpectedly".into());
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let back = match input.0.try_recv() {
            Ok(Ok(line)) => match line.trim() {
                "c" => Some(false),
                "b" | "q" => Some(true),
                _ => {
                    println!("Clone running. Choose c to cancel or b to go back.");
                    None
                }
            },
            Ok(Err(error)) => return Err(error.into()),
            Err(mpsc::TryRecvError::Disconnected) => Some(true),
            Err(mpsc::TryRecvError::Empty) => None,
        };
        if let Some(back) = back {
            println!("Clone cancellation requested. Refresh to reconcile any completion race.");
            return Ok(back);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn connected(input: &Input, server: &Server) -> Result<()> {
    let mut response = request(&server.connection, &Request::List)?;
    println!("\nConnected to {:?}", server.name);
    show_repositories(&response);
    loop {
        let Some(action) =
            input.prompt("\n[a] Clone repository  [c] Clone saved  [l] List  [b] Back: ")?
        else {
            return Ok(());
        };
        match action.as_str() {
            "a" | "c" => {
                let Some(root) = response.repository_dir.as_ref() else {
                    crate::output::error(
                        "Server lacks clone metadata; update Wumpa on the server and refresh.",
                    );
                    continue;
                };
                let url = if action == "c" {
                    let Some(number) = input.prompt("Saved repository number to clone: ")? else {
                        return Ok(());
                    };
                    let entries = response.repository_entries();
                    let entry = number
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|i| entries.get(i));
                    let Some(entry) = entry else {
                        crate::output::error("Choose a valid repository number.");
                        continue;
                    };
                    if entry.checkout_path.is_some() {
                        crate::output::error("Repository is already cloned.");
                        continue;
                    }
                    entry.url.clone()
                } else {
                    let Some(url) = input.prompt("Repository URL (ssh:// only): ")? else {
                        return Ok(());
                    };
                    url
                };
                let Some(folder) = input.prompt("Folder name [derived from URL]: ")? else {
                    return Ok(());
                };
                let message = match Request::clone_repository(&url, &folder) {
                    Ok(message) => message,
                    Err(error) => {
                        crate::output::error(error);
                        continue;
                    }
                };
                let name = crate::repository::folder_name(
                    &url,
                    (!folder.is_empty()).then_some(folder.as_str()),
                )?;
                crate::output::info("Server root", root.display());
                crate::output::info("Destination", root.join(name).display());
                let Some(answer) = input.prompt("Clone on the server? [y/N]: ")? else {
                    return Ok(());
                };
                if answer.eq_ignore_ascii_case("y")
                    && clone_repository(input, server, message, &mut response)?
                {
                    return Ok(());
                }
            }
            "l" => match request(&server.connection, &Request::List) {
                Ok(next) => {
                    response = next;
                    show_repositories(&response);
                }
                Err(error) => crate::output::error(error),
            },
            "b" | "q" => return Ok(()),
            _ => println!("Choose a, c, l, or b."),
        }
    }
}

/// Run the line-based client menu using the configured client file.
pub fn run() -> Result<()> {
    let input = Input::new()?;
    let path = config::path("client")?;
    let mut config: ClientConfig = config::load(&path)?;
    crate::output::heading("client");
    crate::output::info("Config", path.display());
    loop {
        println!("\nSaved servers:");
        if config.servers.is_empty() {
            println!("  None yet. Choose a to add a connection.");
        }
        for (index, server) in config.servers.iter().enumerate() {
            let target = match &server.connection {
                Connection::Local { port } => format!("local :{port}"),
                Connection::Ssh { host, port } => format!("SSH {host:?} → remote :{port}"),
            };
            println!("  {}. {:?} ({target})", index + 1, server.name);
        }
        let Some(choice) = input
            .prompt("\n[number] Connect  [a] Add server  [d] Remove connection  [q] Quit: ")?
        else {
            return Ok(());
        };
        match choice.as_str() {
            "q" => return Ok(()),
            "d" => {
                let Some(number) = input.prompt("Connection number to remove: ")? else {
                    return Ok(());
                };
                let index = number.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
                let Some(index) = index.filter(|&i| i < config.servers.len()) else {
                    println!("Choose a valid connection number.");
                    continue;
                };
                println!(
                    "Remove {:?}? Only the saved connection is removed; the server and repositories are unchanged.",
                    config.servers[index].name
                );
                let Some(answer) = input.prompt("Remove connection? [y/N]: ")? else {
                    return Ok(());
                };
                if answer.eq_ignore_ascii_case("y") {
                    match config.remove_server(index, &path) {
                        Ok(()) => println!("Connection removed. Server unchanged."),
                        Err(error) => crate::output::error(format_args!(
                            "could not remove connection: {error}"
                        )),
                    }
                }
            }
            "a" => match new_server(&input) {
                Ok(Some(server)) => {
                    if config.servers.iter().any(|entry| entry.name == server.name) {
                        crate::output::error("connection name already exists");
                        continue;
                    }
                    config.servers.push(server);
                    if let Err(error) = config::save(&path, &config) {
                        config.servers.pop();
                        crate::output::error(format_args!("could not save connection: {error}"));
                        continue;
                    }
                    println!("Connection saved. Select its number to connect.");
                }
                Ok(None) => return Ok(()),
                Err(error) => crate::output::error(error),
            },
            _ => {
                let selected = choice
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|n| config.servers.get(n));
                match selected {
                    Some(server) => {
                        if let Err(error) = connected(&input, server) {
                            crate::output::error(format_args!(
                                "connection failed: {error}. Is wumpa serve running on the target?"
                            ));
                        }
                    }
                    None => println!("Choose a server number, a, d, or q."),
                }
            }
        }
    }
}
