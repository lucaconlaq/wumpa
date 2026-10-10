//! Server-authorized cascading deletion and shared confirmation prompts.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{Result, config::ServerConfig};

/// A daemon-managed resource, never an arbitrary recursive-deletion path.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Agent {
        id: crate::sessions::SessionId,
    },
    Worktree {
        path: PathBuf,
    },
    /// Resolve a tracked folder to its repository or linked worktree server-side.
    Folder {
        path: PathBuf,
    },
    Repository {
        url: String,
    },
}

/// Server-derived warning and required second-confirmation text.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prompt {
    pub description: String,
    /// Exact second-confirmation text; empty means only the initial yes is needed.
    pub second: String,
}

/// Local control result; no prompt means deletion completed successfully.
#[derive(Default, Serialize, Deserialize)]
pub struct Reply {
    pub prompt: Option<Prompt>,
    pub error: Option<String>,
}

/// Caller holds both configuration and manager locks throughout the operation.
pub fn execute(
    target: &Target,
    confirmation: Option<&str>,
    config: &mut ServerConfig,
    manager: &mut crate::session_runtime::Manager,
) -> Result<Option<Prompt>> {
    if let Target::Folder { path } = target {
        let target = match config
            .repositories
            .iter()
            .find(|repo| repo.checkout_path.as_ref() == Some(path))
        {
            Some(repo) => Target::Repository {
                url: repo.url.clone(),
            },
            None => Target::Worktree { path: path.clone() },
        };
        return execute(&target, confirmation, config, manager);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    if let Target::Agent { id } = target {
        let prompt = Prompt {
            description: "Stop and delete this agent?".into(),
            second: String::new(),
        };
        if !confirmed(&prompt, confirmation)? {
            return Ok(Some(prompt));
        }
        manager.delete_agent(id, deadline)?;
        return Ok(None);
    }
    let (index, root, selected) = match target {
        Target::Repository { url } => {
            let index = config
                .repositories
                .iter()
                .position(|repo| repo.url == *url)
                .ok_or("repository is no longer registered")?;
            let root = config.repositories[index].checkout_path.clone();
            (index, root.clone(), root)
        }
        Target::Worktree { path } => {
            let groups = crate::worktrees::discover(&config.repositories);
            let group = groups
                .iter()
                .find(|group| {
                    group.error.is_none() && group.entries.iter().any(|entry| entry.path == *path)
                })
                .ok_or("worktree is no longer registered")?;
            let index = config
                .repositories
                .iter()
                .position(|repo| repo.url == group.url)
                .ok_or("repository is no longer registered")?;
            (
                index,
                config.repositories[index].checkout_path.clone(),
                Some(path.clone()),
            )
        }
        Target::Agent { .. } | Target::Folder { .. } => return Err("invalid folder target".into()),
    };
    let repository = matches!(target, Target::Repository { .. });
    let name = root
        .as_ref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| config.repositories[index].url.clone());
    let mut entries = Vec::new();
    if let Some(root) = &root {
        if std::fs::symlink_metadata(root)?.file_type().is_symlink() {
            return Err("refusing to delete a symlinked repository checkout".into());
        }
        entries = crate::worktrees::list(root, deadline)?;
        if entries
            .first()
            .map(|entry| entry.path.canonicalize())
            .transpose()?
            != Some(root.canonicalize()?)
        {
            return Err("registered checkout is not the main worktree; removal aborted".into());
        }
    }
    let dirty = if !repository {
        let selected = selected.as_ref().ok_or("missing worktree path")?;
        if Some(selected.canonicalize()?)
            == root.as_ref().map(|root| root.canonicalize()).transpose()?
        {
            return Err("select the repository to delete its main checkout".into());
        }
        !crate::worktrees::git_output(
            selected,
            &["status", "--porcelain", "--untracked-files=all"],
            deadline,
        )?
        .is_empty()
    } else {
        false
    };
    let prompt = Prompt {
        description: if repository {
            format!("Delete repository {name}, ALL its worktrees and agents, and files on disk?")
        } else {
            format!(
                "Delete worktree {} and its agents?{}",
                selected.as_ref().ok_or("missing worktree path")?.display(),
                if dirty {
                    " Uncommitted changes will be lost."
                } else {
                    ""
                }
            )
        },
        second: if repository {
            name
        } else if dirty {
            "DELETE".into()
        } else {
            String::new()
        },
    };
    if !confirmed(&prompt, confirmation)? {
        return Ok(Some(prompt));
    }
    if let Some(root) = &root {
        let targets = if repository {
            entries
                .iter()
                .skip(1)
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>()
        } else {
            vec![selected.clone().ok_or("missing worktree path")?]
        };
        for path in targets {
            let observed = crate::checkout::observe(&path, deadline)?;
            manager.with_checkout_removal(&observed, || {
                // Stopping an agent can flush new changes after inspection.
                if !repository && prompt.second.is_empty()
                    && !crate::worktrees::git_output(&path, &["status", "--porcelain", "--untracked-files=all"], deadline)?.is_empty()
                {
                    return Err("worktree now has uncommitted changes; inspect deletion again for a second confirmation".into());
                }
                let text = path.to_str().ok_or("worktree path is not UTF-8")?;
                crate::worktrees::git_output(
                    root,
                    &["worktree", "remove", "--force", "--force", "--", text],
                    deadline,
                )?;
                Ok(())
            })?;
        }
        if repository {
            let observed = crate::checkout::observe(root, deadline)?;
            manager.with_checkout_removal(&observed, || {
                // Rename to a private sibling before recursive removal; never follow
                // symlinks or recursively remove a replacement at the saved path.
                let staging = tempfile::Builder::new()
                    .prefix(".wumpa-delete-")
                    .tempdir_in(root.parent().ok_or("missing checkout parent")?)?;
                let destination = staging.path().join("checkout");
                std::fs::rename(root, &destination)?;
                let retained = staging.keep();
                let moved = crate::checkout::Observation::read(&destination)?;
                if moved.device != observed.root.device || moved.inode != observed.root.inode {
                    // Keep the unexpected directory for manual recovery, not deletion.
                    return Err(format!(
                        "checkout changed; retained for recovery at {}",
                        retained.display()
                    )
                    .into());
                }
                if let Err(error) = forget_repository(config, index) {
                    // A failed metadata save must not strand the main checkout.
                    // Restore exclusively, never overwrite a newly created path.
                    if let Err(restore) = crate::clone::publish(&destination, root) {
                        return Err(format!(
                            "{error}; restore failed: {restore}; checkout retained at {}",
                            destination.display()
                        )
                        .into());
                    }
                    let _ = std::fs::remove_dir(&retained);
                    return Err(error);
                }
                std::fs::remove_dir_all(&retained).map_err(|error| {
                    format!(
                        "repository unregistered, but cleanup failed at {}: {error}",
                        retained.display()
                    )
                })?;
                Ok(())
            })?;
        }
    }
    if repository && root.is_none() {
        forget_repository(config, index)?;
    }
    Ok(None)
}

fn forget_repository(config: &mut ServerConfig, index: usize) -> Result<()> {
    let mut next = config.clone();
    next.repositories.remove(index);
    crate::config::save(&crate::config::path("server")?, &next)?;
    *config = next;
    Ok(())
}

fn confirmed(prompt: &Prompt, confirmation: Option<&str>) -> Result<bool> {
    match confirmation {
        None => Ok(false),
        Some(value) if value == prompt.second => Ok(true),
        Some(_) => Err("confirmation does not match; inspect deletion again".into()),
    }
}

/// Prompt after inline terminal restoration, so destructive input is never hidden.
pub fn confirm_plain(prompt: &Prompt) -> Result<Option<String>> {
    use std::io::{self, Write};
    println!("{} [y/N]", crate::output::clean(&prompt.description));
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if input.trim() != "y" {
        return Ok(None);
    }
    if !prompt.second.is_empty() {
        println!(
            "Type {} to confirm permanently deleting files:",
            crate::output::clean(&prompt.second)
        );
        io::stdout().flush()?;
        input.clear();
        io::stdin().read_line(&mut input)?;
        if input.trim() != prompt.second {
            return Ok(None);
        }
    }
    Ok(Some(prompt.second.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_confirmation_is_required() {
        let prompt = Prompt {
            description: "Delete".into(),
            second: "repo".into(),
        };
        assert!(!confirmed(&prompt, None).unwrap());
        assert!(confirmed(&prompt, Some("repo")).unwrap());
        assert!(confirmed(&prompt, Some("yes")).is_err());
    }
}
