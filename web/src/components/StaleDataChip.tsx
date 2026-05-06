import { useEffect, useState } from "react";
import { useWsStore } from "../stores/ws-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";

/// How old `lastFetchedAt` must be before we surface the chip. Tuned to
/// be longer than typical fetch latency + a comfortable jitter so the
/// chip only appears once "live updates would normally have arrived by
/// now". Shorter than the human attention span on a dashboard tab so a
/// user who comes back to a stale view sees the warning before they act
/// on what they're reading.
export const STALE_DATA_THRESHOLD_MS = 60_000;

/// How often we re-evaluate the chip. The threshold check fires on
/// connected/disconnect *and* on every tick, so a chip on screen will
/// disappear within a tick of reconnect even if no other store update
/// re-renders the parent.
const STALE_DATA_TICK_MS = 5_000;

/// Which slice's `lastFetchedAt` drives the chip. Plan rows pass `slice="plans"`,
/// agent rows pass `slice="agents"`. Add a new slice when a third store
/// gets a `lastFetchedAt` field.
export type StaleDataSlice = "plans" | "agents";

interface StaleDataChipProps {
  slice: StaleDataSlice;
  /// Optional className for layout positioning. Defaults to `inline`
  /// (no margins) so callers can drop the chip anywhere.
  className?: string;
}

/// Inline chip indicating that the data adjacent to it may be stale.
/// Renders only when BOTH:
///   1. WS is disconnected (we have no live updates), AND
///   2. the relevant `lastFetchedAt` is more than 60s old.
///
/// The "and" gate is deliberate: when WS is connected, every mutation
/// arrives as an event so the locally cached data IS fresh. When the
/// fetch was recent, the data is also fresh — the chip is reserved for
/// the genuinely-uncertain case (no WS + cache is old).
///
/// Audit §15 (major) — without this the user has to guess whether
/// what they're looking at reflects current server state.
export function StaleDataChip({ slice, className }: StaleDataChipProps) {
  const connected = useWsStore((s) => s.connected);
  const lastFetchedAt = usePlanLastFetchedAtForSlice(slice);
  const [tick, setTick] = useState(0);

  // Re-render every STALE_DATA_TICK_MS so we cross the threshold even
  // when no parent update is happening. Cheap: a single setInterval per
  // mounted chip.
  useEffect(() => {
    if (connected) return;
    const id = setInterval(() => setTick((t) => t + 1), STALE_DATA_TICK_MS);
    return () => clearInterval(id);
  }, [connected]);
  // Suppress unused warning — `tick` is the trigger for re-render.
  void tick;

  if (connected) return null;
  if (lastFetchedAt === null) return null;
  const ageMs = Date.now() - lastFetchedAt;
  if (ageMs < STALE_DATA_THRESHOLD_MS) return null;

  const ageSeconds = Math.floor(ageMs / 1000);
  const ageLabel =
    ageSeconds < 60
      ? `${ageSeconds}s ago`
      : ageSeconds < 3600
        ? `${Math.floor(ageSeconds / 60)}m ago`
        : `${Math.floor(ageSeconds / 3600)}h ago`;

  const classes = [
    "inline-flex items-center gap-1 rounded border px-1.5 py-px text-[10px] font-medium",
    "border-amber-700/60 bg-amber-900/40 text-amber-200",
    className,
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <span
      data-testid={`stale-chip-${slice}`}
      className={classes}
      title={`Last refreshed ${ageLabel}. Disconnected from server, so this may be out of date.`}
    >
      <span
        aria-hidden="true"
        className="inline-block w-1.5 h-1.5 rounded-full bg-amber-400"
      />
      stale
    </span>
  );
}

/// Pull `lastFetchedAt` for the requested slice. Kept as a single hook
/// so call sites only ever subscribe to ONE store value, regardless of
/// slice — switching slices doesn't change the subscription shape.
function usePlanLastFetchedAtForSlice(slice: StaleDataSlice): number | null {
  const plansFetchedAt = usePlanStore((s) => s.lastPlansFetchedAt);
  const agentsFetchedAt = useAgentStore((s) => s.lastAgentsFetchedAt);
  return slice === "plans" ? plansFetchedAt : agentsFetchedAt;
}
