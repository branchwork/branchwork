import { useEffect, useRef, useMemo, useState, useCallback } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import { useAgentStore, type AgentOutputLine, type AgentDiff } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";

type Tab = "output" | "diff";

export function AgentPanel() {
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const agents = useAgentStore((s) => s.agents);
  const killAgent = useAgentStore((s) => s.killAgent);
  const finishAgent = useAgentStore((s) => s.finishAgent);
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const plans = usePlanStore((s) => s.plans);
  const driverCapabilities = useSettingsStore((s) => s.driverCapabilities);

  const [activeTab, setActiveTab] = useState<Tab>("output");

  const agent = agents.find((a) => a.id === selectedAgentId);
  const planTitle = agent?.plan_name
    ? plans.find((p) => p.name === agent.plan_name)?.title
    : null;

  const isPty = agent?.mode === "pty";
  const isActive = agent?.status === "running" || agent?.status === "starting";
  const hasBaseCommit = !!agent?.base_commit;

  // Reset to output tab when switching agents
  useEffect(() => {
    setActiveTab("output");
  }, [selectedAgentId]);

  if (!agent) return null;

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="p-2 border-b border-gray-800 flex items-center justify-between flex-shrink-0">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span
              className={`w-2 h-2 rounded-full flex-shrink-0 ${
                isActive ? "bg-emerald-500 animate-pulse" : agent.status === "completed" ? "bg-emerald-500" : "bg-red-500"
              }`}
            />
            <span className="text-sm font-medium truncate">
              {agent.task_id ? `Task ${agent.task_id}` : agent.id.slice(0, 8)}
            </span>
            <span className="text-[10px] text-gray-600">{agent.status}</span>
            <span className="text-[10px] text-gray-700">{agent.mode}</span>
            {driverCapabilities(agent.driver).supports_cost && agent.cost_usd != null && (
              <span className="text-[10px] text-amber-500/80 font-mono">
                ${agent.cost_usd.toFixed(4)}
              </span>
            )}
          </div>
          {agent.plan_name && (
            <div className="text-[10px] text-gray-500 truncate">
              {planTitle ?? agent.plan_name}
              <span className="text-gray-700 font-mono ml-1">{agent.plan_name}</span>
            </div>
          )}
          {agent.branch && (
            <div className="text-[10px] text-indigo-500/70 font-mono truncate">
              {agent.branch}
            </div>
          )}
        </div>
        <div className="flex gap-1 flex-shrink-0">
          {isActive && (
            <>
              <button
                onClick={() => finishAgent(agent.id)}
                className="px-2 py-1 text-xs bg-emerald-900/50 text-emerald-400 hover:bg-emerald-900 rounded transition"
                title="Ask the agent to exit cleanly (sends /exit for Claude Code)"
              >
                Finish
              </button>
              <button
                onClick={() => killAgent(agent.id)}
                className="px-2 py-1 text-xs bg-red-900/50 text-red-400 hover:bg-red-900 rounded transition"
                title="Force-kill the agent process"
              >
                Kill
              </button>
            </>
          )}
          <button
            onClick={() => selectAgent(null)}
            className="px-2 py-1 text-xs text-gray-500 hover:text-gray-300 rounded transition"
          >
            Close
          </button>
        </div>
      </div>

      {/* Tab bar */}
      <div className="flex border-b border-gray-800 flex-shrink-0">
        <button
          onClick={() => setActiveTab("output")}
          className={`px-3 py-1.5 text-xs font-medium transition ${
            activeTab === "output"
              ? "text-gray-200 border-b-2 border-indigo-500"
              : "text-gray-500 hover:text-gray-300"
          }`}
        >
          Output
        </button>
        <button
          onClick={() => setActiveTab("diff")}
          disabled={!hasBaseCommit}
          className={`px-3 py-1.5 text-xs font-medium transition ${
            activeTab === "diff"
              ? "text-gray-200 border-b-2 border-indigo-500"
              : hasBaseCommit
                ? "text-gray-500 hover:text-gray-300"
                : "text-gray-700 cursor-not-allowed"
          }`}
        >
          Diff
        </button>
      </div>

      {/* Content */}
      {activeTab === "output" ? (
        isPty ? (
          <PtyTerminal agentId={agent.id} />
        ) : (
          <StreamJsonView agentId={agent.id} isActive={isActive} />
        )
      ) : (
        <DiffView
          agentId={agent.id}
          canMerge={!isActive && !!agent.branch}
          sourceBranch={agent.source_branch}
        />
      )}
    </div>
  );
}

// --- PTY Terminal (xterm.js) ---

function PtyTerminal({ agentId }: { agentId: string }) {
  const termRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!termRef.current) return;

    const term = new Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: "'JetBrains Mono', 'Fira Code', 'Cascadia Code', Menlo, monospace",
      theme: {
        background: "#0a0a0f",
        foreground: "#e4e4e7",
        cursor: "#818cf8",
        selectionBackground: "#818cf840",
        black: "#18181b",
        red: "#f87171",
        green: "#4ade80",
        yellow: "#facc15",
        blue: "#60a5fa",
        magenta: "#c084fc",
        cyan: "#22d3ee",
        white: "#e4e4e7",
        brightBlack: "#52525b",
        brightRed: "#fca5a5",
        brightGreen: "#86efac",
        brightYellow: "#fde047",
        brightBlue: "#93c5fd",
        brightMagenta: "#d8b4fe",
        brightCyan: "#67e8f9",
        brightWhite: "#fafafa",
      },
      scrollback: 10000,
    });

    const fitAddon = new FitAddon();
    const webLinksAddon = new WebLinksAddon();
    term.loadAddon(fitAddon);
    term.loadAddon(webLinksAddon);
    term.open(termRef.current);

    // Connect WebSocket
    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(
      `${protocol}//${window.location.host}/terminal?agent=${agentId}`
    );

    ws.onopen = () => {
      // Fit after WS opens so we can send correct size
      requestAnimationFrame(() => {
        fitAddon.fit();
        ws.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
      });
    };

    ws.onmessage = (ev) => {
      term.write(ev.data);
    };

    ws.onerror = () => {
      term.write("\r\n\x1b[31m--- connection error ---\x1b[0m\r\n");
    };

    ws.onclose = () => {
      term.write("\r\n\x1b[90m--- session ended ---\x1b[0m\r\n");
    };

    // Forward terminal input to WebSocket
    term.onData((data) => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(data);
      }
    });

    // Handle resize
    const resizeObserver = new ResizeObserver(() => {
      requestAnimationFrame(() => {
        fitAddon.fit();
        if (ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
        }
      });
    });
    resizeObserver.observe(termRef.current);

    return () => {
      resizeObserver.disconnect();
      // Detach handlers first so any orphan messages from a still-connecting
      // socket don't write to a disposed terminal — and defer close() until
      // the socket is actually open. Calling close() on a CONNECTING socket
      // produces the dev-only "WebSocket is closed before the connection is
      // established" error during React StrictMode's mount/unmount/remount
      // cycle. The deferred close keeps the orphan WS quiet without
      // leaking it.
      ws.onopen = null;
      ws.onmessage = null;
      ws.onerror = null;
      ws.onclose = null;
      if (ws.readyState === WebSocket.CONNECTING) {
        ws.addEventListener("open", () => ws.close(), { once: true });
      } else {
        ws.close();
      }
      term.dispose();
    };
  }, [agentId]);

  return (
    <div
      ref={termRef}
      className="flex-1 min-h-0"
      style={{ padding: "4px", background: "#0a0a0f" }}
    />
  );
}

// --- Stream-JSON text view (for check agents) ---

function StreamJsonView({ agentId, isActive }: { agentId: string; isActive: boolean }) {
  const agentOutput = useAgentStore((s) => s.agentOutput);
  const fetchAgentOutput = useAgentStore((s) => s.fetchAgentOutput);
  const outputRef = useRef<HTMLDivElement>(null);

  const output = agentOutput[agentId] ?? [];

  useEffect(() => {
    fetchAgentOutput(agentId);
  }, [agentId]);

  useEffect(() => {
    if (outputRef.current) {
      outputRef.current.scrollTop = outputRef.current.scrollHeight;
    }
  }, [output.length]);

  // Extract verdict
  const verdict = useMemo(() => {
    for (let i = output.length - 1; i >= 0; i--) {
      try {
        const d = JSON.parse(output[i].content);
        if (d.type === "result" && d.result) {
          const m = d.result.match(/\{\s*"status"\s*:\s*"[^"]+"/);
          if (m) {
            const v = JSON.parse(m[0] + (m[0].endsWith("}") ? "" : "}"));
            if (v.status) return v as { status: string; reason: string };
          }
        }
      } catch { /* skip */ }
    }
    return null;
  }, [output]);

  const verdictColors: Record<string, string> = {
    completed: "text-emerald-400",
    in_progress: "text-amber-400",
    pending: "text-gray-400",
  };

  function renderLine(line: AgentOutputLine) {
    try {
      const d = JSON.parse(line.content);
      if (d.type === "assistant" && d.message?.content) {
        const texts = d.message.content
          .filter((b: { type: string }) => b.type === "text")
          .map((b: { text: string }) => b.text)
          .filter(Boolean);
        const toolUses = d.message.content
          .filter((b: { type: string }) => b.type === "tool_use")
          .map((b: { name: string }) => b.name);
        return (
          <div key={line.id}>
            {toolUses.length > 0 && (
              <div className="text-[11px] text-blue-400/70 py-0.5">
                {toolUses.map((t: string) => `[${t}]`).join(" ")}
              </div>
            )}
            {texts.length > 0 && (
              <div className="text-xs text-gray-200 py-1 whitespace-pre-wrap">
                {texts.join("\n")}
              </div>
            )}
          </div>
        );
      }
      if (d.type === "result") {
        const dur = d.duration_ms ? `${(d.duration_ms / 1000).toFixed(1)}s` : "";
        const cost = typeof d.total_cost_usd === "number" ? `$${d.total_cost_usd.toFixed(4)}` : null;
        return (
          <div key={line.id} className="text-[10px] text-gray-600 py-1 border-t border-gray-800 mt-1">
            Finished in {dur} ({d.num_turns ?? 0} turns)
            {cost && <span className="text-amber-500/80 ml-2">{cost}</span>}
          </div>
        );
      }
      // Skip noise
      if (["system", "rate_limit_event", "user"].includes(d.type)) return null;
    } catch { /* skip */ }
    if (line.message_type === "stderr") {
      const text = line.content.trim();
      if (text.startsWith("Warning:")) return null;
      return <div key={line.id} className="text-[10px] text-red-400 py-0.5">{text}</div>;
    }
    return null;
  }

  const rendered = output.map(renderLine).filter(Boolean);

  return (
    <div className="flex-1 overflow-auto flex flex-col min-h-0">
      {/* Verdict banner */}
      {verdict && !isActive && (
        <div className="mx-3 mt-3 p-3 rounded border bg-gray-800/50 border-gray-700 flex-shrink-0">
          <span className={`text-sm font-semibold ${verdictColors[verdict.status] ?? "text-gray-300"}`}>
            {verdict.status === "completed" ? "Done" : verdict.status === "in_progress" ? "In Progress" : "Pending"}
          </span>
          <p className="text-xs text-gray-400 mt-1">{verdict.reason}</p>
        </div>
      )}

      {/* Output */}
      <div ref={outputRef} className="flex-1 overflow-auto p-3 space-y-0.5">
        {rendered.length === 0 && isActive && (
          <p className="text-xs text-gray-600">Agent is working...</p>
        )}
        {rendered.length === 0 && !isActive && (
          <p className="text-xs text-gray-600">No output.</p>
        )}
        {rendered}
      </div>
    </div>
  );
}

// --- Diff View ---

export function DiffView({
  agentId,
  canMerge,
  sourceBranch,
}: {
  agentId: string;
  canMerge: boolean;
  sourceBranch: string | null;
}) {
  const agentDiffs = useAgentStore((s) => s.agentDiffs);
  const fetchAgentDiff = useAgentStore((s) => s.fetchAgentDiff);
  const mergeAgentBranch = useAgentStore((s) => s.mergeAgentBranch);
  const fetchMergeTargets = useAgentStore((s) => s.fetchMergeTargets);
  const discardAgentBranch = useAgentStore((s) => s.discardAgentBranch);
  const [selectedFile, setSelectedFile] = useState<string | null>(null);
  const [mergeState, setMergeState] = useState<"idle" | "confirming" | "merging" | "merged" | "error">("idle");
  const [discardState, setDiscardState] = useState<"idle" | "confirming" | "discarding" | "discarded" | "error">("idle");
  const [actionError, setActionError] = useState<string | null>(null);
  const [mergeTargets, setMergeTargets] = useState<{ default: string | null; available: string[] } | null>(null);
  const [targetsLoading, setTargetsLoading] = useState(true);
  const [targetsError, setTargetsError] = useState(false);
  const [selectedTarget, setSelectedTarget] = useState<string | null>(null);
  const [dropdownOpen, setDropdownOpen] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);

  const diffData = agentDiffs[agentId];

  const refresh = useCallback(() => {
    fetchAgentDiff(agentId);
  }, [agentId, fetchAgentDiff]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  useEffect(() => {
    let cancelled = false;
    setTargetsLoading(true);
    setTargetsError(false);
    fetchMergeTargets(agentId)
      .then((res) => {
        if (cancelled) return;
        setMergeTargets(res);
        setTargetsLoading(false);
      })
      .catch(() => {
        if (cancelled) return;
        setTargetsError(true);
        setTargetsLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [agentId, fetchMergeTargets]);

  useEffect(() => {
    if (!dropdownOpen) return;
    function onMouseDown(e: MouseEvent) {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setDropdownOpen(false);
      }
    }
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, [dropdownOpen]);

  if (!diffData) {
    return (
      <div className="flex-1 flex items-center justify-center">
        <p className="text-xs text-gray-600">Loading diff...</p>
      </div>
    );
  }

  if (diffData.error) {
    return (
      <div className="flex-1 flex items-center justify-center p-4">
        <p className="text-xs text-red-400">{diffData.error}</p>
      </div>
    );
  }

  if (diffData.files.length === 0) {
    return (
      <div className="flex-1 flex flex-col items-center justify-center gap-2 p-4">
        <p className="text-xs text-gray-500">No file changes detected.</p>
        <button
          onClick={refresh}
          className="text-xs text-indigo-400 hover:text-indigo-300 transition"
        >
          Refresh
        </button>
      </div>
    );
  }

  // Parse unified diff into per-file hunks
  const fileDiffs = parseDiff(diffData.diff);
  const displayed = selectedFile
    ? fileDiffs.filter((f) => f.path === selectedFile)
    : fileDiffs;

  const defaultTarget = mergeTargets?.default ?? sourceBranch ?? "main";
  const resolvedTarget = selectedTarget ?? defaultTarget;
  const showChevron =
    !targetsLoading && !targetsError && (mergeTargets?.available.length ?? 0) > 0;

  return (
    <div className="flex-1 flex flex-col min-h-0 overflow-hidden">
      {/* File list bar */}
      <div className="flex items-center gap-1 px-2 py-1.5 border-b border-gray-800 flex-shrink-0 overflow-x-auto">
        <button
          onClick={() => setSelectedFile(null)}
          className={`px-2 py-0.5 text-[11px] rounded whitespace-nowrap transition ${
            selectedFile === null
              ? "bg-indigo-600/30 text-indigo-300"
              : "text-gray-500 hover:text-gray-300"
          }`}
        >
          All ({diffData.files.length})
        </button>
        {diffData.files.map((file) => (
          <button
            key={file}
            onClick={() => setSelectedFile(file)}
            className={`px-2 py-0.5 text-[11px] rounded whitespace-nowrap transition ${
              selectedFile === file
                ? "bg-indigo-600/30 text-indigo-300"
                : "text-gray-500 hover:text-gray-300"
            }`}
          >
            {file.split("/").pop()}
          </button>
        ))}
        <div className="flex-1" />
        <button
          onClick={refresh}
          className="px-2 py-0.5 text-[11px] text-gray-600 hover:text-gray-400 transition"
          title="Refresh diff"
        >
          Refresh
        </button>
      </div>

      {/* Diff content */}
      <div className="flex-1 overflow-auto font-mono text-[12px] leading-[1.6]">
        {displayed.map((file, i) => (
          <div key={i}>
            {/* File header */}
            <div className="sticky top-0 bg-gray-900/95 backdrop-blur-sm px-3 py-1.5 border-b border-gray-800 flex items-center gap-2 z-10">
              <span className="text-indigo-400 font-medium">{file.path}</span>
            </div>
            {/* Hunks */}
            {file.hunks.map((hunk, hi) => (
              <div key={hi}>
                <div className="px-3 py-0.5 text-blue-400/60 bg-blue-950/20 select-none">
                  {hunk.header}
                </div>
                {hunk.lines.map((line, li) => (
                  <DiffLine key={li} line={line} />
                ))}
              </div>
            ))}
          </div>
        ))}
      </div>

      {/* Footer with merge actions */}
      <div className="px-3 py-2 border-t border-gray-800 flex-shrink-0">
        <div className="flex items-center justify-between">
          <span className="text-[10px] text-gray-700">
            base: {diffData.base_commit?.slice(0, 10)}
            {sourceBranch && <> &rarr; {sourceBranch}</>}
          </span>

          {canMerge && mergeState !== "merged" && discardState !== "discarded" && (
            <div className="flex items-center gap-2">
              {/* Discard button */}
              {discardState === "confirming" ? (
                <div className="flex items-center gap-1">
                  <span className="text-[10px] text-red-400">Delete branch?</span>
                  <button
                    onClick={async () => {
                      setDiscardState("discarding");
                      setActionError(null);
                      const result = await discardAgentBranch(agentId);
                      if (result.ok) {
                        setDiscardState("discarded");
                      } else {
                        setDiscardState("error");
                        setActionError(result.error ?? "Discard failed");
                      }
                    }}
                    className="px-2 py-0.5 text-[11px] bg-red-900/60 text-red-300 hover:bg-red-900 rounded transition"
                  >
                    Yes
                  </button>
                  <button
                    onClick={() => setDiscardState("idle")}
                    className="px-2 py-0.5 text-[11px] text-gray-500 hover:text-gray-300 transition"
                  >
                    No
                  </button>
                </div>
              ) : discardState === "discarding" ? (
                <span className="text-[10px] text-gray-500">Discarding...</span>
              ) : (
                <button
                  onClick={() => setDiscardState("confirming")}
                  className="px-2 py-0.5 text-[11px] text-red-500/70 hover:text-red-400 transition"
                >
                  Discard
                </button>
              )}

              {/* Merge button (split: main + chevron dropdown) */}
              {mergeState === "confirming" ? (
                <div className="flex items-center gap-1">
                  <span className="text-[10px] text-amber-400">
                    Merge into {resolvedTarget}?
                  </span>
                  <button
                    onClick={async () => {
                      setMergeState("merging");
                      setActionError(null);
                      const result = await mergeAgentBranch(
                        agentId,
                        selectedTarget ?? undefined
                      );
                      if (result.ok) {
                        setMergeState("merged");
                      } else {
                        setMergeState("error");
                        setActionError(result.error ?? "Merge failed");
                      }
                    }}
                    className="px-2 py-0.5 text-[11px] bg-emerald-900/60 text-emerald-300 hover:bg-emerald-900 rounded transition"
                  >
                    Yes
                  </button>
                  <button
                    onClick={() => setMergeState("idle")}
                    className="px-2 py-0.5 text-[11px] text-gray-500 hover:text-gray-300 transition"
                  >
                    No
                  </button>
                </div>
              ) : mergeState === "merging" ? (
                <span className="text-[10px] text-gray-500">Merging...</span>
              ) : (
                <div className="relative" ref={dropdownRef}>
                  <div className="flex items-center">
                    <button
                      onClick={() => {
                        setDropdownOpen(false);
                        setMergeState("confirming");
                      }}
                      className={`px-3 py-1 text-xs bg-emerald-900/50 text-emerald-400 hover:bg-emerald-900 ${
                        showChevron ? "rounded-l" : "rounded"
                      } transition font-medium`}
                      title={targetsError ? "couldn't load alternates" : undefined}
                    >
                      Merge into {resolvedTarget}
                    </button>
                    {showChevron && (
                      <button
                        onClick={() => setDropdownOpen((o) => !o)}
                        className="px-2 py-1 text-xs bg-emerald-900/50 text-emerald-400 hover:bg-emerald-900 rounded-r border-l border-emerald-700/40 transition"
                        title="Choose target branch"
                        aria-label="Choose target branch"
                      >
                        ∇
                      </button>
                    )}
                  </div>
                  {dropdownOpen && (
                    <div className="absolute bottom-full right-0 mb-1 bg-gray-900 border border-gray-700 rounded shadow-lg min-w-[180px] py-1 z-20">
                      {targetsLoading ? (
                        <div className="px-3 py-1 text-[11px] text-gray-500 flex items-center gap-2">
                          <span className="inline-block w-3 h-3 border-2 border-gray-600 border-t-gray-300 rounded-full animate-spin" />
                          Loading…
                        </div>
                      ) : (
                        <>
                          {mergeTargets?.default && (
                            <button
                              onClick={() => {
                                setSelectedTarget(null);
                                setDropdownOpen(false);
                              }}
                              className={`block w-full text-left px-3 py-1 text-[11px] hover:bg-gray-800 transition ${
                                selectedTarget === null
                                  ? "text-emerald-400"
                                  : "text-gray-300"
                              }`}
                            >
                              {mergeTargets.default}{" "}
                              <span className="text-gray-600">(default)</span>
                            </button>
                          )}
                          {mergeTargets?.available.map((branch) => (
                            <button
                              key={branch}
                              onClick={() => {
                                setSelectedTarget(branch);
                                setDropdownOpen(false);
                              }}
                              className={`block w-full text-left px-3 py-1 text-[11px] hover:bg-gray-800 transition ${
                                selectedTarget === branch
                                  ? "text-emerald-400"
                                  : "text-gray-300"
                              }`}
                            >
                              {branch}
                            </button>
                          ))}
                        </>
                      )}
                    </div>
                  )}
                </div>
              )}
            </div>
          )}

          {mergeState === "merged" && (
            <span className="text-xs text-emerald-400 font-medium">
              Merged into {resolvedTarget}
            </span>
          )}

          {discardState === "discarded" && (
            <span className="text-xs text-gray-500">Branch discarded</span>
          )}
        </div>

        {/* Error display */}
        {actionError && (
          <div className="mt-1 text-[10px] text-red-400">{actionError}</div>
        )}
      </div>
    </div>
  );
}

// --- Diff line rendering ---

interface DiffLineData {
  type: "add" | "del" | "ctx" | "noNewline";
  content: string;
  oldNum?: number;
  newNum?: number;
}

function DiffLine({ line }: { line: DiffLineData }) {
  if (line.type === "noNewline") {
    return (
      <div className="px-3 text-gray-600 italic select-none">
        \ No newline at end of file
      </div>
    );
  }

  const bgClass =
    line.type === "add"
      ? "bg-emerald-950/30"
      : line.type === "del"
        ? "bg-red-950/30"
        : "";

  const textClass =
    line.type === "add"
      ? "text-emerald-400"
      : line.type === "del"
        ? "text-red-400"
        : "text-gray-400";

  const gutterClass =
    line.type === "add"
      ? "text-emerald-700"
      : line.type === "del"
        ? "text-red-700"
        : "text-gray-700";

  const prefix = line.type === "add" ? "+" : line.type === "del" ? "-" : " ";

  return (
    <div className={`flex ${bgClass} hover:brightness-125 transition-all duration-75`}>
      {/* Line numbers */}
      <span className={`w-10 text-right px-1 select-none flex-shrink-0 ${gutterClass}`}>
        {line.oldNum ?? ""}
      </span>
      <span className={`w-10 text-right px-1 select-none flex-shrink-0 border-r border-gray-800/50 ${gutterClass}`}>
        {line.newNum ?? ""}
      </span>
      {/* Prefix and content */}
      <span className={`px-1 select-none flex-shrink-0 ${textClass}`}>{prefix}</span>
      <span className={`flex-1 whitespace-pre ${textClass}`}>
        {line.content}
      </span>
    </div>
  );
}

// --- Diff parser ---

interface FileDiff {
  path: string;
  hunks: HunkData[];
}

interface HunkData {
  header: string;
  lines: DiffLineData[];
}

function parseDiff(raw: string): FileDiff[] {
  const files: FileDiff[] = [];
  const lines = raw.split("\n");
  let current: FileDiff | null = null;
  let currentHunk: HunkData | null = null;
  let oldLine = 0;
  let newLine = 0;

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    // New file diff header
    if (line.startsWith("diff --git")) {
      // Extract path from "diff --git a/path b/path"
      const match = line.match(/diff --git a\/(.+?) b\/(.+)/);
      const path = match ? match[2] : line.slice(13);
      current = { path, hunks: [] };
      files.push(current);
      currentHunk = null;
      continue;
    }

    // Skip index, --- and +++ lines
    if (
      line.startsWith("index ") ||
      line.startsWith("--- ") ||
      line.startsWith("+++ ") ||
      line.startsWith("new file mode") ||
      line.startsWith("deleted file mode") ||
      line.startsWith("old mode") ||
      line.startsWith("new mode") ||
      line.startsWith("similarity index") ||
      line.startsWith("rename from") ||
      line.startsWith("rename to") ||
      line.startsWith("Binary files")
    ) {
      continue;
    }

    // Hunk header
    if (line.startsWith("@@")) {
      const hunkMatch = line.match(/@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)/);
      if (hunkMatch && current) {
        oldLine = parseInt(hunkMatch[1], 10);
        newLine = parseInt(hunkMatch[2], 10);
        currentHunk = { header: line, lines: [] };
        current.hunks.push(currentHunk);
      }
      continue;
    }

    if (!currentHunk) continue;

    // No newline at end of file
    if (line.startsWith("\\ No newline")) {
      currentHunk.lines.push({ type: "noNewline", content: "" });
      continue;
    }

    // Diff lines
    if (line.startsWith("+")) {
      currentHunk.lines.push({
        type: "add",
        content: line.slice(1),
        newNum: newLine++,
      });
    } else if (line.startsWith("-")) {
      currentHunk.lines.push({
        type: "del",
        content: line.slice(1),
        oldNum: oldLine++,
      });
    } else {
      // Context line (starts with space or is empty)
      currentHunk.lines.push({
        type: "ctx",
        content: line.startsWith(" ") ? line.slice(1) : line,
        oldNum: oldLine++,
        newNum: newLine++,
      });
    }
  }

  return files;
}
