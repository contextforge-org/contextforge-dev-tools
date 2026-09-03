#!/usr/bin/env python3
"""Publish an isolated load fixture through the dataplane's own serializer."""

from __future__ import annotations

import base64
import json
import os
import sys
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen

DATAPLANE_CONFIG_URL = (
    "http://dataplane:4445/contextforge-rs/admin/userconfigs/{subject}"
)
BACKEND_URL = "http://mcp_conformance_server:3000/mcp"
BACKEND_NAME = "standalone-load"
TOOL_NAMES = ["test_simple_text"]


def token_subject(token: str) -> str:
    parts = token.split(".")
    if len(parts) != 3:
        raise SystemExit("MCPGATEWAY_BEARER_TOKEN is not a JWT")
    payload = parts[1] + ("=" * (-len(parts[1]) % 4))
    try:
        claims = json.loads(base64.urlsafe_b64decode(payload))
    except (ValueError, json.JSONDecodeError) as error:
        raise SystemExit("MCPGATEWAY_BEARER_TOKEN has invalid claims") from error
    subject = claims.get("sub")
    if not isinstance(subject, str) or not subject:
        raise SystemExit("MCPGATEWAY_BEARER_TOKEN has no string subject")
    return subject


def prepare_config(server_id: str, protocol_version: str) -> dict:
    if not server_id:
        raise SystemExit("virtual-host-id must not be empty")
    return {
        "virtual_hosts": {
            server_id: {
                "backends": {
                    BACKEND_NAME: {
                        "name": BACKEND_NAME,
                        "url": BACKEND_URL,
                        "mcp_protocol_version": protocol_version,
                        "passthrough_headers": [],
                        "add_headers": {},
                        "remove_headers": [],
                        "tool_name_aliases": [
                            {
                                "downstream_prefixed_name": name,
                                "upstream_name": name,
                            }
                            for name in TOOL_NAMES
                        ],
                        "resource_uri_aliases": [],
                        "prompt_name_aliases": [],
                        "completion": {},
                        "tool_schemas": {name: {} for name in TOOL_NAMES},
                    }
                }
            }
        }
    }


def publish_config(subject: str, config: dict) -> None:
    endpoint = DATAPLANE_CONFIG_URL.format(subject=quote(subject, safe=""))
    request = Request(
        endpoint,
        data=json.dumps(config).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urlopen(request, timeout=30) as response:
            if response.status != 202:
                raise SystemExit(
                    f"dataplane config serializer returned HTTP {response.status}"
                )
    except HTTPError as error:
        detail = error.read(512).decode(errors="replace").strip()
        raise SystemExit(
            f"dataplane config serializer returned HTTP {error.code}: {detail}"
        ) from error
    except URLError as error:
        raise SystemExit(f"dataplane config serializer is unavailable: {error.reason}") from error


def main() -> None:
    import msgpack
    import redis

    if len(sys.argv) != 3:
        raise SystemExit(
            "usage: prepare_standalone_config.py <virtual-host-id> <protocol-version>"
        )
    server_id, protocol_version = sys.argv[1:]
    token = os.environ.get("MCPGATEWAY_BEARER_TOKEN", "")
    if not token:
        raise SystemExit("MCPGATEWAY_BEARER_TOKEN is required")
    subject = token_subject(token)
    key = msgpack.dumps(("UserConfig", subject), use_bin_type=True)
    client = redis.Redis.from_url(
        os.environ.get("REDIS_URL", "redis://redis:6379/0"),
        decode_responses=False,
    )
    publish_config(subject, prepare_config(server_id, protocol_version))
    if client.ttl(key) != -1:
        raise SystemExit("dataplane serializer did not persist the Redis snapshot")
    print(json.dumps(TOOL_NAMES, separators=(",", ":")))


if __name__ == "__main__":
    main()
