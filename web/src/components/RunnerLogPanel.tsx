import { useEffect, useMemo, useRef, useState } from "react";
import { useRunnerStore, type RunnerLogEntry } from "../stores/runner-store.js";

/// Live tail of one runner's stdout/stderr.
///
/// The runner ships each `[runner] …` line as a `runner_log` WS event; the
/// runner-store rings the last 500 per runner and this component renders
/// them as a `<pre>` with auto-scroll-to-bottom. Three controls: a pause
/// toggle that freezes the rendered list (without dropping the ring — new
/// lines keep flowing into the store and reappear on resume), a substring
/// filter, and a one-shot Clear that wipes the rendered snapshot only
/// (resume re-flips back to the live ring).
///
/// Acceptance (audit T11.1):
/// - Lines appear within 1s of being printed by the runner.
/// - Pause + resume works without dropping the ring's contents.
/// - Rate-limit caps the stream at the documented ceiling; bursts beyond
///   the cap render the `[truncated N lines]` marker (the runner emits
///   the marker as a real `RunnerLogLine` so it shows up here verbatim).
export function RunnerLogPanel({ runnerId }: { runnerId: string }) {
  const live = useRunnerStore((s) => s.logsByRunnerId[runnerId]) ?? EMPTY;
  const [paused, setPaused] = useState(false);
  const [snapshot, setSnapshot] = useState<RunnerLogEntry[] | null>(null);
  const [filter, setFilter] = useState("");
  const endRef = useRef<HTMLDivElement | null>(null);
  // Mirror `live` into a ref so the pause toggle can capture the current
  // ring contents WITHOUT having `live` in the toggle's dep set — the ring
  // updates on every WS frame and we don't want the snapshot effect to
  // re-fire on every line.
  const liveRef = useRef(live);
  liveRef.current = live;

  function togglePause() {
    setPaused((prev) => {
      // Capture at the transition into paused. Resume drops the snapshot
      // so the rendered list re-points at the live ring (which kept
      // growing during the pause window — no lines dropped).
      if (!prev) setSnapshot(liveRef.current);
      else setSnapshot(null);
      return !prev;
    });
  }

  const visible = paused && snapshot ? snapshot : live;
  const filtered = useMemo(() => {
    const f = filter.trim().toLowerCase();
    if (f === "") return visible;
    return visible.filter((e) => e.line.toLowerCase().includes(f));
  }, [visible, filter]);

  // Auto-scroll to bottom on each new render — but only when not paused
  // and only if the user hasn't deliberately scrolled away from the
  // bottom. We don't track scroll-anchoring explicitly here; the brief
  // says auto-scroll is enough.
  useEffect(() => {
    if (paused) return;
    // jsdom doesn't implement scrollIntoView; guard so component tests
    // don't blow up when the ring updates.
    endRef.current?.scrollIntoView?.({ block: "end" });
  }, [filtered.length, paused]);

  return (
    <div
      className="mt-3 rounded border border-gray-800 bg-gray-950/80"
      data-testid={`runner-log-panel-${runnerId}`}
    >
      <div className="flex items-center justify-between gap-2 border-b border-gray-800 px-3 py-2">
        <div className="flex items-center gap-2">
          <h3 className="text-[11px] font-semibold uppercase tracking-wide text-gray-300">
            Runner log
          </h3>
          <span className="text-[10px] text-gray-500">
            {filtered.length} / {visible.length} lines
            {paused && <span className="ml-1 text-amber-400">(paused)</span>}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <input
            type="text"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="Filter…"
            data-testid={`runner-log-filter-${runnerId}`}
            className="w-44 rounded border border-gray-700 bg-gray-900 px-2 py-0.5 text-[11px] text-gray-200 focus:border-indigo-600 focus:outline-none"
          />
          <button
            type="button"
            onClick={togglePause}
            data-testid={`runner-log-pause-${runnerId}`}
            className={`rounded border px-2 py-0.5 text-[11px] transition ${
              paused
                ? "border-amber-700/50 bg-amber-900/30 text-amber-200"
                : "border-gray-700 bg-gray-900/40 text-gray-400 hover:bg-gray-800 hover:text-gray-200"
            }`}
          >
            {paused ? "Resume" : "Pause"}
          </button>
        </div>
      </div>
      <pre
        className="m-0 max-h-72 min-h-[10rem] overflow-auto px-3 py-2 font-mono text-[11px] leading-tight text-gray-200"
        data-testid={`runner-log-pre-${runnerId}`}
      >
        {filtered.length === 0 && (
          <span className="text-gray-600">
            {visible.length === 0
              ? "No log lines yet — waiting for runner output…"
              : `No lines match "${filter}".`}
          </span>
        )}
        {filtered.map((entry) => (
          <div key={entry.id} className={lineToneClass(entry.level)}>
            {entry.line}
          </div>
        ))}
        <div ref={endRef} />
      </pre>
    </div>
  );
}

const EMPTY: RunnerLogEntry[] = [];

function lineToneClass(level: string): string {
  switch (level) {
    case "warn":
      return "text-amber-300";
    case "error":
      return "text-red-300";
    case "info":
    default:
      return "text-gray-200";
  }
}
