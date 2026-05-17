import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { exportPlan, sha256Hex } from "./plan-export.js";
import { HttpError } from "../api.js";

/// Same Map-backed localStorage stub used by org-store.test.ts /
/// api.test.ts / reset-all.test.ts. jsdom's bundled localStorage in
/// vitest 3 is unreliable (see org-store.test.ts comment), so any test
/// that exercises X-Org-Id pass-through must install a real Map.
function makeMockStorage(): Storage {
  const map = new Map<string, string>();
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (k: string) => map.get(k) ?? null,
    key: (n: number) => Array.from(map.keys())[n] ?? null,
    removeItem: (k: string) => {
      map.delete(k);
    },
    setItem: (k: string, v: string) => {
      map.set(k, v);
    },
  } as Storage;
}

interface ExportMockOptions {
  status?: number;
  body?: string;
  errorBody?: unknown;
  contentDisposition?: string | null;
}

function installExportMock(opts: ExportMockOptions = {}) {
  const status = opts.status ?? 200;
  const fn = vi.fn(async (_input: string | URL | Request, _init?: RequestInit) => {
    if (status >= 400) {
      return new Response(JSON.stringify(opts.errorBody ?? { error: "Plan not found" }), {
        status,
        headers: { "Content-Type": "application/json" },
      });
    }
    const headers: HeadersInit = {
      "Content-Type": "application/yaml",
    };
    if (opts.contentDisposition !== null) {
      headers["Content-Disposition"] =
        opts.contentDisposition ?? `attachment; filename="demo.plan.yaml"`;
    }
    return new Response(opts.body ?? "title: demo\n", { status, headers });
  });
  vi.stubGlobal("fetch", fn);
  return fn;
}

describe("sha256Hex", () => {
  it("matches a known sha256 over a UTF-8 string", async () => {
    // Pre-computed: `printf 'abc' | sha256sum`.
    expect(await sha256Hex("abc")).toBe(
      "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    );
  });

  it("handles the empty string", async () => {
    expect(await sha256Hex("")).toBe(
      "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    );
  });

  it("produces 64 hex chars for arbitrary input", async () => {
    const out = await sha256Hex("hello\nworld\n");
    expect(out).toMatch(/^[0-9a-f]{64}$/);
  });
});

describe("exportPlan", () => {
  let originalCreate: typeof URL.createObjectURL;
  let originalRevoke: typeof URL.revokeObjectURL;
  let originalAppend: typeof document.body.appendChild;
  let originalRemove: typeof document.body.removeChild;
  let clickSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    vi.stubGlobal("localStorage", makeMockStorage());
    // `URL.createObjectURL` is not implemented in jsdom by default — stub
    // both the create + revoke halves so the download path runs cleanly.
    originalCreate = URL.createObjectURL;
    originalRevoke = URL.revokeObjectURL;
    URL.createObjectURL = vi.fn(() => "blob://fake");
    URL.revokeObjectURL = vi.fn(() => undefined);

    // Stub the anchor click so the helper's download-trigger doesn't
    // actually try to navigate or download anything in jsdom. We patch
    // the prototype-bound methods directly instead of `vi.spyOn` so we
    // don't fight the generic `<T extends Node>(node: T) => T` signature
    // (vi.spyOn's mock-impl arg is `unknown`, which isn't assignable to
    // `Node`). Saved originals are restored in afterEach.
    clickSpy = vi.fn();
    originalAppend = document.body.appendChild.bind(document.body);
    originalRemove = document.body.removeChild.bind(document.body);
    document.body.appendChild = function <T extends Node>(node: T): T {
      const el = node as unknown as HTMLElement;
      if (el.tagName === "A") {
        (el as HTMLAnchorElement).click = clickSpy as () => void;
      }
      return node;
    };
    document.body.removeChild = function <T extends Node>(node: T): T {
      return node;
    };
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    URL.createObjectURL = originalCreate;
    URL.revokeObjectURL = originalRevoke;
    document.body.appendChild = originalAppend;
    document.body.removeChild = originalRemove;
  });

  it("downloads the YAML body, surfaces the filename + sha256, and triggers a click", async () => {
    const body = "# branchwork-plan-export: v1\n# exported-at: 2026-05-18T00:00:00Z\ntitle: demo\n";
    const fn = installExportMock({ body });

    const result = await exportPlan("demo");

    // Server is called at the canonical path.
    expect(fn).toHaveBeenCalledTimes(1);
    const callArg = fn.mock.calls[0]?.[0];
    expect(String(callArg)).toBe("/api/plans/demo/export");

    // Filename surfaces from Content-Disposition.
    expect(result.filename).toBe("demo.plan.yaml");
    // Body is round-trippable verbatim — caller can re-export and compare.
    expect(result.body).toBe(body);
    // Hash is over those exact bytes (acceptance criterion: matches
    // `sha256sum <name>.plan.yaml`).
    expect(result.sha256Hex).toBe(await sha256Hex(body));

    // Download was triggered.
    expect(clickSpy).toHaveBeenCalledTimes(1);
    expect(URL.createObjectURL).toHaveBeenCalledTimes(1);
    expect(URL.revokeObjectURL).toHaveBeenCalledTimes(1);
  });

  it("rides the active X-Org-Id from localStorage", async () => {
    localStorage.setItem("branchwork.activeOrgId", "org-42");
    const fn = installExportMock();

    await exportPlan("demo");

    const init = fn.mock.calls[0]?.[1] as RequestInit | undefined;
    const headers = init?.headers as Record<string, string> | undefined;
    expect(headers?.["X-Org-Id"]).toBe("org-42");
    expect(init?.credentials).toBe("same-origin");
  });

  it("falls back to <name>.plan.yaml when Content-Disposition is missing", async () => {
    installExportMock({ contentDisposition: null });
    const result = await exportPlan("my-plan");
    expect(result.filename).toBe("my-plan.plan.yaml");
  });

  it("URL-encodes the plan name", async () => {
    const fn = installExportMock();
    await exportPlan("my/funky plan");
    const callArg = fn.mock.calls[0]?.[0];
    expect(String(callArg)).toBe("/api/plans/my%2Ffunky%20plan/export");
  });

  it("throws HttpError on non-2xx (e.g. 404)", async () => {
    installExportMock({
      status: 404,
      errorBody: { error: "Plan not found" },
    });

    let caught: unknown;
    try {
      await exportPlan("ghost");
    } catch (e) {
      caught = e;
    }
    expect(caught).toBeInstanceOf(HttpError);
    expect((caught as HttpError).status).toBe(404);
    // No download attempted on failure.
    expect(clickSpy).not.toHaveBeenCalled();
  });
});
