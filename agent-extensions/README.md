# Bundled Pi activity extension

`pi.ts` is embedded in Wumpa with Rust `include_str!()` by
`src/agent_integration.rs`. Explicitly configured Pi agents receive a private
runtime copy and literal extension/name arguments from their runner. There is no
separate package to install. `src/agent_activity.rs` maintains authenticated,
bounded subscriptions independently of Git and agent-control locks. Remote-safe
summaries drive dashboard and plain activity/name rows.

## Verified contract

- Host: `@earendil-works/pi-coding-agent` **1.0.4**, interactive mode, Linux.
  Other versions are not yet verified. No model credentials are required for the
  offline smoke test below. The source uses only Node built-ins at runtime.
- Per-child environment: `WUMPA_ACTIVITY_SOCKET` (absolute endpoint) and
  `WUMPA_SESSION_ID` (32 lowercase hexadecimal characters). The runner must
  override both after preparing the caller environment; neither is a credential.
- Endpoint: `a-<Wumpa ID>.activity.sock` beside the source in a fresh, canonical,
  owned `0700` per-agent directory under Wumpa's controlled runtime. Linux Unix
  paths are limited to 107 bytes excluding the NUL terminator.
- The integration is silent: no startup/reload notifications or stdout output.
  Existing live agents retain their original source; rebuild/restart Wumpa and
  launch a new agent to pick up source changes.
- Startup occurs only on `session_start`. Shutdown is idempotent. Replacement
  contexts arrive through `session_start` for new/resumed/forked conversations;
  `/clone` follows the same replacement lifecycle in the tested Pi version.
- A new runtime has a UUID generation. Sequence numbers are positive safe integers
  increasing across all full snapshots, including heartbeats and subscriptions.
  Gaps are allowed. They are not per-subscriber counters.
- `agent_start` publishes `working`; `agent_settled` samples `ctx.isIdle()`.
  `agent_end` and `turn_end` are not used to infer completion. Name changes use
  `pi.getSessionName()`, including `null` when it returns `undefined`.
- Compaction and tree boundaries sample authoritative idle state. Successful
  completion hooks may still see busy state; a deferred sample and the 5-second
  heartbeat reconcile after Pi clears its internal controller. No idle state is
  inferred from a missing event. Waiting does not certify inactivity of arbitrary
  user shells or other extensions, and is not an approval signal.
- Names are raw, nullable Unicode, truncated to 512 code points and JSON-escaped
  on the wire. **Rust/display consumers must escape controls and terminal sequences
  before rendering.** These names are never backend/session identity.

## Version 1 JSONL

One LF-framed UTF-8 record, at most 8192 bytes **excluding LF**. Unicode paragraph
and line separators do not delimit records. Each connection sends exactly one
subscription, with exactly these keys:

```json
{"type":"subscribe","version":1,"wumpa_session_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
```

It then receives full snapshots immediately, on changes, and every 5 seconds:

```json
{"type":"status","version":1,"generation":"4c29a05f-a3c3-4a5d-901f-8f0d4107fd29","sequence":2,"wumpa_session_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","pi_session_id":"conversation-id","pi_session_name":"Fix authentication","activity":"waiting_for_input"}
```

Activity is `working` or `waiting_for_input`. The Rust subscriber must represent
unsupported, malformed, disconnected, or stale integrations as `Unknown`. It uses
a monotonic local receipt time (15-second expiry), verifies peer UID and pinned
namespace identities, rejects incorrect IDs/versions, and enforces generation and
sequence ordering, including replay checks across reconnects.

Four connections are allowed, including unverified handshakes. The handshake
expires after 3 seconds. Additional input, invalid UTF-8, unexpected fields,
malformed JSON, oversized frames, unsupported versions, or ID mismatches close the
connection. Backpressured writes disconnect rather than await or grow a queue.
Hooks never wait for subscribers. No transcript, prompt, environment, or model
credential is collected or sent.

## Filesystem ownership and runner lifetime

The listener binds inside a unique private staging directory, applies `0600`, then
publishes the endpoint via an exclusive hard link. This avoids overwriting occupied
paths and prevents Node's automatic close/unlink from deleting a replacement at
the public endpoint. Shutdown removes the published link only if its inode,
device, owner, type, and mode still match the bound socket; staging cleanup never
recursively removes unknown entries. Missing or insecure paths disable reporting
without affecting Pi.

**Occupied endpoints are deliberately not reclaimed by the extension.** The
runner creates an exclusive per-agent directory and atomically publishes the
embedded source as `pi-v1.ts` with mode `0600`. The directory remains available
through reloads and daemon restarts. After verified descendant termination, the
runner removes its directory, including stale sockets/staging files left by
abrupt Pi exit. It pins and checks directory identity before cleanup and does not
follow symlinks. Error unwinds after spawn retain resources, rather than deleting
a possibly live agent's source. Local recovery records retain the version and
directory association; old records/runners without it remain uninstrumented.
Same-user processes are within the trust boundary; an ID is not authentication
against the same UID.

## Launch/config precedence

Set `agent_integration: "pi"` in the existing server configuration to enable the
integration for newly launched agents after restarting the Wumpa server. The
default is `"disabled"`, even when
`agent_command` is `["pi"]`. Only explicit Pi selection adds literal
`--extension <path>`; normal Pi resource discovery remains enabled. Wrappers must
accept/forward these flags. Integration is never inferred from an executable name.
There is no modification to `~/.pi`, service environment, or tmux environment.

For a new conversation, prepend `--name <Wumpa label>` before configured Pi
arguments (and before an option terminator). Do not inject a name when the
configured arguments explicitly name a conversation (`--name`/`-n`) or can open
an existing conversation (`--continue`/`-c`, `--resume`/`-r`, `--session`,
`--session-id`, or `--fork`). Argument values must not be mistaken for switches;
flags following `--` are prompts, not options. Unknown extension/wrapper switches
conservatively suppress name initialization because their argument semantics are
not known. These rules have unit and launch tests. The extension never initializes
or resets a name, so reloads and session replacements preserve Pi's decisions.
An unnamed resumed conversation remains unnamed.

After caller-environment preparation, the runner overrides both reserved variables
for instrumented children and removes them for disabled/unsupported integrations.
Materialization failures (including overlong socket paths) fall back to the
unmodified configured agent command, without Pi flags or reserved environment.
Each source copy belongs to one runner; upgrades never overwrite live copies.

## Development validation

Run the isolated socket/lifecycle suite on Linux with Node **22.18+** (native
TypeScript stripping; Node 24.21.0 was used here):

```sh
node --test tests/pi_activity.test.mjs
```

An optional real interactive smoke test uses util-linux `script` to provide a PTY.
It isolates HOME/Pi configuration in a temporary directory, loads an ordinary
user extension as a sentinel, disables network activity, sends only slash commands,
and makes no model requests:

```sh
WUMPA_PI_TEST_EXECUTABLE="$(realpath "$(command -v pi)")" \
    node --test tests/pi_activity_interactive.test.mjs
```

Without the environment variable, real-Pi tests are skipped. The smoke test covers
initial naming, idle, ordinary resource discovery, rename, reload, new, and clone.
A second PTY test drives actual Pi prompts, a bash tool, automatic retry, queued
follow-up, cancellation, and manual compaction against a deterministic loopback
OpenAI-compatible model double. It uses disposable configuration, controlled
prompts, and no external model credentials:

```sh
WUMPA_PI_TEST_EXECUTABLE="$(realpath "$(command -v pi)")" \
    node --test tests/pi_activity.test.mjs tests/pi_activity_interactive.test.mjs \
    tests/pi_activity_workflow.test.mjs
```

Real provider behavior, persisted named resume and interactive `/resume`/`/fork`
selectors, branch summaries, approval dialogs, abrupt real-Pi exit, and combined
Pi-to-dashboard latency over an actual SSH connection remain unverified. Naming
precedence and recovery are also covered by literal-launch and socket-double tests;
these do not establish every real-Pi workflow.

The optional binary-to-Pi launch test also verifies embedded source materialization,
initial naming, ordinary extension discovery, `/name`, `/reload`, `/new`, daemon
restart while Pi survives, remote activity/name projection with stable Wumpa IDs,
rename-to-server-cache latency, and cleanup after verified termination:

```sh
WUMPA_PI_TEST_EXECUTABLE="$(realpath "$(command -v pi)")" \
    cargo test --locked --test agent_sessions real_pi_
```

The bundled extension was also typechecked with TypeScript 5.8.3, strict checking,
`noEmit`, `skipLibCheck`, ES2022/ESNext, and bundler module resolution against Pi
1.0.4 and Node declarations. Native stripping and a Pi load do not substitute for
this check. With those declarations on the compiler's module/type search paths:

```sh
tsc --noEmit --strict --skipLibCheck --target ES2022 --module ESNext \
    --moduleResolution bundler --types node agent-extensions/pi.ts
```

## Display, refresh, and troubleshooting

- `Working` displays as `Running`; `WaitingForInput` displays as `Waiting for input`.
  Process starting/stopping/cleanup failure states take precedence. Waiting refers
  only to Pi session-level work, not arbitrary extension commands, shells, or an
  approval decision. Missing observations never imply idle.
- Pi names are display-only; controls, bidi controls, and Unicode line separators
  are escaped. Unnamed/blank conversations use the original Wumpa label. Silence
  retains a cached name with `Unknown` activity; a verified clear or conversation
  switch replaces it. Rows display the shortest server-wide unique Wumpa ID prefix,
  at least five hex characters; prefixes can expand/shrink as sessions change.
  The full Wumpa ID and `wumpa-<ID>` tmux target never change, and short prefixes
  are never sent in attachment/control requests.
- The extension pushes to one persistent server subscription. At most 64 worker
  slots are retained, including unresolved recovery identities; excess agents
  remain usable but untracked. Validated frames are capped at 8 KiB and 64/second
  per connection. Reconnect backoff is bounded at two seconds. Missing recovery
  identities are retained until verified runner completion; discovery failure
  is not termination.
- An optional version-1 `session_status` request reads only the server cache,
  without Git discovery or agent control. The dashboard schedules it roughly one
  second after a successful refresh, with a five-second request deadline and
  five-second failure backoff. It preserves selection/forms/details and cancels
  before attachment/server switches. It pauses while another operation is active.
  Client silence also expires activity after 15 seconds. Transport latency still
  applies; plain listings do not subscribe or continuously refresh.
- Older servers omit the refresh capability and are never sent new requests.
  Optional summary fields preserve old-client compatibility. Existing agents and
  disabled/non-Pi integrations show `Unknown`; enabling configuration affects new
  launches only. Remote summaries never include activity endpoint/source paths.
- For unexpected `Unknown`, verify explicit opt-in, rebuild/restart Wumpa, and
  launch a new agent. Check that wrappers forward literal extension flags, Pi is
  the supported version, and the private Unix path fits 107 bytes. Insecure,
  occupied, replaced, malformed, or unsupported endpoints fail closed. Do not
  chmod runtime directories broadly or delete live agents' files to force recovery.
- If a name looks stale, it may be deliberately cached during disconnection.
  Reconnecting to a verified snapshot restores activity and reconciles the name.
  If many unresolved old identities exhaust worker slots, restart the daemon;
  surviving agents/source files remain intact. Runner-retained artifacts after
  uncertain termination require identity-checked investigation, not blind cleanup.
- Upgrades do not rewrite a live source copy. Rebuild/restart the daemon and launch
  new agents for extension changes. Older recovery records remain usable without
  instrumentation. No separate installation, `~/.pi` edit, deployment, or release
  change is required by this implementation.
