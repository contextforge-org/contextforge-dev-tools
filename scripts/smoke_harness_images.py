"""Verify the released CLI and all fixture eras in built container images."""

import json
import os
import subprocess
import time
from pathlib import Path


def docker(*arguments):
    """Run Docker and return its output, failing on nonzero exit."""
    return subprocess.check_output(["docker", *arguments], text=True).strip()


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
        container = docker("run", "--detach", "--env", f"MCP_CONFORMANCE_SERVER_ERA={era}", f"{prefix}fixture:{version}")
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
        finally:
            docker("rm", "--force", container)
    print("All harness images passed smoke checks")


if __name__ == "__main__":
    main()
