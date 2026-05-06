import { useCallback } from "react";
import {
  useLocation,
  useNavigate,
  useParams,
  useSearchParams,
} from "react-router-dom";

/// Pulls "what is selected" out of the URL — the source of truth after
/// the routing migration in 1.1. Plan name is a path param on
/// /plans/:planName. Agent id can come either from the deep-link path
/// /agents/:agentId or from a `?agent=<id>` search param layered on any
/// other route — the latter keeps today's "agent panel can sit beside
/// PlanBoard" behaviour without nesting routes.
///
/// The admin section comes from /admin/:section?, populated only when
/// the AdminPage route picks up a sub-section.
export function useRouteSelection(): {
  routePlanName: string | undefined;
  routeAgentId: string | null;
  adminSection: string | undefined;
} {
  const params = useParams<{
    planName?: string;
    agentId?: string;
    section?: string;
  }>();
  const [searchParams] = useSearchParams();
  const overlayAgent = searchParams.get("agent");
  return {
    routePlanName: params.planName,
    routeAgentId: params.agentId ?? overlayAgent ?? null,
    adminSection: params.section,
  };
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
