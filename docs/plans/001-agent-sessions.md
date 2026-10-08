# Checkout agent sessions

## Status
Planned.

- The direct-config resolver implementation was reverted. The architecture below
  is not implemented or validated; prior test results applied only to that prototype.
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

## Open decisions
Resolve each before its dependent slice; do not silently choose during implementation.

- Before preflight: filesystem comparison fields and unavailable-check policy;
  identity checks that distinguish confirmed removal/replacement/moves from access
  failures. Canonical paths and Git common-directory paths alone are not durable
  identities.
- Before launch: stable per-instance tmux socket/ownership rules derived from the
  control socket path; retry recovery and service restart lifecycle.
- Before launch: environment encoding/size limits, exact bookkeeping-variable filter,
  and per-process delivery without global environment mutation or secret logging.
  Define handling of relative PATH entries when launch changes to the checkout root.
- Before cleanup: reconciliation cadence when no dashboard is connected, reliable
  ownership/tracking of agent child processes, and bounded verification of immediate
  force-stop completion.
  Checkout deletion commands are not introduced merely to implement this feature;
  any Wumpa deletion path must honor stop-before-remove when provided.

## Next small step
Implement only the Unix control socket and read-only versioned handshake.

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
