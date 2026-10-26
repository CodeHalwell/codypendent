# Code navigation and bounded agent traversal

Date: 19 October 2026.

## Behaviour

Code graph opens in a function explorer: callers on the left, the selected definition in the centre and calls on the right. Structure view retains files, classes, types and other recorded relationships. Both use the existing live physics, pan, zoom and searchable list.

Selecting a symbol automatically requests its immediate relationships with endpoint metadata. Twelve completed neighbourhoods are retained while another selection loads; a failed read keeps the selection and exposes Retry. Definition and recorded call-location links open the original repository at verified source lines, independent of the active chat's run worktree. Locations are omitted when the stored file hash or byte length does not match the current file.

Explicit Python, Rust and TypeScript imports resolve cross-file calls and imports. Supported aliases, namespaces, inheritance, implementations and declared parameter/return types contribute recorded links. Ambiguous, shadowed, wildcard and dynamic bindings are not guessed. Source scanner evidence remains distinct from complete runtime knowledge. The current edge store retains one recorded call site per caller/callee relationship.

When a definition is added, replaced or deleted, the existing watcher captures affected importers before updating it. Unchanged callers are refreshed through a coalescing queue, at most 64 per cycle; remaining work continues without another filesystem event. Full scans resolve imports after definitions have been folded, avoiding file-order dependence.

## Agent context and tools

New context contains short graph-tool guidance, not a repository map. Replay removes legacy repository-map sections before the existing context-size bound. Retrieved tool cards, skills, memories and learnings retain their existing handling.

The existing graph query seam now exposes:

- `graph.find_symbols`: name search with stable IDs.
- `graph.definition`: source definitions and verified locations.
- `graph.callers_of` / `graph.callees_of`: direct recorded calls.
- `graph.neighbors`: direction, relationship and bounded-depth traversal.
- `graph.dependencies`: imports and dependency traversal.

Results default to 10 and stop at 40 per page, with pagination and explicit partial-result metadata. Depth is limited to five, traversal to 512 nodes and 64 expansions, and rendered output to its context budget. Repository identity comes from the durable run context. Foreign-repository endpoints cannot escape into results.

The six graph reads remain advertised when relevance selection narrows other tools. ACP agents use the existing private MCP bridge with the same query policy, cancellation and event path. Ask mode needs no renderer lease. Shell, graph writes and desktop writes retain their existing gates. Chat tool cards use readable action names with transport details collapsed.

## Focused verification

- Import resolution: 8 real SQLite fixtures passed, including cross-file aliases, shadowing/ambiguity, two-pass scanning and importer pagination.
- Traversal: 3 real SQLite fixtures passed, including pagination, cross-file callers, repository isolation and a 530-caller hub.
- Runtime: 6 unique focused cases passed, including advertisement, durable repository identity and private MCP reads without a renderer; forbidden writes remained denied.
- Daemon graph projection: 22 focused cases passed for source verification, Unicode/CRLF positions, complete endpoint metadata and paging.
- New-run and continuation model-driver checks passed with graph guidance and no graph contents. Legacy oversized-map replay passed.
- Protocol additions, golden vectors and SDK typecheck/build passed.
- Watcher: 5 focused cases passed, including real filesystem changes, 67 unchanged importers draining after one event and an edit arriving during queued refresh.
- Desktop: 9 graph interaction tests, 5 source-navigation tests and 7 transcript-label tests passed; the preceding layout/physics/query checks also passed. TypeScript and production frontend/native/CLI builds passed.

## Installed acceptance

The updated `/Applications/Codypendent.app` passed self-contained bundle verification and automatically reconnected after an idle daemon restart. The original executables are preserved in the evidence directory. Signing and notarisation were not assessed.

Rebuilding the original `jev-investigation` project folded 197 Python files into 11,152 symbols and 19,905 relationships, including 661 cross-file call edges to local definitions. Resolved placeholder removal accounts for the lower symbol count compared with the earlier graph.

The native function explorer automatically loaded `question_labels_and_descriptions`, its five callers and eight calls. Opening its definition launched VS Code at `src/yev/primitives.py:400`; opening its `combined_answer` call launched `src/yev/client.py:297`. The editor's accessible state confirmed each path and cursor line. Following `combined_answer` retained the selection and automatically loaded its own relationships. Structure view and its relationship filter remained available.

An actual installed `acp/codex-acp#gpt-6.1-sol` run completed in 26.47 seconds in Ask mode without a renderer lease for that session. Its private MCP calls to `graph.find_symbols`, `graph.definition` and `graph.callers_of` all succeeded. The native chat displayed readable action cards with technical details collapsed and a prose answer; no raw JSON was visible. The acceptance chat was renamed Graph traversal check and archived. The installed app remains connected on Code graph.

Focused formatting checks passed. The workspace-wide formatting check also reports existing unrelated changes in the TUI and session-library test; those files were left unchanged.

Installation and native acceptance evidence is recorded under `target/code-navigation-2026-10-19/`.
