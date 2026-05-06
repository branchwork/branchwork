/// Placeholder for the SaaS runner control plane (Phase 4 of the
/// dashboard-ui-overhaul plan). Routing in 1.1 makes /runners reachable
/// so deep links from the audit / future install flow no longer 404 —
/// the real list, enrolment, and diagnostics ship in 4.2.
export function RunnersPage() {
  return (
    <div className="p-6 max-w-2xl mx-auto">
      <h2 className="text-xl font-bold mb-2">Runners</h2>
      <p className="text-sm text-gray-500">
        The runner control plane is in development. Connected runners,
        enrolment, and diagnostics will live here.
      </p>
      <p className="text-xs text-gray-600 mt-4">
        Tracking in plan{" "}
        <span className="font-mono text-gray-400">
          dashboard-ui-overhaul
        </span>{" "}
        Phase 4.
      </p>
    </div>
  );
}
