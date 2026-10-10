//! Shared newline-delimited JSON messages and repository metadata validation.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::Result;

/// Maximum encoded message size in bytes, including the terminating newline.
pub const MAX_MESSAGE: u64 = 1024 * 1024;

/// A single daemon operation, encoded as newline-delimited JSON.
#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    List,
    /// Inspect or execute a server-managed deletion.
    Delete {
        target: crate::deletion::Target,
        confirmation: Option<String>,
    },
    /// Read the activity cache only; no Git discovery or agent control.
    SessionStatus {
        version: u32,
    },
    Add {
        url: String,
    },
    Clone {
        version: u32,
        url: String,
        folder_name: Option<String>,
        agent_socket: Option<PathBuf>,
    },
    PrepareClone {
        version: u32,
        url: String,
        folder_name: Option<String>,
        agent_socket: Option<PathBuf>,
    },
}

/// Version of the optional read-only activity refresh contract.
pub const SESSION_STATUS_VERSION: u32 = 1;

fn is_false(value: &bool) -> bool {
    !value
}

/// Version of the session-aware helper/daemon contract, separate from metadata.
pub const HELPER_VERSION: u32 = 2;

/// Maximum Git runtime; transport allows additional time for cleanup.
pub const CLONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Client input for the remote helper. Agent paths come from its SSH session only.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperRequest {
    pub version: u32,
    pub port: u16,
    #[serde(default)]
    pub clone: bool,
    pub url: String,
    pub folder_name: Option<String>,
}

/// Versioned clone preflight result; never includes the operation's agent socket.
#[derive(Serialize, Deserialize)]
pub struct Preflight {
    pub version: u32,
    pub destination: Option<PathBuf>,
}

/// Current repository metadata and an optional operation failure.
#[derive(Default, Serialize, Deserialize)]
pub struct Response {
    pub repositories: Vec<String>,
    /// Discovery hint for the server-local attachment helper, not a tmux target.
    /// The helper must authenticate this endpoint through the local handshake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_socket: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_dir: Option<PathBuf>,
    /// Server home used only for display; never inferred from the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_dir: Option<PathBuf>,
    #[serde(default)]
    pub checkouts: Vec<crate::config::Repository>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktrees: Vec<crate::worktrees::RepositoryWorktrees>,
    pub error: Option<String>,
    /// Remote-safe agent metadata; absent means an older daemon lacks discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<crate::session_runtime::RemoteSnapshot>,
    /// Optional capability: old servers are never sent new refresh requests.
    #[serde(default, skip_serializing_if = "is_false")]
    pub sessions_updates: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight: Option<Preflight>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion: Option<crate::deletion::Prompt>,
}

impl Response {
    /// Merge checkout metadata with legacy URL-only responses without claiming
    /// that a missing checkout record represents a completed clone.
    pub fn repository_entries(&self) -> Vec<crate::config::Repository> {
        self.repositories
            .iter()
            .map(|url| {
                self.checkouts
                    .iter()
                    .find(|entry| entry.url == *url)
                    .cloned()
                    .unwrap_or_else(|| crate::config::Repository {
                        url: url.clone(),
                        checkout_path: None,
                    })
            })
            .collect()
    }
}

impl Request {
    /// Build a validated clone request. Agent selection belongs to the transport
    /// and remote session, never to a client-side socket path.
    pub fn clone_repository(url: &str, folder: &str) -> Result<Self> {
        let folder_name = (!folder.trim().is_empty()).then(|| folder.trim().to_owned());
        crate::repository::folder_name(url, folder_name.as_deref())?;
        Ok(Self::Clone {
            version: HELPER_VERSION,
            url: url.trim().into(),
            folder_name,
            agent_socket: None,
        })
    }
}

/// Read one newline-terminated JSON message of at most one MiB.
/// The caller must provide any required I/O deadline.
pub fn read_message<T: serde::de::DeserializeOwned>(reader: impl Read) -> Result<T> {
    let mut bytes = Vec::new();
    BufReader::new(reader.take(MAX_MESSAGE + 1)).read_until(b'\n', &mut bytes)?;
    if bytes.len() as u64 > MAX_MESSAGE || bytes.last() != Some(&b'\n') {
        return Err("invalid or oversized server message".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// Write and flush one JSON message, including its newline, within one MiB.
pub fn write_message(value: &impl Serialize, mut writer: impl Write) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() as u64 >= MAX_MESSAGE {
        return Err("server message is too large".into());
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// Apply basic metadata limits, including to legacy saved URLs.
/// New requests additionally require the SSH-only syntax in `repository`.
pub fn validate_url(url: &str) -> Result<()> {
    let url = url.trim();
    if url.is_empty() || url.len() > 4096 || url.chars().any(char::is_control) {
        return Err("repository URL must be 1–4096 bytes without control characters".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_requires_newline_and_valid_json() {
        assert!(read_message::<Request>(&b"{\"action\":\"list\"}\n"[..]).is_ok());
        assert!(read_message::<Request>(&b"{\"action\":\"list\"}"[..]).is_err());
        assert!(read_message::<Request>(&b"oops\n"[..]).is_err());
    }

    #[test]
    fn request_round_trip() {
        let mut bytes = Vec::new();
        write_message(
            &Request::Add {
                url: "example".into(),
            },
            &mut bytes,
        )
        .unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let Request::Add { url } = read_message(bytes.as_slice()).unwrap() else {
            panic!("expected add request");
        };
        assert_eq!(url, "example");
    }

    #[test]
    fn message_size_limit_includes_newline() {
        // JSON string encoding adds two quote bytes and framing adds a newline.
        let value = "x".repeat(MAX_MESSAGE as usize - 3);
        let mut bytes = Vec::new();
        write_message(&value, &mut bytes).unwrap();
        assert_eq!(bytes.len() as u64, MAX_MESSAGE);
        assert_eq!(read_message::<String>(bytes.as_slice()).unwrap(), value);

        let oversized = format!("{value}x");
        let mut output = Vec::new();
        assert!(write_message(&oversized, &mut output).is_err());
        assert!(output.is_empty());
        let mut bytes = serde_json::to_vec(&oversized).unwrap();
        bytes.push(b'\n');
        assert!(read_message::<String>(bytes.as_slice()).is_err());
    }

    #[test]
    fn clone_request_validates_folders_and_never_includes_a_client_agent() {
        let request = Request::clone_repository(" ssh://host/app.git ", " review ").unwrap();
        let Request::Clone {
            version,
            url,
            folder_name,
            agent_socket,
        } = request
        else {
            panic!("expected clone");
        };
        assert_eq!(version, HELPER_VERSION);
        assert_eq!(url, "ssh://host/app.git");
        assert_eq!(folder_name.as_deref(), Some("review"));
        assert!(agent_socket.is_none());
        assert!(Request::clone_repository("ssh://host/app.git", "../escape").is_err());
        assert!(Request::clone_repository("https://host/app.git", "").is_err());
    }

    #[test]
    fn repository_entries_preserve_legacy_records_without_inventing_checkouts() {
        let response: Response = serde_json::from_str(r#"{"repositories":["ssh://host/old.git","ssh://host/new.git"],"error":null,"checkouts":[{"url":"ssh://host/new.git","checkout_path":"/projects/new"}]}"#).unwrap();
        let entries = response.repository_entries();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].checkout_path.is_none());
        assert_eq!(
            entries[1].checkout_path.as_deref(),
            Some(std::path::Path::new("/projects/new"))
        );
        let legacy: Response =
            serde_json::from_str(r#"{"repositories":["legacy"],"error":null}"#).unwrap();
        assert!(legacy.repository_entries()[0].checkout_path.is_none());
    }

    #[test]
    fn validates_repository_metadata() {
        for url in ["", " ", "a\u{1b}b"] {
            assert!(validate_url(url).is_err());
        }
        assert!(validate_url(&"x".repeat(4097)).is_err());
        assert!(validate_url(&"x".repeat(4096)).is_ok());
        assert!(validate_url(" https://example.com/app.git ").is_ok());
    }
}
