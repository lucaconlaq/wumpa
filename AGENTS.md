# Agent Instructions

Before making changes, read:

- [README.md](README.md) for the tool's purpose, installation, and user-facing usage.
- [Architecture](docs/ARCHITECTURE.md) when locating behavior, changing module
  boundaries, or evaluating integration, persistence, and security implications.
- [Glossary](docs/GLOSSARY.md) when discussing domain concepts or writing code,
  architecture, ideas, specs, or tickets; use its canonical terms.
- [Repository rules](docs/rules/index.md) before changing or reviewing files; read
  and follow all rule documents whose repository-wide patterns match the files.

Keep changes focused and preserve existing behavior unless a change is requested.
Keep coding conventions in the rules directory rather than duplicating them here.
Report unavailable environments and checks not performed; do not imply that test
presence or a newer compiler establishes platform coverage or minimum-version
compatibility.

For Pi activity work, also read [the integration documentation](agent-extensions/README.md).
For TNT deployment work, read [the deployment documentation](tnt/README.md), noting
its unresolved references to the absent Terraform `state.tf`.

Ideas, specs, and tickets are created only when needed under `docs/ideas/`,
`docs/specs/`, and `docs/tickets/`. Preserve provenance and approval history; do not
infer agreement from a finished-looking legacy plan or treat documentation as
implementation authorization.
