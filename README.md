# Wumpa

<p align="center">
  <img src="./.github/logo.png" alt="Grape Cola" width="200" />
</p>

Manage projects, Git worktrees, and persistent coding-agent sessions on a remote
server from your terminal. Open checkouts in Zed without juggling SSH sessions.

- 🖥️ Manage remote projects from your terminal.
- 🌿 Access Git worktrees.
- 🤖 Keep coding-agent sessions running and reconnect anytime.
- ⚡ Open remote checkouts in Zed.
- 🔐 Connect through SSH.

## Quick start

1. Install on your computer and Linux server (requires Rust 1.85+):
   ```sh
   cargo install --path .
   ```
2. Start Wumpa on the server:
   ```sh
   mkdir -p "$HOME/.wumpa/run"
   chmod 700 "$HOME/.wumpa/run"
   wumpa serve --socket "$HOME/.wumpa/run/control.sock" --detach
   ```
3. Run `wumpa` on your computer. Press `n` to add your server's SSH alias or
   `user@host` (default port: `7432`), then Enter to connect. Use the on-screen
   shortcuts to manage repositories, worktrees, and agents; press `z` to open a
   checkout in Zed.

The dashboard shows `clean` or `dirty +added -removed` beside each checkout and
worktree. Line totals sum staged and unstaged tracked changes (a modified line
counts as a removal and an addition); untracked files are counted separately.
Binary, mode, and submodule changes can be dirty with zero line changes. Totals
update with the workspace refresh; unavailable Git metadata is never shown as clean.
