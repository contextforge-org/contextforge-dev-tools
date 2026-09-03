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

#[test]
fn locust_adapter_and_compose_overlay_do_not_reference_the_removed_helper() {
    let root = workspace_root();
    let adapter = fs::read_to_string(root.join("scripts/locustfile_mcp.py"))
        .expect("Locust adapter should be readable");
    let compose = fs::read_to_string(root.join("docker/docker-compose.cf-integration.yaml"))
        .expect("Compose overlay should be readable");

    assert!(!adapter.contains("mcp_http"));
    assert!(adapter.contains("allow_redirects=False"));
    assert!(adapter.contains("timeout=REQUEST_TIMEOUT_SECONDS"));
    assert!(adapter.contains("self.client.trust_env = False"));
    assert!(!compose.contains("mcp_http.py"));
    assert!(
        !compose.contains("JWT_SECRET_KEY="),
        "the load container receives a bearer token and must not receive the signing key"
    );
    assert!(compose.contains("MCP_PROTOCOL_VERSION=${MCP_PROTOCOL_VERSION:-2026-07-28}"));
    assert!(compose.contains("standalone_load_backend:"));
    assert!(compose.contains("profiles: [\"standalone-load\"]"));
    assert!(compose.contains("MCP_CONFORMANCE_SERVER_ERA: modern"));
    assert!(
        compose.contains("LOCUST_REQUEST_TIMEOUT_SECONDS=${LOCUST_REQUEST_TIMEOUT_SECONDS:-60}")
    );
}

#[test]
fn standalone_config_helper_uses_token_subject_and_selected_protocol() {
    let code = r#"
import base64
import json
import prepare_standalone_config as helper

claims = base64.urlsafe_b64encode(json.dumps({"sub": "user-123"}).encode()).decode().rstrip("=")
assert helper.token_subject(f"header.{claims}.signature") == "user-123"

prepared = helper.prepare_config("server-123", "2026-07-28")
backend = prepared["virtual_hosts"]["server-123"]["backends"]["standalone-load"]
assert backend["url"] == "http://mcp_conformance_server:3000/mcp"
assert backend["mcp_protocol_version"] == "2026-07-28"
assert backend["tool_name_aliases"] == [{"downstream_prefixed_name": "test_simple_text", "upstream_name": "test_simple_text"}]
assert backend["tool_schemas"] == {"test_simple_text": {}}
"#;
    let output = Command::new(python())
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", scripts_dir())
        .output()
        .expect("Python helper test should run");

    assert!(
        output.status.success(),
        "standalone config helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn locust_stub() -> TempDir {
    let directory = tempfile::tempdir().expect("temporary Python stub should be created");
    fs::write(
        directory.path().join("locust.py"),
        r#"
class HttpUser:
    pass

class Hook:
    def add_listener(self, function):
        return function

class Events:
    quitting = Hook()

events = Events()

def between(*_args):
    return lambda: None

def task(_weight):
    return lambda function: function
"#,
    )
    .expect("Locust stub should be written");
    directory
}

#[test]
fn locust_adapter_imports_without_the_removed_helper_and_handles_mcp_bodies() {
    let stub = locust_stub();
    let python_path = std::env::join_paths([stub.path(), scripts_dir().as_path()])
        .expect("Python path should join");
    let code = r#"
import json
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
assert adapter.tool_call_args("get_system_time") == {"timezone": "UTC"}
assert adapter.tool_call_args("fast-time-get-system-time") == {"timezone": "UTC"}
for unsafe in ("delete_everything_echo", "prefix-get_system_time", "shell"):
    assert adapter.tool_call_args(unsafe) is None

user = adapter.MCPGatewayUser.__new__(adapter.MCPGatewayUser)
user._session_id = None
initialize_headers = user._headers(include_protocol_version=False)
assert initialize_headers["Mcp-Protocol-Version"] == "2026-07-28"
user._session_id = "session-id"
request_headers = user._headers()
assert request_headers["Mcp-Protocol-Version"] == "2026-07-28"
assert request_headers["Mcp-Session-Id"] == "session-id"

class Total:
    num_requests = 0

class Stats:
    total = Total()

class Environment:
    stats = Stats()
    process_exit_code = 0

empty_environment = Environment()
adapter.fail_empty_run(empty_environment)
assert empty_environment.process_exit_code == 1
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
user.on_stop()
before = len(user.client.requests)
user.ping()
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
fn locust_adapter_validates_and_applies_timeout_to_every_request() {
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

    def post(self, _path, *, data, timeout, **_kwargs):
        self.timeouts.append(("POST", timeout))
        payload = json.loads(data)
        if "id" in payload:
            return FakeResponse({"jsonrpc": "2.0", "id": payload["id"], "result": {}})
        return FakeResponse(status=202)

    def delete(self, _path, *, timeout, **_kwargs):
        self.timeouts.append(("DELETE", timeout))
        return FakeResponse(status=204)

user = adapter.MCPGatewayUser.__new__(adapter.MCPGatewayUser)
user._session_id = "session"
user.client = FakeClient()
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
