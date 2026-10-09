use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    env,
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use crate::Result;

/// Saved client connection profiles; concurrent writers are unsupported.
#[derive(Default, Serialize, Deserialize)]
pub struct ClientConfig {
    pub servers: Vec<Server>,
    /// Name of the last successfully opened workspace; absent in older configs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_server: Option<String>,
}

impl ClientConfig {
    /// Resolve the remembered profile by name, tolerating deleted/stale entries.
    pub fn last_server_index(&self) -> Option<usize> {
        let name = self.last_server.as_ref()?;
        self.servers.iter().position(|server| &server.name == name)
    }

    /// Remember a successful connection, rolling back if persistence fails.
    pub fn remember_server(&mut self, index: usize, path: &Path) -> Result<()> {
        let name = self
            .servers
            .get(index)
            .ok_or("connection no longer exists")?
            .name
            .clone();
        if self.last_server.as_ref() == Some(&name) {
            return Ok(());
        }
        let previous = self.last_server.replace(name);
        if let Err(error) = save(path, self) {
            self.last_server = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Forget a local connection profile; never contact or modify its server.
    pub fn remove_server(&mut self, index: usize, path: &Path) -> Result<()> {
        if index >= self.servers.len() {
            return Err("connection no longer exists".into());
        }
        let removed = self.servers.remove(index);
        let previous = self.last_server.clone();
        if self.last_server.as_ref() == Some(&removed.name) {
            self.last_server = None;
        }
        if let Err(error) = save(path, self) {
            self.last_server = previous;
            self.servers.insert(index, removed);
            return Err(error);
        }
        Ok(())
    }
}

/// A named connection profile stored in the client configuration.
#[derive(Clone, Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    #[serde(flatten)]
    pub connection: Connection,
}

/// A loopback endpoint, reached directly or through SSH forwarding.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Connection {
    Local { port: u16 },
    Ssh { host: String, port: u16 },
}

/// Repository metadata; an absent checkout marks a saved-but-uncloned URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_path: Option<PathBuf>,
}

/// Server-selected executable and literal arguments, never a shell command string.
/// Empty arguments are valid; empty executables and NUL bytes are not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct AgentCommand(Vec<String>);

impl AgentCommand {
    /// Validated executable followed by literal arguments; never a shell string.
    pub fn arguments(&self) -> &[String] {
        &self.0
    }
}

impl Default for AgentCommand {
    fn default() -> Self {
        Self(vec!["pi".into()])
    }
}

impl TryFrom<Vec<String>> for AgentCommand {
    type Error = &'static str;

    fn try_from(arguments: Vec<String>) -> std::result::Result<Self, Self::Error> {
        if arguments
            .first()
            .is_none_or(|executable| executable.is_empty())
        {
            return Err("agent_command must contain a nonempty executable");
        }
        if arguments.iter().any(|argument| argument.contains('\0')) {
            return Err("agent_command arguments must not contain NUL bytes");
        }
        Ok(Self(arguments))
    }
}

impl From<AgentCommand> for Vec<String> {
    fn from(command: AgentCommand) -> Self {
        command.0
    }
}

/// Server-owned storage settings, agent launch settings, and repository metadata.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Applied only to new agents; older configurations default to `["pi"]`.
    #[serde(default)]
    pub agent_command: AgentCommand,
    /// Resolved and persisted at startup; absent in legacy configurations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_dir: Option<PathBuf>,
    #[serde(deserialize_with = "deserialize_repositories")]
    pub repositories: Vec<Repository>,
}

fn deserialize_repositories<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<Repository>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Legacy(String),
        Metadata(Repository),
    }

    Ok(Vec::<Entry>::deserialize(deserializer)?
        .into_iter()
        .map(|entry| match entry {
            Entry::Legacy(url) => Repository {
                url,
                checkout_path: None,
            },
            Entry::Metadata(repository) => repository,
        })
        .collect())
}

impl ServerConfig {
    /// Validate metadata and prepare the root without cloning or moving checkouts.
    /// The caller supplies the server process's HOME and persists on success.
    pub fn initialize(&mut self, home: Option<&OsStr>) -> Result<()> {
        if self.repositories.len() > 100 {
            return Err("this prototype supports at most 100 repositories".into());
        }
        for repository in &self.repositories {
            crate::protocol::validate_url(&repository.url)?;
            if let Some(path) = &repository.checkout_path {
                if !path.is_absolute() {
                    return Err(format!(
                        "checkout_path must be absolute: {}; literal ~ and environment variables are not expanded",
                        path.display()
                    )
                    .into());
                }
            }
        }
        let root = self
            .repository_dir
            .clone()
            .or_else(|| home.filter(|value| !value.is_empty()).map(PathBuf::from))
            .ok_or(
                "set repository_dir in server.json to an absolute directory: server HOME is unavailable",
            )?;
        if !root.is_absolute() {
            return Err("repository_dir must be an absolute directory; literal ~ and environment variables are not expanded".into());
        }
        let prepare = || -> Result<PathBuf> {
            fs::create_dir_all(&root)?;
            let canonical = fs::canonicalize(&root)?;
            // Check listing and creation access, not just permission bits.
            fs::read_dir(&canonical)?;
            let probe = tempfile::Builder::new()
                .prefix(".wumpa-access-")
                .tempdir_in(&canonical)?;
            let file = tempfile::NamedTempFile::new_in(probe.path())?;
            file.close()?;
            probe.close()?;
            Ok(canonical)
        };
        self.repository_dir = Some(prepare().map_err(|error| {
            format!("cannot prepare repository_dir {}: {error}", root.display())
        })?);
        Ok(())
    }
}

/// Resolve a client/server override, then XDG_CONFIG_HOME, then HOME/.config.
/// Overrides are complete paths; an explicitly empty override is an error.
pub fn path(kind: &str) -> Result<PathBuf> {
    let variable = format!("WUMPA_{}_CONFIG", kind.to_uppercase());
    if let Some(value) = env::var_os(&variable) {
        if value.is_empty() {
            return Err(format!("{variable} must not be empty").into());
        }
        return Ok(value.into());
    }
    let root = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or("set HOME, XDG_CONFIG_HOME, or a WUMPA config override")?;
    Ok(root.join("wumpa").join(format!("{kind}.json")))
}

/// Load JSON, defaulting only when the file is missing, not when it is invalid.
pub fn load<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid configuration {}: {error}", path.display()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(format!("cannot read {}: {error}", path.display()).into()),
    }
}

/// Atomically replace JSON using a synced temporary file in the same directory.
/// Parent directories are created as needed; concurrent writers are unsupported.
pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_config() -> ClientConfig {
        ClientConfig {
            servers: ["First", "Second"]
                .into_iter()
                .map(|name| Server {
                    name: name.into(),
                    connection: Connection::Local { port: 7432 },
                })
                .collect(),
            ..ClientConfig::default()
        }
    }

    #[test]
    fn old_configs_and_stale_preferences_are_supported() {
        let old: ClientConfig = serde_json::from_str(r#"{"servers":[]}"#).unwrap();
        assert!(old.last_server_index().is_none());
        let mut config = client_config();
        config.last_server = Some("Deleted".into());
        assert!(config.last_server_index().is_none());
        config.last_server = Some("Second".into());
        config.servers.swap(0, 1);
        assert_eq!(config.last_server_index(), Some(0));
    }

    #[test]
    fn remembers_success_and_clears_only_the_removed_preference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.json");
        let mut config = client_config();
        config.remember_server(1, &path).unwrap();
        assert_eq!(
            load::<ClientConfig>(&path).unwrap().last_server_index(),
            Some(1)
        );
        config.remove_server(0, &path).unwrap();
        assert_eq!(config.last_server_index(), Some(0));
        config.remove_server(0, &path).unwrap();
        assert!(load::<ClientConfig>(&path).unwrap().last_server.is_none());
    }

    #[test]
    fn preference_changes_roll_back_on_save_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = client_config();
        config.last_server = Some("First".into());
        assert!(config.remember_server(1, dir.path()).is_err());
        assert_eq!(config.last_server.as_deref(), Some("First"));
        assert!(config.remove_server(0, dir.path()).is_err());
        assert_eq!(config.last_server_index(), Some(0));
        assert_eq!(config.servers.len(), 2);
    }

    #[test]
    fn agent_command_defaults_and_round_trips_literal_arguments() {
        assert_eq!(
            ServerConfig::default().agent_command,
            AgentCommand::default()
        );
        for json in [r#"{"repositories":[]}"#, r#"{"repositories":["legacy"]}"#] {
            let config: ServerConfig = serde_json::from_str(json).unwrap();
            assert_eq!(config.agent_command, AgentCommand::default());
        }
        let arguments = vec![
            "/path with spaces/pi".to_owned(),
            "".to_owned(),
            "literal '$HOME'; $(not-a-command)\n".to_owned(),
        ];
        let config: ServerConfig = serde_json::from_value(serde_json::json!({
            "repositories": [],
            "agent_command": arguments,
        }))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        save(&path, &config).unwrap();
        let loaded: ServerConfig = load(&path).unwrap();
        assert_eq!(loaded.agent_command, config.agent_command);
        let saved = serde_json::to_value(&loaded).unwrap();
        assert_eq!(saved["agent_command"], serde_json::json!(arguments));
    }

    #[test]
    fn agent_command_rejects_invalid_settings_without_echoing_arguments() {
        for command in [
            serde_json::json!([]),
            serde_json::json!([""]),
            serde_json::json!(["pi\0secret-value"]),
            serde_json::json!(["pi", "secret-value\0"]),
            serde_json::json!("pi --flag"),
            serde_json::json!(null),
            serde_json::json!(["pi", 42]),
        ] {
            let result = serde_json::from_value::<ServerConfig>(serde_json::json!({
                "repositories": [],
                "agent_command": command,
            }));
            let error = result.err().expect("invalid command accepted").to_string();
            assert!(!error.contains("secret-value"));
        }
    }

    #[test]
    fn server_defaults_to_home_and_migrates_legacy_records() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let path = dir.path().join("server.json");
        fs::write(&path, r#"{"repositories":["https://example.com/app.git"]}"#).unwrap();
        let mut config: ServerConfig = load(&path).unwrap();
        config.initialize(Some(home.as_os_str())).unwrap();
        assert_eq!(
            config.repository_dir,
            Some(fs::canonicalize(&home).unwrap())
        );
        assert_eq!(config.repositories[0].url, "https://example.com/app.git");
        assert!(config.repositories[0].checkout_path.is_none());
        assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
        save(&path, &config).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved["repository_dir"].is_string());
        assert!(saved["repositories"][0].is_object());
        let mut restarted: ServerConfig = load(&path).unwrap();
        restarted.initialize(None).unwrap();
        assert_eq!(restarted.repository_dir, config.repository_dir);
        assert_eq!(restarted.repositories, config.repositories);
    }

    #[test]
    fn explicit_root_does_not_relocate_existing_checkouts() {
        let dir = tempfile::tempdir().unwrap();
        let old_checkout = dir.path().join("old/app");
        fs::create_dir_all(&old_checkout).unwrap();
        fs::write(old_checkout.join("keep"), "contents").unwrap();
        let mut config: ServerConfig = serde_json::from_value(serde_json::json!({
            "repository_dir": dir.path().join("new"),
            "repositories": [
                {"url": "https://example.com/app.git", "checkout_path": old_checkout},
                "https://example.com/legacy.git"
            ]
        }))
        .unwrap();
        config.initialize(None).unwrap();
        assert_eq!(
            config.repositories[0].checkout_path,
            Some(old_checkout.clone())
        );
        assert!(config.repositories[1].checkout_path.is_none());
        assert_eq!(
            fs::read_to_string(old_checkout.join("keep")).unwrap(),
            "contents"
        );
        assert_eq!(
            fs::read_dir(config.repository_dir.as_ref().unwrap())
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn server_requires_a_valid_home_or_explicit_absolute_root() {
        let mut config = ServerConfig::default();
        for home in [None, Some(OsStr::new(""))] {
            let error = config.initialize(home).unwrap_err().to_string();
            assert!(error.contains("HOME is unavailable"));
        }
        assert!(
            config
                .initialize(Some(OsStr::new("relative-home")))
                .is_err()
        );
        for root in ["", "relative", "~/projects", "$HOME/projects"] {
            config.repository_dir = Some(root.into());
            let error = config.initialize(None).unwrap_err().to_string();
            assert!(error.contains("absolute directory"));
            assert!(error.contains("not expanded"));
        }
    }

    #[test]
    fn server_rejects_files_as_roots_and_invalid_checkout_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "keep").unwrap();
        let mut config = ServerConfig {
            repository_dir: Some(file.clone()),
            ..ServerConfig::default()
        };
        assert!(
            config
                .initialize(None)
                .unwrap_err()
                .to_string()
                .contains("cannot prepare repository_dir")
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
        config.repository_dir = Some(dir.path().join("unused"));
        config.repositories.push(Repository {
            url: "https://example.com/app.git".into(),
            checkout_path: Some("relative".into()),
        });
        assert!(
            config
                .initialize(None)
                .unwrap_err()
                .to_string()
                .contains("checkout_path must be absolute")
        );
        assert!(!config.repository_dir.as_ref().unwrap().exists());
        config.repositories[0].checkout_path = None;
        config.repositories = vec![config.repositories[0].clone(); 101];
        assert!(
            config
                .initialize(None)
                .unwrap_err()
                .to_string()
                .contains("at most 100")
        );
        config.repositories.truncate(100);
        config.initialize(None).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn server_canonicalizes_symlinked_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let link = dir.path().join("link");
        fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&root, &link).unwrap();
        let mut config = ServerConfig {
            repository_dir: Some(link),
            ..ServerConfig::default()
        };
        config.initialize(None).unwrap();
        assert_eq!(config.repository_dir, Some(fs::canonicalize(root).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn server_rejects_an_inaccessible_root() {
        use std::os::unix::fs::PermissionsExt;

        // Root bypasses permission checks; test this as an ordinary user.
        // SAFETY: geteuid only reads the current process's effective user ID.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();
        let mut config = ServerConfig {
            repository_dir: Some(root.clone()),
            ..ServerConfig::default()
        };
        let result = config.initialize(None);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn config_round_trip_and_invalid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/server.json");
        assert!(load::<ServerConfig>(&path).unwrap().repositories.is_empty());
        let config = ServerConfig {
            repositories: vec![Repository {
                url: "https://example.com/app.git".into(),
                checkout_path: None,
            }],
            ..ServerConfig::default()
        };
        save(&path, &config).unwrap();
        assert_eq!(
            load::<ServerConfig>(&path).unwrap().repositories,
            config.repositories
        );
        fs::write(&path, "not json").unwrap();
        assert!(load::<ServerConfig>(&path).is_err());
    }
}
