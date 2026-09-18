#!/usr/bin/env python3
"""Run the built-in and external FYRE OpenShift lanes in parallel."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import secrets
import subprocess
import threading
import time
from pathlib import Path

from campaign import read_stats

LANES = ("builtin", "external")
SERVER_ID = "fyre-fast-time"


class Oc:
    def __init__(self, image: str, kubeconfig: Path):
        self.image = image
        self.kubeconfig = kubeconfig.resolve()
        self.mount = self.kubeconfig.parent

    def run(
        self,
        *arguments: str,
        input_text: str | None = None,
        check: bool = True,
        timeout: int = 300,
    ) -> subprocess.CompletedProcess[str]:
        command = [
            "docker",
            "run",
            "--rm",
            "-i",
            "--platform",
            "linux/amd64",
            "-v",
            f"{self.mount}:/work",
            "-e",
            "KUBECONFIG=/work/kubeconfig",
            self.image,
            "oc",
            *arguments,
        ]
        result = subprocess.run(
            command,
            input=input_text,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=timeout,
        )
        if check and result.returncode:
            raise RuntimeError(
                f"oc command exited {result.returncode}: "
                f"{result.stderr.strip() or 'no error output'}"
            )
        return result

    def container_path(self, path: Path) -> str:
        relative = path.resolve().relative_to(self.mount)
        return f"/work/{relative.as_posix()}"

    def json(self, *arguments: str) -> dict:
        return json.loads(self.run(*arguments, "-o", "json").stdout)

    def apply(self, resource: dict) -> None:
        self.run("apply", "-f", "-", input_text=json.dumps(resource))

    def delete(self, *arguments: str) -> None:
        self.run("delete", *arguments, "--ignore-not-found", check=False)


def metadata(name: str, namespace: str | None = None) -> dict:
    result = {"name": name, "labels": {"app.kubernetes.io/part-of": "cf-fyre"}}
    if namespace:
        result["namespace"] = namespace
    return result


def resources(
    cpu: str,
    memory: str,
    request_cpu: str | None = None,
    request_memory: str | None = None,
) -> dict:
    return {
        "requests": {
            "cpu": request_cpu or cpu,
            "memory": request_memory or memory,
        },
        "limits": {"cpu": cpu, "memory": memory},
    }


def pod_size(config: dict, key: str) -> tuple[str, str]:
    size = config["infrastructure"]["openshift"][key]
    return f"{int(size['cpu_millicores'])}m", f"{int(size['memory_mib'])}Mi"


def target_size(config: dict) -> tuple[int, int]:
    scenario = config["scenarios"][0]
    return int(scenario["cpu"]) * 1000, int(scenario["memory_gb"]) * 1024


def target_request_size(config: dict) -> tuple[int, int]:
    size = config["infrastructure"]["openshift"]["target_pod"]
    return int(size["cpu_millicores"]), int(size["memory_mib"])


def instance_name(base: str, lane: str, users: int | None = None) -> str:
    suffix = f"-{users}" if users is not None else ""
    return f"{base}-{lane}{suffix}"


def with_users(base: str, users: int | None) -> str:
    return f"{base}-{users}" if users is not None else base


def env_list(values: dict[str, str]) -> list[dict]:
    return [{"name": key, "value": value} for key, value in values.items()]


def secret_env(name: str) -> dict:
    return {
        "name": "MCPGATEWAY_BEARER_TOKEN",
        "valueFrom": {"secretKeyRef": {"name": name, "key": "token"}},
    }


def wait_for(
    oc: Oc,
    namespace: str,
    kind: str,
    name: str,
    condition: str,
    timeout_seconds: int,
) -> None:
    wait_expression = (
        "--for=jsonpath={.status.phase}=Succeeded"
        if condition == "phase=Succeeded"
        else f"--for={condition}"
    )
    result = oc.run(
        "wait",
        wait_expression,
        f"{kind}/{name}",
        "-n",
        namespace,
        f"--timeout={timeout_seconds}s",
        check=False,
        timeout=timeout_seconds + 30,
    )
    if result.returncode:
        describe = oc.run(
            "describe", f"{kind}/{name}", "-n", namespace, check=False
        ).stdout
        raise RuntimeError(
            f"{kind}/{name} did not reach {condition}: {result.stderr}\n{describe}"
        )


def memory_gib(value: str) -> float:
    suffixes = {"Ki": 1 / 1024 / 1024, "Mi": 1 / 1024, "Gi": 1.0}
    for suffix, multiplier in suffixes.items():
        if value.endswith(suffix):
            return float(value[: -len(suffix)]) * multiplier
    return float(value) / 1024 / 1024 / 1024


def assign_nodes(config: dict, nodes: dict) -> dict[str, list[str]]:
    workers = []
    for item in nodes.get("items", []):
        labels = item.get("metadata", {}).get("labels", {})
        if any(
            key in labels
            for key in (
                "node-role.kubernetes.io/master",
                "node-role.kubernetes.io/control-plane",
            )
        ):
            continue
        capacity = item.get("status", {}).get("capacity", {})
        workers.append(
            {
                "name": item["metadata"]["name"],
                "cpu": int(capacity.get("cpu", 0)),
                "memory_gb": memory_gib(capacity.get("memory", "0")),
            }
        )
    pools = config["infrastructure"]["openshift"]["worker_pools"]
    if len(workers) != sum(int(pool["count"]) for pool in pools):
        raise RuntimeError(
            f"expected {sum(int(pool['count']) for pool in pools)} dedicated workers, "
            f"found {len(workers)}: "
            + ", ".join(item["name"] for item in workers)
        )
    remaining = list(workers)
    assigned: dict[str, list[str]] = {}
    for pool in sorted(pools, key=lambda item: (item["memory_gb"], item["role"])):
        assigned[pool["role"]] = []
        for _index in range(int(pool["count"])):
            matches = sorted(
                remaining,
                key=lambda node: (
                    abs(node["memory_gb"] - float(pool["memory_gb"])),
                    abs(node["cpu"] - int(pool["cpu"])),
                    node["name"],
                ),
            )
            if not matches:
                raise RuntimeError(f"no OpenShift worker remains for {pool['role']}")
            selected = matches[0]
            if selected["cpu"] != int(pool["cpu"]) or abs(
                selected["memory_gb"] - float(pool["memory_gb"])
            ) > 2:
                raise RuntimeError(
                    f"worker {selected['name']} does not match {pool['role']} "
                    f"({selected['cpu']} vCPU / {selected['memory_gb']:.1f} GiB)"
                )
            assigned[pool["role"]].append(selected["name"])
            remaining.remove(selected)
    return assigned


def assigned_node(
    nodes: dict[str, list[str]], role: str, lane: str, users: int | None
) -> str:
    candidates = nodes.get(f"{role}-{lane}", nodes.get(role, []))
    if not candidates:
        raise RuntimeError(f"no OpenShift worker assigned for {role}-{lane}")
    lane_offset = 0 if lane == "builtin" else 1
    return candidates[((users or 0) + lane_offset) % len(candidates)]


def setup_namespace(oc: Oc, namespace: str, assets: Path) -> None:
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": metadata(namespace),
        }
    )
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "ServiceAccount",
            "metadata": metadata("benchmark", namespace),
        }
    )
    oc.run(
        "adm",
        "policy",
        "add-scc-to-user",
        "anyuid",
        "-z",
        "benchmark",
        "-n",
        namespace,
    )
    files = {
        "locustfile_mcp.py": assets.parent.parent.joinpath(
            "scripts/locustfile_mcp.py"
        ).read_text(encoding="utf-8"),
        "smoke.py": assets.joinpath("deploy/smoke.py").read_text(encoding="utf-8"),
        "register_builtin.py": assets.joinpath(
            "deploy/register_builtin.py"
        ).read_text(encoding="utf-8"),
    }
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": metadata("benchmark-code", namespace),
            "data": files,
        }
    )


def service(oc: Oc, namespace: str, name: str, selector: str, ports: list[dict]) -> None:
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": metadata(name, namespace),
            "spec": {
                "selector": {"cf.contextforge/service": selector},
                "ports": ports,
            },
        }
    )


def deploy_fast_time(
    oc: Oc,
    config: dict,
    namespace: str,
    lane: str,
    node: str,
    users: int | None = None,
) -> None:
    name = instance_name("fast-time", lane, users)
    cpu, memory = pod_size(config, "backend_pod")
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                **metadata(name, namespace),
                "labels": {"cf.contextforge/service": name},
            },
            "spec": {
                "serviceAccountName": "benchmark",
                "nodeName": node,
                "restartPolicy": "Always",
                "containers": [
                    {
                        "name": "fast-time",
                        "image": config["images"]["fast_time"],
                        "env": env_list(
                            {"BIND_ADDRESS": "0.0.0.0:9080", "RUST_LOG": "warn"}
                        ),
                        "ports": [{"containerPort": 9080}],
                        "resources": resources(cpu, memory),
                        "readinessProbe": {
                            "httpGet": {"path": "/health", "port": 9080},
                            "periodSeconds": 2,
                            "failureThreshold": 90,
                        },
                    }
                ],
            },
        }
    )
    service(
        oc,
        namespace,
        name,
        name,
        [{"name": "http", "port": 9080, "targetPort": 9080}],
    )
    wait_for(oc, namespace, "pod", name, "condition=Ready", 300)


def deploy_external(
    oc: Oc,
    config: dict,
    namespace: str,
    node: str,
    users: int | None = None,
) -> tuple[str, list[str]]:
    name = instance_name("target", "external", users)
    total_cpu, total_memory = target_size(config)
    request_cpu, request_memory = target_request_size(config)
    dataplane_cpu = total_cpu - 500
    dataplane_memory = total_memory - 512
    dataplane_request_cpu = request_cpu - 500
    dataplane_request_memory = request_memory - 512
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                **metadata(name, namespace),
                "labels": {"cf.contextforge/service": name},
            },
            "spec": {
                "serviceAccountName": "benchmark",
                "nodeName": node,
                "restartPolicy": "Always",
                "volumes": [{"name": "keys", "emptyDir": {}}],
                "containers": [
                    {
                        "name": "redis",
                        "image": config["images"]["redis"],
                        "args": ["redis-server", "--save", "", "--appendonly", "no"],
                        "resources": resources("250m", "256Mi"),
                        "readinessProbe": {
                            "exec": {"command": ["redis-cli", "ping"]},
                            "periodSeconds": 2,
                        },
                    },
                    {
                        "name": "dataplane",
                        "image": config["images"]["dataplane"],
                        "env": env_list(
                            {
                                "CONTEXTFORGE_DATA_PLANE_ADDRESS": "0.0.0.0:4445",
                                "CONTEXTFORGE_DATA_PLANE_REDIS_HOSTNAME": "127.0.0.1",
                                "CONTEXTFORGE_DATA_PLANE_REDIS_PORT": "6379",
                                "CONTEXTFORGE_DATA_PLANE_REDIS_CONNECTION_MODE": "plain-text",
                                "CONTEXTFORGE_DATA_PLANE_JWKS_URL": "http://127.0.0.1:4446/.well-known/jwks.json",
                                "CONTEXTFORGE_DATA_PLANE_UPSTREAM_CONNECTION_MODE": "plain-text-or-tls",
                                "CONTEXTFORGE_GATEWAY_RS_MCP_ALLOWED_HOSTS": f"{name}:4445,127.0.0.1:4445,localhost:4445",
                                "CONTEXTFORGE_DATA_PLANE_USER_CONFIG_CACHE_EXPIRY_SECONDS": str(
                                    config["workload"]["config_cache_seconds"]
                                ),
                                "RUST_LOG": "warn",
                            }
                        ),
                        "ports": [{"containerPort": 4445}],
                        "resources": resources(
                            f"{dataplane_cpu}m",
                            f"{dataplane_memory}Mi",
                            f"{dataplane_request_cpu}m",
                            f"{dataplane_request_memory}Mi",
                        ),
                        "readinessProbe": {
                            "tcpSocket": {"port": 4445},
                            "periodSeconds": 2,
                            "failureThreshold": 90,
                        },
                    },
                    {
                        "name": "auth",
                        "image": config["images"]["helpers"],
                        "args": ["__helper", "auth"],
                        "volumeMounts": [{"name": "keys", "mountPath": "/keys"}],
                        "resources": resources("250m", "256Mi"),
                        "readinessProbe": {
                            "exec": {
                                "command": ["cf-integration", "__helper", "health"]
                            },
                            "periodSeconds": 2,
                            "failureThreshold": 90,
                        },
                    },
                ],
            },
        }
    )
    service(
        oc,
        namespace,
        name,
        name,
        [{"name": "mcp", "port": 4445, "targetPort": 4445}],
    )
    wait_for(oc, namespace, "pod", name, "condition=Ready", 300)
    token = oc.run(
        "exec",
        name,
        "-n",
        namespace,
        "-c",
        "auth",
        "--",
        "cf-integration",
        "__helper",
        "token",
        "fyre-benchmark",
        "fyre-user",
    ).stdout.strip()
    if not token or "\n" in token:
        raise RuntimeError("external dataplane token helper returned invalid output")
    backend = f"http://{instance_name('fast-time', 'external', users)}:9080/mcp"
    tools = configure_external(
        oc,
        name,
        namespace,
        token,
        backend,
        config["workload"]["protocol_version"],
    )
    return token, tools


def configure_external(
    oc: Oc,
    pod: str,
    namespace: str,
    token: str,
    backend: str,
    protocol_version: str,
) -> list[str]:
    result = oc.run(
        "exec",
        "-i",
        pod,
        "-n",
        namespace,
        "-c",
        "auth",
        "--",
        "/bin/sh",
        "-ceu",
        "IFS= read -r MCP_CONFORMANCE_TOKEN; export MCP_CONFORMANCE_TOKEN; export CF_CONFIG_REDIS_URL=redis://127.0.0.1:6379; exec cf-integration __helper fixture \"$@\"",
        "fixture-config",
        SERVER_ID,
        backend,
        protocol_version,
        input_text=f"{token}\n",
    ).stdout.splitlines()
    if not result:
        raise RuntimeError("external dataplane configuration produced no result")
    return json.loads(result[-1])


def builtin_environment(
    secret: dict[str, str], database_host: str = "builtin-db"
) -> dict[str, str]:
    return {
        "DATABASE_URL": f"postgresql+psycopg://postgres:{secret['postgres']}@{database_host}:5432/mcp",
        "REDIS_URL": f"redis://{database_host}:6379/0",
        "CACHE_TYPE": "redis",
        "JWT_ALGORITHM": "HS256",
        "JWT_SECRET_KEY": secret["jwt"],
        "JWT_AUDIENCE": "mcpgateway-api",
        "JWT_ISSUER": "mcpgateway",
        "AUTH_ENCRYPTION_SECRET": secret["encryption"],
        "DEFAULT_USER_PASSWORD": secret["password"],
        "PLATFORM_ADMIN_EMAIL": "admin@example.com",
        "PLATFORM_ADMIN_PASSWORD": secret["password"],
        "PASSWORD_CHANGE_ENFORCEMENT_ENABLED": "false",
        "AUTH_REQUIRED": "true",
        "MCP_CLIENT_AUTH_ENABLED": "true",
        "MCP_REQUIRE_AUTH": "true",
        "REQUIRE_USER_IN_DB": "false",
        "LOG_LEVEL": "WARNING",
        "TRANSPORT_TYPE": "streamablehttp",
        "MCPGATEWAY_SKIP_MIGRATIONS": "true",
        "GATEWAY_TOOL_NAME_SEPARATOR": "_",
        "SSRF_ALLOW_LOCALHOST": "true",
        "SSRF_ALLOW_PRIVATE_NETWORKS": "true",
        "SSRF_DNS_FAIL_CLOSED": "false",
        "PLUGINS_ENABLED": "false",
        "MCPGATEWAY_CATALOG_ENABLED": "false",
        "MCPGATEWAY_UI_ENABLED": "false",
        "MCPGATEWAY_ADMIN_API_ENABLED": "true",
        "ENABLE_METRICS": "false",
        "DB_METRICS_RECORDING_ENABLED": "false",
        "STRUCTURED_LOGGING_DATABASE_ENABLED": "false",
        "AUDIT_TRAIL_ENABLED": "false",
        "SECURITY_LOGGING_ENABLED": "false",
        "DISABLE_ACCESS_LOG": "true",
        "COMPRESSION_ENABLED": "false",
        "VALIDATION_MIDDLEWARE_ENABLED": "false",
        "CORRELATION_ID_ENABLED": "false",
        "OBSERVABILITY_ENABLED": "false",
        "RATE_LIMITING_ENABLED": "false",
        "GUNICORN_WORKERS": "4",
        "GUNICORN_KEEP_ALIVE": "30",
        "GUNICORN_BACKLOG": "4096",
        "DB_POOL_CLASS": "queue",
        "DB_POOL_SIZE": "15",
        "DB_MAX_OVERFLOW": "10",
        "DB_POOL_PRE_PING": "true",
        "HTTPX_MAX_CONNECTIONS": "1000",
        "HTTPX_MAX_KEEPALIVE_CONNECTIONS": "500",
        "MCP_SESSION_POOL_ENABLED": "true",
        "MCP_SESSION_POOL_MAX_PER_KEY": "1000",
        "TOOL_RATE_LIMIT": "600000",
        "TOOL_CONCURRENT_LIMIT": "5000",
    }


def deploy_builtin(
    oc: Oc,
    config: dict,
    namespace: str,
    node: str,
    users: int | None = None,
) -> tuple[str, list[str]]:
    database_name = with_users("builtin-db", users)
    target_name = instance_name("target", "builtin", users)
    migration_name = with_users("builtin-migration", users)
    registration_name = with_users("builtin-registration", users)
    total_cpu, total_memory = target_size(config)
    request_cpu, request_memory = target_request_size(config)
    postgres_cpu = total_cpu * 3 // 16
    redis_cpu = total_cpu // 16
    gateway_cpu = total_cpu - postgres_cpu - redis_cpu
    postgres_memory = total_memory * 3 // 16
    redis_memory = total_memory // 16
    gateway_memory = total_memory - postgres_memory - redis_memory
    postgres_request_cpu = request_cpu * 3 // 16
    redis_request_cpu = request_cpu // 16
    gateway_request_cpu = request_cpu - postgres_request_cpu - redis_request_cpu
    postgres_request_memory = request_memory * 3 // 16
    redis_request_memory = request_memory // 16
    gateway_request_memory = (
        request_memory - postgres_request_memory - redis_request_memory
    )
    credentials = {
        "postgres": secrets.token_hex(24),
        "jwt": secrets.token_hex(32),
        "encryption": secrets.token_hex(32),
        "password": secrets.token_hex(24),
    }
    environment = builtin_environment(credentials, database_name)
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                **metadata(database_name, namespace),
                "labels": {"cf.contextforge/service": database_name},
            },
            "spec": {
                "serviceAccountName": "benchmark",
                "nodeName": node,
                "restartPolicy": "Always",
                "containers": [
                    {
                        "name": "postgres",
                        "image": config["images"]["postgres"],
                        "args": [
                            "postgres",
                            "-c",
                            "max_connections=300",
                            "-c",
                            "shared_buffers=256MB",
                            "-c",
                            "synchronous_commit=off",
                        ],
                        "env": env_list(
                            {
                                "POSTGRES_USER": "postgres",
                                "POSTGRES_PASSWORD": credentials["postgres"],
                                "POSTGRES_DB": "mcp",
                            }
                        ),
                        "ports": [{"containerPort": 5432}],
                        "resources": resources(
                            f"{postgres_cpu}m",
                            f"{postgres_memory}Mi",
                            f"{postgres_request_cpu}m",
                            f"{postgres_request_memory}Mi",
                        ),
                        "readinessProbe": {
                            "exec": {
                                "command": ["pg_isready", "-U", "postgres", "-d", "mcp"]
                            },
                            "periodSeconds": 2,
                        },
                    },
                    {
                        "name": "redis",
                        "image": config["images"]["redis"],
                        "args": [
                            "redis-server",
                            "--save",
                            "",
                            "--appendonly",
                            "no",
                            "--maxmemory",
                            "512mb",
                            "--maxmemory-policy",
                            "allkeys-lru",
                        ],
                        "ports": [{"containerPort": 6379}],
                        "resources": resources(
                            f"{redis_cpu}m",
                            f"{redis_memory}Mi",
                            f"{redis_request_cpu}m",
                            f"{redis_request_memory}Mi",
                        ),
                        "readinessProbe": {
                            "exec": {"command": ["redis-cli", "ping"]},
                            "periodSeconds": 2,
                        },
                    },
                ],
            },
        }
    )
    service(
        oc,
        namespace,
        database_name,
        database_name,
        [
            {"name": "postgres", "port": 5432, "targetPort": 5432},
            {"name": "redis", "port": 6379, "targetPort": 6379},
        ],
    )
    wait_for(oc, namespace, "pod", database_name, "condition=Ready", 300)
    migration_env = {**environment, "MCPGATEWAY_SKIP_MIGRATIONS": "false"}
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": metadata(migration_name, namespace),
            "spec": {
                "serviceAccountName": "benchmark",
                "nodeName": node,
                "restartPolicy": "Never",
                "containers": [
                    {
                        "name": "migration",
                        "image": config["images"]["controlplane"],
                        "command": ["python3", "-m", "mcpgateway.bootstrap_db"],
                        "env": env_list(migration_env),
                        "resources": resources("500m", "512Mi"),
                    }
                ],
            },
        }
    )
    wait_for(oc, namespace, "pod", migration_name, "phase=Succeeded", 600)
    oc.delete("pod", migration_name, "-n", namespace)
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                **metadata(target_name, namespace),
                "labels": {"cf.contextforge/service": target_name},
            },
            "spec": {
                "serviceAccountName": "benchmark",
                "nodeName": node,
                "restartPolicy": "Always",
                "containers": [
                    {
                        "name": "gateway",
                        "image": config["images"]["controlplane"],
                        "env": env_list({**environment, "HOST": "0.0.0.0", "PORT": "4444"}),
                        "ports": [{"containerPort": 4444}],
                        "resources": resources(
                            f"{gateway_cpu}m",
                            f"{gateway_memory}Mi",
                            f"{gateway_request_cpu}m",
                            f"{gateway_request_memory}Mi",
                        ),
                        "readinessProbe": {
                            "httpGet": {"path": "/health", "port": 4444},
                            "periodSeconds": 3,
                            "failureThreshold": 100,
                        },
                    }
                ],
            },
        }
    )
    service(
        oc,
        namespace,
        target_name,
        target_name,
        [{"name": "mcp", "port": 4444, "targetPort": 4444}],
    )
    wait_for(oc, namespace, "pod", target_name, "condition=Ready", 600)
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": metadata(registration_name, namespace),
            "spec": {
                "serviceAccountName": "benchmark",
                "nodeName": node,
                "restartPolicy": "Never",
                "volumes": [{"name": "code", "configMap": {"name": "benchmark-code"}}],
                "containers": [
                    {
                        "name": "registration",
                        "image": config["images"]["controlplane"],
                        "command": ["python3", "/work/register_builtin.py"],
                        "args": [
                            "--backend",
                            f"http://{instance_name('fast-time', 'builtin', users)}:9080/mcp",
                        ],
                        "env": env_list(
                            {
                                **environment,
                                "GATEWAY_URL": f"http://{target_name}:4444",
                            }
                        ),
                        "volumeMounts": [{"name": "code", "mountPath": "/work"}],
                        "resources": resources("250m", "256Mi"),
                    }
                ],
            },
        }
    )
    wait_for(oc, namespace, "pod", registration_name, "phase=Succeeded", 600)
    output = oc.run(
        "logs", registration_name, "-n", namespace, "-c", "registration"
    ).stdout.splitlines()
    if not output:
        raise RuntimeError("built-in registration produced no result")
    registration = json.loads(output[-1])
    oc.delete("pod", registration_name, "-n", namespace)
    return registration["token"], registration["tool_names"]


def store_lane_secret(
    oc: Oc,
    namespace: str,
    lane: str,
    token: str,
    tools: list[str],
    users: int,
) -> None:
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": metadata(instance_name("lane", lane, users), namespace),
            "type": "Opaque",
            "stringData": {"token": token, "tools": ",".join(tools)},
        }
    )


def smoke_lane(
    oc: Oc,
    config: dict,
    namespace: str,
    lane: str,
    node: str,
    url: str,
    tools: list[str],
    users: int,
) -> None:
    name = instance_name("smoke", lane, users)
    oc.delete("pod", name, "-n", namespace)
    oc.apply(
        {
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": metadata(name, namespace),
            "spec": {
                "serviceAccountName": "benchmark",
                "nodeName": node,
                "restartPolicy": "Never",
                "volumes": [
                    {"name": "code", "configMap": {"name": "benchmark-code"}},
                    {
                        "name": "secret",
                        "secret": {
                            "secretName": instance_name("lane", lane, users)
                        },
                    },
                ],
                "containers": [
                    {
                        "name": "smoke",
                        "image": config["images"]["locust"],
                        "command": ["python"],
                        "args": [
                            "/work/smoke.py",
                            "--urls",
                            url,
                            "--token-file",
                            "/secret/token",
                            "--tool-names",
                            ",".join(tools),
                        ],
                        "volumeMounts": [
                            {"name": "code", "mountPath": "/work"},
                            {"name": "secret", "mountPath": "/secret", "readOnly": True},
                        ],
                        "resources": resources("500m", "512Mi"),
                    }
                ],
            },
        }
    )
    wait_for(oc, namespace, "pod", name, "phase=Succeeded", 300)
    oc.delete("pod", name, "-n", namespace)


def load_environment(
    config: dict, lane: str, url: str, tools: list[str], users: int
) -> list[dict]:
    values = {
        "MCP_PROTOCOL_VERSION": config["workload"]["protocol_version"],
        "MCP_STACK_MODE": "controlplane" if lane == "builtin" else "dataplane",
        "MCP_SERVER_ID": SERVER_ID,
        "MCP_DIRECT_DATAPLANE": "false" if lane == "builtin" else "true",
        "MCP_SKIP_TOOL_LIST": "true",
        "MCP_EXPLICIT_ZERO_DELAY": "true",
        "MCP_FYRE_WORKLOAD": "true",
        "MCP_TOOL_NAMES": ",".join(tools),
        "MCP_BASE_URLS": url,
        "LOCUST_REQUEST_TIMEOUT_SECONDS": "30",
        "MCP_MEASUREMENT_MARKER": "/reports/measurement-start.txt",
        "MCP_MEASUREMENT_SECONDS": str(config["workload"]["measure_seconds"]),
        "MCP_WARMUP_SECONDS": str(config["workload"]["warmup_seconds"]),
    }
    return [*env_list(values), secret_env(instance_name("lane", lane, users))]


def locust_pod(
    config: dict,
    namespace: str,
    lane: str,
    node: str,
    users: int,
    url: str,
    tools: list[str],
) -> dict:
    name = f"load-{lane}-{users}"
    workload = config["workload"]
    total = (
        int(workload["ramp_seconds"])
        + int(workload["warmup_seconds"])
        + int(workload["measure_seconds"])
        + 30
    )
    common_mounts = [
        {"name": "code", "mountPath": "/mnt/locust-cf"},
        {"name": "reports", "mountPath": "/reports"},
    ]
    common_env = load_environment(config, lane, url, tools, users)
    master_args = [
        "-f",
        "/mnt/locust-cf/locustfile_mcp.py",
        "--master",
        "--expect-workers",
        "3",
        "--headless",
        "--users",
        str(users),
        "--spawn-rate",
        str(max(1.0, users / int(workload["ramp_seconds"]))),
        "--run-time",
        f"{total}s",
        "--stop-timeout",
        "1",
        "--host",
        "http://127.0.0.1",
        "--csv",
        "/reports/locust",
        "--csv-full-history",
        "--html",
        "/reports/locust.html",
        "--json-file",
        "/reports/locust.json",
        "--logfile",
        "/reports/locust.log",
    ]
    containers = [
        {
            "name": "master",
            "image": config["images"]["locust"],
            "args": master_args,
            "env": common_env,
            "volumeMounts": common_mounts,
            "resources": resources(
                f"{int(config['infrastructure']['openshift']['load_pod']['cpu_millicores']) // 4}m",
                f"{int(config['infrastructure']['openshift']['load_pod']['memory_mib']) // 4}Mi",
            ),
        }
    ]
    for index in range(3):
        containers.append(
            {
                "name": f"worker-{index + 1}",
                "image": config["images"]["locust"],
                "args": [
                    "-f",
                    "/mnt/locust-cf/locustfile_mcp.py",
                    "--worker",
                    "--master-host",
                    "127.0.0.1",
                ],
                "env": [
                    *common_env,
                    {"name": "MCP_REPLICA_OFFSET", "value": str(index)},
                ],
                "volumeMounts": common_mounts,
                "resources": resources(
                    f"{int(config['infrastructure']['openshift']['load_pod']['cpu_millicores']) // 4}m",
                    f"{int(config['infrastructure']['openshift']['load_pod']['memory_mib']) // 4}Mi",
                ),
            }
        )
    return {
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": metadata(name, namespace),
        "spec": {
            "serviceAccountName": "benchmark",
            "nodeName": node,
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 5,
            "volumes": [
                {"name": "code", "configMap": {"name": "benchmark-code"}},
                {"name": "reports", "emptyDir": {}},
            ],
            "containers": containers,
        },
    }


def terminated_status(pod: dict, container: str) -> tuple[bool, int]:
    for status in pod.get("status", {}).get("containerStatuses", []):
        if status.get("name") != container:
            continue
        terminated = status.get("state", {}).get("terminated")
        if terminated:
            return True, int(terminated.get("exitCode", 1))
    return False, 0


def telemetry_snapshot(oc: Oc, namespace: str) -> dict:
    timestamp = time.time()
    pods = oc.run(
        "adm", "top", "pods", "-n", namespace, "--containers", "--no-headers", check=False
    )
    nodes = oc.run("adm", "top", "nodes", "--no-headers", check=False)
    return {
        "time": timestamp,
        "pods": pods.stdout,
        "pods_error": pods.stderr if pods.returncode else "",
        "nodes": nodes.stdout,
        "nodes_error": nodes.stderr if nodes.returncode else "",
    }


def parse_memory_mib(value: str) -> float:
    units = {"Ki": 1 / 1024, "Mi": 1.0, "Gi": 1024.0}
    for suffix, multiplier in units.items():
        if value.endswith(suffix):
            return float(value[: -len(suffix)]) * multiplier
    return float(value) / 1024 / 1024


def parse_cpu_millicores(value: str) -> float:
    units = {"n": 1 / 1_000_000, "u": 1 / 1_000, "m": 1.0}
    for suffix, multiplier in units.items():
        if value.endswith(suffix):
            return float(value[: -len(suffix)]) * multiplier
    return float(value) * 1000


def measurement_samples(
    samples: list[dict], start_time: float | None, end_time: float | None
) -> list[dict]:
    selected = []
    for sample in samples:
        timestamp = float(sample.get("time", 0))
        if start_time is not None and timestamp < start_time:
            continue
        if end_time is not None and timestamp > end_time:
            continue
        selected.append(sample)
    return selected


def lane_memory(
    samples: list[dict],
    lane: str,
    users: int | None = None,
    start_time: float | None = None,
    end_time: float | None = None,
) -> dict[str, float]:
    pod_names = (
        (
            instance_name("target", "builtin", users),
            with_users("builtin-db", users),
        )
        if lane == "builtin"
        else (instance_name("target", "external", users),)
    )
    totals = []
    for sample in measurement_samples(samples, start_time, end_time):
        total = 0.0
        seen = False
        for line in sample.get("pods", "").splitlines():
            fields = line.split()
            if len(fields) < 4 or fields[0] not in pod_names:
                continue
            try:
                total += parse_memory_mib(fields[3])
                seen = True
            except ValueError:
                continue
        if seen:
            totals.append(total)
    return {
        "average_mib": sum(totals) / len(totals) if totals else 0.0,
        "peak_mib": max(totals, default=0.0),
        "samples": len(totals),
    }


def helper_pressure(
    samples: list[dict],
    lane: str,
    config: dict,
    users: int,
    start_time: float,
    end_time: float,
) -> dict:
    load_size = config["infrastructure"]["openshift"]["load_pod"]
    backend_size = config["infrastructure"]["openshift"]["backend_pod"]
    load_cpu = float(load_size["cpu_millicores"])
    load_memory = float(load_size["memory_mib"])
    backend_cpu = float(backend_size["cpu_millicores"])
    backend_memory = float(backend_size["memory_mib"])
    snapshots = measurement_samples(samples, start_time, end_time)
    totals: list[tuple[float, float, float, float]] = []
    worker_cpu: dict[str, list[float]] = {}
    for sample in snapshots:
        locust_cpu = locust_memory = fast_cpu = fast_memory = 0.0
        seen_locust = seen_fast = False
        for line in sample.get("pods", "").splitlines():
            fields = line.split()
            if len(fields) < 4:
                continue
            pod, container, cpu, memory = fields[:4]
            try:
                cpu_milli = parse_cpu_millicores(cpu)
                memory_mib = parse_memory_mib(memory)
            except ValueError:
                continue
            if pod == instance_name("load", lane, users):
                locust_cpu += cpu_milli
                locust_memory += memory_mib
                seen_locust = True
                if container.startswith("worker-"):
                    worker_cpu.setdefault(container, []).append(
                        cpu_milli / (load_cpu / 4) * 100
                    )
            elif pod == instance_name("fast-time", lane, users):
                fast_cpu += cpu_milli
                fast_memory += memory_mib
                seen_fast = True
        if seen_locust and seen_fast:
            totals.append(
                (
                    locust_cpu / load_cpu * 100,
                    locust_memory / load_memory * 100,
                    fast_cpu / backend_cpu * 100,
                    fast_memory / backend_memory * 100,
                )
            )
    average = lambda index: (
        sum(values[index] for values in totals) / len(totals) if totals else 0.0
    )
    worker_max = max(
        (sum(values) / len(values) for values in worker_cpu.values()), default=0.0
    )
    thresholds = config["workload"]
    saturated = []
    if len(totals) < 3:
        saturated.append("helper metrics unavailable")
    if average(0) > float(thresholds["helper_cpu_percent"]):
        saturated.append("Locust CPU")
    if average(1) > float(thresholds["helper_memory_percent"]):
        saturated.append("Locust memory")
    if average(2) > float(thresholds["helper_cpu_percent"]):
        saturated.append("Fast Time CPU")
    if average(3) > float(thresholds["helper_memory_percent"]):
        saturated.append("Fast Time memory")
    if worker_max > float(thresholds["worker_core_percent"]):
        saturated.append("Locust worker CPU")
    return {
        "samples": len(totals),
        "locust_cpu_average_percent": average(0),
        "locust_memory_average_percent": average(1),
        "fast_time_cpu_average_percent": average(2),
        "fast_time_memory_average_percent": average(3),
        "worker_cpu_max_average_percent": worker_max,
        "saturated": saturated,
    }


def collect_reports(
    oc: Oc, namespace: str, pod: str, destination: Path
) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    result = oc.run(
        "cp",
        f"{namespace}/{pod}:/reports/.",
        oc.container_path(destination),
        "-c",
        "master",
        check=False,
        timeout=300,
    )
    if result.returncode:
        raise RuntimeError(f"failed to collect {pod} reports: {result.stderr}")


def run_parallel_step(
    oc: Oc,
    config: dict,
    namespace: str,
    nodes: dict[str, list[str]],
    urls: dict[str, str],
    tools: dict[str, list[str]],
    output: Path,
    users: int,
    stop_event: threading.Event | None = None,
) -> dict[str, dict]:
    pods = {lane: f"load-{lane}-{users}" for lane in LANES}
    for lane in LANES:
        oc.delete("pod", pods[lane], "-n", namespace)
        oc.apply(
            locust_pod(
                config,
                namespace,
                lane,
                assigned_node(nodes, "locust", lane, users),
                users,
                urls[lane],
                tools[lane],
            )
        )
    completed: dict[str, int] = {}
    samples: list[dict] = []
    deadline = time.monotonic() + (
        int(config["workload"]["ramp_seconds"])
        + int(config["workload"]["warmup_seconds"])
        + int(config["workload"]["measure_seconds"])
        + 300
    )
    last_sample = 0.0
    while len(completed) < len(LANES):
        if stop_event is not None and stop_event.is_set():
            break
        if time.monotonic() > deadline:
            raise RuntimeError(f"parallel {users}-user step exceeded its time bound")
        for lane, pod_name in pods.items():
            if lane in completed:
                continue
            pod = oc.json("get", "pod", pod_name, "-n", namespace)
            for container in ["master", "worker-1", "worker-2", "worker-3"]:
                done, exit_code = terminated_status(pod, container)
                if done and exit_code and container != "master":
                    completed[lane] = exit_code
                    break
            done, exit_code = terminated_status(pod, "master")
            if done:
                completed[lane] = exit_code
        now = time.monotonic()
        if now - last_sample >= 10:
            samples.append(telemetry_snapshot(oc, namespace))
            last_sample = now
        if any(code for code in completed.values()):
            if stop_event is not None:
                stop_event.set()
            break
        time.sleep(2)

    results: dict[str, dict] = {}
    for lane, pod_name in pods.items():
        lane_output = output / f"{lane}-{users}"
        try:
            collect_reports(oc, namespace, pod_name, lane_output)
            marker = lane_output / "measurement-start.txt"
            if not marker.is_file():
                raise RuntimeError("Locust did not record the measurement-window start")
            measurement_start = float(marker.read_text(encoding="utf-8").strip())
            measurement_end = measurement_start + int(
                config["workload"]["measure_seconds"]
            )
            stats = read_stats(lane_output / "locust_stats.csv", use_aggregate=True)
            exit_code = completed.get(lane, 1)
            pressure = helper_pressure(
                samples, lane, config, users, measurement_start, measurement_end
            )
            stats.update(
                {
                    "lane": "rust" if lane == "external" else "builtin",
                    "users": users,
                    "passed": exit_code == 0
                    and stats["failures"] == 0
                    and not pressure["saturated"],
                    "memory": lane_memory(
                        samples, lane, users, measurement_start, measurement_end
                    ),
                    "pressure": pressure,
                }
            )
            if exit_code:
                stats["reason"] = f"Locust exited {exit_code}"
            elif pressure["saturated"]:
                stats["reason"] = "helper pressure: " + ", ".join(
                    pressure["saturated"]
                )
                if stop_event is not None:
                    stop_event.set()
            results[lane] = stats
        except Exception as error:
            results[lane] = {
                "lane": "rust" if lane == "external" else "builtin",
                "users": users,
                "passed": False,
                "reason": str(error),
                "memory": lane_memory(samples, lane, users),
                "pressure": {},
            }
        finally:
            oc.delete("pod", pod_name, "-n", namespace)
    telemetry = output / f"telemetry-{users}.jsonl"
    telemetry.write_text(
        "".join(json.dumps(sample, sort_keys=True) + "\n" for sample in samples),
        encoding="utf-8",
    )
    return results


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    parser.add_argument("--kubeconfig", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--assets", required=True)
    args = parser.parse_args()
    config = json.loads(Path(args.config).read_text(encoding="utf-8"))
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    openshift = config["infrastructure"]["openshift"]
    oc = Oc(openshift["oc_image"], Path(args.kubeconfig))
    namespace = f"cf-fyre-{args.run_id}"
    result = {
        "status": "running",
        "scenario": config["scenarios"][0],
        "protocol_version": config["workload"]["protocol_version"],
        "user_levels": config["workload"]["user_levels"],
        "parallel_lanes": True,
        "runs": {"rust": [], "builtin": []},
    }
    result_path = output / "result.json"

    def save() -> None:
        result_path.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

    save()
    try:
        setup_namespace(oc, namespace, Path(args.assets))
        nodes = assign_nodes(config, oc.json("get", "nodes"))
        all_parallel = bool(config["workload"].get("parallel_user_levels"))
        result["inventory"] = {
            "namespace": namespace,
            "nodes": nodes,
            "architecture": (
                "eight reserved 2v2 targets running concurrently on three dedicated-role workers"
                if all_parallel
                else "two isolated lanes running concurrently on six workers"
            ),
        }
        (output / "inventory.json").write_text(
            json.dumps(result["inventory"], indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        levels = [int(users) for users in config["workload"]["user_levels"]]
        instances = (
            [(lane, users) for users in levels for lane in LANES]
            if all_parallel
            else [(lane, None) for lane in LANES]
        )
        with concurrent.futures.ThreadPoolExecutor(
            max_workers=len(instances)
        ) as executor:
            futures = [
                executor.submit(
                    deploy_fast_time,
                    oc,
                    config,
                    namespace,
                    lane,
                    assigned_node(nodes, "fast-time", lane, users),
                    users,
                )
                for lane, users in instances
            ]
            for future in futures:
                future.result()
        credentials: dict[tuple[str, int | None], tuple[str, list[str]]] = {}
        with concurrent.futures.ThreadPoolExecutor(
            max_workers=len(instances)
        ) as executor:
            futures = {}
            for lane, users in instances:
                function = deploy_builtin if lane == "builtin" else deploy_external
                future = executor.submit(
                    function,
                    oc,
                    config,
                    namespace,
                    assigned_node(nodes, "target", lane, users),
                    users,
                )
                futures[future] = (lane, users)
            for future, key in futures.items():
                credentials[key] = future.result()
        urls: dict[tuple[str, int], str] = {}
        for users in levels:
            for lane in LANES:
                credential_key = (lane, users if all_parallel else None)
                token, tool_names = credentials[credential_key]
                if len(tool_names) != 6:
                    raise RuntimeError(
                        f"{lane} at {users} users did not expose all six benchmark tools"
                    )
                store_lane_secret(
                    oc, namespace, lane, token, tool_names, users
                )
                target = instance_name(
                    "target", lane, users if all_parallel else None
                )
                urls[(lane, users)] = (
                    f"http://{target}:4444/mcp"
                    if lane == "builtin"
                    else f"http://{target}:4445/contextforge-rs/servers/{SERVER_ID}/mcp"
                )
        smoke_instances = (
            [(lane, users) for users in levels for lane in LANES]
            if all_parallel
            else [(lane, levels[0]) for lane in LANES]
        )
        with concurrent.futures.ThreadPoolExecutor(
            max_workers=len(smoke_instances)
        ) as executor:
            futures = [
                executor.submit(
                    smoke_lane,
                    oc,
                    config,
                    namespace,
                    lane,
                    assigned_node(nodes, "locust", lane, users),
                    urls[(lane, users)],
                    credentials[(lane, users if all_parallel else None)][1],
                    users,
                )
                for lane, users in smoke_instances
            ]
            for future in futures:
                future.result()
        stop_event = threading.Event()
        step_results: dict[int, dict[str, dict]] = {}
        workers = len(levels) if all_parallel else 1
        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as executor:
            futures = {}
            for users in levels:
                future = executor.submit(
                    run_parallel_step,
                    oc,
                    config,
                    namespace,
                    nodes,
                    {lane: urls[(lane, users)] for lane in LANES},
                    {
                        lane: credentials[
                            (lane, users if all_parallel else None)
                        ][1]
                        for lane in LANES
                    },
                    output,
                    users,
                    stop_event if all_parallel else None,
                )
                futures[future] = users
                if not all_parallel:
                    step = future.result()
                    step_results[users] = step
                    if not all(item.get("passed") for item in step.values()):
                        break
            if all_parallel:
                for future, users in futures.items():
                    step_results[users] = future.result()
        for users in sorted(step_results):
            step = step_results[users]
            result["runs"]["builtin"].append(step["builtin"])
            result["runs"]["rust"].append(step["external"])
        save()
        if len(step_results) != len(levels) or not all(
            item.get("passed")
            for step in step_results.values()
            for item in step.values()
        ):
            result["status"] = "failed"
            result["reason"] = "first request, worker, or helper-pressure error"
            save()
            raise SystemExit(1)
        result["status"] = "confirmed"
        save()
    except BaseException as error:
        if result["status"] == "running":
            result["status"] = "failed"
            result["reason"] = str(error)
            save()
        raise
    finally:
        oc.delete("namespace", namespace, "--wait=false")


if __name__ == "__main__":
    main()
