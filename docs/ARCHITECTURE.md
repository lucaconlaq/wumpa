# Architecture

## Purpose

Wumpa manages remote repositories, worktrees, and persistent agent sessions from a
terminal. This document describes the current source and configuration, not a
future behavioral contract. Canonical terms live in the [glossary](GLOSSARY.md).

## Entry Points

`src/main.rs` dispatches the default TUI, `--plain` menus, `serve`, `agent`,
`agent-create`, `check-clone`, and `worktrees list`. Hidden commands provide SSH
attachment, clone-helper, and persistent agent-runner entry points.

## Technology used

A single Rust 2024 binary declares Rust 1.85 as its minimum. Clap parses commands;
Ratatui and Crossterm implement the TUI; Serde and serde_json encode configuration
and protocol messages. Git, OpenSSH, tmux, and an agent executable are external
processes. The bundled Pi activity extension uses TypeScript and Node built-ins.
Optional TNT deployment uses NixOS, Terraform, and Google Cloud.

## Main Modules

| Modules | Responsibilities |
| --- | --- |
| `tui/`, `tui.rs`, `client`, `output` | Interactive and plain client presentation. |
| `config` | Client profiles, server settings, repository metadata, and JSON persistence. |
| `transport`, `protocol` | Local/SSH request transport and remote wire contracts. |
| `server`, `daemon` | Loopback service, operation coordination, and detached startup. |
| `repository`, `clone`, `helper`, `agent` | Clone validation, bounded execution, SSH handoff, and SSH-agent endpoint checks. |
| `checkout`, `worktrees` | Checkout observations and read-only Git discovery. |
| `control`, `agent_client`, `sessions` | Local control, session models, and typed local operations. |
| `session_cli/`, `session_cli`, `session_environment` | Creation/attachment flows and caller launch environment preparation. |
| `session_runtime` | Instance-owned tmux execution, persistent supervision, and creation outcomes. |
| `agent_integration`, `agent_activity` | Embedded Pi source resources and observational activity subscriptions. |
| `deletion` | Server-authorized deletion and confirmation. |

## Module Boundaries / Seams

- Presentation calls transport or local control rather than owning persistent
  agent processes. Remote summaries exclude local attachment targets and launch
  environments.
- The loopback protocol and Unix control protocol are separate contracts. Local
  control validates instance, peer, and checkout identity before session operations.
- Repository preflight does not reserve a destination; clone execution owns
  publication and rollback. Checkout observations are not durable deletion IDs.
- A Linux agent runner owns descendant supervision and integration resources,
  allowing agent sessions to outlive daemon restarts. The daemon must not infer
  safe cleanup merely from unavailable discovery.
- Pi activity is observational, independent of Git discovery and agent-control
  locks. It does not authorize lifecycle operations or establish approval.

## Data Flow

1. The client loads server profiles and sends requests directly to a loopback
   endpoint or through SSH. The server loads repository metadata and exposes
   discovery and operation results.
2. Remote clone requests use a one-shot SSH helper for the caller's SSH-agent
   context. Validation precedes bounded Git execution and no-clobber publication;
   successful operations update server metadata.
3. Local agent commands select an explicit control socket, validate a tracked
   checkout, and request creation or attachment. On Linux, the runner launches the
   configured executable inside an instance-owned tmux environment.
4. Instrumented Pi processes publish activity snapshots over private Unix sockets.
   Server subscriptions validate/cache observations; client summaries and refresh
   requests consume that cache without exposing endpoint paths.

## Storage / Persistence

`config` resolves `WUMPA_CLIENT_CONFIG` or `WUMPA_SERVER_CONFIG` overrides, then
`XDG_CONFIG_HOME/wumpa/{client,server}.json`, then the equivalent under
`$HOME/.config`. Client files store profiles and the last selected server; server
files store repository and launch settings. Concurrent client writers are not
supported.

Checkouts live beneath the configured repository directory. Instance-owned runtime
resources include the control/tmux sockets, runner recovery metadata, creation
outcomes, and per-agent Pi source copies. Launch environments are not recovery
records. There is no application database.

## External Integrations

OpenSSH supplies remote access and clone credential handoff; Git supplies repository
and worktree operations; Zed opens remote checkouts; tmux supplies persistent
terminals. Pi integration details, security boundaries, validation commands, and
explicitly unverified workflows remain in the
[extension documentation](../agent-extensions/README.md). Optional deployment
instructions remain in [TNT documentation](../tnt/README.md).

## Testing Seams

Inline Rust tests cover protocol/configuration and module-level behavior.
`tests/` contains CLI, clone, control-socket, SSH-command, attachment, lifecycle,
and platform-specific integration tests. Node tests exercise the Pi activity
protocol; optional real-Pi tests require an explicitly configured executable.
See the extension documentation for prerequisites and limits. Test presence is
not a claim that every environment or workflow has been verified.

## Important Constraints

- The server binds TCP to loopback; remote access relies on SSH. Session-control
  socket paths must be absolute and live in private directories.
- Agent creation requires Linux containment support; unsupported platforms fail
  closed rather than promise equivalent supervision.
- Socket ownership, filesystem identity, bounds, deadlines, literal arguments, and
  terminal-control escaping are security boundaries, not presentation details.
- Rust 1.85 remains the declared compatibility floor. Mise selects `latest`, while
  CI uses `stable` across Linux/macOS targets; neither establishes minimum-version
  compatibility. See [Rust rules](rules/RUST.md).
- `tnt/README.md` and `tnt/terraform/variables.tf` refer to `state.tf`, which is
  absent from the current checkout. Backend/bootstrap instructions need resolution
  before being treated as a verified deployment procedure.
