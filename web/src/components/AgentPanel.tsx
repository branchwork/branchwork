import { useEffect, useRef, useState, useMemo } from "react";
import { useAgentStore } from "../stores/agent-store.js";

const statusBanner: Record<string, { bg: string; label: string }> = {
  completed: { bg: "bg-emerald-900/30 border-emerald-700", label: "Completed" },
  failed: { bg: "bg-red-900/30 border-red-700", label: "Failed" },
  killed: { bg: "bg-red-900/30 border-red-700", label: "Killed" },
};

const verdictColors: Record<string, string> = {
  completed: "text-emerald-400",
  in_progress: "text-amber-400",
  pending: "text-gray-400",
};

interface Verdict {
  status: string;
  reason: string;
}

export function AgentPanel() {
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const agents = useAgentStore((s) => s.agents);
  const agentOutput = useAgentStore((s) => s.agentOutput);
  const fetchAgentOutput = useAgentStore((s) => s.fetchAgentOutput);
  const sendMessage = useAgentStore((s) => s.sendMessage);
  const killAgent = useAgentStore((s) => s.killAgent);
  const selectAgent = useAgentStore((s) => s.selectAgent);

  const [input, setInput] = useState("");
  const outputRef = useRef<HTMLDivElement>(null);

  const agent = agents.find((a) => a.id === selectedAgentId);
  const output = selectedAgentId ? agentOutput[selectedAgentId] ?? [] : [];

  useEffect(() => {
    if (selectedAgentId) {
      fetchAgentOutput(selectedAgentId);
    }
  }, [selectedAgentId]);

  // Auto-scroll
  useEffect(() => {
    if (outputRef.current) {
      outputRef.current.scrollTop = outputRef.current.scrollHeight;
    }
  }, [output.length]);

  // Extract verdict from result line (for check agents)
  const verdict = useMemo<Verdict | null>(() => {
    for (let i = output.length - 1; i >= 0; i--) {
      const line = output[i];
      try {
        const d = JSON.parse(line.content);
        if (d.type === "result" && d.result) {
          const jsonMatch = d.result.match(/\{[^{}]*"status"\s*:\s*"[^"]+"/);
          if (jsonMatch) {
            const parsed = JSON.parse(jsonMatch[0] + (jsonMatch[0].endsWith("}") ? "" : "}"));
            if (parsed.status && parsed.reason !== undefined) {
              return parsed as Verdict;
            }
          }
        }
      } catch {
        // skip
      }
    }
    return null;
  }, [output]);

  if (!agent) return null;

  const isActive = agent.status === "running" || agent.status === "starting";
  const banner = statusBanner[agent.status];

  async function handleSend() {
    if (!input.trim() || !selectedAgentId) return;
    await sendMessage(selectedAgentId, input.trim());
    setInput("");
  }

  function handleKeyDown(e: React.KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  }

  // Parse a single output line into displayable content
  function renderOutputLine(line: (typeof output)[0]) {
    try {
      const d = JSON.parse(line.content);

      // Assistant text
      if (d.type === "assistant" && d.message?.content) {
        const texts: string[] = [];
        const toolUses: string[] = [];
        for (const block of d.message.content) {
          if (block.type === "text" && block.text) texts.push(block.text);
          if (block.type === "tool_use") toolUses.push(block.name ?? "tool");
        }
        const parts: React.ReactNode[] = [];
        if (toolUses.length > 0) {
          parts.push(
            <div key="tools" className="text-[11px] text-blue-400/70 py-0.5">
              {toolUses.map((t) => `[${t}]`).join(" ")}
            </div>
          );
        }
        if (texts.length > 0) {
          const combined = texts.join("\n").trim();
          if (combined) {
            parts.push(
              <div key="text" className="text-xs text-gray-200 py-1 whitespace-pre-wrap">
                {combined}
              </div>
            );
          }
        }
        if (parts.length > 0) return <div key={line.id}>{parts}</div>;
        return null;
      }

      // Result
      if (d.type === "result") {
        const duration = d.duration_ms ? `${(d.duration_ms / 1000).toFixed(1)}s` : "";
        const turns = d.num_turns ?? 0;
        return (
          <div key={line.id} className="text-[10px] text-gray-600 py-1 border-t border-gray-800 mt-1">
            Finished in {duration} ({turns} turn{turns !== 1 ? "s" : ""})
          </div>
        );
      }

      // System events — show only relevant ones
      if (d.type === "system") {
        if (d.subtype === "init") return null; // skip init noise
        if (d.subtype?.startsWith("hook_")) return null; // skip hook events
        if (d.subtype === "task_progress") return null;
        return (
          <div key={line.id} className="text-[10px] text-yellow-400/50 py-0.5">
            [{d.subtype}]
          </div>
        );
      }

      // Rate limit — skip
      if (d.type === "rate_limit_event") return null;

      // User messages (tool results) — skip the verbose tool result content
      if (d.type === "user") return null;

    } catch {
      // Not JSON
    }

    // Stderr
    if (line.message_type === "stderr") {
      const text = line.content.trim();
      if (text.startsWith("Warning:")) return null; // skip stdin warnings
      return (
        <div key={line.id} className="text-[10px] text-red-400 py-0.5">
          {text}
        </div>
      );
    }

    return null;
  }

  const renderedLines = output.map(renderOutputLine).filter(Boolean);

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="p-3 border-b border-gray-800 flex items-center justify-between">
        <div>
          <div className="flex items-center gap-2">
            <span
              className={`w-2 h-2 rounded-full ${
                isActive ? "bg-emerald-500 animate-pulse" : agent.status === "completed" ? "bg-emerald-500" : "bg-red-500"
              }`}
            />
            <span className="text-sm font-medium">
              {agent.task_id ? `Task ${agent.task_id}` : agent.id.slice(0, 8)}
            </span>
            <span className="text-[10px] text-gray-600">{agent.status}</span>
          </div>
          {agent.plan_name && (
            <div className="text-[10px] text-gray-500 mt-0.5 truncate max-w-[300px]">
              {agent.plan_name}
            </div>
          )}
        </div>
        <div className="flex gap-1">
          {isActive && (
            <button
              onClick={() => killAgent(agent.id)}
              className="px-2 py-1 text-xs bg-red-900/50 text-red-400 hover:bg-red-900 rounded transition"
            >
              Kill
            </button>
          )}
          <button
            onClick={() => selectAgent(null)}
            className="px-2 py-1 text-xs text-gray-500 hover:text-gray-300 rounded transition"
          >
            Close
          </button>
        </div>
      </div>

      {/* Verdict banner (for check agents) */}
      {verdict && !isActive && (
        <div className={`mx-3 mt-3 p-3 rounded border ${statusBanner[agent.status]?.bg ?? "bg-gray-800 border-gray-700"}`}>
          <div className="flex items-center gap-2 mb-1">
            <span className={`text-sm font-semibold ${verdictColors[verdict.status] ?? "text-gray-300"}`}>
              {verdict.status === "completed" ? "Done" : verdict.status === "in_progress" ? "In Progress" : "Pending"}
            </span>
          </div>
          <p className="text-xs text-gray-400 leading-relaxed">
            {verdict.reason}
          </p>
        </div>
      )}

      {/* Finished banner (non-check agents) */}
      {!verdict && !isActive && banner && (
        <div className={`mx-3 mt-3 p-2 rounded border text-xs text-center ${banner.bg}`}>
          Agent {banner.label.toLowerCase()}
        </div>
      )}

      {/* Output stream */}
      <div ref={outputRef} className="flex-1 overflow-auto p-3 space-y-0.5">
        {renderedLines.length === 0 && isActive ? (
          <p className="text-xs text-gray-600">Agent is starting...</p>
        ) : renderedLines.length === 0 ? (
          <p className="text-xs text-gray-600">No output captured.</p>
        ) : (
          renderedLines
        )}
      </div>

      {/* Input */}
      {isActive && (
        <div className="p-3 border-t border-gray-800">
          <div className="flex gap-2">
            <input
              type="text"
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={handleKeyDown}
              placeholder="Send message to agent..."
              className="flex-1 bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 placeholder:text-gray-600 focus:outline-none focus:border-indigo-600"
            />
            <button
              onClick={handleSend}
              disabled={!input.trim()}
              className="px-3 py-1.5 text-sm bg-indigo-600 hover:bg-indigo-500 disabled:bg-gray-700 disabled:text-gray-500 text-white rounded transition"
            >
              Send
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
