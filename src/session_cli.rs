//! Local terminal selection and direct tmux attachment, never configuration access.

use crate::Result;
use std::path::Path;

#[cfg(unix)]
mod folders;
#[cfg(unix)]
mod picker;

#[cfg(unix)]
pub fn run(socket: &Path, retry: Option<&str>, plain: bool) -> Result<()> {
    run_selected(socket, retry, plain, None)
}

/// Attach only the requested existing agent; never open a picker or create one.
#[cfg(unix)]
pub fn attach_remote(port: u16, session_id: crate::sessions::SessionId) -> Result<()> {
    let response = crate::transport::request(
        &crate::config::Connection::Local { port },
        &crate::protocol::Request::List,
    )?;
    if let Some(error) = response.error {
        return Err(error.into());
    }
    let socket = response
        .control_socket
        .ok_or("Server lacks attachment discovery; update and restart the Wumpa server.")?;
    run_selected(&socket, None, false, Some(session_id))
}

#[cfg(not(unix))]
pub fn attach_remote(_port: u16, _session_id: crate::sessions::SessionId) -> Result<()> {
    Err("agent attachment requires Unix".into())
}

#[cfg(unix)]
fn run_selected(
    socket: &Path,
    retry: Option<&str>,
    plain: bool,
    attach: Option<crate::sessions::SessionId>,
) -> Result<()> {
    use crate::{
        control, output,
        sessions::{Attachment, CreationId, Failure, LocalResponse, Operation},
    };
    use std::{
        os::unix::ffi::{OsStrExt, OsStringExt},
        process::Command,
    };
    control::validate_path(socket)?;
    let handshake = control::handshake(socket)?;
    let expected = crate::session_runtime::tmux_socket(&handshake.socket)?;
    let inside = if let Some(value) = std::env::var_os("TMUX").filter(|value| !value.is_empty()) {
        let fields = value
            .as_bytes()
            .rsplitn(3, |byte| *byte == b',')
            .collect::<Vec<_>>();
        if fields.len() != 3 {
            return Err("Cannot identify current tmux server; detach and rerun".into());
        }
        let current = std::path::PathBuf::from(std::ffi::OsString::from_vec(fields[2].to_vec()));
        if current.canonicalize().ok() != expected.canonicalize().ok() || !expected.exists() {
            return Err("Inside a different tmux server. Detach and rerun wumpa agent; nested tmux is not supported".into());
        }
        true
    } else {
        false
    };
    let directory = std::env::current_dir()?;
    let checkout = match control::preflight(&handshake, &directory) {
        Ok(checkout) => checkout,
        Err(error) if attach.is_some() => return Err(error),
        Err(_) => {
            let choices = control::tracked_folders(&handshake, &directory).map_err(|error| {
                format!("Couldn't list Wumpa-tracked folders. Check the daemon, or restart it after updating Wumpa: {error}")
            })?;
            let Some(selected) = folders::choose(&choices, &directory, plain)? else {
                return Ok(());
            };
            // A displayed path is only a suggestion, never launch authorization.
            control::preflight(&handshake, &selected).map_err(|_| {
                format!("Couldn't open {}. Check its path, permissions, and Git membership, then retry.", output::clean(&selected.display().to_string()))
            })?
        }
    };
    let session_id = if let Some(session_id) = attach {
        session_id
    } else if let Some(retry) = retry {
        let (originating_run_id, id) = retry
            .split_once(':')
            .ok_or("--retry requires RUN_ID:REQUEST_ID")?;
        let request_id = CreationId::try_from(id.to_owned())?;
        match control::sessions(
            &handshake,
            Operation::RetryCreate {
                request_id,
                originating_run_id: originating_run_id.into(),
                observations: Box::new(checkout.clone()),
            },
        )? {
            LocalResponse::Created { session_id } => session_id,
            LocalResponse::Failed { failure } => {
                return Err(format!(
                    "Agent retry failed: {failure}; no additional agent was launched"
                )
                .into());
            }
            _ => return Err("invalid agent retry response".into()),
        }
    } else {
        let sessions = match control::sessions(
            &handshake,
            Operation::List {
                observations: Box::new(checkout.clone()),
            },
        )? {
            LocalResponse::Listed { sessions } => sessions,
            LocalResponse::Failed { failure } => {
                return Err(format!("Agent listing failed: {failure}").into());
            }
            _ => return Err("invalid agent list response".into()),
        };
        // The picker restores terminal modes before launch or tmux attachment.
        match picker::choose(&sessions, &checkout.root.path, plain)? {
            picker::Choice::Cancel => return Ok(()),
            picker::Choice::Attach(index) => sessions[index].id.clone(),
            picker::Choice::Create(name) => {
                let request_id = CreationId::try_from(crate::session_runtime::random_id()?)?;
                let key: String = request_id.clone().into();
                // Folder selection does not reinterpret relative/empty caller PATH.
                let environment = crate::session_environment::Environment::capture()?
                    .prepare(&directory)?
                    .into_environment();
                let result = control::sessions(
                    &handshake,
                    Operation::Create {
                        request_id,
                        observations: Box::new(checkout.clone()),
                        environment,
                        name,
                    },
                );
                match result {
                    Ok(LocalResponse::Created { session_id }) => session_id,
                    Ok(LocalResponse::Failed { failure }) if failure != Failure::OutcomeUnknown => {
                        return Err(format!("Agent creation failed: {failure}").into());
                    }
                    _ => {
                        output::info("Retry key", format!("{}:{key}", handshake.run_id));
                        return Err("Agent creation outcome is uncertain. Rerun with the same --socket and --retry key above; do not create another attempt blindly".into());
                    }
                }
            }
        }
    };
    let attachment = match control::sessions(
        &handshake,
        Operation::Attach {
            session_id,
            observations: Box::new(checkout),
        },
    )? {
        LocalResponse::Attached { attachment } => attachment,
        LocalResponse::Failed { failure } => {
            return Err(format!(
                "Agent attachment failed: {failure}; an existing agent was not stopped"
            )
            .into());
        }
        _ => return Err("invalid attachment response".into()),
    };
    let Attachment::Tmux { socket, session } = attachment;
    let status = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .arg(if inside {
            "switch-client"
        } else {
            "attach-session"
        })
        .arg("-t")
        .arg(format!("={session}"))
        .status()?;
    if !status.success() {
        return Err("tmux attachment failed; no existing agent was stopped by this command".into());
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn run(_socket: &Path, _retry: Option<&str>, _plain: bool) -> Result<()> {
    Err("local agent sessions require Unix".into())
}
