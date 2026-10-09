//! Bounded, byte-preserving local launch environments; never recovery metadata.

// Preparation APIs remain available on Unix platforms without execution support.
#![allow(dead_code)]

use std::{collections::HashSet, fmt};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeSeq};

/// Limits apply before filtering; size is compact canonical JSON, including Base64.
pub const MAX_ENCODED: usize = 256 * 1024;
pub const MAX_VARIABLES: usize = 4096;

const FILTERED: &[&[u8]] = &[
    b"TMUX",
    b"TMUX_PANE",
    b"STY",
    b"WINDOW",
    b"PWD",
    b"OLDPWD",
    b"SHLVL",
    b"_",
    b"LINES",
    b"COLUMNS",
    b"SSH_TTY",
    b"TERM",
    b"TERMCAP",
];

/// Errors deliberately contain no variable names, values, or caller paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidEntry,
    DuplicateName,
    TooManyVariables,
    TooLarge,
    InvalidDirectory,
    UnrepresentablePath,
    MissingPath,
    RelativeExecutableUnsupported,
    ExecutableUnavailable,
    UnsupportedPlatform,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidEntry => "invalid launch environment entry",
            Self::DuplicateName => "duplicate launch environment name",
            Self::TooManyVariables => "launch environment exceeds 4096 variables",
            Self::TooLarge => "encoded launch environment exceeds 256 KiB",
            Self::InvalidDirectory => "launch directories must be absolute",
            Self::UnrepresentablePath => "adjusted PATH cannot represent caller directory",
            Self::MissingPath => "caller PATH is missing for a bare agent executable",
            Self::RelativeExecutableUnsupported => {
                "relative agent executable paths are not yet supported"
            }
            Self::ExecutableUnavailable => "configured agent executable is unavailable",
            Self::UnsupportedPlatform => "launch environments require Unix",
        })
    }
}

impl std::error::Error for Error {}

#[derive(Clone, PartialEq, Eq)]
struct Entry {
    name: Vec<u8>,
    value: Vec<u8>,
}

fn entry_size(entry: &Entry) -> Result<usize, Error> {
    if entry.name.is_empty()
        || entry.name.contains(&b'=')
        || entry.name.contains(&0)
        || entry.value.contains(&0)
    {
        return Err(Error::InvalidEntry);
    }
    let mut size = b"{\"name\":\"\",\"value\":\"\"}".len();
    for length in [entry.name.len(), entry.value.len()] {
        let encoded = base64::encoded_len(length, true).ok_or(Error::TooLarge)?;
        size = size.checked_add(encoded).ok_or(Error::TooLarge)?;
    }
    Ok(size)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEntry {
    name: String,
    value: String,
}

/// Secret-bearing local payload. Debug is redacted; serialization is transport only.
/// Names/values use standard padded Base64; duplicate decoded names are rejected.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Environment(Vec<Entry>);

impl fmt::Debug for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Environment([REDACTED])")
    }
}

impl Environment {
    /// Validate unfiltered entries without modifying process-global environment.
    pub fn from_entries(
        entries: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
    ) -> Result<Self, Error> {
        let mut result = Self::default();
        let mut size = 2usize;
        for (name, value) in entries {
            if result.0.len() == MAX_VARIABLES {
                return Err(Error::TooManyVariables);
            }
            let entry = Entry { name, value };
            size = size
                .checked_add(entry_size(&entry)?)
                .and_then(|size| size.checked_add(usize::from(!result.0.is_empty())))
                .ok_or(Error::TooLarge)?;
            if size > MAX_ENCODED {
                return Err(Error::TooLarge);
            }
            result.0.push(entry);
        }
        result.validate()?;
        Ok(result)
    }

    /// Capture the invoking CLI's byte-preserving environment, not daemon settings.
    #[cfg(unix)]
    pub fn capture() -> Result<Self, Error> {
        use std::os::unix::ffi::OsStringExt;
        Self::from_entries(
            std::env::vars_os().map(|(name, value)| (name.into_vec(), value.into_vec())),
        )
    }

    fn validate(&self) -> Result<(), Error> {
        if self.0.len() > MAX_VARIABLES {
            return Err(Error::TooManyVariables);
        }
        let mut names = HashSet::new();
        let mut size = 2usize; // Array brackets.
        for (index, entry) in self.0.iter().enumerate() {
            if !names.insert(entry.name.as_slice()) {
                return Err(Error::DuplicateName);
            }
            size = size
                .checked_add(entry_size(entry)?)
                .and_then(|size| size.checked_add(usize::from(index > 0)))
                .ok_or(Error::TooLarge)?;
            if size > MAX_ENCODED {
                return Err(Error::TooLarge);
            }
        }
        Ok(())
    }
}

impl Serialize for Environment {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for entry in &self.0 {
            sequence.serialize_element(&WireEntry {
                name: STANDARD.encode(&entry.name),
                value: STANDARD.encode(&entry.value),
            })?;
        }
        sequence.end()
    }
}

impl<'de> Deserialize<'de> for Environment {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Environment;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a bounded Base64 environment array")
            }

            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                let mut encoded_size = 2usize;
                while let Some(entry) = sequence
                    .next_element::<WireEntry>()
                    .map_err(|_| de::Error::custom(Error::InvalidEntry))?
                {
                    if entries.len() == MAX_VARIABLES {
                        return Err(de::Error::custom(Error::TooManyVariables));
                    }
                    // Bound allocations before decoding, even for malformed Base64.
                    encoded_size = encoded_size.saturating_add(
                        entry
                            .name
                            .len()
                            .saturating_add(entry.value.len())
                            .saturating_add(b"{\"name\":\"\",\"value\":\"\"}".len())
                            .saturating_add(usize::from(!entries.is_empty())),
                    );
                    if encoded_size > MAX_ENCODED {
                        return Err(de::Error::custom(Error::TooLarge));
                    }
                    let name = STANDARD
                        .decode(entry.name)
                        .map_err(|_| de::Error::custom(Error::InvalidEntry))?;
                    let value = STANDARD
                        .decode(entry.value)
                        .map_err(|_| de::Error::custom(Error::InvalidEntry))?;
                    entries.push(Entry { name, value });
                }
                let result = Environment(entries);
                result.validate().map_err(de::Error::custom)?;
                Ok(result)
            }
        }
        // Suppress deserializer errors that might contain secret input values.
        deserializer
            .deserialize_seq(Visitor)
            .map_err(|_| de::Error::custom("invalid or oversized launch environment"))
    }
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::{CString, OsStr, OsString},
        os::unix::ffi::{OsStrExt, OsStringExt},
        path::{Path, PathBuf},
        process::Command,
    };

    use super::{Entry, Environment, Error, FILTERED};

    /// Filtered and PATH-adjusted environment; intentionally not serializable.
    pub struct PreparedEnvironment(Environment);

    impl std::fmt::Debug for PreparedEnvironment {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("PreparedEnvironment([REDACTED])")
        }
    }

    impl Environment {
        /// Call only after daemon validation of the original caller directory.
        pub fn prepare(&self, caller_directory: &Path) -> Result<PreparedEnvironment, Error> {
            if !caller_directory.is_absolute() {
                return Err(Error::InvalidDirectory);
            }
            let mut entries = Vec::new();
            for entry in &self.0 {
                if FILTERED.contains(&entry.name.as_slice()) {
                    continue;
                }
                let mut entry = entry.clone();
                if entry.name == b"PATH" {
                    let mut adjusted = Vec::new();
                    for (index, component) in entry.value.split(|byte| *byte == b':').enumerate() {
                        if index > 0 {
                            adjusted.push(b':');
                        }
                        let component = Path::new(OsStr::from_bytes(component));
                        let absolute = if component.is_absolute() {
                            component.to_path_buf()
                        } else {
                            // Unix PATH cannot encode a directory containing ':'.
                            if caller_directory.as_os_str().as_bytes().contains(&b':') {
                                return Err(Error::UnrepresentablePath);
                            }
                            caller_directory.join(component)
                        };
                        adjusted.extend_from_slice(absolute.as_os_str().as_bytes());
                        if adjusted.len() > super::MAX_ENCODED {
                            return Err(Error::TooLarge);
                        }
                    }
                    entry.value = adjusted;
                }
                entries.push(entry);
            }
            let result = Environment(entries);
            // Normalization must not bypass the agreed size limit.
            result.validate()?;
            Ok(PreparedEnvironment(result))
        }
    }

    fn executable(path: &Path) -> bool {
        if !path.metadata().is_ok_and(|metadata| metadata.is_file()) {
            return false;
        }
        let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: path is a valid NUL-terminated string. faccessat only checks
        // access; AT_EACCESS uses the daemon's effective credentials.
        unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0 }
    }

    impl PreparedEnvironment {
        /// Resolve only using the caller's adjusted PATH. No shell or service PATH.
        /// This is a pre-launch check, not protection against filesystem races.
        pub fn resolve_executable(&self, configured: &str) -> Result<PathBuf, Error> {
            if configured.is_empty() || configured.contains('\0') {
                return Err(Error::ExecutableUnavailable);
            }
            let configured = Path::new(configured);
            if configured.is_absolute() {
                return executable(configured)
                    .then(|| configured.to_path_buf())
                    .ok_or(Error::ExecutableUnavailable);
            }
            if configured.as_os_str().as_bytes().contains(&b'/') {
                // Its base-directory policy has not been agreed; fail closed.
                return Err(Error::RelativeExecutableUnsupported);
            }
            let path = self
                .0
                .0
                .iter()
                .find(|entry| entry.name == b"PATH")
                .ok_or(Error::MissingPath)?;
            for component in path.value.split(|byte| *byte == b':') {
                let candidate = Path::new(OsStr::from_bytes(component)).join(configured);
                if executable(&candidate) {
                    return Ok(candidate);
                }
            }
            Err(Error::ExecutableUnavailable)
        }

        /// Build a fresh command using server settings, with literal arguments.
        /// Does not spawn; the backend still supplies terminal/process ownership.
        /// Never log the returned Command: its Debug includes secret environment.
        pub fn command(
            &self,
            configured: &crate::config::AgentCommand,
            checkout_root: &Path,
        ) -> Result<Command, Error> {
            if !checkout_root.is_absolute() {
                return Err(Error::InvalidDirectory);
            }
            let arguments = configured.arguments();
            let configured_path = Path::new(&arguments[0]);
            let executable = if !configured_path.is_absolute() && arguments[0].contains('/') {
                // A server-configured repository script is relative to the validated
                // checkout root, unlike relative caller PATH entries.
                let path = checkout_root.join(configured_path);
                self.resolve_executable(path.to_str().ok_or(Error::InvalidDirectory)?)?;
                // Keep exec relative: the runner fchdirs into the pinned root.
                // An absolute pathname could execute from a replacement checkout.
                configured_path.to_path_buf()
            } else {
                self.resolve_executable(&arguments[0])?
            };
            let mut command = Command::new(executable);
            command.args(&arguments[1..]);
            self.apply_to(&mut command, checkout_root)?;
            Ok(command)
        }

        /// Apply to one fresh agent command, not a tmux server or global environment.
        /// The terminal backend must subsequently supply fresh TERM/terminal data.
        pub fn apply_to(&self, command: &mut Command, checkout_root: &Path) -> Result<(), Error> {
            if !checkout_root.is_absolute() {
                return Err(Error::InvalidDirectory);
            }
            command.env_clear();
            for Entry { name, value } in &self.0.0 {
                command.env(
                    OsString::from_vec(name.clone()),
                    OsString::from_vec(value.clone()),
                );
            }
            command.current_dir(checkout_root).env("PWD", checkout_root);
            Ok(())
        }
    }
}

/// Filtered per-process environment; never serializable or recovery metadata.
#[cfg(unix)]
pub type PreparedEnvironment = unix::PreparedEnvironment;

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(entries: &[(&[u8], &[u8])]) -> Environment {
        Environment::from_entries(
            entries
                .iter()
                .map(|(name, value)| (name.to_vec(), value.to_vec())),
        )
        .unwrap()
    }

    #[test]
    fn base64_preserves_non_utf8_and_empty_values() {
        let input = environment(&[(b"KEY\xff", b"value\xfe=\n"), (b"EMPTY", b"")]);
        let json = serde_json::to_vec(&input).unwrap();
        assert_eq!(serde_json::from_slice::<Environment>(&json).unwrap(), input);
        let wire: Vec<WireEntry> = serde_json::from_slice(&json).unwrap();
        assert_eq!(STANDARD.decode(&wire[0].name).unwrap(), b"KEY\xff");
        assert_eq!(format!("{input:?}"), "Environment([REDACTED])");
    }

    #[test]
    fn malformed_entries_and_duplicates_fail_without_secret_echoes() {
        for entries in [
            vec![(vec![], b"secret".to_vec())],
            vec![(b"bad=name".to_vec(), b"secret".to_vec())],
            vec![(b"bad\0name".to_vec(), b"secret".to_vec())],
            vec![(b"KEY".to_vec(), b"secret\0".to_vec())],
        ] {
            let error = Environment::from_entries(entries.clone()).unwrap_err();
            assert_eq!(error, Error::InvalidEntry);
            let wire: Vec<_> = entries
                .into_iter()
                .map(|(name, value)| WireEntry {
                    name: STANDARD.encode(name),
                    value: STANDARD.encode(value),
                })
                .collect();
            let error = serde_json::from_slice::<Environment>(&serde_json::to_vec(&wire).unwrap())
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret"));
            assert!(!error.contains(&STANDARD.encode("secret")));
        }
        assert_eq!(
            Environment::from_entries(vec![
                (b"KEY".to_vec(), b"one".to_vec()),
                (b"KEY".to_vec(), b"two".to_vec()),
            ])
            .unwrap_err(),
            Error::DuplicateName
        );
        assert!(
            serde_json::from_str::<Environment>(
                r#"[{"name":"S0VZ","value":""},{"name":"S0VZ","value":""}]"#
            )
            .is_err()
        );
        for json in [
            r#"[{"name":"secret!","value":""}]"#,
            r#"[{"name":"S0VZ","value":{"secret":"token"}}]"#,
            r#"[{"name":"S0VZ","value":"","secret":"token"}]"#,
            r#""secret-token""#,
        ] {
            let error = serde_json::from_str::<Environment>(json)
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret"));
            assert!(!error.contains("token"));
        }
    }

    #[test]
    fn count_and_encoded_size_boundaries_are_enforced_in_both_directions() {
        let entries = (0..MAX_VARIABLES)
            .map(|index| (format!("KEY{index}").into_bytes(), Vec::new()))
            .collect::<Vec<_>>();
        let input = Environment::from_entries(entries.clone()).unwrap();
        let encoded = serde_json::to_vec(&input).unwrap();
        assert_eq!(
            serde_json::from_slice::<Environment>(&encoded).unwrap(),
            input
        );
        let mut excess = entries;
        excess.push((b"EXTRA".to_vec(), Vec::new()));
        assert_eq!(
            Environment::from_entries(excess).unwrap_err(),
            Error::TooManyVariables
        );
        let mut wire: Vec<WireEntry> = serde_json::from_slice(&encoded).unwrap();
        wire.push(WireEntry {
            name: STANDARD.encode("EXTRA"),
            value: String::new(),
        });
        assert!(
            serde_json::from_slice::<Environment>(&serde_json::to_vec(&wire).unwrap()).is_err()
        );

        // One 3-byte name encodes to 4 bytes; object/array overhead totals 24.
        let value = vec![b'x'; (MAX_ENCODED - 28) / 4 * 3];
        let input = environment(&[(b"KEY", &value)]);
        let encoded = serde_json::to_vec(&input).unwrap();
        assert_eq!(encoded.len(), MAX_ENCODED);
        assert_eq!(
            serde_json::from_slice::<Environment>(&encoded).unwrap(),
            input
        );
        let mut excess = value;
        excess.extend_from_slice(b"xxx");
        assert_eq!(
            Environment::from_entries(vec![(b"KEY".to_vec(), excess.clone())]).unwrap_err(),
            Error::TooLarge
        );
        let wire = [WireEntry {
            name: STANDARD.encode("KEY"),
            value: STANDARD.encode(excess),
        }];
        assert!(
            serde_json::from_slice::<Environment>(&serde_json::to_vec(&wire).unwrap()).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn filtering_and_path_adjustment_preserve_other_bytes() {
        use std::{os::unix::ffi::OsStrExt, path::Path, process::Command};
        let mut entries = FILTERED
            .iter()
            .map(|name| (name.to_vec(), b"stale".to_vec()))
            .collect::<Vec<_>>();
        entries.extend([
            (b"PATH".to_vec(), b"bin::/absolute:\xff/tools".to_vec()),
            (b"SSH_AUTH_SOCK".to_vec(), b"/agent.sock".to_vec()),
            (b"TOKEN".to_vec(), b"secret\xff".to_vec()),
        ]);
        let prepared = Environment::from_entries(entries)
            .unwrap()
            .prepare(Path::new("/caller"))
            .unwrap();
        let mut command = Command::new("unused");
        command.env("DAEMON_ONLY", "not-inherited");
        prepared
            .apply_to(&mut command, Path::new("/checkout"))
            .unwrap();
        let get = |name: &[u8]| {
            command
                .get_envs()
                .find(|(key, _)| key.as_bytes() == name)
                .and_then(|(_, value)| value)
                .map(|value| value.as_bytes())
        };
        for name in FILTERED {
            if *name != b"PWD" {
                assert!(get(name).is_none());
            }
        }
        assert_eq!(get(b"PWD"), Some(b"/checkout".as_slice()));
        assert_eq!(
            get(b"PATH"),
            Some(b"/caller/bin:/caller/:/absolute:/caller/\xff/tools".as_slice())
        );
        assert_eq!(get(b"TOKEN"), Some(b"secret\xff".as_slice()));
        assert_eq!(get(b"SSH_AUTH_SOCK"), Some(b"/agent.sock".as_slice()));
        assert!(get(b"DAEMON_ONLY").is_none());
        assert_eq!(command.get_current_dir(), Some(Path::new("/checkout")));
        assert_eq!(format!("{prepared:?}"), "PreparedEnvironment([REDACTED])");
    }

    #[cfg(unix)]
    #[test]
    fn normalization_rechecks_size_and_rejects_ambiguous_directories() {
        use std::path::Path;
        let input = environment(&[(b"PATH", b"bin:")]);
        assert_eq!(
            input.prepare(Path::new("relative")).unwrap_err(),
            Error::InvalidDirectory
        );
        assert_eq!(
            input.prepare(Path::new("/caller:alias")).unwrap_err(),
            Error::UnrepresentablePath
        );
        let input = environment(&[(b"PATH", &vec![b':'; 1024])]);
        assert_eq!(
            input
                .prepare(Path::new(&format!("/{}", "x".repeat(256))))
                .unwrap_err(),
            Error::TooLarge
        );
    }

    #[cfg(unix)]
    #[cfg(target_os = "linux")]
    #[test]
    fn relative_executable_uses_pinned_root_after_replacement() {
        use std::{
            fs,
            os::{
                fd::AsRawFd,
                unix::{fs::PermissionsExt, process::CommandExt},
            },
        };
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let script = root.join("agent");
        fs::write(&script, "#!/bin/sh\nprintf original").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let anchor = fs::File::open(&root).unwrap();
        let prepared = Environment::from_entries([])
            .unwrap()
            .prepare(&root)
            .unwrap();
        let configured =
            serde_json::from_str::<crate::config::AgentCommand>(r#"["./agent"]"#).unwrap();
        let mut command = prepared.command(&configured, &root).unwrap();
        assert_eq!(command.get_program(), "./agent");
        fs::rename(&root, directory.path().join("old")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(&script, "#!/bin/sh\nprintf replacement").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let fd = anchor.as_raw_fd();
        // SAFETY: fchdir is async-signal-safe; anchor is live through spawn.
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(fd) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"original");
    }

    #[test]
    fn executable_resolution_uses_only_caller_path_and_absolute_paths() {
        use std::{fs, os::unix::fs::PermissionsExt, path::Path};
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("harmless agent");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let absent = Environment::default().prepare(dir.path()).unwrap();
        assert_eq!(
            absent.resolve_executable("harmless agent"),
            Err(Error::MissingPath)
        );
        assert_eq!(
            absent
                .resolve_executable(executable.to_str().unwrap())
                .unwrap(),
            executable
        );
        assert_eq!(
            absent.resolve_executable("./harmless agent"),
            Err(Error::RelativeExecutableUnsupported)
        );
        let empty = environment(&[(b"PATH", b"")]).prepare(dir.path()).unwrap();
        assert_eq!(
            empty.resolve_executable("harmless agent").unwrap(),
            executable
        );
        let wrong = environment(&[(b"PATH", b"/nonexistent")])
            .prepare(Path::new("/caller"))
            .unwrap();
        assert_eq!(
            wrong.resolve_executable("harmless agent"),
            Err(Error::ExecutableUnavailable)
        );
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            empty.resolve_executable("harmless agent"),
            Err(Error::ExecutableUnavailable)
        );
        assert_eq!(
            empty.resolve_executable(""),
            Err(Error::ExecutableUnavailable)
        );
    }

    #[cfg(unix)]
    #[test]
    fn server_command_is_literal_and_does_not_use_service_environment() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("fake pi");
        fs::write(&executable, "unused test executable").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let configured = crate::config::AgentCommand::try_from(vec![
            "fake pi".into(),
            "".into(),
            "literal $(command); '$HOME'".into(),
        ])
        .unwrap();
        let prepared = environment(&[(b"PATH", b""), (b"TOKEN", b"test-only")])
            .prepare(dir.path())
            .unwrap();
        let command = prepared.command(&configured, dir.path()).unwrap();
        assert_eq!(command.get_program(), executable.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new(""),
                std::ffi::OsStr::new("literal $(command); '$HOME'"),
            ]
        );
        assert_eq!(command.get_current_dir(), Some(dir.path()));
        let relative = crate::config::AgentCommand::try_from(vec!["./fake pi".into()]).unwrap();
        let absent = Environment::default().prepare(dir.path()).unwrap();
        assert_eq!(
            absent.command(&relative, dir.path()).unwrap().get_program(),
            std::ffi::OsStr::new("./fake pi")
        );
    }

    #[cfg(unix)]
    #[test]
    fn per_process_environments_do_not_leak_or_mutate_global_state() {
        use std::{path::Path, process::Command};
        let original = std::env::vars_os().collect::<Vec<_>>();
        let first = environment(&[(b"TOKEN", b"first")])
            .prepare(Path::new("/caller"))
            .unwrap();
        let second = environment(&[(b"TOKEN", b"second")])
            .prepare(Path::new("/caller"))
            .unwrap();
        let spawn = |prepared: &PreparedEnvironment| {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "printf '%s' \"$TOKEN\""]);
            command.stdout(std::process::Stdio::piped());
            prepared.apply_to(&mut command, Path::new("/")).unwrap();
            command.spawn().unwrap()
        };
        // Harmless concurrent child processes, never real agents or tmux.
        let a = spawn(&first);
        let b = spawn(&second);
        let a = a.wait_with_output().unwrap();
        let b = b.wait_with_output().unwrap();
        assert!(a.status.success() && b.status.success());
        assert_eq!(a.stdout, b"first");
        assert_eq!(b.stdout, b"second");
        assert_eq!(std::env::vars_os().collect::<Vec<_>>(), original);
    }
}
