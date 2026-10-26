# Monty generated helpers — 19 October 2026

Codypendent now supports Python generated helpers through the released Monty 1.2.0 native worker. Existing JavaScript helpers and interactive HTML results retain their current sandbox.

The upstream reference was read before implementation: [repository](https://github.com/pydantic/monty), [current Rust API](https://pydantic.dev/docs/monty/quickstart/rust/), [security model](https://pydantic.dev/docs/monty/concepts/security/) and [resource limits](https://pydantic.dev/docs/monty/concepts/resource-limits/). The Rust documentation recommends subprocess pools for untrusted programs because interpreter aborts must not terminate the parent app. Codypendent therefore uses `monty-pool`, rather than linking the interpreter into the app.

The official [1.2.0 runtime release](https://pypi.org/project/pydantic-monty-runtime/1.2.0/) supplies a standalone worker. On this Apple Silicon machine the verified wheel is 14,514,981 bytes and its executable is 28,705,264 bytes. Its strict macOS signature check succeeds and `--version` reports `monty-runtime 1.2.0`. Python installation is unnecessary. The native desktop requires Rust 1.96; the shared libraries and CLI retain their existing Rust requirement.

## Tool contract

`desktop.defineTool` accepts optional `language: "python"`. Its `functionBody` is a Python function body; `args` is the input dictionary and the body returns a JSON value. `host_call(name, args)` reaches declared workspace capabilities. Omitting language retains JavaScript compatibility.

```json
{
  "name": "desktop.custom.workspaceSummary",
  "description": "Read the current workspace title",
  "language": "python",
  "parameters": {"type": "object", "properties": {}, "additionalProperties": false},
  "functionBody": "workspace = host_call('desktop.getWorkspace', {})\nreturn {'title': workspace['title']}",
  "capabilities": ["desktop.getWorkspace"]
}
```

Native execution reads the authoritative claimed pending request. The saved source, language, version, argument schema and capability list must still match the approved definition. Existing tool and per-capability policy/approval gates remain in place. The native host validates every requested capability and its arguments before forwarding it to the desktop. Changing chats cancels execution.

Workers receive no filesystem mounts and the upstream pool clears their environment. OS calls and unknown names receive no handler. Limits are 32 MiB allocator memory, 500 ms execution per feed, 250 ms per uninterrupted turn, a two-second parent deadline per turn, five seconds total wall time, 32 host calls, 64 suspensions, 64 recursion levels and 256 KiB JSON/output. Failed execution discards its checkout. The pool permits two workers.

## Packaging and evidence

`prepare-monty-runtime.py` verifies the pinned release wheel checksum, extracts its exact executable and prepares the Tauri sidecar. Tauri dev/build hooks prepare it automatically. Apple desktop staging, signature checks and signing include `monty`; the MIT notice is bundled. CLI archives do not acquire a Monty requirement.

Focused shared contract checks passed 2/2. Four actual native-worker checks passed in 2.73 seconds: workspace host-call/JSON roundtrip, undeclared capability denial, host file/environment/process/network denial, and CPU/memory/output/host-call limits with recovery. The observed CPU limit stopped execution at 250.1725 ms; a 64 MiB allocation was denied against the 32 MiB limit. Oversized JSON, 512 KiB stdout and the 33rd host call were rejected, and a fresh helper returned 42 after each failure. Evidence is recorded in `target/monty-assessment-2026-10-19/evidence.json`. Model-driven native webview acceptance passed in the rebuilt self-contained Tauri app: DeepSeek V4.1 Flash created and saved a Python helper, executed it through Monty, read the real workspace through its declared capability and returned the actual repository plus 42. Run `01a15402-5a74-7a83-8983-0dc83947e2a0` completed in 67 seconds. Definition, execution and context access displayed compact approvals; source and policy text stayed under closed disclosures. The final answer was plain prose. The entire Your workspace section also remained collapsed across page changes and a native restart. Screenshots and accessibility evidence are under `target/monty-assessment-2026-10-19/proof/`.

These helpers use Monty's Python subset and supported modules. HTML/CSS/JavaScript presentation continues to use the browser sandbox.
