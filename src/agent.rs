//! Operation-scoped SSH agent socket validation; never changes global environment.

use std::path::{Path, PathBuf};

use crate::Result;

/// Resolve only the explicitly supplied process environment, never search for agents.
pub fn from_environment() -> Result<PathBuf> {
    let socket = std::env::var_os("SSH_AUTH_SOCK")
        .filter(|value| !value.is_empty())
        .ok_or(
            "SSH_AUTH_SOCK is missing; enable SSH agent forwarding (-A) and unlock your agent",
        )?;
    validate(Path::new(&socket))
}

/// Require an absolute Unix socket owned by this server/helper's effective user.
/// This checks the endpoint, not key availability or approval; Git will test those.
#[cfg(unix)]
pub fn validate(path: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    if !path.is_absolute() {
        return Err("SSH_AUTH_SOCK must be an absolute Unix socket path".into());
    }
    let socket = std::fs::canonicalize(path).map_err(|error| {
        format!("cannot resolve SSH_AUTH_SOCK: {error}; reconnect with agent forwarding")
    })?;
    let metadata = std::fs::metadata(&socket)?;
    if !metadata.file_type().is_socket() {
        return Err("SSH_AUTH_SOCK is not a Unix socket; check your agent configuration".into());
    }
    // SAFETY: geteuid only reads the current process's effective user ID.
    validate_owner(metadata.uid(), unsafe { libc::geteuid() })?;
    Ok(socket)
}

#[cfg(unix)]
fn validate_owner(owner: u32, current_user: u32) -> Result<()> {
    if owner != current_user {
        return Err("SSH_AUTH_SOCK must belong to the server user".into());
    }
    Ok(())
}

/// Report unsupported agent forwarding rather than accepting an unchecked path.
#[cfg(not(unix))]
pub fn validate(_path: &Path) -> Result<PathBuf> {
    Err("SSH agent forwarding is supported on macOS and Linux only".into())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::{fs::symlink, net::UnixListener};

    use super::*;

    #[test]
    fn validates_owned_sockets_and_resolves_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent");
        let _listener = UnixListener::bind(&path).unwrap();
        let link = dir.path().join("link");
        symlink(&path, &link).unwrap();
        assert_eq!(
            validate(&link).unwrap(),
            std::fs::canonicalize(path).unwrap()
        );
    }

    #[test]
    fn rejects_another_users_socket_owner() {
        assert!(validate_owner(1000, 1000).is_ok());
        assert!(validate_owner(1001, 1000).is_err());
    }

    #[test]
    fn rejects_missing_relative_and_non_socket_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "not a socket").unwrap();
        for path in [
            Path::new("relative"),
            dir.path(),
            &file,
            &dir.path().join("missing"),
        ] {
            assert!(validate(path).is_err());
        }
    }
}
