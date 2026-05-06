import { useEffect, useState } from "react";
import { useWsStore } from "../stores/ws-store.js";

/// How long the WS must be disconnected before we surface the banner.
/// Tuned to absorb short blips (HMR reload, brief network glitch, server
/// restart that comes back inside ~2s) so the user doesn't see a flash
/// of warning copy on every reconnect. Longer outages cross the
/// threshold and the banner stays up until reconnect.
export const CONNECTION_BANNER_GRACE_MS = 2_000;

/// Persistent top-of-viewport banner shown when the WebSocket has been
/// disconnected for longer than `CONNECTION_BANNER_GRACE_MS`. Auto-
/// dismisses on reconnect (the grace timer also clears so a flap doesn't
/// stick the banner up). Audit §15 (major) — promotes the previous 2px
/// bottom-right dot from "easily missed" to "obviously broken".
///
/// The bottom-right indicator (in App.tsx) is intentionally retained as
/// a secondary cue — the banner is the loud signal during outages, the
/// dot is the always-on health indicator.
export function ConnectionBanner() {
  const connected = useWsStore((s) => s.connected);
  const disconnectedSince = useWsStore((s) => s.disconnectedSince);
  const [show, setShow] = useState(false);

  useEffect(() => {
    if (connected || disconnectedSince === null) {
      setShow(false);
      return;
    }
    // If we've already crossed the threshold (e.g. component just
    // mounted while disconnect was already in progress), skip the
    // setTimeout and render immediately.
    const elapsed = Date.now() - disconnectedSince;
    if (elapsed >= CONNECTION_BANNER_GRACE_MS) {
      setShow(true);
      return;
    }
    const remaining = CONNECTION_BANNER_GRACE_MS - elapsed;
    const id = setTimeout(() => setShow(true), remaining);
    return () => clearTimeout(id);
  }, [connected, disconnectedSince]);

  if (!show) return null;

  return (
    <div
      role="alert"
      aria-live="polite"
      data-testid="connection-banner"
      className="fixed top-0 inset-x-0 z-50 border-b border-amber-700/60 bg-amber-900/80 backdrop-blur px-4 py-2 text-center text-sm text-amber-100 shadow-lg"
    >
      <span
        aria-hidden="true"
        className="inline-block w-2 h-2 rounded-full bg-amber-400 animate-pulse mr-2 align-middle"
      />
      <span>Disconnected. Reconnecting&hellip;</span>
    </div>
  );
}
