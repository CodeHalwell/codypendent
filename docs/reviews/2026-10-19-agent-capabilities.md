# Codypendent feature and agent capability inventory

Checked against `Navigation.NAV_GROUPS`, `App` bindings, the frontend host-tool registry, the runtime schema catalogue and the native/ACP run assemblies on 19 October 2026. This covers all 23 desktop pages and the agent features behind them. A page being available does not make its controls callable by an agent.

## Desktop pages

Every page can be discovered through `desktop.listFeatures` and opened through `desktop.navigate` when desktop tools are permitted and the renderer connection is live. The navigation schema is derived from the same table as the sidebar and command palette; the feature recipes are checked exhaustively against `DesktopView`.

| Page (view token) | Agent tools and recipe | Controls retained by the user |
| --- | --- | --- |
| Chat (`sessions`) | `desktop.getWorkspace`; read/prepare an unsent draft with `desktop.readDraft`, `desktop.setDraft(expectedDraft)`; focus the composer. Agent runs keep the existing conversation and retrieved context. | Send a message, start/select a chat, queue/steer follow-ups, pause/resume/cancel, decide approval and question cards. No attachment UI is implemented here. |
| All chats (`library`) | `desktop.searchChats(query)` returns the bounded first page and continuation cursor. | Open, rename, pin/archive/delete and export conversations through their existing controls. The host search tool currently has no continuation argument. |
| Inbox (`inbox`) | Navigation only. `user.ask` can create a real question when the channel is available. | Read/manage notifications, answer questions and approve/reject requests. Agents cannot approve themselves. |
| Usage (`analytics`) | Navigation only. Agents can report measured usage returned by their actual tools; no analytics-query tool is exposed. | Inspect usage, run duration, measured cost and outcomes. |
| Context (`context`) | `desktop.getWorkspace` gives the active workspace snapshot; context manifests supply bounded retrieved evidence automatically. | Inspect the actual token/context breakdown. Agents cannot change this view's budgets through navigation. |
| Checkpoints (`backtrack`) | Navigation only; no agent checkpoint-restore tool. | Inspect checkpoints, fork and restore a selected point. |
| Documents (`docs`) | `desktop.listDocuments`, then `docs.read`; `docs.create`, `docs.edit`, `docs.suggest` use actual document/block IDs and report applied versus proposed changes. | Inspect collaboration mode, review suggestions and publish. The desktop does not provide a collaboration-mode change control. |
| Skills (`skills`) | `desktop.listSkills`; `skills.search(query)` ranks tool/skill/command cards and `skills.search(query, open)` opens a selected procedure. | Inspect registered skills and declared permissions. Discovery does not install a package or grant permissions. |
| Memory (`memory`) | `desktop.listMemory`; `memory.remember` records one durable fact/decision as a memory proposal. Retrieved memories are evidence to verify. | Inspect/correct/forget curated memory; review/promote/edit/retire/delete learnings. |
| Extensions (`plugins`) | Navigation only; no extension trust/install tool is exposed to the agent. | Refresh, smoke-test, enable at a selected scope, approve/reject a permission update, revoke. Remote plugin surfaces are not rendered in this desktop build. |
| Agent tools (`agentTools`) | `desktop.manageTools(operation=list)`; define/update a `desktop.custom` helper with `desktop.defineTool`; execute with `desktop.executeTool(name,args)`. | Inspect source/schema/version/capabilities and decide the native approval. |
| Workflows (`workflow`) | `workflow.query` lists recent runs, then reads a returned durable run ID; `workflow.create` saves a definition; `workflow.run` starts it and returns the run to inspect. | Inspect workflow definitions and node progress. Creating is distinct from executing. |
| Task Board (`board`) | `task.list`, then `task.create`, `task.update` or `task.move` with real card IDs. | Organise backlog cards and inspect status, assignees and history. |
| Shared results (`blackboard`) | `blackboard.query` and `blackboard.post` only when a workflow-node context and channel are wired. | Inspect shared workflow evidence, confidence, provenance and questions. Ordinary ACP chat does not pretend to own a workflow blackboard. |
| Repository (`repository`) | `desktop.getWorkspace`, scoped `workspace.search`/`workspace.read_file`; `desktop.openFile(path,line)` opens source in the authorised run checkout. | Choose the checkout; an agent cannot widen its scope by supplying a different repository path. |
| Councils (`council`) | Inspect actual model IDs using `desktop.listModels`; `council.create` defines members/chair; `council.run` convenes the requested objective; `council.result` reads a persisted outcome. | Create/inspect/start/delete saved councils. There is no edit-existing-definition control. |
| Council Results (`councilResults`) | `council.result(selector)` reads a real result ID or council name, including member outcomes and chair synthesis. | Inspect the saved reports and evidence. |
| Models (`models`) | `desktop.listModels` reads configured profiles and the default. | Discover/test/manage profiles and choose the next run's model. No automatic agent model-switch tool is exposed. |
| Providers (`providers`) | `desktop.listProviders` reads configured providers and installed adapters without credentials. | Inspect readiness, connect and edit providers; complete genuinely missing authentication/installation. |
| API Keys (`keys`) | Navigation only; no tool reads or writes secret values. | Set/remove credentials. Agents must not request or print an already available secret. |
| Mode (`mode`) | `plan_enter`/`plan_exit` provide the real planning hand-off when the relevant question/plan channels and modes permit them. | Choose the next submission's mode. Navigation does not switch the current run's permissions. |
| Code Graph (`edges`) | `graph.find_symbols` → stable ID → `graph.definition`, `graph.callers_of`, `graph.callees_of`; follow bounded `graph.neighbors`/`graph.dependencies`. `graph.blast_radius` and `graph.tests_covering` inspect impact. `graph.assert_edge`, when wired, records sourced agent assertions without displacing resolved facts. | Visual graph search/filter, selection/expansion, physics, pan/zoom and source opening. The complete graph is never dumped into model context. |
| Get Started (`onboarding`) | `desktop.listModels`, `desktop.listProviders`, `desktop.getWorkspace` inspect the available configuration. | Choose a repository and complete only missing prerequisites. |

## Other agent feature families

- **Code changes and verification:** native tools include bounded file reads/search, targeted edits/file writes, patch/diff, structured shell execution, interactive process sessions and the detected repository test command. Their schemas, current mode and policy determine availability. Installed ACP agents own their coding loop; the private application MCP bridge provides Codypendent's scoped read/search rather than a second unrestricted shell or file writer.
- **GitHub and connected integrations:** the native runtime exposes the wired GitHub and operator-configured MCP tools and applies retrieval narrowing to their advertisement. These require their actual configured connection and native policy. ACP agents receive forwardable operator-declared MCP server launch specs; Codypendent does not pretend to control the external agent's own built-in tool selection.
- **Interactive answers / AG-UI:** the native agent adapter turns the real lifecycle, text, tool, result, approval and question events into AG-UI. `generateSandboxedUi` renders isolated HTML/CSS/JS widgets. Source and JSON remain in the tool arguments/results, with a short human-facing explanation in chat. A successful acknowledgement is not a request to rebuild the widget.
- **Reusable helper tools:** Python helpers run in the bundled Monty subprocess with memory/time limits and JSON arguments/results. They can call only their declared connected desktop host capabilities through `host_call`; JavaScript helpers retain the bounded worker bridge through `host.call`. Neither has direct native, filesystem, shell or network access. Stable `desktop.executeTool` supports clients that cannot refresh dynamic tool lists.
- **Approvals and run controls:** Codypendent owns approval decisions, scoped grants, cancellation, pause/resume, the durable event ledger, isolated run checkouts and reviewable changes. Agents wait for the actual result; page navigation and a discovery card never grant authority.
- **Saved/folded tool artefacts:** `artifact.read` has a tested runtime seam but no production reader is wired in the current run assemblies. This inventory does not claim it is callable. The desktop can display/save actual tool-result artefacts through its existing controls.

## Prompt and discovery changes

The native model's system message now combines the existing workspace instruction files with shared Codypendent behaviour and concise recipes selected from the actual advertised tool schemas. This replaces the previous two-line generic coding-agent instruction. The same system-guide content is counted in context estimates and the Context view's system-token breakdown.

The ACP application's private MCP server now exposes the actual policy-filtered application tool families: workspace read/search, registry retrieval, graph reads/assertions, documents, workflows, repository task cards, councils, memory proposals, questions/planning and live desktop tools. Its schemas are independent of a top-k selection made for the initial objective, so later discovery cannot strand a supported application tool. Unwired tools stay absent. Ask/Explore still exclude frontend `desktop.*` actions and the widget renderer, as before; repository writes and shell commands remain absent from this private bridge in every mode.

Every ACP run receives its current mode, authorised read/search root, durable repository identity, actual application recipes and bounded desktop snapshot in the initial application instructions, and the MCP initialise response supplies the same current guidance. Dotted names in the guide are explicitly mapped to the advertised encoded MCP name rather than guessed prefixes. Build-mode agents can request the complete feature catalogue with `desktop.listFeatures` instead of receiving all page details in every prompt.

Search guidance prefers Codypendent's bounded `workspace.search`. A transient interrupted read-only search may have one identical retry; an automatic retry already reported by the tool counts. A result reporting two attempts ends the retry budget. Invalid patterns, denials, cancellations, side effects and complete agent runs are never replayed by this rule.

## Semantic tool and skill retrieval

The existing registry has hybrid retrieval: lexical/exact signals plus vector similarity, hard repository/user/system visibility and risk filters, and bounded cards/procedure disclosure. `PoolRegistrySearch` uses the same `semantic_indexes` and retrieval funnel as initial context assembly. The default embedding model is now bundled EmbeddingGemma 2 with llama.cpp; an explicit embedding provider remains an override. Token hashing is the bounded fallback during warm-up or failure. See [built-in semantic retrieval](2026-10-20-local-semantic-retrieval.md). Native runs already had this connection; ACP's private application MCP now wires the same `skills.search` seam.

`skills.search` is discovery, not installation or a permission change. A skill's procedure remains bounded and evidence-framed. The private MCP schemas expose supported application tools regardless of initial objective ranking; external agent built-ins and arbitrary disconnected integrations do not become callable merely because a registry card names them.

## Validation

Focused checks passed: two shared-guide tests, six provider-message tests, four private-MCP tests, four compaction tests, the request-token estimate test, the live ContextUsage event test, two frontend feature-catalogue tests and the frontend type check. The private-MCP checks execute real scoped file reads and skill procedure discovery through the socket, retain Ask mode and approval denials, preserve failed document/missing council results and accept successful content beginning `desktop.`. Native installed-agent acceptance is recorded separately by the integrating agent; these source checks alone do not establish installed application readiness.

## Installed acceptance

The packaged application and matching CLI were rebuilt and installed in `/Applications/Codypendent.app`; the self-contained bundle check passed. The actual installed `acp/codex-acp#gpt-6-luna` connection executed the scoped workspace search, registry/skill discovery, workflow query and repository task listing against a disposable acceptance repository. No tasks, workflows or repository files were created by the agent. The returned skill cards included graph navigation tools.

A second real run deliberately submitted one invalid regular expression, then a valid search. Both the native tool journal and the external ACP completion retained `unclosed character class` and exit status 2, and the valid query found `README.md:3`. There was one failed search invocation and one successful search invocation; the activity archive also records their ACP and guardian wrappers. Native expansion displayed the actual error and restored compact history afterwards. The app restarted and reconnected automatically to the updated daemon with zero active runs.

This initial acceptance preceded the bundled semantic implementation. Default registry and native tool retrieval now share the built-in model, including MCP narrowing and later schema discovery; the newer acceptance is recorded in [built-in semantic retrieval](2026-10-20-local-semantic-retrieval.md).

Evidence, installed executable hashes, the complete real-agent event streams and the native failure screenshot are under `target/agent-features-2026-10-19/`. Rust compilation caches were removed after the final build, preserving installed binaries, the source bundle and acceptance evidence; measured disk free space was 99.87 GiB.
