use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;

use crate::{
    Result,
    config::{self, ClientConfig, Connection, Repository, Server},
    protocol::{Request, Response},
    transport,
};

mod github;
mod ssh;
mod view;
mod zed;

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Servers,
    Repositories,
}

#[derive(Clone, Copy, PartialEq)]
enum FormKind {
    Server,
    Repository,
}

struct Form {
    kind: FormKind,
    values: Vec<String>,
    field: usize,
    error: String,
    saved_url: Option<String>,
}

impl Form {
    fn new(kind: FormKind) -> Self {
        Self {
            kind,
            values: match kind {
                FormKind::Server => vec![String::new(), String::new(), "7432".into()],
                FormKind::Repository => vec![String::new(), String::new()],
            },
            field: 0,
            error: String::new(),
            saved_url: None,
        }
    }

    fn repository_url(&self) -> Result<String> {
        let url = github::url(&self.values[0])?;
        // Preserve the saved URL (including an omitted .git suffix) so cloning
        // updates the existing record instead of creating a duplicate.
        if let Some(saved) = &self.saved_url {
            if github::name(saved)
                .and_then(|name| github::url(&name))
                .ok()
                .as_ref()
                == Some(&url)
            {
                return Ok(saved.trim().into());
            }
        }
        Ok(url)
    }

    fn server(&self) -> Result<Server> {
        let name = self.values[0].trim();
        if name.is_empty() {
            return Err("Give this connection a name.".into());
        }
        let host = self.values[1].trim();
        let port = self.values[2]
            .trim()
            .parse::<u16>()
            .map_err(|_| "Port must be a number from 1 to 65535.")?;
        if port == 0 {
            return Err("Port must be a number from 1 to 65535.".into());
        }
        let connection = if host.is_empty() {
            Connection::Local { port }
        } else {
            Connection::Ssh {
                host: crate::parse_host(host)?,
                port,
            }
        };
        Ok(Server {
            name: name.into(),
            connection,
        })
    }
}

struct Job {
    target: usize,
    cloning: bool,
    receiver: mpsc::Receiver<std::result::Result<Response, String>>,
    cancelled: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        // Reap SSH before leaving the terminal, including on errors and quit.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct App {
    config: ClientConfig,
    path: PathBuf,
    servers: ListState,
    repos: ListState,
    repositories: Vec<Repository>,
    worktrees: Vec<crate::worktrees::RepositoryWorktrees>,
    sessions: Option<crate::session_runtime::RemoteSnapshot>,
    repository_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    connected: Option<usize>,
    // Keep the workspace visible during connection attempts and failures.
    workspace: Option<usize>,
    pane: Pane,
    form: Option<Form>,
    remove_target: Option<usize>,
    delete_target: Option<crate::deletion::Target>,
    delete_prompt: Option<crate::deletion::Prompt>,
    delete_second: bool,
    delete_input: String,
    details: bool,
    details_scroll: u16,
    zed_launch: Option<zed::Launch>,
    ssh_command: Option<std::process::Command>,
    job: Option<Job>,
    sessions_job: Option<Job>,
    sessions_updates: bool,
    next_sessions_refresh: Instant,
    sessions_received: Option<Instant>,
    status: String,
    error: bool,
    tick: usize,
}

impl App {
    fn new(config: ClientConfig, path: PathBuf) -> Self {
        let selected = config
            .last_server_index()
            .or((!config.servers.is_empty()).then_some(0));
        Self {
            config,
            path,
            servers: ListState::default().with_selected(selected),
            repos: ListState::default(),
            repositories: vec![],
            worktrees: vec![],
            sessions: None,
            repository_dir: None,
            home_dir: None,
            connected: None,
            workspace: None,
            pane: Pane::Servers,
            form: None,
            remove_target: None,
            delete_target: None,
            delete_prompt: None,
            delete_second: false,
            delete_input: String::new(),
            details: false,
            details_scroll: 0,
            zed_launch: None,
            ssh_command: None,
            job: None,
            sessions_job: None,
            sessions_updates: false,
            next_sessions_refresh: Instant::now(),
            sessions_received: None,
            status: "Welcome. Select a server and press Enter, or press n to add one.".into(),
            error: false,
            tick: 0,
        }
    }

    fn resume(&mut self) {
        if let Some(target) = self.config.last_server_index() {
            self.open_workspace(target);
        }
    }

    fn open_workspace(&mut self, target: usize) {
        self.job = None;
        self.sessions_job = None;
        self.workspace = Some(target);
        self.servers.select(Some(target));
        self.pane = Pane::Repositories;
        if self.connected != Some(target) {
            self.connected = None;
            self.repositories.clear();
            self.worktrees.clear();
            self.sessions = None;
            self.sessions_updates = false;
            self.sessions_received = None;
            self.repository_dir = None;
            self.home_dir = None;
            self.repos.select(None);
        }
        self.start(target, Request::List);
    }

    fn switch_servers(&mut self) {
        self.job = None;
        self.sessions_job = None;
        self.details = false;
        self.zed_launch = None;
        self.pane = Pane::Servers;
        if let Some(target) = self.workspace {
            self.servers.select(Some(target));
        }
        self.status = "Choose a workspace. Enter to open · n to add a server.".into();
        self.error = false;
    }

    fn start(&mut self, target: usize, request: Request) {
        if self.job.is_some() {
            return;
        }
        self.sessions_job = None;
        self.details = false;
        self.zed_launch = None;
        let cloning = matches!(request, Request::Clone { .. });
        let connection = self.config.servers[target].connection.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let (sender, receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            let result = transport::cancellable_request(&connection, &request, &worker_cancelled)
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
        self.status = if cloning {
            "Cloning on the server… Approve your SSH key if prompted. Esc cancels; refresh to reconcile.".into()
        } else {
            format!("Contacting {}…", self.config.servers[target].name)
        };
        self.error = false;
        self.job = Some(Job {
            target,
            cloning,
            receiver,
            cancelled,
            thread: Some(thread),
        });
    }

    fn poll(&mut self) {
        self.poll_sessions();
        self.tick = self.tick.wrapping_add(1);
        if let Some(result) = self.zed_launch.as_mut().and_then(zed::Launch::poll) {
            self.zed_launch = None;
            match result {
                Ok(()) => {
                    self.status = "Sent to Zed. Complete any SSH prompts in Zed.".into();
                    self.error = false;
                }
                Err(error) => {
                    self.status = error.to_string();
                    self.error = true;
                }
            }
        }
        let Some(job) = &self.job else { return };
        let result = match job.receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Network worker stopped unexpectedly.".into())
            }
        };
        let target = job.target;
        let cloning = job.cloning;
        self.job = None;
        match result {
            Ok(response) => {
                self.delete_prompt = response.deletion.clone();
                self.delete_second = false;
                self.delete_input.clear();
                if self.delete_prompt.is_none() {
                    self.delete_target = None;
                }
                self.connected = Some(target);
                self.workspace = Some(target);
                self.details = false;
                let previous_agent = self.selected_agent().map(|agent| agent.id.clone());
                let previous = self.selected_checkout().map(|(repo, worktree)| {
                    (repo.url.clone(), worktree.map(|entry| entry.path.clone()))
                });
                self.repositories = response.repository_entries();
                self.worktrees = response.worktrees;
                self.sessions_updates = response.sessions_updates;
                self.sessions_received = Some(Instant::now());
                self.next_sessions_refresh = Instant::now() + Duration::from_secs(1);
                if let Some(snapshot) = response.sessions {
                    self.update_sessions(snapshot);
                } else {
                    self.sessions = None;
                }
                self.repository_dir = response.repository_dir;
                self.home_dir = response.home_dir;
                let selected = if self.repositories.is_empty() {
                    None
                } else {
                    Some(
                        self.dashboard_rows()
                            .iter()
                            .position(|(index, worktree, agent)| {
                                previous.as_ref().is_some_and(|(url, path)| {
                                    self.repositories[*index].url == *url
                                        && worktree.map(|entry| &entry.path) == path.as_ref()
                                        && agent.map(|agent| &agent.id) == previous_agent.as_ref()
                                })
                            })
                            .unwrap_or(0),
                    )
                };
                self.repos.select(selected);
                self.pane = Pane::Repositories;
                self.status = if cloning {
                    match response.preflight.and_then(|value| value.destination) {
                        Some(path) => format!("Cloned to {}", path.display()),
                        None => "Clone finished. Refresh to verify checkout metadata.".into(),
                    }
                } else {
                    "Workspace up to date. Press i for details or z to open in Zed.".into()
                };
                self.error = false;
                if let Err(error) = self.config.remember_server(target, &self.path) {
                    self.status = format!("Connected, but could not remember this server: {error}");
                    self.error = true;
                }
            }
            Err(error) => {
                if let Some(snapshot) = &mut self.sessions {
                    snapshot.invalidate_activity();
                }
                self.status = error;
                self.error = true;
            }
        }
    }

    // Activity refresh is a separate bounded, cancellable job. It never changes
    // the current form/details/status/pane or triggers repository discovery.
    fn poll_sessions(&mut self) {
        if self
            .sessions_received
            .is_some_and(|received| received.elapsed() >= Duration::from_secs(15))
        {
            if let Some(snapshot) = &mut self.sessions {
                snapshot.invalidate_activity();
            }
        }
        let result = self
            .sessions_job
            .as_ref()
            .and_then(|job| match job.receiver.try_recv() {
                Ok(result) => Some((job.target, result)),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some((job.target, Err("activity refresh worker stopped".into())))
                }
            });
        if let Some((target, result)) = result {
            self.sessions_job = None;
            self.next_sessions_refresh =
                Instant::now() + Duration::from_secs(if result.is_ok() { 1 } else { 5 });
            if self.connected == Some(target) && self.workspace == Some(target) {
                match result {
                    Ok(response) if response.sessions_updates => {
                        if let Some(snapshot) = response.sessions {
                            self.update_sessions(snapshot);
                            self.sessions_received = Some(Instant::now());
                        } else {
                            self.sessions_updates = false;
                            if let Some(snapshot) = &mut self.sessions {
                                snapshot.invalidate_activity();
                            }
                        }
                    }
                    Ok(_) => {
                        self.sessions_updates = false;
                        if let Some(snapshot) = &mut self.sessions {
                            snapshot.invalidate_activity();
                        }
                    }
                    Err(_) => {
                        if let Some(snapshot) = &mut self.sessions {
                            snapshot.invalidate_activity();
                        }
                    }
                }
            }
        }
        if self.sessions_job.is_some()
            || self.job.is_some()
            || !self.sessions_updates
            || self.pane != Pane::Repositories
            || self.form.is_some()
            || self.remove_target.is_some()
            || self.delete_prompt.is_some()
            || self.ssh_command.is_some()
            || self.zed_launch.is_some()
            || Instant::now() < self.next_sessions_refresh
        {
            return;
        }
        let Some(target) = self
            .connected
            .filter(|target| self.workspace == Some(*target))
        else {
            return;
        };
        let Some(server) = self.config.servers.get(target) else {
            return;
        };
        let connection = server.connection.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stopping = cancelled.clone();
        let (sender, receiver) = mpsc::channel();
        if let Ok(thread) = thread::Builder::new()
            .name("dashboard-activity".into())
            .spawn(move || {
                let result = transport::cancellable_request(
                    &connection,
                    &Request::SessionStatus {
                        version: crate::protocol::SESSION_STATUS_VERSION,
                    },
                    &stopping,
                )
                .map_err(|error| error.to_string());
                let _ = sender.send(result);
            })
        {
            self.sessions_job = Some(Job {
                target,
                cloning: false,
                receiver,
                cancelled,
                thread: Some(thread),
            });
        } else {
            self.next_sessions_refresh = Instant::now() + Duration::from_secs(5);
        }
    }

    fn update_sessions(&mut self, mut snapshot: crate::session_runtime::RemoteSnapshot) {
        if snapshot.error.is_some() || !snapshot.supported {
            if let Some(previous) = &mut self.sessions {
                previous.error = snapshot.error;
                previous.supported = snapshot.supported;
                previous.invalidate_activity();
                return;
            }
        }
        let previous_agent = self.selected_agent().map(|agent| agent.id.clone());
        let previous_checkout = self
            .selected_checkout()
            .map(|(repo, tree)| (repo.url.clone(), tree.map(|tree| tree.path.clone())));
        if let Some(previous) = &self.sessions {
            snapshot.retain_cached_names(previous);
        }
        self.sessions = Some(snapshot);
        let rows = self.dashboard_rows();
        let matches_checkout = |index: usize, tree: Option<&crate::worktrees::Worktree>| {
            previous_checkout.as_ref().is_some_and(|(url, path)| {
                self.repositories[index].url == *url && tree.map(|tree| &tree.path) == path.as_ref()
            })
        };
        let selected = rows
            .iter()
            .position(|(index, tree, agent)| {
                matches_checkout(*index, *tree)
                    && agent.map(|agent| &agent.id) == previous_agent.as_ref()
            })
            .or_else(|| {
                rows.iter().position(|(index, tree, agent)| {
                    matches_checkout(*index, *tree) && agent.is_none()
                })
            })
            .or((!rows.is_empty()).then_some(0));
        self.repos.select(selected);
    }

    fn paste(&mut self, text: &str) {
        if let Some(form) = &mut self.form {
            let max = if form.kind == FormKind::Repository {
                4096
            } else {
                256
            };
            for c in text.chars().filter(|c| !c.is_control()) {
                if form.values[form.field].len() + c.len_utf8() > max {
                    break;
                }
                form.values[form.field].push(c);
            }
        }
    }

    fn move_selection(&mut self, down: bool) {
        let (state, len) = match self.pane {
            Pane::Servers => (&mut self.servers, self.config.servers.len()),
            Pane::Repositories => {
                let count = self.dashboard_rows().len();
                (&mut self.repos, count)
            }
        };
        if len == 0 {
            return;
        }
        let current = state.selected().unwrap_or(0);
        state.select(Some(if down {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        }));
    }

    fn submit(&mut self, form: &Form) -> Result<()> {
        match form.kind {
            FormKind::Server => {
                let server = form.server()?;
                if self
                    .config
                    .servers
                    .iter()
                    .any(|entry| entry.name == server.name)
                {
                    return Err("A connection with that name already exists.".into());
                }
                self.config.servers.push(server);
                if let Err(error) = config::save(&self.path, &self.config) {
                    self.config.servers.pop();
                    return Err(error);
                }
                self.servers.select(Some(self.config.servers.len() - 1));
                self.pane = Pane::Servers;
                self.status = "Connection saved. Press Enter to connect.".into();
                self.error = false;
            }
            FormKind::Repository => {
                let request = Request::clone_repository(&form.repository_url()?, &form.values[1])?;
                let target = self.connected.ok_or("Connect to a server first.")?;
                if self.repository_dir.is_none() {
                    return Err(
                        "Server lacks clone metadata; update Wumpa on the server and refresh."
                            .into(),
                    );
                }
                self.start(target, request);
            }
        }
        Ok(())
    }

    fn remove_connection(&mut self, index: usize) -> Result<()> {
        self.config.remove_server(index, &self.path)?;
        self.connected = match self.connected {
            Some(active) if active == index => {
                self.repositories.clear();
                self.worktrees.clear();
                self.sessions = None;
                self.repository_dir = None;
                self.home_dir = None;
                self.repos.select(None);
                None
            }
            Some(active) if active > index => Some(active - 1),
            active => active,
        };
        self.workspace = match self.workspace {
            Some(active) if active == index => None,
            Some(active) if active > index => Some(active - 1),
            active => active,
        };
        self.servers.select(if self.config.servers.is_empty() {
            None
        } else {
            Some(index.min(self.config.servers.len() - 1))
        });
        self.status = "Connection removed. The server and its repositories are unchanged.".into();
        self.error = false;
        Ok(())
    }

    /// The saved checkout is the parent; other Git checkouts are children.
    fn checkout_rows(&self) -> Vec<(usize, Option<&crate::worktrees::Worktree>)> {
        let mut rows = Vec::new();
        for (index, repo) in self.repositories.iter().enumerate() {
            rows.push((index, None));
            if let Some(group) = self.worktrees.iter().find(|group| group.url == repo.url) {
                for entry in &group.entries {
                    if Some(&entry.path) != repo.checkout_path.as_ref() {
                        rows.push((index, Some(entry)));
                    }
                }
            }
        }
        rows
    }

    fn dashboard_rows(
        &self,
    ) -> Vec<(
        usize,
        Option<&crate::worktrees::Worktree>,
        Option<&crate::session_runtime::Summary>,
    )> {
        let mut rows = Vec::new();
        for (index, worktree) in self.checkout_rows() {
            rows.push((index, worktree, None));
            let path = worktree
                .map(|entry| &entry.path)
                .or(self.repositories[index].checkout_path.as_ref());
            if let Some(snapshot) = &self.sessions {
                for agent in &snapshot.sessions {
                    if Some(&agent.checkout) == path {
                        rows.push((index, worktree, Some(agent)));
                    }
                }
            }
        }
        rows
    }

    fn finish_delete(&mut self, confirmation: String) {
        self.delete_prompt = None;
        if let (Some(server), Some(target)) = (self.connected, self.delete_target.take()) {
            self.start(
                server,
                Request::Delete {
                    target,
                    confirmation: Some(confirmation),
                },
            );
        }
    }

    fn selected_agent(&self) -> Option<&crate::session_runtime::Summary> {
        self.dashboard_rows()
            .get(self.repos.selected()?)
            .and_then(|(_, _, agent)| *agent)
    }

    fn selected_checkout(&self) -> Option<(&Repository, Option<&crate::worktrees::Worktree>)> {
        let rows = self.dashboard_rows();
        let (index, worktree, _) = *rows.get(self.repos.selected()?)?;
        Some((&self.repositories[index], worktree))
    }

    fn checkout_target(&self) -> Result<(&Connection, &std::path::Path)> {
        let target = self
            .connected
            .filter(|target| self.workspace == Some(*target))
            .ok_or("Connect to a server first.")?;
        let (entry, worktree) = self
            .selected_checkout()
            .ok_or("Select a repository first.")?;
        let metadata = worktree.or_else(|| {
            self.worktrees
                .iter()
                .find(|group| group.url == entry.url)
                .and_then(|group| {
                    group
                        .entries
                        .iter()
                        .find(|worktree| Some(&worktree.path) == entry.checkout_path.as_ref())
                })
        });
        if metadata.is_some_and(|worktree| worktree.bare || worktree.prunable) {
            return Err("This worktree is bare or prunable and cannot be opened.".into());
        }
        let checkout = worktree
            .map(|worktree| &worktree.path)
            .or(entry.checkout_path.as_ref())
            .ok_or("This repository is not cloned. Clone it first.")?;
        Ok((&self.config.servers[target].connection, checkout))
    }

    fn zed_target(&self) -> Result<String> {
        let (connection, checkout) = self.checkout_target()?;
        zed::target(connection, checkout)
    }

    fn create_agent(&mut self) {
        self.sessions_job = None;
        let result = self.checkout_target().and_then(|(connection, checkout)| {
            crate::agent_client::create(connection, checkout, None, true)
        });
        match result {
            Ok(command) => {
                self.details = false;
                self.ssh_command = Some(command);
            }
            Err(error) => {
                self.status = error.to_string();
                self.error = true;
            }
        }
    }

    fn attach_agent(&mut self) {
        self.sessions_job = None;
        let result = (|| {
            let agent = self.selected_agent().ok_or("Select an agent first.")?;
            let (connection, _) = self.checkout_target()?;
            ssh::attach(connection, &agent.checkout, &agent.id)
        })();
        match result {
            Ok(command) => {
                self.details = false;
                self.ssh_command = Some(command);
            }
            Err(error) => {
                self.status = error.to_string();
                self.error = true;
            }
        }
    }

    fn open_ssh(&mut self) {
        self.sessions_job = None;
        if self.selected_agent().is_some() {
            return;
        }
        match self
            .checkout_target()
            .and_then(|(connection, checkout)| ssh::command(connection, checkout))
        {
            Ok(command) => {
                self.details = false;
                self.ssh_command = Some(command);
            }
            Err(error) => {
                self.status = error.to_string();
                self.error = true;
            }
        }
    }

    fn open_zed(&mut self) {
        if self.zed_launch.is_some() {
            return;
        }
        match self
            .zed_target()
            .and_then(|target| zed::Launch::start(&target))
        {
            Ok(launch) => {
                self.zed_launch = Some(launch);
                self.status = "Opening in Zed over SSH…".into();
                self.error = false;
            }
            Err(error) => {
                self.status = error.to_string();
                self.error = true;
            }
        }
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if let Some(prompt) = self.delete_prompt.clone() {
            match key.code {
                KeyCode::Esc => {
                    self.delete_prompt = None;
                    self.delete_target = None;
                }
                KeyCode::Char('n') if !self.delete_second => {
                    self.delete_prompt = None;
                    self.delete_target = None;
                }
                KeyCode::Char('y') if !self.delete_second => {
                    if prompt.second.is_empty() {
                        self.finish_delete(String::new());
                    } else {
                        self.delete_second = true;
                    }
                }
                KeyCode::Char(c) if self.delete_second && !c.is_control() => {
                    self.delete_input.push(c)
                }
                KeyCode::Backspace if self.delete_second => {
                    self.delete_input.pop();
                }
                KeyCode::Enter if self.delete_second && self.delete_input == prompt.second => {
                    self.finish_delete(prompt.second)
                }
                _ => {}
            }
            return false;
        }
        if self.details {
            match key.code {
                KeyCode::Char('i') | KeyCode::Esc => self.details = false,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.details_scroll = self.details_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.details_scroll = self.details_scroll.saturating_sub(1)
                }
                KeyCode::Char('z') => {
                    self.details = false;
                    self.open_zed();
                }
                KeyCode::Enter if self.job.is_none() && self.selected_agent().is_some() => {
                    self.attach_agent();
                }
                KeyCode::Char('n') if self.job.is_none() => self.create_agent(),
                KeyCode::Char('t') if self.job.is_none() => self.open_ssh(),
                KeyCode::Char('s') => self.switch_servers(),
                KeyCode::Char('q') => return true,
                _ => {}
            }
            return false;
        }
        if let Some(index) = self.remove_target {
            match key.code {
                KeyCode::Char('y') => {
                    self.remove_target = None;
                    if let Err(error) = self.remove_connection(index) {
                        self.status = format!("Could not remove connection: {error}");
                        self.error = true;
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') => self.remove_target = None,
                KeyCode::Char('q') => return true,
                _ => {}
            }
            return false;
        }
        if let Some(mut form) = self.form.take() {
            match key.code {
                KeyCode::Esc => return false,
                KeyCode::Tab => form.field = (form.field + 1) % form.values.len(),
                KeyCode::BackTab => {
                    form.field = (form.field + form.values.len() - 1) % form.values.len()
                }
                KeyCode::Enter => {
                    if form.field + 1 < form.values.len() {
                        form.field += 1;
                    } else {
                        match self.submit(&form) {
                            Ok(()) => return false,
                            Err(error) => form.error = error.to_string(),
                        }
                    }
                }
                KeyCode::Backspace => {
                    form.values[form.field].pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.values[form.field].clear()
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !c.is_control() =>
                {
                    let max = if form.kind == FormKind::Repository {
                        4096
                    } else {
                        256
                    };
                    if form.values[form.field].len() + c.len_utf8() <= max {
                        form.values[form.field].push(c);
                    }
                }
                _ => {}
            }
            self.form = Some(form);
            return false;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('s') => self.switch_servers(),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(false),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(true),
            KeyCode::Char('i')
                if self.pane == Pane::Repositories
                    && self.job.is_none()
                    && self.selected_checkout().is_some() =>
            {
                self.details = true;
                self.details_scroll = 0;
            }
            KeyCode::Char('t') if self.pane == Pane::Repositories && self.job.is_none() => {
                self.open_ssh()
            }
            KeyCode::Char('z') if self.pane == Pane::Repositories && self.job.is_none() => {
                self.open_zed()
            }
            KeyCode::Char('d') | KeyCode::Delete
                if self.pane == Pane::Repositories && self.job.is_none() =>
            {
                if let Some(server) = self.connected {
                    let target = if let Some(agent) = self.selected_agent() {
                        Some(crate::deletion::Target::Agent {
                            id: agent.id.clone(),
                        })
                    } else {
                        self.selected_checkout()
                            .map(|(repo, worktree)| match worktree {
                                Some(worktree) => crate::deletion::Target::Worktree {
                                    path: worktree.path.clone(),
                                },
                                None => crate::deletion::Target::Repository {
                                    url: repo.url.clone(),
                                },
                            })
                    };
                    if let Some(target) = target {
                        self.delete_target = Some(target.clone());
                        self.start(
                            server,
                            Request::Delete {
                                target,
                                confirmation: None,
                            },
                        );
                    }
                }
            }
            KeyCode::Char('d') | KeyCode::Delete if self.pane == Pane::Servers => {
                if self.job.is_none() {
                    self.remove_target = self.servers.selected();
                } else {
                    self.status =
                        "Wait for the current request before removing a connection.".into();
                }
            }
            KeyCode::Char('n') if self.pane == Pane::Repositories && self.job.is_none() => {
                self.create_agent()
            }
            KeyCode::Char('n') if self.pane == Pane::Servers && self.job.is_none() => {
                self.form = Some(Form::new(FormKind::Server))
            }
            KeyCode::Char('a')
                if self.pane == Pane::Repositories
                    && self.job.is_none()
                    && self.connected.is_some() =>
            {
                self.form = Some(Form::new(FormKind::Repository))
            }
            KeyCode::Char('c')
                if self.pane == Pane::Repositories
                    && self.job.is_none()
                    && self.connected.is_some() =>
            {
                if let Some((entry, _)) = self.selected_checkout() {
                    if entry.checkout_path.is_some() {
                        self.status = "This repository is already cloned.".into();
                        self.error = true;
                    } else {
                        match github::name(&entry.url) {
                            Ok(name) => {
                                let mut form = Form::new(FormKind::Repository);
                                form.values[0] = name;
                                form.saved_url = Some(entry.url.clone());
                                form.field = 1;
                                self.form = Some(form);
                            }
                            Err(error) => {
                                self.status = error.to_string();
                                self.error = true;
                            }
                        }
                    }
                }
            }
            KeyCode::Enter if self.pane == Pane::Servers && self.job.is_none() => {
                if let Some(target) = self.servers.selected() {
                    self.open_workspace(target);
                }
            }
            KeyCode::Enter
                if self.pane == Pane::Repositories
                    && self.job.is_none()
                    && self.selected_agent().is_some() =>
            {
                self.attach_agent();
            }
            KeyCode::Char('r') | KeyCode::Enter
                if self.pane == Pane::Repositories && self.job.is_none() =>
            {
                if let Some(target) = self.workspace {
                    self.start(target, Request::List);
                }
            }
            KeyCode::Esc if self.job.as_ref().is_some_and(|job| job.cloning) => {
                self.job = None;
                self.status =
                    "Clone cancellation requested. Press r to reconcile any completion race."
                        .into();
                self.error = false;
            }
            KeyCode::Esc => {
                if self.pane == Pane::Servers && self.workspace.is_some() {
                    self.pane = Pane::Repositories;
                    self.status = if self.connected.is_some() {
                        "Workspace ready.".into()
                    } else {
                        "Not connected. Press r to retry or s to switch server.".into()
                    };
                    self.error = false;
                } else {
                    self.switch_servers();
                }
            }
            _ => {}
        }
        false
    }
}

struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stdout(), event::DisableBracketedPaste);
        ratatui::restore();
    }
}

/// Run the interactive dashboard, restoring the terminal and reaping jobs on exit.
pub fn run() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "the TUI needs an interactive terminal; use wumpa --plain for the text menu".into(),
        );
    }
    let path = config::path("client")?;
    let config = config::load(&path)?;
    let mut terminal = ratatui::try_init()?;
    let _restore = RestoreTerminal;
    crossterm::execute!(io::stdout(), event::EnableBracketedPaste)?;
    let mut app = App::new(config, path);
    app.resume();
    loop {
        if let Some(mut command) = app.ssh_command.take() {
            crossterm::execute!(io::stdout(), event::DisableBracketedPaste)?;
            ratatui::restore();
            // Leaving the alternate screen does not restore cursor visibility.
            terminal.show_cursor()?;
            let result = command.status();
            match result {
                Ok(status) if status.success() => {
                    app.status = "SSH session ended.".into();
                    app.error = false;
                }
                Ok(status) => {
                    app.status = format!("SSH exited with {status}.");
                    app.error = true;
                }
                Err(error) => {
                    app.status = format!("Could not start SSH: {error}");
                    app.error = true;
                }
            }
            if app.error {
                // Keep inherited SSH/helper diagnostics visible on the normal
                // screen before returning to the dashboard's alternate screen.
                eprintln!("\n{}", crate::output::clean(&app.status));
                eprintln!(
                    "If agent-attach or agent-create is unrecognized, update wumpa on the remote SSH PATH."
                );
                eprintln!("Press Enter to return to Wumpa.");
                let mut line = String::new();
                io::stdin().read_line(&mut line)?;
            }
            terminal = ratatui::try_init()?;
            crossterm::execute!(io::stdout(), event::EnableBracketedPaste)?;
        }
        app.poll();
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if app.key(key) => break,
                Event::Paste(text) => app.paste(&text),
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    use super::view::{ACCENT, PANEL};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn repository_deletion_requires_two_distinct_confirmations() {
        let mut app = App::new(ClientConfig::default(), PathBuf::from("/unused"));
        app.delete_target = Some(crate::deletion::Target::Repository { url: "repo".into() });
        app.delete_prompt = Some(crate::deletion::Prompt {
            description: "Delete repository?".into(),
            second: "repo".into(),
        });
        app.key(key(KeyCode::Enter));
        assert!(!app.delete_second);
        app.key(key(KeyCode::Char('y')));
        assert!(app.delete_second);
        app.key(key(KeyCode::Enter));
        assert!(app.delete_prompt.is_some());
        for c in "wrong".chars() {
            app.key(key(KeyCode::Char(c)));
        }
        app.key(key(KeyCode::Enter));
        assert!(app.delete_prompt.is_some());
        app.key(key(KeyCode::Esc));
        assert!(app.delete_prompt.is_none());
        assert!(app.delete_target.is_none());
        assert!(app.job.is_none());
    }

    fn complete_job(app: &mut App, target: usize, result: std::result::Result<Response, String>) {
        let (sender, receiver) = mpsc::channel();
        sender.send(result).unwrap();
        app.job = Some(Job {
            target,
            cloning: false,
            receiver,
            cancelled: Arc::new(AtomicBool::new(false)),
            thread: None,
        });
        app.poll();
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn repository_form_rejects_unsupported_urls_without_starting_a_job() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        let mut form = Form::new(FormKind::Repository);
        for url in [
            "https://host/app.git",
            "git@host:app.git",
            "ssh://host/../app",
            "ssh://git@github.com/owner/repo.git",
            "https://github.com/owner/repo",
            "owner/repo/extra",
        ] {
            form.values[0] = url.into();
            assert!(app.submit(&form).is_err());
            assert!(app.job.is_none());
        }
    }

    #[test]
    fn github_forms_preserve_saved_urls_and_refuse_non_github_cloning() {
        let mut form = Form::new(FormKind::Repository);
        form.values[0] = "owner/repo".into();
        assert_eq!(
            form.repository_url().unwrap(),
            "ssh://git@github.com/owner/repo.git"
        );
        form.saved_url = Some("ssh://git@github.com/owner/repo".into());
        assert_eq!(
            form.repository_url().unwrap(),
            "ssh://git@github.com/owner/repo"
        );
        form.values[0] = "owner/another".into();
        assert_eq!(
            form.repository_url().unwrap(),
            "ssh://git@github.com/owner/another.git"
        );
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        app.pane = Pane::Repositories;
        app.repos.select(Some(0));
        app.repositories[0].url = "ssh://git@elsewhere/owner/app.git".into();
        app.key(key(KeyCode::Char('c')));
        assert!(app.form.is_none());
        assert!(app.job.is_none());
        assert!(app.status.contains("GitHub repositories only"));
        assert_eq!(app.repositories.len(), 1);
    }

    #[test]
    fn clone_form_previews_destination_and_prefills_only_uncloned_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        app.pane = Pane::Repositories;
        app.repository_dir = Some(PathBuf::from("/projects"));
        app.repositories[0].url = "ssh://git@github.com/owner/app.git".into();
        app.repos.select(Some(0));
        app.key(key(KeyCode::Char('c')));
        let form = app.form.as_mut().unwrap();
        assert_eq!(form.values[0], "owner/app");
        assert_eq!(form.field, 1);
        form.values[1] = "review".into();
        for (width, height) in [(60, 20), (100, 30)] {
            let text = screen(&mut app, width, height);
            assert!(text.contains("Clone repository"));
            assert!(text.contains("GitHub repository · owner/repo"));
            assert!(text.contains("owner/repo"));
            assert!(text.contains("Server root: /projects"));
            assert!(text.contains("Destination: /projects/review"));
        }
        app.form.as_mut().unwrap().values[1] = "../escape".into();
        app.key(key(KeyCode::Enter));
        assert!(app.job.is_none());
        assert!(!app.form.as_ref().unwrap().error.is_empty());
        app.key(key(KeyCode::Esc));
        app.repositories[0].checkout_path = Some(PathBuf::from("/projects/app"));
        app.key(key(KeyCode::Char('c')));
        assert!(app.form.is_none());
        assert!(app.status.contains("already cloned"));
        let text = screen(&mut app, 100, 30);
        assert!(text.contains("cloned"));
        assert!(!text.contains("/projects/app"));
        app.key(key(KeyCode::Char('i')));
        assert!(screen(&mut app, 100, 30).contains("/projects/app"));
        app.key(key(KeyCode::Esc));
        app.repository_dir = None;
        app.key(key(KeyCode::Char('a')));
        let form = app.form.as_mut().unwrap();
        form.values[0] = "owner/new".into();
        form.field = 1;
        app.key(key(KeyCode::Enter));
        assert!(app.form.as_ref().unwrap().error.contains("update Wumpa"));
        assert!(app.job.is_none());
    }

    #[test]
    fn cloning_reports_success_only_after_the_response_and_cancellation_discards_jobs() {
        use std::{
            io::Read,
            net::{Ipv4Addr, TcpListener},
            time::Instant,
        };
        for action in ["success", "escape", "switch", "quit"] {
            let dir = tempfile::tempdir().unwrap();
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let mut app = removal_app(dir.path().join("client.json"));
            app.config.servers[1].connection = Connection::Local {
                port: listener.local_addr().unwrap().port(),
            };
            app.pane = Pane::Repositories;
            app.repository_dir = Some(PathBuf::from("/projects"));
            let (started, received) = mpsc::channel();
            let responder = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let request = crate::protocol::read_message::<Request>(&mut stream).unwrap();
                let Request::Clone {
                    url,
                    folder_name,
                    agent_socket,
                    ..
                } = request
                else {
                    panic!("expected clone");
                };
                assert_eq!(url, "ssh://git@github.com/owner/app.git");
                assert_eq!(folder_name.as_deref(), Some("review"));
                assert!(agent_socket.is_none());
                started.send(()).unwrap();
                if action == "success" {
                    thread::sleep(Duration::from_millis(50));
                    crate::protocol::write_message(
                        &Response {
                            repositories: vec![url.clone()],
                            checkouts: vec![Repository {
                                url,
                                checkout_path: Some(PathBuf::from("/projects/review")),
                            }],
                            repository_dir: Some(PathBuf::from("/projects")),
                            preflight: Some(crate::protocol::Preflight {
                                version: crate::protocol::HELPER_VERSION,
                                destination: Some(PathBuf::from("/projects/review")),
                            }),
                            ..Response::default()
                        },
                        &mut stream,
                    )
                    .unwrap();
                } else {
                    assert_eq!(
                        stream.read(&mut [0]).unwrap(),
                        0,
                        "cancellation must close the transport"
                    );
                }
            });
            let mut form = Form::new(FormKind::Repository);
            form.values = vec!["owner/app".into(), "review".into()];
            app.submit(&form).unwrap();
            received.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(
                app.repositories
                    .iter()
                    .all(|entry| entry.checkout_path.is_none())
            );
            assert!(screen(&mut app, 100, 30).contains("Cloning"));
            match action {
                "success" => {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while app.job.is_some() {
                        assert!(Instant::now() < deadline);
                        app.poll();
                        thread::sleep(Duration::from_millis(10));
                    }
                    assert!(app.status.contains("Cloned to /projects/review"));
                    assert_eq!(
                        app.repositories[0].checkout_path.as_deref(),
                        Some(std::path::Path::new("/projects/review"))
                    );
                }
                "escape" => {
                    app.key(key(KeyCode::Esc));
                    app.poll();
                    assert!(app.job.is_none());
                    assert!(app.pane == Pane::Repositories);
                    assert!(app.status.contains("cancellation"));
                    assert!(app.repositories[0].checkout_path.is_none());
                }
                "switch" => {
                    app.key(key(KeyCode::Char('s')));
                    app.poll();
                    assert!(app.job.is_none());
                    assert!(app.pane == Pane::Servers);
                    assert!(app.repositories[0].checkout_path.is_none());
                }
                "quit" => {
                    assert!(app.key(key(KeyCode::Char('q'))));
                    drop(app);
                }
                _ => unreachable!(),
            }
            responder.join().unwrap();
        }
    }

    #[test]
    fn new_agent_targets_the_selected_checkout_or_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        app.config.servers[1].connection = Connection::Ssh {
            host: "dev-host".into(),
            port: 8123,
        };
        app.pane = Pane::Repositories;
        app.repositories = vec![Repository {
            url: "ssh://host/repo.git".into(),
            checkout_path: Some(PathBuf::from("/repo")),
        }];
        app.worktrees = vec![crate::worktrees::RepositoryWorktrees {
            url: "ssh://host/repo.git".into(),
            entries: vec![crate::worktrees::Worktree {
                path: PathBuf::from("/linked tree"),
                ..Default::default()
            }],
            error: None,
        }];
        app.repos.select(Some(0));
        app.key(key(KeyCode::Char('n')));
        let command = app.ssh_command.take().unwrap();
        assert_eq!(
            command.get_args().last().unwrap(),
            "cd '/repo' && exec wumpa agent-create --port 8123"
        );
        app.repos.select(Some(1));
        app.key(key(KeyCode::Char('n')));
        let command = app.ssh_command.take().unwrap();
        assert_eq!(
            command.get_args().last().unwrap(),
            "cd '/linked tree' && exec wumpa agent-create --port 8123"
        );
        app.config.servers[1].connection = Connection::Local { port: 8123 };
        app.key(key(KeyCode::Char('n')));
        let command = app.ssh_command.take().unwrap();
        assert_eq!(
            command.get_current_dir(),
            Some(std::path::Path::new("/linked tree"))
        );
        app.repos.select(Some(0));
        app.repositories[0].checkout_path = None;
        app.key(key(KeyCode::Char('n')));
        assert!(app.ssh_command.is_none());
        assert!(app.error);
    }

    #[test]
    fn ssh_action_queues_selected_checkout_and_rejects_uncloned_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        app.config.servers[1].connection = Connection::Ssh {
            host: "dev-host".into(),
            port: 7432,
        };
        app.pane = Pane::Repositories;
        app.repositories = vec![Repository {
            url: "ssh://host/repo.git".into(),
            checkout_path: Some(PathBuf::from("/projects/repo")),
        }];
        app.repos.select(Some(0));
        assert!(screen(&mut app, 100, 30).contains("SSH"));
        app.key(key(KeyCode::Char('t')));
        let command = app.ssh_command.take().unwrap();
        assert_eq!(
            command.get_args().last().unwrap(),
            "cd '/projects/repo' && exec /bin/sh -c 'exec \"${SHELL:-/bin/sh}\" -i'"
        );
        app.details = true;
        app.key(key(KeyCode::Char('t')));
        assert!(!app.details);
        assert!(app.ssh_command.take().is_some());
        app.repositories[0].checkout_path = None;
        app.key(key(KeyCode::Char('t')));
        assert!(app.ssh_command.is_none());
        assert!(app.error);
    }

    #[test]
    fn repository_rows_show_names_only_and_details_are_modal_and_scrollable() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        app.config.servers[1].connection = Connection::Ssh {
            host: "dev-host".into(),
            port: 7432,
        };
        app.pane = Pane::Repositories;
        app.repository_dir = Some(PathBuf::from("/home/remote"));
        app.repositories = vec![
            Repository {
                url: "ssh://git@github.com/owner/repo.git".into(),
                checkout_path: Some(PathBuf::from("/home/remote/repo")),
            },
            Repository {
                url: "ssh://git@github.com/owner/another.git".into(),
                checkout_path: None,
            },
        ];
        app.repos.select(Some(0));
        let text = screen(&mut app, 100, 30);
        for name in ["repo", "another", "Details", "Zed"] {
            assert!(text.contains(name));
        }
        for detail in [
            "github.com",
            "owner",
            "/home/remote",
            "not cloned",
            "Repository details",
        ] {
            assert!(!text.contains(detail), "unexpected {detail}");
        }
        assert_eq!(app.zed_target().unwrap(), "ssh://dev-host/home/remote/repo");
        app.key(key(KeyCode::Char('i')));
        assert!(app.details);
        let text = screen(&mut app, 100, 30);
        for detail in [
            "Repository details",
            "Name: repo",
            "URL: ssh://git@github.com/owner/repo.git",
            "State: Cloned",
            "Checkout: /home/remote/repo",
            "Server: Second",
            "Server root: /home/remote",
            "Zed: ssh://dev-host/home/remote/repo",
        ] {
            assert!(text.contains(detail), "missing {detail}");
        }
        app.key(key(KeyCode::Char('j')));
        assert_eq!(
            app.repos.selected(),
            Some(0),
            "details scrolling must not move selection"
        );
        app.details_scroll = u16::MAX;
        let text = screen(&mut app, 60, 20);
        assert!(text.contains("i/Esc Close"));
        assert!(app.details_scroll < u16::MAX);
        app.key(key(KeyCode::Esc));
        assert!(!app.details);
        assert!(app.pane == Pane::Repositories);
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Char('i')));
        assert_eq!(app.details_scroll, 0);
        assert!(screen(&mut app, 100, 30).contains("Saved — not cloned"));
        app.key(key(KeyCode::Char('z')));
        assert!(!app.details);
        assert!(app.zed_launch.is_none());
        assert!(app.status.contains("not cloned"));
        app.key(key(KeyCode::Char('i')));
        app.key(key(KeyCode::Char('s')));
        assert!(!app.details);
        assert!(app.pane == Pane::Servers);
    }

    #[test]
    fn worktrees_are_nested_and_selection_targets_their_checkout() {
        use crate::worktrees::{RepositoryWorktrees, Worktree};

        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        app.config.servers[1].connection = Connection::Ssh {
            host: "dev-host".into(),
            port: 7432,
        };
        app.pane = Pane::Repositories;
        app.repositories = vec![
            Repository {
                url: "ssh://host/app.git".into(),
                checkout_path: Some("/repo".into()),
            },
            Repository {
                url: "ssh://host/next.git".into(),
                checkout_path: None,
            },
        ];
        app.worktrees = vec![RepositoryWorktrees {
            url: app.repositories[0].url.clone(),
            entries: vec![
                Worktree {
                    path: "/repo".into(),
                    branch: Some("main".into()),
                    ..Default::default()
                },
                Worktree {
                    path: "/external/feature work".into(),
                    branch: Some("feature".into()),
                    ..Default::default()
                },
            ],
            error: None,
        }];
        assert_eq!(app.checkout_rows().len(), 3);
        app.repos.select(Some(0));
        assert_eq!(app.zed_target().unwrap(), "ssh://dev-host/repo");
        let text = screen(&mut app, 100, 30);
        assert!(text.contains("app [main]"));
        assert!(text.contains("└ 🌲"));
        assert!(text.contains("/external/feature work [feature]"));
        app.key(key(KeyCode::Down));
        assert_eq!(
            app.zed_target().unwrap(),
            "ssh://dev-host/external/feature%20work"
        );
        app.key(key(KeyCode::Char('i')));
        assert!(screen(&mut app, 100, 30).contains("Checkout: /external/feature work"));
        app.key(key(KeyCode::Esc));
        app.worktrees[0].entries[1].prunable = true;
        assert!(app.zed_target().is_err());
        app.key(key(KeyCode::Down));
        assert_eq!(
            app.selected_checkout().unwrap().0.url,
            "ssh://host/next.git"
        );
        app.worktrees[0].error = Some("Git discovery failed".into());
        assert!(screen(&mut app, 100, 30).contains("worktrees unavailable"));
    }

    #[test]
    fn agents_render_under_checkouts_and_keep_selection_identity_and_actions() {
        use crate::{
            session_runtime::{RemoteSnapshot, Summary},
            sessions::{SessionId, State},
        };
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        app.config.servers[1].connection = Connection::Ssh {
            host: "dev-host".into(),
            port: 7432,
        };
        app.pane = Pane::Repositories;
        app.repositories = vec![Repository {
            url: "ssh://host/app.git".into(),
            checkout_path: Some("/repo".into()),
        }];
        let first = Summary {
            id: SessionId::try_from(format!("abcde0{}", "a".repeat(26))).unwrap(),
            checkout: "/repo".into(),
            label: "pi".into(),
            state: State::Running,
            activity: crate::agent_activity::Activity::Unknown,
            pi_session_name: None,
            pi_session_id: None,
        };
        let second = Summary {
            id: SessionId::try_from(format!("abcde1{}", "b".repeat(26))).unwrap(),
            ..first.clone()
        };
        app.sessions = Some(RemoteSnapshot {
            sessions: vec![first.clone()],
            supported: true,
            error: None,
        });
        app.repos.select(Some(1));
        assert_eq!(app.zed_target().unwrap(), "ssh://dev-host/repo");
        assert!(screen(&mut app, 100, 30).contains("🤖"));
        assert_eq!(app.selected_agent().unwrap().id, first.id);
        let text = screen(&mut app, 100, 30);
        assert!(text.contains("Attach"));
        assert!(text.contains("· abcde · Unknown"));
        assert!(!text.contains(&String::from(first.id.clone())));
        assert!(!text.contains(" t  SSH"));
        app.key(key(KeyCode::Char('t')));
        assert!(app.ssh_command.is_none());
        app.key(key(KeyCode::Enter));
        let command = app.ssh_command.take().unwrap();
        assert_eq!(
            command.get_args().last().unwrap().to_str().unwrap(),
            format!(
                "cd '/repo' && exec wumpa agent-attach --port 7432 --session {}",
                String::from(first.id.clone())
            )
        );
        app.details = true;
        app.key(key(KeyCode::Char('t')));
        assert!(app.ssh_command.is_none());
        assert!(!screen(&mut app, 100, 30).contains("t SSH"));
        app.key(key(KeyCode::Enter));
        assert!(app.ssh_command.take().is_some());
        assert!(!app.details);
        let repositories = app.repositories.clone();
        complete_job(
            &mut app,
            1,
            Ok(Response {
                repositories: repositories.iter().map(|entry| entry.url.clone()).collect(),
                checkouts: repositories,
                sessions: Some(RemoteSnapshot {
                    sessions: vec![second, first.clone()],
                    supported: true,
                    error: None,
                }),
                ..Default::default()
            }),
        );
        assert_eq!(app.repos.selected(), Some(2));
        assert_eq!(app.selected_agent().unwrap().id, first.id);
        let text = screen(&mut app, 100, 30);
        assert!(text.contains("· abcde0 · Unknown"));
        assert!(text.contains("· abcde1 · Unknown"));
        assert!(!text.contains(&String::from(first.id.clone())));
        assert_eq!(app.zed_target().unwrap(), "ssh://dev-host/repo");
        app.sessions.as_mut().unwrap().error = Some("discovery timed out".into());
        assert!(screen(&mut app, 100, 30).contains("Agents unavailable"));
    }

    #[test]
    fn activity_refresh_updates_rows_promptly_without_disrupting_selection_or_details() {
        use crate::{
            agent_activity::Activity,
            session_runtime::{RemoteSnapshot, Summary},
            sessions::{SessionId, State},
        };
        use std::net::{Ipv4Addr, TcpListener};
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        app.config.servers[1].connection = Connection::Local {
            port: listener.local_addr().unwrap().port(),
        };
        app.pane = Pane::Repositories;
        app.repositories = vec![Repository {
            url: "ssh://host/app.git".into(),
            checkout_path: Some("/repo".into()),
        }];
        let agent = Summary {
            id: SessionId::try_from("a".repeat(32)).unwrap(),
            checkout: "/repo".into(),
            label: "original".into(),
            state: State::Running,
            activity: Activity::Working,
            pi_session_name: Some("First name".into()),
            pi_session_id: Some("conversation".into()),
        };
        app.sessions = Some(RemoteSnapshot {
            sessions: vec![agent.clone()],
            supported: true,
            error: None,
        });
        app.sessions_updates = true;
        app.sessions_received = Some(Instant::now());
        app.next_sessions_refresh = Instant::now();
        app.repos.select(Some(1));
        app.details = true;
        app.status = "Do not replace this message".into();
        let mut next = agent.clone();
        next.pi_session_name = Some("Renamed\u{1b}[2J\u{2028}".into());
        next.activity = Activity::WaitingForInput;
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            assert!(matches!(
                crate::protocol::read_message::<Request>(&mut stream).unwrap(),
                Request::SessionStatus { version: 1 }
            ));
            crate::protocol::write_message(
                &Response {
                    sessions_updates: true,
                    sessions: Some(RemoteSnapshot {
                        sessions: vec![next],
                        supported: true,
                        error: None,
                    }),
                    ..Default::default()
                },
                &mut stream,
            )
            .unwrap();
        });
        let started = Instant::now();
        while app.selected_agent().unwrap().activity != Activity::WaitingForInput {
            app.poll();
            assert!(started.elapsed() < Duration::from_secs(3));
            thread::sleep(Duration::from_millis(10));
        }
        server.join().unwrap();
        assert!(app.details);
        assert_eq!(app.selected_agent().unwrap().id, agent.id);
        assert_eq!(app.status, "Do not replace this message");
        app.details = false;
        let screen = screen(&mut app, 130, 30);
        assert!(screen.contains("Renamed\\u{1b}[2J\\u{2028}"));
        assert!(screen.contains("Waiting for input"));
        assert!(screen.contains("· aaaaa · Waiting for input"));
        assert!(!screen.contains(&"a".repeat(32)));
        app.update_sessions(RemoteSnapshot {
            supported: true,
            error: Some("temporary failure".into()),
            sessions: Vec::new(),
        });
        assert_eq!(app.selected_agent().unwrap().activity, Activity::Unknown);
        assert!(
            app.selected_agent()
                .unwrap()
                .display_name()
                .starts_with("Renamed")
        );
        app.sessions_received = Some(Instant::now() - Duration::from_secs(16));
        app.next_sessions_refresh = Instant::now() + Duration::from_secs(60);
        app.poll();
        assert_eq!(app.selected_agent().unwrap().activity, Activity::Unknown);
        app.sessions_updates = false;
        app.next_sessions_refresh = Instant::now();
        app.poll();
        assert!(app.sessions_job.is_none()); // No new requests to old servers.
    }

    #[test]
    fn zed_rejects_unselected_uncloned_and_local_repositories_without_launching() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        app.pane = Pane::Repositories;
        app.key(key(KeyCode::Char('z')));
        assert!(app.status.contains("Select a repository"));
        app.repos.select(Some(0));
        app.key(key(KeyCode::Char('z')));
        assert!(app.status.contains("not cloned"));
        app.repositories[0].checkout_path = Some(PathBuf::from("/projects/app"));
        app.key(key(KeyCode::Char('z')));
        assert!(app.status.contains("requires an SSH connection"));
        app.connected = None;
        app.key(key(KeyCode::Char('z')));
        assert!(app.status.contains("Connect to a server"));
        assert!(app.zed_launch.is_none());
    }

    #[test]
    fn workspace_hides_servers_and_switcher_preserves_context() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        app.pane = Pane::Repositories;
        for (width, height) in [(60, 20), (100, 30)] {
            let text = screen(&mut app, width, height);
            assert!(text.contains("Second"));
            assert!(text.contains("Repositories"));
            assert!(!text.contains("First"));
            assert!(!text.contains(" Servers "));
            assert!(text.contains("Switch server"));
        }
        app.key(key(KeyCode::Tab));
        assert!(app.pane == Pane::Repositories);
        app.key(key(KeyCode::Char('d')));
        assert!(app.remove_target.is_none());
        app.key(key(KeyCode::Char('s')));
        assert!(app.pane == Pane::Servers);
        assert_eq!(app.servers.selected(), Some(1));
        let text = screen(&mut app, 100, 30);
        assert!(text.contains("First"));
        assert!(!text.contains("Repositories"));
        app.key(key(KeyCode::Esc));
        assert!(app.pane == Pane::Repositories);
        assert_eq!(app.connected, Some(1));
        assert_eq!(app.repositories.len(), 1);
        assert!(app.job.is_none());
    }

    #[test]
    fn successful_connection_is_remembered_but_failure_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.json");
        let mut app = removal_app(path.clone());
        app.config.last_server = None;
        complete_job(&mut app, 1, Ok(Response::default()));
        assert_eq!(
            config::load::<ClientConfig>(&path)
                .unwrap()
                .last_server
                .as_deref(),
            Some("Second")
        );
        assert!(app.pane == Pane::Repositories);
        app.connected = None;
        app.workspace = Some(0);
        complete_job(&mut app, 0, Err("Connection refused".into()));
        assert_eq!(app.config.last_server.as_deref(), Some("Second"));
        let text = screen(&mut app, 60, 20);
        assert!(text.contains("Workspace unavailable"));
        assert!(text.contains("retry"));
        assert!(text.contains("Connection refused"));
        app.key(key(KeyCode::Char('s')));
        assert!(app.pane == Pane::Servers);
    }

    #[test]
    fn startup_reconnects_to_the_remembered_server() {
        use std::net::{Ipv4Addr, TcpListener};
        use std::time::Instant;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.json");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut app = removal_app(path.clone());
        app.config.servers[1].connection = Connection::Local { port };
        config::save(&path, &app.config).unwrap();
        let mut reopened = App::new(config::load(&path).unwrap(), path);
        reopened.resume();
        assert!(reopened.pane == Pane::Repositories);
        assert_eq!(reopened.workspace, Some(1));
        assert_eq!(reopened.job.as_ref().unwrap().target, 1);
        assert!(!screen(&mut reopened, 60, 20).contains("First"));
        // The listening socket makes the connection local and deterministic.
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        // Accepted sockets inherit nonblocking mode on some platforms.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert!(matches!(
            crate::protocol::read_message(&mut stream).unwrap(),
            Request::List
        ));
        crate::protocol::write_message(
            &Response {
                repositories: vec!["https://example.com/resumed.git".into()],
                ..Response::default()
            },
            &mut stream,
        )
        .unwrap();
        while reopened.job.is_some() {
            assert!(Instant::now() < deadline);
            reopened.poll();
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(reopened.connected, Some(1));
        assert!(screen(&mut reopened, 100, 30).contains("resumed"));
    }

    #[test]
    fn missing_preference_opens_picker_and_save_failure_keeps_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().to_path_buf());
        app.config.last_server = Some("Deleted".into());
        app.resume();
        assert!(app.pane == Pane::Servers);
        assert!(app.job.is_none());
        complete_job(&mut app, 0, Ok(Response::default()));
        assert_eq!(app.connected, Some(0));
        assert!(app.pane == Pane::Repositories);
        assert!(app.error);
        assert!(app.status.contains("could not remember"));
        assert_eq!(app.config.last_server.as_deref(), Some("Deleted"));
    }

    #[test]
    fn server_form_is_flat_and_does_not_draw_outside_its_panel() {
        for (width, height) in [(60, 20), (100, 30)] {
            let mut app = App::new(ClientConfig::default(), PathBuf::from("unused"));
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let background = terminal.backend().buffer().clone();
            app.form = Some(Form::new(FormKind::Server));
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let rendered = terminal.backend().buffer();
            let panel_width = 68.min(width - 4);
            let left = (width - panel_width) / 2;
            let top = (height - 18) / 2;
            for y in 0..height {
                for x in 0..width {
                    if x < left || x >= left + panel_width || y < top || y >= top + 18 {
                        assert_eq!(rendered[(x, y)], background[(x, y)]);
                    }
                }
            }
            assert_eq!(rendered[(left, top)].fg, ACCENT);
            assert_eq!(rendered[(left, top + 1)].symbol(), " ");
            assert_eq!(rendered[(left, top + 1)].bg, PANEL);
        }
    }

    #[test]
    fn picker_heading_is_only_wumpa_and_server_form_has_clear_actions() {
        let mut app = App::new(ClientConfig::default(), PathBuf::from("unused"));
        let text = screen(&mut app, 60, 20);
        assert!(text.contains("WUMPA"));
        assert!(!text.contains("Your workspaces"));
        assert!(!text.contains(" / "));
        app.key(key(KeyCode::Char('n')));
        for (width, height) in [(60, 20), (100, 30)] {
            let text = screen(&mut app, width, height);
            for label in [
                "New server",
                "Connection name",
                "e.g. Development",
                "SSH host",
                "Local connection",
                "Continue",
                "Esc Cancel",
            ] {
                assert!(text.contains(label), "missing {label} at {width}x{height}");
            }
        }
        let form = app.form.as_mut().unwrap();
        form.values[0] = "開発".repeat(100);
        form.values[1] = "user@host".into();
        form.field = 2;
        form.error = "Port must be a number from 1 to 65535.".into();
        let text = screen(&mut app, 60, 20);
        assert!(text.contains("SSH connection"));
        assert!(text.contains("Save server"));
        assert!(text.contains("Port must be a number"));
        assert!(text.contains("3 / 3"));
    }

    #[test]
    fn form_saves_connection_and_validates_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(ClientConfig::default(), dir.path().join("client.json"));
        app.key(key(KeyCode::Char('n')));
        let form = app.form.as_mut().unwrap();
        form.values = vec!["Development".into(), "".into(), "7432".into()];
        form.field = 2;
        app.key(key(KeyCode::Enter));
        assert!(app.form.is_none());
        assert_eq!(
            config::load::<ClientConfig>(&app.path).unwrap().servers[0].name,
            "Development"
        );
        let mut duplicate = Form::new(FormKind::Server);
        duplicate.values[0] = "Development".into();
        assert!(app.submit(&duplicate).is_err());
    }

    #[test]
    fn editing_and_cancel_do_not_save() {
        let mut app = App::new(ClientConfig::default(), PathBuf::from("unused"));
        app.key(key(KeyCode::Char('n')));
        app.key(key(KeyCode::Char('é')));
        app.key(key(KeyCode::Backspace));
        assert!(app.form.as_ref().unwrap().values[0].is_empty());
        app.key(key(KeyCode::Esc));
        assert!(app.form.is_none());
        assert!(app.config.servers.is_empty());
        assert!(app.key(key(KeyCode::Char('q'))));
    }

    #[test]
    fn renders_empty_dashboard_forms_and_small_terminal() {
        let mut app = App::new(ClientConfig::default(), PathBuf::from("client.json"));
        for (width, height) in [(100, 30), (60, 20), (20, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            if width == 100 {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(text.contains("WUMPA"));
                assert!(!text.contains("(w)"));
                assert!(text.contains("No servers yet"));
            }
            app.form = Some(Form::new(FormKind::Server));
            terminal.draw(|frame| app.draw(frame)).unwrap();
            app.form = None;
        }
    }

    #[test]
    fn renders_connected_workspace_and_keeps_paste_inside_form() {
        let config = ClientConfig {
            servers: vec![Server {
                name: "Development".into(),
                connection: Connection::Local { port: 7432 },
            }],
            ..ClientConfig::default()
        };
        let mut app = App::new(config, PathBuf::from("client.json"));
        app.connected = Some(0);
        app.workspace = Some(0);
        app.repositories = vec![Repository {
            url: "https://example.com/project.git".into(),
            checkout_path: None,
        }];
        app.repos.select(Some(0));
        app.pane = Pane::Repositories;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Development"));
        assert!(text.contains("project"));
        assert!(text.contains("saved"));
        app.key(key(KeyCode::Char('a')));
        app.paste("https://example.com/new.git\n\r\u{1b}");
        assert_eq!(
            app.form.as_ref().unwrap().values[0],
            "https://example.com/new.git"
        );
        assert!(app.job.is_none(), "pasting must not submit the form");
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.key(key(KeyCode::Esc));
        app.key(key(KeyCode::Esc));
        assert!(app.pane == Pane::Servers);
        assert_eq!(app.connected, Some(0));
        assert_eq!(app.repositories.len(), 1);
        app.key(key(KeyCode::Esc));
        assert!(app.pane == Pane::Repositories);
    }

    fn removal_app(path: PathBuf) -> App {
        let config = ClientConfig {
            servers: ["First", "Second"]
                .into_iter()
                .map(|name| Server {
                    name: name.into(),
                    connection: Connection::Local { port: 7432 },
                })
                .collect(),
            last_server: Some("Second".into()),
        };
        let mut app = App::new(config, path);
        app.servers.select(Some(0));
        app.workspace = Some(1);
        app.connected = Some(1);
        app.repositories = vec![Repository {
            url: "https://example.com/app.git".into(),
            checkout_path: None,
        }];
        app
    }

    #[test]
    fn removal_confirms_persists_and_preserves_other_connection() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().join("client.json"));
        app.key(key(KeyCode::Char('d')));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.key(key(KeyCode::Enter)); // Enter is deliberately not confirmation.
        assert_eq!(app.config.servers.len(), 2);
        app.key(key(KeyCode::Esc));
        assert!(app.remove_target.is_none());
        assert!(!app.path.exists());
        app.key(key(KeyCode::Delete));
        app.key(key(KeyCode::Char('y')));
        assert_eq!(app.connected, Some(0));
        assert_eq!(app.repositories.len(), 1);
        assert_eq!(
            config::load::<ClientConfig>(&app.path).unwrap().servers[0].name,
            "Second"
        );
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        assert!(app.connected.is_none());
        assert!(app.repositories.is_empty());
        assert!(app.servers.selected().is_none());
        assert!(
            config::load::<ClientConfig>(&app.path)
                .unwrap()
                .servers
                .is_empty()
        );
    }

    #[test]
    fn failed_removal_preserves_config_selection_and_connection() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = removal_app(dir.path().to_path_buf()); // Cannot replace a directory.
        app.key(key(KeyCode::Char('d')));
        app.key(key(KeyCode::Char('y')));
        assert_eq!(app.config.servers.len(), 2);
        assert_eq!(app.config.servers[0].name, "First");
        assert_eq!(app.connected, Some(1));
        assert_eq!(app.servers.selected(), Some(0));
        assert_eq!(app.repositories.len(), 1);
        assert!(app.error);
    }

    #[test]
    fn form_rejects_bad_host_and_port() {
        let mut form = Form::new(FormKind::Server);
        assert!(form.server().is_err());
        form.values[0] = "Dev".into();
        form.values[1] = "-bad".into();
        assert!(form.server().is_err());
        form.values[1] = "dev".into();
        form.values[2] = "0".into();
        assert!(form.server().is_err());
        form.values[2] = "7432".into();
        assert!(matches!(
            form.server().unwrap().connection,
            Connection::Ssh { .. }
        ));
    }
}
