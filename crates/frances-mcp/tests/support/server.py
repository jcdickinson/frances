"""Small wire fixture: no SDK, so tests exercise the actual MCP envelopes."""
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

mode, log_path = sys.argv[1:3]
listen_id = None


def respond(message):
    global listen_id
    with open(log_path, "a") as log:
        log.write(json.dumps(message) + "\n")
    method = message["method"]
    params = message.get("params", {})
    if "id" not in message:
        return None
    envelope = {"jsonrpc": "2.0", "id": message["id"]}
    capabilities = {"tools": {}, "resources": {}, "prompts": {}}
    if mode == "notify":
        capabilities["tools"]["listChanged"] = True
    if method == "server/discover":
        if mode == "legacy":
            return {**envelope, "error": {"code": -32601, "message": "unknown method"}}
        result = {"supportedVersions": ["2026-07-28"], "capabilities": capabilities}
    elif method == "initialize":
        result = {"protocolVersion": "2025-11-25", "capabilities": capabilities,
                  "serverInfo": {"name": "fixture", "version": "1"}}
    elif method == "subscriptions/listen":
        listen_id = message["id"]
        print(json.dumps({"jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
                          "params": {"notifications": {"toolsListChanged": True},
                                     "_meta": {"io.modelcontextprotocol/subscriptionId": listen_id}}}), flush=True)
        return None
    elif method == "tools/list":
        names = ["fail", "slow", "changed", "continue"] if params.get("cursor") else ["echo"]
        if mode == "duplicate":
            names = ["echo", "echo"]
        result = {"tools": [{"name": name, "description": name, "inputSchema": {
            "type": "object", "properties": {"text": {"type": "string"}},
            "required": ["text"], "additionalProperties": False}} for name in names]}
        if not params.get("cursor") and mode != "duplicate":
            result["nextCursor"] = "second-page"
    elif method == "tools/call":
        name = params["name"]
        if name == "slow":
            return None
        if name == "forever" or name == "continue" and not params.get("requestState"):
            return {**envelope, "result": {"resultType": "input_required", "requestState": "opaque"}}
        if name == "changed" and listen_id is not None:
            print(json.dumps({"jsonrpc": "2.0", "method": "notifications/tools/list_changed",
                              "params": {"_meta": {"io.modelcontextprotocol/subscriptionId": listen_id}}}), flush=True)
        result = {"content": [{"type": "text", "text": params["arguments"]["text"]}],
                  "structuredContent": {"cwd": os.getcwd(), "env": os.getenv("MCP_FIXTURE_ENV")},
                  "isError": name == "fail", "_meta": {"private": "host-only"}}
    elif method == "resources/list":
        result = {"resources": [{"uri": "fixture://readme", "name": "Readme"}]}
    elif method == "resources/templates/list":
        result = {"resourceTemplates": [{"uriTemplate": "fixture://{name}", "name": "Fixture"}]}
    elif method == "resources/read":
        result = {"contents": [{"uri": params["uri"], "text": "fixture resource"}]}
    elif method == "prompts/list":
        result = {"prompts": [{"name": "review", "arguments": [{"name": "topic", "required": True}]}]}
    elif method == "prompts/get":
        result = {"messages": [{"role": "user", "content": {"type": "text", "text": params["arguments"]["topic"]}}]}
    else:
        return {**envelope, "error": {"code": -32601, "message": "unknown method"}}
    return {**envelope, "result": {"resultType": "complete", "ttlMs": 0, "cacheScope": "private", **result}}


if len(sys.argv) > 3 and sys.argv[3] == "http":
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            message = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            result = respond(message)
            body = json.dumps(result).encode()
            self.send_response(200 if result else 202)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(server.server_port, flush=True)
    server.serve_forever()
else:
    for line in sys.stdin:
        result = respond(json.loads(line))
        if result is not None:
            print(json.dumps(result), flush=True)
