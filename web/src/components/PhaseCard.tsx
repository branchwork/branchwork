import { memo, useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import type { PlanPhase, PlanTask } from "../stores/plan-store.js";
import { postJson } from "../api.js";
import { TaskCard } from "./TaskCard.js";
import { Button } from "./ui/Button.js";
import { Modal } from "./ui/Modal.js";
import { toastError } from "../lib/toast.js";
import { useScrollParent } from "../hooks/use-scroll-parent.js";

interface Props {
  phase: PlanPhase;
  planName: string;
  statusFilter?: string | null;
}

// Matches the Tailwind breakpoints used by the task grid below
// (`md:grid-cols-2 xl:grid-cols-3`).
function pickColumns(width: number): number {
  if (width >= 1280) return 3;
  if (width >= 768) return 2;
  return 1;
}

function useResponsiveColumns(): number {
  const [cols, setCols] = useState<number>(() =>
    typeof window === "undefined" ? 3 : pickColumns(window.innerWidth),
  );
  useEffect(() => {
    function update() {
      setCols(pickColumns(window.innerWidth));
    }
    window.addEventListener("resize", update);
    return () => window.removeEventListener("resize", update);
  }, []);
  return cols;
}

interface VirtualizedTaskGridProps {
  tasks: PlanTask[];
  planName: string;
  phaseNumber: number;
}

// Below this count we render the original full grid (cheaper than the
// virtualiser's overhead for small lists, and avoids a host of edge
// cases — jsdom layout, focus management, drag-and-drop, etc.). At or
// above the threshold we switch to row virtualisation; the brief's
// acceptance test (200 tasks → ~30 in DOM) is comfortably above this.
export const VIRTUALIZATION_THRESHOLD = 30;

// Row-based virtualisation: tasks are chunked into rows of `cols`
// (1/2/3 to mirror the Tailwind grid), each row is a virtual item.
// Dynamic measurement via `virtualizer.measureElement` lets variable
// TaskCard heights compose without a fixed estimate.
function VirtualizedTaskGrid({
  tasks,
  planName,
  phaseNumber,
}: VirtualizedTaskGridProps) {
  const cols = useResponsiveColumns();
  const containerRef = useRef<HTMLDivElement>(null);
  const scrollEl = useScrollParent(containerRef);

  const rows = useMemo(() => {
    const out: PlanTask[][] = [];
    for (let i = 0; i < tasks.length; i += cols) {
      out.push(tasks.slice(i, i + cols));
    }
    return out;
  }, [tasks, cols]);

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollEl,
    estimateSize: () => 140,
    overscan: 4,
    gap: 8,
    // Seed jsdom (and SSR / first paint before layout) with a plausible
    // viewport so initial render isn't empty.
    initialRect: { width: 1280, height: 800 },
  });

  const items = virtualizer.getVirtualItems();
  const colsClass =
    cols === 3
      ? "grid-cols-3"
      : cols === 2
        ? "grid-cols-2"
        : "grid-cols-1";

  return (
    <div ref={containerRef} className="px-3 pb-3">
      <div
        style={{
          height: `${virtualizer.getTotalSize()}px`,
          position: "relative",
          width: "100%",
        }}
      >
        {items.map((virtualRow) => {
          const row = rows[virtualRow.index];
          return (
            <div
              key={virtualRow.key}
              data-index={virtualRow.index}
              ref={virtualizer.measureElement}
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                width: "100%",
                transform: `translateY(${virtualRow.start}px)`,
              }}
              className={`grid ${colsClass} gap-2`}
            >
              {row.map((task) => (
                <TaskCard
                  key={task.number}
                  task={task}
                  planName={planName}
                  phaseNumber={phaseNumber}
                />
              ))}
            </div>
          );
        })}
      </div>
    </div>
  );
}

function StaticTaskGrid({
  tasks,
  planName,
  phaseNumber,
}: VirtualizedTaskGridProps) {
  return (
    <div className="px-3 pb-3 grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-2">
      {tasks.map((task) => (
        <TaskCard
          key={task.number}
          task={task}
          planName={planName}
          phaseNumber={phaseNumber}
        />
      ))}
    </div>
  );
}

function TaskGrid(props: VirtualizedTaskGridProps) {
  return props.tasks.length >= VIRTUALIZATION_THRESHOLD ? (
    <VirtualizedTaskGrid {...props} />
  ) : (
    <StaticTaskGrid {...props} />
  );
}

function PhaseCardInner({ phase, planName, statusFilter }: Props) {
  const total = phase.tasks.length;
  const done = phase.tasks.filter(
    (t) => t.status === "completed" || t.status === "skipped"
  ).length;
  const inProgress = phase.tasks.filter(
    (t) => t.status === "in_progress"
  ).length;
  const pending = total - done - inProgress;
  const pct = total > 0 ? Math.round((done / total) * 100) : 0;
  const allDone = total > 0 && done === total;

  // Done phases start collapsed, active phases start expanded
  const [expanded, setExpanded] = useState(!allDone);
  const [checking, setChecking] = useState(false);
  const [confirmCheck, setConfirmCheck] = useState(false);

  const filteredTasks = statusFilter
    ? phase.tasks.filter((t) => (t.status ?? "pending") === statusFilter)
    : phase.tasks;

  const eligibleCount = phase.tasks.filter(
    (t) => !["completed", "skipped", "checking"].includes(t.status ?? "pending")
  ).length;

  function openConfirm(e: React.MouseEvent) {
    e.stopPropagation();
    if (eligibleCount === 0) return;
    setConfirmCheck(true);
  }

  async function runCheckPhase() {
    setConfirmCheck(false);
    setChecking(true);
    try {
      await postJson(`/api/plans/${planName}/check-all`, { phase: phase.number });
    } catch (e) {
      toastError(e, "Check phase failed");
    } finally {
      setChecking(false);
    }
  }

  return (
    <div
      className={`rounded-lg border transition ${
        allDone
          ? "border-gray-800/50 bg-gray-900/50"
          : "border-gray-800 bg-gray-900"
      }`}
    >
      {/* Phase header — always visible, clickable. Uses a div instead of a
        * button so the nested "Check" <button> below stays HTML-valid (a
        * <button> cannot legally contain another <button>; React flags it
        * as a hydration error in dev). */}
      <div
        role="button"
        tabIndex={0}
        onClick={() => setExpanded(!expanded)}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            setExpanded(!expanded);
          }
        }}
        className="w-full text-left p-3 hover:bg-gray-800/30 transition rounded-lg cursor-pointer"
      >
        <div className="flex items-center gap-3">
          {/* Expand arrow */}
          <span className={`text-[10px] text-gray-600 transition-transform ${expanded ? "rotate-90" : ""}`}>
            &#9654;
          </span>

          {/* Phase badge */}
          <span
            className={`text-xs font-mono px-1.5 py-0.5 rounded flex-shrink-0 ${
              allDone
                ? "bg-emerald-600/20 text-emerald-400"
                : inProgress > 0
                ? "bg-amber-600/20 text-amber-400"
                : "bg-indigo-600/20 text-indigo-400"
            }`}
          >
            Phase {phase.number}
          </span>

          {/* Title */}
          <span className={`text-sm font-semibold truncate ${allDone ? "text-gray-500" : "text-gray-200"}`}>
            {phase.title}
          </span>

          {/* Stats */}
          <div className="ml-auto flex items-center gap-3 flex-shrink-0">
            {/* Status counts */}
            <div className="flex items-center gap-2 text-[10px]">
              {done > 0 && (
                <span className="text-emerald-400">{done} done</span>
              )}
              {inProgress > 0 && (
                <span className="text-amber-400">{inProgress} active</span>
              )}
              {pending > 0 && (
                <span className="text-gray-500">{pending} pending</span>
              )}
            </div>

            {/* Progress bar */}
            {total > 0 && (
              <div className="w-24 h-1.5 bg-gray-800 rounded-full overflow-hidden flex-shrink-0">
                <div
                  className={`h-full rounded-full transition-all duration-300 ${
                    allDone ? "bg-emerald-500" : "bg-indigo-500"
                  }`}
                  style={{ width: `${pct}%` }}
                />
              </div>
            )}

            {/* Check phase button (only when there's something to check) */}
            {!allDone && (
              <button
                onClick={openConfirm}
                disabled={checking || eligibleCount === 0}
                className="flex-shrink-0 px-2 py-0.5 text-[10px] bg-gray-800 border border-gray-700 hover:border-emerald-600 hover:text-emerald-400 disabled:opacity-50 text-gray-400 rounded transition"
                title="Spawn a check agent for every unfinished task in this phase"
              >
                {checking ? "..." : "Check"}
              </button>
            )}

            {/* Percentage */}
            <span className={`text-xs font-mono w-8 text-right ${allDone ? "text-emerald-400" : "text-gray-500"}`}>
              {pct}%
            </span>
          </div>
        </div>
      </div>

      {/* Expanded: show tasks. Audit §10 major: plans with 100+ tasks
          per phase used to render every card; TaskGrid switches to a
          row virtualiser at VIRTUALIZATION_THRESHOLD so the DOM stays
          bounded to the visible viewport plus a small overscan. Phases
          with fewer tasks render the full grid for simpler keyboard
          navigation and zero virtualiser overhead. */}
      {expanded && filteredTasks.length > 0 && (
        <TaskGrid
          tasks={filteredTasks}
          planName={planName}
          phaseNumber={phase.number}
        />
      )}

      {expanded && filteredTasks.length === 0 && phase.tasks.length > 0 && (
        <div className="px-3 pb-3">
          <p className="text-xs text-gray-600">No matching tasks</p>
        </div>
      )}
      <Modal
        open={confirmCheck}
        onClose={() => setConfirmCheck(false)}
        title={`Check Phase ${phase.number}`}
        description={`Spawn ${eligibleCount} check agents for Phase ${phase.number}? This will use API credits.`}
      >
        <div className="mt-4 flex justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={() => setConfirmCheck(false)}>
            Cancel
          </Button>
          <Button variant="primary" size="sm" onClick={runCheckPhase}>
            Spawn {eligibleCount}
          </Button>
        </div>
      </Modal>
    </div>
  );
}

/// Wrapped with `memo` so a re-render of `PlanBoard` (e.g. a status-filter
/// toggle that doesn't change `phase`) doesn't fan out to every phase.
/// Re-renders are gated on the props `phase` / `planName` / `statusFilter`.
/// Audit §10 minor.
export const PhaseCard = memo(PhaseCardInner);
