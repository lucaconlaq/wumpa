//! Local terminal selection and direct tmux attachment, never configuration access.

use crate::Result;
use std::path::Path;

#[cfg(unix)]
pub fn run(socket: &Path, retry: Option<&str>) -> Result<()> {
    use crate::{
        control, output,
        sessions::{Attachment, CreationId, Failure, LocalResponse, Operation},
    };
    use std::{
        io::{BufRead, Write},
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
    let checkout = control::preflight(&handshake, &directory)?;
    let session_id = if let Some(retry) = retry {
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
                    "Agent retry failed: {failure:?}; no additional agent was launched"
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
                return Err(format!("Agent listing failed: {failure:?}").into());
            }
            _ => return Err("invalid agent list response".into()),
        };
        let selected = if sessions.is_empty() {
            None
        } else {
            for (index, session) in sessions.iter().enumerate() {
                let id: String = session.id.clone().into();
                println!(
                    "{}) {} · {} · {:?}",
                    index + 1,
                    output::clean(&session.label),
                    id,
                    session.state
                );
            }
            print!("n) Create new   q) Cancel\nChoice: ");
            std::io::stdout().flush()?;
            let mut input = String::new();
            if std::io::stdin().lock().read_line(&mut input)? == 0 {
                return Ok(());
            }
            match input.trim() {
                "q" | "" => return Ok(()),
                "n" => None,
                number => {
                    let index = number
                        .parse::<usize>()
                        .ok()
                        .and_then(|value| value.checked_sub(1))
                        .filter(|index| *index < sessions.len())
                        .ok_or("invalid agent selection")?;
                    Some(sessions[index].id.clone())
                }
            }
        };
        match selected {
            Some(id) => id,
            None => {
                let request_id = CreationId::try_from(crate::session_runtime::random_id()?)?;
                let key: String = request_id.clone().into();
                let environment = crate::session_environment::Environment::capture()?;
                let result = control::sessions(
                    &handshake,
                    Operation::Create {
                        request_id,
                        observations: Box::new(checkout.clone()),
                        environment,
                    },
                );
                match result {
                    Ok(LocalResponse::Created { session_id }) => session_id,
                    Ok(LocalResponse::Failed { failure }) if failure != Failure::OutcomeUnknown => {
                        return Err(format!("Agent creation failed: {failure:?}").into());
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
                "Agent attachment failed: {failure:?}; an existing agent was not stopped"
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
pub fn run(_socket: &Path, _retry: Option<&str>) -> Result<()> {
    Err("local agent sessions require Unix".into())
}
