//! Local terminal selection and direct tmux attachment, never configuration access.

use crate::Result;
use std::path::Path;

#[cfg(unix)]
mod folders;
#[cfg(unix)]
mod picker;

#[cfg(unix)]
pub fn run(socket: &Path, retry: Option<&str>, plain: bool) -> Result<()> {
    run_selected(socket, retry, plain, None, None)
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
    run_selected(&socket, None, false, Some(session_id), None)
}

#[cfg(not(unix))]
pub fn attach_remote(_port: u16, _session_id: crate::sessions::SessionId) -> Result<()> {
    Err("agent attachment requires Unix".into())
}

#[cfg(unix)]
struct Creation {
    name: Option<crate::sessions::SessionName>,
    prompt: bool,
    no_attach: bool,
}

/// Discover the local daemon, create in the exact current checkout, and optionally attach.
#[cfg(unix)]
pub fn create_remote(port: u16, name: Option<&str>, no_attach: bool, plain: bool) -> Result<()> {
    if name.is_some_and(|name| {
        name.chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    }) {
        return Err("Session names must be a single line without control characters.".into());
    }
    let parsed = name
        .filter(|name| !name.trim().is_empty())
        .map(|name| crate::sessions::SessionName::try_from(name.to_owned()))
        .transpose()?;
    if no_attach && name.is_none() {
        return Err("detached creation requires --name".into());
    }
    let response = crate::transport::request(
        &crate::config::Connection::Local { port },
        &crate::protocol::Request::List,
    )?;
    let socket = response
        .control_socket
        .ok_or("Server lacks agent discovery; update and restart Wumpa.")?;
    run_selected(
        &socket,
        None,
        plain,
        None,
        Some(Creation {
            name: parsed,
            prompt: name.is_none(),
            no_attach,
        }),
    )
}

#[cfg(not(unix))]
pub fn create_remote(
    _port: u16,
    _name: Option<&str>,
    _no_attach: bool,
    _plain: bool,
) -> Result<()> {
    Err("agent creation requires Unix".into())
}

#[cfg(unix)]
fn run_selected(
    socket: &Path,
    retry: Option<&str>,
    plain: bool,
    attach: Option<crate::sessions::SessionId>,
    creation: Option<Creation>,
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
    let tmux = if creation.as_ref().is_some_and(|creation| creation.no_attach) {
        None
    } else {
        std::env::var_os("TMUX").filter(|value| !value.is_empty())
    };
    let inside = if let Some(value) = tmux {
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
        Err(error) if attach.is_some() || creation.is_some() => return Err(error),
        Err(_) => {
            let choices = control::tracked_folders(&handshake, &directory).map_err(|error| {
                format!("Couldn't list Wumpa-tracked folders. Check the daemon, or restart it after updating Wumpa: {error}")
            })?;
            let Some(selected) = folders::choose(&choices, &directory, plain, &handshake)? else {
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
        loop {
            let sessions = if creation.is_some() {
                Vec::new()
            } else {
                match control::sessions(
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
                }
            };
            // The picker restores terminal modes before launch or tmux attachment.
            let choice = match &creation {
                Some(creation) if !creation.prompt => picker::Choice::Create(creation.name.clone()),
                _ => picker::choose(&sessions, &checkout.root.path, plain)?,
            };
            match choice {
                picker::Choice::Cancel => return Ok(()),
                picker::Choice::Attach(index) => break sessions[index].id.clone(),
                picker::Choice::Delete(index) => {
                    delete_selected(
                        &handshake,
                        crate::deletion::Target::Agent {
                            id: sessions[index].id.clone(),
                        },
                    )?;
                }
                picker::Choice::DeleteFolder => {
                    if delete_selected(
                        &handshake,
                        crate::deletion::Target::Folder {
                            path: checkout.root.path.clone(),
                        },
                    )? {
                        return Ok(());
                    }
                }
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
                        Ok(LocalResponse::Created { session_id }) => break session_id,
                        Ok(LocalResponse::Failed { failure })
                            if failure != Failure::OutcomeUnknown =>
                        {
                            return Err(format!("Agent creation failed: {failure}").into());
                        }
                        _ => {
                            output::info("Retry socket", handshake.socket.display());
                            output::info("Retry key", format!("{}:{key}", handshake.run_id));
                            return Err("Agent creation outcome is uncertain. Rerun with the same --socket and --retry key above; do not create another attempt blindly".into());
                        }
                    }
                }
            }
        }
    };
    if creation.as_ref().is_some_and(|creation| creation.no_attach) {
        let id: String = session_id.into();
        crate::output::info("Created agent", id);
        return Ok(());
    }
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

#[cfg(unix)]
fn delete_selected(
    handshake: &crate::control::Handshake,
    target: crate::deletion::Target,
) -> Result<bool> {
    let reply = crate::control::delete(handshake, &target, None)?;
    if let Some(error) = reply.error {
        return Err(error.into());
    }
    let prompt = reply.prompt.ok_or("missing deletion confirmation")?;
    let Some(confirmation) = crate::deletion::confirm_plain(&prompt)? else {
        return Ok(false);
    };
    let reply = crate::control::delete(handshake, &target, Some(confirmation))?;
    if let Some(error) = reply.error {
        return Err(error.into());
    }
    Ok(true)
}

#[cfg(not(unix))]
pub fn run(_socket: &Path, _retry: Option<&str>, _plain: bool) -> Result<()> {
    Err("local agent sessions require Unix".into())
}
