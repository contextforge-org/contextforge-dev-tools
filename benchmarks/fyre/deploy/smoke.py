"""Call every measured Fast Time tool through every dataplane replica."""

from __future__ import annotations

import argparse
import json
import urllib.error
import urllib.request
import uuid

TOOLS = {
    "convert_time": {
        "time": "2025-06-21T16:00:00Z",
        "source_timezone": "UTC",
        "target_timezone": "Europe/Dublin",
    },
    "echo": {"message": "cf-integration", "delay": 0},
    "get_stats": {},
    "get_system_time": {"timezone": "UTC"},
    "schema_success": {},
    "verify-protocol": {},
}


def base_tool_name(name: str) -> str:
    for prefix in ("fast_time_", "fast-time-"):
        if name.startswith(prefix):
            name = name[len(prefix) :]
            break
    return "verify-protocol" if name == "verify_protocol" else name


def call(url: str, token: str, tool: str, arguments: dict) -> None:
    payload = {
        "jsonrpc": "2.0",
        "id": str(uuid.uuid4()),
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": arguments,
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {
                    "name": "cf-integration-smoke",
                    "version": "1.0",
                },
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        },
    }
    request = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Accept": "application/json, text/event-stream",
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "Mcp-Protocol-Version": "2026-07-28",
            "Mcp-Method": "tools/call",
            "Mcp-Name": tool,
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            body = response.read().decode()
            if (
                response.status != 200
                or '"error"' in body
                or '"isError":true' in body.replace(" ", "")
            ):
                raise RuntimeError(
                    f"{url} {tool} failed: HTTP {response.status}: {body[:500]}"
                )
    except urllib.error.HTTPError as error:
        body = error.read().decode(errors="replace")
        raise RuntimeError(
            f"{url} {tool} failed: HTTP {error.code}: {body[:500]}"
        ) from error


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--urls", required=True)
    parser.add_argument("--token-file", required=True)
    parser.add_argument("--tool-names", default=",".join(TOOLS))
    args = parser.parse_args()
    with open(args.token_file, encoding="utf-8") as stream:
        token = stream.read().strip()
    tool_names = [name.strip() for name in args.tool_names.split(",") if name.strip()]
    if {base_tool_name(name) for name in tool_names} != set(TOOLS):
        raise RuntimeError("smoke requires exactly the six Fast Time benchmark tools")
    for url in args.urls.split(","):
        for tool in tool_names:
            arguments = dict(TOOLS[base_tool_name(tool)])
            call(url, token, tool, arguments)
            print(f"PASS {url} {tool}")


if __name__ == "__main__":
    main()
