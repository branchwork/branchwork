import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { RunnerEnrollModal, buildInstallCommand } from "./RunnerEnrollModal.js";
import { useRunnerStore } from "../stores/runner-store.js";

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

describe("buildInstallCommand", () => {
  it("single-quotes the token so a future token format with shell metacharacters wouldn't break", () => {
    expect(buildInstallCommand("abc123")).toBe(
      "branchwork-runner --token 'abc123'",
    );
  });
});

describe("RunnerEnrollModal", () => {
  it("requires a non-empty runner name before issuing", async () => {
    const createRunnerToken = vi.fn();
    useRunnerStore.setState({ createRunnerToken });
    render(<RunnerEnrollModal open={true} onClose={() => {}} />);
    // Type spaces only — Submit stays disabled, so simulate Enter to
    // exercise the trim() guard. We do NOT click the Issue button
    // because it's disabled by `!name.trim()`.
    const input = screen.getByTestId("runner-name-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "   " } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(createRunnerToken).not.toHaveBeenCalled();
  });

  it("issues a token and renders the install command on success", async () => {
    const createRunnerToken = vi
      .fn()
      .mockResolvedValue({ token: "deadbeef", runner_name: "laptop" });
    useRunnerStore.setState({ createRunnerToken });

    render(<RunnerEnrollModal open={true} onClose={() => {}} />);

    fireEvent.change(screen.getByTestId("runner-name-input"), {
      target: { value: "laptop" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Issue token/i }));

    await waitFor(() => {
      expect(createRunnerToken).toHaveBeenCalledWith("laptop");
    });

    const tokenField = (await screen.findByTestId(
      "enroll-token",
    )) as HTMLInputElement;
    expect(tokenField.value).toBe("deadbeef");

    const cmdField = screen.getByTestId("enroll-command") as HTMLTextAreaElement;
    expect(cmdField.value).toBe("branchwork-runner --token 'deadbeef'");
  });

  it("surfaces a server error inline without leaving the form", async () => {
    const createRunnerToken = vi.fn().mockRejectedValue(new Error("boom"));
    useRunnerStore.setState({ createRunnerToken });

    render(<RunnerEnrollModal open={true} onClose={() => {}} />);
    fireEvent.change(screen.getByTestId("runner-name-input"), {
      target: { value: "laptop" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Issue token/i }));

    await waitFor(() => {
      expect(screen.getByText(/boom/)).toBeTruthy();
    });
    // Still on phase 1 — no token / command fields yet.
    expect(screen.queryByTestId("enroll-token")).toBeNull();
  });

  it("copies the token via navigator.clipboard.writeText", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    const createRunnerToken = vi
      .fn()
      .mockResolvedValue({ token: "hex", runner_name: "ci" });
    useRunnerStore.setState({ createRunnerToken });

    render(<RunnerEnrollModal open={true} onClose={() => {}} />);
    fireEvent.change(screen.getByTestId("runner-name-input"), {
      target: { value: "ci" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Issue token/i }));

    const copyButton = await screen.findByRole("button", {
      name: /Copy token/i,
    });
    fireEvent.click(copyButton);
    await waitFor(() => expect(writeText).toHaveBeenCalledWith("hex"));
  });

  it("calls onClose when Done is clicked after a successful issue", async () => {
    const onClose = vi.fn();
    const createRunnerToken = vi
      .fn()
      .mockResolvedValue({ token: "x", runner_name: "n" });
    useRunnerStore.setState({ createRunnerToken });

    render(<RunnerEnrollModal open={true} onClose={onClose} />);
    fireEvent.change(screen.getByTestId("runner-name-input"), {
      target: { value: "n" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Issue token/i }));

    const done = await screen.findByRole("button", { name: /^Done$/ });
    fireEvent.click(done);
    expect(onClose).toHaveBeenCalled();
  });

  it("resets state every time the modal reopens", async () => {
    const createRunnerToken = vi
      .fn()
      .mockResolvedValueOnce({ token: "first", runner_name: "n" });
    useRunnerStore.setState({ createRunnerToken });

    const { rerender } = render(
      <RunnerEnrollModal open={true} onClose={() => {}} />,
    );
    fireEvent.change(screen.getByTestId("runner-name-input"), {
      target: { value: "n" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Issue token/i }));
    await screen.findByTestId("enroll-token");

    // Close and reopen — the previously-issued token must NOT survive
    // the round-trip; the operator must explicitly issue a new one.
    rerender(<RunnerEnrollModal open={false} onClose={() => {}} />);
    rerender(<RunnerEnrollModal open={true} onClose={() => {}} />);
    expect(screen.queryByTestId("enroll-token")).toBeNull();
    const input = screen.getByTestId("runner-name-input") as HTMLInputElement;
    expect(input.value).toBe("");
  });
});
