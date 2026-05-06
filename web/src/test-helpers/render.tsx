import type { ReactElement, ReactNode } from "react";
import { render, type RenderOptions, type RenderResult } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";

interface RouterRenderOptions extends Omit<RenderOptions, "wrapper"> {
  /// Initial entries seed MemoryRouter — pass to deep-link a route under
  /// test (e.g. `["/plans/foo"]`). Defaults to `["/"]`, which is what
  /// most isolated component renders want.
  initialEntries?: string[];
}

/// Wraps `@testing-library/react`'s render in a MemoryRouter. Use this
/// in place of `render(...)` for any test whose component (or its
/// descendants) calls `useNavigate`, `useParams`, `useLocation`,
/// `useSearchParams`, or renders `<Link>` / `<NavLink>`. After the
/// 1.1 routing migration that's most components, so default to this
/// helper unless the test explicitly needs a different router.
export function renderWithRouter(
  ui: ReactElement,
  options: RouterRenderOptions = {},
): RenderResult {
  const { initialEntries = ["/"], ...rest } = options;
  return render(ui, {
    wrapper: ({ children }: { children: ReactNode }) => (
      <MemoryRouter initialEntries={initialEntries}>{children}</MemoryRouter>
    ),
    ...rest,
  });
}
