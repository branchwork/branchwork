import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { useLearningsStore, type PendingLearningRow } from "../stores/learnings-store.js";
import { useAuthStore } from "../stores/auth-store.js";
import { formatRelative } from "../lib/time.js";

/// "Learnings due" panel — surfaces every CI-failure event waiting on a
/// `capture_learning` (Phase 1, Task 1.4). Data source is
/// `GET /api/learnings/pending`; the panel hides itself when the queue
/// is empty (acceptance: "panel re-renders empty" once
/// `capture_learning` lands).
///
/// Drilldown lazily fetches the last 100 lines of the failing job's
/// log via `GET /api/learnings/pending/{id}/log` and shows it inline
/// inside a `<details>` block — `<summary>` toggling fires the fetch
/// once and the result stays cached in the store for the session.
///
/// Mount point: top of App.tsx's main content area, so it appears on
/// every route (Plans / Agents / Activity / etc.) while at least one
/// row is pending. Org-scoped via the same `OptionalAuthUser` flow
/// the rest of the dashboard uses — a logout-and-resignup-as-someone-
/// else flushes `useLearningsStore` via `lib/reset-all.ts`.
export function LearningsDuePanel() {
  const user = useAuthStore((s) => s.user);
  const items = useLearningsStore((s) => s.items);
  const loaded = useLearningsStore((s) => s.loaded);
  const fetchPending = useLearningsStore((s) => s.fetchPending);

  // Fire the initial fetch once we have an authenticated user. The WS
  // bootstrap in App.tsx already awaits the rest of the data stores;
  // this one is panel-local so it doesn't block the WS open.
  useEffect(() => {
    if (!user) return;
    fetchPending().catch(() => {});
  }, [user, fetchPending]);

  // Hide self while the first fetch hasn't landed AND the queue is
  // empty. After the fetch lands, the panel hides on `items.length===0`
  // (the acceptance criterion) and shows when there is at least one row.
  if (!user) return null;
  if (!loaded) return null;
  if (items.length === 0) return null;

  return (
    <section
      data-testid="learnings-due-panel"
      aria-labelledby="learnings-due-panel-heading"
      className="border-b border-rose-900/60 bg-rose-950/40 px-4 py-3 text-sm text-rose-100"
    >
      <header className="flex items-center justify-between gap-3 mb-2">
        <h2
          id="learnings-due-panel-heading"
          className="flex items-center gap-2 text-rose-100 font-medium"
        >
          <span aria-hidden="true" className="inline-block w-1.5 h-1.5 rounded-full bg-rose-400" />
          {items.length === 1
            ? "1 agent blocked on a CI failure — capture a learning to unblock"
            : `${items.length} agents blocked on CI failures — capture a learning to unblock`}
        </h2>
      </header>
      <ul className="space-y-1.5">
        {items.map((row) => (
          <PendingLearningRowView key={row.id} row={row} />
        ))}
      </ul>
    </section>
  );
}

function PendingLearningRowView({ row }: { row: PendingLearningRow }) {
  const logEntry = useLearningsStore((s) => s.logsByEventId[row.id]);
  const fetchLog = useLearningsStore((s) => s.fetchLog);
  const [open, setOpen] = useState(false);

  // Lazy: fetch the log only when the operator expands the row.
  // Reopening a previously-expanded row is free (the store caches the
  // result by `ci_failure_events.id`).
  useEffect(() => {
    if (open && !logEntry) {
      fetchLog(row.id).catch(() => {});
    }
  }, [open, logEntry, fetchLog, row.id]);

  const taskLabel = row.taskNumber ? ` / ${row.taskNumber}` : "";
  const workflowLabel = row.workflow ?? "workflow";
  const summary = row.summary ?? `${workflowLabel} failed`;
  const agentStatus = row.agentStatus ?? "unknown";

  return (
    <li className="rounded border border-rose-900/40 bg-rose-950/30">
      <details
        onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}
        className="px-3 py-2"
      >
        <summary className="cursor-pointer list-none flex items-baseline gap-2 flex-wrap text-sm">
          <span aria-hidden="true" className="text-rose-300">
            &#x25B8;
          </span>
          <Link
            to={`/plans/${encodeURIComponent(row.planName)}`}
            className="font-mono text-rose-100 hover:underline"
          >
            {row.planName}
            {taskLabel}
          </Link>
          <span className="text-rose-200/80">{summary}</span>
          <AgentStatusChip status={agentStatus} />
          {row.runUrl && (
            <a
              href={row.runUrl}
              target="_blank"
              rel="noopener noreferrer"
              className="ml-auto text-xs text-rose-300/80 hover:text-rose-100 hover:underline"
            >
              run #{row.runId} &#x2197;
            </a>
          )}
          <time
            className="text-xs text-rose-300/70"
            dateTime={row.observedAt}
            title={row.observedAt}
          >
            {formatRelative(row.observedAt)}
          </time>
        </summary>
        <div className="mt-2 text-xs">
          <LogView entry={logEntry} />
        </div>
      </details>
    </li>
  );
}

function AgentStatusChip({ status }: { status: string }) {
  // Colour map: running/starting = sky (in-flight), completed/failed/
  // killed = neutral (the agent has moved on; the failure is now a
  // back-log entry). Unknown = neutral grey.
  const palette =
    status === "running" || status === "starting"
      ? "border-sky-700/60 bg-sky-900/40 text-sky-100"
      : status === "failed" || status === "killed"
        ? "border-rose-700/60 bg-rose-900/40 text-rose-100"
        : "border-gray-700/60 bg-gray-900/40 text-gray-200";
  return (
    <span
      className={`inline-flex items-center px-1.5 py-0.5 rounded text-xs ${palette}`}
      title={`agents.status = ${status}`}
    >
      agent: {status}
    </span>
  );
}

function LogView({
  entry,
}: {
  entry:
    | { status: "loading" | "ok" | "error"; text: string | null; error: string | null }
    | undefined;
}) {
  if (!entry || entry.status === "loading") {
    return <p className="text-rose-300/70 italic">Fetching last 100 lines…</p>;
  }
  if (entry.status === "error") {
    return (
      <p className="text-rose-300/80">
        Log unavailable
        {entry.error ? `: ${entry.error}` : "."}
      </p>
    );
  }
  if (!entry.text) {
    return <p className="text-rose-300/70 italic">No log output captured.</p>;
  }
  return (
    <pre className="max-h-96 overflow-auto rounded bg-gray-950 border border-rose-900/40 p-2 font-mono text-[11px] leading-snug text-gray-200 whitespace-pre-wrap">
      {entry.text}
    </pre>
  );
}
