# Protocol golden vectors

This directory is the **single source of truth** for the Rust <-> TypeScript
wire-codec drift guard (T16). The VS Code extension hand-duplicates the Rust
wire codec in `extensions/vscode/src/protocol/` (there is no generated SDK —
see `ROADMAP.md`'s cross-cutting "Generate the protocol SDK" item). That
duplication drifted once for real: the S1 bug, where the extension's approval
card omitted the `environment`/`cwd` fields the Rust
`ProposedAction::ExecuteCommand` type carries. These vectors exist so that
never happens silently again.

## What is here

One JSON file per source module in `crates/protocol/src/`
(`command.rs` -> `command.json`, `envelope.rs` -> `envelope.json`, ...). Each
file is a JSON object mapping a descriptive vector name (e.g.
`CommandBody_StartRun`, `ProposedAction_ExecuteCommand`) to one deterministic,
pretty-printed instance of that Rust type's serialized JSON. Every id is a
fixed sentinel (never `Uuid::now_v7()`) and every timestamp is fixed (never
`Utc::now()`), so the files are byte-for-byte stable across regenerations
until a wire type actually changes.

## Who reads these files

- **Rust**: `crates/protocol/tests/golden_vectors.rs` is both the generator
  and the two CI checks:
  - `committed_vectors_match_current_protocol_types` — a fresh regeneration
    must equal the committed bytes exactly (catches "changed a type but forgot
    to regenerate").
  - `committed_vectors_round_trip_through_their_rust_types` — every committed
    entry, read off disk, deserialized through its own concrete Rust type, and
    re-serialized, must reproduce itself exactly (catches a hand-edited or
    otherwise-stale file even if the check above were bypassed).
  Both run in the ordinary `cargo test --workspace --all-features` CI job —
  no separate CI wiring needed.
- **TypeScript**: `extensions/vscode/test/protocol-vectors.test.ts` reads
  these SAME files directly via a relative path
  (`extensions/vscode/test/` -> `../../../protocol-vectors/`) — no copy, no
  second source of truth. It asserts the extension's hand-written
  `CommandBody`/`Payload`/`EventBody`/`ProposedAction`/... types in
  `src/protocol/types.ts` can represent every field of the vectors the
  extension actually sends/consumes, and that command vectors re-encode to
  identical JSON. This runs in the existing `extension` CI job via `npm test`
  — no separate CI wiring needed there either.

Both sides read the identical files; neither copies or re-derives the other's
data. A Rust field the TypeScript type lacks makes the corresponding vector
fail on the TypeScript side — that is the drift catch.

## Regenerating

Whenever a wire type in `crates/protocol/src/` changes shape (new field, new
variant, a changed field type):

> **New _variant_ (not just a new field): add its vector first.** A new field on
> an already-vectored type is self-enforcing — the Rust struct literal in
> `golden_vectors.rs` won't compile until you supply the new field, so it flows
> into the vectors automatically. A brand-new **variant** has no such forcing
> function: `regenerate_vectors` only re-serializes the instances already listed,
> so you must first add a `vec_of("TypeName_NewVariant", …)` call in the matching
> `*_vectors()` function (and, if the extension uses that type, its
> `reconstruct*`/partition entry) — *then* regenerate. Otherwise the guard stays
> silently blind to the new variant on both sides.

```sh
cargo test -p codypendent-protocol --test golden_vectors regenerate_vectors -- --ignored
```

Then:

1. Review the diff under `protocol-vectors/` — it should show exactly the
   change you made (a new key, a new field on an existing entry, ...).
2. If the change is one the VS Code extension needs to know about (it sends or
   reads the affected type), update `extensions/vscode/src/protocol/types.ts`
   and the corresponding case in
   `extensions/vscode/test/protocol-vectors.test.ts` in the same commit.
3. Run `cargo test -p codypendent-protocol --test golden_vectors` (the two
   non-ignored checks) and, from `extensions/vscode/`, `npm test` — both must
   be green.
4. Commit the regenerated `protocol-vectors/*.json` files alongside the code
   change.

## Scope

The Rust generator enumerates comprehensively: every `CommandBody` variant,
every `Payload` variant, the nested `PromotionAction` enum, and the newer
`blackboard.rs`/`workflow.rs`/`capabilities.rs`/`input.rs` modules — this
protects the Rust wire format on its own merits, independent of what the
extension uses.

The TypeScript test only checks the subset the extension actually types.
Known, intentional gaps (not drift — the extension simply does not model these
yet):

- `document.json`, `blackboard.json`, `workflow.json`, `input.json` — the
  extension does not subscribe to `Document`/`Blackboard`/`Workflow` streams
  and has no `InputEnvelope` capture path, so it has no TypeScript type for
  these at all.
- `CommandBody`: only the 9 variants the extension actually sends
  (`AttachSession`, `SubmitUserInput`, `StartRun`, `ResolveApproval`,
  `CancelRun`, `PauseRun`, `ResumeRun`, `QueueSteering`, `UpdateIdeContext`) are
  checked. The other ~16 (workflow lifecycle, promotion, document, blackboard
  read commands) are Rust-only client-to-daemon commands the extension never
  issues.
- `Payload`: the 12 variants the extension's `Payload` union names explicitly
  are checked field-by-field; the rest fall through the union's permissive
  `{ type: string; [key: string]: unknown }` catch-all member (proving they at
  least parse and carry a `type` tag, matching the extension's actual
  forward-compatible handling — it ignores payload types it does not
  recognize).
- `ProposedAction`: `PublishDocument`, `BlackboardPost`, and `BlackboardQuery`
  are not modeled — they only ever appear on a workflow run's tool activity,
  which the extension does not subscribe to.
- `Subscription`: `Document`, `Blackboard`, and `Workflow` are not modeled, for
  the same reason.

The TypeScript test enforces this partition explicitly (a completeness check
per family) so a future Rust vector that is not accounted for on either side —
covered, or in one of the documented "not modeled" lists above — fails loudly
instead of silently falling through a gap.

A full generated TypeScript SDK / JSON-Schema pipeline remains the more
complete future direction (named in the 2026-07-21 project review and
ROADMAP.md); these vectors are the pragmatic guard in the meantime.
