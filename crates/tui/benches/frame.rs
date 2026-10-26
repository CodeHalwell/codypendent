//! The per-frame and per-keystroke paths of the TUI shell.
//!
//! # Why these two, and not something else
//!
//! `render` and `reduce` are the only two functions the shell's event loop
//! calls per event (see the crate docs' loop diagram). Everything else in the
//! crate is reached THROUGH one of them, so instrumenting them covers the hot
//! surface without pinning a benchmark to a private helper that a refactor is
//! free to rename.
//!
//! # The regression this locks in
//!
//! The transcript measure pass used to re-measure the WHOLE transcript every
//! frame, twice (the build pass repeated the work), which is O(total graphemes)
//! with an allocation per row. It is now memoised per run behind
//! `TranscriptMeasureCache`, and the build pass materialises only the viewport.
//!
//! `frame/warm` is the steady state — the cache is hot, so the pass is one
//! content hash per run plus the viewport build. `frame/cold` renders a state
//! whose cache has never been filled, which is the work the old code did on
//! EVERY frame. The ratio between them, at a fixed corpus, IS the win; if a
//! future change reintroduces per-frame full measurement, `warm` converges on
//! `cold` and the regression is visible as a number rather than as a report of
//! sluggish scrolling.
//!
//! Note that `warm` is not free and is not expected to be: the cache-hit path
//! still hashes every run's rows each frame to decide the hit. That cost scales
//! with total transcript content, not with the viewport, so it is the next
//! thing that would bite on a very long session — which is precisely why it is
//! measured at two corpus sizes rather than one.
//!
//! # Determinism
//!
//! The corpus is built in-process from a fixed script of daemon events with
//! fixed identifiers and a fixed timestamp — no clock, no network, no files, no
//! global state. Every bench builds its own `AppState`; nothing is shared.

use chrono::{DateTime, TimeZone, Utc};
use codypendent_protocol::artifact::{ArtifactRef, DataClassification};
use codypendent_protocol::events::{Actor, EventBody, SessionEvent};
use codypendent_protocol::ids::ArtifactId;
use codypendent_protocol::ids::RunId;
use codypendent_protocol::run::{AgentMode, RunDisposition};
use codypendent_tui::{reduce, render, Action, AppState, Theme};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::hint::black_box;

/// A realistic terminal. 30 of the 40 rows reach the transcript once the
/// header, composer and footer take their share, so the viewport-bounded build
/// pass has real work to do without the window dominating the measurement.
const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

fn fixed_time() -> DateTime<Utc> {
    Utc.timestamp_opt(1_765_000_000, 0).single().expect("fixed")
}

/// A run id that depends only on its index, so the corpus is byte-identical
/// between runs of the bench (`RunId::new()` is a v7 UUID — it reads the clock).
fn run_id(i: usize) -> RunId {
    RunId(uuid::Uuid::from_u128(
        0x5eed_0000_0000_0000_0000_0000_0000_0000 + i as u128,
    ))
}

fn ev(body: EventBody) -> Action {
    Action::daemon_event(SessionEvent {
        sequence: 1,
        occurred_at: fixed_time(),
        causation_id: None,
        correlation_id: None,
        actor: Actor::System,
        body,
    })
}

/// Prose in four scripts. The transcript bug that motivated this harness only
/// reproduced with CJK and ZWJ emoji: wide glyphs make the wrapped row count
/// diverge from the byte and the char count, and a ZWJ sequence is several
/// codepoints that must measure as one cluster. A corpus of pure ASCII would
/// measure a grapheme walk that never has to do anything interesting.
const PROSE: &[&str] = &[
    "Refactored the ledger append so the sequence subquery uses the covering index.",
    "設定ファイルを読み込み、スキーマの検証を行いました。三件の警告があります。",
    "완료되었습니다 👨‍👩‍👧‍👦 — 모든 테스트가 통과했습니다 🎉 다음 단계로 진행합니다.",
    "Разбор завершён: 1 024 узла, 87 рёбер, 0 ошибок. Готово к следующему шагу.",
];

/// One run's worth of conversation: an objective, some streamed prose, a tool
/// that runs and finishes, a note, and a terminal marker. This is the shape the
/// renderer actually walks — a transcript of nothing but `Model` entries would
/// skip the tool-card and fold measurement entirely.
fn push_run(state: &mut AppState, i: usize) {
    let run = run_id(i);
    reduce(
        state,
        ev(EventBody::RunStarted {
            run_id: run,
            objective: format!("turn {i}: tighten the projection write path"),
            mode: AgentMode::Build,
        }),
    );
    for (k, line) in PROSE.iter().enumerate() {
        // Streamed in fragments, the way the daemon actually delivers it, so
        // the reducer's coalescing runs rather than being handed whole lines.
        for chunk in split_graphemes_roughly(line, 3 + k) {
            reduce(
                state,
                ev(EventBody::ModelStreamDelta {
                    run_id: run,
                    text: chunk,
                    thought: false,
                }),
            );
        }
    }
    reduce(
        state,
        ev(EventBody::ToolStarted {
            run_id: run,
            tool: "shell.run".to_owned(),
            args_digest: format!("{i:064x}"),
            label: Some("cargo test -p codypendent-daemon".to_owned()),
        }),
    );
    reduce(
        state,
        ev(EventBody::NoteAppended {
            run_id: Some(run),
            text: PROSE[i % PROSE.len()].to_owned(),
        }),
    );
    reduce(
        state,
        ev(EventBody::RunCompleted {
            run_id: run,
            disposition: RunDisposition::Completed {
                summary: Some("done".to_owned()),
            },
            chronicle: ArtifactRef {
                id: ArtifactId(uuid::Uuid::from_u128(
                    0xc0de_0000_0000_0000_0000_0000_0000_0000 + i as u128,
                )),
                media_type: "application/json".to_owned(),
                byte_length: 2_048,
                sha256: format!("{i:064x}"),
                sensitivity: DataClassification::Internal,
            },
        }),
    );
}

/// Split on char boundaries in small groups — a stand-in for the arbitrary
/// fragment boundaries a provider's token stream lands on. Deliberately splits
/// multi-codepoint clusters, because the real stream does too.
fn split_graphemes_roughly(s: &str, group: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars
        .chunks(group.max(1))
        .map(|c| c.iter().collect::<String>())
        .collect()
}

fn corpus(runs: usize) -> AppState {
    let mut state = AppState::new();
    for i in 0..runs {
        push_run(&mut state, i);
    }
    state
}

fn draw(terminal: &mut Terminal<TestBackend>, state: &AppState, theme: &Theme) {
    terminal
        .draw(|f| render(f, black_box(state), black_box(theme)))
        .expect("draw");
}

fn bench_frame(c: &mut Criterion) {
    let theme = Theme::dark();
    let mut group = c.benchmark_group("tui/frame");

    for runs in [64usize, 512] {
        // Never rendered, so its measure cache is empty. Cloning it gives a
        // guaranteed-cold state without needing access to the cache's innards.
        let cold_template = corpus(runs);

        // A separate state that IS rendered once up front, so the cache is hot
        // for every timed iteration below.
        let warm_state = corpus(runs);
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
        draw(&mut terminal, &warm_state, &theme);

        group.bench_function(format!("warm/{runs}_runs"), |b| {
            b.iter(|| draw(&mut terminal, &warm_state, &theme))
        });

        let mut cold_terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
        group.bench_function(format!("cold/{runs}_runs"), |b| {
            b.iter_batched_ref(
                || cold_template.clone(),
                |state| draw(&mut cold_terminal, state, &theme),
                // The corpus is large; one clone per timed iteration keeps peak
                // memory flat instead of holding a whole batch of them.
                BatchSize::PerIteration,
            )
        });
    }

    group.finish();
}

/// A keystroke. `reduce` is called once per key, so anything in here that
/// scales with the session is felt directly as input lag. `InputChar` fans out
/// to the composer edit, the `@`-mention popup check, and the session-library
/// sync — none of which are obviously O(1), which is the reason to measure
/// rather than assume.
fn bench_keystroke(c: &mut Criterion) {
    let mut group = c.benchmark_group("tui/reduce");

    for runs in [64usize, 512] {
        let template = corpus(runs);

        group.bench_function(format!("input_char/{runs}_runs"), |b| {
            b.iter_batched_ref(
                || template.clone(),
                |state| reduce(state, black_box(Action::InputChar('x'))),
                BatchSize::PerIteration,
            )
        });

        // The per-token path during a live run: one streamed fragment folded
        // into the transcript. This fires far more often than a keystroke.
        let run = run_id(0);
        group.bench_function(format!("stream_delta/{runs}_runs"), |b| {
            b.iter_batched_ref(
                || template.clone(),
                |state| {
                    reduce(
                        state,
                        ev(EventBody::ModelStreamDelta {
                            run_id: run,
                            text: "ing the ".to_owned(),
                            thought: false,
                        }),
                    )
                },
                BatchSize::PerIteration,
            )
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Per-frame breakdown.
//
// `render` reaches every section through private functions, so a bench in an
// external crate cannot time them individually. It can, however, DIFFERENCE
// states that are identical except for the section under test — which is what
// the groups below do, and which has the advantage of measuring the sections
// exactly as `render` actually invokes them.
//
//   chrome            = header + composer + footer + empty transcript
//   chat  - chrome    = the transcript pass
//   workspace - chat  = the runs pane + context pane
//   overlay/X - base  = overlay X, drawn over an identical base
//
// The overlay group deliberately uses a SMALL base corpus (4 runs): an overlay
// draws on top of a base frame that is still fully rendered, so a large base
// would bury the signal under transcript cost.
// ---------------------------------------------------------------------------

/// The base for the overlay group: small enough that the overlay dominates.
const OVERLAY_BASE_RUNS: usize = 4;

fn learning(i: usize) -> codypendent_tui::LearningCard {
    codypendent_tui::LearningCard {
        id: format!("learning-{i}"),
        // Multi-byte, so the per-row `truncate_display_width` grapheme walk has
        // real work rather than an ASCII fast path.
        statement: format!("{i}: 設定ファイルの検証を先に行う — verified across 3 runs 🎉"),
        kind: "preference".to_owned(),
        state: "active".to_owned(),
        scope: "workspace".to_owned(),
        provenance: format!("run {i} · verified outcome"),
        confidence: 0.87,
        pinned: i.is_multiple_of(7),
        revision: 3,
    }
}

fn issue(i: usize) -> String {
    format!("{i}: プロバイダの資格情報が見つかりません — set CODYPENDENT_API_KEY 🔑")
}

/// Render `state` once so every render-time memo it owns is hot, then time it.
/// Every bench in the breakdown is a STEADY-STATE frame; cold caches are the
/// separate `frame/cold` case above.
fn bench_warm(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    name: String,
    state: &AppState,
) {
    let theme = Theme::dark();
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
    draw(&mut terminal, state, &theme);
    group.bench_function(name, |b| b.iter(|| draw(&mut terminal, state, &theme)));
}

fn bench_frame_parts(c: &mut Criterion) {
    let mut group = c.benchmark_group("tui/frame_parts");

    // The floor: no runs at all, so the transcript pass has nothing to walk.
    bench_warm(&mut group, "chrome_only".to_owned(), &AppState::new());

    for runs in [64usize, 512] {
        let chat = corpus(runs);
        bench_warm(&mut group, format!("chat/{runs}_runs"), &chat);

        let mut workspace = corpus(runs);
        workspace.layout = codypendent_tui::LayoutMode::Workspace;
        bench_warm(&mut group, format!("workspace/{runs}_runs"), &workspace);
    }

    group.finish();
}

fn bench_overlays(c: &mut Criterion) {
    let mut group = c.benchmark_group("tui/overlay");

    // The subtrahend: the same base frame with no overlay open.
    bench_warm(&mut group, "none".to_owned(), &corpus(OVERLAY_BASE_RUNS));

    // Both of these are list overlays whose row-formatting loop is not bounded
    // by the viewport. Measured at two list lengths: if the cost tracks the
    // list rather than the ~30-row window, the ratio says so.
    for n in [50usize, 2000] {
        let mut issues = corpus(OVERLAY_BASE_RUNS);
        issues.issues = (0..n).map(issue).collect();
        issues.selected_issue = 0;
        issues.overlay = codypendent_tui::Overlay::Issues;
        bench_warm(&mut group, format!("issues/{n}_rows"), &issues);

        let mut journey = corpus(OVERLAY_BASE_RUNS);
        journey.learnings = (0..n).map(learning).collect();
        journey.selected_learning = 0;
        journey.overlay = codypendent_tui::Overlay::Journey;
        bench_warm(&mut group, format!("journey/{n}_rows"), &journey);
    }

    // A static overlay, as a control: it has no list, so its cost must not move
    // with anything. If `help` drifts, the harness is measuring noise.
    let mut help = corpus(OVERLAY_BASE_RUNS);
    help.overlay = codypendent_tui::Overlay::Help;
    bench_warm(&mut group, "help".to_owned(), &help);

    group.finish();
}

/// Run just the clipboard boundary with:
/// `cargo bench -p codypendent-tui --bench frame -- tui/paste --quick`.
/// The event is constructed outside timing: measure production normalisation,
/// including its allocation, without repeatedly cloning the source clipboard.
fn bench_paste(c: &mut Criterion) {
    let mut group = c.benchmark_group("tui/paste");
    for repeats in [100usize, 750_000] {
        let event = crossterm::event::Event::Paste("code café\r\n".repeat(repeats));
        group.bench_function(format!("{}_bytes", repeats * 12), |b| {
            b.iter(|| {
                black_box(codypendent_tui::map_event(
                    black_box(&event),
                    codypendent_tui::state::InputMode::Composer,
                    WIDTH,
                    &[],
                ));
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_frame,
    bench_keystroke,
    bench_frame_parts,
    bench_overlays,
    bench_paste
);
criterion_main!(benches);
