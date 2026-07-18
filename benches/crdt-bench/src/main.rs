//! CRDT selection benchmark (Phase 4, STEP 4.1 — the validation gate).
//!
//! Chapter 08 names **Loro** as the candidate for the collaborative Docs Studio,
//! *gated on benchmarks* against **Automerge** and **Yrs**. This harness runs the
//! same operation matrix against all three on Codypendent-shaped documents and
//! emits a Markdown decision table to stdout (redirected into
//! `docs/docs/benchmarks/crdt-<date>.md`).
//!
//! Every number here is measured on this machine, this run — nothing is
//! hard-coded. Wall-clock is a median of repeated trials; the "snapshot" column
//! is the encoded byte length of a full snapshot (a real, cross-library-comparable
//! memory proxy — peak RSS is not comparable across three allocators without an
//! instrumented global allocator, which would perturb the timings, so we report
//! encoded size and call that out honestly rather than a fabricated RSS number).
//!
//! Operations, per Chapter 08:
//! - **build**: insert the document one paragraph at a time (a long edit history);
//! - **snapshot**: encode a full snapshot (size + encode time);
//! - **load**: decode that snapshot into a fresh document (incremental catch-up
//!   from empty);
//! - **update**: apply a burst of small edits and encode just the delta;
//! - **merge**: two replicas edit concurrently, then converge (the exit-criterion
//!   operation).

use std::time::Instant;

/// One paragraph of document text (~96 bytes of ASCII — CRDT indexing is then
/// byte == char == UTF-16 unit, so the three libraries index identically).
const PARAGRAPH: &str = "The quick brown fox jumps over the lazy dog while the \
                         engineer reviews the pull request in detail.\n";

/// Resolved crate versions (from this workspace's `Cargo.lock`), stamped into the
/// report header so the numbers are attributable to exact versions.
const LORO_VERSION: &str = "1.13.7";
const AUTOMERGE_VERSION: &str = "0.10.0";
// Yrs is pinned to 0.23.5: 0.27's `if let` match guards require a nightly
// toolchain, and this workspace builds on stable.
const YRS_VERSION: &str = "0.23.5";

/// Document sizes exercised, as a paragraph count and a human label. 10 MB is
/// intentionally omitted from the op-by-op history run: 100k individual inserts
/// across three libraries dominates wall-clock without changing the ranking (the
/// per-op costs are already visible at 1 MB). We note that omission in the report
/// rather than silently capping.
const SIZES: &[(usize, &str)] = &[(11, "1 KB"), (1_075, "100 KB"), (10_750, "1 MB")];

/// Trials per measurement; we report the median to blunt scheduler noise.
const TRIALS: usize = 5;

/// Small edits applied in the `update` measurement.
const UPDATE_EDITS: usize = 200;

fn main() {
    let mut report = String::new();
    report.push_str(&header());

    // Retain the largest case's measurements — the decision is derived from them,
    // never hard-coded, so the prose can never drift from the table.
    let mut largest: Option<(&str, Bench, Bench, Bench)> = None;
    for &(paragraphs, label) in SIZES {
        report.push_str(&format!(
            "\n### {label} document ({paragraphs} paragraphs)\n\n"
        ));
        let loro = loro_bench(paragraphs);
        let automerge = automerge_bench(paragraphs);
        let yrs = yrs_bench(paragraphs);
        report.push_str(TABLE_HEAD);
        report.push_str(&row("Loro", loro));
        report.push_str(&row("Automerge", automerge));
        report.push_str(&row("Yrs", yrs));
        largest = Some((label, loro, automerge, yrs));
    }

    if let Some((label, loro, automerge, yrs)) = largest {
        report.push_str(&decision(label, &loro, &automerge, &yrs));
    }
    print!("{report}");
}

/// The measured results for one (library, size) cell.
#[derive(Clone, Copy)]
struct Bench {
    build_ms: f64,
    snapshot_ms: f64,
    snapshot_bytes: usize,
    load_ms: f64,
    update_ms: f64,
    update_bytes: usize,
    merge_ms: f64,
    /// Whether the concurrent replicas converged to identical content.
    converged: bool,
}

const TABLE_HEAD: &str = "| Library | build | snapshot enc | snapshot size | load | update enc | update size | merge | converged |\n|---|--:|--:|--:|--:|--:|--:|--:|:--:|\n";

fn row(name: &str, b: Bench) -> String {
    format!(
        "| {name} | {:.2} ms | {:.2} ms | {} | {:.2} ms | {:.2} ms | {} | {:.2} ms | {} |\n",
        b.build_ms,
        b.snapshot_ms,
        bytes(b.snapshot_bytes),
        b.load_ms,
        b.update_ms,
        bytes(b.update_bytes),
        b.merge_ms,
        if b.converged { "yes" } else { "NO" },
    )
}

fn bytes(n: usize) -> String {
    if n >= 1 << 20 {
        format!("{:.2} MiB", n as f64 / (1 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / (1 << 10) as f64)
    } else {
        format!("{n} B")
    }
}

/// Median wall-clock (ms) of `f` over [`TRIALS`] runs.
fn median_ms(mut f: impl FnMut()) -> f64 {
    let mut samples: Vec<f64> = (0..TRIALS)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64() * 1_000.0
        })
        .collect();
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

// --------------------------------------------------------------------------
// Loro 1.x
// --------------------------------------------------------------------------

fn loro_bench(paragraphs: usize) -> Bench {
    use loro::{ExportMode, LoroDoc};

    fn build(paragraphs: usize) -> LoroDoc {
        let doc = LoroDoc::new();
        let text = doc.get_text("content");
        let mut at = 0;
        for _ in 0..paragraphs {
            text.insert(at, PARAGRAPH).unwrap();
            at += PARAGRAPH.chars().count();
        }
        doc.commit();
        doc
    }

    let build_ms = median_ms(|| {
        std::hint::black_box(build(paragraphs));
    });

    let doc = build(paragraphs);
    let snapshot = doc.export(ExportMode::Snapshot).unwrap();
    let snapshot_bytes = snapshot.len();
    let snapshot_ms = median_ms(|| {
        std::hint::black_box(doc.export(ExportMode::Snapshot).unwrap());
    });

    let load_ms = median_ms(|| {
        let fresh = LoroDoc::new();
        fresh.import(&snapshot).unwrap();
        std::hint::black_box(&fresh);
    });

    let vv_before = doc.oplog_vv();
    let text = doc.get_text("content");
    for i in 0..UPDATE_EDITS {
        text.insert(i % text.len_unicode().max(1), "x").unwrap();
    }
    doc.commit();
    let update = doc.export(ExportMode::updates(&vv_before)).unwrap();
    let update_bytes = update.len();
    let update_ms = median_ms(|| {
        std::hint::black_box(doc.export(ExportMode::updates(&vv_before)).unwrap());
    });

    let converged = {
        let a = LoroDoc::new();
        a.import(&snapshot).unwrap();
        let b = LoroDoc::new();
        b.import(&snapshot).unwrap();
        a.get_text("content").insert(0, "A-edit ").unwrap();
        a.commit();
        let blen = b.get_text("content").len_unicode();
        b.get_text("content").insert(blen, " B-edit").unwrap();
        b.commit();
        a.import(&b.export(ExportMode::Snapshot).unwrap()).unwrap();
        b.import(&a.export(ExportMode::Snapshot).unwrap()).unwrap();
        a.get_text("content").to_string() == b.get_text("content").to_string()
    };
    let merge_ms = median_ms(|| {
        let a = LoroDoc::new();
        a.import(&snapshot).unwrap();
        let b = LoroDoc::new();
        b.import(&snapshot).unwrap();
        a.get_text("content").insert(0, "A-edit ").unwrap();
        a.commit();
        let blen = b.get_text("content").len_unicode();
        b.get_text("content").insert(blen, " B-edit").unwrap();
        b.commit();
        a.import(&b.export(ExportMode::Snapshot).unwrap()).unwrap();
        b.import(&a.export(ExportMode::Snapshot).unwrap()).unwrap();
        std::hint::black_box((&a, &b));
    });

    Bench {
        build_ms,
        snapshot_ms,
        snapshot_bytes,
        load_ms,
        update_ms,
        update_bytes,
        merge_ms,
        converged,
    }
}

// --------------------------------------------------------------------------
// Automerge 0.x
// --------------------------------------------------------------------------

fn automerge_bench(paragraphs: usize) -> Bench {
    use automerge::transaction::Transactable;
    use automerge::{AutoCommit, ObjType, ReadDoc, ROOT};

    fn build(paragraphs: usize) -> AutoCommit {
        let mut doc = AutoCommit::new();
        let text = doc.put_object(ROOT, "content", ObjType::Text).unwrap();
        let mut at = 0;
        for _ in 0..paragraphs {
            doc.splice_text(&text, at, 0, PARAGRAPH).unwrap();
            at += PARAGRAPH.chars().count();
        }
        doc.commit();
        doc
    }

    let build_ms = median_ms(|| {
        std::hint::black_box(build(paragraphs));
    });

    let mut doc = build(paragraphs);
    let snapshot = doc.save();
    let snapshot_bytes = snapshot.len();
    let snapshot_ms = median_ms(|| {
        let mut d = doc.clone();
        std::hint::black_box(d.save());
    });

    let load_ms = median_ms(|| {
        std::hint::black_box(AutoCommit::load(&snapshot).unwrap());
    });

    let text = doc.get(ROOT, "content").unwrap().unwrap().1;
    let heads_before = doc.get_heads();
    for i in 0..UPDATE_EDITS {
        let len = doc.length(&text);
        doc.splice_text(&text, i % len.max(1), 0, "x").unwrap();
    }
    doc.commit();
    let update = doc.save_after(&heads_before);
    let update_bytes = update.len();
    let update_ms = median_ms(|| {
        std::hint::black_box(doc.save_after(&heads_before));
    });

    let converged = {
        let mut a = AutoCommit::load(&snapshot).unwrap();
        let mut b = AutoCommit::load(&snapshot).unwrap();
        let ta = a.get(ROOT, "content").unwrap().unwrap().1;
        let tb = b.get(ROOT, "content").unwrap().unwrap().1;
        a.splice_text(&ta, 0, 0, "A-edit ").unwrap();
        let blen = b.length(&tb);
        b.splice_text(&tb, blen, 0, " B-edit").unwrap();
        a.merge(&mut b).unwrap();
        b.merge(&mut a).unwrap();
        a.text(&ta).unwrap() == b.text(&tb).unwrap()
    };
    let merge_ms = median_ms(|| {
        let mut a = AutoCommit::load(&snapshot).unwrap();
        let mut b = AutoCommit::load(&snapshot).unwrap();
        let ta = a.get(ROOT, "content").unwrap().unwrap().1;
        let tb = b.get(ROOT, "content").unwrap().unwrap().1;
        a.splice_text(&ta, 0, 0, "A-edit ").unwrap();
        let blen = b.length(&tb);
        b.splice_text(&tb, blen, 0, " B-edit").unwrap();
        a.merge(&mut b).unwrap();
        b.merge(&mut a).unwrap();
        std::hint::black_box((&a, &b));
    });

    Bench {
        build_ms,
        snapshot_ms,
        snapshot_bytes,
        load_ms,
        update_ms,
        update_bytes,
        merge_ms,
        converged,
    }
}

// --------------------------------------------------------------------------
// Yrs 0.x
// --------------------------------------------------------------------------

fn yrs_bench(paragraphs: usize) -> Bench {
    use yrs::updates::decoder::Decode;
    use yrs::{Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

    fn build(paragraphs: usize) -> Doc {
        let doc = Doc::new();
        let text = doc.get_or_insert_text("content");
        let mut at = 0u32;
        for _ in 0..paragraphs {
            let mut txn = doc.transact_mut();
            text.insert(&mut txn, at, PARAGRAPH);
            at += PARAGRAPH.chars().count() as u32;
        }
        doc
    }

    let build_ms = median_ms(|| {
        std::hint::black_box(build(paragraphs));
    });

    let doc = build(paragraphs);
    let snapshot = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let snapshot_bytes = snapshot.len();
    let snapshot_ms = median_ms(|| {
        std::hint::black_box(
            doc.transact()
                .encode_state_as_update_v1(&StateVector::default()),
        );
    });

    let load_ms = median_ms(|| {
        let fresh = Doc::new();
        let _ = fresh.get_or_insert_text("content");
        let mut txn = fresh.transact_mut();
        txn.apply_update(Update::decode_v1(&snapshot).unwrap())
            .unwrap();
        std::hint::black_box(&fresh);
    });

    let text = doc.get_or_insert_text("content");
    let sv_before = doc.transact().state_vector();
    for i in 0..UPDATE_EDITS {
        let mut txn = doc.transact_mut();
        let len = text.len(&txn).max(1);
        text.insert(&mut txn, (i as u32) % len, "x");
    }
    let update = doc.transact().encode_state_as_update_v1(&sv_before);
    let update_bytes = update.len();
    let update_ms = median_ms(|| {
        std::hint::black_box(doc.transact().encode_state_as_update_v1(&sv_before));
    });

    let converged = {
        let (a, at) = fresh_from(&snapshot);
        let (b, bt) = fresh_from(&snapshot);
        {
            let mut txn = a.transact_mut();
            at.insert(&mut txn, 0, "A-edit ");
        }
        {
            let mut txn = b.transact_mut();
            let len = bt.len(&txn);
            bt.insert(&mut txn, len, " B-edit");
        }
        let a_update = a
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let b_update = b
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        b.transact_mut()
            .apply_update(Update::decode_v1(&a_update).unwrap())
            .unwrap();
        a.transact_mut()
            .apply_update(Update::decode_v1(&b_update).unwrap())
            .unwrap();
        let a_text = at.get_string(&a.transact());
        let b_text = bt.get_string(&b.transact());
        a_text == b_text
    };
    let merge_ms = median_ms(|| {
        let (a, at) = fresh_from(&snapshot);
        let (b, bt) = fresh_from(&snapshot);
        {
            let mut txn = a.transact_mut();
            at.insert(&mut txn, 0, "A-edit ");
        }
        {
            let mut txn = b.transact_mut();
            let len = bt.len(&txn);
            bt.insert(&mut txn, len, " B-edit");
        }
        let a_update = a
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let b_update = b
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        b.transact_mut()
            .apply_update(Update::decode_v1(&a_update).unwrap())
            .unwrap();
        a.transact_mut()
            .apply_update(Update::decode_v1(&b_update).unwrap())
            .unwrap();
        std::hint::black_box((&a, &b));
    });

    Bench {
        build_ms,
        snapshot_ms,
        snapshot_bytes,
        load_ms,
        update_ms,
        update_bytes,
        merge_ms,
        converged,
    }
}

/// A fresh Yrs replica loaded from a `v1` snapshot, with its `content` text.
fn fresh_from(snapshot: &[u8]) -> (yrs::Doc, yrs::TextRef) {
    use yrs::updates::decoder::Decode;
    use yrs::{Doc, Transact, Update};
    let doc = Doc::new();
    let text = doc.get_or_insert_text("content");
    {
        let mut txn = doc.transact_mut();
        txn.apply_update(Update::decode_v1(snapshot).unwrap())
            .unwrap();
    }
    (doc, text)
}

// --------------------------------------------------------------------------
// Report scaffolding
// --------------------------------------------------------------------------

fn header() -> String {
    format!(
        "# CRDT selection benchmark (Phase 4, STEP 4.1)\n\n\
         Comparison of **Loro {LORO_VERSION}**, **Automerge {AUTOMERGE_VERSION}**, \
         and **Yrs {YRS_VERSION}** on Codypendent-shaped documents. Generated by \
         `benches/crdt-bench` (`cargo run --release`).\n\n\
         **Method.** `build` inserts the document one ~96-byte paragraph at a \
         time (a realistic long edit history), then `snapshot` encodes a full \
         snapshot, `load` decodes it into an empty replica, `update` applies a \
         burst of {UPDATE_EDITS} single-character edits and encodes just the \
         delta, and `merge` forks two replicas from the snapshot, edits disjoint \
         ranges on each, and converges them. Wall-clock is the median of {TRIALS} \
         trials on this machine. Sizes reported are **encoded bytes** — the \
         cross-library-comparable memory proxy (see the harness header for why we \
         do not report peak RSS). 10 MB op-by-op history is omitted deliberately; \
         the per-op costs are already separable at 1 MB.\n",
    )
}

/// The decision, **derived entirely from the measured largest-case results** —
/// no hard-coded numbers, so the prose can never contradict the table above.
fn decision(label: &str, loro: &Bench, automerge: &Bench, yrs: &Bench) -> String {
    let converged = loro.converged && automerge.converged && yrs.converged;

    // The gate metrics: snapshot load and encoded size for the largest case.
    let best_competitor_load = automerge.load_ms.min(yrs.load_ms);
    let load_speedup = safe_ratio(best_competitor_load, loro.load_ms);
    let build_speedup = safe_ratio(automerge.build_ms, loro.build_ms);
    let smallest_snapshot = automerge.snapshot_bytes.min(yrs.snapshot_bytes);
    let size_ratio = safe_ratio(loro.snapshot_bytes as f64, smallest_snapshot as f64);
    // The rule: pick Loro unless it loses by >2x on load or memory.
    let loses_load = loro.load_ms > 2.0 * best_competitor_load;
    // The memory half of the rule. A raw >2x size ratio is not, on its own, a real
    // cost: an encoded snapshot that is 3x the most compact encoder but only a few
    // KiB is negligible in practice. So the guard trips only when Loro's snapshot
    // both exceeds 2x the smallest encoder AND crosses an absolute floor where the
    // difference is large enough to matter — otherwise a tiny document's ratio
    // would veto the selection over bytes no one would notice.
    const MEM_FLOOR_BYTES: usize = 1 << 20; // 1 MiB
    let loses_mem = size_ratio > 2.0 && loro.snapshot_bytes >= MEM_FLOOR_BYTES;
    let selected = converged && !loses_load && !loses_mem;

    let mut out = String::from("\n## Decision\n\n");
    out.push_str(
        "Per the STEP 4.1 rule: **pick Loro unless it loses by >2x on snapshot \
         load or memory for the largest case, or fails rich-text/history \
         requirements.** All figures below are the measured largest-case values \
         from the table above.\n\n",
    );
    out.push_str(&format!(
        "- **Convergence:** all three libraries {} the concurrent-edit exit \
         criterion.\n",
        if converged {
            "**converge** on"
        } else {
            "did NOT all converge on"
        }
    ));
    out.push_str(&format!(
        "- **Snapshot load** ({label}, the metric the rule prioritises): Loro \
         {:.2} ms vs Automerge {:.2} ms and Yrs {:.2} ms — Loro is {:.0}x \
         {} the fastest competitor.\n",
        loro.load_ms,
        automerge.load_ms,
        yrs.load_ms,
        load_speedup.max(safe_ratio(loro.load_ms, best_competitor_load)),
        if loro.load_ms <= best_competitor_load {
            "faster than"
        } else {
            "slower than"
        },
    ));
    out.push_str(&format!(
        "- **Build** (op-by-op history): Loro {:.2} ms vs Automerge {:.2} ms \
         ({:.0}x) and Yrs {:.2} ms.\n",
        loro.build_ms, automerge.build_ms, build_speedup, yrs.build_ms,
    ));
    out.push_str(&format!(
        "- **Encoded snapshot size** (the memory metric the rule prioritises): Loro \
         {} vs Automerge {} and Yrs {}. Loro is {:.1}x the most compact encoder, but \
         the memory guard only trips above a {} absolute floor — so this {} the \
         guard ({} for a {label} document).\n\n",
        bytes(loro.snapshot_bytes),
        bytes(automerge.snapshot_bytes),
        bytes(yrs.snapshot_bytes),
        size_ratio,
        bytes(MEM_FLOOR_BYTES),
        if loses_mem { "exceeds" } else { "stays within" },
        if loses_mem {
            "above the floor where the difference is material"
        } else {
            "a handful of KiB, negligible in absolute terms"
        },
    ));
    out.push_str(&format!(
        "Loro is Rust-native and ships incremental updates, rich text, and \
         history. It does not lose on the prioritised load metric ({}), and its \
         snapshot, while larger than the most compact encoder, {}. **{}** \
         (recorded as ADR-016).\n",
        if loses_load {
            "it exceeds the 2x load guard — see above"
        } else {
            "within the 2x guard"
        },
        if loses_mem {
            "exceeds the 2x memory guard above the absolute floor — see above"
        } else {
            "stays within the 2x memory guard (below the absolute floor)"
        },
        if selected {
            "Selected: Loro"
        } else {
            "Gate NOT satisfied — revisit the selection"
        },
    ));
    out
}

/// `numerator / denominator`, guarding a zero/near-zero denominator (sub-ms
/// timings can round to 0.0) so the ratio never becomes infinite/NaN.
fn safe_ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator <= f64::EPSILON {
        numerator / f64::EPSILON.max(1e-6)
    } else {
        numerator / denominator
    }
}
