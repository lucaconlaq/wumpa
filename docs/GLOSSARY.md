# Project Glossary

Canonical domain vocabulary for code, documentation, and discussions.

## Terms

**Server profile**:
A named, saved client connection to a Wumpa server, reached locally or through SSH.
Removing a profile does not remove the server or its repositories.

**Repository**:
A saved Git repository URL, optionally associated with a main checkout on the
server. A saved repository need not have been cloned yet.

**Checkout**:
A local working copy of a Git repository on the server, including either its main
checkout or a linked worktree. Agents are associated with a specific checkout.

**Worktree**:
An additional Git working tree linked to a repository and sharing its common Git
directory. It is a checkout distinct from the main checkout.

**Agent session**:
A Wumpa-managed, persistent coding-agent execution associated with a checkout.
Its Wumpa identity is independent of its display name and any Pi conversation.
_Avoid_: Pi session (when referring to the Wumpa-managed execution).

**Instance**:
A Wumpa server's local control identity, selected by its canonical control socket
path and retained across daemon restarts.

**Attachment**:
A terminal connection to an existing agent session. Detaching ends that connection,
not the agent session.

**Pi conversation**:
A conversation managed by Pi within an agent session. Its name and identity may
change independently of the enclosing Wumpa agent session.

**Activity**:
An observation of an integrated agent's current conversation-level work, distinct
from its process lifecycle. Waiting for input does not establish that arbitrary
shell commands are inactive or that approval has been granted; unknown activity
must not be interpreted as idle.
