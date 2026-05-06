import { useLocation } from "react-router-dom";
import { useUiStore } from "../stores/ui-store.js";
import { useWsStore } from "../stores/ws-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { OrgChip } from "./OrgChip.js";
import { RunnerStatus } from "./RunnerStatus.js";

/// Compact top bar shown only on `<md` viewports (Tailwind 768px).
/// Replaces the bottom-right chrome cluster on mobile by reflowing
/// the org chip, runner status, and connection indicator up top, and
/// adds a hamburger that opens the sidebar slide-over via
/// `useUiStore.toggleSidebar()`.
///
/// On `≥md` this entire component is hidden by Tailwind's responsive
/// classes; the sidebar stays persistent and the bottom-right cluster
/// in `App.tsx` carries the same items as before.
export function MobileTopBar() {
  const toggleSidebar = useUiStore((s) => s.toggleSidebar);
  const connected = useWsStore((s) => s.connected);
  const title = useViewTitle();

  return (
    <header
      data-testid="mobile-top-bar"
      className="md:hidden flex items-center gap-2 px-3 py-2 bg-gray-900 border-b border-gray-800 text-xs text-gray-500 flex-shrink-0"
    >
      <button
        type="button"
        data-testid="mobile-top-bar-hamburger"
        onClick={toggleSidebar}
        aria-label="Open menu"
        className="p-1.5 -ml-1 rounded text-gray-400 hover:text-gray-100 hover:bg-gray-800 transition flex-shrink-0"
      >
        <HamburgerIcon />
      </button>
      <h1
        data-testid="mobile-top-bar-title"
        className="text-sm font-semibold text-gray-100 truncate flex-1 min-w-0"
        title={title}
      >
        {title}
      </h1>
      <div className="flex items-center gap-2 min-w-0">
        <OrgChip placement="below" />
        <RunnerStatus />
        <span
          data-testid="mobile-top-bar-connection"
          aria-label={connected ? "Connected" : "Disconnected"}
          title={connected ? "Connected" : "Disconnected"}
          className={`inline-block w-2 h-2 rounded-full flex-shrink-0 ${
            connected ? "bg-emerald-500" : "bg-red-500"
          }`}
        />
      </div>
    </header>
  );
}

function HamburgerIcon() {
  // Inline SVG keeps the bundle from pulling in a heroicons-style
  // library for a single icon.
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      width="20"
      height="20"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <line x1="3" y1="6" x2="21" y2="6" />
      <line x1="3" y1="12" x2="21" y2="12" />
      <line x1="3" y1="18" x2="21" y2="18" />
    </svg>
  );
}

const PLAN_PATH_RE = /^\/plans\/([^/]+)\/?$/;
const AGENT_PATH_RE = /^\/agents(?:\/[^/]+)?\/?$/;
const ADMIN_PATH_RE = /^\/admin(?:\/[^/]+)?\/?$/;

/// Derive the view title from the URL plus, for a plan deep-link, the
/// selected plan's title. Falls back to the brand name on unknown
/// routes (matches `<NotFoundPage />` semantics — better than showing
/// an empty title).
function useViewTitle(): string {
  const { pathname } = useLocation();
  const selectedPlan = usePlanStore((s) => s.selectedPlan);

  if (pathname === "/" || pathname === "/plans" || pathname === "/plans/") {
    return "Plans";
  }
  const planMatch = PLAN_PATH_RE.exec(pathname);
  if (planMatch) {
    return selectedPlan?.title ?? safeDecode(planMatch[1]);
  }
  if (AGENT_PATH_RE.test(pathname)) return "Agents";
  if (pathname === "/audit" || pathname === "/audit/") return "Activity";
  if (pathname === "/archive" || pathname === "/archive/") return "Archive";
  if (ADMIN_PATH_RE.test(pathname)) return "Admin";
  if (pathname === "/runners" || pathname === "/runners/") return "Runners";
  if (pathname === "/new-plan" || pathname === "/new-plan/") return "New Plan";
  return "Branchwork";
}

function safeDecode(s: string): string {
  try {
    return decodeURIComponent(s);
  } catch {
    return s;
  }
}
