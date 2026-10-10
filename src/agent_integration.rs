//! Embedded Pi source and runner-owned, private launch resources.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Local recovery association, never part of a remote session summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    /// Bundled source version retained for this agent's lifetime.
    pub version: u32,
    /// Unique runner-owned directory; never an attachment or display identity.
    pub directory: PathBuf,
}

#[cfg(target_os = "linux")]
pub use linux::Resource;

#[cfg(target_os = "linux")]
mod linux {
    use super::Metadata;
    use crate::{Result, config::AgentCommand, sessions::SessionId};
    use std::{
        fs::{File, OpenOptions},
        io::Write,
        os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        path::{Path, PathBuf},
        process::Command,
    };

    const SOURCE: &str = include_str!("../agent-extensions/pi.ts");
    const VERSION: u32 = 1;

    /// A unique directory belongs to one runner, not to the daemon. Once an
    /// agent is spawned, Drop deliberately retains it until verified cleanup.
    pub struct Resource {
        metadata: Metadata,
        anchor: File,
        extension: PathBuf,
        socket: PathBuf,
        live: bool,
    }

    fn private_directory(path: &Path) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid only reads the effective identity.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err("Pi resources require a private owned directory".into());
        }
        Ok(file)
    }

    impl Resource {
        /// Publish a versioned source exclusively in a fresh private directory.
        /// Errors disable instrumentation; they must not prevent agent launch.
        pub fn new(runtime: &Path, id: &SessionId) -> Result<Self> {
            if !runtime.is_absolute() || runtime.canonicalize()? != runtime {
                return Err("Pi runtime must be canonical and absolute".into());
            }
            let _parent = private_directory(runtime)?;
            let temporary = tempfile::Builder::new().prefix("pi-").tempdir_in(runtime)?;
            std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))?;
            let directory = temporary.path().to_path_buf();
            let socket = directory.join(format!("a-{}.activity.sock", String::from(id.clone())));
            if socket.as_os_str().as_encoded_bytes().len() > 107 {
                return Err("Pi activity endpoint exceeds the Unix path limit".into());
            }
            let anchor = private_directory(&directory)?;
            let extension = directory.join("wumpa-pi.ts");
            let mut source = tempfile::NamedTempFile::new_in(&directory)?;
            source
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
            source.write_all(SOURCE.as_bytes())?;
            source.as_file().sync_all()?;
            source.persist_noclobber(&extension)?;
            anchor.sync_all()?;
            // Retain across daemon restarts and every Pi reload. Cleanup is
            // explicit after the runner has proved all descendants terminated.
            let _ = temporary.keep();
            Ok(Self {
                metadata: Metadata {
                    version: VERSION,
                    directory,
                },
                anchor,
                extension,
                socket,
                live: false,
            })
        }

        /// Non-secret association for runner status and compatible recovery.
        pub fn metadata(&self) -> Metadata {
            self.metadata.clone()
        }

        /// Configure a command created with the executable only: add literal
        /// integration/configured arguments and authoritative child environment.
        pub fn configure(
            &self,
            child: &mut Command,
            command: &AgentCommand,
            label: &str,
            id: &SessionId,
        ) {
            // Insert before configured arguments, including any `--` terminator.
            let arguments = command.arguments();
            child.args(["--extension"]).arg(&self.extension);
            if seed_name(&arguments[1..]) {
                child.args(["--name", label]);
            }
            child.args(&arguments[1..]);
            child.env("WUMPA_ACTIVITY_SOCKET", &self.socket);
            child.env("WUMPA_SESSION_ID", String::from(id.clone()));
        }

        /// Do not delete a live agent's reload source during an error unwind.
        pub fn launched(&mut self) {
            self.live = true;
        }

        /// Call only after verified descendant termination, not discovery loss.
        pub fn terminated(&mut self) {
            self.live = false;
        }
    }

    impl Drop for Resource {
        fn drop(&mut self) {
            if self.live {
                return;
            }
            let cleanup = (|| -> Result<()> {
                let observed = std::fs::symlink_metadata(&self.metadata.directory)?;
                let pinned = self.anchor.metadata()?;
                if !observed.is_dir()
                    || observed.dev() != pinned.dev()
                    || observed.ino() != pinned.ino()
                    || observed.uid() != pinned.uid()
                    || observed.mode() & 0o7777 != 0o700
                {
                    return Err("Pi resource directory replaced; retaining resources".into());
                }
                // This exclusively created directory contains only this agent's
                // artifacts. remove_dir_all does not follow embedded symlinks.
                std::fs::remove_dir_all(&self.metadata.directory)?;
                Ok(())
            })();
            if cleanup.is_err() {
                crate::output::error("Pi resource cleanup unavailable; retained for inspection");
            }
        }
    }

    // Pi 1.0.4 literal option grammar. Unknown extension/wrapper options are
    // conservative: do not initialize a name whose selection cannot be proven.
    fn seed_name(arguments: &[String]) -> bool {
        let mut index = 0;
        while let Some(argument) = arguments.get(index) {
            match argument.as_str() {
                "--" => break,
                "--name" | "-n" | "--continue" | "-c" | "--resume" | "-r" | "--session"
                | "--session-id" | "--fork" => return false,
                "--provider"
                | "--model"
                | "--api-key"
                | "--system-prompt"
                | "--append-system-prompt"
                | "--session-dir"
                | "--models"
                | "--tools"
                | "-t"
                | "--exclude-tools"
                | "-xt"
                | "--thinking"
                | "--export"
                | "--extension"
                | "-e"
                | "--skill"
                | "--prompt-template"
                | "--theme"
                | "--mode"
                | "--use-theme"
                | "--tui-mode" => index += 1,
                "--print" | "-p" | "--list-models" => {
                    if arguments
                        .get(index + 1)
                        .is_some_and(|next| !next.starts_with('-') && !next.starts_with('@'))
                    {
                        index += 1;
                    }
                }
                "--help"
                | "-h"
                | "--version"
                | "-v"
                | "--no-session"
                | "--no-tools"
                | "-nt"
                | "--no-builtin-tools"
                | "-nbt"
                | "--no-extensions"
                | "-ne"
                | "--no-mcp"
                | "--no-skills"
                | "-ns"
                | "--no-prompt-templates"
                | "-np"
                | "--no-themes"
                | "--no-context-files"
                | "-nc"
                | "--verbose"
                | "--approve"
                | "-a"
                | "--no-approve"
                | "-na"
                | "--offline" => {}
                value if value.starts_with('-') => return false,
                _ => {}
            }
            index += 1;
        }
        true
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn id() -> SessionId {
            SessionId::try_from("a".repeat(32)).unwrap()
        }

        #[test]
        fn names_preserve_configured_selection_and_do_not_parse_values_as_options() {
            let args = |values: &[&str]| {
                values
                    .iter()
                    .map(|value| (*value).into())
                    .collect::<Vec<_>>()
            };
            for flag in [
                "--name",
                "-n",
                "--continue",
                "-c",
                "--resume",
                "-r",
                "--session",
                "--session-id",
                "--fork",
            ] {
                assert!(!seed_name(&args(&[flag, "existing"])));
            }
            assert!(seed_name(&args(&["--model", "--resume", "--", "--name"])));
            assert!(seed_name(&args(&["--offline", "--extension", "normal.ts"])));
            assert!(!seed_name(&args(&["--custom-wrapper-option", "value"])));
        }

        #[test]
        fn embedded_source_is_private_unique_and_retained_only_while_live() {
            let runtime = tempfile::tempdir().unwrap();
            std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let mut first = Resource::new(runtime.path(), &id()).unwrap();
            let second = Resource::new(runtime.path(), &id()).unwrap();
            assert_ne!(first.metadata.directory, second.metadata.directory);
            assert_eq!(std::fs::read_to_string(&first.extension).unwrap(), SOURCE);
            assert_eq!(
                std::fs::metadata(&first.extension).unwrap().mode() & 0o7777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&first.metadata.directory).unwrap().mode() & 0o7777,
                0o700
            );
            let retained = first.metadata.directory.clone();
            first.launched();
            drop(first);
            assert!(retained.join("wumpa-pi.ts").exists());
            let removed = second.metadata.directory.clone();
            drop(second);
            assert!(!removed.exists());
        }

        #[test]
        fn occupied_or_unsafe_runtime_and_replaced_resources_are_never_followed() {
            let parent = tempfile::tempdir().unwrap();
            let runtime = parent.path().join("runtime");
            std::fs::create_dir(&runtime).unwrap();
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(Resource::new(&runtime, &id()).is_err());
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
            let alias = parent.path().join("alias");
            std::os::unix::fs::symlink(&runtime, &alias).unwrap();
            assert!(Resource::new(&alias, &id()).is_err());
            let resource = Resource::new(&runtime, &id()).unwrap();
            let saved = runtime.join("saved");
            std::fs::rename(&resource.metadata.directory, &saved).unwrap();
            std::os::unix::fs::symlink(&saved, &resource.metadata.directory).unwrap();
            drop(resource);
            assert!(saved.join("wumpa-pi.ts").exists());
        }

        #[test]
        fn concurrent_materializations_are_exclusive_and_cleanup_does_not_follow_symlinks() {
            let runtime = tempfile::tempdir().unwrap();
            std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let workers = (0..8)
                .map(|_| {
                    let path = runtime.path().to_path_buf();
                    std::thread::spawn(move || Resource::new(&path, &id()).unwrap())
                })
                .collect::<Vec<_>>();
            let resources = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>();
            let mut paths = resources
                .iter()
                .map(|resource| resource.metadata.directory.clone())
                .collect::<Vec<_>>();
            paths.sort();
            paths.dedup();
            assert_eq!(paths.len(), 8);
            let outside = runtime.path().join("untouched");
            std::fs::write(&outside, "keep").unwrap();
            std::os::unix::fs::symlink(&outside, resources[0].metadata.directory.join("link"))
                .unwrap();
            drop(resources);
            assert!(paths.iter().all(|path| !path.exists()));
            assert_eq!(std::fs::read_to_string(outside).unwrap(), "keep");
        }

        #[test]
        fn long_socket_paths_disable_integration_without_retaining_partial_files() {
            let parent = tempfile::tempdir().unwrap();
            let runtime = parent.path().join("x".repeat(100));
            std::fs::create_dir(&runtime).unwrap();
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(Resource::new(&runtime, &id()).is_err());
            assert_eq!(std::fs::read_dir(runtime).unwrap().count(), 0);
        }
    }
}
