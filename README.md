
# Wumpa
Code on your remote server without juggling SSH sessions. Wumpa lets you manage projects and worktrees, keep agent coding sessions running, and open your code in Zed—all from your terminal.

<p align="center">
  <img src="./.github/logo.png" alt="Grape Cola" width="200" />
</p>

In the dashboard, select a repository or worktree and press `z` to open it in Zed,
or `t` to SSH directly into its folder in the current terminal. Exit the remote
shell to return to Wumpa. SSH uses your configured host alias and SSH settings
(not the Wumpa daemon port); this action requires an SSH server connection.

On an agent row, press `Enter` to SSH to the server and attach directly to that
agent's existing tmux session. `t` is disabled on agent rows. Detach with tmux's
`Ctrl-b`, then `d` to return to Wumpa without stopping the agent. Both client and
server must be updated, and `wumpa` must be on the remote SSH command PATH. The
server discovers its control socket automatically; no extra configuration is
needed. Attachment validates the existing agent and never creates a replacement.
If SSH or attachment fails, its diagnostics stay visible until you press Enter.
If `agent-attach` is unrecognized, check the remote binary with
`ssh YOUR_HOST 'command -v wumpa; wumpa agent-attach --help'` and update that binary.

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
