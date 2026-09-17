"""Register the remote Fast Time server and print benchmark credentials as JSON."""

from __future__ import annotations

import argparse
import json
import os
import time
import urllib.error
import urllib.request
import uuid

import jwt

SERVER_ID = "9779b6698cbd4b4995ee04a4fab38737"
EXPECTED_TOOLS = {
    "convert_time",
    "echo",
    "get_stats",
    "get_system_time",
    "schema_success",
    "verify-protocol",
}


def token() -> str:
    now = int(time.time())
    email = "admin@example.com"
    payload = {
        "username": email,
        "sub": email,
        "iat": now,
        "exp": now + 43_200,
        "iss": "mcpgateway",
        "aud": "mcpgateway-api",
        "jti": str(uuid.uuid4()),
        "env": "development",
        "user": {
            "email": email,
            "full_name": "FYRE Benchmark",
            "is_admin": True,
            "auth_provider": "cli",
        },
        "teams": None,
    }
    return jwt.encode(payload, os.environ["JWT_SECRET_KEY"], algorithm="HS256")


def api(method: str, path: str, bearer: str, data: dict | None = None):
    request = urllib.request.Request(
        os.environ.get("GATEWAY_URL", "http://127.0.0.1:4444").rstrip("/") + path,
        method=method,
        data=json.dumps(data).encode() if data is not None else None,
        headers={
            "Authorization": f"Bearer {bearer}",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        body = response.read()
        return json.loads(body) if body else None


def retry(method: str, path: str, bearer: str, data: dict | None = None):
    last: Exception | None = None
    for _attempt in range(60):
        try:
            return api(method, path, bearer, data)
        except (OSError, urllib.error.HTTPError) as error:
            last = error
            time.sleep(2)
    raise RuntimeError(f"{method} {path} did not become ready") from last


def tool_base(name: str) -> str:
    for prefix in ("fast_time_", "fast-time-"):
        if name.startswith(prefix):
            name = name[len(prefix) :]
            break
    return "verify-protocol" if name == "verify_protocol" else name


def tool_identity(tool: dict) -> str:
    for field in ("originalName", "customName", "name"):
        name = tool.get(field)
        if isinstance(name, str) and tool_base(name) in EXPECTED_TOOLS:
            return tool_base(name)
    return ""


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--backend", required=True)
    args = parser.parse_args()
    bearer = token()
    gateways = retry("GET", "/gateways", bearer) or []
    gateway = next((item for item in gateways if item.get("name") == "fast_time"), None)
    if gateway is None:
        gateway = retry(
            "POST",
            "/gateways",
            bearer,
            {"name": "fast_time", "url": args.backend, "transport": "STREAMABLEHTTP"},
        )
    gateway_id = gateway["id"]
    retry(
        "POST",
        f"/gateways/{gateway_id}/tools/refresh?include_resources=true&include_prompts=true",
        bearer,
    )
    selected = []
    all_tools = []
    for _attempt in range(60):
        all_tools = retry("GET", "/tools", bearer) or []
        selected = [
            item
            for item in all_tools
            if (item.get("gatewayId") or item.get("gateway_id")) == gateway_id
            and tool_identity(item) in EXPECTED_TOOLS
        ]
        if {tool_identity(item) for item in selected} == EXPECTED_TOOLS:
            break
        time.sleep(1)
    else:
        raise RuntimeError("Fast Time registration did not expose all six benchmark tools")
    try:
        api("DELETE", f"/servers/{SERVER_ID}", bearer)
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
    retry(
        "POST",
        "/servers",
        bearer,
        {
            "server": {
                "id": SERVER_ID,
                "name": "Fast Time Server",
                "description": "FYRE zero-delay Fast Time benchmark",
                "associated_tools": [item["id"] for item in selected],
                "associated_resources": [],
                "associated_prompts": [],
            }
        },
    )
    print(
        json.dumps(
            {
                "token": bearer,
                "server_id": SERVER_ID,
                "tool_names": sorted(item["name"] for item in selected),
            },
            separators=(",", ":"),
        )
    )


if __name__ == "__main__":
    main()
