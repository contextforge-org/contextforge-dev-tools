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
fn standalone_config_writer_has_valid_javascript_syntax() {
    for script in [
        "conformance/write_dataplane_config.mjs",
        "standalone/generate_auth_key.mjs",
    ] {
        let output = Command::new("node")
            .arg("--check")
            .arg(scripts_dir().join(script))
            .output()
            .expect("Node standalone-helper syntax check should run");

        assert!(
            output.status.success(),
            "standalone helper syntax check failed for {script}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
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
fn locust_adapter_imports_and_handles_mcp_bodies() {
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

#[test]
fn standalone_fixture_catalog_preserves_discovered_routes_and_schemas() {
    let script = r#"
import assert from 'node:assert/strict';
import { pathToFileURL } from 'node:url';
const scriptPath = process.argv[1];
process.argv[1] = undefined;
const { fixtureCatalog } = await import(pathToFileURL(scriptPath).href);
const schema = { type: 'object', properties: { value: { type: 'string', 'x-mcp-header': 'Value' } } };
const calls = [];
globalThis.fetch = async (_, options) => {
    const request = JSON.parse(options.body);
    calls.push(request);
    assert.equal(request.params._meta['io.modelcontextprotocol/protocolVersion'], '2026-07-28');
    assert.equal(options.headers['mcp-method'], request.method);
    let result;
    switch (request.method) {
        case 'server/discover': result = {}; break;
        case 'tools/list': result = request.params.cursor !== undefined
            ? { tools: [{ name: 'new_diagnostic_tool', inputSchema: schema }], nextCursor: null }
            : { tools: [{ name: 'first', inputSchema: {} }], nextCursor: '' }; break;
        case 'resources/list': result = { resources: [{ uri: 'test://new-resource' }] }; break;
        case 'resources/templates/list': result = { resourceTemplates: [{ uriTemplate: 'test://new/{id}' }] }; break;
        case 'prompts/list': result = { prompts: [{ name: 'new_prompt' }] }; break;
        default: assert.fail(request.method);
    }
    const message = JSON.stringify({ jsonrpc: '2.0', id: request.id, result });
    return new Response(request.params.cursor !== undefined ? `event: message\ndata: ${message}\n\n` : message,
        { headers: { 'content-type': request.params.cursor !== undefined ? 'text/event-stream' : 'application/json' } });
};
const catalog = await fixtureCatalog('http://fixture/mcp', '2026-07-28');
assert.deepEqual(catalog.tools, ['first', 'new_diagnostic_tool']);
assert.deepEqual(catalog.toolSchemas.new_diagnostic_tool, schema);
assert.deepEqual(catalog.resources, ['test://new-resource']);
assert.deepEqual(catalog.resourceTemplates, ['test://new/{id}']);
assert.deepEqual(catalog.prompts, ['new_prompt']);
assert.equal(calls.filter((r) => r.method === 'tools/list').length, 2);

globalThis.fetch = async (_, options) => {
    const request = JSON.parse(options.body);
    return Response.json({ id: request.id, error: { code: -32603, message: 'fixture failed' } });
};
await assert.rejects(fixtureCatalog('http://fixture/mcp', '2026-07-28'), /successful result/);

globalThis.fetch = async (_, options) => {
    const request = JSON.parse(options.body);
    return Response.json({ id: request.id, result: request.method === 'server/discover' ? {} : {
        tools: [{ name: 'first', inputSchema: {} }], nextCursor: 'repeated',
    }});
};
await assert.rejects(fixtureCatalog('http://fixture/mcp', '2026-07-28'), /repeated cursor/);

const legacyMethods = [];
globalThis.fetch = async (_, options) => {
    if (options.method === 'DELETE') {
        assert.equal(options.headers['mcp-session-id'], 'legacy-session');
        legacyMethods.push('DELETE');
        return new Response(null, { status: 204 });
    }
    const request = JSON.parse(options.body);
    legacyMethods.push(request.method);
    assert.equal(options.headers['mcp-method'], undefined);
    assert.equal(request.params._meta, undefined);
    let result;
    if (request.method === 'initialize') {
        assert.equal(options.headers['mcp-protocol-version'], undefined);
        assert.equal(request.params.protocolVersion, '2025-11-25');
        result = { protocolVersion: '2025-11-25' };
    } else {
        assert.equal(options.headers['mcp-protocol-version'], '2025-11-25');
        assert.equal(options.headers['mcp-session-id'], 'legacy-session');
        if (request.method === 'notifications/initialized') return new Response(null, { status: 202 });
        result = request.method === 'tools/list' ? { tools: [{ name: 'legacy_tool', inputSchema: schema }] }
            : request.method === 'resources/list' ? { resources: [] }
            : request.method === 'resources/templates/list' ? { resourceTemplates: [] }
            : { prompts: [] };
    }
    const message = JSON.stringify({ id: request.id, result });
    return new Response(`event: message\ndata:\n\nevent: message\ndata: ${message}\n\n`, {
        headers: { 'content-type': 'text/event-stream', 'mcp-session-id': 'legacy-session' },
    });
};
const legacyCatalog = await fixtureCatalog('http://fixture/mcp', '2025-11-25');
assert.deepEqual(legacyCatalog.tools, ['legacy_tool']);
assert.deepEqual(legacyCatalog.toolSchemas.legacy_tool, schema);
assert.deepEqual(legacyMethods, ['initialize', 'notifications/initialized', 'tools/list',
    'resources/list', 'resources/templates/list', 'prompts/list', 'DELETE']);
"#;
    let output = Command::new("node")
        .args(["--input-type=module", "--eval", script])
        .arg(scripts_dir().join("conformance/write_dataplane_config.mjs"))
        .output()
        .expect("Node fixture catalog test runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn client_config_writer_publishes_a_schema_for_each_scenario_tool() {
    let script = r#"
import assert from 'node:assert/strict';
import { pathToFileURL } from 'node:url';
const scriptPath = process.argv[1];
process.argv = ['node', scriptPath, 'client', 'scenario-server', 'http://fixture/mcp',
    '2026-07-28', '["metadata_probe","add_numbers"]'];
process.env.MCP_CONFORMANCE_TOKEN = `header.${Buffer.from('{"sub":"scenario-user"}').toString('base64url')}.signature`;
let published = false;
globalThis.fetch = async (url, options) => {
    assert.ok(url.endsWith('/userconfigs/scenario-user'));
    assert.equal(options.method, 'POST');
    const host = JSON.parse(options.body).virtual_hosts['scenario-server'];
    assert.deepEqual(Object.keys(host.tools), ['metadata_probe', 'add_numbers']);
    assert.deepEqual(host.backends['conformance-backend'].tool_schemas, { metadata_probe: {}, add_numbers: {} });
    published = true;
    return new Response(null, { status: 202 });
};
await import(pathToFileURL(scriptPath).href);
assert.ok(published);
"#;
    let output = Command::new("node")
        .args(["--input-type=module", "--eval", script])
        .arg(scripts_dir().join("conformance/write_dataplane_config.mjs"))
        .output()
        .expect("Node config writer test runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
