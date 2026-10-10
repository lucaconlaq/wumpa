# Real-time Pi activity and session names

## Status

Implementation complete (steps 1–6), with the acceptance limitations below.
The bundled [`agent-extensions/pi.ts`](../../agent-extensions/pi.ts) and
[`integration documentation`](../../agent-extensions/README.md) cover Pi 1.0.4,
private publication, naming precedence, opt-in configuration, upgrades, and
troubleshooting. No deployment or release changes are included.

Wumpa embeds/materializes runner-owned private source copies, injects literal Pi
flags and authoritative per-child environment, and preserves source through reloads
and daemon restarts. Explicit names/existing conversation selections take
precedence. Materialization/reporting failures never prevent ordinary launch.

`src/agent_activity.rs` implements authenticated bounded subscriptions, LF framing,
strict version/ID/generation/sequence validation, replay rejection, reconnects,
15-second monotonic freshness, and cached-name semantics outside manager/Git locks.
Verified runner completion retires missing recovery identities; discovery failures
invalidate freshness without discarding them. Worker replacements retire before
spawning, keeping the 64-worker bound true during descriptor changes.

Remote-safe optional summaries expose current Pi identity/name and activity.
Dashboard/plain rows use current names, unique short Wumpa ID prefixes, and Running/Waiting
for input/Unknown labels while preserving explicit process lifecycle states.
Controls/bidi/separators are escaped. A capability-gated, cache-only versioned
refresh updates the open dashboard about once a second plus transport time, without
Git, form/details/status resets, or attachment identity changes. Old clients,
servers, recovery records, disabled integration, and non-Pi agents remain usable.

Validation includes 198 Rust tests (latest serialized run), 18 Node tests,
formatting, strict Clippy, Rust 1.85 Linux/Apple checks, and strict TypeScript 5.8.3
checking against Pi 1.0.4 declarations. Real interactive Pi tests cover resource
discovery, initial/changed names, reload/new/clone, and prompts/tools/retry/queued
follow-up/cancellation/manual compaction against a deterministic loopback model
double. The binary-to-Pi test verifies embedding, restart survival, remote status
projection, stable IDs, name-clear on new conversation, and rename-to-server-cache
latency below three seconds. A TCP-driven TUI test verifies cache-to-row updates,
selection/details preservation, escaping, stale/failed refreshes, and old-server
capability gating. Display-prefix tests cover cross-checkout collisions, minimal
lengths, near-identical full IDs, reordering, and unchanged full-ID attachment.
Socket doubles cover deadlines, actual heartbeat expiry,
malformed/rate-limited peers, replay/reconnect, replacement/security, and completion.

Unverified acceptance checks: actual external model-provider behavior; persisted
named resume and interactive resume/fork selectors; branch summaries and approval
dialogs; abrupt real-Pi exit; and combined Pi-to-dashboard latency on a real SSH
connection. Native macOS runtime tests were not run; agent execution remains
unsupported there. These limits are not claimed as verified by socket/model doubles.

Rust 1.99.0, Node 24.21.0, a Nix GCC wrapper, and an isolated temporary TypeScript
compiler cache were used; project dependencies are unchanged. `nix develop` is
unavailable because this checkout has no flake. Integration startup/reload is silent;
existing live agents retain their original source until relaunched.

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
  Leave an unnamed resumed conversation unnamed. The extension never initializes
  names and never resets them on `/reload`, `/name`, or later session switches.
  Pi's name becomes authoritative after launch initialization.
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
  🤖 Fix authentication · 326bc · Running
  ```

  The name is Pi's current session name, initially seeded from the Wumpa agent
  session label for a new conversation. Display the shortest server-wide unique
  prefix of the stable Wumpa session ID, starting at five hex characters, not Pi's
  conversation ID. Prefixes may expand/shrink as peers change. Keep full IDs in
  metadata, selection, attachment, and recovery; never resolve control targets
  from a display prefix. Map activity `Working` to display text `Running`,
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

The extension-side contract is now documented in `agent-extensions/README.md`, including
exact bounds, exclusive socket publication, remaining stale-resource cleanup,
configuration/name precedence, and tested API timing. Embedding, private source
publication, opt-in launch arguments/environment, runner lifetime, and compatible
local recovery association are implemented. Unit/launch tests cover permissions,
concurrent creation, symlinks, distinct agents/instances, caller spoofing, naming
precedence, long-path fallback, daemon restart retention, and verified cleanup.
Bounded subscriptions/cache, remote-safe summaries, dashboard/plain rendering,
capability-gated cache-only refresh, and troubleshooting are also implemented.
An optional real Pi binary-launch test covers initial name, `/name`, `/reload`,
`/new`, normal resource discovery, daemon restart while Pi survives, remote cache
updates and stable IDs, and source cleanup. The loopback-model PTY test covers
actual Pi session-level workflows without external credentials.

All implementation steps are complete; unverified acceptance checks remain listed
in Status above.

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
