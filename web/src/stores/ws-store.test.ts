import { describe, expect, it, vi } from "vitest";
import { useAgentStore } from "./agent-store.js";
import { handleWsMessage } from "./ws-store.js";

describe("ws-store handleWsMessage", () => {
  it("refreshes agents on task_advanced", () => {
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({ fetchAgents });

    handleWsMessage({
      type: "task_advanced",
      data: {
        plan: "fix-plan-done-in-progress",
        from_task: "1.1",
        to_tasks: ["1.2", "1.3"],
      },
    });

    expect(fetchAgents).toHaveBeenCalledTimes(1);
  });
});
