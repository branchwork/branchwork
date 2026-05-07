import { useCallback } from "react";
import { useLocation, useNavigate, useSearchParams } from "react-router-dom";

/// Pulls "what is selected" out of the URL — the source of truth after
/// the routing migration in 1.1. Plan name is a path param on
/// /plans/:planName. Agent id can come either from the deep-link path
/// /agents/:agentId or from a `?agent=<id>` search param layered on any
/// other route — the latter keeps today's "agent panel can sit beside
/// PlanBoard" behaviour without nesting routes.
///
/// The admin section comes from /admin/:section?, populated only when
/// the AdminPage route picks up a sub-section.
///
/// Implementation note: this hook is called from `RouteSync` and
/// `AgentRail`, both of which sit OUTSIDE the `<Routes>` element in
/// `App.tsx` — RouteSync as a sibling to the Routes (so its effect
/// fires regardless of which route is active), AgentRail as the right
/// rail next to whichever main route is rendered. In react-router v6
/// `useParams()` returns `{}` when called outside a matched `<Route>`,
/// so we cannot use it here. Instead we read `useLocation().pathname`
/// (always available) and pattern-match the prefixes ourselves. The
/// regexes match exactly the routes declared in App.tsx.
const PLAN_PATH_RE = /^\/plans\/([^/]+)\/?$/;
const AGENT_PATH_RE = /^\/agents\/([^/]+)\/?$/;
const ADMIN_PATH_RE = /^\/admin\/([^/]+)\/?$/;

function safeDecode(s: string): string {
  try {
    return decodeURIComponent(s);
  } catch {
    // Malformed % escapes shouldn't crash the dashboard. Fall back to
    // the raw segment — useParams takes the same defensive stance.
    return s;
  }
}

export function useRouteSelection(): {
  routePlanName: string | undefined;
  routeAgentId: string | null;
  adminSection: string | undefined;
} {
  const { pathname } = useLocation();
  const [searchParams] = useSearchParams();
  const overlayAgent = searchParams.get("agent");

  const planMatch = PLAN_PATH_RE.exec(pathname);
  const agentMatch = AGENT_PATH_RE.exec(pathname);
  const adminMatch = ADMIN_PATH_RE.exec(pathname);

  const routePlanName = planMatch ? safeDecode(planMatch[1]) : undefined;
  const pathAgentId = agentMatch ? safeDecode(agentMatch[1]) : null;
  const routeAgentId = pathAgentId ?? overlayAgent ?? null;
  const adminSection = adminMatch ? safeDecode(adminMatch[1]) : undefined;

  return { routePlanName, routeAgentId, adminSection };
}

/// Returns a navigation helper that updates the `?agent=<id>` overlay
/// on the current path without changing the main route. Passing null
/// clears the overlay. When the current main route already deep-links
/// the agent (`/agents/:agentId`), the helper navigates to `/agents`
/// before clearing — otherwise the path-param would silently put the
/// id back.
export function useGoToAgent(): (id: string | null) => void {
  const navigate = useNavigate();
  const location = useLocation();
  return useCallback(
    (id: string | null) => {
      const params = new URLSearchParams(location.search);
      const onAgentDeepLink = /^\/agents\/[^/]+$/.test(location.pathname);
      if (id) {
        if (onAgentDeepLink) {
          // Stay on the deep-link form so refresh restores the same shape.
          navigate(`/agents/${id}`);
          return;
        }
        params.set("agent", id);
      } else {
        params.delete("agent");
      }
      const search = params.toString();
      const target = onAgentDeepLink
        ? "/agents"
        : `${location.pathname}${search ? `?${search}` : ""}`;
      navigate(target);
    },
    [navigate, location.pathname, location.search],
  );
}

/// Convenience helper for callers that know they want to land on a
/// PlanBoard. Passing null routes back to the project dashboard.
export function useGoToPlan(): (name: string | null) => void {
  const navigate = useNavigate();
  return useCallback(
    (name: string | null) => {
      navigate(name ? `/plans/${name}` : "/plans");
    },
    [navigate],
  );
}
