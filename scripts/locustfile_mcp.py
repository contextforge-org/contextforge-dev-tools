"""Locust load test for the public control-plane or dataplane MCP route.

Harness-owned replacement for the upstream locustfile_mcp_protocol.py, which
sends ``Accept: application/json`` and gets HTTP 406 from the streamable HTTP
endpoint. This file negotiates ``application/json, text/event-stream`` and
parses either response form.

Env:
  MCP_PROTOCOL_VERSION                   harness-selected wire revision (internal)
  MCP_STACK_MODE                         controlplane or dataplane
  MCP_SERVER_ID                         virtual server id (dataplane only)
  MCPGATEWAY_BEARER_TOKEN                bearer token (required)
  MCP_TOOL_NAMES                         optional comma-separated tools to call
  MCP_SKIP_TOOL_LIST                     true when direct tool aliases are supplied
  MCP_BASE_URLS                          optional comma-separated replica origins
  MCP_DIRECT_DATAPLANE                   use the native dataplane route without nginx
  MCP_FYRE_WORKLOAD                      enable the six-tool FYRE workload arguments
  MCP_EXPLICIT_ZERO_DELAY                send zero delay to Fast Time echo
  MCP_MEASUREMENT_MARKER                 FYRE path written after ramp and warmup
  MCP_MEASUREMENT_SECONDS                FYRE measured duration after the marker
  MCP_WARMUP_SECONDS                     FYRE steady-state warmup after spawning
  LOCUST_REQUEST_TIMEOUT_SECONDS         positive finite per-request timeout (default 60)
"""

from __future__ import annotations

import itertools
import json
import logging
import math
import os
import random
import time
import uuid
from pathlib import Path
from urllib.parse import quote

import gevent
from locust import constant, events, task

try:
    from locust import FastHttpUser
except ImportError:  # Minimal test doubles expose only HttpUser.
    from locust import HttpUser as FastHttpUser
from locust.runners import MasterRunner, WorkerRunner

PROTOCOL_VERSION = os.environ.get("MCP_PROTOCOL_VERSION", "2026-07-28")
STATELESS = PROTOCOL_VERSION == "2026-07-28"
LEGACY_PROTOCOL_VERSIONS = {"2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"}
if PROTOCOL_VERSION not in {"2025-11-25", "2026-07-28"}:
    raise RuntimeError(
        "MCP_PROTOCOL_VERSION must be a harness-selected client revision"
    )
ACCEPT = "application/json, text/event-stream"
_REQUEST_TIMEOUT_ERROR = (
    "LOCUST_REQUEST_TIMEOUT_SECONDS must be a finite number greater than zero"
)
_FAIL_FAST_MESSAGE = "cf_integration_fail_fast"
_LOGGER = logging.getLogger(__name__)


def _request_timeout_seconds() -> float:
    try:
        timeout = float(os.environ.get("LOCUST_REQUEST_TIMEOUT_SECONDS", "60"))
    except ValueError:
        raise RuntimeError(_REQUEST_TIMEOUT_ERROR) from None
    if not math.isfinite(timeout) or timeout <= 0:
        raise RuntimeError(_REQUEST_TIMEOUT_ERROR)
    return timeout


REQUEST_TIMEOUT_SECONDS = _request_timeout_seconds()

_TOOL_ARGUMENTS = {
    "echo": {"message": "cf-integration"},
    "fast_time_echo": {"message": "cf-integration"},
    "fast-time-echo": {"message": "cf-integration"},
}
_FYRE_TOOL_ARGUMENTS = {
    "convert_time": {
        "time": "2025-06-21T16:00:00Z",
        "source_timezone": "UTC",
        "target_timezone": "Europe/Dublin",
    },
    "get_stats": {},
    "get_system_time": {"timezone": "UTC"},
    "schema_success": {},
    "verify-protocol": {},
}


def jsonrpc(method: str, params: dict | None = None) -> dict:
    """Build one MCP JSON-RPC request."""
    payload = {"jsonrpc": "2.0", "id": str(uuid.uuid4()), "method": method}
    if params is not None:
        payload["params"] = params
    return payload


def stateless_params(params: dict | None = None) -> dict:
    """Add the mandatory 2026 per-request client metadata."""
    result = dict(params or {})
    metadata = dict(result.get("_meta") or {})
    metadata.update(
        {
            "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientInfo": {
                "name": "cf-integration-locust",
                "version": "1.0",
            },
            "io.modelcontextprotocol/clientCapabilities": {},
        }
    )
    result["_meta"] = metadata
    return result


def _sse_data_events(text: str):
    data_lines: list[str] = []
    for line in text.splitlines():
        if not line:
            if data_lines:
                yield "\n".join(data_lines)
                data_lines = []
            continue
        if line.startswith(":"):
            continue
        field, separator, value = line.partition(":")
        if separator and value.startswith(" "):
            value = value[1:]
        if field == "data":
            data_lines.append(value)
    if data_lines:
        yield "\n".join(data_lines)


def parse_mcp_body(text: str, content_type: str):
    """Return one JSON-RPC message from a JSON or SSE response body."""
    media_type = content_type.partition(";")[0].strip().lower()
    if media_type == "text/event-stream":
        message = None
        for event_data in _sse_data_events(text):
            try:
                message = json.loads(event_data)
            except ValueError:
                continue
        return message
    if media_type != "application/json":
        raise ValueError(f"unsupported MCP content type: {media_type or '<missing>'}")
    return json.loads(text) if text else None


def tool_call_args(tool_name: str) -> dict | None:
    """Use the same Fast Time echo payload for raw and control-plane aliases."""
    arguments = _TOOL_ARGUMENTS.get(tool_name)
    if (
        arguments is None
        and os.environ.get("MCP_FYRE_WORKLOAD", "false").lower() == "true"
    ):
        arguments = _FYRE_TOOL_ARGUMENTS.get(tool_name)
    if arguments is None:
        return None
    result = dict(arguments)
    if (
        tool_name == "echo"
        and os.environ.get("MCP_EXPLICIT_ZERO_DELAY", "false").lower() == "true"
    ):
        result["delay"] = 0
    return result


def validate_result(method: str, result) -> dict:
    """Validate the MCP result shape used by each load-test operation."""
    if not isinstance(result, dict):
        raise ValueError(f"{method} result must be an object")
    if method == "initialize":
        version = result.get("protocolVersion")
        if not isinstance(version, str) or version not in LEGACY_PROTOCOL_VERSIONS:
            raise ValueError(
                "initialize must negotiate a supported legacy protocol revision"
            )
        if not isinstance(result.get("capabilities"), dict):
            raise ValueError("initialize result must include capabilities")
        server_info = result.get("serverInfo")
        if not isinstance(server_info, dict) or not all(
            isinstance(server_info.get(field), str) and server_info[field]
            for field in ("name", "version")
        ):
            raise ValueError(
                "initialize result must include serverInfo name and version"
            )
    elif method == "server/discover":
        versions = result.get("supportedVersions")
        if not isinstance(versions, list) or PROTOCOL_VERSION not in versions:
            raise ValueError(
                "server/discover must advertise the requested protocol version"
            )
        if not isinstance(result.get("capabilities"), dict):
            raise ValueError("server/discover result must include capabilities")
        if not isinstance(result.get("resultType"), str):
            raise ValueError("server/discover result must include resultType")
        if not isinstance(result.get("cacheScope"), str):
            raise ValueError("server/discover result must include cacheScope")
        if not isinstance(result.get("ttlMs"), int) or result["ttlMs"] < 0:
            raise ValueError("server/discover result must include a non-negative ttlMs")
    elif method == "tools/list":
        tools = result.get("tools")
        if not isinstance(tools, list):
            raise ValueError("tools/list result must include a tools array")
        if any(
            not isinstance(tool, dict)
            or not isinstance(tool.get("name"), str)
            or not tool["name"].strip()
            for tool in tools
        ):
            raise ValueError("tools/list result contains an invalid tool")
    elif method == "tools/call":
        is_error = result.get("isError", False)
        if not isinstance(is_error, bool):
            raise ValueError("tools/call isError must be a boolean")
        if is_error:
            raise ValueError("tools/call reported isError=true")
        content = result.get("content")
        if not isinstance(content, list):
            raise ValueError("tools/call result must include a content array")
        if any(
            not isinstance(item, dict)
            or not isinstance(item.get("type"), str)
            or not item["type"]
            for item in content
        ):
            raise ValueError("tools/call result contains invalid content")
    return result


MCP_SERVER_ID = os.environ.get("MCP_SERVER_ID", "")
MCP_STACK_MODE = os.environ.get("MCP_STACK_MODE", "dataplane")
BEARER_TOKEN = os.environ.get("MCPGATEWAY_BEARER_TOKEN", "")
TOOL_NAMES = [
    name.strip()
    for name in os.environ.get("MCP_TOOL_NAMES", "").split(",")
    if name.strip()
]
SKIP_TOOL_LIST = os.environ.get("MCP_SKIP_TOOL_LIST", "false").lower() == "true"
BASE_URLS = [
    url.strip().rstrip("/")
    for url in os.environ.get("MCP_BASE_URLS", "").split(",")
    if url.strip()
]
DIRECT_DATAPLANE = os.environ.get("MCP_DIRECT_DATAPLANE", "false").lower() == "true"
_TARGET_SEQUENCE = itertools.count()


def safe_diagnostic(value) -> str:
    """Redact credentials before Locust persists a failure message."""
    text = str(value).replace("\r", "\\r").replace("\n", "\\n")
    return text.replace(BEARER_TOKEN, "<redacted>") if BEARER_TOKEN else text


def mcp_path() -> str:
    """Return the mode-aware public MCP route."""
    if MCP_STACK_MODE == "controlplane":
        return "/mcp"
    if DIRECT_DATAPLANE:
        return f"/contextforge-rs/servers/{quote(MCP_SERVER_ID, safe='')}/mcp"
    return f"/servers/{quote(MCP_SERVER_ID, safe='')}/mcp"


@events.init.add_listener
def install_fail_fast(environment, **_kwargs) -> None:
    """Stop after the first request or user error, retaining failed-run reports."""
    stopping = False

    def stop_runner():
        nonlocal stopping
        if stopping:
            return
        stopping = True
        environment.process_exit_code = 1
        # Let the triggering event finish recording statistics before stopping users.
        gevent.spawn_later(0, environment.runner.quit)

    def stop_from_worker(msg=None, **_message):
        data = getattr(msg, "data", None)
        detail = data.get("error") if isinstance(data, dict) else None
        _LOGGER.error("Distributed worker failed: %s", detail or "unspecified error")
        stop_runner()

    if isinstance(environment.runner, MasterRunner):
        environment.runner.register_message(_FAIL_FAST_MESSAGE, stop_from_worker)

    marker = os.environ.get("MCP_MEASUREMENT_MARKER")
    if marker:
        warmup_seconds = float(os.environ["MCP_WARMUP_SECONDS"])
        measurement_seconds = float(os.environ["MCP_MEASUREMENT_SECONDS"])

        def begin_measurement() -> None:
            environment.runner.stats.reset_all()
            if isinstance(environment.runner, MasterRunner):
                Path(marker).write_text(f"{time.time()}\n", encoding="utf-8")
                gevent.spawn_later(measurement_seconds, environment.runner.quit)

        def finish_warmup(**_kwargs) -> None:
            gevent.spawn_later(warmup_seconds, begin_measurement)

        environment.events.spawning_complete.add_listener(finish_warmup)

    def stop_on_error(exception=None, **_kwargs):
        nonlocal stopping
        if exception is not None and not stopping:
            stopping = True
            environment.process_exit_code = 1
            if isinstance(environment.runner, WorkerRunner):
                gevent.spawn_later(
                    0,
                    environment.runner.send_message,
                    _FAIL_FAST_MESSAGE,
                    {"error": safe_diagnostic(exception)},
                )
            else:
                gevent.spawn_later(0, environment.runner.quit)

    environment.events.request.add_listener(stop_on_error)
    environment.events.user_error.add_listener(stop_on_error)


@events.quitting.add_listener
def fail_empty_run(environment, **_kwargs) -> None:
    """Fail closed when user setup prevented every request."""
    if (
        not isinstance(environment.runner, WorkerRunner)
        and environment.stats.total.num_requests == 0
    ):
        environment.process_exit_code = 1


class MCPGatewayUser(FastHttpUser):
    """Drives discovery or initialization, then tool requests on the public route."""

    wait_time = constant(0)
    host = BASE_URLS[0] if BASE_URLS else None

    def __init__(self, *args, **kwargs):
        self._replica_index = (
            next(_TARGET_SEQUENCE) % len(BASE_URLS) if BASE_URLS else None
        )
        if self._replica_index is not None:
            self.host = BASE_URLS[self._replica_index]
        super().__init__(*args, **kwargs)
        self._session_id: str | None = None
        self._protocol_version = PROTOCOL_VERSION
        self._ready = False
        self._tool_names: list[str] = list(TOOL_NAMES)

    def on_start(self):
        self.client.trust_env = False
        if MCP_STACK_MODE not in {"controlplane", "dataplane"}:
            raise RuntimeError("MCP_STACK_MODE must be controlplane or dataplane")
        if MCP_STACK_MODE == "dataplane" and not MCP_SERVER_ID:
            raise RuntimeError("MCP_SERVER_ID is required")
        if not BEARER_TOKEN:
            raise RuntimeError("MCPGATEWAY_BEARER_TOKEN is required")
        if STATELESS:
            result = self._mcp_request(
                "server/discover", None, name="MCP server/discover"
            )
        else:
            result = self._mcp_request(
                "initialize",
                {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "cf-integration-locust",
                        "version": "1.0",
                    },
                },
                name="MCP initialize",
                include_protocol_version=False,
            )
        if result is None:
            return
        if not STATELESS and not self._session_id:
            raise RuntimeError("initialize response did not include Mcp-Session-Id")
        if not STATELESS:
            self._protocol_version = result["protocolVersion"]
            if not self._mcp_notification(
                "notifications/initialized", None, name="MCP initialized"
            ):
                return
        if not self._tool_names and not SKIP_TOOL_LIST:
            listed = self._mcp_request("tools/list", {}, name="MCP tools/list")
            if listed:
                self._tool_names = [
                    tool["name"]
                    for tool in listed.get("tools", [])
                    if isinstance(tool, dict)
                    and isinstance(tool.get("name"), str)
                    and tool["name"].strip()
                ]
        self._tool_names = [
            name for name in self._tool_names if tool_call_args(name) is not None
        ]
        if not self._tool_names:
            raise RuntimeError(
                "Fast Time echo tool is required; refusing an empty load workload"
            )
        self._ready = True

    def on_stop(self):
        if STATELESS or not self._session_id:
            return
        with self.client.delete(
            mcp_path(),
            headers=self._headers(),
            name="MCP session delete",
            catch_response=True,
            allow_redirects=False,
            timeout=REQUEST_TIMEOUT_SECONDS,
        ) as response:
            if not self._validate_backend(response):
                return
            if response.status_code not in (200, 202, 204, 404, 405):
                response.failure(
                    f"HTTP {response.status_code}; expected session termination response"
                )
                return
            response.success()

    def _headers(
        self,
        *,
        include_protocol_version: bool = True,
        method: str | None = None,
        params: dict | None = None,
    ) -> dict[str, str]:
        headers = {
            "Content-Type": "application/json",
            "Accept": ACCEPT,
            "Authorization": f"Bearer {BEARER_TOKEN}",
        }
        if include_protocol_version or STATELESS:
            headers["Mcp-Protocol-Version"] = self._protocol_version
        if STATELESS and method:
            headers["Mcp-Method"] = method
            if method in {"tools/call", "prompts/get"} and isinstance(params, dict):
                name = params.get("name")
                if isinstance(name, str) and name:
                    headers["Mcp-Name"] = name
            elif method == "resources/read" and isinstance(params, dict):
                uri = params.get("uri")
                if isinstance(uri, str) and uri:
                    headers["Mcp-Name"] = uri
            elif method in {"tasks/get", "tasks/update", "tasks/cancel"} and isinstance(
                params, dict
            ):
                task_id = params.get("taskId")
                if isinstance(task_id, str) and task_id:
                    headers["Mcp-Name"] = task_id
        if self._session_id:
            headers["Mcp-Session-Id"] = self._session_id
        return headers

    @staticmethod
    def _validate_backend(response) -> bool:
        if MCP_STACK_MODE != "dataplane" or DIRECT_DATAPLANE:
            return True
        marker = (
            response.headers.get("X-CF-Integration-Backend")
            if response.headers
            else None
        )
        if marker != "dataplane":
            response.failure("Missing or invalid dataplane backend marker")
            return False
        return True

    def _mcp_request(
        self,
        method: str,
        params: dict | None,
        name: str,
        *,
        include_protocol_version: bool = True,
    ) -> dict | None:
        """Send an MCP JSON-RPC request; return the result field or None."""
        request_params = stateless_params(params) if STATELESS else params
        payload = jsonrpc(method, request_params)
        with self.client.post(
            mcp_path(),
            data=json.dumps(payload),
            headers=self._headers(
                include_protocol_version=include_protocol_version,
                method=method,
                params=request_params,
            ),
            name=name,
            catch_response=True,
            allow_redirects=False,
            timeout=REQUEST_TIMEOUT_SECONDS,
        ) as response:
            if not self._validate_backend(response):
                return None
            session_id = (
                response.headers.get("Mcp-Session-Id") if response.headers else None
            )
            if session_id and not STATELESS:
                self._session_id = session_id

            if response.status_code != 200:
                detail = getattr(response, "error", None)
                response.failure(
                    safe_diagnostic(
                        f"HTTP {response.status_code}"
                        + (f": {detail}" if detail else "")
                    )
                )
                return None
            try:
                message = parse_mcp_body(
                    response.text, response.headers.get("Content-Type", "")
                )
            except ValueError as exc:
                response.failure(safe_diagnostic(f"Invalid body: {exc}"))
                return None
            if not isinstance(message, dict):
                response.failure("No JSON-RPC message in response")
                return None
            if message.get("jsonrpc") != "2.0" or message.get("id") != payload["id"]:
                response.failure("Invalid JSON-RPC version or response ID")
                return None
            if "error" in message:
                error = message["error"]
                response.failure(
                    safe_diagnostic(
                        f"JSON-RPC error {error.get('code', '?')}: {error.get('message', '?')}"
                    )
                )
                return None
            if "result" not in message:
                response.failure("JSON-RPC response did not include a result")
                return None
            try:
                result = validate_result(method, message["result"])
            except ValueError as exc:
                response.failure(safe_diagnostic(f"Invalid {method} result: {exc}"))
                return None
            response.success()
            return result

    def _mcp_notification(self, method: str, params: dict | None, name: str) -> bool:
        payload = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            payload["params"] = params
        with self.client.post(
            mcp_path(),
            data=json.dumps(payload),
            headers=self._headers(),
            name=name,
            catch_response=True,
            allow_redirects=False,
            timeout=REQUEST_TIMEOUT_SECONDS,
        ) as response:
            if not self._validate_backend(response):
                return False
            if response.status_code != 202:
                detail = getattr(response, "error", None)
                response.failure(
                    safe_diagnostic(
                        f"HTTP {response.status_code}; expected 202"
                        + (f": {detail}" if detail else "")
                    )
                )
                return False
            if response.content:
                response.failure("HTTP 202 notification response body must be empty")
                return False
            response.success()
            return True

    @task(1)
    def tools_call(self):
        if not self._ready:
            return
        tool = random.choice(self._tool_names)
        args = tool_call_args(tool)
        name = "MCP tools/call"
        if self._replica_index is not None:
            name += f" [replica-{self._replica_index + 1}]"
        self._mcp_request("tools/call", {"name": tool, "arguments": args}, name=name)
