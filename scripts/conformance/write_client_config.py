#!/usr/bin/env python3
"""Publish one isolated upstream-client conformance route for the dataplane."""

from __future__ import annotations

import base64
import json
import os
import sys
from urllib.parse import urlparse

import msgpack
import redis


def token_subject(token: str) -> str:
    parts = token.split(".")
    if len(parts) != 3:
        raise SystemExit("MCP_CONFORMANCE_TOKEN is not a JWT")
    payload = parts[1] + ("=" * (-len(parts[1]) % 4))
    try:
        claims = json.loads(base64.urlsafe_b64decode(payload))
    except (ValueError, json.JSONDecodeError) as error:
        raise SystemExit("MCP_CONFORMANCE_TOKEN has invalid claims") from error
    subject = claims.get("sub")
    if not isinstance(subject, str) or not subject:
        raise SystemExit("MCP_CONFORMANCE_TOKEN has no string subject")
    return subject


def main() -> None:
    if len(sys.argv) != 4:
        raise SystemExit(
            "usage: write_client_config.py <virtual-host-id> <backend-url> <tool-names-json>"
        )
    virtual_host_id, backend_url, tool_names_json = sys.argv[1:]
    token = os.environ.get("MCP_CONFORMANCE_TOKEN")
    redis_url = os.environ.get("REDIS_URL")
    if not token:
        raise SystemExit("MCP_CONFORMANCE_TOKEN is required")
    if not redis_url:
        raise SystemExit("REDIS_URL is required")
    if not virtual_host_id:
        raise SystemExit("virtual-host-id must not be empty")

    parsed_url = urlparse(backend_url)
    if parsed_url.scheme not in {"http", "https"} or not parsed_url.hostname:
        raise SystemExit("backend-url must be an absolute HTTP(S) URL")
    tool_names = json.loads(tool_names_json)
    if (
        not isinstance(tool_names, list)
        or not tool_names
        or not all(isinstance(name, str) and name for name in tool_names)
    ):
        raise SystemExit("tool-names-json must be a non-empty JSON string array")
    backend_name = "conformance-backend"
    config = {
        "virtual_hosts": {
            virtual_host_id: {
                "backends": {
                    backend_name: {
                        "name": backend_name,
                        "url": backend_url,
                        "mcp_protocol_version": "2026-07-28",
                        "passthrough_headers": [],
                        "add_headers": {},
                        "remove_headers": [],
                        "tool_name_aliases": [
                            {
                                "downstream_prefixed_name": name,
                                "upstream_name": name,
                            }
                            for name in tool_names
                        ],
                        "resource_uri_aliases": [],
                        "prompt_name_aliases": [],
                        "completion": {},
                    }
                }
            }
        }
    }
    key = msgpack.dumps(("UserConfig", token_subject(token)), use_bin_type=True)
    value = msgpack.dumps(config, use_bin_type=True)
    redis.Redis.from_url(redis_url, decode_responses=False).set(key, value, ex=600)


if __name__ == "__main__":
    main()
