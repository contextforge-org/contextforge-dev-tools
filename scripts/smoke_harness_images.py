"""Verify the released CLI and all fixture eras in built container images."""

import json
import os
import subprocess
import time
from pathlib import Path
from urllib.request import Request, urlopen


def docker(*arguments):
    """Run Docker and return its output, failing on nonzero exit."""
    return subprocess.check_output(["docker", *arguments], text=True).strip()


def verify_resumption(container):
    """Recover results produced while a legacy response stream is disconnected."""
    address = docker("port", container, "3000/tcp")
    url = f"http://{address}/mcp"
    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
        "MCP-Protocol-Version": "2025-11-25",
    }

    def post(message):
        request = Request(url, data=json.dumps(message).encode(), headers=headers)
        return urlopen(request, timeout=10)

    with post({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "resumption-smoke", "version": "1"}},
    }) as response:
        headers["Mcp-Session-Id"] = response.headers["Mcp-Session-Id"]
        response.read()
    try:
        with post({"jsonrpc": "2.0", "method": "notifications/initialized"}) as response:
            assert response.status == 202, response.status
        for request_id in range(2, 5):
            with post({"jsonrpc": "2.0", "id": request_id, "method": "tools/call", "params": {"name": "test_reconnection", "arguments": {}}}) as response:
                priming = response.read().decode()
            fields = dict(line.split(":", 1) for line in priming.splitlines() if ":" in line)
            event_id = fields["id"].strip()
            time.sleep(int(fields["retry"].strip()) / 1000)
            request = Request(url, headers={**headers, "Last-Event-ID": event_id})
            with urlopen(request, timeout=10) as response:
                for line in response:
                    if line.startswith(b"data:"):
                        message = json.loads(line[5:])
                        break
                else:
                    raise AssertionError("Resumed stream ended without the tool result")
            assert message["id"] == request_id, message
            assert "Reconnection test completed successfully" in message["result"]["content"][0]["text"], message
    finally:
        with urlopen(Request(url, headers=headers, method="DELETE"), timeout=10) as response:
            assert response.status == 200, response.status


def main():
    """Smoke test images before their release tags are published."""
    version = os.environ["IMAGE_VERSION"]
    prefix = "ghcr.io/contextforge-org/cf-integration-"
    os.environ.update({
        "CF_INTEGRATION_ROOT": str(Path.cwd()),
        "CF_HARNESS_VERSION": version,
        "CF_CONFORMANCE_SERVER_ERA": "legacy",
    })
    compose = ["compose", "--profile", "conformance", "-f", "docker/docker-compose.cf-conformance-fixture.yaml", "config", "--format", "json"]
    fixture = json.loads(docker(*compose))["services"]["mcp_conformance_server"]
    assert fixture["image"] == f"{prefix}fixture:{version}", fixture
    assert fixture["pull_policy"] == "missing", fixture
    os.environ["CF_CONFORMANCE_IMAGE"] = f"{prefix}fixture:preloaded"
    overridden = json.loads(docker(*compose))["services"]["mcp_conformance_server"]
    assert overridden["image"] == os.environ.pop("CF_CONFORMANCE_IMAGE"), overridden

    for image in ("helpers", "tools"):
        output = docker("run", "--rm", "--entrypoint", "cf-integration", f"{prefix}{image}:{version}", "--version")
        assert output == f"cf-integration {version.rsplit('-', 1)[0]}", output
    docker("run", "--rm", "--entrypoint", "conformance", f"{prefix}tools:{version}", "--help")
    docker("run", "--rm", "--entrypoint", "mcp-inspector", f"{prefix}tools:{version}", "--help")

    for era in ("legacy", "modern", "dual"):
        container = docker("run", "--detach", "--publish", "127.0.0.1::3000", "--env", f"MCP_CONFORMANCE_SERVER_ERA={era}", f"{prefix}fixture:{version}")
        try:
            for _ in range(60):
                result = subprocess.run(
                    ["docker", "exec", container, "curl", "--silent", "--output", "/dev/null", "--write-out", "%{http_code}", "http://127.0.0.1:3000/mcp"],
                    capture_output=True, text=True, check=False,
                )
                if result.returncode == 0 and result.stdout == "400":
                    break
                state = json.loads(docker("inspect", "--format", "{{json .State}}", container))
                if not state["Running"]:
                    raise RuntimeError(f"{era} fixture exited: {docker('logs', container)}")
                time.sleep(1)
            else:
                raise RuntimeError(f"{era} fixture did not become ready: {docker('logs', container)}")
            if era in ("legacy", "dual"):
                verify_resumption(container)
        finally:
            docker("rm", "--force", container)
    print("All harness images passed smoke checks")


if __name__ == "__main__":
    main()
