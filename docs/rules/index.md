# Repository Rules

Before changing or reviewing files, read every rule document matching those files.
Patterns apply throughout the repository, including nested directories.

| File patterns | Rules | Scope |
| --- | --- | --- |
| `*.rs`, `Cargo.toml` | [Rust](RUST.md) | Rust source and Cargo manifest formatting, compatibility, engineering requirements, and review checks. |

The Rust rules are adapted from `../psst/docs/rules/RUST.md`, preserving Wumpa's
existing Rust 1.85 requirement and engineering guidance formerly in
`docs/CODE_GUIDELINES.md`. Wumpa-specific toolchain and CI descriptions replace
those of the source repository. This directory is the authoritative home for
coding conventions; do not maintain parallel copies elsewhere.
