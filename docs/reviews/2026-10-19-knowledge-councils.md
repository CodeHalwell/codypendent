# Code knowledge graph and councils

Date: 19 October 2026.

## Code graph

The former edges view now presents real repository symbols and directed relationships as an interactive SVG knowledge graph. Dragging, wheel/button/keyboard zoom, Fit, relationship filtering, symbol inspection, immediate-neighbour focus and bounded neighbourhood expansion share the actual daemon graph. A searchable list provides an alternative to the map. Source paths, relationship evidence and technical IDs remain available in the inspector.

The default repository query loads at most 100 nodes and 100 edges. The visual projection is limited to 240 nodes and 400 edges; it reports partial results and offers repository search or the list rather than inventing additional symbols. Up to six neighbourhood queries can extend a view. Late requests cannot replace a newer repository or query.

Build code map and Update map invoke the existing daemon scan through the selected repository's native controller connection. The build request has a dedicated 300-second reply deadline, an in-flight duplicate guard and the existing daemon authorisation. Other requests keep their existing deadlines. Scan failures and unavailable overview data remain distinct from an empty graph.

Packaged native acceptance built a real four-file Rust repository from the empty state and displayed 44 symbols and 56 connections. This exposed a short-window clipping defect, fixed by placing the graph body in the existing scroll container. A second native check found an SVG/grid sizing cycle; a definite stage height and inset SVG now keep Fit inside the drawing area. Final native checks passed at 1280 × 840 and 1568 × 1101: page scrolling, 100% → 125% → Fit, actual symbol inspection and a successful neighbourhood query. The list's symbol search also passed in the preceding native build.

The sidebar shares the outer shell surface. The main workspace has an eight-pixel inset, rounded corners and a subtle outline; the former full-height sidebar rule is removed. The narrow overlay retains its existing edge-to-edge layout.

The final installed app also loaded the original `jev-investigation` repository graph: 12,012 symbols, 19,882 connections and 197 files. A native search for an actual loaded `YevError` symbol produced 35 symbols and 26 relationships. List selection inspected the real Python type in `src/yev/errors.py`; its neighbourhood request and Focus/Show all controls passed. No source edits or new scan were needed in that repository.

## Councils

Council cards now show a readable team name, purpose, member perspectives and a distinct chair. The primary action starts the council; raw IDs, quorum and deletion controls sit under Council settings. Search and Saved results are available from the collection.

The builder accepts a natural name, produces the existing portable council ID, uses readable model choices while preserving canonical values, prevents duplicate member models and previews the selected team. Roles and extra rounds remain optional disclosures. Results prioritise the chair's Markdown answer, real run progress and partial failures; individual reports and technical details are expandable.

The app retains council questions, builder drafts and active-run state across navigation. The run guard lives above the results page. Returning from Models refreshes choices, and child routes use normal navigation history.

Native acceptance created Graph Review using the actually configured DeepSeek V4.1 Flash cloud and GLM-5.2 cloud models, with DeepSeek as chair. One real discussion completed in 65 seconds. The question and live progress survived leaving for Chat and returning to Council results; the start action stayed disabled during the live run. The durable result contains both members, the chair and an independent reviewer. This exposed a cost projection that omitted the reviewer; the corrected projection includes its recorded usage and presents the independent review in a closed disclosure. Successful live completion folds the question into Ask another question. The chair/member notice uses the readable model label, retaining its original text in Report details.

After the reporting fix, native restoration displayed 26,988 measured tokens across 4/4 attributed runs. The actual independent review and both member responses opened correctly, and raw chair coordinates stayed inside Report details. The saved JSON was preserved; the loader derives corrected totals from the recorded outcomes.

Daniel approved replacing Code Wizards' retired `acp/claude-acp#opus[1m]` member and chair with the discovered `acp/claude-acp#opus` profile. The original configuration was backed up, and all other fields were retained. The installed council page visibly shows the current Opus selection. A separate real exact-selector CLI probe returned `OPUS CONNECTED` in 16.85 seconds without tools or a substituted model.

## Verification

- Graph layout, interaction and existing plot checks: 16 passed across four focused test files before the final query-recovery corrections.
- Final EdgesView query retry and retained filter choices: 6 passed; TypeScript passed.
- Council navigation, presentation and existing load-loop checks: 22 passed before the final result polish.
- Final council presentation/navigation checks, including completed-question disclosure, readable notice and independent review: 17 passed; TypeScript passed.
- Council review aggregation/restoration and native saved/fallback projection: 3 focused Rust regressions passed.
- Known slow-agent metadata deadline regression: 1 passed. Claude and Gemini have a 45-second background budget; other agents retain eight seconds and runtime deadlines are unchanged.
- Theme token checks: 2 passed; changed stylesheets parsed and referenced defined tokens.
- Production frontend and native desktop builds passed. The matching CLI, desktop worker and Monty worker were checked as a self-contained app bundle.
- Final native graph, saved review/cost presentation and installed council checks passed. Screenshots are under `target/knowledge-council-2026-10-19/proof/`.

The final `/Applications/Codypendent.app` contains matching desktop, CLI and worker executables, passed self-contained bundle verification, and reconnected to the original repository after an idle-guard daemon restart. The previous full installation and council configuration are preserved under the evidence directory. The isolated acceptance app and daemon were stopped; the installed app was left connected on Councils.

Agent connection results are recorded separately in [authentication recovery](2026-10-19-auth-recovery.md). Test evidence and installation hashes are under `target/knowledge-council-2026-10-19/`.

## Graph refinement and physics

The map now packs real connected components into readable neighbourhoods, uses collision-checked symbol cards and clearer directed curves, and keeps source details in a floating inspector on wide windows. The guide stays mounted so selecting a symbol cannot shrink the page during a drag. Nodes can be dragged individually; spring forces, local repulsion and damping move their real neighbours. Pause, resume, reset and reduced-motion behaviour are supported. Zoomed dragging uses the actual SVG viewport transform; a second pointer cannot take over an active drag. Expanding a neighbourhood preserves existing positions and the camera, and resizing recentres the current arrangement.

The old initial query independently paged 100 symbols and 100 relationships, producing 165 displayed symbols, including 100 with no loaded links. The desktop now opts into `connected_view`: the daemon starts at a connected application file, follows recorded relationships, hydrates every endpoint and stops at the component or page budget. It never fills the page with unrelated symbols. Explicit filters and the existing independent paging contract retain their previous behaviour. The CLI exposes the same read with `graph show --connected`.

The actual installed daemon returned 77 fully described symbols and 100 connections around `src/yev/investigation/hard_questions.py` in 1.208 seconds. A reachability check confirmed one 77-node component and zero unconnected symbols, while repository totals remained 12,012 symbols and 19,882 connections. This is a connected neighbourhood of recorded relationships; the syntax scanner still does not resolve arbitrary cross-file calls semantically.

Final focused checks passed: 39 desktop graph, query, physics and theme tests; five daemon graph query tests; and 12 protocol/vector compatibility checks. Frontend, native desktop and CLI builds passed, and the updated installed bundle passed self-contained verification. Native checks exercised node dragging while paused, resume with responding neighbours, dragging at 125% zoom, real metadata expansion, wide and short-window layouts, reset and final connected-view inspection. Evidence, returned graph data, executable hashes and screenshots are under `target/graph-polish-2026-10-19/`. The separate test window was closed; the installed app was left connected to the original repository on Code graph.
