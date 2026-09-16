"""Local, dependency-free MCP fixture; never opens files or network sockets."""
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    # Fill stderr before each protocol response; the owner must drain while waiting.
    sys.stderr.buffer.write(b"x" * (512 * 1024))
    sys.stderr.buffer.flush()
    method = request["method"]
    if method == "server/discover":
        result = {
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
            "ttlMs": 0,
            "cacheScope": "private",
        }
    elif method == "tools/list":
        result = {"tools": [{"name": "fixture", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "done"}]}
    elif method == "resources/list":
        result = {"resources": []}
    elif method == "resources/templates/list":
        result = {"resourceTemplates": []}
    elif method == "prompts/list":
        result = {"prompts": []}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
