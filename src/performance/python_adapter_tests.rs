use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn python() -> &'static str {
    if Command::new("python3").arg("--version").output().is_ok() {
        "python3"
    } else {
        "python"
    }
}

fn scripts_dir() -> PathBuf {
    workspace_root().join("scripts")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn locust_stub() -> TempDir {
    let directory = tempfile::tempdir().expect("temporary Python stub should be created");
    let locust = directory.path().join("locust");
    fs::create_dir(&locust).expect("Locust stub package should be created");
    fs::write(
        locust.join("__init__.py"),
        r#"
class HttpUser:
    pass

class Hook:
    def add_listener(self, function):
        return function

class Events:
    quitting = Hook()
    init = Hook()

events = Events()

def constant(*_args):
    return lambda: None

def task(_weight):
    return lambda function: function
"#,
    )
    .expect("Locust stub should be written");
    fs::write(
        locust.join("runners.py"),
        "class MasterRunner:\n    pass\n\nclass WorkerRunner:\n    pass\n",
    )
    .expect("Locust runner stubs should be written");
    fs::write(
        directory.path().join("gevent.py"),
        "def spawn_later(_delay, callback, *args):\n    callback(*args)\n",
    )
    .expect("gevent stub");
    directory
}

#[test]
fn locust_adapter_imports_and_handles_mcp_bodies() {
    let stub = locust_stub();
    let python_path = std::env::join_paths([stub.path(), scripts_dir().as_path()])
        .expect("Python path should join");
    let code = r#"
import json
import os
import tempfile
import locustfile_mcp as adapter

assert adapter.PROTOCOL_VERSION == "2026-07-28"
assert adapter.ACCEPT == "application/json, text/event-stream"
assert adapter.REQUEST_TIMEOUT_SECONDS == 60.0
assert adapter.mcp_path() == "/servers/server%2Fid/mcp"
adapter.MCP_STACK_MODE = "controlplane"
assert adapter.mcp_path() == "/mcp"
adapter.MCP_STACK_MODE = "dataplane"

request = adapter.jsonrpc("ping", None)
assert request["jsonrpc"] == "2.0"
assert request["method"] == "ping"
assert "params" not in request
assert isinstance(request["id"], str) and request["id"]

payload = {"jsonrpc": "2.0", "id": "1", "result": {"tools": []}}
assert adapter.parse_mcp_body(json.dumps(payload), "Application/Json; Charset=UTF-8") == payload

sse = ": heartbeat\r\nevent: message\r\ndata: {\"jsonrpc\":\"2.0\",\r\ndata: \"id\":\"1\",\"result\":{}}\r\n\r\n"
sse = "data: not-json\r\n\r\n" + sse
assert adapter.parse_mcp_body(sse, "text/event-stream; charset=utf-8") == {
    "jsonrpc": "2.0", "id": "1", "result": {}
}

assert adapter.safe_diagnostic("reflected token and session-id") == "reflected <redacted> and session-id"

assert adapter.tool_call_args("echo") == {"message": "cf-integration"}
assert adapter.tool_call_args("fast-time-echo") == {"message": "cf-integration"}
assert adapter.tool_call_args("fast_time_echo") == {"message": "cf-integration"}
assert adapter.tool_call_args("get_system_time") is None
assert adapter.tool_call_args("fast-time-get-system-time") is None
assert adapter.tool_call_args("test_simple_text") is None
for unsafe in ("delete_everything_echo", "prefix-get_system_time", "shell"):
    assert adapter.tool_call_args(unsafe) is None

user = adapter.MCPGatewayUser.__new__(adapter.MCPGatewayUser)
user._protocol_version = adapter.PROTOCOL_VERSION
user._session_id = None
initialize_headers = user._headers(include_protocol_version=False)
assert initialize_headers["Mcp-Protocol-Version"] == "2026-07-28"
user._protocol_version = adapter.PROTOCOL_VERSION
user._session_id = "session-id"
request_headers = user._headers()
assert request_headers["Mcp-Protocol-Version"] == "2026-07-28"
assert request_headers["Mcp-Session-Id"] == "session-id"

class Total:
    num_requests = 0

class Stats:
    total = Total()
    def __init__(self): self.reset_calls = 0
    def reset_all(self): self.reset_calls += 1

class Environment:
    stats = Stats()
    process_exit_code = 0

empty_environment = Environment()
empty_environment.runner = object()
adapter.fail_empty_run(empty_environment)
assert empty_environment.process_exit_code == 1

class Hook:
    def add_listener(self, callback): self.callback = callback
class Events:
    request = Hook()
    user_error = Hook()
class Runner:
    stopped = 0
    def quit(self): self.stopped += 1
running = Environment()
running.events = Events()
running.runner = Runner()
adapter.install_fail_fast(running)
running.events.request.callback(exception=None)
assert running.runner.stopped == 0
running.events.request.callback(exception=RuntimeError("request failed"))
assert running.process_exit_code == 1
assert running.runner.stopped == 1
running.events.user_error.callback(exception=RuntimeError("user failed"))
assert running.runner.stopped == 1

from locust.runners import MasterRunner, WorkerRunner

class DistributedWorker(WorkerRunner):
    def __init__(self):
        self.messages = []
        self.stopped = 0
        self.client_id = "worker-1"
    def send_message(self, kind, payload): self.messages.append((kind, payload))
    def quit(self): self.stopped += 1

worker = Environment()
worker.events = Events()
worker.runner = DistributedWorker()
adapter.install_fail_fast(worker)
worker.events.request.callback(exception=RuntimeError("worker request failed"))
assert worker.process_exit_code == 1
assert worker.runner.messages == [(
    adapter._FAIL_FAST_MESSAGE,
    {"error": "worker request failed", "worker": "worker-1"},
)]
assert worker.runner.stopped == 0

class DistributedMaster(MasterRunner):
    def __init__(self):
        self.listeners = {}
        self.stats = Stats()
        self.stopped = 0
    def register_message(self, kind, listener): self.listeners[kind] = listener
    def quit(self): self.stopped += 1

master = Environment()
master.events = Events()
master.runner = DistributedMaster()
adapter.install_fail_fast(master)
master.runner.listeners[adapter._FAIL_FAST_MESSAGE](environment=master, msg=object())
assert master.process_exit_code == 1
assert master.runner.stopped == 1

measurement = Environment()
measurement.events = Events()
measurement.events.spawning_complete = Hook()
measurement.runner = DistributedMaster()
with tempfile.TemporaryDirectory() as directory:
    marker = os.path.join(directory, "measurement-start.txt")
    os.environ["MCP_MEASUREMENT_MARKER"] = marker
    os.environ["MCP_MEASUREMENT_SECONDS"] = "120"
    os.environ["MCP_WARMUP_SECONDS"] = "0"
    adapter.install_fail_fast(measurement)
    measurement.events.spawning_complete.callback(user_count=125)
    assert os.path.isfile(marker)
    assert float(open(marker, encoding="utf-8").read()) > 0
    assert measurement.runner.stats.reset_calls == 1
    assert measurement.runner.stopped == 1
os.environ.pop("MCP_MEASUREMENT_MARKER")
os.environ.pop("MCP_MEASUREMENT_SECONDS")
os.environ.pop("MCP_WARMUP_SECONDS")
"#;

    let output = Command::new(python())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", python_path)
        .env("MCP_SERVER_ID", "server/id")
        .env("MCPGATEWAY_BEARER_TOKEN", "token")
        .env_remove("LOCUST_REQUEST_TIMEOUT_SECONDS")
        .output()
        .expect("Python adapter check should run");

    assert!(
        output.status.success(),
        "Python adapter check failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn locust_adapter_emits_stateless_metadata_and_routing_headers() {
    let stub = locust_stub();
    let python_path = std::env::join_paths([stub.path(), scripts_dir().as_path()])
        .expect("Python path should join");
    let code = r#"
import json
import locustfile_mcp as adapter

assert adapter.STATELESS
assert adapter.PROTOCOL_VERSION == "2026-07-28"

class FakeResponse:
    def __init__(self, payload):
        self.status_code = 200
        self.headers = {
            "Content-Type": "application/json",
            "X-CF-Integration-Backend": "dataplane",
            "Mcp-Session-Id": "unexpected-legacy-session",
        }
        self.text = json.dumps({
            "jsonrpc": "2.0",
            "id": payload["id"],
            "result": {"content": [], "isError": False},
        })
        self.content = self.text.encode()
        self.failures = []
        self.successes = 0

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def failure(self, detail):
        self.failures.append(detail)

    def success(self):
        self.successes += 1

class FakeClient:
    def __init__(self):
        self.requests = []

    def post(self, path, *, data, headers, **_kwargs):
        payload = json.loads(data)
        self.requests.append((path, payload, headers))
        return FakeResponse(payload)

    def delete(self, *_args, **_kwargs):
        raise AssertionError("stateless lifecycle must not delete a session")

user = adapter.MCPGatewayUser.__new__(adapter.MCPGatewayUser)
user._protocol_version = adapter.PROTOCOL_VERSION
user._session_id = None
user._ready = True
user.client = FakeClient()
result = user._mcp_request(
    "tools/call",
    {"name": "echo", "arguments": {"message": "hello"}},
    name="tools/call",
)
assert result == {"content": [], "isError": False}
path, payload, headers = user.client.requests[0]
assert path == "/servers/server-id/mcp"
assert payload["params"]["_meta"] == {
    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
    "io.modelcontextprotocol/clientInfo": {
        "name": "cf-integration-locust", "version": "1.0"
    },
    "io.modelcontextprotocol/clientCapabilities": {},
}
assert headers["Mcp-Protocol-Version"] == "2026-07-28"
assert headers["Mcp-Method"] == "tools/call"
assert headers["Mcp-Name"] == "echo"
assert "Mcp-Session-Id" not in headers
assert user._session_id is None
before = len(user.client.requests)
user.on_stop()
assert len(user.client.requests) == before

adapter.validate_result("server/discover", {
    "supportedVersions": ["2026-07-28"],
    "capabilities": {},
    "resultType": "complete",
    "cacheScope": "private",
    "ttlMs": 0,
})
"#;

    let output = Command::new(python())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", python_path)
        .env("MCP_SERVER_ID", "server-id")
        .env("MCPGATEWAY_BEARER_TOKEN", "token")
        .env("MCP_PROTOCOL_VERSION", "2026-07-28")
        .output()
        .expect("Python stateless adapter check should run");

    assert!(
        output.status.success(),
        "Python stateless adapter check failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn locust_adapter_applies_timeouts_and_disables_redirects_and_environment_proxies() {
    let stub = locust_stub();
    let python_path = std::env::join_paths([stub.path(), scripts_dir().as_path()])
        .expect("Python path should join");
    let code = r#"
import json
import locustfile_mcp as adapter

assert adapter.REQUEST_TIMEOUT_SECONDS == 2.5
adapter.MCP_STACK_MODE = "controlplane"

class FakeResponse:
    def __init__(self, message=None, *, status=200):
        self.status_code = status
        self.headers = {"Content-Type": "application/json"}
        self.text = json.dumps(message) if message is not None else ""
        self.content = b""
        self.failures = []
        self.successes = 0

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def failure(self, detail):
        self.failures.append(detail)

    def success(self):
        self.successes += 1

class FakeClient:
    def __init__(self):
        self.timeouts = []

    def post(self, _path, *, data, timeout, allow_redirects, **_kwargs):
        assert allow_redirects is False
        self.timeouts.append(("POST", timeout))
        payload = json.loads(data)
        if "id" in payload:
            return FakeResponse({"jsonrpc": "2.0", "id": payload["id"], "result": {}})
        return FakeResponse(status=202)

    def delete(self, _path, *, timeout, allow_redirects, **_kwargs):
        assert allow_redirects is False
        self.timeouts.append(("DELETE", timeout))
        return FakeResponse(status=204)

user = adapter.MCPGatewayUser.__new__(adapter.MCPGatewayUser)
user._protocol_version = adapter.PROTOCOL_VERSION
user._session_id = "session"
user.client = FakeClient()
user.on_start()
assert user.client.trust_env is False
user.client.timeouts.clear()
assert user._mcp_request("ping", None, name="ping") == {}
user._mcp_notification("notifications/initialized", None, name="initialized")
user.on_stop()
assert user.client.timeouts == [
    ("POST", 2.5),
    ("POST", 2.5),
    ("DELETE", 2.5),
]
"#;

    let output = Command::new(python())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", &python_path)
        .env("MCP_SERVER_ID", "server-id")
        .env("MCPGATEWAY_BEARER_TOKEN", "token")
        .env("LOCUST_REQUEST_TIMEOUT_SECONDS", "2.5")
        .env("MCP_PROTOCOL_VERSION", "2025-11-25")
        .output()
        .expect("Python adapter timeout check should run");

    assert!(
        output.status.success(),
        "Python adapter timeout check failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    for invalid in ["", "0", "-1", "nan", "inf"] {
        let output = Command::new(python())
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .arg("-c")
            .arg("import locustfile_mcp")
            .env("PYTHONPATH", &python_path)
            .env("LOCUST_REQUEST_TIMEOUT_SECONDS", invalid)
            .output()
            .expect("Python adapter invalid-timeout check should run");

        assert!(
            !output.status.success(),
            "invalid timeout {invalid:?} was accepted"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(
                "LOCUST_REQUEST_TIMEOUT_SECONDS must be a finite number greater than zero"
            ),
            "unexpected invalid-timeout error for {invalid:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn locust_adapter_marks_invalid_method_results_and_notification_bodies_failed() {
    let stub = locust_stub();
    let python_path = std::env::join_paths([stub.path(), scripts_dir().as_path()])
        .expect("Python path should join");
    let code = r#"
import json
import locustfile_mcp as adapter
adapter.MCP_STACK_MODE = "controlplane"

class FakeResponse:
    def __init__(self, message=None, *, status=200, content=b""):
        self.status_code = status
        self.headers = {
            "Content-Type": "application/json",
            "X-CF-Integration-Backend": "dataplane",
        }
        self.text = json.dumps(message) if message is not None else content.decode()
        self.content = content
        self.failures = []
        self.successes = 0

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def failure(self, detail):
        self.failures.append(detail)

    def success(self):
        self.successes += 1

class FakeClient:
    def __init__(self, result=None, *, notification_content=b""):
        self.result = result
        self.notification_content = notification_content
        self.response = None

    def post(self, _path, *, data, **_kwargs):
        payload = json.loads(data)
        if "id" not in payload:
            self.response = FakeResponse(status=202, content=self.notification_content)
        else:
            message = {"jsonrpc": "2.0", "id": payload["id"], "result": self.result}
            self.response = FakeResponse(message)
        return self.response

def request(method, result):
    user = adapter.MCPGatewayUser.__new__(adapter.MCPGatewayUser)
    user._protocol_version = adapter.PROTOCOL_VERSION
    user._session_id = "session"
    user.client = FakeClient(result)
    returned = user._mcp_request(method, {}, name=method)
    return returned, user.client.response

for method, invalid in [
    ("tools/list", {}),
    ("tools/list", {"tools": {}}),
    ("tools/call", {"isError": True, "content": []}),
    ("tools/call", {}),
    ("tools/call", {"content": {}}),
    ("tools/call", {"content": [{"text": "missing type"}]}),
]:
    returned, response = request(method, invalid)
    assert returned is None, (method, invalid)
    assert response.failures and response.successes == 0, (method, invalid)

returned, response = request("tools/list", {"tools": [{"name": "safe"}]})
assert returned == {"tools": [{"name": "safe"}]}
assert response.successes == 1 and not response.failures
returned, response = request("tools/call", {"content": [{"type": "text", "text": "ok"}]})
assert returned == {"content": [{"type": "text", "text": "ok"}]}
assert response.successes == 1 and not response.failures

user = adapter.MCPGatewayUser.__new__(adapter.MCPGatewayUser)
user._protocol_version = adapter.PROTOCOL_VERSION
user._session_id = "session"
user.client = FakeClient(notification_content=b"unexpected")
user._mcp_notification("notifications/initialized", None, name="initialized")
assert user.client.response.failures and user.client.response.successes == 0
"#;

    let output = Command::new(python())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", python_path)
        .env("MCP_SERVER_ID", "server-id")
        .env("MCPGATEWAY_BEARER_TOKEN", "token")
        .output()
        .expect("Python adapter check should run");

    assert!(
        output.status.success(),
        "Python adapter validation check failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn locust_adapter_requires_exact_dataplane_backend_identity_without_reflection() {
    let stub = locust_stub();
    let python_path = std::env::join_paths([stub.path(), scripts_dir().as_path()])
        .expect("Python path should join");
    let code = r#"
import json
import locustfile_mcp as adapter

class FakeResponse:
    def __init__(self, marker):
        self.status_code = 200
        self.headers = {"Content-Type": "application/json"}
        if marker is not None:
            self.headers["X-CF-Integration-Backend"] = marker
        self.text = ""
        self.content = b""
        self.failures = []
        self.successes = 0

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def failure(self, detail):
        self.failures.append(detail)

    def success(self):
        self.successes += 1

class FakeClient:
    def __init__(self, marker):
        self.marker = marker
        self.response = None

    def post(self, _path, *, data, **_kwargs):
        payload = json.loads(data)
        self.response = FakeResponse(self.marker)
        self.response.text = json.dumps({
            "jsonrpc": "2.0", "id": payload["id"], "result": {}
        })
        return self.response

def make_request(mode, marker):
    adapter.MCP_STACK_MODE = mode
    user = adapter.MCPGatewayUser.__new__(adapter.MCPGatewayUser)
    user._protocol_version = adapter.PROTOCOL_VERSION
    user._session_id = None
    user.client = FakeClient(marker)
    returned = user._mcp_request("ping", None, name="ping")
    return returned, user.client.response

for marker in (None, "controlplane-fallback", "private-forged-marker", "dataplane, dataplane"):
    returned, response = make_request("dataplane", marker)
    assert returned is None, marker
    assert response.failures and response.successes == 0, marker
    assert "backend marker" in response.failures[0]
    assert "private-forged-marker" not in response.failures[0]

returned, response = make_request("dataplane", "dataplane")
assert returned == {}
assert response.successes == 1 and not response.failures

returned, response = make_request("controlplane", None)
assert returned == {}
assert response.successes == 1 and not response.failures
"#;

    let output = Command::new(python())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", python_path)
        .env("MCP_SERVER_ID", "server-id")
        .env("MCPGATEWAY_BEARER_TOKEN", "token")
        .output()
        .expect("Python adapter backend identity check should run");

    assert!(
        output.status.success(),
        "Python adapter backend identity check failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn locust_legacy_negotiation_drives_requests_and_failed_setup_stops_workload() {
    let stub = locust_stub();
    let python_path = std::env::join_paths([stub.path(), scripts_dir().as_path()])
        .expect("Python path should join");
    let code = r#"
import json
import locustfile_mcp as adapter

class Response:
    def __init__(self, payload, result):
        self.status_code = 200 if "id" in payload else 202
        self.headers = {"Content-Type": "application/json", "Mcp-Session-Id": "session"}
        self.text = json.dumps({"jsonrpc": "2.0", "id": payload.get("id"), "result": result})
        self.content = b"" if self.status_code == 202 else self.text.encode()
        self.failures = []
    def __enter__(self): return self
    def __exit__(self, *_args): return False
    def success(self): pass
    def failure(self, message): self.failures.append(message)

class Client:
    def __init__(self, version):
        self.version = version
        self.tools = ["echo"]
        self.requests = []
        self.responses = []
    def post(self, path, *, data, headers, **_kwargs):
        payload = json.loads(data)
        self.requests.append((payload, headers))
        if payload["method"] == "initialize":
            assert payload["params"]["protocolVersion"] == "2025-11-25"
            assert "Mcp-Protocol-Version" not in headers
            result = {"protocolVersion": self.version, "capabilities": {}, "serverInfo": {"name": "fixture", "version": "1"}}
        elif payload["method"] == "tools/list":
            result = {"tools": [{"name": name} for name in self.tools]}
        elif payload["method"] == "tools/call":
            result = {"content": []}
        else:
            result = {}
        response = Response(payload, result)
        self.responses.append(response)
        return response

for version in ["2025-11-25", "2025-06-18", "2026-07-28", "invalid", None]:
    user = adapter.MCPGatewayUser()
    user.client = Client(version)
    user.on_start()
    user.tools_call()
    if version in {"2025-11-25", "2025-06-18"}:
        assert user._ready
        assert user._protocol_version == version
        methods = [payload["method"] for payload, _ in user.client.requests]
        assert methods == ["initialize", "notifications/initialized", "tools/list", "tools/call"], methods
        for payload, headers in user.client.requests[1:]:
            assert headers["Mcp-Protocol-Version"] == version
            assert headers["Mcp-Session-Id"] == "session"
            assert "_meta" not in payload.get("params", {})
        assert all(not response.failures for response in user.client.responses)
    else:
        assert not user._ready
        assert len(user.client.requests) == 1
        assert user.client.responses[0].failures

for alias in ("echo", "fast-time-echo", "fast_time_echo"):
    user = adapter.MCPGatewayUser()
    user.client = Client("2025-11-25")
    user.client.tools = ["test_simple_text", "get_system_time", alias]
    user.on_start()
    for _ in range(10): user.tools_call()
    calls = [body for body, _ in user.client.requests if body["method"] == "tools/call"]
    assert len(calls) == 10
    assert all(body["params"] == {"name": alias, "arguments": {"message": "cf-integration"}} for body in calls)

user = adapter.MCPGatewayUser()
user.client = Client("2025-11-25")
user.client.tools = ["test_simple_text", "get_system_time"]
try:
    user.on_start()
    raise AssertionError("missing echo must fail setup")
except RuntimeError as error:
    assert "Fast Time echo tool is required" in str(error)
assert not user._ready

user = adapter.MCPGatewayUser()
user.client = Client("2025-11-25")
original_post = user.client.post
def fail_notification(*args, **kwargs):
    response = original_post(*args, **kwargs)
    if response.status_code == 202:
        response.status_code = 500
    return response
user.client.post = fail_notification
user.on_start()
user.tools_call()
assert not user._ready
assert [payload["method"] for payload, _ in user.client.requests] == ["initialize", "notifications/initialized"]
"#;
    let output = Command::new(python())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .args(["-c", code])
        .env("PYTHONPATH", python_path)
        .env("MCP_STACK_MODE", "controlplane")
        .env("MCP_PROTOCOL_VERSION", "2025-11-25")
        .env("MCPGATEWAY_BEARER_TOKEN", "token")
        .env_remove("MCP_TOOL_NAMES")
        .env_remove("MCP_SKIP_TOOL_LIST")
        .output()
        .expect("Python legacy lifecycle check should run");
    assert!(
        output.status.success(),
        "legacy lifecycle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
