// Bash stub that stands in for the `claude` CLI during e2e tests. The
// supervisor's PTY runs this in place of the real Anthropic binary so the
// flow is deterministic and offline.
//
// Mode detection has to handle two spawn paths:
//   1. Standalone (server-side spawn) — argv carries `--session-id` +
//      `--add-dir`, and the supervisor pre-checks-out the task branch.
//      The stub picks task vs planner mode from the current branch.
//   2. SaaS (runner-side spawn) — argv only carries `--effort` /
//      `--dangerously-skip-permissions`; the runner does NOT check out
//      the task branch nor `git init` the project. The stub reads the
//      prompt off stdin (the supervisor injects it via the PTY) and
//      detects `branchwork/<plan>/<task>` to identify task agents.
//
// Planner mode:
//   - Ensure the project is a git repo so subsequent task agents have a
//     baseline to branch from (saas runner skips ensure_git_initialized).
//   - Write `<plans-dir>/<plan>.yaml`. Plan name is derived from the
//     project folder (strip `-project` suffix); the e2e specs use the
//     convention `<plan>-project` for project folders.
//
// Task mode:
//   - Ensure git is initialized (saas pre-condition).
//   - Ensure HEAD is on `branchwork/<plan>/<task>` (saas pre-condition).
//   - Commit a sentinel file.
//   - PUT task_status=completed via the API (mirrors the real agent
//     contract: agent commits then marks itself completed before stopping).
//   - POST Stop to `$BRANCHWORK_HOOK_URL` (best-effort — the hook handler
//     looks up the agent by session_id which is only present on the
//     standalone path; saas spawns ignore the hook silently).
export const STUB_CLAUDE_SCRIPT = `#!/usr/bin/env bash
set -e
sid=""
add_dir=""
while [ $# -gt 0 ]; do
    case "$1" in
        --session-id) sid="$2"; shift 2 ;;
        --add-dir) add_dir="$2"; shift 2 ;;
        --effort|--mcp-config|--settings|--max-budget-usd) shift 2 ;;
        *) shift ;;
    esac
done

# Fallbacks for the saas-runner spawn path which omits these flags.
[ -z "$add_dir" ] && add_dir="$PWD"
[ -z "$sid" ] && sid="stub-$$"

# Optional debug trail: when BRANCHWORK_E2E_TRACE=1, append every step
# to a per-cwd log so the test can post-mortem stub behaviour.
debug_log() {
    if [ "\${BRANCHWORK_E2E_TRACE:-0}" = "1" ] && [ -d "$add_dir" ]; then
        printf '[stub %s] %s\\n' "$sid" "$1" >> "$add_dir/.stub-trace.log"
    fi
}
debug_log "start argv-add_dir=$add_dir pwd=$PWD api=\${BRANCHWORK_HOOK_URL:-unset}"

# Emit the claude readiness glyph (driver.rs::CLAUDE_PROMPT_GLYPH) so
# the supervisor's / runner's is_ready() probe fires immediately
# instead of waiting out its 16s deadline. Print it five times across
# 1 second so we cover the saas-runner window where the daemon's PTY
# broadcaster has no subscribers yet (the runner connects to the
# daemon socket asynchronously, after spawn — no replay buffer for
# bytes emitted in that window). 1 second is comfortably more than
# the observed runner-connect latency on Linux.
for _i in 1 2 3 4 5; do
    printf '\\xe2\\x9d\\xaf '
    sleep 0.2
done

# Hold the PTY open long enough for the parent's pidfile poll +
# UPDATE agents SET status='running' to land. Without this the Stop
# hook drops on the floor with status!='running'. The 1s of readiness
# pings above already counts toward this — sleep half a second more.
sleep 0.5

[ -d "$add_dir" ] || exit 0

# Capture up to 8 KB of the PTY-injected prompt with a 3-second deadline.
# pty_agent::send_initial_prompt fires the bytes once the readiness probe
# clears, then a 1s pause, then \\r. 3s comfortably covers both writes.
# Capture up to 8 KB of the PTY-injected prompt with a 3-second deadline.
# The bytes may not arrive at all in saas mode (the daemon's broadcaster
# has no replay buffer for the readiness window) — that's fine, the
# branch / plan-yaml fallbacks below handle that case.
prompt=$(timeout 3s head -c 8192 2>/dev/null || true)
debug_log "prompt-len=\${#prompt}"

project_name=$(basename "$add_dir")
plan_name_from_dir="\${project_name%-project}"
plans_dir="\${BRANCHWORK_STUB_PLANS_DIR:-$HOME/.claude/plans}"
plan_yaml="$plans_dir/$plan_name_from_dir.yaml"

task_branch=""
# 1. Prompt-derived branch (richest signal — embeds plan + task verbatim).
if printf '%s' "$prompt" | grep -qE 'branchwork/[a-z0-9._-]+/[0-9.]+'; then
    task_branch=$(printf '%s' "$prompt" | grep -oE 'branchwork/[a-z0-9._-]+/[0-9.]+' | head -1)
fi
# 2. Standalone path: the supervisor has already checked out the task
#    branch before invoking claude — git rev-parse is authoritative.
if [ -z "$task_branch" ] && [ -d "$add_dir/.git" ]; then
    current_branch=$(git -C "$add_dir" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
    case "$current_branch" in
        branchwork/*) task_branch="$current_branch" ;;
    esac
fi
# 3. Saas path: the runner does not checkout a branch and the prompt
#    never arrives over the broadcaster (no replay buffer in the
#    daemon for the readiness window). When the plan YAML already
#    exists in plans_dir, this is a task-mode invocation — our test
#    plans are single-task ("0.1") so we hardcode the fallback. Same
#    reasoning lets us derive plan_name from the project basename.
if [ -z "$task_branch" ] && [ -f "$plan_yaml" ] && [ -d "$add_dir/.git" ]; then
    task_branch="branchwork/$plan_name_from_dir/0.1"
fi
debug_log "task_branch=$task_branch (prompt-len=\${#prompt}, plan_yaml_exists=$([ -f "$plan_yaml" ] && echo y || echo n))"

if [ -n "$task_branch" ]; then
    plan_name="\${task_branch#branchwork/}"
    task_num="\${plan_name##*/}"
    plan_name="\${plan_name%/*}"

    if [ ! -d "$add_dir/.git" ]; then
        git -C "$add_dir" init -q --initial-branch=main
        git -C "$add_dir" config user.email branchwork@e2e.local
        git -C "$add_dir" config user.name "Branchwork e2e"
        printf 'stub initial\\n' > "$add_dir/.stub-init"
        git -C "$add_dir" add -A
        git -C "$add_dir" commit -q -m "stub: initial"
    fi

    current=$(git -C "$add_dir" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
    if [ "$current" != "$task_branch" ]; then
        git -C "$add_dir" checkout -q -B "$task_branch" 2>/dev/null || \\
            git -C "$add_dir" checkout -q -b "$task_branch" 2>/dev/null || true
    fi

    fname="$add_dir/work-$sid.txt"
    printf 'e2e stub task work for %s\\n' "$sid" > "$fname"
    git -C "$add_dir" add "$(basename "$fname")"
    git -C "$add_dir" commit -q -m "stub: $sid" >/dev/null 2>&1 || true

    api_base="\${BRANCHWORK_HOOK_URL%/hooks}"
    if [ -n "$api_base" ] && [ -n "$plan_name" ] && [ -n "$task_num" ]; then
        curl -sS --max-time 5 -X PUT \\
            "$api_base/api/plans/$plan_name/tasks/$task_num/status" \\
            -H 'Content-Type: application/json' \\
            -d '{"status":"completed"}' >/dev/null 2>&1 || true
    fi

    if [ -n "$BRANCHWORK_HOOK_URL" ] && [ -n "$sid" ]; then
        body=$(printf '{"session_id":"%s","hook_event_name":"Stop"}' "$sid")
        curl -sS --max-time 5 -X POST "$BRANCHWORK_HOOK_URL" \\
            -H 'Content-Type: application/json' \\
            -d "$body" >/dev/null 2>&1 || true
    fi
else
    if [ ! -d "$add_dir/.git" ]; then
        git -C "$add_dir" init -q --initial-branch=main
        git -C "$add_dir" config user.email branchwork@e2e.local
        git -C "$add_dir" config user.name "Branchwork e2e"
        git -C "$add_dir" commit -q --allow-empty -m "stub: initial"
    fi

    project_name=$(basename "$add_dir")
    plan_name="\${project_name%-project}"
    plans_dir="\${BRANCHWORK_STUB_PLANS_DIR:-$HOME/.claude/plans}"
    mkdir -p "$plans_dir"
    cat > "$plans_dir/$plan_name.yaml" <<YAML_EOF
title: Golden Path
context: |
  Generated by the e2e stub claude. Single phase, single task — the
  task spec drives Start → commit → PUT status → merge → completed.
project: $project_name
phases:
  - number: 0
    title: Phase 0
    description: Stubbed phase
    tasks:
      - number: '0.1'
        title: Stubbed task
        description: |
          Stubbed task for the e2e golden-path spec. The stub claude
          commits a sentinel file and marks itself completed via the API.
        acceptance: |
          The merge button completes successfully and the task's status
          is reported as completed.
        dependencies: []
YAML_EOF
fi

exit 0
`;

import { promises as fs } from "node:fs";
import * as path from "node:path";

/** Write the stub script to `<dir>/claude` and chmod +x it. */
export async function installStubClaude(dir: string): Promise<string> {
  await fs.mkdir(dir, { recursive: true });
  const target = path.join(dir, "claude");
  await fs.writeFile(target, STUB_CLAUDE_SCRIPT, { mode: 0o755 });
  return target;
}
