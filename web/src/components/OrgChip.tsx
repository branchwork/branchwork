import { useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { useAuthStore } from "../stores/auth-store.js";
import { useOrgStore } from "../stores/org-store.js";
import { useRunnerStore } from "../stores/runner-store.js";

/// Bottom-right chrome cluster's org-context indicator. Shows the
/// current org name and member count; clickable when the user belongs
/// to >1 org so they can switch. Hidden on standalone deployments —
/// those have no org concept and the chip would be misleading.
///
/// Audit §17 gap: org name was previously shown nowhere except an
/// accidental hardcode in `AuditLog.tsx`. This component is the canonical
/// surface for org context.
///
/// Mode discriminator is shared with `RunnerStatus` — both render gates
/// read `useRunnerStore.mode`, so a SaaS box without a runner still
/// shows the org chip (the org concept is independent of runner
/// presence).
export function OrgChip() {
  const mode = useRunnerStore((s) => s.mode);
  const runnerLoaded = useRunnerStore((s) => s.loaded);
  const orgsLoaded = useOrgStore((s) => s.loaded);
  const memberships = useOrgStore((s) => s.memberships);
  const switchOrg = useOrgStore((s) => s.switchOrg);
  const user = useAuthStore((s) => s.user);

  const [open, setOpen] = useState(false);
  const [switching, setSwitching] = useState(false);
  const containerRef = useRef<HTMLDivElement | null>(null);

  // Click-outside dismiss for the dropdown. Mirrors the pattern used by
  // AgentPanel's split-button merge dropdown — bind on mousedown so
  // clicking *into* the dropdown (which sits inside `containerRef`) does
  // not close it before the action fires.
  useEffect(() => {
    if (!open) return;
    const onMouseDown = (e: MouseEvent) => {
      const target = e.target as Node | null;
      if (target && containerRef.current && !containerRef.current.contains(target)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, [open]);

  // Hide while either source-of-truth is still resolving — a flash of
  // "Default Organization" on a standalone box would be misleading. Both
  // `loaded` flags must be true OR mode must be `standalone` (in which
  // case we never render anything regardless of org-store state).
  if (!runnerLoaded) return null;
  if (mode === "standalone") return null;
  if (!orgsLoaded) return null;
  if (memberships.length === 0) return null;

  const currentOrgId = user?.orgId ?? null;
  const current =
    memberships.find((m) => m.id === currentOrgId) ?? memberships[0];
  if (!current) return null;

  const others = memberships.filter((m) => m.id !== current.id);
  const canSwitch = others.length > 0;
  const memberLabel =
    current.memberCount === 1 ? "1 member" : `${current.memberCount} members`;

  const onSwitch = async (orgId: string) => {
    setOpen(false);
    setSwitching(true);
    try {
      await switchOrg(orgId);
    } finally {
      setSwitching(false);
    }
  };

  return (
    <div
      ref={containerRef}
      data-testid="org-chip"
      data-can-switch={canSwitch}
      className="relative flex items-center gap-2"
    >
      {canSwitch ? (
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          disabled={switching}
          aria-haspopup="menu"
          aria-expanded={open}
          className="text-gray-300 hover:text-gray-100 transition disabled:opacity-50"
          title={`Switch organization (${memberships.length} available)`}
        >
          <span className="font-mono">{current.name}</span>
          <span aria-hidden="true" className="ml-1 text-gray-600">
            ▾
          </span>
        </button>
      ) : (
        <span className="text-gray-300 font-mono" title={current.name}>
          {current.name}
        </span>
      )}

      <Link
        to="/admin/members"
        data-testid="org-chip-member-count"
        className="text-[11px] px-1.5 py-0.5 rounded border border-gray-700 text-gray-400 hover:text-gray-200 hover:border-gray-500 transition"
        title="Manage members"
      >
        {memberLabel}
      </Link>

      {open && canSwitch && (
        <div
          role="menu"
          data-testid="org-chip-menu"
          className="absolute bottom-full mb-2 right-0 min-w-[200px] bg-gray-900 border border-gray-700 rounded shadow-lg py-1 z-50"
        >
          <div className="px-3 py-1 text-[11px] uppercase tracking-wide text-gray-500">
            Switch to
          </div>
          {others.map((m) => (
            <button
              key={m.id}
              type="button"
              role="menuitem"
              onClick={() => onSwitch(m.id)}
              className="w-full text-left px-3 py-1.5 text-xs text-gray-300 hover:bg-gray-800 transition"
            >
              <span className="font-mono">{m.name}</span>
              <span className="ml-2 text-gray-500">
                ({m.memberCount === 1 ? "1 member" : `${m.memberCount} members`})
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
