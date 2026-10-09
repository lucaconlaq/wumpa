
# Wumpa
Code on your remote server without juggling SSH sessions. Wumpa lets you manage projects and worktrees, keep agent coding sessions running, and open your code in Zed—all from your terminal.

<p align="center">
  <img src="./.github/logo.png" alt="Grape Cola" width="200" />
</p>

## Platform support

macOS and Linux support repository browsing, cloning, and the local control
socket. Coding agent sessions currently require a Linux server: macOS servers
reject agent operations explicitly because verified descendant-process cleanup
is not implemented there. macOS clients can still display agent sessions running
on Linux servers.

Start a server with an explicit socket in an existing user-owned private directory:

```sh
mkdir -p "$HOME/.wumpa-runtime"
chmod 700 "$HOME/.wumpa-runtime"
wumpa serve --socket "$HOME/.wumpa-runtime/control.sock"
```

On Linux, run `wumpa agent --socket "$HOME/.wumpa-runtime/control.sock"` from a
registered checkout to create or attach an agent. The server requires tmux 3.2 or
newer. This local command does not connect to a remote server over TCP or SSH.
