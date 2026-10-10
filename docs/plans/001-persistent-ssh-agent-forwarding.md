# Persistent SSH-agent forwarding

## Status
Planned.

## Goal
New Wumpa agents keep the same `SSH_AUTH_SOCK` across detach/reattach. Attaching
through the dashboard or `ssh -A …` plus `wumpa agent` supplies the current
forwarded agent automatically, without restarting the coding agent. Unavailable
or stalled forwarding must fail within a bounded time rather than hang forever.

This is a focused session-lifecycle change, not a dashboard or daemon rewrite.
Existing running agents cannot have their environment repaired retroactively.

## Affected areas
- `src/tui/ssh.rs`: fresh, agent-forwarded dashboard SSH connections.
- `src/session_cli.rs`: attachment-scoped forwarding registration and cleanup.
- `src/session_runtime.rs`: runner-owned relay lifetime and launch environment.
- New `src/session_forwarding.rs`: private socket relay and attachment leases.
- `src/sessions.rs`, `src/control.rs`: local-only capability discovery and attachment
  setup, using existing peer, checkout, instance, and session validation.
- `src/session_environment.rs`, `src/agent.rs`: environment and endpoint validation.
- `tests/agent_attach.rs`, `tests/agent_sessions.rs`, `tests/tui_ssh.rs`: regressions.
- `README.md`: forwarding requirements, trust implications, and lifecycle behavior.

## Implementation
1. **Specify the forwarding contract and compatibility boundary.**
   - Each newly created agent gets a stable private socket, even when created
     without forwarding. Set its `SSH_AUTH_SOCK` explicitly after preparing the
     launch environment; never inherit a transient sshd socket into the agent.
   - Forward only the attaching caller's explicitly supplied `SSH_AUTH_SOCK`.
     Never search `/tmp`, inspect unrelated processes, or fall back to daemon
     credentials. Do not change the process-global environment or tmux's global
     environment.
   - Forwarding is optional: missing/invalid forwarding warns but does not prevent
     terminal attachment or terminate the coding agent. With no live forwarding
     lease, requests fail immediately. Detached agents remain alive but have no
     guaranteed SSH credentials.
   - Concurrent attachments: newest live valid registration wins. When it ends,
     fall back to the next live registration, if any. An older attachment's
     cleanup must never revoke a newer registration. An attachment without an
     agent does not displace another attachment's valid registration.
   - An in-flight request belongs to its original registration: switching targets
     closes affected streams; never replay a signing request against another key.
   - Use a versioned/advertised local capability. Current envelopes reject unknown
     fields and operations; do not silently extend old requests. Mixed versions
     must retain ordinary attachment and show an upgrade/new-session warning when
     forwarding refresh is unavailable. Do not expose relay paths or credentials
     through TCP snapshots, logs, creation outcome records, or debug output.

2. **Add a runner-owned, bounded Unix-socket relay.**
   - Bind a short per-session path under the existing private runtime directory
     before spawning the agent. Use `0600` sockets and existing replacement-safe
     cleanup; check Unix path-length limits and refuse unsafe existing paths.
   - Keep relay ownership in `agent-runner`, which already outlives daemon restarts
     and supervises the coding agent. Keep forwarding work off the runner's
     lifecycle/stop loop and bound workers, connections, frame sizes, and buffers.
   - Validate registration peers and upstream socket ownership/type. Pin/check
     endpoint identity when connecting to prevent silent path replacement. Reject
     self-targets and relay cycles, including inherited Wumpa relay sockets.
   - Relay SSH-agent protocol frames without logging payloads or caching keys.
     Return protocol failure or close promptly when no upstream exists.
   - Specify separate bounded connect and request deadlines: initially 5 seconds
     to connect/list identities and 120 seconds for signing/user approval. Enforce
     an absolute frame/request deadline so trickle traffic cannot extend it
     indefinitely; allow idle clients without an outstanding request.
   - Lease loss or target replacement closes affected upstream/downstream streams
     immediately. A timed-out upstream must not leave blocking worker threads.
     Ensure cancellation, shutdown, and agent deletion join/terminate relay work.

3. **Register forwarding for the actual attachment lifetime.**
   - After validated session lookup, establish a private authenticated registration
     channel to the owning runner. Keep an unguessable registration identifier and
     a live connection/lease; EOF and helper death revoke only that registration.
   - Ordinary `attach-session`: hold the registration while waiting for tmux;
     detach, SSH loss, failed attachment, signals, and helper termination release it.
     Bind readiness/revocation to the tmux result, not merely successful lookup.
   - Handle the existing `switch-client` path explicitly: that command returns
     immediately, so its process lifetime is not an attachment lease. Track the
     actual tmux client's session/lifetime and move or release its registration
     on switches/detach. Never register an inherited relay as its own upstream.
     Prove this lifecycle with tests before shipping; do not leave a background
     registration that survives an unrelated client indefinitely.
   - Apply the same setup to create-and-attach and retry-and-attach. Detached
     `agent-create --no-attach` creates a stable but unconnected relay; later
     attachment supplies credentials. Do not retain the creation helper's socket
     after the helper exits.
   - Daemon restart must preserve existing runner relays and active leases;
     discovery must distinguish old runners without this capability. No persistent
     forwarded socket paths or registrations are restored from disk.

4. **Enable forwarding on fresh dashboard connections.**
   - Use `ssh -A -t` and disable connection multiplexing with
     `ControlMaster=no` and `ControlPath=none`, following the existing clone
     transport precedent. Preserve host-key checking, host aliases, remote-command
     quoting, and configured SSH ports; the daemon port is not the SSH port.
   - Apply this to dashboard checkout shells and agent attachment so subsequent
     `wumpa agent` creation also receives forwarding. Audit any agent creation
     handoff rather than assuming it uses the same helper.
   - Document that forwarding permits the trusted remote account to request key
     signatures while connected; it does not copy private keys. Server SSH policy
     may disable forwarding, in which case attachment still works with a warning.

5. **Test lifecycle, security, and user-visible behavior.**
   - Fake Unix SSH agents with distinct identity replies: create with A, detach,
     remove A, attach with B; the same coding-agent PID and `SSH_AUTH_SOCK` must
     now list B's identity. Exercise multiple requests on a client connection.
   - Test unavailable, refused, EOF, and deliberately hanging upstreams; bounded
     failure and recovery on the next attachment, including a pending request
     during detach. Use injectable short deadlines in tests, not long sleeps.
   - Test concurrent attachments, stale cleanup, fallback, missing forwarding,
     endpoint replacement, wrong owner/peer, invalid paths, cycles, capacity
     limits, and partial/oversized protocol frames. Never use real private keys.
   - Test helper crash, SSH disconnect, tmux switch/detach, failed attach, daemon
     restart, runner exit, forced agent deletion, and runtime-directory replacement.
   - Cover initial creation without forwarding, detached creation, local CLI use,
     old-client/server/runner combinations, and unchanged macOS unsupported-agent
     behavior. Update SSH command expectations and PTY regression coverage.
   - Linux integration test with real tmux and fake SSH agents; manual end-to-end
     check from macOS to Linux: forwarded SSH creation, detach/close original SSH,
     dashboard reattach, then `ssh-add -l` and an authorized Git operation.
   - Run `cargo fmt --check`, `cargo clippy --all-targets`, and `cargo test` in
     `nix develop`, plus Linux integration coverage. Retain Rust 1.85 support.
     Record unavailable environments and unperformed checks explicitly.

## Acceptance criteria
- Newly created agents use fresh attachment credentials after reconnect without
  agent restart, environment exports, or tmux configuration changes.
- No forwarding request hangs indefinitely; no cross-session credential fallback.
- Detach/reconnect and simultaneous attachments have deterministic, tested behavior.
- Existing sessions remain attachable; unsupported refresh is explained honestly.
