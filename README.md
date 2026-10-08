
# Wumpa
Wumpa is a CLI and terminal UI for coding on a remote server. It manages repositories, Git worktrees, and persistent pi coding sessions, and opens remote checkouts in Zed.

<p align="center">
  <img src="./.github/logo.png" alt="Grape Cola" width="200" />
</p>

## Releases

Push a version tag (for example, `v0.1.0`) to build, test, and publish a GitHub
release. Update the version in `Cargo.toml` and `Cargo.lock` before tagging.
Builds upload directly to a draft release, which is published only after all four
targets pass. Failed runs leave the draft unpublished; rerun failed jobs to retry.

Releases include `wumpa-<target>.tar.gz` archives for:

- `aarch64-apple-darwin` — Apple Silicon Macs
- `x86_64-apple-darwin` — Intel Macs
- `x86_64-unknown-linux-musl` — x86-64 Linux, including `tnt`
- `aarch64-unknown-linux-musl` — ARM64 Linux

Each archive contains a `wumpa` executable; Linux binaries are statically linked.
`SHA256SUMS` contains checksums for all archives. Private-repository downloads
require GitHub authentication, such as `gh release download`.
