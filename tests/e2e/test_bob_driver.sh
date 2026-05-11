#!/usr/bin/env bash
# E2E test: Verify Bob driver can spawn with MCP config
set -euo pipefail

BASE_URL="${BASE_URL:-http://localhost:3199}"

echo "[test_bob_driver] Creating test plan with Bob driver..."
plan_id=$(curl -sf "$BASE_URL/api/plans" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "test-bob-driver",
    "tasks": [
      {
        "id": "1",
        "description": "Echo hello world",
        "driver": "bob"
      }
    ]
  }' | jq -r .plan_id)

if [ -z "$plan_id" ] || [ "$plan_id" = "null" ]; then
  echo "[test_bob_driver] FAIL: Could not create plan"
  exit 1
fi

echo "[test_bob_driver] Plan created: $plan_id"

echo "[test_bob_driver] Starting agent for task 1..."
agent_response=$(curl -sf "$BASE_URL/api/plans/$plan_id/tasks/1/start" \
  -X POST \
  -H "Content-Type: application/json" \
  -d '{"prompt": "echo hello world"}')

agent_id=$(echo "$agent_response" | jq -r .agent_id)
if [ -z "$agent_id" ] || [ "$agent_id" = "null" ]; then
  echo "[test_bob_driver] FAIL: Could not start agent"
  echo "$agent_response"
  exit 1
fi

echo "[test_bob_driver] Agent started: $agent_id"

# Wait a few seconds for agent to initialize
sleep 3

# Check agent status
agent_status=$(curl -sf "$BASE_URL/api/agents/$agent_id" | jq -r .status)
echo "[test_bob_driver] Agent status: $agent_status"

if [ "$agent_status" = "error" ]; then
  echo "[test_bob_driver] FAIL: Agent entered error state"
  curl -sf "$BASE_URL/api/agents/$agent_id" | jq .
  exit 1
fi

# Kill agent
curl -sf "$BASE_URL/api/agents/$agent_id/kill" -X POST >/dev/null

echo "[test_bob_driver] PASS: Bob driver spawned successfully"
exit 0
