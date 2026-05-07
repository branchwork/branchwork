import { useMemo } from "react";
import {
  ORPHANS_REAPED_RED_THRESHOLD,
  OUTBOX_DEPTH_RED_THRESHOLD,
  WS_RECONNECTS_RED_THRESHOLD,
  useRunnerStore,
  type AgentCounts,
  type Runner,
} from "../stores/runner-store.js";
import { formatRelative } from "../lib/time.js";

/// Per-runner Health panel (T11.3).
///
/// Surfaces the metrics already in the DB and on the wire: CI poll
/// latency percentiles, 24 h reconnect count, outbox depth, server-vs-
/// runner version verdict, and a pie of deployment-wide agent counts by
/// status. Renders inline under the selected runner row in `RunnersPage`
/// (the brief mentions a `/runners/{id}` URL but the dashboard's
/// per-runner UX is the existing Select toggle + log panel — keeping the
/// Health panel inline avoids cutting a dedicated route just for this).
///
/// Reads exclusively from the runner-store. Live updates come from the
/// `runner_health` WS handler in `ws-store.ts`; the store applies the
/// snapshot in place so re-renders are cheap.
///
/// Pie chart is a CSS conic-gradient (no D3 / chart lib). Sparkline is
/// inline SVG over the last ~60 minutes' p50 samples reconstructed from
/// the latest few snapshots — the runner's in-memory ring keeps the
/// per-poll values; the wire snapshot only ships the percentiles, so
/// the dashboard sparkline tracks "latest snapshot" cadence (one point
/// per ~30 s tick), not raw samples. Honest fall-back: if there are no
/// samples yet, render a neutral "—" rather than a flat line at 0.
export function RunnerHealthPanel({ runnerId }: { runnerId: string }) {
  const runner = useRunnerStore((s) => s.runners.find((r) => r.id === runnerId)) ?? null;
  const agentCounts = useRunnerStore((s) => s.agentCounts);
  const serverVersion = useRunnerStore((s) => s.serverVersion);
  const history = useRunnerStore((s) => s.healthHistoryByRunnerId[runnerId]) ?? EMPTY_HISTORY;

  if (!runner) {
    return null;
  }

  const health = runner.health;
  const outbox = health?.outboxDepth ?? null;
  const reconnects = health?.wsReconnects24h ?? null;
  const p50 = health?.ciPollMsP50 ?? null;
  const p99 = health?.ciPollMsP99 ?? null;
  const lastHealthAt = health?.lastHealthAt ?? null;
  const orphansReaped = health?.orphansReaped24h ?? null;

  return (
    <div
      data-testid={`runner-health-panel-${runnerId}`}
      className="mt-3 rounded border border-gray-800 bg-gray-900/40 p-3"
    >
      <div className="flex items-center justify-between mb-3">
        <h3 className="text-xs font-semibold text-gray-300">Health</h3>
        <div className="text-[11px] text-gray-500">
          {lastHealthAt ? (
            <>Updated {formatRelative(lastHealthAt)}</>
          ) : (
            <span className="text-gray-600">No health snapshot yet</span>
          )}
        </div>
      </div>
      <div className="grid grid-cols-1 md:grid-cols-3 gap-3">
        <AgentStatusPie counts={agentCounts} />
        <CiPollSparkline history={history} latestP50={p50} latestP99={p99} />
        <div className="flex flex-col gap-2">
          <ReconnectMetric value={reconnects} />
          <OutboxMetric value={outbox} />
          <OrphansReapedMetric value={orphansReaped} />
          <VersionMetric runner={runner} serverVersion={serverVersion} />
        </div>
      </div>
    </div>
  );
}

// ── Agent counts pie ────────────────────────────────────────────────────────

const AGENT_STATUS_ORDER: { key: string; label: string; color: string }[] = [
  { key: "running", label: "Running", color: "#10b981" },
  { key: "starting", label: "Starting", color: "#facc15" },
  { key: "completed", label: "Completed", color: "#6366f1" },
  { key: "failed", label: "Failed", color: "#ef4444" },
  { key: "killed", label: "Killed", color: "#9ca3af" },
];

function AgentStatusPie({ counts }: { counts: AgentCounts }) {
  const slices = AGENT_STATUS_ORDER.map((s) => ({
    ...s,
    count: counts[s.key] ?? 0,
  }));
  const total = slices.reduce((sum, s) => sum + s.count, 0);

  // Build the conic-gradient stops. When the total is 0, render a flat
  // gray ring so the panel layout doesn't collapse.
  const gradient = useMemo(() => {
    if (total === 0) return "conic-gradient(#374151 0% 100%)";
    let cursor = 0;
    const stops: string[] = [];
    for (const s of slices) {
      if (s.count === 0) continue;
      const start = (cursor / total) * 100;
      cursor += s.count;
      const end = (cursor / total) * 100;
      stops.push(`${s.color} ${start}% ${end}%`);
    }
    return `conic-gradient(${stops.join(", ")})`;
  }, [slices, total]);

  return (
    <div className="flex items-start gap-3">
      <div
        aria-hidden="true"
        className="w-16 h-16 rounded-full shrink-0"
        style={{ background: gradient }}
      />
      <div className="text-[11px]">
        <div className="text-gray-400 mb-1">Agents (deployment-wide)</div>
        <ul className="space-y-0.5">
          {slices.map((s) => (
            <li key={s.key} className="flex items-center gap-1.5 text-gray-500">
              <span
                aria-hidden="true"
                className="inline-block w-2 h-2 rounded-sm"
                style={{ backgroundColor: s.color, opacity: s.count > 0 ? 1 : 0.25 }}
              />
              <span>{s.label}</span>
              <span className="text-gray-300 font-mono ml-auto">{s.count}</span>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}

// ── CI poll sparkline ───────────────────────────────────────────────────────

interface HealthSample {
  /// Wall-clock when this sample was applied.
  ts: number;
  p50: number | null;
  p99: number | null;
}

const EMPTY_HISTORY: HealthSample[] = [];

const SPARKLINE_WIDTH = 120;
const SPARKLINE_HEIGHT = 36;

function CiPollSparkline({
  history,
  latestP50,
  latestP99,
}: {
  history: HealthSample[];
  latestP50: number | null;
  latestP99: number | null;
}) {
  // Only points with a non-null p50 contribute to the line. Cold
  // runners or plans without GitHub Actions stay blank (no zero spike).
  const samples = history.map((h) => h.p50).filter((v): v is number => v !== null);
  const max = samples.length === 0 ? 0 : Math.max(...samples);

  const path = useMemo(() => {
    if (samples.length < 2) return "";
    const stepX = SPARKLINE_WIDTH / Math.max(samples.length - 1, 1);
    const ymax = Math.max(max, 1);
    return samples
      .map((v, i) => {
        const x = i * stepX;
        const y = SPARKLINE_HEIGHT - (v / ymax) * SPARKLINE_HEIGHT;
        return `${i === 0 ? "M" : "L"}${x.toFixed(1)},${y.toFixed(1)}`;
      })
      .join(" ");
  }, [samples, max]);

  return (
    <div className="flex flex-col text-[11px]">
      <div className="text-gray-400 mb-1">CI poll latency</div>
      <div className="flex items-end gap-3">
        <svg
          aria-hidden="true"
          width={SPARKLINE_WIDTH}
          height={SPARKLINE_HEIGHT}
          className="bg-gray-950/40 rounded"
          data-testid="ci-poll-sparkline"
        >
          {path ? (
            <path d={path} fill="none" stroke="#60a5fa" strokeWidth="1.5" />
          ) : (
            <line
              x1={0}
              y1={SPARKLINE_HEIGHT - 1}
              x2={SPARKLINE_WIDTH}
              y2={SPARKLINE_HEIGHT - 1}
              stroke="#374151"
              strokeWidth="1"
            />
          )}
        </svg>
        <div className="font-mono space-y-0.5">
          <div className="text-gray-500">
            p50 <span className="text-gray-200">{latestP50 !== null ? `${latestP50}ms` : "—"}</span>
          </div>
          <div className="text-gray-500">
            p99 <span className="text-gray-200">{latestP99 !== null ? `${latestP99}ms` : "—"}</span>
          </div>
        </div>
      </div>
    </div>
  );
}

// ── Single-number metrics ───────────────────────────────────────────────────

function ReconnectMetric({ value }: { value: number | null }) {
  const danger = value !== null && value > WS_RECONNECTS_RED_THRESHOLD;
  return (
    <MetricRow
      label="Reconnects (24h)"
      value={value === null ? "—" : value.toString()}
      tone={danger ? "danger" : "neutral"}
      data-testid="reconnect-metric"
    />
  );
}

function OutboxMetric({ value }: { value: number | null }) {
  const danger = value !== null && value > OUTBOX_DEPTH_RED_THRESHOLD;
  return (
    <MetricRow
      label="Outbox depth"
      value={value === null ? "—" : value.toString()}
      tone={danger ? "danger" : "neutral"}
      data-testid="outbox-metric"
    />
  );
}

function OrphansReapedMetric({ value }: { value: number | null }) {
  // null ⇒ runner pre-dates Task 11.6, no chip warning. 0 ⇒ healthy
  // (neutral). >0 ⇒ silent rot has happened and the runner caught it
  // (warn). Above the red threshold ⇒ steady-state failure (danger).
  let tone: MetricTone = "neutral";
  if (value !== null && value > 0) {
    tone = value > ORPHANS_REAPED_RED_THRESHOLD ? "danger" : "warn";
  }
  return (
    <MetricRow
      label="Orphans reaped (24h)"
      value={value === null ? "—" : value.toString()}
      tone={tone}
      data-testid="orphans-reaped-metric"
    />
  );
}

function VersionMetric({
  runner,
  serverVersion,
}: {
  runner: Runner;
  serverVersion: string | null;
}) {
  const verdict = runner.versionMismatch ?? "ok";
  let tone: MetricTone = "neutral";
  const label = "Version";
  let icon = "✓";
  if (verdict === "patch") {
    tone = "neutral";
    icon = "✓";
  } else if (verdict === "minor") {
    tone = "warn";
    icon = "⚠";
  } else if (verdict === "major") {
    tone = "danger";
    icon = "⛔";
  }
  const value =
    runner.version && serverVersion && runner.version !== serverVersion
      ? `${runner.version} → ${serverVersion}`
      : (runner.version ?? "—");
  return (
    <MetricRow
      label={label}
      value={
        <span className="font-mono">
          <span aria-hidden="true">{icon} </span>
          {value}
        </span>
      }
      tone={tone}
      data-testid="version-metric"
    />
  );
}

// ── Shared metric row ───────────────────────────────────────────────────────

type MetricTone = "neutral" | "warn" | "danger";

function MetricRow({
  label,
  value,
  tone,
  ...rest
}: {
  label: string;
  value: React.ReactNode;
  tone: MetricTone;
  "data-testid"?: string;
}) {
  const valueClass =
    tone === "danger" ? "text-red-300" : tone === "warn" ? "text-amber-300" : "text-gray-200";
  return (
    <div
      className="flex items-center justify-between text-[11px] text-gray-500"
      data-testid={rest["data-testid"]}
    >
      <span>{label}</span>
      <span className={valueClass}>{value}</span>
    </div>
  );
}
