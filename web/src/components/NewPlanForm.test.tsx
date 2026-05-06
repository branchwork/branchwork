import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { NewPlanForm } from "./NewPlanForm.js";
import { useToastStore } from "../stores/toast-store.js";

type FetchHandler = (path: string) => {
  status: number;
  body: unknown;
};

function installFetchMock(handler: FetchHandler) {
  const fn = vi.fn(async (input: string | URL | Request) => {
    const path =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname + input.search
          : input.url;
    const { status, body } = handler(path);
    return new Response(JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json" },
    });
  });
  vi.stubGlobal("fetch", fn);
  return fn;
}

describe("NewPlanForm runner banner", () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("renders the no-runner banner when /api/folders returns 503", async () => {
    installFetchMock((path) => {
      if (path.startsWith("/api/folders")) {
        return { status: 503, body: { error: "no_runner_connected" } };
      }
      if (path.startsWith("/api/templates")) {
        return { status: 200, body: [] };
      }
      return { status: 404, body: {} };
    });

    render(<NewPlanForm onClose={() => {}} />);

    await waitFor(() => {
      expect(screen.getByText(/No runner connected/i)).toBeTruthy();
    });

    const link = screen.getByRole("link", { name: /Runners page/i });
    expect((link as HTMLAnchorElement).getAttribute("href")).toBe("/runners");
  });

  it("renders the runner-offline banner with last-seen on 504", async () => {
    const lastSeen = new Date(Date.now() - 5 * 60 * 1000)
      .toISOString()
      .replace("T", " ")
      .replace("Z", "")
      .split(".")[0]; // SQLite-style: "YYYY-MM-DD HH:mm:ss"

    installFetchMock((path) => {
      if (path.startsWith("/api/folders")) {
        return { status: 504, body: { error: "runner_unavailable" } };
      }
      if (path.startsWith("/api/runners")) {
        return {
          status: 200,
          body: { runners: [{ lastSeenAt: lastSeen }] },
        };
      }
      if (path.startsWith("/api/templates")) {
        return { status: 200, body: [] };
      }
      return { status: 404, body: {} };
    });

    render(<NewPlanForm onClose={() => {}} />);

    await waitFor(() => {
      expect(screen.getByText(/Runner is offline\. Last seen 5m ago\./)).toBeTruthy();
    });
  });

  it("hides the suggestion dropdown when the runner is offline", async () => {
    installFetchMock((path) => {
      if (path.startsWith("/api/folders")) {
        return { status: 503, body: { error: "no_runner_connected" } };
      }
      if (path.startsWith("/api/templates")) {
        return { status: 200, body: [] };
      }
      return { status: 404, body: {} };
    });

    render(<NewPlanForm onClose={() => {}} />);

    await waitFor(() => {
      expect(screen.getByText(/No runner connected/i)).toBeTruthy();
    });

    // Folder input is still enabled — user can still type an absolute path.
    const input = screen.getByPlaceholderText(/~\/my-project/) as HTMLInputElement;
    expect(input.disabled).toBe(false);
  });

  it("does not render any banner on a clean fetch", async () => {
    installFetchMock((path) => {
      if (path.startsWith("/api/folders")) {
        return {
          status: 200,
          body: [{ name: "proj", path: "/home/cpo/proj" }],
        };
      }
      if (path.startsWith("/api/templates")) {
        return { status: 200, body: [] };
      }
      return { status: 404, body: {} };
    });

    render(<NewPlanForm onClose={() => {}} />);

    // Wait for the folders fetch to settle, then assert no banner.
    await waitFor(() => {
      expect(screen.queryByText(/No runner connected/i)).toBeNull();
      expect(screen.queryByText(/Runner is offline/i)).toBeNull();
    });
  });
});

describe("NewPlanForm /api/plans/create error mapping", () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    useToastStore.getState().clear();
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.useRealTimers();
    useToastStore.getState().clear();
  });

  async function fillFormAndSubmit() {
    const folderInput = screen.getByPlaceholderText(/~\/my-project/) as HTMLInputElement;
    const descriptionInput = screen.getByPlaceholderText(
      /Describe the feature/i
    ) as HTMLTextAreaElement;
    fireEvent.change(folderInput, { target: { value: "~/missing" } });
    fireEvent.change(descriptionInput, { target: { value: "build a thing" } });
    fireEvent.click(screen.getByRole("button", { name: /Create Plan/i }));
  }

  it("shows the no-runner banner when /api/plans/create returns 503", async () => {
    installFetchMock((path) => {
      if (path === "/api/folders") {
        return { status: 200, body: [] };
      }
      if (path === "/api/templates") {
        return { status: 200, body: [] };
      }
      if (path === "/api/plans/create") {
        return { status: 503, body: { error: "no_runner_connected" } };
      }
      return { status: 404, body: {} };
    });

    render(<NewPlanForm onClose={() => {}} />);
    await fillFormAndSubmit();

    await waitFor(() => {
      expect(screen.getByText(/No runner connected/i)).toBeTruthy();
    });
  });

  it("pushes a 'Runner did not respond in time' toast when /api/plans/create returns 504", async () => {
    installFetchMock((path) => {
      if (path === "/api/folders") {
        return { status: 200, body: [] };
      }
      if (path === "/api/templates") {
        return { status: 200, body: [] };
      }
      if (path === "/api/plans/create") {
        return { status: 504, body: { error: "runner_unavailable" } };
      }
      return { status: 404, body: {} };
    });

    render(<NewPlanForm onClose={() => {}} />);
    await fillFormAndSubmit();

    await waitFor(() => {
      const toasts = useToastStore.getState().toasts;
      expect(toasts.some((t) => t.title === "Runner did not respond in time. Try again." && t.kind === "error")).toBe(true);
    });
    // The 504 path does NOT route into the runner-offline banner — that
    // branch only fires from the /api/folders mount fetch.
    expect(screen.queryByText(/Runner is offline/i)).toBeNull();
  });

  it("pushes the runner's message verbatim as a toast on create_failed", async () => {
    installFetchMock((path) => {
      if (path === "/api/folders") {
        return { status: 200, body: [] };
      }
      if (path === "/api/templates") {
        return { status: 200, body: [] };
      }
      if (path === "/api/plans/create") {
        return {
          status: 400,
          body: {
            error: "create_failed",
            resolvedFolder: "/home/runner/missing",
            message: "Permission denied (os error 13)",
          },
        };
      }
      return { status: 404, body: {} };
    });

    render(<NewPlanForm onClose={() => {}} />);
    await fillFormAndSubmit();

    await waitFor(() => {
      const toasts = useToastStore.getState().toasts;
      expect(toasts.some((t) => t.title === "Permission denied (os error 13)" && t.kind === "error")).toBe(true);
    });
  });

  it("shows the create-folder confirm dialog on folder_not_found", async () => {
    installFetchMock((path) => {
      if (path === "/api/folders") {
        return { status: 200, body: [] };
      }
      if (path === "/api/templates") {
        return { status: 200, body: [] };
      }
      if (path === "/api/plans/create") {
        return {
          status: 400,
          body: {
            error: "folder_not_found",
            resolvedFolder: "/home/runner/missing",
            message: "Directory does not exist: /home/runner/missing",
          },
        };
      }
      return { status: 404, body: {} };
    });

    render(<NewPlanForm onClose={() => {}} />);
    await fillFormAndSubmit();

    await waitFor(() => {
      expect(screen.getByText(/Create folder and continue/i)).toBeTruthy();
      expect(screen.getByText("/home/runner/missing")).toBeTruthy();
    });
  });
});
