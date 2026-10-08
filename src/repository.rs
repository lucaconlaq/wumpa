//! Strict SSH repository URLs and non-mutating clone destination preflight.

use std::{fs, io::ErrorKind, net::Ipv6Addr, path::PathBuf};

use crate::{Result, config::ServerConfig, protocol};

/// Validate the supported Git URL subset and derive or validate a checkout name.
/// Leading/trailing spaces are ignored. No decoding or URL rewriting is performed.
/// Restrict syntax deliberately so Git and SSH cannot reinterpret URL components.
pub fn folder_name(url: &str, override_name: Option<&str>) -> Result<String> {
    protocol::validate_url(url)?;
    if url.chars().any(char::is_control) {
        return Err("repository URL must not contain control characters".into());
    }
    let url = url.trim();
    let remainder = url
        .strip_prefix("ssh://")
        .ok_or("use an ssh:// repository URL, such as ssh://git@github.com/owner/repo.git")?;
    let (authority, path) = remainder
        .split_once('/')
        .ok_or("SSH repository URL must include a host and repository path")?;
    let host = if let Some((user, host)) = authority.split_once('@') {
        if !identifier(user) {
            return Err(
                "invalid SSH username; passwords and tokens in URLs are not supported".into(),
            );
        }
        host
    } else {
        authority
    };
    validate_host(host)?;
    // Reject percent encoding rather than disagreeing with Git about decoding.
    // Empty/dot components, shell syntax, queries and fragments are unsupported.
    if path.split('/').any(|part| {
        part.is_empty()
            || matches!(part, "." | "..")
            || part.starts_with('-')
            || !part.chars().all(path_character)
    }) {
        return Err("invalid repository path: use nonempty components containing letters, numbers, '.', '_' or '-'; no traversal, encoding, queries or fragments".into());
    }
    let last = path.rsplit('/').next().ok_or("missing repository name")?;
    let derived = last.strip_suffix(".git").unwrap_or(last);
    let name = override_name.unwrap_or(derived);
    validate_folder_name(name)?;
    Ok(name.into())
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(['-', '.'])
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn path_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '.' | '_' | '-')
}

fn validate_host(value: &str) -> Result<()> {
    let port = if let Some(bracketed) = value.strip_prefix('[') {
        let (address, suffix) = bracketed
            .split_once(']')
            .ok_or("IPv6 SSH hosts must use brackets, such as [::1]")?;
        address
            .parse::<Ipv6Addr>()
            .map_err(|_| "invalid IPv6 SSH host")?;
        if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':').ok_or("invalid SSH host suffix")?)
        }
    } else {
        let (host, port) = match value.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (value, None),
        };
        // A single trailing dot denotes an absolute DNS name, not an empty label.
        let labels = host.strip_suffix('.').unwrap_or(host);
        if !identifier(labels) || labels.split('.').any(str::is_empty) {
            return Err(
                "invalid SSH host; use a hostname, SSH alias, or bracketed IPv6 address".into(),
            );
        }
        port
    };
    if let Some(port) = port {
        if port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || !matches!(port.parse::<u16>(), Ok(1..=u16::MAX))
        {
            return Err("SSH port must be between 1 and 65535".into());
        }
    }
    Ok(())
}

fn validate_folder_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 255
        || name.starts_with(['.', '-'])
        || !name.chars().all(path_character)
    {
        return Err("folder name must be a single non-hidden component of 1–255 bytes using letters, numbers, '.', '_' or '-'; it must not start with '-'".into());
    }
    Ok(())
}

/// Check a prospective checkout without creating, reserving, or deleting paths.
/// A saved-but-uncloned URL reuses its record, even at the repository limit.
/// The clone executor must reserve the URL/path and publish without clobbering:
/// this check alone cannot protect against a destination appearing later.
pub fn destination(
    config: &ServerConfig,
    url: &str,
    override_name: Option<&str>,
) -> Result<PathBuf> {
    let name = folder_name(url, override_name)?;
    let url = url.trim();
    let existing = config
        .repositories
        .iter()
        .filter(|entry| entry.url.trim() == url);
    let mut saved = false;
    for entry in existing {
        if entry.checkout_path.is_some() {
            return Err("repository is already cloned".into());
        }
        saved = true;
    }
    if !saved && config.repositories.len() >= 100 {
        return Err("this prototype supports at most 100 repositories".into());
    }
    let root = config
        .repository_dir
        .as_ref()
        .ok_or("repository_dir is not initialized")?;
    if !root.is_absolute() || !root.is_dir() {
        return Err("repository_dir must be an existing absolute directory".into());
    }
    let destination = root.join(name);
    if config
        .repositories
        .iter()
        .any(|entry| entry.checkout_path.as_ref() == Some(&destination))
    {
        return Err(format!(
            "destination is already registered: {}",
            destination.display()
        )
        .into());
    }
    match fs::symlink_metadata(&destination) {
        Ok(_) => Err(format!("destination already exists: {}", destination.display()).into()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(destination),
        Err(error) => Err(format!(
            "cannot inspect destination {}: {error}",
            destination.display()
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Repository;

    use super::*;

    #[test]
    fn accepts_explicit_ssh_urls_and_derives_names() {
        for (url, name) in [
            ("ssh://git@github.com/owner/repo.git", "repo"),
            (" ssh://my-alias/projects/app ", "app"),
            ("ssh://git@host:2222/team/app.git", "app"),
            ("ssh://git@[2001:db8::1]:22/app.git", "app"),
            ("ssh://[::1]/app.git", "app"),
            ("ssh://host/café.git", "café"),
            ("ssh://host/app.git.git", "app.git"),
        ] {
            assert_eq!(folder_name(url, None).unwrap(), name, "{url}");
        }
    }

    #[test]
    fn accepts_a_single_dns_root_dot_but_not_empty_host_labels() {
        for host in ["example.com.", "example.com.:2222", "git@example.com."] {
            assert_eq!(
                folder_name(&format!("ssh://{host}/repo.git"), None).unwrap(),
                "repo"
            );
        }
        for host in [
            ".",
            "..",
            ".example.com",
            "example..com",
            "example.com..",
            "example.com..:22",
        ] {
            assert!(
                folder_name(&format!("ssh://{host}/repo.git"), None).is_err(),
                "{host}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_schemes_credentials_and_ambiguous_syntax() {
        for url in [
            "",
            "https://github.com/owner/repo.git",
            "git@github.com:owner/repo.git",
            "/tmp/repo",
            "file:///repo",
            "ext::command",
            "--upload-pack=command",
            "ssh://",
            "ssh:///app",
            "ssh://host",
            "ssh://host/",
            "ssh://@host/app",
            "ssh://user:password@host/app",
            "ssh://user%3Apassword@host/app",
            "ssh://a@b@host/app",
            "ssh://-host/app",
            "ssh://-user@host/app",
            "ssh://host:/app",
            "ssh://host:0/app",
            "ssh://host:65536/app",
            "ssh://host:+22/app",
            "ssh://host:abc/app",
            "ssh://::1/app",
            "ssh://[not-ipv6]/app",
            "ssh://[::1]suffix/app",
            "ssh://[::1]:/app",
            "ssh://ho st/app",
            "ssh://host/a b",
            "ssh://host/../app",
            "ssh://host/./app",
            "ssh://host//app",
            "ssh://host/app/",
            "ssh://host/a/../../app",
            "ssh://host/%2e%2e/app",
            "ssh://host/app%2fgit",
            "ssh://host/app?token=secret",
            "ssh://host/app#fragment",
            "ssh://host/app\\name",
            "ssh://host/$(command)",
            "ssh://host/app;command",
            "ssh://host/'app'",
            "ssh://host/~user/app",
            "ssh://host/-app",
            "ssh://host/a\nb",
            "ssh://host/app\n",
            "\tssh://host/app",
            "ssh://host/.git",
            "ssh://host/.hidden.git",
        ] {
            assert!(folder_name(url, None).is_err(), "accepted {url:?}");
        }
    }

    #[test]
    fn overrides_only_the_folder_not_url_validation() {
        assert_eq!(
            folder_name("ssh://host/.hidden.git", Some("visible")).unwrap(),
            "visible"
        );
        for name in [
            "", ".", "..", ".hidden", "-option", "../app", "a/b", "a\\b", "a\nb", " app", "app ",
            "a:b", "a%2fb",
        ] {
            assert!(
                folder_name("ssh://host/app.git", Some(name)).is_err(),
                "{name:?}"
            );
        }
        assert!(folder_name("https://host/app", Some("safe")).is_err());
        assert!(folder_name("ssh://host/../app", Some("safe")).is_err());
        assert!(folder_name("ssh://host/app", Some(&"x".repeat(256))).is_err());
        assert!(folder_name("ssh://host/app", Some(&"é".repeat(128))).is_err());
        assert!(folder_name("ssh://host/app", Some(&"x".repeat(255))).is_ok());
    }

    #[test]
    fn preflight_preserves_paths_and_allows_uncloned_entries_at_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = ServerConfig::default();
        config.initialize(Some(dir.path().as_os_str())).unwrap();
        let root = config.repository_dir.clone().unwrap();
        let url = "ssh://host/app.git";
        assert_eq!(destination(&config, url, None).unwrap(), root.join("app"));
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::create_dir(root.join("app")).unwrap();
        assert!(destination(&config, url, None).is_err());
        fs::write(root.join("app/keep"), "contents").unwrap();
        assert!(destination(&config, url, None).is_err());
        assert_eq!(
            fs::read_to_string(root.join("app/keep")).unwrap(),
            "contents"
        );
        fs::write(root.join("file"), "keep").unwrap();
        assert!(destination(&config, url, Some("file")).is_err());
        config.repositories = (0..100)
            .map(|i| Repository {
                url: format!("ssh://host/repo{i}.git"),
                checkout_path: None,
            })
            .collect();
        assert!(destination(&config, url, Some("new")).is_err());
        assert!(destination(&config, "ssh://host/repo0.git", None).is_ok());
        config.repositories[0].checkout_path = Some(root.join("missing"));
        assert!(destination(&config, "ssh://host/repo0.git", Some("other")).is_err());
        assert!(destination(&config, "ssh://host/repo1.git", Some("missing")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_live_and_dangling_symlink_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = ServerConfig::default();
        config.initialize(Some(dir.path().as_os_str())).unwrap();
        let root = config.repository_dir.as_ref().unwrap();
        for target in [root.clone(), root.join("missing")] {
            std::os::unix::fs::symlink(target, root.join("app")).unwrap();
            assert!(destination(&config, "ssh://host/app.git", None).is_err());
            assert!(
                root.join("app")
                    .symlink_metadata()
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            fs::remove_file(root.join("app")).unwrap();
        }
    }
}
