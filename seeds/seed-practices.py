#!/usr/bin/env python3
"""Seed Reglyze practices into Branchwork over its OWN MCP (stdio JSON-RPC).

Idempotent: a practice whose exact `rule` already exists is skipped. This is
deliberately a *client* of the MCP rather than raw SQL — seeding doubles as an
end-to-end smoke test of the pivot surface (practice_add / practice_search).

Usage: python3 seeds/seed-practices.py [path/to/practices.json]
"""

import json
import subprocess
import sys
from pathlib import Path


class McpClient:
    def __init__(self, cmd):
        self.proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        self.next_id = 1

    def _send(self, obj):
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def _recv(self, want_id):
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("MCP server closed stdout")
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == want_id:
                if "error" in msg:
                    raise RuntimeError(f"MCP error: {msg['error']}")
                return msg["result"]

    def request(self, method, params=None):
        rid = self.next_id
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}})
        return self._recv(rid)

    def notify(self, method, params=None):
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def call_tool(self, name, arguments):
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        if result.get("isError"):
            raise RuntimeError(f"{name} failed: {result}")
        # rmcp Json<T> tools return structuredContent; fall back to text blob.
        if "structuredContent" in result:
            return result["structuredContent"]
        for item in result.get("content", []):
            if item.get("type") == "text":
                try:
                    return json.loads(item["text"])
                except json.JSONDecodeError:
                    return item["text"]
        return result

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()


def main():
    seeds_path = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).parent / "practices-reglyze.json")
    seeds = json.loads(seeds_path.read_text())["practices"]

    mcp = McpClient(["branchwork-server", "mcp"])
    mcp.request(
        "initialize",
        {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "branchwork-seed", "version": "1.0"},
        },
    )
    mcp.notify("notifications/initialized")

    existing = mcp.call_tool("practice_search", {})
    existing_rules = {p["rule"] for p in existing.get("practices", [])}

    added = skipped = 0
    for p in seeds:
        if p["rule"] in existing_rules:
            skipped += 1
            continue
        mcp.call_tool(
            "practice_add",
            {
                "scope_globs": p.get("scope_globs", []),
                "keywords": p.get("keywords", []),
                "rule": p["rule"],
                "why": p.get("why"),
                "source": p.get("source"),
            },
        )
        added += 1

    total = len(mcp.call_tool("practice_search", {}).get("practices", []))
    print(f"practices: +{added} added, {skipped} already present, {total} total")
    mcp.close()


if __name__ == "__main__":
    main()
