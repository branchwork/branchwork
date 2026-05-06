import { afterEach, describe, expect, it } from "vitest";
import {
  MAX_OUTPUT_LINES,
  useAgentStore,
  type AgentOutputLine,
} from "./agent-store.js";

afterEach(() => {
  useAgentStore.getState().reset();
});

function makeLine(seq: number): AgentOutputLine {
  return {
    id: seq,
    agent_id: "a1",
    message_type: "stdout",
    content: `line ${seq}`,
    timestamp: "2026-05-06T00:00:00Z",
  };
}

describe("agent-store appendOutput cap", () => {
  it("caps at MAX_OUTPUT_LINES and evicts the oldest on overflow (audit §10)", () => {
    const { appendOutput } = useAgentStore.getState();
    const total = 10_000;
    for (let i = 1; i <= total; i++) {
      appendOutput("a1", makeLine(i));
    }
    const buf = useAgentStore.getState().agentOutput["a1"];
    expect(buf).toBeDefined();
    expect(buf.length).toBe(MAX_OUTPUT_LINES);
    // Acceptance: after 10k pushes, the first surviving line is the 5001st
    // (id=5001) and the last is the 10000th — the ring buffer kept the
    // most-recent MAX_OUTPUT_LINES entries.
    expect(buf[0].id).toBe(total - MAX_OUTPUT_LINES + 1);
    expect(buf[buf.length - 1].id).toBe(total);
  });

  it("leaves the buffer unchanged below the cap", () => {
    const { appendOutput } = useAgentStore.getState();
    for (let i = 1; i <= 100; i++) {
      appendOutput("a1", makeLine(i));
    }
    const buf = useAgentStore.getState().agentOutput["a1"];
    expect(buf.length).toBe(100);
    expect(buf[0].id).toBe(1);
    expect(buf[99].id).toBe(100);
  });

  it("trims from existing oversize buffers (e.g. seeded by fetchAgentOutput)", () => {
    // Simulate a fetched buffer that's already past the cap, then an
    // appendOutput pushing one more line. The new line lands at the end and
    // the trim brings us back to MAX_OUTPUT_LINES.
    const seeded: AgentOutputLine[] = [];
    for (let i = 1; i <= MAX_OUTPUT_LINES + 200; i++) {
      seeded.push(makeLine(i));
    }
    useAgentStore.setState({ agentOutput: { a1: seeded } });
    useAgentStore.getState().appendOutput("a1", makeLine(99_999));
    const buf = useAgentStore.getState().agentOutput["a1"];
    expect(buf.length).toBe(MAX_OUTPUT_LINES);
    expect(buf[buf.length - 1].id).toBe(99_999);
  });
});
