import { afterEach, describe, expect, it, vi } from "vitest";
import { useAgentStore, type Agent } from "./agent-store.js";
import { usePlanStore, type ParsedPlan, type PlanSummary } from "./plan-store.js";
import { handleWsMessage, subscribeToWsEvents, useWsStore } from "./ws-store.js";

afterEach(() => {
  // Reset zustand stores so seeded state and spies don't leak between
  // tests. Several earlier tests replace store ACTIONS via
  // `usePlanStore.setState({ fetchPlans: vi.fn() })`; without restoring
  // them here, later tests that depend on the real coalesce/in-flight
  // logic would see the spy and silently skip the network round trip.
  // `getInitialState()` returns the actions defined in `create<...>(...)`
  // and is the canonical "reset to fresh" entry point in zustand.
  const planInitial = usePlanStore.getInitialState();
  const agentInitial = useAgentStore.getInitialState();
  usePlanStore.setState({
    autoModeRuntimes: {},
    autoPushRebases: {},
    autoPushRebaseConflicts: {},
    preMergeCheckFailures: {},
    toasts: [],
    plans: [],
    selectedPlan: null,
    planConfigs: {},
    fetchPlans: planInitial.fetchPlans,
    selectPlan: planInitial.selectPlan,
    patchTaskStatus: planInitial.patchTaskStatus,
  });
  useAgentStore.setState({
    agents: [],
    fetchAgents: agentInitial.fetchAgents,
  });
});

function makeAgent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "agent-1",
    session_id: "session-1",
    pid: null,
    parent_agent_id: null,
    plan_name: null,
    task_id: null,
    cwd: "/tmp",
    status: "running",
    mode: "pty",
    prompt: null,
    started_at: "2026-04-12T00:00:00Z",
    finished_at: null,
    last_tool: null,
    last_activity_at: null,
    base_commit: null,
    branch: null,
    source_branch: null,
    cost_usd: null,
    driver: null,
    merge_status: null,
    spawn_error: null,
    ...overrides,
  };
}

function makePlan(overrides: Partial<ParsedPlan> = {}): ParsedPlan {
  return {
    name: "p",
    filePath: "p.yaml",
    title: "P",
    context: "",
    project: null,
    createdAt: "2026-04-12T00:00:00Z",
    modifiedAt: "2026-04-12T00:00:00Z",
    phases: [],
    ...overrides,
  };
}

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

  it("agent_branch_merged optimistically clears the branch so the Merge button hides without a reload", () => {
    // Regression: relying on fetchAgents() alone was racy (it coalesces onto
    // an in-flight /api/agents call started before the server cleared the
    // branch, returning stale data with the branch still set), so the Merge
    // button stayed up until a full page reload. The handler must patch the
    // store synchronously — by branch name, mirroring the server's
    // `UPDATE agents SET branch = NULL WHERE branch = ?` (siblings too).
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({
      fetchAgents,
      agents: [
        { id: "agent-a", branch: "branchwork/p/1.1" },
        { id: "agent-b", branch: "branchwork/p/1.1" }, // sibling on same branch
        { id: "agent-c", branch: "branchwork/p/2.1" }, // unrelated
      ] as unknown as ReturnType<typeof useAgentStore.getState>["agents"],
    });

    handleWsMessage({
      type: "agent_branch_merged",
      data: { id: "agent-a", merged: "branchwork/p/1.1", into: "master" },
    });

    const agents = useAgentStore.getState().agents;
    expect(agents.find((a) => a.id === "agent-a")?.branch).toBeNull();
    expect(agents.find((a) => a.id === "agent-b")?.branch).toBeNull();
    expect(agents.find((a) => a.id === "agent-c")?.branch).toBe("branchwork/p/2.1");
    // Still reconciles against the server.
    expect(fetchAgents).toHaveBeenCalledTimes(1);
  });

  it("agent_branch_discarded clears the branch by name too", () => {
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({
      fetchAgents,
      agents: [{ id: "agent-d", branch: "branchwork/p/3.1" }] as unknown as ReturnType<
        typeof useAgentStore.getState
      >["agents"],
    });

    handleWsMessage({
      type: "agent_branch_discarded",
      data: { id: "agent-d", deleted: "branchwork/p/3.1" },
    });

    expect(useAgentStore.getState().agents.find((a) => a.id === "agent-d")?.branch).toBeNull();
    expect(fetchAgents).toHaveBeenCalledTimes(1);
  });

  it("agent_spawn_failed refetches agents so the inline banner appears", () => {
    // Task 1.1, runner-install-and-spawn-reliability: when the runner
    // can't `Command::spawn` the session daemon, the server pre-renders
    // a "runner could not spawn: <path> (<errno_tag>)" message to
    // `agents.spawn_error` and broadcasts this event. The frontend's
    // job is to refetch so the task card surfaces the new column without
    // requiring a page reload.
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({ fetchAgents });

    handleWsMessage({
      type: "agent_spawn_failed",
      data: {
        id: "agent-x",
        command: "/usr/local/bin/branchwork-server",
        errno: 2,
        errno_str: "ENOENT",
        message: "runner could not spawn: /usr/local/bin/branchwork-server (ENOENT)",
      },
    });

    expect(fetchAgents).toHaveBeenCalledTimes(1);
  });

  it("auto_mode_paused stashes the rebase-conflict file list when reason matches", () => {
    // Seed the persistent half so the patchPlanConfig call inside
    // handleWsMessage can attach `pausedReason` (the action is a no-op
    // for plans with no existing config row).
    usePlanStore.setState({
      planConfigs: {
        p: {
          autoAdvance: false,
          autoMode: true,
          maxFixAttempts: 3,
          pausedReason: null,
          parallel: false,
          worktreeIsolation: false,
          runnerId: null,
          runnerFailover: "pause",
        },
      },
    });

    handleWsMessage({
      type: "auto_mode_paused",
      data: {
        plan: "p",
        task: "1.3",
        reason: "auto_push_rebase_conflict",
        target: null,
        branch: "master",
        files: ["Cargo.toml", "src/lib.rs"],
        file_count: 2,
      },
    });
    const conflict = usePlanStore.getState().autoPushRebaseConflicts["p"];
    expect(conflict).toEqual({
      branch: "master",
      files: ["Cargo.toml", "src/lib.rs"],
      fileCount: 2,
    });
    // PausedReason is also patched on the persistent half so the banner
    // survives a page reload (without the file list).
    expect(usePlanStore.getState().planConfigs["p"]?.pausedReason).toBe(
      "auto_push_rebase_conflict",
    );
  });

  it("auto_mode_paused does NOT stash conflict files for unrelated reasons", () => {
    handleWsMessage({
      type: "auto_mode_paused",
      data: {
        plan: "p",
        task: "1.3",
        reason: "merge_conflict",
        target: null,
      },
    });
    expect(usePlanStore.getState().autoPushRebaseConflicts["p"]).toBeUndefined();
  });

  it("auto_mode_resumed clears the rebase-conflict stash", () => {
    usePlanStore.setState({
      autoPushRebaseConflicts: {
        p: { branch: "master", files: ["Cargo.toml"], fileCount: 1 },
      },
    });
    handleWsMessage({
      type: "auto_mode_resumed",
      data: { plan: "p", last_completed_task: null },
    });
    expect(usePlanStore.getState().autoPushRebaseConflicts["p"]).toBeUndefined();
  });

  it("auto_mode_pre_merge_check_failed stashes the structured detail", () => {
    handleWsMessage({
      type: "auto_mode_pre_merge_check_failed",
      data: {
        plan: "p",
        task: "0.1",
        agent_id: "agent-xyz",
        check_name: "cargo-clippy",
        exit_code: 101,
        output_snippet: "error[E0412]: cannot find type `Foo`",
      },
    });
    expect(usePlanStore.getState().preMergeCheckFailures["p"]).toEqual({
      checkName: "cargo-clippy",
      exitCode: 101,
      outputSnippet: "error[E0412]: cannot find type `Foo`",
      agentId: "agent-xyz",
    });
  });

  it("auto_mode_pre_merge_check_failed treats missing exit_code as null (timeout kill)", () => {
    handleWsMessage({
      type: "auto_mode_pre_merge_check_failed",
      data: {
        plan: "p",
        task: "0.1",
        agent_id: "agent-xyz",
        check_name: "long-runner",
        // exit_code omitted: server emits `null` when the gate killed
        // the check on its per-check timeout.
        output_snippet: "[killed by gate: exceeded per-check timeout of 5s]",
      },
    });
    const failure = usePlanStore.getState().preMergeCheckFailures["p"];
    expect(failure?.exitCode).toBeNull();
    expect(failure?.outputSnippet).toMatch(/killed by gate/);
  });

  it("auto_mode_resumed clears the pre-merge-check-failed stash", () => {
    usePlanStore.setState({
      preMergeCheckFailures: {
        p: {
          checkName: "cargo-clippy",
          exitCode: 1,
          outputSnippet: "fail",
          agentId: "agent-xyz",
        },
      },
    });
    handleWsMessage({
      type: "auto_mode_resumed",
      data: { plan: "p", last_completed_task: null },
    });
    expect(usePlanStore.getState().preMergeCheckFailures["p"]).toBeUndefined();
  });

  it("sets auto_finishing pill state on auto_finish_triggered", () => {
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({ fetchAgents });

    handleWsMessage({
      type: "auto_finish_triggered",
      data: {
        agent_id: "abc-123",
        plan: "unattended-auto-mode",
        task: "6.1",
        trigger: "stop_hook",
      },
    });

    const runtime = usePlanStore.getState().autoModeRuntimes["unattended-auto-mode"];
    expect(runtime).toEqual({
      state: "auto_finishing",
      task: "6.1",
    });
    // Stop-hook path also defensively refreshes agents so the row's
    // stop_reason flips visibly before `agent_stopped` lands.
    expect(fetchAgents).toHaveBeenCalledTimes(1);
  });

  it("soft plan_deleted drops the plan, clears selectedPlan, " + "and pushes an Undo toast", () => {
    const summary: PlanSummary = {
      name: "doomed",
      title: "Doomed",
      project: null,
      phaseCount: 1,
      taskCount: 1,
      doneCount: 0,
      createdAt: "2026-04-12T00:00:00Z",
      modifiedAt: "2026-04-12T00:00:00Z",
    };
    const selected: ParsedPlan = {
      name: "doomed",
      filePath: "doomed.yaml",
      title: "Doomed",
      context: "",
      project: null,
      createdAt: "2026-04-12T00:00:00Z",
      modifiedAt: "2026-04-12T00:00:00Z",
      phases: [],
    };
    usePlanStore.setState({ plans: [summary], selectedPlan: selected });

    handleWsMessage({
      type: "plan_deleted",
      data: { plan: "doomed", snapshot_id: "snap-123", hard: false },
    });

    const state = usePlanStore.getState();
    expect(state.plans.find((p) => p.name === "doomed")).toBeUndefined();
    // App.tsx routes back to ProjectDashboard when selectedPlan is null.
    expect(state.selectedPlan).toBeNull();
    expect(state.toasts).toHaveLength(1);
    expect(state.toasts[0]).toMatchObject({
      kind: "info",
      message: "Deleted plan doomed",
      action: { label: "Undo", snapshotId: "snap-123" },
    });
  });

  it("hard plan_deleted (no snapshot_id) pushes a toast without an Undo action", () => {
    const summary: PlanSummary = {
      name: "obsolete",
      title: "Obsolete",
      project: null,
      phaseCount: 0,
      taskCount: 0,
      doneCount: 0,
      createdAt: "2026-04-12T00:00:00Z",
      modifiedAt: "2026-04-12T00:00:00Z",
    };
    usePlanStore.setState({ plans: [summary], selectedPlan: null });

    handleWsMessage({
      type: "plan_deleted",
      data: { plan: "obsolete", snapshot_id: null, hard: true },
    });

    const state = usePlanStore.getState();
    expect(state.plans.find((p) => p.name === "obsolete")).toBeUndefined();
    expect(state.toasts).toHaveLength(1);
    expect(state.toasts[0].action).toBeUndefined();
    expect(state.toasts[0]).toMatchObject({
      kind: "info",
      message: "Deleted plan obsolete",
    });
  });

  it("plan_deleted leaves selectedPlan alone when the user is viewing a different plan", () => {
    const other: ParsedPlan = {
      name: "still-here",
      filePath: "still-here.yaml",
      title: "Still here",
      context: "",
      project: null,
      createdAt: "2026-04-12T00:00:00Z",
      modifiedAt: "2026-04-12T00:00:00Z",
      phases: [],
    };
    usePlanStore.setState({ plans: [], selectedPlan: other });

    handleWsMessage({
      type: "plan_deleted",
      data: { plan: "doomed", snapshot_id: "snap-1", hard: false },
    });

    expect(usePlanStore.getState().selectedPlan).toEqual(other);
  });

  it(
    "drops a malformed task_status_changed: warns, does not throw, " + "does not partially apply",
    () => {
      const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
      const patchTaskStatus = vi.fn();
      // Replace the action with a spy so we can assert the malformed
      // payload never reached the store.
      usePlanStore.setState({ patchTaskStatus });

      // `plan_name` must be a string — sending a number is the canonical
      // shape mismatch the schema needs to reject.
      expect(() =>
        handleWsMessage({
          type: "task_status_changed",
          data: { plan_name: 123, task_number: "1.1", status: "completed" },
        }),
      ).not.toThrow();

      expect(patchTaskStatus).not.toHaveBeenCalled();
      expect(warn).toHaveBeenCalledTimes(1);
      const args = warn.mock.calls[0];
      expect(String(args[0])).toMatch(/dropped malformed/);

      warn.mockRestore();
    },
  );

  it("drops a JSON string that fails the discriminator: warns and ignores", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({ fetchAgents });

    // Unknown event types fall outside the discriminated union and the
    // listener silently drops them. The on-the-wire shape is a string
    // (this is what `ws.onmessage` hands to `handleWsMessage`), so we
    // exercise the JSON-string branch here too.
    expect(() =>
      handleWsMessage(JSON.stringify({ type: "completely_unknown_event", data: {} })),
    ).not.toThrow();

    expect(fetchAgents).not.toHaveBeenCalled();
    expect(warn).toHaveBeenCalled();

    warn.mockRestore();
  });

  it("drops non-JSON garbage from ws.onmessage without throwing", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});

    expect(() => handleWsMessage("<<not even json>>")).not.toThrow();
    expect(warn).toHaveBeenCalled();
    expect(String(warn.mock.calls[0][0])).toMatch(/malformed/);

    warn.mockRestore();
  });

  it("phase_advanced refetches the selected plan and pushes a toast", () => {
    const selected = makePlan({ name: "alpha", title: "Alpha" });
    const selectPlan = vi.fn().mockResolvedValue(undefined);
    usePlanStore.setState({ selectedPlan: selected, selectPlan });

    handleWsMessage({
      type: "phase_advanced",
      data: { plan_name: "alpha", from_phase: 1, to_phase: 2 },
    });

    expect(selectPlan).toHaveBeenCalledWith("alpha");
    const toasts = usePlanStore.getState().toasts;
    expect(toasts).toHaveLength(1);
    expect(toasts[0].message).toMatch(/alpha.*Phase 2/);
  });

  it(
    "phase_advanced for a non-selected plan still pushes a toast " + "but does not refetch",
    () => {
      const other = makePlan({ name: "beta" });
      const selectPlan = vi.fn().mockResolvedValue(undefined);
      usePlanStore.setState({ selectedPlan: other, selectPlan });

      handleWsMessage({
        type: "phase_advanced",
        data: { plan_name: "alpha", from_phase: 1, to_phase: 2 },
      });

      expect(selectPlan).not.toHaveBeenCalled();
      expect(usePlanStore.getState().toasts).toHaveLength(1);
    },
  );

  it("task_cost_reported updates the per-task cost and the plan aggregate", () => {
    const selected = makePlan({
      name: "alpha",
      totalCostUsd: 0.5,
      phases: [
        {
          number: 1,
          title: "P1",
          description: "",
          tasks: [
            {
              number: "1.1",
              title: "T1",
              description: "",
              filePaths: [],
              acceptance: "",
              costUsd: 0.2,
            },
          ],
        },
      ],
    });
    const summary: PlanSummary = {
      name: "alpha",
      title: "Alpha",
      project: null,
      phaseCount: 1,
      taskCount: 1,
      doneCount: 0,
      createdAt: "2026-04-12T00:00:00Z",
      modifiedAt: "2026-04-12T00:00:00Z",
      totalCostUsd: 0.5,
    };
    usePlanStore.setState({ selectedPlan: selected, plans: [summary] });

    handleWsMessage({
      type: "task_cost_reported",
      data: { plan_name: "alpha", task_number: "1.1", amount_usd: 0.7 },
    });

    const next = usePlanStore.getState();
    expect(next.selectedPlan!.phases[0].tasks[0].costUsd).toBeCloseTo(0.7);
    // Aggregate is bumped by the signed delta (+0.5 = 0.7 - 0.2)
    expect(next.selectedPlan!.totalCostUsd).toBeCloseTo(1.0);
    expect(next.plans[0].totalCostUsd).toBeCloseTo(1.0);
  });

  it("task_cost_reported on a non-selected plan still bumps the summary", () => {
    const summary: PlanSummary = {
      name: "alpha",
      title: "Alpha",
      project: null,
      phaseCount: 1,
      taskCount: 1,
      doneCount: 0,
      createdAt: "2026-04-12T00:00:00Z",
      modifiedAt: "2026-04-12T00:00:00Z",
      totalCostUsd: 0.5,
    };
    usePlanStore.setState({ selectedPlan: null, plans: [summary] });

    handleWsMessage({
      type: "task_cost_reported",
      data: { plan_name: "alpha", task_number: "1.1", amount_usd: 0.3 },
    });

    // Non-selected: store has no per-task data, so the unsigned +amount
    // fallback applies. Next plan refetch reconciles drift.
    expect(usePlanStore.getState().plans[0].totalCostUsd).toBeCloseTo(0.8);
  });

  it("plan_reset refetches the selected plan, refreshes the list, and toasts", async () => {
    const selected = makePlan({ name: "alpha" });
    const selectPlan = vi.fn().mockResolvedValue(undefined);
    const fetchPlans = vi.fn().mockResolvedValue(undefined);
    usePlanStore.setState({ selectedPlan: selected, selectPlan, fetchPlans });

    handleWsMessage({
      type: "plan_reset",
      data: { plan_name: "alpha", cleared: 4 },
    });

    expect(selectPlan).toHaveBeenCalledWith("alpha");
    expect(fetchPlans).toHaveBeenCalledTimes(1);
    const toasts = usePlanStore.getState().toasts;
    expect(toasts).toHaveLength(1);
    expect(toasts[0].message).toMatch(/alpha.*4 tasks/);
  });

  it("plan_reset with cleared=1 toasts in the singular", () => {
    const fetchPlans = vi.fn().mockResolvedValue(undefined);
    usePlanStore.setState({ selectedPlan: null, fetchPlans });

    handleWsMessage({
      type: "plan_reset",
      data: { plan_name: "alpha", cleared: 1 },
    });

    expect(usePlanStore.getState().toasts[0].message).toMatch(/1 task\)/);
  });

  it("ci_run_dismissed clears the CI badge on the matching task", () => {
    const selected = makePlan({
      name: "alpha",
      phases: [
        {
          number: 1,
          title: "P1",
          description: "",
          tasks: [
            {
              number: "1.1",
              title: "T1",
              description: "",
              filePaths: [],
              acceptance: "",
              ci: {
                id: 7,
                status: "failure",
                conclusion: "failure",
                runUrl: null,
                commitSha: null,
                updatedAt: "2026-04-12T00:00:00Z",
              },
            },
          ],
        },
      ],
    });
    usePlanStore.setState({ selectedPlan: selected });

    handleWsMessage({
      type: "ci_run_dismissed",
      data: { id: 7, plan_name: "alpha", task_number: "1.1" },
    });

    const ci = usePlanStore.getState().selectedPlan!.phases[0].tasks[0].ci;
    expect(ci).toBeNull();
  });

  it("agent_branch_cleared with agent_id patches branch on the matching agent", () => {
    useAgentStore.setState({
      agents: [
        makeAgent({ id: "a-1", branch: "feature/foo" }),
        makeAgent({ id: "a-2", branch: "feature/bar" }),
      ],
    });

    handleWsMessage({
      type: "agent_branch_cleared",
      data: {
        agent_id: "a-1",
        branch: "feature/foo",
        reason: "boot_sweep: branch not present in project git",
      },
    });

    const agents = useAgentStore.getState().agents;
    expect(agents.find((a) => a.id === "a-1")!.branch).toBeNull();
    expect(agents.find((a) => a.id === "a-2")!.branch).toBe("feature/bar");
  });

  it("agent_branch_cleared without agent_id falls back to matching by branch", () => {
    useAgentStore.setState({
      agents: [
        makeAgent({ id: "a-1", branch: "feature/foo" }),
        makeAgent({ id: "a-2", branch: "feature/foo" }),
        makeAgent({ id: "a-3", branch: "feature/bar" }),
      ],
    });

    handleWsMessage({
      type: "agent_branch_cleared",
      data: { branch: "feature/foo" },
    });

    const agents = useAgentStore.getState().agents;
    expect(agents.find((a) => a.id === "a-1")!.branch).toBeNull();
    expect(agents.find((a) => a.id === "a-2")!.branch).toBeNull();
    expect(agents.find((a) => a.id === "a-3")!.branch).toBe("feature/bar");
  });

  it("hook_event is logged via the validator instead of swallowed", () => {
    const debug = vi.spyOn(console, "debug").mockImplementation(() => {});

    handleWsMessage({
      type: "hook_event",
      data: { session_id: "s-1", hook_type: "PreToolUse", tool_name: "bash" },
    });

    expect(debug).toHaveBeenCalled();
    expect(String(debug.mock.calls[0][0])).toMatch(/hook_event/);

    debug.mockRestore();
  });

  it("runner_connected validates and is accepted as a no-op (Phase 4)", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});

    expect(() =>
      handleWsMessage({
        type: "runner_connected",
        data: { runner_id: "r-1", runner_name: "alpha" },
      }),
    ).not.toThrow();
    // No `dropped malformed` warning — the schema accepted the payload.
    expect(warn).not.toHaveBeenCalled();

    warn.mockRestore();
  });

  // Audit §4 acceptance: a slow `/api/plans` response arriving AFTER a
  // `task_status_changed` event must NOT clobber the WS patch. The handler
  // defers the patch behind the in-flight fetch so the final state is
  // "fetch result then WS patch", not "WS patch overwritten by fetch".
  it(
    "task_status_changed: WS patch lands on top of in-flight fetchPlans " +
      "result, not under it (audit §4 race)",
    async () => {
      // Server's snapshot at fetch-start time: task 1.1 still pending,
      // doneCount 0. The fetch resolves AFTER the WS event (at 200ms).
      // The state change that triggered the WS event happened on the
      // server but is NOT yet reflected in the fetch response.
      const fetchedPlans: PlanSummary[] = [
        {
          name: "p",
          title: "P",
          project: null,
          phaseCount: 1,
          taskCount: 1,
          doneCount: 0,
          createdAt: "2026-04-12T00:00:00Z",
          modifiedAt: "2026-04-12T00:00:00Z",
        },
      ];
      const fetchSpy = vi.fn().mockImplementation(async (url: unknown) => {
        const u = typeof url === "string" ? url : String(url);
        if (u.endsWith("/api/plans")) {
          await new Promise((r) => setTimeout(r, 200));
          return new Response(JSON.stringify(fetchedPlans), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          });
        }
        return new Response("[]", {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      });
      vi.stubGlobal("fetch", fetchSpy);

      const selectedPlan: ParsedPlan = makePlan({
        name: "p",
        phases: [
          {
            number: 1,
            title: "Phase 1",
            description: "",
            tasks: [
              {
                number: "1.1",
                title: "Task 1.1",
                description: "",
                filePaths: [],
                acceptance: "",
                status: "pending",
              },
            ],
          },
        ],
      });
      const initialSummary: PlanSummary = { ...fetchedPlans[0] };
      usePlanStore.setState({
        selectedPlan,
        plans: [initialSummary],
      });

      // Start the fetch — in flight for 200ms.
      const fetchPromise = usePlanStore.getState().fetchPlans();

      // 50ms in, fire the WS event. The handler should DEFER the patch
      // behind the in-flight fetch (no synchronous patch yet).
      await new Promise((r) => setTimeout(r, 50));
      handleWsMessage({
        type: "task_status_changed",
        data: {
          plan_name: "p",
          task_number: "1.1",
          status: "completed",
        },
      });

      // Mid-state assertion: patch has NOT been applied yet because the
      // fetch is still in flight. (If the patch had run synchronously the
      // fetch's response would clobber it on resolve.)
      expect(usePlanStore.getState().selectedPlan!.phases[0].tasks[0].status).toBe("pending");

      // Wait for fetch to resolve, then for the deferred .then() callback.
      await fetchPromise;
      await Promise.resolve();
      await Promise.resolve();

      const final = usePlanStore.getState();
      // `plans[]` was overwritten by the fetch result, then bumped by the
      // deferred patchTaskStatus's signed delta — doneCount must be 1.
      // Without deferral: fetch lands AFTER the synchronous patch and resets
      // doneCount back to 0, even though `selectedPlan.task.status` keeps
      // the patched "completed" — incoherent state, the bug audit §4
      // describes.
      expect(final.plans).toHaveLength(1);
      expect(final.plans[0].name).toBe("p");
      expect(final.plans[0].doneCount).toBe(1);
      expect(final.selectedPlan!.phases[0].tasks[0].status).toBe("completed");

      vi.unstubAllGlobals();
    },
  );

  // Audit §4 acceptance: an external subscriber registered via the
  // ws-store API must keep receiving events across a reconnect cycle.
  // Before this refactor, AuditLog grabbed the raw socket and bound a
  // listener to it; the new socket created on reconnect had no
  // listener, so audit_log events were silently dropped until the
  // user remounted the component. Subscribers now live in a module-
  // level Set decoupled from the WsStore's `socket` field, so
  // toggling `socket` / `connected` cannot disturb them.
  it(
    "subscribeToWsEvents survives a reconnect cycle " +
      "(audit_log handler keeps firing after socket swap)",
    () => {
      const handler = vi.fn();
      const unsubscribe = subscribeToWsEvents(["audit_log"], handler);

      const auditPayload = (resourceId: string) => ({
        type: "audit_log" as const,
        data: {
          org_id: "default-org",
          user_email: null,
          action: "agent.start",
          resource_type: "agent",
          resource_id: resourceId,
        },
      });

      // First delivery — through the initial "socket".
      handleWsMessage(auditPayload("ag-1"));
      expect(handler).toHaveBeenCalledTimes(1);
      expect(handler.mock.calls[0][0]).toMatchObject({
        type: "audit_log",
        data: { resource_id: "ag-1" },
      });

      // Mimic ws-store reconnect internals: ws.onclose nulls socket
      // and flips connected=false; ws.onopen later sets a new socket
      // and connected=true. Subscribers live outside the store, so the
      // toggle is a no-op for them.
      useWsStore.setState({ socket: null, connected: false });
      useWsStore.setState({
        socket: {} as WebSocket,
        connected: true,
      });

      // Delivery through the "new socket" — handler must still fire.
      handleWsMessage(auditPayload("ag-2"));
      expect(handler).toHaveBeenCalledTimes(2);
      expect(handler.mock.calls[1][0]).toMatchObject({
        type: "audit_log",
        data: { resource_id: "ag-2" },
      });

      // Unsubscribe — no further deliveries.
      unsubscribe();
      handleWsMessage(auditPayload("ag-3"));
      expect(handler).toHaveBeenCalledTimes(2);
    },
  );

  it(
    "subscribeToWsEvents only invokes the handler for matching event " +
      "types and ignores everything else",
    () => {
      // Stub fetchAgents because the built-in dispatch for
      // `agent_started` would otherwise hit the real fetch (and bubble
      // an unhandled rejection out of the store action).
      useAgentStore.setState({ fetchAgents: vi.fn().mockResolvedValue(undefined) });

      const handler = vi.fn();
      const unsubscribe = subscribeToWsEvents(["audit_log"], handler);

      // Wrong type — must not fire.
      handleWsMessage({
        type: "agent_started",
        data: {},
      });
      expect(handler).not.toHaveBeenCalled();

      // Right type — fires once.
      handleWsMessage({
        type: "audit_log",
        data: {
          org_id: "default-org",
          user_email: null,
          action: "agent.kill",
          resource_type: "agent",
          resource_id: "ag-x",
        },
      });
      expect(handler).toHaveBeenCalledTimes(1);

      unsubscribe();
    },
  );

  it(
    "subscribeToWsEvents isolates throwing handlers " +
      "(other subscribers and built-in dispatch still run)",
    () => {
      const error = vi.spyOn(console, "error").mockImplementation(() => {});
      const fetchAgents = vi.fn().mockResolvedValue(undefined);
      useAgentStore.setState({ fetchAgents });

      const thrower = vi.fn(() => {
        throw new Error("boom");
      });
      const survivor = vi.fn();
      const unsubA = subscribeToWsEvents(["agent_started"], thrower);
      const unsubB = subscribeToWsEvents(["agent_started"], survivor);

      handleWsMessage({ type: "agent_started", data: {} });

      expect(thrower).toHaveBeenCalledTimes(1);
      expect(survivor).toHaveBeenCalledTimes(1);
      // Built-in dispatch (fetchAgents on agent_started) ran too.
      expect(fetchAgents).toHaveBeenCalledTimes(1);
      // The thrown error is swallowed and logged through console.error.
      expect(error).toHaveBeenCalled();
      expect(String(error.mock.calls[0][0])).toMatch(/subscriber/);

      unsubA();
      unsubB();
      error.mockRestore();
    },
  );

  // Coalescing: a second `fetchPlans()` call while one is in flight must
  // share the same network round trip — bootstrap + a WS-driven refetch
  // must not turn into two parallel /api/plans requests racing each other
  // (whichever resolves second wins; audit §4).
  it("fetchPlans coalesces concurrent callers onto one /api/plans request", async () => {
    let fetchCount = 0;
    const fetchSpy = vi.fn().mockImplementation(async (url: unknown) => {
      const u = typeof url === "string" ? url : String(url);
      if (u.endsWith("/api/plans")) {
        fetchCount += 1;
        await new Promise((r) => setTimeout(r, 50));
        return new Response("[]", {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      return new Response("[]", {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    });
    vi.stubGlobal("fetch", fetchSpy);

    const a = usePlanStore.getState().fetchPlans();
    const b = usePlanStore.getState().fetchPlans();
    const c = usePlanStore.getState().fetchPlans();

    await Promise.all([a, b, c]);
    // Single network round trip even though three callers asked.
    expect(fetchCount).toBe(1);

    vi.unstubAllGlobals();
  });
});

describe("auto_push_rebased pill", () => {
  it("records the retry and bumps the pill expiry on a single event", () => {
    handleWsMessage({
      type: "auto_push_rebased",
      data: {
        plan: "auto-push-rebase-on-non-fast-forward",
        task: "1.2",
        branch: "master",
        attempt: 1,
        last_rebase_sha: "a".repeat(40),
        prior_remote_sha: "b".repeat(40),
      },
    });
    const entry = usePlanStore.getState().autoPushRebases["auto-push-rebase-on-non-fast-forward"];
    expect(entry).toBeTruthy();
    expect(entry!.count).toBe(1);
    expect(entry!.branch).toBe("master");
    // Expiry must be in the future (the 10 s TTL window).
    expect(entry!.expiresAt).toBeGreaterThan(Date.now());
  });

  it("increments the running count when retries land in quick succession", () => {
    for (let i = 1; i <= 3; i += 1) {
      handleWsMessage({
        type: "auto_push_rebased",
        data: {
          plan: "p",
          task: "1.2",
          branch: "master",
          attempt: i,
          last_rebase_sha: "a".repeat(40),
          prior_remote_sha: "b".repeat(40),
        },
      });
    }
    const entry = usePlanStore.getState().autoPushRebases["p"];
    expect(entry).toBeTruthy();
    expect(entry!.count).toBe(3);
  });

  it("scopes the running count per plan", () => {
    handleWsMessage({
      type: "auto_push_rebased",
      data: {
        plan: "plan-a",
        task: "1.2",
        branch: "master",
        attempt: 1,
        last_rebase_sha: "a".repeat(40),
        prior_remote_sha: "b".repeat(40),
      },
    });
    handleWsMessage({
      type: "auto_push_rebased",
      data: {
        plan: "plan-b",
        task: "2.3",
        branch: "main",
        attempt: 1,
        last_rebase_sha: "c".repeat(40),
        prior_remote_sha: "d".repeat(40),
      },
    });
    const state = usePlanStore.getState().autoPushRebases;
    expect(state["plan-a"]!.count).toBe(1);
    expect(state["plan-b"]!.count).toBe(1);
    expect(state["plan-a"]!.branch).toBe("master");
    expect(state["plan-b"]!.branch).toBe("main");
  });
});

describe("DAG node/gate status events", () => {
  function seedDagPlan() {
    usePlanStore.setState({
      selectedPlan: {
        name: "dag-p",
        filePath: "dag-p.yaml",
        title: "DAG",
        context: "",
        project: "proj",
        createdAt: "",
        modifiedAt: "",
        phases: [],
        nodes: [
          { id: "init", type: "gate", title: "Init", gateKind: "init", status: "in_progress" },
          { id: "a", type: "task", title: "A", status: "pending" },
        ],
      } as ParsedPlan,
    });
  }

  it("gate_status_changed patches the gate node status on the selected plan", () => {
    seedDagPlan();
    handleWsMessage({
      type: "gate_status_changed",
      data: { plan_name: "dag-p", node_id: "init", status: "completed", gate_kind: "init" },
    });
    const nodes = usePlanStore.getState().selectedPlan?.nodes;
    expect(nodes?.find((n) => n.id === "init")?.status).toBe("completed");
  });

  it("node_status_changed patches a task node status", () => {
    seedDagPlan();
    handleWsMessage({
      type: "node_status_changed",
      data: { plan_name: "dag-p", node_id: "a", status: "in_progress" },
    });
    const nodes = usePlanStore.getState().selectedPlan?.nodes;
    expect(nodes?.find((n) => n.id === "a")?.status).toBe("in_progress");
  });

  it("ignores a node event for a non-selected plan", () => {
    seedDagPlan();
    handleWsMessage({
      type: "node_status_changed",
      data: { plan_name: "other", node_id: "init", status: "failed" },
    });
    const nodes = usePlanStore.getState().selectedPlan?.nodes;
    expect(nodes?.find((n) => n.id === "init")?.status).toBe("in_progress");
  });

  it("sub_plan_completed flips the sub-plan node status to completed (Task 5.2)", () => {
    usePlanStore.setState({
      selectedPlan: {
        name: "dag-p",
        filePath: "dag-p.yaml",
        title: "DAG",
        context: "",
        project: "proj",
        createdAt: "",
        modifiedAt: "",
        phases: [],
        nodes: [
          {
            id: "sp",
            type: "sub_plan",
            title: "Integration",
            status: "in_progress",
            nodes: [
              { id: "a", type: "task", title: "A", status: "completed" },
              { id: "b", type: "task", title: "B", status: "completed" },
            ],
          },
        ],
      } as ParsedPlan,
    });
    handleWsMessage({
      type: "sub_plan_completed",
      data: { plan_name: "dag-p", parent_node_id: "sp" },
    });
    const sp = usePlanStore.getState().selectedPlan?.nodes?.find((n) => n.id === "sp");
    expect(sp?.status).toBe("completed");
  });

  it("sub_plan_completed patches a nested sub-plan by its scoped id", () => {
    usePlanStore.setState({
      selectedPlan: {
        name: "dag-p",
        filePath: "dag-p.yaml",
        title: "DAG",
        context: "",
        project: "proj",
        createdAt: "",
        modifiedAt: "",
        phases: [],
        nodes: [
          {
            id: "outer",
            type: "sub_plan",
            title: "Outer",
            status: "in_progress",
            nodes: [
              {
                id: "inner",
                type: "sub_plan",
                title: "Inner",
                status: "in_progress",
                nodes: [{ id: "x", type: "task", title: "X", status: "completed" }],
              },
            ],
          },
        ],
      } as ParsedPlan,
    });
    handleWsMessage({
      type: "sub_plan_completed",
      data: { plan_name: "dag-p", parent_node_id: "outer.inner" },
    });
    const outer = usePlanStore.getState().selectedPlan?.nodes?.find((n) => n.id === "outer");
    const inner = outer?.nodes?.find((n) => n.id === "inner");
    expect(inner?.status).toBe("completed");
    // The outer parent is untouched (its own completion fires a separate event).
    expect(outer?.status).toBe("in_progress");
  });

  it("ignores sub_plan_completed for a non-selected plan", () => {
    seedDagPlan();
    handleWsMessage({
      type: "sub_plan_completed",
      data: { plan_name: "other", parent_node_id: "init" },
    });
    const nodes = usePlanStore.getState().selectedPlan?.nodes;
    // Unchanged — the selected plan has no `sp` node and the event named a
    // different plan anyway.
    expect(nodes?.find((n) => n.id === "init")?.status).toBe("in_progress");
  });

  it("gate_check_results patches the gate node's per-check results (Task 3.6)", () => {
    usePlanStore.setState({
      selectedPlan: {
        name: "dag-p",
        filePath: "dag-p.yaml",
        title: "DAG",
        context: "",
        project: "proj",
        createdAt: "",
        modifiedAt: "",
        phases: [],
        nodes: [{ id: "end", type: "gate", title: "End", gateKind: "end", status: "in_progress" }],
      } as ParsedPlan,
    });
    handleWsMessage({
      type: "gate_check_results",
      data: {
        plan_name: "dag-p",
        node_id: "end",
        gate_kind: "end",
        checks: [
          { name: "all_merged", status: "passed", detail: "3/3 branches merged" },
          { name: "compiles", status: "failed", detail: "check 'build' failed (exit 7)" },
          { name: "ci_green", status: "skipped", detail: "not run — an earlier check failed" },
        ],
      },
    });
    const end = usePlanStore.getState().selectedPlan?.nodes?.find((n) => n.id === "end");
    expect(end?.gateChecks).toHaveLength(3);
    expect(end?.gateChecks?.[0]).toMatchObject({ name: "all_merged", status: "passed" });
    expect(end?.gateChecks?.[1]).toMatchObject({ name: "compiles", status: "failed" });
  });
});
