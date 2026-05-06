# Bob Shell Integration

Branchwork is fully compatible with Bob Shell, an AI-powered terminal assistant. Bob can interact with Branchwork's MCP (Model Context Protocol) server to query and manage your plans and tasks.

## Overview

Branchwork exposes an MCP server at `http://localhost:3100/mcp` (or your configured port) that provides structured access to:
- Plan listings and details
- Task information and context
- Task status updates
- Cost reporting
- Blocker tracking

Bob Shell can connect to this MCP server and use these tools to help you manage your development workflow.

Bob Shell can connect to this MCP server and use these tools to help you manage your development workflow.

## Using Bob as an Agent Driver

In addition to MCP integration, Bob Shell can be used as a first-class agent driver in Branchwork, alongside Claude, Aider, Codex, and Gemini.

### Prerequisites

1. **Bob Shell installed**: Install Bob Shell and ensure the `bob` binary is in your PATH
2. **Authentication configured**: Set `ANTHROPIC_API_KEY` environment variable for Claude-based interactions
3. **Branchwork server running**: Start with `branchwork-server`

### Selecting Bob as a Driver

When creating or starting an agent in Branchwork:

1. Open the Branchwork dashboard at `http://localhost:3100`
2. Navigate to a task in your plan
3. Click the driver dropdown (defaults to "claude")
4. Select "bob" from the available drivers
5. Click "Start" to spawn a Bob Shell agent

The agent will run in an interactive terminal session, just like Claude or Aider agents.

### Driver Capabilities

Bob Shell driver in Branchwork:
- **Interactive REPL**: Full terminal session with `> ` prompt
- **Verdict support**: Can report task completion status via JSON format
- **Git integration**: Works on isolated branches like other drivers
- **Session persistence**: Survives server restarts
- **Authentication**: Uses `ANTHROPIC_API_KEY` for API access

### Current Limitations

- No session ID support (cannot resume interrupted sessions)
- No cost tracking (cost information not parsed from output)
- Interactive-only mode (no headless operation)

These limitations match other REPL-based drivers (Aider, Codex, Gemini) and don't affect normal usage.



## Setup

### Prerequisites

1. **Branchwork server running**: Start Branchwork with `branchwork-server` (default port 3100)
2. **Bob Shell installed**: Follow Bob Shell installation instructions from the official documentation

### Configuration

Branchwork already includes a `.mcp.json` file in the project root that registers the MCP server. Bob Shell will automatically discover and use this configuration when running in the Branchwork directory.

The configuration looks like this:

```json
{
  "mcpServers": {
    "branchwork": {
      "type": "http",
      "url": "http://localhost:3100/mcp"
    }
  }
}
```

If you need to customize the port or run Bob from a different directory, create or update your `.mcp.json` file with the appropriate URL.

## Available MCP Tools

Bob Shell can use the following Branchwork tools:

### Plan Management

- **`list_plans`**: List all plans with name, title, project, and task counts
  ```
  Returns: Array of plans with completion statistics
  ```

- **`get_plan`**: Get detailed information about a specific plan
  ```
  Parameters:
    - name: Plan name (file stem, e.g., "my-plan")
  Returns: Full plan with phases, tasks, and current status
  ```

### Task Operations

- **`get_task`**: Get details for a specific task
  ```
  Parameters:
    - plan: Plan name
    - task_number: Task number (e.g., "2.3")
  Returns: Task details including description, files, acceptance criteria
  ```

- **`get_task_context`**: Get rich context for a task including learnings
  ```
  Parameters:
    - plan: Plan name
    - task_number: Task number
  Returns: Task details plus recorded learnings and related completed tasks
  ```

### Status Updates

- **`update_task_status`**: Update the status of a task
  ```
  Parameters:
    - plan: Plan name
    - task_number: Task number
    - status: New status (pending/in_progress/completed/skipped/blocked)
    - reason: Optional reason for the status change
  ```

- **`report_cost`**: Report cost for a task
  ```
  Parameters:
    - plan: Plan name
    - task_number: Task number
    - cost_usd: Cost in USD
  ```

- **`report_blocker`**: Report a blocker for a task
  ```
  Parameters:
    - plan: Plan name
    - task_number: Task number
    - blocker: Description of the blocker
  ```

## Usage Examples

### Example 1: List all plans

```bash
bob "Show me all my Branchwork plans"
```

Bob will use the `list_plans` tool to fetch and display your plans with their completion status.

### Example 2: Get task details

```bash
bob "What are the details for task 2.3 in the api-refactor plan?"
```

Bob will use `get_task` to retrieve the task's description, file paths, acceptance criteria, and current status.

### Example 3: Update task status

```bash
bob "Mark task 1.2 in the frontend-redesign plan as completed"
```

Bob will use `update_task_status` to update the task status in Branchwork's database.

### Example 4: Get task context with learnings

```bash
bob "Show me the context and learnings for task 3.1 in the backend-optimization plan"
```

Bob will use `get_task_context` to retrieve the task details along with any recorded learnings and related completed tasks from the same project.

## Integration Benefits

Using Bob Shell with Branchwork provides:

1. **Natural language interface**: Query and manage plans using conversational commands
2. **Quick status updates**: Update task status without opening the dashboard
3. **Context-aware assistance**: Bob can access task context and learnings to provide better guidance
4. **Workflow automation**: Combine Bob's capabilities with Branchwork's structured task management

## Troubleshooting

### Bob can't find the MCP server

**Problem**: Bob reports that the Branchwork MCP server is unavailable.

**Solutions**:
1. Verify Branchwork is running: `curl http://localhost:3100/mcp`
2. Check the port in `.mcp.json` matches your Branchwork server port
3. Ensure you're running Bob from a directory where `.mcp.json` is accessible

### Tools not working as expected

**Problem**: Bob can use the tools but they return errors.

**Solutions**:
1. Verify your plans directory exists and contains valid YAML files
2. Check that task numbers match the format in your plan files (e.g., "2.3")
3. Ensure the Branchwork database is accessible and not corrupted

### Port conflicts

**Problem**: Port 3100 is already in use.

**Solution**: Start Branchwork on a different port and update `.mcp.json`:
```bash
branchwork-server --port 3200
```

Then update `.mcp.json`:
```json
{
  "mcpServers": {
    "branchwork": {
      "type": "http",
      "url": "http://localhost:3200/mcp"
    }
  }
}
```

## Advanced Usage

### Custom MCP Configuration

If you want Bob to connect to a remote Branchwork instance, update the URL in `.mcp.json`:

```json
{
  "mcpServers": {
    "branchwork": {
      "type": "http",
      "url": "http://your-server:3100/mcp"
    }
  }
}
```

### Multiple Branchwork Instances

You can configure Bob to connect to multiple Branchwork instances:

```json
{
  "mcpServers": {
    "branchwork-local": {
      "type": "http",
      "url": "http://localhost:3100/mcp"
    },
    "branchwork-remote": {
      "type": "http",
      "url": "http://remote-server:3100/mcp"
    }
  }
}
```

Bob will have access to tools from both instances.

## See Also

- [Branchwork MCP Protocol Documentation](architecture/protocols.md#3-dashboard-websocket)
- [Branchwork User Guide](user-guide.md)
- [MCP Tools Implementation](../server-rs/src/mcp/tools/)
- Bob Shell official documentation
