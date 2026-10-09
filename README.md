
# Wumpa
Code on your remote server without juggling SSH sessions. Wumpa lets you manage projects and worktrees, keep agent coding sessions running, and open your code in Zed—all from your terminal.

<p align="center">
  <img src="./.github/logo.png" alt="Grape Cola" width="200" />
</p>

## Run a local daemon

On Linux or macOS, select an explicit absolute control socket path:

```sh
mkdir -p "$HOME/.local/run/wumpa"
chmod 700 "$HOME/.local/run/wumpa"
wumpa serve --socket "$HOME/.local/run/wumpa/control.sock"
# Add --detach to run in the background; --port selects the separate TCP port.
```

The containing directory must already exist, belong to your OS user, and have
mode `0700`. Wumpa creates a `0600` socket and verifies same-user peers. Directory
aliases resolve to a canonical instance path, which must be valid UTF-8;
endpoint symlinks are rejected.
Different socket paths select independent instances (use different TCP ports too).
There is no default socket discovery or TCP fallback for local control.

Ctrl-C or SIGTERM removes only this daemon's socket. A persistent adjacent
`.lock` file serializes startup; do not delete it while a daemon is running.
After an abrupt exit, startup recovers only a verified same-user socket whose
connection is refused. Active endpoints, ordinary files, and symlinks are never
deleted automatically. Cleanup captures a candidate in a private adjacent directory
before verifying its identity. A concurrent replacement is restored without
clobbering a newer endpoint. If restoration conflicts, the entry is preserved under
`.wumpa-control-cleanup-*/endpoint` and its path is reported for manual recovery;
never delete those directories without inspecting their contents.

Both listeners retry transient resource pressure for up to ten seconds with
backoff. Persistent or fatal listener failures stop the daemon with an error rather
than silently leaving local control unavailable.

The local control protocol provides a read-only version-1 handshake/checkout
preflight and local-only session operations: newline-delimited JSON, at most 1 MiB
including the newline, one exchange per connection. Handshake/preflight use a
2-second connection and 5-second exchange deadline; session operations have a
20-second total exchange budget. The handshake returns the canonical socket path and a fresh daemon-run
ID; preflight requires that run ID and validates against the daemon's live
registration snapshot without modifying configuration. Main/linked checkouts,
subdirectories, and aliases are supported. Required canonical paths and device/inode
observations of the caller directory, checkout root, Git directory, and shared Git
directory must agree. Unavailable checks fail preflight but never establish removal.
These observations are not durable identities or proof of identical filesystem
namespaces; deliberate Unix socket forwarding remains possible. TCP/SSH repository
access is unchanged. SSH/TCP snapshots expose agent labels, IDs, checkout paths,
and lifecycle states, never launch environments or local attachment details.
Session-control requests are rejected on TCP, including SSH-forwarded TCP.

## Agent launch configuration

Server configuration accepts an optional `agent_command` argument array, defaulting
to `["pi"]` when absent:

```json
{"repositories": [], "agent_command": ["pi", "--model", "provider/model"]}
```

The first element must be a nonempty executable; all elements must be strings
without NUL bytes. Arguments are literal, not a shell command: shell expansion and
initialization are not performed. Restart the daemon to reload this setting;
existing agents survive and keep their original command/environment. New agents
receive the updated setting. Configured paths such as `./tools/agent` are relative
to the validated checkout root; bare names are resolved using caller PATH.

## Run a checkout agent

Agent execution currently requires **Linux with procfs and tmux 3.2 or newer**.
Checkout, Git/common, and runtime directories must use supported local directory
identities: ext2/3/4, XFS, Btrfs, tmpfs, or OverlayFS. NFS/FUSE/9p and unknown
filesystems fail closed: remote inode reuse is not prevented by an open local FD.
The Unix control socket and repository browsing also support macOS, but agent
creation there fails closed until reliable descendant containment is implemented.
Activate your normal shell environment (including mise, if used), enter a
registered main checkout or linked worktree—even a subdirectory or symlink—and run:

```sh
cd /path/to/registered/checkout
wumpa agent --socket "$HOME/.local/run/wumpa/control.sock"
```

With no live agents, Wumpa creates one and attaches. Otherwise choose a numbered
session, `n` for another agent, or `q` to cancel. Multiple agents may edit the same
checkout; their filesystem edits are not isolated. Detach with tmux's default
`Ctrl-B`, then `D`; the agent keeps running. Running `wumpa agent` again resumes it
without changing its environment. An attachment failure never deliberately stops
an agent. Within the same dedicated tmux server, Wumpa switches clients; from a
different tmux server, detach first and rerun—nested tmux is refused before creation.
Your personal/default tmux server and configuration are not used.

The agent receives the **calling CLI's environment**, not the service or tmux
server environment. Byte-safe Base64 transport preserves non-UTF-8 Unix entries;
Base64 is not encryption. The compact encoded payload is limited to 256 KiB and
4,096 variables. Invalid, duplicate, or oversized entries fail without exposing
contents; environments are never saved in session metadata or remote snapshots.
Stale terminal/tmux bookkeeping is filtered and replaced with fresh pane settings.
Relative/empty PATH entries use the CLI's original validated working directory,
not the checkout root. Missing caller PATH rejects bare executable names, without
a service-PATH fallback. Absolute and checkout-relative executable paths do not
require PATH. OS argument/environment launch limits also apply. If PATH expansion
would include a caller directory containing `:`, it fails rather than changing
search semantics.

If creation loses its response, Wumpa prints a **retry key**. Reuse that key rather
than blindly requesting another agent:

```sh
wumpa agent --socket /absolute/control.sock --retry RUN_ID:REQUEST_ID
```

Retry performs fresh handshake/preflight and resolves the original outcome; it
never launches a replacement. Missing/ambiguous recovery evidence is an explicit
error. A recovered ID may refer to an agent that has since exited; attachment
reports that separately.

## Session lifetime and instance state

For `/absolute/control.sock`, the dedicated tmux server, supervisor sockets, and
non-secret outcome records live under `/absolute/.control.sock.sessions/` (mode
`0700`, files/sockets `0600`). An adjacent `.control.sock.session-identity.json`
protects against accidental runtime replacement. Restarting at the same canonical
socket path rediscovers live sessions; different paths own independent servers,
even when registering the same checkout. Keep socket paths short enough for the
longer derived Unix socket paths; Wumpa fails explicitly rather than choosing a
different instance location.

**Do not delete, move, or replace runtime directories, identity markers, or startup
locks while owned sessions may exist.** Unexpected ownership, missing recovery
records, or conflicting identity blocks new launch/recovery rather than silently
reassociating agents. There are at most 64 live sessions and 4,096 creation outcome
records per instance. Records are not automatically evicted: when capacity is
full, creation fails before launch. Archive old-run records only with the daemon
stopped and all owned agents confirmed terminated; keep runtime directories and
identity markers intact. An archived/missing retry outcome fails as unknown.

Each persistent Linux supervisor pins the checkout root, Git directory, shared
Git directory, and runtime directory with open descriptors. It force-stops and
reaps its owned descendants (including detached/double-forked children) after
confirmed removal, replacement, or movement of the old location. It checks paths
every 100 ms and runs bounded Git cross-checks about once per second, independently
of dashboards and daemon availability. Access/discovery failures alone never
establish removal. Agent exit also cleans up remaining descendants and removes
the entire tmux session, including extra panes/windows; exited terminal output is
not retained. Supervisors preserve non-secret `.done` completion evidence so
reconciliation can finish session removal if the supervisor endpoint is gone. Forced termination starts
immediately on detection, without a grace period; completion verification is
bounded to two seconds. Failed cleanup remains supervised/reported, not treated
as successful removal. These are same-user process/lifecycle protections, not a
sandbox against deliberate interference by that OS user.

The daemon refreshes cached discovery about every 500 ms with a two-second budget.
Discovery failures/unsupported execution are distinct from successful empty data
and do not prevent repository browsing. Dashboard/plain-text agents use 🤖 under
their checkout; linked worktrees use 🌲. Agent selection preserves checkout actions
such as opening Zed. No checkout-deletion command is added; future managed deletion
must use the stop-before-remove callback while holding the instance manager lock,
and abort if termination cannot be verified.

`tnt/nixos/server.nix` supplies tmux, a private `/run/wumpa/control.sock`, persistent
runtime state, and process-only service shutdown so tmux/supervisors survive daemon
restart/stop. This configuration targets a feature-capable binary; the pinned
release is unchanged. Deployment and actual NixOS service-lifecycle validation
remain separate work—nothing has been deployed.
