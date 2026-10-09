//! Read-only checkout validation; observations are not durable lifecycle identities.

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use serde::{Deserialize, Serialize};

use crate::{Result, config::Repository, worktrees};

/// Filesystem view of one directory, following aliases to its canonical location.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
}

impl Observation {
    /// Observe a directory without requiring it to be a Git checkout.
    pub fn read(path: &Path) -> Result<Self> {
        if !path.is_absolute() || path.to_str().is_none() {
            return Err("checkout paths must be absolute UTF-8 paths".into());
        }
        let path = path.canonicalize()?;
        if path.to_str().is_none() {
            return Err("canonical checkout paths must be UTF-8".into());
        }
        let metadata = path.metadata()?;
        if !metadata.is_dir() {
            return Err("checkout observation is not a directory".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                path,
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        Err("checkout observations require Unix".into())
    }
}

/// Required caller observations, also returned after daemon validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkout {
    pub directory: Observation,
    pub root: Observation,
    pub git_directory: Observation,
    pub common_directory: Observation,
}

fn git_path(directory: &Path, flag: &str, deadline: Instant) -> Result<PathBuf> {
    let bytes = worktrees::git_output(
        directory,
        &["rev-parse", "--path-format=absolute", flag],
        deadline,
    )?;
    // Remove exactly Git's terminator, preserving newlines in the path itself.
    let bytes = bytes
        .strip_suffix(b"\n")
        .ok_or("Git path has no terminator")?;
    let text = std::str::from_utf8(bytes).map_err(|_| "Git path is not UTF-8")?;
    let path = PathBuf::from(text);
    if !path.is_absolute() {
        return Err("Git returned a relative path".into());
    }
    Ok(path)
}

/// Collect mandatory observations without reading any Wumpa configuration.
pub fn observe(directory: &Path, deadline: Instant) -> Result<Checkout> {
    let directory = Observation::read(directory)?;
    let root = Observation::read(&git_path(&directory.path, "--show-toplevel", deadline)?)?;
    let git_directory =
        Observation::read(&git_path(&directory.path, "--absolute-git-dir", deadline)?)?;
    let common_directory =
        Observation::read(&git_path(&directory.path, "--git-common-dir", deadline)?)?;
    let checkout = Checkout {
        directory,
        root,
        git_directory,
        common_directory,
    };
    // Reject changes during discovery rather than returning mixed observations.
    for observation in [
        &checkout.directory,
        &checkout.root,
        &checkout.git_directory,
        &checkout.common_directory,
    ] {
        if Observation::read(&observation.path)? != *observation {
            return Err("filesystem changed during checkout discovery".into());
        }
    }
    Ok(checkout)
}

/// A daemon-advertised folder choice, not authorization to launch or attach.
/// Unavailable registrations remain visible but cannot be selected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackedFolder {
    pub path: PathBuf,
    pub error: Option<String>,
}

/// List registered roots and their linked worktrees within one discovery budget.
/// Selected folders still require fresh caller/daemon preflight observations.
pub fn tracked_folders(
    repositories: &[Repository],
    deadline: Instant,
) -> Result<Vec<TrackedFolder>> {
    let mut folders = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut add = |path: &Path, error: Option<&str>| -> Result<()> {
        let path = if path.is_absolute() {
            path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
        } else {
            path.to_path_buf()
        };
        if seen.insert(path.clone()) {
            if folders.len() >= 128 {
                return Err("Too many tracked folders to list; select a checkout with cd.".into());
            }
            folders.push(TrackedFolder {
                path,
                error: error.map(str::to_owned),
            });
        }
        Ok(())
    };
    for repository in repositories {
        if Instant::now() >= deadline {
            return Err("Listing tracked folders timed out; retry when Git is available.".into());
        }
        let Some(path) = &repository.checkout_path else {
            continue;
        };
        if !path.is_absolute() {
            add(path, Some("Tracked path must be absolute."))?;
            continue;
        }
        let registered = match observe(path, deadline) {
            Ok(checkout) if checkout.directory == checkout.root => checkout,
            _ => {
                add(
                    path,
                    Some("Folder unavailable; check its saved path, permissions, and Git access."),
                )?;
                continue;
            }
        };
        let entries = match worktrees::list(&registered.root.path, deadline) {
            Ok(entries) => entries,
            Err(_) => {
                add(
                    path,
                    Some("Git discovery unavailable; check this checkout."),
                )?;
                continue;
            }
        };
        for entry in entries.into_iter().filter(|entry| !entry.bare) {
            if Instant::now() >= deadline {
                return Err(
                    "Listing tracked folders timed out; retry when Git is available.".into(),
                );
            }
            let usable = !entry.prunable
                && observe(&entry.path, deadline).is_ok_and(|checkout| {
                    checkout.directory == checkout.root
                        && checkout.common_directory == registered.common_directory
                });
            add(
                &entry.path,
                (!usable).then_some(
                    "Worktree unavailable; check its path, permissions, and Git membership.",
                ),
            )?;
        }
    }
    if Instant::now() >= deadline {
        return Err("Listing tracked folders timed out; retry when Git is available.".into());
    }
    if serde_json::to_vec(&folders)?.len() > 128 * 1024 {
        return Err(
            "Tracked folder list exceeds its size budget; select a checkout with cd.".into(),
        );
    }
    Ok(folders)
}

/// Validate against an in-memory registration snapshot; never mutate configuration.
pub fn validate(
    directory: &Path,
    caller: &Checkout,
    repositories: &[Repository],
    deadline: Instant,
) -> Result<Checkout> {
    let checkout = observe(directory, deadline)?;
    if &checkout != caller {
        return Err("caller and daemon filesystem observations disagree".into());
    }
    for repository in repositories {
        let Some(path) = &repository.checkout_path else {
            continue;
        };
        if !path.is_absolute() {
            return Err("registered checkout paths must be absolute".into());
        }
    }
    for repository in repositories {
        let Some(path) = &repository.checkout_path else {
            continue;
        };
        let registered = match observe(path, deadline) {
            Ok(registered) => registered,
            // An unavailable registration cannot establish membership.
            Err(_) => continue,
        };
        // A saved subdirectory or a reused worktree path is not a registration.
        if registered.directory != registered.root
            || registered.common_directory != checkout.common_directory
        {
            continue;
        }
        let entries = worktrees::list(&registered.root.path, deadline)?;
        let member = entries
            .iter()
            .filter(|entry| !entry.bare && !entry.prunable)
            .any(|entry| Observation::read(&entry.path).is_ok_and(|root| root == checkout.root));
        if member {
            let final_checkout = observe(directory, deadline)?;
            let final_registered = observe(path, deadline)?;
            if final_checkout != checkout || final_registered != registered {
                return Err("filesystem changed during checkout validation".into());
            }
            return Ok(checkout);
        }
    }
    Err("checkout is not a member of a registered repository (or discovery is unavailable)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Command, time::Duration};

    fn git(path: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    #[test]
    fn validates_main_linked_aliases_and_rejects_independent_repositories() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main space\n'");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init"]);
        git(
            &main,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        let linked = temp.path().join("linked\n");
        git(
            &main,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );
        let repositories = vec![Repository {
            url: "test".into(),
            checkout_path: Some(main.clone()),
        }];
        let before = repositories.clone();
        let deadline = || Instant::now() + Duration::from_secs(5);
        let choices = tracked_folders(&repositories, deadline()).unwrap();
        assert_eq!(choices.len(), 2);
        assert!(choices.iter().all(|folder| folder.error.is_none()));
        assert!(
            choices
                .iter()
                .any(|folder| folder.path == linked.canonicalize().unwrap())
        );
        assert!(tracked_folders(&repositories, Instant::now()).is_err());
        let mut unavailable = repositories.clone();
        unavailable.push(Repository {
            url: "missing".into(),
            checkout_path: Some(temp.path().join("missing")),
        });
        let choices = tracked_folders(&unavailable, deadline()).unwrap();
        assert_eq!(choices.len(), 3);
        assert!(
            choices
                .iter()
                .find(|folder| folder.path == temp.path().join("missing"))
                .unwrap()
                .error
                .is_some()
        );
        for root in [&main, &linked] {
            let sub = root.join("sub");
            std::fs::create_dir(&sub).unwrap();
            let caller = observe(&sub, deadline()).unwrap();
            assert_eq!(
                validate(&sub, &caller, &repositories, deadline()).unwrap(),
                caller
            );
            let mut mismatch = caller.clone();
            mismatch.common_directory.inode ^= 1;
            assert!(validate(&sub, &mismatch, &repositories, deadline()).is_err());
            assert!(validate(&sub, &caller, &[], deadline()).is_err());
        }
        #[cfg(unix)]
        {
            let alias = temp.path().join("alias");
            std::os::unix::fs::symlink(&linked, &alias).unwrap();
            let caller = observe(&alias, deadline()).unwrap();
            assert!(validate(&alias, &caller, &repositories, deadline()).is_ok());
            let mut aliases = repositories.clone();
            aliases.push(Repository {
                url: "alias".into(),
                checkout_path: Some(alias),
            });
            assert_eq!(tracked_folders(&aliases, deadline()).unwrap().len(), 2);
        }
        let nested = main.join("independent");
        std::fs::create_dir(&nested).unwrap();
        git(&nested, &["init"]);
        let caller = observe(&nested, deadline()).unwrap();
        assert!(validate(&nested, &caller, &repositories, deadline()).is_err());
        assert!(observe(Path::new("relative"), deadline()).is_err());
        assert!(observe(temp.path(), deadline()).is_err());
        assert!(observe(&main, Instant::now()).is_err());
        let caller = observe(&linked, deadline()).unwrap();
        std::fs::rename(&linked, temp.path().join("moved")).unwrap();
        assert!(validate(&linked, &caller, &repositories, deadline()).is_err());
        std::fs::create_dir(&linked).unwrap();
        git(&linked, &["init"]);
        let replacement = observe(&linked, deadline()).unwrap();
        assert!(validate(&linked, &replacement, &repositories, deadline()).is_err());
        let bare = temp.path().join("bare");
        std::fs::create_dir(&bare).unwrap();
        git(&bare, &["init", "--bare"]);
        assert!(observe(&bare, deadline()).is_err());
        let relative = vec![Repository {
            url: "test".into(),
            checkout_path: Some("relative".into()),
        }];
        let caller = observe(&main, deadline()).unwrap();
        assert!(validate(&main, &caller, &relative, deadline()).is_err());
        assert_eq!(repositories, before);
    }
}
