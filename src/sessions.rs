//! Backend-independent session model, local operations, and retry outcome contracts.
//!
//! Launch environments belong only to create requests, never recovery records.

// Wire contracts include lifecycle/error variants not exercised on every platform.
#![allow(dead_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::checkout::{Checkout, Observation};
use crate::session_environment::Environment;

fn validate_id(value: &str) -> Result<(), &'static str> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("session/request IDs must be 32 lowercase hexadecimal characters");
    }
    Ok(())
}

/// Daemon-generated opaque identity, independent of any backend session name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(String);

impl TryFrom<String> for SessionId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_id(&value)?;
        Ok(Self(value))
    }
}

impl From<SessionId> for String {
    fn from(id: SessionId) -> Self {
        id.0
    }
}

/// Caller-generated creation key; retries reuse it, not a new key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CreationId(String);

impl TryFrom<String> for CreationId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_id(&value)?;
        Ok(Self(value))
    }
}

impl From<CreationId> for String {
    fn from(id: CreationId) -> Self {
        id.0
    }
}

/// Maximum Unicode characters in a user-visible session name.
pub const MAX_NAME_CHARACTERS: usize = 64;

/// Display-only name; never used as an executable, argument, or tmux identifier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionName(String);

impl TryFrom<String> for SessionName {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
        {
            return Err("Session names must be a single line without control characters.");
        }
        let value = value.trim();
        if value.is_empty() || value.chars().count() > MAX_NAME_CHARACTERS {
            return Err("Session names must contain 1–64 characters.");
        }
        Ok(Self(value.into()))
    }
}

impl From<SessionName> for String {
    fn from(name: SessionName) -> Self {
        name.0
    }
}

/// Checkout association, independent of the caller's subdirectory or agent cwd.
/// These observations are NOT a durable removal/replacement identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutAssociation {
    pub root: Observation,
    pub git_directory: Observation,
    pub common_directory: Observation,
}

impl From<&Checkout> for CheckoutAssociation {
    fn from(checkout: &Checkout) -> Self {
        Self {
            root: checkout.root.clone(),
            git_directory: checkout.git_directory.clone(),
            common_directory: checkout.common_directory.clone(),
        }
    }
}

/// Only live or incompletely cleaned-up sessions are listed; exits are removed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Starting,
    Running,
    Stopping,
    CleanupFailed,
}

/// Backend-independent metadata; contains no environment or attachment target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub id: SessionId,
    /// Canonical control socket path, stable across daemon restarts.
    pub instance: PathBuf,
    pub checkout: CheckoutAssociation,
    /// Validated user name or daemon default, never an executable/backend identifier.
    pub label: String,
    pub state: State,
}

/// Local attachment details, kept separate from remotely visible metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum Attachment {
    Tmux { socket: PathBuf, session: String },
}

/// Typed failures without subprocess output, environment values, or free-form text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    IncompatibleVersion,
    RunChanged,
    CheckoutUnavailable,
    CheckoutMismatch,
    SessionNotFound,
    SessionNotRunning,
    OwnershipMismatch,
    RequestConflict,
    CreationInProgress,
    CapacityExceeded,
    OutcomeUnknown,
    BackendUnavailable,
    UnsupportedPlatform,
    UnsupportedFilesystem,
    AgentUnavailable,
    InvalidEnvironment,
    MissingCallerPath,
    RelativeExecutableUnsupported,
    LaunchFailed,
    CleanupFailed,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => f.write_str(
                "Agent sessions require a Linux server. Repository browsing and cloning remain available.",
            ),
            _ => write!(f, "{self:?}"),
        }
    }
}

impl From<crate::session_environment::Error> for Failure {
    fn from(error: crate::session_environment::Error) -> Self {
        use crate::session_environment::Error;
        match error {
            Error::MissingPath => Self::MissingCallerPath,
            Error::RelativeExecutableUnsupported => Self::RelativeExecutableUnsupported,
            Error::ExecutableUnavailable => Self::AgentUnavailable,
            _ => Self::InvalidEnvironment,
        }
    }
}

/// Unix-only operations; environments travel only on new creation attempts.
/// TCP dispatch deliberately has no corresponding session-control operations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    List {
        observations: Box<Checkout>,
    },
    Create {
        request_id: CreationId,
        observations: Box<Checkout>,
        environment: Environment,
        /// Optional display name; omission preserves legacy clients' default label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<SessionName>,
    },
    /// After re-handshake, look up the original attempt; never launch from a retry.
    RetryCreate {
        request_id: CreationId,
        originating_run_id: String,
        observations: Box<Checkout>,
    },
    Attach {
        session_id: SessionId,
        observations: Box<Checkout>,
    },
}

/// Local-only results; attachment targets must never enter remote snapshots.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalResponse {
    Listed { sessions: Vec<Session> },
    Created { session_id: SessionId },
    Attached { attachment: Attachment },
    Failed { failure: Failure },
}

/// Expected current run is checked before discovery, lookup, or any backend action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRequest {
    pub version: u32,
    pub run_id: String,
    pub operation: Operation,
}

impl LocalRequest {
    /// This only checks the envelope, not peer, checkout, or session ownership.
    pub fn validate_run(&self, current_run_id: &str) -> Result<(), Failure> {
        if self.version != 1 {
            return Err(Failure::IncompatibleVersion);
        }
        if self.run_id != current_run_id {
            return Err(Failure::RunChanged);
        }
        Ok(())
    }
}

/// Non-secret outcome record, not an authoritative process registry.
/// A created session ID remains the outcome even after the agent has exited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CreationOutcome {
    InProgress,
    Created { session_id: SessionId },
    Failed { failure: Failure },
}

/// Retain accepted attempt outcomes for the daemon run, without launch environments.
/// Restart recovery requires backend evidence; absence is not proof of no launch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreationRecord {
    pub instance: PathBuf,
    pub originating_run_id: String,
    pub request_id: CreationId,
    pub checkout: CheckoutAssociation,
    pub outcome: CreationOutcome,
}

/// Resolve a retry using a retained or reconciled record, never authorizing launch.
/// Only pass records whose backend ownership has already been verified.
/// Missing/ambiguous recovery evidence is represented by `None`, not empty success.
pub fn resolve_retry(
    instance: &std::path::Path,
    originating_run_id: &str,
    request_id: &CreationId,
    checkout: &CheckoutAssociation,
    record: Option<&CreationRecord>,
) -> Result<SessionId, Failure> {
    let record = record.ok_or(Failure::OutcomeUnknown)?;
    if record.instance != instance {
        return Err(Failure::OwnershipMismatch);
    }
    if record.originating_run_id != originating_run_id
        || record.request_id != *request_id
        || record.checkout != *checkout
    {
        return Err(Failure::RequestConflict);
    }
    match &record.outcome {
        CreationOutcome::InProgress => Err(Failure::CreationInProgress),
        CreationOutcome::Created { session_id } => Ok(session_id.clone()),
        CreationOutcome::Failed { failure } => Err(failure.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout() -> Checkout {
        let observation = |path: &str| Observation {
            path: path.into(),
            device: 1,
            inode: 2,
        };
        Checkout {
            directory: observation("/checkout/sub"),
            root: observation("/checkout"),
            git_directory: observation("/checkout/.git"),
            common_directory: observation("/checkout/.git"),
        }
    }

    fn record() -> CreationRecord {
        CreationRecord {
            instance: "/private/control.sock".into(),
            originating_run_id: "old-run".into(),
            request_id: CreationId::try_from("a".repeat(32)).unwrap(),
            checkout: CheckoutAssociation::from(&checkout()),
            outcome: CreationOutcome::Created {
                session_id: SessionId::try_from("b".repeat(32)).unwrap(),
            },
        }
    }

    fn retry(record: Option<&CreationRecord>) -> Result<SessionId, Failure> {
        let expected = self::record();
        resolve_retry(
            &expected.instance,
            &expected.originating_run_id,
            &expected.request_id,
            &expected.checkout,
            record,
        )
    }

    #[test]
    fn session_names_are_bounded_validated_and_display_only() {
        for value in [
            "".into(),
            " ".into(),
            "x".repeat(65),
            "line\nbreak".into(),
            "\x1b[31m".into(),
            "name\u{2028}line".into(),
        ] {
            assert!(serde_json::from_value::<SessionName>(serde_json::json!(value)).is_err());
        }
        let name = SessionName::try_from("  Review 🤖  ".to_owned()).unwrap();
        assert_eq!(String::from(name), "Review 🤖");
        assert!(SessionName::try_from("🤖".repeat(64)).is_ok());
        let mut create = serde_json::to_value(Operation::Create {
            request_id: record().request_id,
            observations: Box::new(checkout()),
            environment: Environment::default(),
            name: None,
        })
        .unwrap();
        assert!(create.get("name").is_none());
        assert!(serde_json::from_value::<Operation>(create.clone()).is_ok());
        create["name"] = serde_json::json!("named session");
        assert!(serde_json::from_value::<Operation>(create.clone()).is_ok());
        create["name"] = serde_json::json!("invalid\nname");
        assert!(serde_json::from_value::<Operation>(create.clone()).is_err());
        create["action"] = serde_json::json!("list");
        create.as_object_mut().unwrap().remove("request_id");
        create.as_object_mut().unwrap().remove("environment");
        assert!(serde_json::from_value::<Operation>(create).is_err());
    }

    #[test]
    fn ids_are_bounded_and_validated_on_decode() {
        for value in ["".into(), "a".repeat(31), "a".repeat(33), "G".repeat(32)] {
            let json = serde_json::to_string(&value).unwrap();
            assert!(serde_json::from_str::<SessionId>(&json).is_err());
            assert!(serde_json::from_str::<CreationId>(&json).is_err());
        }
        let id = SessionId::try_from("0123456789abcdef".repeat(2)).unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<SessionId>(&json).unwrap(), id);
    }

    #[test]
    fn metadata_excludes_caller_directory_and_attachment_details() {
        let original = checkout();
        let mut subdirectory = original.clone();
        subdirectory.directory.path = "/checkout/other".into();
        let association = CheckoutAssociation::from(&original);
        assert_eq!(association, CheckoutAssociation::from(&subdirectory));
        let session = Session {
            id: SessionId::try_from("b".repeat(32)).unwrap(),
            instance: "/private/control.sock".into(),
            checkout: association,
            label: "pi".into(),
            state: State::Running,
        };
        let mut json = serde_json::to_value(&session).unwrap();
        assert_eq!(
            serde_json::from_value::<Session>(json.clone()).unwrap(),
            session
        );
        assert!(json.get("attachment").is_none());
        assert!(json.get("environment").is_none());
        json["environment"] = serde_json::json!({"TOKEN": "secret"});
        assert!(serde_json::from_value::<Session>(json).is_err());
    }

    #[test]
    fn operations_round_trip_and_check_current_run() {
        let record = record();
        for operation in [
            Operation::List {
                observations: Box::new(checkout()),
            },
            Operation::Create {
                request_id: record.request_id.clone(),
                observations: Box::new(checkout()),
                environment: Environment::default(),
                name: None,
            },
            Operation::RetryCreate {
                request_id: record.request_id.clone(),
                originating_run_id: record.originating_run_id.clone(),
                observations: Box::new(checkout()),
            },
            Operation::Attach {
                session_id: SessionId::try_from("b".repeat(32)).unwrap(),
                observations: Box::new(checkout()),
            },
        ] {
            let mut request = LocalRequest {
                version: 1,
                run_id: "current-run".into(),
                operation,
            };
            let json = serde_json::to_vec(&request).unwrap();
            assert_eq!(
                serde_json::from_slice::<LocalRequest>(&json).unwrap(),
                request
            );
            assert_eq!(request.validate_run("current-run"), Ok(()));
            assert_eq!(request.validate_run("old-run"), Err(Failure::RunChanged));
            request.version = 2;
            assert_eq!(
                request.validate_run("current-run"),
                Err(Failure::IncompatibleVersion)
            );
        }
    }

    #[test]
    fn only_creation_carries_environment_and_debug_is_redacted() {
        let environment = Environment::from_entries([
            (b"TOKEN".to_vec(), b"secret-sentinel".to_vec()),
            (b"BYTES".to_vec(), vec![0xff]),
        ])
        .unwrap();
        let create = Operation::Create {
            request_id: record().request_id,
            observations: Box::new(checkout()),
            environment: environment.clone(),
            name: None,
        };
        let json = serde_json::to_vec(&create).unwrap();
        assert_eq!(serde_json::from_slice::<Operation>(&json).unwrap(), create);
        assert!(!format!("{create:?}").contains("secret-sentinel"));
        let mut missing = serde_json::to_value(create).unwrap();
        missing.as_object_mut().unwrap().remove("environment");
        assert!(serde_json::from_value::<Operation>(missing).is_err());
        for operation in [
            Operation::List {
                observations: Box::new(checkout()),
            },
            Operation::RetryCreate {
                request_id: record().request_id,
                originating_run_id: "old-run".into(),
                observations: Box::new(checkout()),
            },
            Operation::Attach {
                session_id: SessionId::try_from("b".repeat(32)).unwrap(),
                observations: Box::new(checkout()),
            },
        ] {
            let mut json = serde_json::to_value(operation).unwrap();
            json["environment"] = serde_json::to_value(&environment).unwrap();
            assert!(serde_json::from_value::<Operation>(json).is_err());
        }
        let mut record = serde_json::to_value(record()).unwrap();
        record["environment"] = serde_json::to_value(environment).unwrap();
        assert!(serde_json::from_value::<CreationRecord>(record).is_err());
        assert_eq!(
            Failure::from(crate::session_environment::Error::MissingPath),
            Failure::MissingCallerPath
        );
    }

    #[test]
    fn retry_returns_same_outcome_or_explicit_uncertainty_never_launch() {
        let mut record = record();
        let first = retry(Some(&record)).unwrap();
        assert_eq!(retry(Some(&record)).unwrap(), first);
        let recovered: CreationRecord =
            serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(retry(Some(&recovered)).unwrap(), first);
        assert_eq!(retry(None), Err(Failure::OutcomeUnknown));
        record.outcome = CreationOutcome::InProgress;
        assert_eq!(retry(Some(&record)), Err(Failure::CreationInProgress));
        record.outcome = CreationOutcome::Failed {
            failure: Failure::LaunchFailed,
        };
        assert_eq!(retry(Some(&record)), Err(Failure::LaunchFailed));
    }

    #[test]
    fn retry_rejects_foreign_instances_keys_runs_and_checkout_reassociation() {
        let mut foreign = record();
        foreign.instance = "/other/control.sock".into();
        assert_eq!(retry(Some(&foreign)), Err(Failure::OwnershipMismatch));
        let mut wrong_run = record();
        wrong_run.originating_run_id = "another-run".into();
        assert_eq!(retry(Some(&wrong_run)), Err(Failure::RequestConflict));
        let mut wrong_key = record();
        wrong_key.request_id = CreationId::try_from("c".repeat(32)).unwrap();
        assert_eq!(retry(Some(&wrong_key)), Err(Failure::RequestConflict));
        let mut replacement = record();
        replacement.checkout.root.inode += 1;
        assert_eq!(retry(Some(&replacement)), Err(Failure::RequestConflict));
    }

    #[test]
    fn local_targets_are_separate_and_requests_reject_arbitrary_commands() {
        let attachment = Attachment::Tmux {
            socket: "/private/tmux.sock".into(),
            session: "wumpa-session".into(),
        };
        let response = LocalResponse::Attached { attachment };
        let json = serde_json::to_vec(&response).unwrap();
        assert_eq!(
            serde_json::from_slice::<LocalResponse>(&json).unwrap(),
            response
        );
        let operation = Operation::Create {
            request_id: record().request_id,
            observations: Box::new(checkout()),
            environment: Environment::default(),
            name: None,
        };
        let mut json = serde_json::to_value(operation).unwrap();
        json["command"] = serde_json::json!(["arbitrary-command"]);
        assert!(serde_json::from_value::<Operation>(json).is_err());
    }
}
