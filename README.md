
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

The local control protocol provides an internal read-only version-1 handshake
and checkout preflight: newline-delimited JSON, at most 1 MiB including the newline,
one exchange per connection, with a 2-second connection and 5-second total exchange
deadline. The handshake returns the canonical socket path and a fresh daemon-run
ID; preflight requires that run ID and validates against the daemon's live
registration snapshot without modifying configuration. Main/linked checkouts,
subdirectories, and aliases are supported. Required canonical paths and device/inode
observations of the caller directory, checkout root, Git directory, and shared Git
directory must agree. Unavailable checks fail preflight but never establish removal.
These observations are not durable identities or proof of identical filesystem
namespaces; deliberate Unix socket forwarding remains possible. TCP/SSH repository
access is unchanged; `wumpa agent` and tmux sessions are not yet implemented.
