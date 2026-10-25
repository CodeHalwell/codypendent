# Built-in semantic retrieval and targeted performance work

## Packaged inference

Codypendent ships EmbeddingGemma 2 Q8_0 text weights with a pinned llama.cpp runtime and its libraries. The application discovers its own resources, verifies the model/server checksums and starts an owned embedding-only process on an authenticated random loopback port. No Ollama, account, environment setting, first-launch download or external inference service is needed. Local requests explicitly bypass proxy settings. An explicit existing embedding-provider configuration remains an override.

The model is `ggml-org/embeddinggemma-2-GGUF` at revision `58faf90940886df2d62be555c8765d00dac362c5`; the 309,855,456-byte Q8_0 model has SHA256 `2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135`. The runtime is the official llama.cpp `b11669` build, corresponding to 0.7.0. Model/runtime downloads are build preparation, with pinned size/hash verification. Tauri resources, CLI archives, installer, Apple staging and signing include the runtime/model/licences and verify the final bytes. The Mac runtime requires macOS 13.3, reflected by the desktop package metadata.

Current `llama-cpp-2` Rust bindings were investigated but their vendored engine did not yet recognise `gemma-embedding2`. The compatible current llama.cpp runtime is therefore managed by Codypendent's Rust retrieval layer rather than those outdated bindings. Its listener accepts only authenticated loopback embedding requests, and the supervisor stops its owned process when dropped.

## Retrieval

Native initial context, tool selection and `skills.search` share the semantic embedder. Built-in and MCP selection run concurrently under a two-second bound; complete lexical/hash fallback remains available during warm-up or failure. Live MCP tools can be discovered later and their exact selected input schema opened. Discovery respects the existing run-mode/tool availability and does not grant permissions. Skill procedures retain bounded disclosure and package validation.

Ordinary ecosystem `SKILL.md` packages can now be opened without `skill.toml`, using the same pinned-directory snapshot and registered content-hash checks. Changed contents, mismatched identity and symlink substitutions are refused. Packages removed from disk stop ranking in searches and cached context without changing their registry records.

EmbeddingGemma queries use `task: search result | query: ...`; documents use `title: none | text: ...`. Persisted vectors are identified by model digest, runtime revision and prompt format, independent of temporary ports/tokens. Counts, finite/nonzero values and dimensions are checked before writes. Fresh reads load only relevant IDs and embedding space. Visibility, risk, trust, status and scope overrides are applied before dense, lexical, exact and history candidate limits, so inaccessible items cannot crowd out authorised capabilities.

Startup/outbox indexing batches documents and coalesces duplicate changes. A 65-item persistence fixture now uses two embedding batches instead of 65; a current index makes zero model calls. The bundled runtime further chunks document requests to 16 and yields between chunks so a new query can run during indexing. Background warm-up retries promptly without holding the daemon socket closed. The in-memory cache is bounded, handles duplicate inputs in linear time and lets cached queries bypass a cold indexing request.

Run preparation schedules repository graph indexing in the background instead of waiting for a full scan. Existing revision guards, writer locks, watchers and documentation checks remain in use. Native and ACP graph tools return an actionable indexing status until the current revision is ready, then delegate to normal traversal; an unfinished index is never reported as an empty search result.

## Frontend

Graph animation reuses topology signatures between frames, avoids square roots for distant unrelated nodes and reuses squared distances. Cancellation history is scanned only when its dialog opens. Existing transcript paging remains intact.

For 240 nodes/400 edges over 480 physics frames, the median of seven warmed samples improved from 692.54 to 190.57 ms for one component (3.63x), and from 753.83 to 153.95 ms for 30 components (4.90x). Coordinates differed by at most 1.94e-11 pixels after 180 frames. These measurements cover force calculation, not total application speed.

## Validation

- Thirteen knowledge persistence tests passed, including batching, changed spaces/dimensions, malformed outputs and outbox retry retention.
- Thirteen focused runtime embedding tests and eight embedding integration tests passed. The latter included the real bundled model under deliberately broken proxy settings: 768-dimensional vectors, correct checkpoint paraphrase ranking and a 31-microsecond cached query. The final cold readiness plus queries check took 18.77 seconds during concurrent builds.
- Independent staged runtime acceptance passed with only the system PATH: cold readiness 10.78 seconds, four vectors in 0.529 seconds and unauthenticated requests rejected with HTTP 401. The owned process was terminated and reaped.
- Thirty frontend tests, TypeScript and the production frontend build passed. Fourteen packaging/installer tests, Python lint/complexity checks, shell syntax and workflow invariants passed.
- Semantic selector tests cover paraphrases, invalid vectors, timeout/fallback, stable identity, schema bounds, mode filtering and discovery after top-k narrowing. Seven daemon retrieval tests passed.
- Two graph-preparation regressions passed: an Ask run progresses while the repository scan lock is held and later returns a real caller; moved revisions rescan while unchanged revisions reuse the existing graph. Execution times were 4.97 and 4.02 seconds.
- The ecosystem Markdown skill regression covers secure opening, changed assets, identity mismatch, symlink substitution and missing-package filtering in searches and cached context. It passed alongside six existing daemon retrieval cases (7/7).

Evidence: `target/semantic-2026-10-19/`, `target/frontend-performance-2026-10-19/` and the focused test outputs.

## Installed acceptance

Both local application bundles now contain the rebuilt native desktop, matching CLI and embedding resources; self-contained verification passed. The installed daemon automatically populated 907 vectors of dimension 768 with the bundled model identity, with no embedding-provider override configured.

A real installed Codex run searched the registry and successfully opened `azure-resource-visualizer`, reporting its actual resource inventory, relationship and Mermaid/report procedure. Four native skill-search calls completed with no tool failures, and no cloud procedure ran. Preparation reached `Preparing` in 0.66 seconds, compared with 191 seconds in the earlier graph-blocked run; the complete model interaction took 76.11 seconds. Native checks showed the answer with one collapsed Activity archive and no raw JSON, and verified graph physics pause/resume with callers, definitions and call locations visible.

Three retrieval evaluation tests and the repository scope-shadowing regression also passed. A further native relaunch automatically started and connected to its own daemon; the previous daemon and owned embedding process had both stopped. The first acceptance failure and final successful run remain separate evidence files. See `target/semantic-2026-10-19/installed-acceptance.json`, `installed-agent-final.jsonl`, `native-semantic.png` and `native-graph.png`.

Final project compilation-cache cleanup recovered 18,102,849,536 bytes of actual disk space (16.86 GiB), preserving installed/build executables, app bundles, model/runtime resources and acceptance evidence. Receipt: `target/semantic-2026-10-19/rust-cache-cleanup.json`.
