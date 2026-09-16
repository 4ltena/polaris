"""Synthetic stdio servers shaped like the M2b app research; never a real app."""
import base64
import json
import sys

APP = sys.argv[1]
PNG = base64.b64encode(bytes.fromhex("89504e470d0a1a0a0000000d49484452")).decode()
OBJECT = {"type": "object"}
TOOLS = {
    "blender": [
        {"name": "execute_blender_code", "annotations": {"destructiveHint": True},
         "inputSchema": {"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}},
        {"name": "get_objects_summary", "annotations": {"readOnlyHint": True}, "inputSchema": OBJECT},
        {"name": "get_screenshot_of_window_as_image", "annotations": {"readOnlyHint": True}, "inputSchema": OBJECT},
    ],
    "resolve": [
        {"name": "get_resolve_status", "inputSchema": OBJECT},
        {"name": "launch_resolve", "inputSchema": OBJECT},
        {"name": "run_script", "inputSchema": {"type": "object", "properties": {
            "script": {"type": "string"}, "timeout": {"type": "integer"}}, "required": ["script"]}},
    ],
    "unity": [
        {"name": "Unity.ManageGameObject", "inputSchema": OBJECT},
        {"name": "Unity.ManageScene", "inputSchema": OBJECT},
    ],
}
VERSION = "2024-11-05" if APP == "resolve" else "2025-11-25"


def send(message):
    print(json.dumps({"jsonrpc": "2.0", **message}), flush=True)


for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    params = request.get("params") or {}
    if method == "initialize":
        result = {"protocolVersion": VERSION, "capabilities": {"tools": {"listChanged": True}},
                  "serverInfo": {"name": APP, "version": "fixture"}}
        if APP == "resolve":
            result["instructions"] = "DaVinci Resolve is not running. Call launch_resolve first."
    elif method == "tools/list":
        result = {"tools": TOOLS[APP]}
    elif method == "tools/call":
        name = params.get("name")
        if name == "get_screenshot_of_window_as_image":
            content = [{"type": "image", "data": PNG, "mimeType": "image/png"}]
        elif name == "execute_blender_code":
            content = [{"type": "text", "text": json.dumps({"status": "ok", "result": {}, "stdout": "", "stderr": ""})}]
        elif name == "run_script":
            content = [{"type": "text", "text": json.dumps({"result": "(script completed with no output or result)"})}]
        else:
            if APP == "unity":
                TOOLS["unity"].append({"name": "Unity.CustomCommand", "inputSchema": OBJECT})
                send({"method": "notifications/tools/list_changed"})
            content = [{"type": "text", "text": "ok"}]
        result = {"content": content}
    else:
        send({"id": request["id"], "error": {"code": -32601, "message": "Method not found"}})
        continue
    send({"id": request["id"], "result": result})
