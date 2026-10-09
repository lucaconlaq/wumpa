# Checkout agent sessions

## Status
Linux end-to-end implementation integrated. No deployment or release-pin change.
Full cross-platform completion remains blocked on verified macOS descendant
containment and runtime validation; macOS creation/discovery is explicitly
unsupported rather than using an unsafe process-group-only fallback.

| Step | Implementation status |
| --- | --- |
| 1. Unix control socket | Implemented, including safe internal runner socket cleanup. |
| 2. Checkout preflight | Implemented; read-only observations alone remain non-durable. |
| 3. Contracts/settings | Implemented local dispatch, durable non-secret outcomes, byte-safe environments, retry reconciliation, and deadlines. |
| 4. tmux lifecycle | Implemented on Linux: dedicated server, pinned identities, subreaper supervision, exit/removal cleanup, and managed-removal callback. |
| 5. Agent CLI | Implemented selection/new/cancel, direct attach/switch, nesting refusal, and explicit uncertain-creation retry keys. |
| 6. Snapshots | Implemented optional remote-safe cached summaries, unsupported/error states, and bounded discovery. |
| 7. Display | Implemented nested agents/worktrees in TUI/plain lists, stable agent selection, and checkout actions. |
| 8. Validation | Linux automated tests, strict Clippy, formatting, Rust 1.85 validation, and Apple cross-compilation; macOS runtime remains unverified. |
| 9. Docs/deployment | Usage/deployment configuration updated; NixOS module syntax checked, not deployed or service-runtime tested. |

### Validation

- Review findings in `issues.md` are addressed, including whole-session cleanup,
  nonblocking unsafe-record rejection, fail-closed managed removal, fresh response
  snapshots, pinned-root relative execution, and explicit dashboard discovery states.
- `cargo +1.85.0 test --locked`: 150 tests pass, including 12 isolated Linux tmux
  integration tests and managed-removal/runtime-identity unit tests.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `git diff --check` pass.
- `cargo +1.85.0 check --locked --target aarch64-apple-darwin` passes without
  warnings. This is cross-compilation only, not macOS runtime validation.
- `nix-instantiate --parse tnt/nixos/server.nix` passes; no deployment performed.
- `nix develop` is unavailable because this checkout has no `flake.nix`; validation
  used installed Rust toolchains with the Nix GCC wrapper on PATH.

### Historical pre-runtime slices

The following records describe earlier slices, not the current implementation.

- `src/session_environment.rs` adds standard padded Base64 name/value entries,
  arbitrary Unix-byte preservation, secret-safe validation and redacted Debug,
  256 KiB compact encoded payload and 4,096-variable limits, bookkeeping filtering,
  and original-caller-directory PATH adjustment. Duplicate names are rejected.
- Executable lookup uses only the adjusted caller PATH or an absolute configured
  executable. Missing PATH never falls back to daemon settings. Relative configured
  paths containing `/` fail closed until their base-directory policy is agreed.
- A preparation helper builds fresh Commands from server-owned literal arguments,
  clearing inherited environment without process-global mutation. The tmux backend
  must still supply fresh terminal settings and implement safe per-agent delivery;
  no tmux server environment is mutated by this slice.
- Create operation types now require the bounded environment; retry, list, attach,
  session metadata, and recovery records reject environment fields. Neither control
  listener dispatches session operations yet; there is still no public agent CLI.
- Linux formatting, strict Clippy, and all 127 tests pass with Rust 1.99 and
  Nix-provided GCC/Git. `nix develop` is unavailable because this checkout has no
  flake; Rust 1.85 and macOS runtime validation remain outstanding. Added `base64`
  0.22.1 (declared MSRV 1.48) instead of implementing a custom codec.

### Previous session contract slice

Step 3 session model and retry contract implemented as internal types.

- `src/sessions.rs` defines validated Wumpa session/creation IDs, checkout
  associations independent of caller subdirectories, labels, live lifecycle states,
  separate local tmux attachment details, local operation/result types, and typed
  failures. These types are not dispatched by either listener.
- Added current-run envelope validation and pure retry resolution: verified
  retained/recovered records yield the original session ID or failure; missing
  evidence returns `outcome_unknown`, never permission to launch.
- Retry/recovery policy agreed below. This is a contract slice, not a running
  deduplication store or restart recovery implementation. Create metadata is not a
  complete wire request until the environment payload contract is agreed.
- Linux formatting, strict Clippy, and all 118 tests pass with Rust 1.99 and
  Nix-provided GCC/Git. No flake is available for `nix develop`; Rust 1.85 and macOS
  runtime validation remain outstanding.
- Still pending in step 3: bounded launch environment and PATH rules, actual local
  dispatch/ownership revalidation, and retry record storage/reconciliation.
  Durable checkout lifecycle identity remains a prerequisite for launching.

### Previous configuration slice

- Added validated `agent_command` argument arrays, defaulting to `["pi"]` for
  missing/legacy configurations. Empty arrays/executables, non-string arguments,
  and NUL bytes are rejected; literal arguments and empty non-executable arguments
  round-trip without shell interpretation.
- No session operations, launch, environment delivery, tmux, or public agent CLI
  added. The remainder of step 3 is pending, including retry retention/recovery
  contracts and the open lifecycle/environment decisions below.
- Read-only checkout preflight is implemented; its review gate remains applicable.
- Configuration-slice Linux validation: formatting, strict Clippy, and all 112
  tests pass using installed Rust 1.99 and Nix-provided GCC/Git. `nix develop`
  remains unavailable because this checkout has no flake; Rust 1.85 and macOS
  runtime validation remain outstanding.

### Previous preflight slice

- Preflight uses the daemon's live in-memory registration snapshot, bounded isolated
  Git discovery, and mandatory matching canonical paths/device/inode observations
  for the caller directory, root, Git directory, and shared Git directory.
- Unavailable observations fail closed; access/discovery errors remain unknown,
  never confirmed removal. Observations are not durable lifecycle identities.
- No public agent command, launch settings, session lifecycle, or tmux changes.
  Stop for review before the session-contract slice.
- Current Linux validation: formatting, strict Clippy, and all 110 tests pass.
  Nix development shell unavailable (no flake); used installed Rust 1.99 and
  Nix-provided GCC/Git. Rust 1.85 and macOS runtime checks remain unavailable.

- The previous slice added the isolated Unix listener and internal handshake
  client in `src/control.rs`. `serve --socket` is required in foreground and detached
  mode. That slice excluded checkout preflight, public agent commands, configuration
  settings, and tmux.
- Review fixes: directory-FD-anchored quarantine/identity validation protects
  concurrent endpoint replacements; exclusive restoration never clobbers newer
  entries. Unexpected entries are preserved with diagnostics if restoration fails.
  Accepted Unix streams are restored to blocking mode. Both listeners use bounded
  resource-pressure retries, and fatal control-worker failures reach daemon supervision.
  Non-UTF-8 canonical socket paths are rejected before creating runtime files.
- Linux validation: `cargo fmt --check`, `cargo clippy --all-targets`, and all 108
  tests pass, including socket lifecycle, bounded framing/deadlines, aliases,
  independent instances, restart identity, TCP rejection, and config immutability.
  Ownership mismatch is unit-tested; live peers are verified with OS credentials.
- This checkout has no Nix flake, so `nix develop` was unavailable. Validation used
  installed Rust/Cargo 1.99 with Nix-provided GCC and Git. macOS runtime and
  Rust 1.85 validation remain outstanding; no dependencies or toolchain pins changed.
- The direct-config resolver implementation was reverted. Prior test results
  applied only to that prototype; session architecture slices remain unimplemented.
- Work in small, independently tested slices, stopping for review after each.
  Start with the local Unix control socket and handshake, then checkout preflight;
  defer public agent commands and session launch until their prerequisites are ready.

## Goal
Run `wumpa agent` on the daemon's machine, inside a registered main checkout or
linked worktree, including subdirectories. The CLI contacts the selected daemon
through a Unix socket; the daemon validates the checkout and creates or discovers
coding-agent sessions, defaulting to `pi`. The CLI attaches its terminal directly
to the selected session. Remote dashboards can see sessions but cannot create or
attach to them in this scope.

## Architecture decisions
- The daemon owns configuration, checkout validation, session creation, and
  discovery. The agent CLI never reads, saves, or migrates server configuration.
- A local Unix socket carries control messages, not terminal input/output. Existing
  SSH/TCP connections continue serving remote repository and session snapshots.
  Agent session-control operations are accepted only on the Unix listener.
- Both `wumpa serve` and `wumpa agent` require `--socket /absolute/path`.
  There is no default discovery or `--instance` option. The socket path identifies
  the instance across restarts; a different path identifies a different instance.
  TCP port selection remains separate and is not an agent-command option. Never
  fall back to TCP or direct config access.
- Initially support the same OS user and shared filesystem view. Use a private
  user-owned socket directory, mandatory same-user ownership/peer checks, and filesystem
  cross-checks to catch accidental context mismatches. Do not add machine-ID
  fingerprints. Unix sockets can be deliberately forwarded or namespace-shared;
  this design does not claim proof of locality or identical filesystem namespaces.
- Each daemon instance owns an isolated, dedicated Wumpa tmux server, not the user's
  personal/default server. Instances registering the same checkout do not share
  sessions; this does not isolate their edits to checkout files. Restarting the same
  instance rediscovers its sessions. The CLI attaches directly using daemon-provided
  attachment information. When invoked inside another tmux server, require the user
  to detach and rerun; do not nest tmux. Switching is supported within the same server.
- Remove sessions when their agent exits; do not retain exited output in v1.
- New agents receive the invoking CLI's environment, including mise-activated PATH
  and exported credentials, not the daemon's service or stale tmux environment.
  The daemon supplies the configured command and validated checkout root. Filter
  incorrect terminal/tmux bookkeeping variables; do not rerun shell initialization.
  Reattachment never changes an existing agent's environment. Do not log, persist
  in session metadata, or include launch environments in remote snapshots.
- Sessions must not outlive their checkout/worktree. Wumpa-managed deletion warns
  about running agents and stops owned sessions before removal; stopping failure
  aborts deletion. Confirmed external removal/replacement triggers termination on
  detection, not reassociation with a replacement checkout. Access/discovery errors
  alone must never trigger termination. Treat a checkout/worktree move as removal
  of the old location: stop its sessions and require fresh agents at the new location.
  Force-stop agents immediately, without a graceful shutdown interval, including
  their owned child processes. Confirm termination before Wumpa-managed removal;
  report failures and abort removal. Other instances reconcile independently.
- Sessions have backend-independent Wumpa IDs, checkout associations, labels, and
  lifecycle states. Keep tmux names and socket paths in backend/attachment details,
  separate from checkout validation and session identity.
- Future isolated execution, such as Firecracker, can replace the execution and
  attachment backend. No VM implementation or generic plugin framework now.
  Agents run in the daemon's execution environment or one explicitly managed by it;
  future isolation will require filesystem, credentials, and terminal-bridge design.

## Affected areas
- `src/main.rs`: required socket path and eventual local `wumpa agent` command.
- `src/config.rs`: backward-compatible server agent command configuration.
- New session modules: session model, daemon orchestration, tmux backend, and local
  selection/attachment. Keep separate from SSH authentication in `src/agent.rs`.
- `src/protocol.rs`, `src/server.rs`, `src/transport.rs`: bounded Unix control
  transport, versioned handshake/preflight/session operations, and remote snapshots.
- `src/worktrees.rs`: reuse bounded Git discovery for daemon-side validation.
- `src/tui.rs`, `src/tui/view.rs`, `src/client.rs`: nested agent display.
- `tnt/nixos/server.nix`: tmux PATH, runtime socket setup, and session survival.
- Usage documentation and focused unit/integration tests.

## Agreed Unix socket policy
- On Linux and macOS, create the socket at the required absolute `--socket` path
  when the daemon starts. Reject unsafe existing directories; do not discover a
  default location or fall back to another path.
- Resolve path aliases, including symlinks and `..`, to one canonical socket path;
  allow aliases rather than rejecting them. Resolve the containing directory before
  creating the socket, which does not yet exist. Ownership/access checks and safe
  cleanup still apply; never unlink a symlink at the supplied endpoint.
- The canonical socket path is the stable instance identity, independent of TCP
  ports. Restarting at the same path keeps that identity; changing paths does not.
- Generate a fresh random daemon-run ID at each startup. The handshake returns
  control protocol version `1`, the canonical socket path, and the daemon-run ID.
  Reject incompatible protocol versions.
- Use newline-delimited JSON with a maximum of 1 MiB per message, including the
  newline, matching existing protocol framing and size limits. Each connection
  carries one request/response and then closes.
- Allow 2 seconds to connect, then 5 seconds total for the handshake. Enforce an
  overall exchange deadline rather than resetting it when bytes arrive; bound
  server-side reads/writes as well. Later operations define their own deadlines.
- Future operations include the expected daemon-run ID. Reject mismatches before
  any action; clients repeat handshake and validation rather than reuse results
  from the previous daemon run. Do not automatically retry state-changing operations.
- Require a same-user private socket directory with mode `0700`, a socket with
  mode `0600`, and same-user peer credential verification on Linux/macOS. Fail
  closed when peer verification fails or is unavailable.
- Serialize startup with a per-endpoint lock. Automatically recover only verified
  same-user stale sockets after connection refusal and socket identity rechecking.
  Never remove an active endpoint, symlink, or ordinary file; a timeout does not
  establish staleness. Normal shutdown removes only the daemon's own socket.

## Agreed session/retry contract

- Session IDs are daemon-generated; creation request IDs are caller-generated.
  Both use opaque 128-bit IDs encoded as 32 lowercase hexadecimal characters.
  ID generation and backend collision checking belong to the launch implementation.
- Local list/create/attach requests require the current run ID, checked before any
  backend action. Each operation carries mandatory checkout observations; daemon
  registration/membership and owned-session association must be revalidated.
  Session checkout association excludes the caller subdirectory and agent cwd.
- Session metadata and local attachment details are distinct. Labels are selected
  by the daemon for display, not accepted as commands. Starting/running/stopping
  and cleanup-failed states describe live or incompletely cleaned-up sessions;
  successful agent exit removes the session, without retaining terminal output.
- Reserve a creation key scoped to the canonical instance and originating daemon
  run before any launch side effect. Serialize concurrent attempts on that key;
  duplicates return the original outcome or creation-in-progress, never launch.
  Reject reuse for a different checkout. Command/environment changes must not
  affect an already accepted attempt.
- Retain accepted outcomes for the originating daemon run, even when the session
  exits. Never evict a record and then treat its key as new in that run. Storage
  capacity and rejection-before-reservation limits must be set before launch.
- Following a lost response, explicitly retry with the same key. A restarted
  daemon requires a new handshake and preflight; retry carries the original run ID
  as well as the expected current run ID. Retry is lookup/reconciliation only,
  never a new creation request. Do not automatically retry state-changing requests.
- Reconcile verified owned backend sessions before resolving an attempt after
  restart. Retained/recovered evidence returns the original ID or failure; a
  missing session/record does not prove no launch occurred. Missing, conflicting,
  incomplete, or unavailable evidence fails explicitly without a duplicate launch.
  Returning a previously created ID does not promise it is still attachable:
  attachment separately revalidates ownership, checkout, and running state.
- Recovery records contain only instance/run/request/session identities, checkout
  association, and typed outcome. No launch environment, terminal output, or
  secret-bearing error text is retained. Backend discovery remains authoritative
  for live processes; outcome records are not a parallel process registry.
- Durable checkout identity, concrete backend recovery/storage sequencing, local
  operation deadlines, and environment delivery remain prerequisites to dispatch
  and launch; existing observations alone must not establish confirmed removal.

## Agreed launch environment policy

- Preserve arbitrary Unix bytes in environment names and values using an array
  of objects with standard padded Base64 `name` and `value` strings. Do not require
  UTF-8 or use lossy conversion. Base64 is encoding, not encryption; these entries
  belong only to local create requests. Reject duplicate decoded names.
- Reject NUL bytes, empty names, and names containing `=` without echoing entry
  contents. Values may contain `=` or be empty.
- Limit the compact canonical encoded environment JSON array to 256 KiB
  (262,144 bytes), including Base64, entry fields, commas, and brackets, within
  the existing 1 MiB complete-message limit. JSON whitespace does not count toward
  the canonical environment budget but still counts toward the complete message.
  Recheck the environment budget after PATH adjustment. Reject oversized payloads without
  truncation or entry contents in errors. OS process-launch limits still apply.
- Allow at most 4,096 environment variables, alongside the encoded-size limit.
  Reject excess entries without exposing names or values.
- Filter caller bookkeeping variables: `TMUX`, `TMUX_PANE`, `STY`, `WINDOW`,
  `PWD`, `OLDPWD`, `SHLVL`, `_`, `LINES`, `COLUMNS`, `SSH_TTY`, `TERM`, and
  `TERMCAP`. The backend supplies fresh terminal settings and checkout `PWD`.
  Preserve caller PATH, credentials, `SSH_AUTH_SOCK`, and other variables unless
  another agreed rule applies; do not broadly remove SSH or credential variables.
- Resolve relative PATH entries against the CLI's original validated working
  directory before changing to the checkout root. Empty PATH entries mean that
  original directory too. Use this adjusted PATH for configured-executable lookup
  and the new agent environment, preserving caller lookup semantics.
- If caller PATH is absent, reject a configured bare executable such as `pi`
  with a clear, secret-safe failure. Absolute executable paths still work. Never
  fall back to the daemon's PATH. An explicitly empty PATH follows the empty-entry
  rule above rather than being treated as absent.
- A fresh process Command can receive a cleared, filtered per-process environment
  plus checkout PWD without changing the daemon's global environment. Delivery
  uses a private one-shot Unix channel, not tmux global state. Never log a prepared
  Command: its Debug
  includes environment contents. Environments must never enter logs, recovery
  records, or snapshots.
- If relative/empty PATH entries require prefixing a caller directory containing
  `:`, fail closed: Unix PATH cannot encode that directory without changing lookup
  semantics. Do not silently split it into unrelated search directories.

## Implementation decisions (delegated by user)

- Durable live identity: a persistent per-agent supervisor opens/pins checkout,
  Git/common, and runtime directory descriptors before spawn. Require local
  ext2/3/4, XFS, Btrfs, tmpfs, or OverlayFS directory identities; NFS/FUSE/9p and
  unknown filesystems fail closed before reservation because their server-side
  inode reuse is not prevented by local FDs. Supported directory identities cannot
  be reused while their objects remain pinned. Restart recovery verifies live owned
  tmux membership, a strict non-secret record, and the supervisor's matching pinned
  association; stale disk observations alone never authorize reassociation.
- Confirm removal on successful path-object mismatch or ENOENT at an original
  location; permission/discovery failures remain unknown. Successful isolated Git
  queries additionally catch metadata reassociation. Execute with `fchdir` on the
  pinned root, never accidentally in a replacement directory.
- Linux containment uses a dedicated foreground agent group plus a persistent
  `PR_SET_CHILD_SUBREAPER` supervisor. Immediately SIGKILL owned direct children,
  then repeatedly kill/reap newly adopted descendants, including detached children.
  SIGCHLD is defaulted; the sole supervisor thread signals its captured children
  before reaping, so those PIDs cannot be reused during signalling. Never signal
  potentially stale queried group IDs. Two seconds bounds verification; failed
  cleanup remains supervised. Exit/hangup/termination use the same cleanup path.
  Deliberate SIGKILL/interference by the owning OS user is outside sandbox claims.
- Stable tmux/runtime names derive from the full canonical control endpoint via an
  adjacent `.<endpoint>.sessions` directory. A private adjacent runtime-identity
  marker and pinned descriptor detect replacement/movement. Validate ownership,
  modes, instance tag, strict session ID/name, and recovery tag; never use the
  personal tmux server or configuration. Require tmux >= 3.2 for direct argument
  vectors, and accessible procfs; too-long derived socket paths fail explicitly.
- Creation reserves a synced atomic record (including parent-directory fsync)
  before launch, tags the owned backend before delivering an environment, and
  records readiness only after runner acknowledgement. Lost/incomplete outcomes
  reconcile against the owned runner; missing evidence is unknown, not another
  launch. Capacity is 4,096 records and 64 live sessions per instance: reject before
  launch; never automatically evict/forget an accepted key. Administrative archival
  of old-run outcomes requires daemon shutdown and confirmed agent termination;
  missing archived retries stay unknown.
- Secrets travel over a private, same-user one-shot Unix launch channel, never
  argv, disk, tmux global environment, or remote metadata. Runner builds a fresh
  cleared Command and supplies freshly allocated pane bookkeeping. Configured
  relative executable paths containing `/` use the validated checkout root;
  relative/empty caller PATH entries still use the original caller directory.
- Session control has a 20-second overall exchange budget; reads initially share
  the existing five-second framing budget. Runner IPC is at most three seconds
  within the caller's remaining budget. Cached refresh is about 500 ms with a
  two-second budget; supervisors independently poll paths every 100 ms and perform
  bounded Git checks about once per second. Remote summaries have a 128 KiB budget
  and exclude local instance/attachment and environment details.
- Managed deletion integration is `Manager::with_checkout_removal`: hold the same
  instance mutex through stop/verification and replacement-safe removal callback.
  Unverified termination aborts removal. No current checkout deletion command
  exists to wire; none was introduced for this feature. Other instances reconcile
  independently.
- Service policy preserves dedicated tmux/supervisors across daemon restart/stop
  (`KillMode=process`, persistent private runtime directory). NixOS configuration
  was syntax-checked only; deployment and the pinned binary release are unchanged.

### Remaining completion gates

- macOS needs verified descendant containment and runtime tests before enabling
  execution. Unix socket/control and repository browsing remain supported;
  snapshots explicitly report unsupported agent execution there.
- Actual NixOS service restart/stop verification and a feature-capable release need
  a separately authorized deployment/release task.
- Real-agent interactive acceptance is not automated: tests deliberately use
  harmless isolated commands, not Pi or developer sessions.

## Implementation review scope
Review the integrated Linux path in `src/session_runtime.rs`, `src/session_cli.rs`,
`src/control.rs`, environment/contracts, remote snapshots, and display. Tests use
private tmux sockets and harmless commands, never real Pi or developer sessions.
Focus on restart recovery, pinned-object identity, descendant cleanup/confirmation,
secret boundaries, and fail-closed unknown outcomes. The user delegated remaining
implementation choices; historical per-slice review stops no longer gate this
integration.

### Previous socket/handshake review criteria
The following criteria are retained for reference.

- Follow the agreed Unix socket policy, protocol limits, and daemon-run ID binding.
  Implement race-safe lock, socket identity-check, and cleanup mechanics.
- Add required daemon socket-path selection and an internal local client helper using
  bounded framing and deadlines. Keep existing TCP behavior compatible.
- Return the canonical socket path, fresh daemon-run ID, and control protocol
  version without repository snapshots, Git discovery, or tmux calls.
- Done when isolated tests cover required absolute socket paths, alias resolution,
  two independent daemons, stable path identity and fresh run IDs after restart,
  missing/incompatible endpoints, ownership failures, malformed/oversized messages,
  total deadlines including slow incoming bytes, one-exchange connections, socket
  collisions, and safe cleanup without config mutation.
- Not included: checkout preflight, agent settings, public `wumpa agent`, tmux,
  session discovery, or UI changes. Stop for review after this slice.

## Implementation
1. Add the local Unix control listener and handshake.
   - Require an explicit absolute socket path in a private user-owned directory.
     Validate directory/socket ownership and access.
   - Do not unlink an active daemon's socket or arbitrary files. Define cleanup on
     shutdown and stale-endpoint recovery, including concurrent startup.
   - Reuse bounded message framing and transport patterns, not the current server
     pipeline that appends repository snapshots and Git discovery to every response.
   - Enforce the Unix-only session-control boundary in server dispatch, including
     requests delivered through SSH-forwarded TCP. Keep remote listing available.
   - Return the agreed version, canonical path, and daemon-run ID in the handshake.
     Future operations carry the expected run ID and reject mismatches before acting.
     Report absent/incompatible daemons.
2. Add read-only daemon-side checkout preflight.
   - Send the invoking process's absolute current directory and agreed filesystem
     observations through the local control channel; reject detected view mismatches.
   - Validate against a snapshot of the daemon's in-memory registered repositories,
     never a client-supplied config path. Do not mutate or persist configuration.
   - Resolve the actual Git checkout root, not merely a matching directory prefix.
     Match canonical paths against Git worktree membership and shared Git metadata;
     reject stale worktree paths reused by independent repositories.
   - Accept main/linked checkouts, subdirectories, and symlinks. Reject unregistered
     or nested independent repositories, bare/prunable worktrees, missing paths,
     non-checkout directories, and relative registered checkout paths.
   - Clear inherited Git overrides, bound runtime/output, and release the config
     mutex before querying Git. Return canonical checkout and identity information.
   - Test without tmux or a partial public agent command, including malformed input,
     special-character paths, config immutability, and different daemon registrations.
3. Define the session contract and server launch settings.
   - Add `agent_command` as a nonempty argument array, defaulting to `["pi"]` when
     absent. Reject empty executables and invalid arguments; preserve old configs.
   - The daemon selects and executes the configured argument vector; do not accept
     arbitrary CLI shell commands or interpret a configurable shell command string.
   - Define Wumpa session IDs, checkout association, labels, lifecycle states, and
     separate backend attachment details. Keep tmux behind a focused backend boundary.
   - Define local list/create/attachment operations and typed failures. Revalidate
     checkout and session ownership before launch or returning an attachment target.
   - Make creation retry-safe using request IDs with explicit retention/recovery
     semantics; a lost response must not silently create duplicate agents on retry.
   - Scope ownership to the selected daemon instance. Resolve remaining checkout
     identity and lifecycle details before launch. Command/environment changes apply
     only to newly created sessions; resume never silently restarts an existing agent.
   - Carry the caller's bounded launch environment only over local control transport.
     Resolve the configured executable using the caller's PATH, preserving activated
     mise behavior without requiring mise or shell initialization in the service.
     Reject invalid/oversized payloads without echoing secret values.
4. Implement daemon-managed tmux lifecycle.
   - Create/use the instance's dedicated Wumpa tmux server and launch from the
     validated root with collision-resistant session names. Explicitly deliver the
     filtered caller environment per agent, without leaking variables between sessions
     or inheriting stale/default tmux variables; do not mutate daemon-wide environment.
   - Tag sessions with Wumpa identity, checkout identity, and non-secret agent metadata.
     Preserve paths and arguments containing spaces or punctuation safely.
   - Define creation/tagging/launch sequencing and cleanup of incomplete creation.
     Track the agent pane/process, not merely whether a tmux session still exists;
     remove the session when the agent exits, even if tmux retains dead panes or
     unrelated panes remain alive. Do not retain output for later inspection in v1.
   - Preserve sessions on CLI disconnect, detach, or attach failure. Recover live
     sessions after daemon restart without a parallel authoritative process registry.
   - Ensure service restart/stop behavior does not unintentionally kill the dedicated
     tmux server; verify this explicitly in the deployment lifecycle.
   - Reconcile checkout identity independently of dashboard requests. Terminate owned
     sessions after confirmed removal/replacement/move; never kill on transient discovery
     or permission failures. Prevent launch/delete races and reassociation to a new
     checkout at an old path. Force-stop immediately without a grace period, including
     owned child processes; bound completion checks and report cleanup failures.
   - Provide a stop-before-remove integration for Wumpa-managed checkout deletion:
     warn about running sessions and abort removal if termination fails. Do not treat
     stopping as detach. Other daemon instances detect the removal independently.
   - Report missing tmux/agent executables and bounded subprocess failures clearly.
5. Add the public local `wumpa agent` flow.
   - Require `--socket /absolute/path`; perform handshake and preflight.
   - With no matching live sessions, request creation and attach. Otherwise show a
     numbered list plus create-new/cancel choices; allow multiple agents per checkout.
   - Request attachment by Wumpa session ID. The daemon returns the initial tmux
     socket/session target; terminal traffic bypasses the Wumpa control connection.
   - Attach outside tmux; switch within the same tmux server. Detect a different
     tmux server before requesting creation, and instruct the user to detach and
     rerun instead of nesting. Restore terminal state before attach.
   - Preflight/launch failures produce nonzero exits. An attachment failure must not
     destroy an otherwise successfully created session.
6. Include live sessions in local and remote snapshots.
   - Discover once per snapshot through the backend, then associate sessions with
     validated checkout identities, not names or the agent's changing directory.
   - Use an overall refresh runtime/output budget compatible with transport deadlines;
     do not hold the config mutex during Git or tmux subprocesses.
   - Distinguish unsupported session discovery, successful empty results, and failures.
     Discovery failures must not prevent repository browsing or appear as empty data.
   - Add backward-compatible optional/defaulted protocol fields. Remote SSH/TCP
     clients see labels, Wumpa IDs, and lifecycle status but cannot create or attach.
     Do not expose credentials, environment variables, or local attachment details
     in remote snapshots. Refresh alongside existing repository refreshes.
7. Render sessions in dashboard and plain-text lists.
   - Main checkout agents appear directly beneath their repository. Linked worktrees
     use 🌲; their agents use 🤖 one level farther down.
   - Show labels and session IDs to distinguish agents. Preserve selection identity
     across refreshes; agent entries must not break checkout actions such as Zed.

   ```text
   repository
   ├ 🤖 pi · session-1
   └ 🌲 feature-worktree
     ├ 🤖 pi · session-2
     └ 🤖 pi · session-3
   ```
8. Add tests alongside every slice and run cross-feature validation.
   - Cover socket permissions/lifecycle, required path selection, TCP control rejection,
     malformed/versioned messages, filesystem mismatches, and read-only preflight.
   - Cover main/linked checkouts, symlinks, nested unrelated repos, missing/replaced
     checkouts, malformed config, and special-character paths/arguments.
   - Test multiple agents, create retries, resume/new/cancel, stale sessions, agent
     exit, missing executables, environment propagation, restart recovery, discovery
     failures/timeouts, remote read-only display, and older protocol decoding.
   - Verify per-instance isolation, caller/mise PATH resolution, absence of environment
     leakage between concurrent launches, secret-safe errors, and unchanged resume
     environments. Cover confirmed deletion/replacement, transient access failures,
     deletion/launch races, termination failure, and cleanup without connected clients.
   - Test checkout moves terminating old sessions, agent exits removing sessions
     without output retention, same-server switching, and different-server refusal
     before creation.
   - Use isolated runtime directories/tmux sockets and harmless test commands; never
     touch developer sessions or launch real Pi in tests.
   - Run formatting, Clippy, and tests; document unavailable runtime validation.
9. Document usage and deployment.
   - Explain required explicit socket paths and path-based instance identity, required local daemon,
     same-user/filesystem assumptions, server-owned commands and caller-provided
     launch environments, mise activation, attachment/detachment, remote read-only
     visibility, checkout-deletion termination, and refresh/reconciliation behavior.
   - Document the chosen multi-daemon policy and tmux-in-tmux behavior. Add tmux to
     the NixOS service PATH and configure runtime sockets and session survival.
   - Firecracker and remote session creation/attachment remain out of scope.
     Do not deploy or bump the pinned CI release without a separate request.
