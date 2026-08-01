# Vendored app-server schemas

Every file under here is machine-generated output from the `codex` CLI itself
— never hand-written, never edited after capture. They are the permitted
source this crate's typed protocol bodies (`src/schema.rs`) derive from, per
`docs/derivation/SPECS.md`'s `CODEX-APP-SERVER` row.

## Where these came from

| | |
|---|---|
| Command | `codex app-server generate-json-schema --out <dir> --experimental` |
| CLI version | `codex-cli 0.146.0` — the pin in `docs/PARITY.md`'s version table |
| Captured | 2026-08-01, offline, run exactly once |

Codex's generator emits **two schema generations in one bundle**: a legacy
surface, and the thread/turn/item surface this adapter actually speaks
(`docs/PARITY.md`'s inventory notes: "build 0.146.0 ships two protocol
generations in one schema bundle"). The generator's own output splits these
by directory — `v1/` for the legacy generation, `v2/` for thread/turn/item —
and this directory keeps that split for the files it vendors. **Only `v2/`
files are vendored here.** `v1/` (two files: `InitializeParams`/
`InitializeResponse`, the legacy handshake) is not — this adapter never
speaks the legacy generation, so vendoring it would be pinning a surface
nothing in this crate reads.

## Why a subset, not the whole bundle

The full run outputs 300+ files (every request, response, and notification
the app-server protocol has, including remote-control pairing, MCP OAuth,
plugin marketplace management, and other surfaces this adapter has no reason
to touch). Vendoring all of it would pin hundreds of type definitions this
crate never derives a line from — the opposite of what CLEANROOM.md rule 3
asks a citation to mean. What is here is exactly the set `src/schema.rs`
names in a `Derivation:` marker:

| File | What it's for |
|---|---|
| `JSONRPCMessage.json` | The wire envelope: request / notification / response / error, and the `id` union type. Root-level in the generator's own output — shared by both generations, not v2-specific. |
| `ServerNotification.json` | The `method` ⇄ params-type mapping for every notification the app-server can send — where `event.rs`'s method-string constants and the `error`/`thread/started`/etc. citations in `schema.rs` come from. Root-level, same reason as `JSONRPCMessage.json`. |
| `ServerRequest.json` | The same mapping for server-initiated requests — where the two approval-relay method strings come from. |
| `v2/ThreadStartedNotification.json` | `thread/started` — lifecycle: start |
| `v2/ThreadStatusChangedNotification.json` | `thread/status/changed` — status truth, including `waitingOnApproval` |
| `v2/ThreadClosedNotification.json` | `thread/closed` — lifecycle: end |
| `v2/TurnCompletedNotification.json` | `turn/completed` — lifecycle: idle/error, and the typed `ThreadItem`/`Turn` shapes transcript ingestion needs |
| `v2/ErrorNotification.json` | the `error` notification — lifecycle: error |
| `v2/ItemStartedNotification.json` | `item/started` — transcript ingestion |
| `v2/ItemCompletedNotification.json` | `item/completed` — transcript ingestion |
| `v2/ThreadTokenUsageUpdatedNotification.json` | `thread/tokenUsage/updated` — the `RecordCostEntry` source |
| `v2/ServerRequestResolvedNotification.json` | `serverRequest/resolved` — approval-relay clearance |
| `CommandExecutionRequestApprovalParams.json` / `...Response.json` | `item/commandExecution/requestApproval` — approval relay |
| `FileChangeRequestApprovalParams.json` / `...Response.json` | `item/fileChange/requestApproval` — approval relay |

A future change that reads a field from a type not listed above needs to
vendor that file first — that ordering (registry before code, restated here
for schema files rather than `SPECS.md` rows) is the same discipline
`docs/derivation/SPECS.md` states for spec rows.

## Freshness

`tests/schema_freshness.rs` re-runs the same generator command against
whatever `codex` binary is on `PATH` and diffs its output, file for file,
against what's vendored here. It **skips with a printed message** when no
`codex` binary is on `PATH`, or when the installed CLI reports a version
other than the pin above — public CI has neither, by design (`docs/PARITY.md`:
"public CI... must never acquire an engine binary, an engine login, or a
network path to either"). Where a live `codex` at the pinned version IS
present, the test is a real verification, not a skip.
