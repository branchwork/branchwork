import { Link, useLocation } from "react-router-dom";

/// Catch-all 404 for routes that don't match any defined path. The
/// previous behaviour was a `<Navigate to="/" replace>` that silently
/// rewrote the URL; that hides typos and broken bookmarks behind the
/// project dashboard, which is then easy to mistake for "the link
/// worked but the destination is empty". This page surfaces the failed
/// path explicitly and offers a back-to-dashboard affordance.
export function NotFoundPage() {
  const location = useLocation();
  return (
    <div className="flex h-full items-center justify-center p-6">
      <div
        role="alert"
        className="max-w-lg rounded border border-gray-800 bg-gray-900/50 p-6 text-center"
      >
        <p className="text-xs font-mono uppercase tracking-wider text-gray-600">404</p>
        <h2 className="mt-1 text-lg font-semibold text-gray-100">Page not found</h2>
        <p className="mt-1 text-sm text-gray-400">
          No route matches <span className="font-mono text-gray-200">{location.pathname}</span>.
        </p>
        <div className="mt-4">
          <Link
            to="/"
            className="inline-flex items-center rounded border border-gray-700 bg-gray-800 px-3 py-1.5 text-xs text-gray-200 transition hover:border-indigo-600 hover:text-indigo-400"
          >
            Back to dashboard
          </Link>
        </div>
      </div>
    </div>
  );
}
