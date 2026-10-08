use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::{Result, config, output};

/// Listener metadata published atomically after successful server startup.
#[derive(Serialize, Deserialize)]
pub struct Ready {
    pub pid: u32,
    pub port: u16,
    pub repositories: usize,
}

/// Display listener status and foreground or detached shutdown instructions.
pub fn banner(ready: &Ready, config: &Path, log: Option<&Path>) {
    output::heading("server");
    output::info(
        "Status",
        if log.is_some() {
            "Listening · detached"
        } else {
            "Listening · foreground"
        },
    );
    output::info("Address", format!("127.0.0.1:{}", ready.port));
    output::info("PID", ready.pid);
    output::info("Config", config.display());
    output::info(
        "Repositories",
        format!("{} saved · metadata only", ready.repositories),
    );
    if let Some(log) = log {
        output::info("Log", log.display());
        output::info("Stop", format!("kill {}", ready.pid));
    } else {
        output::info("Stop", "Ctrl-C");
    }
    output::hint("Open wumpa to connect locally or through SSH.");
}

/// Start a new session and wait up to ten seconds for listener readiness.
/// On startup failure, terminate and reap the child before returning an error.
#[cfg(unix)]
pub fn detach(port: u16) -> Result<()> {
    use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};

    let path = config::path("server")?;
    // Validate before starting a child or creating a log file.
    let _: config::ServerConfig = config::load(&path)?;
    let mut log_name = path.as_os_str().to_os_string();
    log_name.push(".log");
    let log_path = std::path::PathBuf::from(log_name);
    if let Some(parent) = log_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .mode(0o600)
        .open(&log_path)?;
    let log_offset = log.metadata()?.len();
    let directory = tempfile::tempdir()?;
    let ready_path = directory.path().join("ready.json");
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["serve", "--port", &port.to_string(), "--ready-file"])
        .arg(&ready_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    // SAFETY: setsid is async-signal-safe, touches no Rust shared state, and
    // starts a new session without a controlling terminal before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let result = (|| -> Result<Ready> {
        loop {
            if let Some(status) = child.try_wait()? {
                log.seek(SeekFrom::Start(log_offset))?;
                let mut details = String::new();
                (&mut log).take(8192).read_to_string(&mut details)?;
                return Err(
                    format!("server failed to start ({status}): {}", details.trim()).into(),
                );
            }
            match fs::read(&ready_path) {
                Ok(bytes) => return Ok(serde_json::from_slice(&bytes)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            if Instant::now() >= deadline {
                return Err(format!("server startup timed out; see {}", log_path.display()).into());
            }
            thread::sleep(Duration::from_millis(25));
        }
    })();
    match result {
        Ok(ready) => {
            banner(&ready, &path, Some(&log_path));
            // The child owns its session and log handles; dropping Child does
            // not terminate it. The readiness temp directory can now disappear.
            Ok(())
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

/// Report that detached startup is unsupported on this platform.
#[cfg(not(unix))]
pub fn detach(_port: u16) -> Result<()> {
    Err("detached mode is currently supported on macOS and Linux only".into())
}
