import { PageShell } from "./PageShell.js";
/**
 * The measured-analytics surface.
 *
 * Two rules govern everything here, and both exist because the opposite was
 * shipped before:
 *
 * 1. An absent measurement is not zero. `AnalyticsMetrics` is nullable by
 *    design (`crates/protocol/src/analytics.rs`) precisely so "we did not
 *    measure this" survives the wire, so every value renders through
 *    `formatMetric`, which prints `unavailable` for null/undefined.
 * 2. A failed query is not an empty result. `onQueryAnalytics` resolves to
 *    `null` when the command did not reach a daemon that answers it, and that
 *    case renders as an explicit unavailable panel — never as "no observations
 *    recorded", which would assert a measurement we never made.
 */
import React, { useCallback, useLayoutEffect, useMemo, useRef, useState } from "react";
import { ArtifactSaveButton, type SaveArtifact } from "./ArtifactActions.js";
import { formatCostMicros } from "../runActivity.js";
import { modelLabel } from "../modelPresentation.js";
import type {
  AnalyticsBucket,
  AnalyticsExportFormat,
  AnalyticsExportRequest,
  AnalyticsExportResult,
  AnalyticsGrouping,
  AnalyticsMetrics,
  AnalyticsPage,
  AnalyticsQuery,
  AnalyticsTimeRange,
} from "@codypendent/protocol";

export interface AnalyticsDashboardProps {
  onQueryAnalytics?: (query?: AnalyticsQuery) => Promise<AnalyticsPage | null>;
  onExportAnalytics?: (request: AnalyticsExportRequest) => Promise<AnalyticsExportResult | null>;
  initialPage?: AnalyticsPage | null;
  /** Invalidate in-flight requests when the native connection is replaced. */
  connectionEpoch?: number;
  onSaveArtifact?: SaveArtifact;
}

export function formatMetric(
  value: number | null | undefined,
  formatType: "number" | "currency" | "latency" | "score" = "number",
): string {
  if (value === null || value === undefined) {
    return "unavailable";
  }
  switch (formatType) {
    case "currency":
      return formatCostMicros(value);
    case "latency":
      return `${value.toLocaleString()} ms`;
    case "score":
      return `${(value / 1_000_000).toFixed(2)}`;
    case "number":
    default:
      return value.toLocaleString();
  }
}

/** Metrics that are counts, so the page total is their sum. */
type AdditiveMetric =
  | "input_tokens"
  | "output_tokens"
  | "cached_tokens"
  | "reasoning_tokens"
  | "cost_micros"
  | "completion_count"
  | "retry_count"
  | "escalation_count";

/** Metrics that are per-observation averages, so summing them would be wrong. */
type MeanMetric = "latency_ms" | "grader_score_micros" | "cost_per_successful_task_micros";

/**
 * Sum the buckets that measured `key`. Null when none did — an unmeasured
 * total stays unmeasured rather than collapsing to zero.
 */
export function sumMeasured(buckets: AnalyticsBucket[], key: AdditiveMetric): number | null {
  const measured = buckets
    .map((bucket) => bucket.metrics[key])
    .filter((value): value is number => value !== null && value !== undefined);
  if (measured.length === 0) {
    return null;
  }
  return measured.reduce((total, value) => total + value, 0);
}

/**
 * Combine a per-observation average across buckets, weighted by how many
 * completions each bucket represents.
 *
 * A single measured bucket needs no weighting. Several measured buckets need
 * `completion_count` on every one of them; without those weights the combined
 * average is not derivable from this page, and the honest answer is `null`
 * (rendered as "unavailable") rather than an unweighted mean that would
 * silently misreport a busy bucket and a quiet one as equals.
 */
export function meanMeasured(buckets: AnalyticsBucket[], key: MeanMetric): number | null {
  const measured = buckets.filter(
    (bucket) => bucket.metrics[key] !== null && bucket.metrics[key] !== undefined,
  );
  if (measured.length === 0) {
    return null;
  }
  if (measured.length === 1) {
    return measured[0].metrics[key] as number;
  }
  const weights: number[] = [];
  for (const bucket of measured) {
    const weight = bucket.metrics.completion_count;
    if (weight === null || weight === undefined || weight <= 0) {
      return null;
    }
    weights.push(weight);
  }
  const totalWeight = weights.reduce((total, weight) => total + weight, 0);
  const weighted = measured.reduce(
    (total, bucket, i) => total + (bucket.metrics[key] as number) * weights[i],
    0,
  );
  return Math.round(weighted / totalWeight);
}

/**
 * Totals across the buckets on this page.
 *
 * This is derived from the page, not read from it: `AnalyticsPage` carries no
 * totals field, so a previous version showed `buckets[0].metrics` under a
 * heading that read as an overall figure — one arbitrary model's numbers
 * presented as the whole. Every field below is either a real sum, a real
 * weighted mean, or null.
 */
export function aggregateMetrics(buckets: AnalyticsBucket[]): AnalyticsMetrics {
  return {
    input_tokens: sumMeasured(buckets, "input_tokens"),
    output_tokens: sumMeasured(buckets, "output_tokens"),
    cached_tokens: sumMeasured(buckets, "cached_tokens"),
    reasoning_tokens: sumMeasured(buckets, "reasoning_tokens"),
    cost_micros: sumMeasured(buckets, "cost_micros"),
    completion_count: sumMeasured(buckets, "completion_count"),
    retry_count: sumMeasured(buckets, "retry_count"),
    escalation_count: sumMeasured(buckets, "escalation_count"),
    latency_ms: meanMeasured(buckets, "latency_ms"),
    grader_score_micros: meanMeasured(buckets, "grader_score_micros"),
    cost_per_successful_task_micros: meanMeasured(buckets, "cost_per_successful_task_micros"),
  };
}

/**
 * Turn the window control into a real `AnalyticsFilters.time`. Returning null
 * for "all" means no restriction; every other option must actually narrow the
 * query, or the control would label the numbers with a window that was never
 * applied.
 */
export function timeRangeFilter(range: string, now: number = Date.now()): AnalyticsTimeRange | null {
  const spans: Record<string, number> = {
    "24h": 24 * 60 * 60 * 1000,
    "7d": 7 * 24 * 60 * 60 * 1000,
    "30d": 30 * 24 * 60 * 60 * 1000,
  };
  const span = spans[range];
  if (span === undefined) {
    return null;
  }
  return { from: new Date(now - span).toISOString(), until: null };
}

export const AnalyticsDashboard: React.FC<AnalyticsDashboardProps> = ({
  onQueryAnalytics,
  onExportAnalytics,
  onSaveArtifact,
  initialPage,
  connectionEpoch = 0,
}) => {
  const [grouping, setGrouping] = useState<AnalyticsGrouping["type"]>("model");
  const [timeRange, setTimeRange] = useState<string>("7d");
  const [page, setPage] = useState<AnalyticsPage | null>(initialPage ?? null);
  const [loading, setLoading] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [exportResult, setExportResult] = useState<AnalyticsExportResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** Set when the query could not be answered, as opposed to answering "none". */
  const [unavailable, setUnavailable] = useState<string | null>(
    initialPage || onQueryAnalytics
      ? null
      : "No analytics source is wired to this view, so nothing here has been measured.",
  );

  const [refreshTick, setRefreshTick] = useState(0);
  const queryGeneration = useRef(0);
  const exportGeneration = useRef(0);
  const queryInFlight = useRef<number | null>(null);
  const exportInFlight = useRef<number | null>(null);
  // Cursor hashes include the time filter. Freeze the window across pages;
  // recomputing "seven days ago" on each click makes the next cursor invalid.
  const query = useMemo<AnalyticsQuery>(() => {
    const time = timeRangeFilter(timeRange);
    return {
      group_by: grouping === "unknown" ? [] : [{ type: grouping }],
      ...(time ? { filters: { time } } : {}),
    };
  }, [grouping, timeRange, refreshTick]);

  const fetchAnalytics = useCallback(async (cursor?: string) => {
    if (!onQueryAnalytics || queryInFlight.current !== null) return;
    const generation = ++queryGeneration.current;
    queryInFlight.current = generation;
    setLoading(true);
    setError(null);
    try {
      const res = await onQueryAnalytics({ ...query, ...(cursor ? { cursor } : {}) });
      if (queryGeneration.current !== generation) return;
      if (res === null) throw new Error("The daemon did not answer the analytics query, so no measurements could be read.");
      setPage((previous) => cursor && previous
        ? { ...res, items: [...previous.items, ...res.items] }
        : res);
      setUnavailable(null);
    } catch (err) {
      if (queryGeneration.current !== generation) return;
      const detail = err instanceof Error ? err.message : String(err);
      if (!cursor) {
        setPage(null);
        setUnavailable(detail);
      }
      setError(cursor ? `The next page failed: ${detail}. Loaded measurements are retained; retry below.` : detail);
    } finally {
      if (queryGeneration.current === generation) {
        queryInFlight.current = null;
        setLoading(false);
      }
    }
  }, [onQueryAnalytics, query]);

  useLayoutEffect(() => {
    ++queryGeneration.current;
    queryInFlight.current = null;
    setPage(onQueryAnalytics ? null : initialPage ?? null);
    setLoading(false);
    setError(null);
    setUnavailable(onQueryAnalytics || initialPage ? null : "No analytics source is wired to this view, so nothing here has been measured.");
    void fetchAnalytics();
    return () => { ++queryGeneration.current; queryInFlight.current = null; };
  }, [fetchAnalytics, initialPage, connectionEpoch, onQueryAnalytics]);

  useLayoutEffect(() => {
    ++exportGeneration.current;
    exportInFlight.current = null;
    setExporting(false);
    setExportResult(null);
    return () => { ++exportGeneration.current; exportInFlight.current = null; };
  }, [query, onExportAnalytics, connectionEpoch]);

  const handleExport = async (format: AnalyticsExportFormat["type"]) => {
    if (!onExportAnalytics || exportInFlight.current !== null) return;
    const generation = ++exportGeneration.current;
    exportInFlight.current = generation;
    setExporting(true);
    setExportResult(null);
    setError(null);
    try {
      const res = await onExportAnalytics({ format: { type: format }, query });
      if (exportGeneration.current !== generation) return;
      if (res === null) throw new Error("The daemon did not answer the export request, so no artifact was produced.");
      setExportResult(res);
    } catch (err) {
      if (exportGeneration.current === generation) setError(err instanceof Error ? err.message : String(err));
    } finally {
      if (exportGeneration.current === generation) {
        exportInFlight.current = null;
        setExporting(false);
      }
    }
  };

  const buckets: AnalyticsBucket[] = page?.items ?? [];
  // Derived from the buckets actually on this page — see `aggregateMetrics`.
  const totals: AnalyticsMetrics = aggregateMetrics(buckets);
  const truncated = Boolean(page?.next_cursor);

  return (
    <PageShell
      role="region"
      aria-label="Analytics Dashboard"
      style={{
        flex: 1,
        display: "flex",
        flexDirection: "column",
        height: "100%",
        background: "var(--cody-canvas)",
        color: "var(--cody-text)",
        overflowY: "auto",
      }}
    >
      {/* Header */}
      <div
        className="cody-page-header"
        style={{
          padding: "20px 24px 16px",
          borderBottom: "1px solid var(--cody-inset)",
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          flexWrap: "wrap",
          gap: 12,
        }}
      >
        <div>
          <h1 style={{ margin: 0, fontSize: 20, fontWeight: 600 }}>Usage</h1>
          <p style={{ margin: "4px 0 0", fontSize: 13, color: "var(--cody-text-muted)" }}>
            See what your agents use, how much they cost and how they perform.
          </p>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <button
            onClick={() => setRefreshTick((tick) => tick + 1)}
            disabled={loading || !onQueryAnalytics}
            style={{
              padding: "6px 12px",
              background: "var(--cody-inset)",
              border: "1px solid var(--cody-border-strong)",
              borderRadius: 6,
              color: "var(--cody-text)",
              cursor: loading ? "not-allowed" : "pointer",
              fontSize: 13,
            }}
          >
            {loading ? "Refreshing…" : "Refresh"}
          </button>
          <button
            onClick={() => handleExport("json")}
            disabled={exporting || !onExportAnalytics}
            style={{
              padding: "6px 12px",
              background: "var(--cody-inset)",
              border: "1px solid var(--cody-border-strong)",
              borderRadius: 6,
              color: "var(--cody-text)",
              cursor: exporting ? "not-allowed" : "pointer",
              fontSize: 13,
            }}
          >
            {exporting ? "Exporting…" : "Export JSON"}
          </button>
          <button
            onClick={() => handleExport("csv")}
            disabled={exporting || !onExportAnalytics}
            style={{
              padding: "6px 12px",
              background: "var(--cody-inset)",
              border: "1px solid var(--cody-border-strong)",
              borderRadius: 6,
              color: "var(--cody-text)",
              cursor: exporting ? "not-allowed" : "pointer",
              fontSize: 13,
            }}
          >
            {exporting ? "Exporting…" : "Export CSV"}
          </button>
        </div>
      </div>

      {/* Toolbar */}
      <div
        style={{
          padding: "12px 24px",
          borderBottom: "1px solid var(--cody-inset)",
          display: "flex",
          alignItems: "center",
          gap: 20,
          flexWrap: "wrap",
          background: "var(--cody-panel-raised)",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <label htmlFor="grouping-select" style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>
            Group By:
          </label>
          <select
            id="grouping-select"
            disabled={!onQueryAnalytics}
            value={grouping}
            onChange={(e) => setGrouping(e.target.value as AnalyticsGrouping["type"])}
            style={{
              padding: "4px 8px",
              background: "var(--cody-inset)",
              border: "1px solid var(--cody-border-strong)",
              borderRadius: 6,
              color: "var(--cody-text)",
              fontSize: 12,
            }}
          >
            <option value="model">Model</option>
            <option value="provider">Provider</option>
            <option value="repository">Repository</option>
            <option value="workflow">Workflow</option>
            <option value="task_class">Task Class</option>
            <option value="time">Time</option>
            <option value="completion">Completion</option>
            <option value="route">Route</option>
          </select>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <label htmlFor="timerange-select" style={{ fontSize: 12, color: "var(--cody-text-muted)" }}>
            Window:
          </label>
          <select
            id="timerange-select"
            disabled={!onQueryAnalytics}
            value={timeRange}
            onChange={(e) => setTimeRange(e.target.value)}
            style={{
              padding: "4px 8px",
              background: "var(--cody-inset)",
              border: "1px solid var(--cody-border-strong)",
              borderRadius: 6,
              color: "var(--cody-text)",
              fontSize: 12,
            }}
          >
            <option value="24h">Last 24 Hours</option>
            <option value="7d">Last 7 Days</option>
            <option value="30d">Last 30 Days</option>
            <option value="all">All Time</option>
          </select>
        </div>

        {loading && <span role="status" style={{ fontSize: 12, color: "var(--cody-link)" }}>Loading observations…</span>}
      </div>

      {exportResult && (
        <div
          role="status"
          style={{
            margin: "12px 24px 0",
            padding: "8px 12px",
            background: "var(--cody-info-bg)",
            border: "1px solid var(--cody-accent-strong)",
            borderRadius: 6,
            fontSize: 12,
            color: "var(--cody-link)",
          }}
        >
          Export ready ({exportResult.format.type}): {exportResult.row_count} rows, artifact {exportResult.artifact.id}
          {exportResult.truncated && " (truncated)"}
          <ArtifactSaveButton key={exportResult.artifact.id} artifact={exportResult.artifact} onSave={onSaveArtifact} />
        </div>
      )}

      {error && (
        <div
          role="alert"
          style={{
            margin: "12px 24px 0",
            padding: "8px 12px",
            background: "var(--cody-danger-bg)",
            border: "1px solid var(--cody-danger)",
            borderRadius: 6,
            fontSize: 12,
            color: "var(--cody-danger-soft)",
          }}
        >
          {error}
        </div>
      )}

      {/* New filters clear old measurements; continuation requests keep the
          confirmed pages visible while another page is loading. */}
      <div
        style={{
          padding: "20px 24px",
          display: "flex",
          flexDirection: "column",
          gap: 24,
          opacity: loading ? 0.7 : 1,
          transition: "opacity 120ms ease",
        }}
      >
        {/* Metric Cards Grid */}
        <div>
          <h2 style={{ fontSize: 14, fontWeight: 600, color: "var(--cody-text-muted)", textTransform: "uppercase", marginBottom: 4 }}>
            Execution Measurements
          </h2>
          <p style={{ margin: "0 0 12px", fontSize: 12, color: "var(--cody-text-faint)" }}>
            {/* Say what the numbers cover. Counts are summed over the buckets on
                this page; per-observation averages are weighted by completions,
                and read "unavailable" when this page lacks the weights. */}
            Measured subtotals across the {buckets.length} loaded bucket
            {buckets.length === 1 ? "" : "s"} in this view; latency, grader score and cost per task
            are completion-weighted averages.
            {truncated && " More buckets remain beyond the loaded pages, so these are not totals for the whole query."}
          </p>
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
              gap: 12,
            }}
          >
            {/* Input Tokens */}
            <div
              data-testid="metric-input-tokens"
              style={{
                padding: 16,
                background: "var(--cody-panel-raised)",
                border: "1px solid var(--cody-border-strong)",
                borderRadius: 8,
              }}
            >
              <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 4 }}>Input Tokens</div>
              <div style={{ fontSize: 20, fontWeight: 600, color: "var(--cody-text)" }}>
                {formatMetric(totals.input_tokens)}
                <MeasurementCoverage buckets={buckets} metric="input_tokens" />
              </div>
            </div>

            {/* Output Tokens */}
            <div
              data-testid="metric-output-tokens"
              style={{
                padding: 16,
                background: "var(--cody-panel-raised)",
                border: "1px solid var(--cody-border-strong)",
                borderRadius: 8,
              }}
            >
              <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 4 }}>Output Tokens</div>
              <div style={{ fontSize: 20, fontWeight: 600, color: "var(--cody-text)" }}>
                {formatMetric(totals.output_tokens)}
                <MeasurementCoverage buckets={buckets} metric="output_tokens" />
              </div>
            </div>

            {/* Cached Tokens */}
            <div
              data-testid="metric-cached-tokens"
              style={{
                padding: 16,
                background: "var(--cody-panel-raised)",
                border: "1px solid var(--cody-border-strong)",
                borderRadius: 8,
              }}
            >
              <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 4 }}>Cached Tokens</div>
              <div style={{ fontSize: 20, fontWeight: 600, color: "var(--cody-text)" }}>
                {formatMetric(totals.cached_tokens)}
                <MeasurementCoverage buckets={buckets} metric="cached_tokens" />
              </div>
            </div>

            {/* Reasoning Tokens */}
            <div
              data-testid="metric-reasoning-tokens"
              style={{
                padding: 16,
                background: "var(--cody-panel-raised)",
                border: "1px solid var(--cody-border-strong)",
                borderRadius: 8,
              }}
            >
              <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 4 }}>Reasoning Tokens</div>
              <div style={{ fontSize: 20, fontWeight: 600, color: "var(--cody-text)" }}>
                {formatMetric(totals.reasoning_tokens)}
                <MeasurementCoverage buckets={buckets} metric="reasoning_tokens" />
              </div>
            </div>

            {/* Measured Cost */}
            <div
              data-testid="metric-cost"
              style={{
                padding: 16,
                background: "var(--cody-panel-raised)",
                border: "1px solid var(--cody-border-strong)",
                borderRadius: 8,
              }}
            >
              <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 4 }}>Measured Cost</div>
              <div style={{ fontSize: 20, fontWeight: 600, color: "var(--cody-success)" }}>
                {formatMetric(totals.cost_micros, "currency")}
                <MeasurementCoverage buckets={buckets} metric="cost_micros" />
              </div>
            </div>

            {/* Cost Per Successful Task */}
            <div
              data-testid="metric-cost-per-task"
              style={{
                padding: 16,
                background: "var(--cody-panel-raised)",
                border: "1px solid var(--cody-border-strong)",
                borderRadius: 8,
              }}
            >
              <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 4 }}>Cost / Successful Task</div>
              <div style={{ fontSize: 20, fontWeight: 600, color: "var(--cody-success)" }}>
                {formatMetric(totals.cost_per_successful_task_micros, "currency")}
                <MeasurementCoverage buckets={buckets} metric="cost_per_successful_task_micros" />
              </div>
            </div>

            {/* Latency */}
            <div
              data-testid="metric-latency"
              style={{
                padding: 16,
                background: "var(--cody-panel-raised)",
                border: "1px solid var(--cody-border-strong)",
                borderRadius: 8,
              }}
            >
              <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 4 }}>Avg Latency</div>
              <div style={{ fontSize: 20, fontWeight: 600, color: "var(--cody-text)" }}>
                {formatMetric(totals.latency_ms, "latency")}
                <MeasurementCoverage buckets={buckets} metric="latency_ms" />
              </div>
            </div>

            {/* Grader Score */}
            <div
              data-testid="metric-grader-score"
              style={{
                padding: 16,
                background: "var(--cody-panel-raised)",
                border: "1px solid var(--cody-border-strong)",
                borderRadius: 8,
              }}
            >
              <div style={{ fontSize: 12, color: "var(--cody-text-muted)", marginBottom: 4 }}>Grader Score</div>
              <div style={{ fontSize: 20, fontWeight: 600, color: "var(--cody-text)" }}>
                {formatMetric(totals.grader_score_micros, "score")}
                <MeasurementCoverage buckets={buckets} metric="grader_score_micros" />
              </div>
            </div>
          </div>
        </div>

        {/* Breakdown Buckets Table */}
        <div>
          <h2 style={{ fontSize: 14, fontWeight: 600, color: "var(--cody-text-muted)", textTransform: "uppercase", marginBottom: 12 }}>
            Grouped Results ({buckets.length} {buckets.length === 1 ? "bucket" : "buckets"})
          </h2>
          {unavailable ? (
            /* Not the same thing as an empty result: the query was never
               answered, so we cannot say whether observations exist. */
            <div
              data-testid="analytics-unavailable"
              role="status"
              style={{
                padding: 32,
                textAlign: "center",
                background: "var(--cody-panel-raised)",
                border: "1px dashed var(--cody-text-faint)",
                borderRadius: 8,
                color: "var(--cody-warning)",
                fontSize: 14,
              }}
            >
              Analytics unavailable — {unavailable}
            </div>
          ) : loading && buckets.length === 0 ? (
            <div data-testid="analytics-loading" role="status">Loading measurements for the selected filters…</div>
          ) : buckets.length === 0 ? (
            <div
              data-testid="analytics-empty"
              style={{
                padding: 32,
                textAlign: "center",
                background: "var(--cody-panel-raised)",
                border: "1px solid var(--cody-border-strong)",
                borderRadius: 8,
                color: "var(--cody-text-muted)",
                fontSize: 14,
              }}
            >
              No execution observations recorded for this query.
            </div>
          ) : (
            <div
              style={{
                overflowX: "auto",
                background: "var(--cody-panel-raised)",
                border: "1px solid var(--cody-border-strong)",
                borderRadius: 8,
              }}
            >
              <table style={{ width: "100%", borderCollapse: "collapse", fontSize: 13 }}>
                <thead>
                  <tr style={{ borderBottom: "1px solid var(--cody-border-strong)", textAlign: "left", color: "var(--cody-text-muted)" }}>
                    <th style={{ padding: "10px 16px" }}>Dimension</th>
                    <th style={{ padding: "10px 16px" }}>Input Tokens</th>
                    <th style={{ padding: "10px 16px" }}>Output Tokens</th>
                    <th style={{ padding: "10px 16px" }}>Cached</th>
                    <th style={{ padding: "10px 16px" }}>Cost</th>
                    <th style={{ padding: "10px 16px" }}>Latency</th>
                    <th style={{ padding: "10px 16px" }}>Completions</th>
                  </tr>
                </thead>
                <tbody>
                  {buckets.map((bucket, i) => {
                    const dimLabel = bucket.dimensions?.map((dimension) => grouping === "model" ? modelLabel(dimension) : dimension).join(" / ") || `Bucket #${i + 1}`;
                    return (
                      <tr key={i} style={{ borderBottom: "1px solid var(--cody-inset)" }}>
                        <td style={{ padding: "10px 16px", fontWeight: 500 }}>{dimLabel}</td>
                        <td style={{ padding: "10px 16px" }}>{formatMetric(bucket.metrics.input_tokens)}</td>
                        <td style={{ padding: "10px 16px" }}>{formatMetric(bucket.metrics.output_tokens)}</td>
                        <td style={{ padding: "10px 16px" }}>{formatMetric(bucket.metrics.cached_tokens)}</td>
                        <td style={{ padding: "10px 16px", color: bucket.metrics.cost_micros !== null ? "var(--cody-success)" : undefined }}>
                          {formatMetric(bucket.metrics.cost_micros, "currency")}
                        </td>
                        <td style={{ padding: "10px 16px" }}>{formatMetric(bucket.metrics.latency_ms, "latency")}</td>
                        <td style={{ padding: "10px 16px" }}>{formatMetric(bucket.metrics.completion_count)}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </div>
        {truncated && <button type="button" disabled={loading || !onQueryAnalytics}
          onClick={() => void fetchAnalytics(page?.next_cursor ?? undefined)}
          style={{ alignSelf: "flex-start", padding: "8px 14px", background: "var(--cody-inset)", color: "var(--cody-text)", border: "1px solid var(--cody-border-strong)", borderRadius: 6 }}>
          {loading ? "Loading more…" : "Load more measurements"}
        </button>}
      </div>
    </PageShell>
  );
};

function MeasurementCoverage({ buckets, metric }: { buckets: AnalyticsBucket[]; metric: keyof AnalyticsMetrics }) {
  const measured = buckets.filter((bucket) => bucket.metrics[metric] != null).length;
  if (measured === 0 || measured === buckets.length) return null;
  return <div style={{ marginTop: 4, fontSize: 12, fontWeight: 400, color: "var(--cody-warning-text)" }}>
    Partial: {measured} of {buckets.length} loaded buckets reported this metric
  </div>;
}
