//! Bounded Git execution and atomic, no-clobber checkout publication.

use std::{path::Path, process::Command, time::Duration};

use crate::Result;

/// Execute Git with only this operation's agent. No repository setup or submodules.
pub fn run(
    url: &str,
    destination: &Path,
    socket: &Path,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    let mut command = Command::new("git");
    // Ignore inherited Git overrides, including config injection and tracing.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    // The long-lived server may have been started in a now-deleted directory.
    command
        .current_dir(destination.parent().ok_or("missing clone staging directory")?)
        .env("SSH_AUTH_SOCK", socket)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "ssh")
        .env("GIT_SSH_VARIANT", "ssh")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o ControlMaster=no -o ControlPath=none -o ForwardAgent=no -o IdentityAgent=SSH_AUTH_SOCK -o ConnectTimeout=15")
        .args(["-c", "core.hooksPath=/dev/null", "clone", "--no-recurse-submodules", "--template=", "--"])
        .arg(url)
        .arg(destination);
    execute(command, crate::protocol::CLONE_TIMEOUT, cancelled)
}

#[cfg(unix)]
fn execute(mut command: Command, timeout: Duration, cancelled: impl Fn() -> bool) -> Result<()> {
    use std::{
        io::Read,
        os::{fd::AsRawFd, unix::process::CommandExt},
        process::{Child, Stdio},
        time::Instant,
    };

    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            // Kill the entire operation group, including SSH and descendants,
            // even if the Git parent has already exited. Always reap the parent.
            // SAFETY: child is in its own process group created by process_group.
            unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
            let _ = self.0.wait();
        }
    }

    if cancelled() {
        return Err("clone cancelled".into());
    }
    let mut child = Process(
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let mut errors = child.0.stderr.take().ok_or("missing Git stderr")?;
    let fd = errors.as_raw_fd();
    // SAFETY: fd is an owned live pipe; fcntl does not retain references.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let deadline = Instant::now() + timeout;
    let mut diagnostic = Vec::new();
    loop {
        if cancelled() {
            return Err("clone cancelled; temporary checkout removed".into());
        }
        if Instant::now() >= deadline {
            return Err("clone timed out; temporary checkout removed".into());
        }
        let status = child.0.try_wait()?;
        // Bound both retained output and work per poll, even with a noisy child.
        for _ in 0..16 {
            let mut bytes = [0; 4096];
            match errors.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => {
                    let keep = count.min(16_384 - diagnostic.len());
                    diagnostic.extend_from_slice(&bytes[..keep]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(status) = status {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("Git clone failed ({status}): {}. Check repository access, agent approval and the repository host's known_hosts entry on the server.", crate::output::clean(&String::from_utf8_lossy(&diagnostic))).into())
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(not(unix))]
fn execute(_command: Command, _timeout: Duration, _cancelled: impl Fn() -> bool) -> Result<()> {
    Err("cloning is supported on macOS and Linux only".into())
}

/// Atomically move a directory without replacing any existing filesystem entry.
/// Unsupported platforms fail closed rather than using a racy exists/rename pair.
pub fn publish(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let source = CString::new(source.as_os_str().as_bytes())?;
        let target = CString::new(destination.as_os_str().as_bytes())?;
        #[cfg(target_os = "linux")]
        // musl does not expose renameat2; call the kernel directly.
        // SAFETY: the arguments match renameat2's syscall ABI, and both C
        // strings remain valid for the duration of the call.
        let status = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        // SAFETY: both C strings are valid for this synchronous syscall.
        let status =
            unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
        if status == 0 {
            return Ok(());
        }
        Err(format!(
            "cannot publish checkout at {} (destination must not exist): {}",
            destination.display(),
            std::io::Error::last_os_error()
        )
        .into())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (source, destination);
        Err("atomic no-clobber publication requires macOS or Linux".into())
    }
}

/// Quarantine a failed publication before deleting it. If someone replaced the
/// public path, restore it without clobbering, or preserve it for manual recovery.
#[cfg(unix)]
pub fn rollback(destination: &Path, identity: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let root = destination.parent().ok_or("missing checkout parent")?;
    let quarantine = tempfile::Builder::new()
        .prefix(".wumpa-clone-rollback-")
        .tempdir_in(root)?;
    let moved = quarantine.path().join("checkout");
    publish(destination, &moved)?;
    let metadata = std::fs::symlink_metadata(&moved);
    let ours = metadata
        .as_ref()
        .is_ok_and(|value| value.dev() == identity.dev() && value.ino() == identity.ino());
    if !ours {
        if publish(&moved, destination).is_err() {
            let leftover = quarantine.keep();
            return Err(format!(
                "checkout was replaced; preserved unrelated path at {}",
                leftover.display()
            )
            .into());
        }
        return Err("checkout was replaced; left unrelated destination unchanged".into());
    }
    let leftover = quarantine.path().to_path_buf();
    quarantine.close().map_err(|error| {
        format!("checkout cleanup failed at {}: {error}", leftover.display()).into()
    })
}

#[cfg(not(unix))]
pub fn rollback(_destination: &Path, _identity: &std::fs::Metadata) -> Result<()> {
    Err("checkout rollback requires macOS or Linux".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn publication_never_replaces_files_directories_or_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        assert!(publish(&source, &target).is_err());
        std::fs::remove_dir(&target).unwrap();
        std::fs::write(&target, "keep").unwrap();
        assert!(publish(&source, &target).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink("missing", &target).unwrap();
        assert!(publish(&source, &target).is_err());
        std::fs::remove_file(&target).unwrap();
        publish(&source, &target).unwrap();
        assert!(target.is_dir());
        assert!(!source.exists());
    }

    #[test]
    fn rollback_preserves_replacement_and_removes_only_owned_checkout() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let identity = std::fs::symlink_metadata(&target).unwrap();
        std::fs::rename(&target, root.path().join("original")).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), "unrelated").unwrap();
        assert!(rollback(&target, &identity).is_err());
        assert_eq!(
            std::fs::read_to_string(target.join("keep")).unwrap(),
            "unrelated"
        );
        rollback(&root.path().join("original"), &identity).unwrap();
        assert!(!root.path().join("original").exists());
    }

    #[test]
    fn bounded_sanitized_failure_and_timeout() {
        let mut command = Command::new("sh");
        command.args(["-c", r"printf '\033[31merror' >&2; exit 1"]);
        let error = execute(command, Duration::from_secs(2), || false)
            .unwrap_err()
            .to_string();
        assert!(!error.contains('\x1b'));
        assert!(error.contains("error"), "{error}");
        let mut command = Command::new("sh");
        command.args(["-c", "while :; do printf 'lots of output' >&2; done"]);
        let start = std::time::Instant::now();
        assert!(
            execute(command, Duration::from_millis(100), || false)
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
