import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { RunnerEnrollModal, buildInstallCommand } from "./RunnerEnrollModal.js";
import { useRunnerStore, type Runner } from "../stores/runner-store.js";

function seedStore() {
  useRunnerStore.setState({
    mode: "saas",
    loaded: true,
    runners: [],
    fetchRunners: vi.fn().mockResolvedValue(undefined),
  });
}

beforeEach(() => {
  seedStore();
  Object.assign(navigator, {
    clipboard: { writeText: vi.fn().mockResolvedValue(undefined) },
  });
});

afterEach(() => {
  cleanup();
  useRunnerStore.getState().reset();
  vi.restoreAllMocks();
});

const sampleIssue = {
  token: "deadbeef",
  command:
    "curl -fsSL https://branchwork.dev/install-runner.sh | sh -s -- 'deadbeef'",
  runner_name: "laptop",
  saas_url: "https://branchwork.dev",
};

describe("buildInstallCommand", () => {
  it("single-quotes the token so a future token format with shell metacharacters wouldn't break", () => {
    expect(buildInstallCommand("abc123")).toBe(
      "branchwork-runner --token 'abc123'",
    );
  });
});

describe("RunnerEnrollModal", () => {
  it("requires a non-empty runner name before issuing", async () => {
    const fetchInstallCommand = vi.fn();
    useRunnerStore.setState({ fetchInstallCommand });
    render(<RunnerEnrollModal open={true} onClose={() => {}} />);
    // Type spaces only — Submit stays disabled, so simulate Enter to
    // exercise the trim() guard. We do NOT click the Issue button
    // because it's disabled by `!name.trim()`.
    const input = screen.getByTestId("runner-name-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "   " } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(fetchInstallCommand).not.toHaveBeenCalled();
  });

  it("issues a command and renders the curl|sh one-liner on success", async () => {
    const fetchInstallCommand = vi.fn().mockResolvedValue(sampleIssue);
    useRunnerStore.setState({ fetchInstallCommand });

    render(<RunnerEnrollModal open={true} onClose={() => {}} />);

    fireEvent.change(screen.getByTestId("runner-name-input"), {
      target: { value: "laptop" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Issue command/i }));

    await waitFor(() => {
      expect(fetchInstallCommand).toHaveBeenCalledWith("laptop");
    });

    const cmdField = (await screen.findByTestId(
      "enroll-command",
    )) as HTMLTextAreaElement;
    expect(cmdField.value).toBe(sampleIssue.command);
    // Raw token still surfaced for manual installs.
    const tokenField = screen.getByTestId("enroll-token") as HTMLInputElement;
    expect(tokenField.value).toBe("deadbeef");
    // Waiting indicator is present until the WS event flips us.
    expect(screen.getByTestId("enroll-waiting")).toBeTruthy();
  });

  it("flips to the connected view when a runner_connected WS event matches the runner name", async () => {
    const fetchInstallCommand = vi.fn().mockResolvedValue(sampleIssue);
    useRunnerStore.setState({ fetchInstallCommand });

    render(<RunnerEnrollModal open={true} onClose={() => {}} />);
    fireEvent.change(screen.getByTestId("runner-name-input"), {
      target: { value: "laptop" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Issue command/i }));
    await screen.findByTestId("enroll-command");

    // Simulate the WS layer's `applyConnected` insert. This is the
    // exact path `useRunnerStore.applyConnected` writes to when a
    // `runner_connected` WS event lands.
    const r: Runner = {
      id: "runner-uuid",
      name: "laptop",
      status: "online",
      hostname: null,
      version: null,
      lastSeenAt: new Date().toISOString(),
      createdAt: new Date().toISOString(),
    };
    act(() => {
      useRunnerStore.setState({ runners: [r] });
    });

    expect(await screen.findByTestId("enroll-connected")).toBeTruthy();
    // Done button is rendered in the connected view.
    screen.getByRole("button", { name: /^Done$/ });
  });

  it("does not flip to connected for a different runner_name", async () => {
    const fetchInstallCommand = vi.fn().mockResolvedValue(sampleIssue);
    useRunnerStore.setState({ fetchInstallCommand });

    render(<RunnerEnrollModal open={true} onClose={() => {}} />);
    fireEvent.change(screen.getByTestId("runner-name-input"), {
      target: { value: "laptop" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Issue command/i }));
    await screen.findByTestId("enroll-command");

    // A different runner connects (someone else's host) — must NOT pivot.
    const stranger: Runner = {
      id: "stranger",
      name: "ci-runner",
      status: "online",
      hostname: null,
      version: null,
      lastSeenAt: null,
      createdAt: null,
    };
    act(() => {
      useRunnerStore.setState({ runners: [stranger] });
    });

    // Give the effect a chance to run, then assert we're still on
    // the install view.
    await new Promise((r) => setTimeout(r, 10));
    expect(screen.queryByTestId("enroll-connected")).toBeNull();
    expect(screen.getByTestId("enroll-command")).toBeTruthy();
  });

  it("surfaces a server error inline without leaving the form", async () => {
    const fetchInstallCommand = vi.fn().mockRejectedValue(new Error("boom"));
    useRunnerStore.setState({ fetchInstallCommand });

    render(<RunnerEnrollModal open={true} onClose={() => {}} />);
    fireEvent.change(screen.getByTestId("runner-name-input"), {
      target: { value: "laptop" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Issue command/i }));

    await waitFor(() => {
      expect(screen.getByText(/boom/)).toBeTruthy();
    });
    // Still on phase 1 — no command field yet.
    expect(screen.queryByTestId("enroll-command")).toBeNull();
  });

  it("copies the command via navigator.clipboard.writeText", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    const fetchInstallCommand = vi.fn().mockResolvedValue(sampleIssue);
    useRunnerStore.setState({ fetchInstallCommand });

    render(<RunnerEnrollModal open={true} onClose={() => {}} />);
    fireEvent.change(screen.getByTestId("runner-name-input"), {
      target: { value: "laptop" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Issue command/i }));

    const copyButton = await screen.findByRole("button", {
      name: /Copy command/i,
    });
    fireEvent.click(copyButton);
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith(sampleIssue.command),
    );
  });

  it("resets state every time the modal reopens", async () => {
    const fetchInstallCommand = vi.fn().mockResolvedValueOnce(sampleIssue);
    useRunnerStore.setState({ fetchInstallCommand });

    const { rerender } = render(
      <RunnerEnrollModal open={true} onClose={() => {}} />,
    );
    fireEvent.change(screen.getByTestId("runner-name-input"), {
      target: { value: "laptop" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Issue command/i }));
    await screen.findByTestId("enroll-command");

    // Close and reopen — the previously-issued command must NOT survive
    // the round-trip; the operator must explicitly issue a new one.
    rerender(<RunnerEnrollModal open={false} onClose={() => {}} />);
    rerender(<RunnerEnrollModal open={true} onClose={() => {}} />);
    expect(screen.queryByTestId("enroll-command")).toBeNull();
    const input = screen.getByTestId("runner-name-input") as HTMLInputElement;
    expect(input.value).toBe("");
  });
});
