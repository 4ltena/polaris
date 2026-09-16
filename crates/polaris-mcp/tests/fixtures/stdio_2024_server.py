"""A stdio server that only speaks MCP 2024-11-05, like an older app bundle."""
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "initialize":
        reply = {"result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                            "serverInfo": {"name": "fixture-2024", "version": "1"}}}
    elif method == "tools/list":
        reply = {"result": {"tools": [{"name": "fixture", "inputSchema": {"type": "object"}}]}}
    elif method == "tools/call":
        reply = {"result": {"content": [{"type": "text", "text": "done"}]}}
    else:
        reply = {"error": {"code": -32601, "message": "Method not found"}}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], **reply}), flush=True)
