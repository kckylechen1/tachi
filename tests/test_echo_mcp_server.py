#!/usr/bin/env python3
"""Minimal MCP server for testing hub_call. Exposes an 'echo' tool."""
import json, sys

def respond(id, result):
    msg = json.dumps({"jsonrpc": "2.0", "id": id, "result": result})
    sys.stdout.write(msg + "\n")
    sys.stdout.flush()

def main():
    for line in sys.stdin:
        msg = json.loads(line.strip())
        method = msg.get("method", "")
        id = msg.get("id")

        if method == "initialize":
            respond(id, {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "test-echo-server", "version": "1.0.0"}
            })
        elif method == "notifications/initialized":
            pass  # notification, no response
        elif method == "tools/list":
            respond(id, {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echoes back the input text",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string", "description": "Text to echo"}},
                            "required": ["text"]
                        }
                    },
                    {
                        "name": "add",
                        "description": "Adds two numbers",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                            "required": ["a", "b"]
                        }
                    }
                ]
            })
        elif method == "tools/call":
            tool = msg["params"]["name"]
            args = msg["params"].get("arguments", {})
            if tool == "echo":
                respond(id, {"content": [{"type": "text", "text": args.get("text", "")}]})
            elif tool == "add":
                result = args.get("a", 0) + args.get("b", 0)
                respond(id, {"content": [{"type": "text", "text": str(result)}]})
            else:
                respond(id, {"content": [{"type": "text", "text": f"Unknown tool: {tool}"}], "isError": True})
        else:
            if id:
                respond(id, {"error": {"code": -32601, "message": f"Unknown method: {method}"}})

if __name__ == "__main__":
    main()
