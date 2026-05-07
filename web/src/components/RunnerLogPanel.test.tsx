import { afterEach, describe, expect, it } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { RunnerLogPanel } from "./RunnerLogPanel.js";
import { useRunnerStore } from "../stores/runner-store.js";
import { handleWsMessage } from "../stores/ws-store.js";

afterEach(() => {
  cleanup();
  useRunnerStore.getState().reset();
});

function pushLine(runnerId: string, line: string, level = "info") {
  useRunnerStore
    .getState()
    .pushRunnerLog(runnerId, { ts: "2026-05-07T00:00:00.000Z", level, line });
}

describe("RunnerLogPanel", () => {
  it("renders the runner-store ring as a <pre> with one line per entry", () => {
    pushLine("r1", "[runner] connected");
    pushLine("r1", "[runner] start_agent: a1");
    render(<RunnerLogPanel runnerId="r1" />);
    const pre = screen.getByTestId("runner-log-pre-r1");
    expect(pre.textContent).toContain("[runner] connected");
    expect(pre.textContent).toContain("[runner] start_agent: a1");
  });

  it("shows an empty-state hint when the ring has no entries", () => {
    render(<RunnerLogPanel runnerId="r1" />);
    expect(screen.getByTestId("runner-log-pre-r1").textContent).toContain("No log lines yet");
  });

  it("filter input narrows visible lines without dropping them from the ring", () => {
    pushLine("r1", "[runner] connected");
    pushLine("r1", "[runner] start_agent: alpha");
    pushLine("r1", "[runner] kill_agent: alpha");
    render(<RunnerLogPanel runnerId="r1" />);
    fireEvent.change(screen.getByTestId("runner-log-filter-r1"), {
      target: { value: "kill" },
    });
    const pre = screen.getByTestId("runner-log-pre-r1");
    expect(pre.textContent).toContain("kill_agent: alpha");
    expect(pre.textContent).not.toContain("connected");
    // Underlying ring is untouched — clearing the filter brings every
    // line back, proving the filter is render-time only.
    fireEvent.change(screen.getByTestId("runner-log-filter-r1"), {
      target: { value: "" },
    });
    expect(screen.getByTestId("runner-log-pre-r1").textContent).toContain("connected");
  });

  it("filter is case-insensitive substring match", () => {
    pushLine("r1", "[runner] Connected to wss://app.branchwork.dev");
    render(<RunnerLogPanel runnerId="r1" />);
    fireEvent.change(screen.getByTestId("runner-log-filter-r1"), {
      target: { value: "BRANCHWORK" },
    });
    expect(screen.getByTestId("runner-log-pre-r1").textContent).toContain(
      "wss://app.branchwork.dev",
    );
  });

  it("pause freezes the rendered list, resume re-points at the (now-larger) ring", () => {
    // T11.1 acceptance: "Pause + resume works without dropping the ring's
    // contents." The panel snapshots at pause; the store ring keeps
    // accepting WS frames; resume shows everything that arrived during
    // the pause window.
    pushLine("r1", "before-pause-1");
    pushLine("r1", "before-pause-2");
    render(<RunnerLogPanel runnerId="r1" />);
    fireEvent.click(screen.getByTestId("runner-log-pause-r1"));
    // Push two more lines while paused — they land in the ring but should
    // NOT appear on screen yet.
    act(() => {
      pushLine("r1", "during-pause-1");
      pushLine("r1", "during-pause-2");
    });
    let pre = screen.getByTestId("runner-log-pre-r1");
    expect(pre.textContent).toContain("before-pause-1");
    expect(pre.textContent).toContain("before-pause-2");
    expect(pre.textContent).not.toContain("during-pause-1");
    expect(pre.textContent).not.toContain("during-pause-2");
    // Resume — the during-pause lines re-appear (they were never dropped).
    fireEvent.click(screen.getByTestId("runner-log-pause-r1"));
    pre = screen.getByTestId("runner-log-pre-r1");
    expect(pre.textContent).toContain("during-pause-1");
    expect(pre.textContent).toContain("during-pause-2");
  });

  it("renders the [truncated N lines] marker verbatim", () => {
    // The runner ships truncated-marker frames as real RunnerLogLine
    // events with level=warn; the dashboard treats them like any other
    // entry. Acceptance: bursts beyond the cap render the marker.
    pushLine("r1", "[truncated 42 lines]", "warn");
    render(<RunnerLogPanel runnerId="r1" />);
    expect(screen.getByTestId("runner-log-pre-r1").textContent).toContain("[truncated 42 lines]");
  });

  it("ws-store runner_log handler appends to the panel within one render", () => {
    // Acceptance: lines appear within 1s of being printed by the runner.
    // The path is runner WS → server fan-out → ws-store handler →
    // runner-store ring → React render. Drive that path end-to-end with a
    // synthesized WS message to prove it lands in the panel.
    render(<RunnerLogPanel runnerId="r1" />);
    act(() => {
      handleWsMessage({
        type: "runner_log",
        data: {
          runner_id: "r1",
          ts: "2026-05-07T00:00:00.000Z",
          level: "info",
          line: "[runner] hello-from-ws",
        },
      });
    });
    expect(screen.getByTestId("runner-log-pre-r1").textContent).toContain("[runner] hello-from-ws");
  });
});
