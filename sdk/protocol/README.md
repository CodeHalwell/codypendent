# `@codypendent/protocol`

TypeScript types for the Codypendent daemon wire protocol.

## Generated from the Rust protocol schemas

The wire contracts in `src/generated/` are produced from the authoritative
Draft 7 schemas in `schema/`, which are exported from `crates/protocol/src/*.rs`.
`scripts/generate.mjs` pins `json-schema-to-typescript` and emits only the
public command, event, envelope, payload, and identifier surfaces; the small
files beside them are stable public facades and handwritten runtime helpers.

Generation drift and serialized compatibility are separate gates. The drift
checker regenerates twice and compares the complete committed directory.
`test/protocol-vectors.test.ts` then pins the generated declarations to the
**committed golden vectors** in `<repo-root>/protocol-vectors/`. Those vectors
are the authoritative serialized shapes: `crates/protocol/tests/golden_vectors.rs`
both generates them and asserts in CI that a fresh regeneration matches the
committed bytes exactly, so a Rust type that changes shape without regenerating
fails on the Rust side, and a regenerated vector that this package cannot
represent fails here.

This package reads those same files directly by relative path. Both editor
integrations consume these generated contracts rather than maintaining a second
wire model.

## Node clients and session discovery

The `@codypendent/protocol/node` entry point exports `NodeDaemonClient` and the
daemon's socket-discovery rules. The main entry point remains host-neutral.
The Codypendent VS Code client and Son of Anton use this same Node adapter.

After `serverHello`, `requestGlobal` permits session listing, searching, and
creation without an existing attached session. It returns the entire response
envelope because `CreateSession` places the new id in `session_id`, outside the
`CommandAccepted` payload. `request` continues to require an attached session
and returns a payload.

Correlated requests expire after 15 seconds by default (`requestTimeoutMs`
overrides this). Disconnect rejects outstanding requests; they are not replayed
on reconnect. A timeout or lost connection can leave a mutation's outcome
unknown, so callers should reconcile daemon state before retrying it. These
request semantics are separate from the existing explicit offline command queue.

### What the conformance suite actually checks

Each vector is `unknown` JSON at runtime, so the suite runs it through a
`reconstructX` function that copies named fields one by one into an object
literal **annotated with the exact exported type**. Two independent failures
follow from drift:

1. **`npm run typecheck` fails** when the literal names a field the type does
   not declare, or omits one it requires. (Verified: adding a stray property to
   any reconstructed literal is a `TS2353`.)
2. **The round-trip deep-equal fails** when a field present in the vector never
   made it across, because the type had nowhere to put it. (Verified: deleting
   `label` from the `ToolStarted` reconstruction fails
   `decodes and re-encodes EventBody_ToolStarted identically`.)

Coverage is **total**: every committed vector file, and every vector in it, is
exercised. The suite walks the directory listing at run time rather than naming
files, so a newly added vector file is picked up automatically instead of being
silently ignored — the failure mode that let three new vector files land in the
extension's suite with every assertion still green. The machinery to declare
something out of scope is still there (`EXCLUDED_VECTORS`, `EXCLUDED_FILES`) and
both lists are empty today; a vector with no reconstructor and no exclusion entry
fails the per-file partition check.

### What golden vectors do not cover

The schemas cover every reachable Rust variant, but a brand-new variant does
not automatically acquire a golden example (see `protocol-vectors/README.md`).
The generated declaration therefore prevents source drift while the vector
suite proves concrete serialized examples. The currently vector-less families
remain:

- `CommandBody`: the UI-plugin lifecycle family, `ResolveQuestion`,
  `RestoreCheckpoint`, `ForkSession`, the queued-prompt family, `RunUserShell`,
  `RememberMemory`.
- `Payload`: `SessionForked`, `UiPluginLifecycle`, and `RemoteUi`; the generated
  `RemoteUi` declaration includes the complete `UiWireMessage` schema even
  though no committed golden example currently exercises it.
- `EventBody`: `ModelRetrying`, `ContextUsage`, `QuestionAsked`,
  `QuestionResolved`, `CheckpointRecorded`, `CheckpointRestored`,
  `SessionForked`, `PendingPromptsChanged`.

## How to keep it in sync

When a wire type in `crates/protocol/src/` changes:

1. Export the authoritative schemas:
   `cargo run --locked -p codypendent-protocol --features schema-export --bin export_schema -- --output-dir sdk/protocol/schema`.
2. Regenerate the TypeScript declarations with
   `npm --prefix sdk/protocol run generate`.
3. Regenerate the vectors as `protocol-vectors/README.md` describes
   (`cargo test -p codypendent-protocol --test golden_vectors regenerate_vectors -- --ignored`).
   For a brand-new **variant**, add its `vec_of(...)` entry first — otherwise
   the example suite will not cover it.
4. Run `npm --prefix sdk/protocol run check`. It verifies deterministic
   generation, typechecks every vector reconstructor, runs all tests, and builds
   the publishable package.

## Conventions used when mapping Rust to TypeScript

| Rust | TypeScript |
| --- | --- |
| `#[serde(tag = "type")]` enum | discriminated union on `type` |
| `#[serde(tag = "op")]` / `"kind"` / `"block"` | union on that field instead — `DocumentMutation`, `PublishTarget`, `InputBlock` |
| `#[serde(other)] Unknown` | an explicit `{ type: "Unknown" }` member, plus the folds in `src/tags.ts` |
| newtype variant, e.g. `Payload::Event(SessionEvent)` | intersection — the inner fields sit alongside the tag |
| `skip_serializing_if` (`Option::is_none`, `Vec::is_empty`, zero, false, empty string) | optional `field?:` — absent on the wire, never a placeholder value |
| `#[serde(default)]` with **no** skip | required — the daemon always serializes it (e.g. `ExecuteCommand.environment`, `ExecuteCommand.cwd`, `IdeContextUpdate.diagnostics_revision`) |
| `Option<T>` with no skip | `T \| null` — serialized as an explicit `null` |
| `#[serde(transparent)]` id newtypes | plain `string` aliases (`SessionId`, `RunId`, …), so `JSON.parse` output is directly assignable |
| `chrono::DateTime<Utc>` | `Timestamp` (an RFC 3339 string) |
| `serde_json::Value` | `JsonValue` |
| `u64` / `i64` | `number` — values above `Number.MAX_SAFE_INTEGER` are not representable, which matches what `JSON.parse` would give you anyway |

## Forward compatibility

Rust's `#[serde(other)] Unknown` degrades an unrecognized variant instead of
failing the frame. TypeScript has no deserializer to hang that off, so
`src/tags.ts` provides it explicitly:

```ts
import { foldUnknownPayload } from "@codypendent/protocol";

const payload = foldUnknownPayload(JSON.parse(frame) as { type: string });
// a tag from a newer daemon becomes { type: "Unknown" } rather than a lie
```

`PAYLOAD_TAGS`, `EVENT_BODY_TAGS` and `COMMAND_BODY_TAGS` are checked against
their unions at compile time in both directions, so a variant added to a union
without a tag entry (or vice versa) is a type error.

## Scripts

```sh
npm --prefix sdk/protocol run generate # refresh committed declarations
npm --prefix sdk/protocol run check    # generation drift + typecheck + vectors + build
```
