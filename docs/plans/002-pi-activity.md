# Real-time Pi activity and session names

## Status

Planned; not implemented. The earlier socket-path plumbing prototype is reverted
in favor of implementing the complete integration described here. No deployment
or release changes are included.

## Goal and constraints

Show whether an interactive Pi agent is working or waiting for a new prompt, and
reflect its current session name (including `/name` changes), without changing how
the user interacts with Pi.

- Keep Pi's normal terminal UI, existing tmux sessions, and SSH attachment.
- Keep the user's normal Pi extensions, skills, themes, and configuration.
- No separate extension package, npm installation, or changes to `~/.pi`.
- Bundle a small TypeScript extension inside the Wumpa binary and materialize it
  as a private runtime file only when launching an explicitly configured Pi agent.
- Use a persistent local Unix socket subscription for event-driven updates.
- Never let status-reporting failures block, terminate, or orphan agents.
- Unsupported agents and disconnected integrations report activity as `Unknown`.

Pi's built-in RPC is JSONL over stdin/stdout and replaces its interactive UI; it
is not a monitoring channel attached to interactive Pi. The extension is the
supported integration point that preserves the current workflow. Terminal parsing
and session-file inspection are possible fallbacks, but are not authoritative
busy/idle detection and are outside the initial implementation.

## Earlier prototype: changes made and reverted

### `src/session_runtime.rs`

The prototype added socket-path provisioning and local collection, not a listener
or a status reader:

1. Added `ACTIVITY_SOCKET_ENV = "WUMPA_ACTIVITY_SOCKET"`.
2. Added `activity_socket(directory, id)`, deriving
   `a-<wumpa-session-id>.activity.sock` inside the existing private runtime directory
   adjacent to the canonical control socket.
3. Added `Manager.activity_sockets: Vec<(SessionId, PathBuf)>`, initialized in both
   manager construction paths, and a local-only accessor.
4. Cleared that collection when starting a snapshot and rebuilt it from sessions
   whose backend membership, runner identity, and checkout association passed
   reconciliation. Successful creation also inserted its endpoint.
5. Before spawning the agent, the runner set `WUMPA_ACTIVITY_SOCKET` on that fresh
   child command, overriding any caller-supplied value. It did not modify global
   environment or the tmux server environment.

Paths were never exposed in remote summaries. Pi did not consume the variable:
there was no extension, socket binding, subscription, protocol, or UI update.
The variable was passed to all launched agents, which could simply ignore it.

The future implementation should reuse deterministic endpoint derivation and
per-child environment injection, but not blindly copy the registry semantics.
Distinguish unknown reconciliation from confirmed session removal; temporary
failure must invalidate activity freshness without discarding recovery identity.
Old sessions predating integration must not be treated as supported merely
because their expected socket path can be derived.

### `tests/agent_sessions.rs`

Added `agents_receive_distinct_private_activity_socket_paths`:

- Replaced the fixture's fake agent with a shell script that wrote
  `$WUMPA_ACTIVITY_SOCKET` into a pane-specific file.
- Created two agents in the same checkout.
- Asserted each received its own deterministic path under `.c.sock.sessions`.
- Asserted neither socket existed, proving the prototype reserved paths rather
  than creating listeners.

The prototype passed formatting, Clippy, and the full test suite using installed
Rust and a Nix GCC wrapper. `nix develop` was unavailable because this checkout
has no `flake.nix`. Those results validate only the reverted plumbing, not the
planned extension or push protocol. Both Rust files return to their pre-prototype
contents; unrelated README edits are preserved.

## Architecture

```text
Wumpa server (Rust)
  | creates/recovers session endpoint metadata
  | embeds and materializes the TypeScript extension
  | launches Pi through the existing runner and tmux
  |
  | connects + subscribes to private Unix socket
  v
Pi process, normal interactive mode
  | Wumpa extension owns listener
  | sends initial snapshot, then event-triggered snapshots
  | reads Pi's current session name and idle state
  v
Wumpa local activity cache -> remote-safe summaries -> dashboard
```

The extension owns the socket listener. Wumpa owns the endpoint association and
subscription client. Wumpa receives pushed updates over an established connection;
it does not poll on every dashboard refresh. Heartbeats and occasional state
reconciliation complement events, rather than replacing them.

## Extension packaging and launch

- Store the reviewed extension source in Wumpa's repository and embed it with
  Rust `include_str!()`. No runtime download or TypeScript build step is required.
- Materialize a `.ts` file in a private, user-owned runtime subdirectory with
  directory mode `0700` and file mode `0600`. Prefer Wumpa's controlled runtime
  directory over a shared `/tmp` filename or a temporary directory removed when
  the daemon exits.
- Use versioned or content-addressed filenames, safe exclusive creation, verified
  ownership/type, and atomic publication. Never overwrite a file used by a live
  session or follow an untrusted symlink.
- Launch Pi with literal `--extension <absolute-path>` arguments in addition to
  its existing configured arguments. This adds to normal extension discovery;
  it must not inject `--no-extensions` or replace the user's resources.
- Initialize a newly created Pi conversation with the Wumpa agent session label
  using Pi's supported `--name <label>` (`-n`) option, passed as literal arguments.
  This makes the initial dashboard name and Pi session name identical. Do not
  overwrite an existing name when configured arguments resume/continue a Pi
  conversation or explicitly supply a name; establish and test argument precedence.
  For an unnamed resumed conversation, the extension may initialize the name from
  the Wumpa label once. Never reinitialize it on `/reload`, `/name`, or later session
  switches. Pi's name becomes authoritative after initialization.
- Inject `WUMPA_ACTIVITY_SOCKET` and a Wumpa session identifier into the individual
  Pi child environment after existing environment preparation. Do not use a shell
  wrapper or mutate the service or tmux server environment.
- Add an explicit integration selector to server configuration, defaulting to
  disabled for backward compatibility. Do not append Pi-specific flags to arbitrary
  agents based only on an executable basename; wrappers need explicit support.
- Retain the source file for the entire agent lifetime, including `/reload` and
  daemon restarts. The runner must own or coordinate cleanup; daemon shutdown
  alone must not delete resources belonging to surviving agents.
- Existing sessions are not retroactively instrumented. They show `Unknown` until
  relaunched with integration enabled. Updates apply to newly launched sessions;
  older sessions keep their original extension version.

## Extension lifecycle and activity semantics

Start sockets, timers, and other long-lived resources in `session_start`, not in
the extension factory. Dispose them idempotently during `session_shutdown`.

On subscription, send a fresh snapshot. Use the current context's `isIdle()`,
`sessionManager.getSessionId()`, and `pi.getSessionName()` to obtain data. Refresh
context on `/new`, `/resume`, `/fork`, `/clone`, and `/reload`; do not retain a
context belonging to a replaced Pi session.

Event handling:

| Boundary | Action |
| --- | --- |
| `agent_start` | Publish working state. |
| `agent_settled` | Re-read current state and publish settled/idle state. |
| `session_info_changed` | Publish the current name, including clearing it. |
| Session replacement/start | Publish new Pi session identity and name. |
| Compaction/summary boundaries | Refresh activity where supported by the verified Pi API. |
| Shutdown/reload | Close connections and listener; Wumpa marks activity unknown until reconnection. |

`agent_end` and `turn_end` are not completion signals: retries, tools, recovery,
compaction, or queued follow-ups may continue. Verify handler timing against the
supported Pi version so a callback cannot accidentally publish stale state.

Add a low-frequency reconciliation timer and heartbeat (proposed: every 5 seconds)
for operations not completely covered by extension events and liveness. Coalesce
unchanged state except for heartbeats. Do not await slow subscribers in Pi lifecycle
handlers: bounded writes/queues must disconnect slow clients instead of delaying
the model, tools, or terminal UI.

Initial activity states are `Working`, `WaitingForInput`, and `Unknown`, separate
from Wumpa's existing process lifecycle (`Starting`, `Running`, etc.). Here waiting
means no active session-level agent work; it is not proof that every arbitrary
extension command or user shell command is inactive. Validate these limitations
and document them. A distinct `NeedsApproval` state is deferred unless supported
by authoritative interaction hooks; a dialog can block an otherwise active run.

## Local protocol

Use versioned JSONL with LF framing, one bounded record per line. Split on LF,
not Unicode line/paragraph separators. Proposed maximum frame size: 8 KiB.

Client subscribes:

```json
{"type":"subscribe","version":1,"wumpa_session_id":"..."}
```

Initial and subsequent full snapshots:

```json
{"type":"status","version":1,"generation":"...","sequence":1,"wumpa_session_id":"...","pi_session_id":"...","pi_session_name":"Fix authentication","activity":"working"}
```

- `generation` identifies a new extension runtime; `sequence` orders updates
  within it. Full snapshots avoid reconstructing missed deltas.
- Name is nullable, bounded, and escaped for terminal display. Never use it as a
  tmux target, filesystem path, or session identity.
- Heartbeats carry a complete current snapshot; Wumpa records its own monotonic
  receipt time rather than trusting a remote timestamp.
- No prompts, transcripts, model credentials, environment dumps, or command
  execution belong in this protocol. It is observational only.
- Reject unsupported protocol versions, mismatched Wumpa IDs, malformed frames,
  oversized buffers, and excess connections. Bound subscription handshake time.

## Endpoint security and recovery

- Validate private directory identity, socket type/ownership/mode, and Unix path
  length before use. Bind with restrictive permissions, without changing process-
  global umask. Rust verifies peer UID using existing local-control patterns.
- Same-user processes remain within the trust boundary; a reported session ID
  alone is not authentication against a malicious process with the same UID.
- Never unlink arbitrary occupied paths. Handle stale sockets with ownership and
  identity checks comparable to the existing control socket cleanup. Shutdown
  must not remove another listener that replaced the path.
- Preserve endpoint/integration-version association in local recovery metadata;
  extend older records compatibly and keep paths out of remote responses.
- A Rust subscription worker reconnects with bounded backoff and sends a new
  subscription after startup, daemon restart, extension reload, or connection loss.
- Disconnect or missed heartbeat deadline (proposed: 15 seconds) makes activity
  `Unknown`; old names may remain explicitly cached. Never infer idle from silence.
- Runner lifecycle remains authoritative for process exit and descendant cleanup.
  Clean up activity resources only after verified agent termination, not simply
  after a failed discovery attempt.

## Server cache and dashboard

- Keep activity subscriptions outside the manager's reconciliation mutex and
  repository request budget. Bound worker count, I/O, buffers, and reconnect work.
- Cache records keyed by stable Wumpa session ID, with Pi identity, optional name,
  activity, extension generation/sequence, and freshness.
- Add backward-compatible optional fields to remote-safe session summaries.
  Old clients must remain usable; older servers yield unknown activity.
- Render agent rows in the requested form:

  ```text
  🤖 Fix authentication · 326bc28613ffc9df3477946c60b9b462 · Running
  ```

  The name is Pi's current session name, initially seeded from the Wumpa agent
  session label for a new conversation. The full ID is the stable Wumpa session
  ID, not Pi's conversation ID. Map activity `Working` to display text `Running`,
  `WaitingForInput` to `Waiting for input`, and unavailable activity to `Unknown`;
  retain explicit lifecycle states for starting/stopping/cleanup failures.
- Reflect `/name` changes in the same row as soon as the update reaches the client.
  Retain Wumpa's original label as fallback for unnamed/unsupported sessions.
  Keep a known Pi name during transient disconnects while showing activity as
  `Unknown`; clear/replace it when a verified Pi session switch or name-clear
  update arrives. Names never rename tmux targets or alter attachment/selection
  identity.
- Name newly created tmux sessions from the stable Wumpa session ID only:
  `wumpa-<full-wumpa-session-id>`. Do not include the Wumpa label or Pi display
  name. This keeps the attachment target independent of `/name` changes and
  allows duplicate display names. Preserve existing tmux session targets during
  recovery; do not rename live sessions solely to migrate this convention.
- Push is real-time from Pi to the server. Existing client refresh intervals can
  still delay dashboard updates: inspect the current transport and either add
  bounded update delivery or schedule prompt refreshes. Do not claim end-to-end
  real-time delivery until the client path is tested.

## Implementation steps

1. Verify the supported Pi API/version and finalize config, lifecycle, and wire
   contracts. Re-read Pi extension docs and linked examples before implementation.
2. Build and test the bundled extension independently with interactive Pi.
3. Add private versioned materialization, explicit Pi launch integration, per-child
   environment injection, runner-owned lifetime, and compatible recovery metadata.
4. Implement bounded Rust subscriptions, reconnection, freshness, and cache updates.
5. Expose remote-safe summaries and display activity/current Pi name without
   changing SSH/tmux attachment behavior.
6. Document opt-in configuration, supported Pi versions, status limitations,
   troubleshooting, and upgrade behavior. No separate installation instructions.

## Validation and acceptance

- Unit-test framing, version rejection, size limits, ordering, name escaping,
  freshness, generation changes, and private path validation.
- Restore and extend the prototype's two-agent environment/path test, including
  caller spoofing, two instances, and no environment leakage into tmux/service.
- Test bundled source creation races, symlinks, permissions, occupied endpoints,
  excessive path length, retained old versions, and runner-owned cleanup.
- Test subscription/reconnect against a socket double, slow subscribers, malformed
  peers, heartbeat expiry, rapid updates, and daemon responsiveness.
- Real interactive Pi tests: initial idle, prompt start, tools, retries, compaction,
  queued follow-ups, cancellation, `/name`/clearing names, `/new`, `/resume`,
  `/reload`, abrupt exit, and server restart while Pi survives.
- Test initial Pi name equals the chosen Wumpa label, `/name` updates the row,
  the full Wumpa ID remains unchanged, resumed named conversations are preserved,
  explicit Pi naming arguments have documented precedence, and reload does not
  reset a user-selected name. Verify new tmux targets use only the Wumpa ID,
  duplicate Pi names remain attachable independently, and legacy tmux names
  survive recovery without renaming.
- Confirm ordinary user extensions still load and existing attachment/terminal
  restoration tests pass. No RPC mode, altered prompts, or manual setup.
- Test old recovery records, older clients/servers, disabled integration, and
  non-Pi agents. Keep unsupported macOS server execution behavior unchanged.
- Run formatting, Clippy, Rust tests, supported Pi TypeScript checks, and Rust 1.85
  compatibility validation. Use `nix develop` where available; report the actual
  toolchain/environment when the checkout lacks a flake.

Acceptance: a user installs only Wumpa, launches explicitly configured Pi through
its existing agent flow, interacts normally, and sees activity/name updates with
bounded latency. Reporting failure becomes unknown status, never broken Pi.

## Investigation references

- [Pi extension documentation](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md)
- [Pi issue #9626: state endpoint outside RPC mode](https://github.com/earendil-works/pi/issues/9626)
  is a related request, automatically closed without a maintainer response at
  investigation time; it is not an accepted built-in feature.
- [Agent Deck](https://github.com/asheshgoplani/agent-deck) uses a Pi extension for
  event-driven detection, with pane inspection as a less reliable fallback.
- [Claude Squad tmux monitor](https://github.com/smtg-ai/claude-squad/blob/main/session/tmux/tmux.go)
  compares pane content and recognizes agent-specific prompts.
- [cmux](https://github.com/manaflow-ai/cmux) receives terminal notifications and
  notifications emitted by agent hooks.
