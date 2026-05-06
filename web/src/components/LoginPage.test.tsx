import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { LoginPage } from "./LoginPage.js";
import { useAuthStore } from "../stores/auth-store.js";

type FetchHandler = (
  path: string,
  init?: RequestInit,
) => {
  status: number;
  body: unknown;
};

function installFetchMock(handler: FetchHandler) {
  const fn = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    const path =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname + input.search
          : input.url;
    const { status, body } = handler(path, init);
    return new Response(JSON.stringify(body), {
      status,
      statusText: status === 200 ? "OK" : "Error",
      headers: { "Content-Type": "application/json" },
    });
  });
  vi.stubGlobal("fetch", fn);
  return fn;
}

function seedAuthStore(overrides: Partial<ReturnType<typeof useAuthStore.getState>> = {}) {
  useAuthStore.setState({
    user: null,
    loading: false,
    error: null,
    ...overrides,
  });
}

beforeEach(() => {
  // Reset URL so tests that rely on a clean window.location don't see
  // bleed from a prior test that wrote ?sso_error=...
  window.history.replaceState({}, "", "/");
  seedAuthStore();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("LoginPage typo fix (audit §13)", () => {
  it("uses 'a Branchwork account' (not 'an Branchwork account') in signup mode", () => {
    installFetchMock(() => ({ status: 200, body: [] }));
    render(<LoginPage />);

    // Toggle into signup mode.
    fireEvent.click(screen.getByText(/Need an account/i));

    // Snapshot the heading text. The fix is the article 'a' vs 'an'.
    const heading = screen.getByRole("heading", { level: 1 });
    expect(heading.textContent).toBe("Create a Branchwork account");
    expect(screen.queryByText(/Create an Branchwork account/i)).toBeNull();
  });
});

describe("LoginPage forgot-password link", () => {
  it("renders the link in login mode and hides it in signup mode", () => {
    installFetchMock(() => ({ status: 200, body: [] }));
    render(<LoginPage />);

    expect(screen.getByText(/Forgot your password\?/i)).toBeTruthy();

    fireEvent.click(screen.getByText(/Need an account/i));
    expect(screen.queryByText(/Forgot your password\?/i)).toBeNull();
  });

  it("prompts for email when clicked with no email entered", () => {
    installFetchMock(() => ({ status: 200, body: [] }));
    render(<LoginPage />);

    fireEvent.click(screen.getByText(/Forgot your password\?/i));
    expect(screen.getByText(/Enter your email above first/i)).toBeTruthy();
  });

  it("POSTs to /api/auth/password-reset/request and shows confirmation", async () => {
    const fetchMock = installFetchMock((path) => {
      if (path === "/api/auth/password-reset/request") {
        return { status: 202, body: {} };
      }
      return { status: 200, body: [] };
    });

    render(<LoginPage />);
    fireEvent.change(screen.getByPlaceholderText("you@example.com"), {
      target: { value: "user@example.com" },
    });
    fireEvent.click(screen.getByText(/Forgot your password\?/i));

    await waitFor(() => {
      expect(screen.getByText(/password-reset link is on its way/i)).toBeTruthy();
    });

    const calls = fetchMock.mock.calls.filter(
      ([path]) => typeof path === "string" && path === "/api/auth/password-reset/request",
    );
    expect(calls).toHaveLength(1);
    const init = calls[0][1] as RequestInit | undefined;
    expect(init?.method).toBe("POST");
    expect(init?.body).toBe(JSON.stringify({ email: "user@example.com" }));
  });

  it("shows the same generic confirmation when the endpoint 404s (stub-friendly)", async () => {
    installFetchMock((path) => {
      if (path === "/api/auth/password-reset/request") {
        return { status: 404, body: { error: "not_found" } };
      }
      return { status: 200, body: [] };
    });

    render(<LoginPage />);
    fireEvent.change(screen.getByPlaceholderText("you@example.com"), {
      target: { value: "user@example.com" },
    });
    fireEvent.click(screen.getByText(/Forgot your password\?/i));

    await waitFor(() => {
      expect(screen.getByText(/password-reset link is on its way/i)).toBeTruthy();
    });
    // No error banner, no toast — generic copy is the same regardless.
    expect(screen.queryByText(/not[_ ]found/i)).toBeNull();
  });
});

describe("LoginPage partial-auth Sign-out link", () => {
  it("hides the Sign out link when there is no sso_error in the URL", () => {
    installFetchMock(() => ({ status: 200, body: [] }));
    render(<LoginPage />);
    expect(screen.queryByRole("button", { name: /^Sign out$/ })).toBeNull();
  });

  it("shows the Sign out link when ?sso_error= is in the URL on mount", () => {
    window.history.replaceState({}, "", "/?sso_error=invalid_token");
    installFetchMock(() => ({ status: 200, body: [] }));
    render(<LoginPage />);

    expect(screen.getByRole("button", { name: /^Sign out$/ })).toBeTruthy();
    // The error message is also shown via the existing Banner path.
    expect(screen.getByText(/Identity verification failed/i)).toBeTruthy();
  });

  it("calls auth-store.logout when Sign out is clicked", async () => {
    window.history.replaceState({}, "", "/?sso_error=invalid_state");
    const logoutSpy = vi.fn().mockResolvedValue(undefined);
    seedAuthStore({ logout: logoutSpy });
    installFetchMock(() => ({ status: 200, body: [] }));

    render(<LoginPage />);

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: /^Sign out$/ }));
    });

    expect(logoutSpy).toHaveBeenCalledTimes(1);
    // After successful logout, the SSO error is cleared so the partial-auth
    // banner stops nagging.
    await waitFor(() => {
      expect(screen.queryByText(/SSO session expired/i)).toBeNull();
      expect(screen.queryByRole("button", { name: /^Sign out$/ })).toBeNull();
    });
  });
});

describe("LoginPage SSO discovery fallback", () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
  });

  it("hides the fallback link before discovery has resolved", async () => {
    installFetchMock(() => ({ status: 200, body: [] }));
    render(<LoginPage />);

    fireEvent.change(screen.getByPlaceholderText("you@example.com"), {
      target: { value: "person@personal.com" },
    });

    // Discovery is debounced 400ms; we have NOT advanced time yet.
    expect(screen.queryByText(/Don.t see SSO/i)).toBeNull();
  });

  it("shows the fallback link when discovery returns no providers", async () => {
    installFetchMock((path) => {
      if (path.startsWith("/api/auth/sso/providers")) {
        return { status: 200, body: [] };
      }
      return { status: 200, body: {} };
    });
    render(<LoginPage />);

    fireEvent.change(screen.getByPlaceholderText("you@example.com"), {
      target: { value: "person@personal.com" },
    });

    // Advance past the 400ms debounce.
    await act(async () => {
      vi.advanceTimersByTime(500);
    });

    await waitFor(() => {
      expect(screen.getByText(/Don.t see SSO\?/i)).toBeTruthy();
    });
  });

  it("hides the fallback link when discovery returns providers", async () => {
    installFetchMock((path) => {
      if (path.startsWith("/api/auth/sso/providers")) {
        return {
          status: 200,
          body: [
            {
              id: "okta",
              name: "Okta",
              protocol: "saml",
              loginUrl: "/api/auth/sso/okta/login",
            },
          ],
        };
      }
      return { status: 200, body: {} };
    });
    render(<LoginPage />);

    fireEvent.change(screen.getByPlaceholderText("you@example.com"), {
      target: { value: "user@bigco.com" },
    });

    await act(async () => {
      vi.advanceTimersByTime(500);
    });

    await waitFor(() => {
      // SSO button shows up.
      expect(screen.getByText(/Sign in with Okta/i)).toBeTruthy();
    });
    // Fallback hint is NOT rendered when providers exist.
    expect(screen.queryByText(/Don.t see SSO\?/i)).toBeNull();
  });

  it("preserves the email value when the fallback link is clicked", async () => {
    installFetchMock((path) => {
      if (path.startsWith("/api/auth/sso/providers")) {
        return { status: 200, body: [] };
      }
      return { status: 200, body: {} };
    });
    render(<LoginPage />);

    const emailInput = screen.getByPlaceholderText(
      "you@example.com",
    ) as HTMLInputElement;
    fireEvent.change(emailInput, {
      target: { value: "person@personal.com" },
    });

    await act(async () => {
      vi.advanceTimersByTime(500);
    });

    const link = await waitFor(() => screen.getByText(/Don.t see SSO\?/i));
    fireEvent.click(link);

    // Email value survives the click — it lives in component state and the
    // fallback handler only focuses the password field.
    expect(emailInput.value).toBe("person@personal.com");
  });
});
