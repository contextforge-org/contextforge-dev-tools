"""Bootstrap FYRE hosts and find one scenario's zero-error capacity."""

from __future__ import annotations

import argparse
import csv
import json
import shlex
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

HELPER_SATURATED = 42
ANSIBLE_CORE_VERSION = "2.21.4"


def run(
    arguments: list[str],
    *,
    check: bool = True,
    capture: bool = False,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        arguments,
        check=check,
        text=True,
        timeout=timeout,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
    )


class Remote:
    def __init__(self, user: str, key: Path, known_hosts: Path):
        self.user = user
        self.options = [
            "-i",
            str(key),
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            f"UserKnownHostsFile={known_hosts}",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "ConnectTimeout=10",
        ]

    def ssh(
        self,
        host: str,
        command: str,
        *,
        check: bool = True,
        capture: bool = False,
        timeout: float | None = None,
    ):
        return run(
            ["ssh", *self.options, f"{self.user}@{host}", command],
            check=check,
            capture=capture,
            timeout=timeout,
        )

    def copy_to(self, host: str, source: Path, destination: str) -> None:
        destination = destination.removeprefix("~/")
        run(["scp", *self.options, str(source), f"{self.user}@{host}:{destination}"])

    def copy_from(
        self,
        host: str,
        source: str,
        destination: Path,
        *,
        recursive: bool = False,
        check: bool = True,
    ) -> None:
        if recursive:
            destination.mkdir(parents=True, exist_ok=True)
        else:
            destination.parent.mkdir(parents=True, exist_ok=True)
        source = source.removeprefix("~/")
        arguments = ["scp", *self.options]
        if recursive:
            arguments.append("-r")
        run([*arguments, f"{self.user}@{host}:{source}", str(destination)], check=check)


def bootstrap_hosts(
    config: dict,
    inventory: dict,
    deploy: Path,
    playbook: Path,
    known_hosts: Path,
    output: Path,
) -> None:
    hosts = [inventory["locust"], inventory["fast_time"], *inventory["dataplanes"]]
    ansible_inventory = {
        "all": {
            "hosts": {
                host["name"]: {
                    "ansible_host": host["public_ip"],
                    "ansible_user": config["infrastructure"]["ssh_user"],
                    "ansible_python_interpreter": "/usr/bin/python3",
                }
                for host in hosts
            },
            "vars": {
                "ansible_ssh_private_key_file": config["resolved_ssh_private_key"],
                "ansible_ssh_common_args": " ".join(
                    [
                        "-o BatchMode=yes",
                        "-o IdentitiesOnly=yes",
                        f"-o UserKnownHostsFile={known_hosts}",
                        "-o StrictHostKeyChecking=accept-new",
                        "-o ConnectTimeout=10",
                    ]
                ),
            },
        }
    }
    inventory_path = output / "ansible-inventory.json"
    inventory_path.write_text(
        json.dumps(ansible_inventory, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    run(
        [
            "uv",
            "tool",
            "run",
            "--from",
            f"ansible-core=={ANSIBLE_CORE_VERSION}",
            "ansible-playbook",
            "--inventory",
            str(inventory_path),
            str(playbook),
            "--extra-vars",
            json.dumps({"fyre_deploy_dir": str(deploy.resolve())}),
        ],
        timeout=1_200,
    )


def write_remote_file(
    remote: Remote, host: str, contents: str, destination: str, mode: int = 0o600
) -> None:
    destination = destination.removeprefix("~/")
    with tempfile.NamedTemporaryFile("w", delete=False, encoding="utf-8") as stream:
        stream.write(contents)
        temporary = Path(stream.name)
    try:
        temporary.chmod(mode)
        remote.copy_to(host, temporary, destination)
        remote.ssh(host, f"chmod {mode:o} {shlex.quote(destination)}")
    finally:
        temporary.unlink(missing_ok=True)


def compose_up(remote: Remote, host: str, compose: str) -> None:
    remote.ssh(
        host,
        f"cd ~/cf-fyre && docker compose --env-file benchmark.env -f {shlex.quote(compose)} pull && docker compose --env-file benchmark.env -f {shlex.quote(compose)} up -d --wait",
        timeout=900,
    )


def prepare_hosts(
    config: dict,
    inventory: dict,
    remote: Remote,
    deploy: Path,
    playbook: Path,
    known_hosts: Path,
    output: Path,
) -> tuple[str, list[str]]:
    bootstrap_hosts(config, inventory, deploy, playbook, known_hosts, output)

    images = config["images"]
    fast_env = f"FAST_TIME_IMAGE={images['fast_time']}\n"
    write_remote_file(
        remote, inventory["fast_time"]["public_ip"], fast_env, "~/cf-fyre/benchmark.env"
    )
    compose_up(remote, inventory["fast_time"]["public_ip"], "fast-time.compose.yaml")

    first = inventory["dataplanes"][0]
    for index, target in enumerate(inventory["dataplanes"]):
        allowed = ",".join(
            [
                f"{target['private_ip']}:4445",
                f"{target['public_ip']}:4445",
                "127.0.0.1:4445",
                "localhost:4445",
            ]
        )
        target_env = "\n".join(
            [
                f"DATAPLANE_IMAGE={images['dataplane']}",
                f"HELPERS_IMAGE={images['helpers']}",
                f"REDIS_IMAGE={images['redis']}",
                f"DATAPLANE_ALLOWED_HOSTS={allowed}",
                f"CONFIG_CACHE_SECONDS={config['workload']['config_cache_seconds']}",
                "",
            ]
        )
        write_remote_file(
            remote, target["public_ip"], target_env, "~/cf-fyre/benchmark.env"
        )
        if index > 0:
            with tempfile.TemporaryDirectory() as temporary:
                key = Path(temporary) / "jwt.key"
                remote.copy_from(
                    first["public_ip"], "~/cf-fyre/state/keys/jwt.key", key
                )
                remote.copy_to(target["public_ip"], key, "~/cf-fyre/state/keys/jwt.key")
                remote.ssh(
                    target["public_ip"], "chmod 600 ~/cf-fyre/state/keys/jwt.key"
                )
        compose_up(remote, target["public_ip"], "dataplane.compose.yaml")
        if index == 0:
            # The first auth container creates the campaign key; subsequent replicas receive it.
            remote.ssh(
                first["public_ip"],
                "test -s ~/cf-fyre/state/keys/jwt.key && sudo chown $USER:$(id -gn) ~/cf-fyre/state/keys/jwt.key && chmod 600 ~/cf-fyre/state/keys/jwt.key",
            )

    token = remote.ssh(
        first["public_ip"],
        "cd ~/cf-fyre && docker compose --env-file benchmark.env -f dataplane.compose.yaml run --rm --no-deps config_writer token fyre-benchmark fyre-user",
        capture=True,
    ).stdout.strip()
    if not token or "\n" in token:
        raise RuntimeError("config helper did not return one bearer token")
    token_file = output / ".token"
    token_file.write_text(token, encoding="utf-8")
    token_file.chmod(0o600)
    try:
        for target in inventory["dataplanes"]:
            remote.copy_to(target["public_ip"], token_file, "~/cf-fyre/state/token")
            remote.ssh(target["public_ip"], "chmod 600 ~/cf-fyre/state/token")
            remote.ssh(
                target["public_ip"],
                'cd ~/cf-fyre && export MCP_CONFORMANCE_TOKEN="$(cat state/token)" && docker compose --env-file benchmark.env -f dataplane.compose.yaml run --rm --no-deps -e MCP_CONFORMANCE_TOKEN config_writer fixture fyre-fast-time http://'
                + inventory["fast_time"]["private_ip"]
                + ":9080/mcp 2026-07-28",
                timeout=120,
            )
        locust_env = "\n".join(
            [
                f"MCPGATEWAY_BEARER_TOKEN={token}",
                "MCP_PROTOCOL_VERSION=2026-07-28",
                "MCP_STACK_MODE=dataplane",
                "MCP_SERVER_ID=fyre-fast-time",
                "MCP_DIRECT_DATAPLANE=true",
                "MCP_SKIP_TOOL_LIST=true",
                "MCP_EXPLICIT_ZERO_DELAY=true",
                "MCP_FYRE_WORKLOAD=true",
                "MCP_TOOL_NAMES=convert_time,echo,get_stats,get_system_time,schema_success,verify-protocol",
                "LOCUST_REQUEST_TIMEOUT_SECONDS=30",
                "MCP_BASE_URLS="
                + ",".join(
                    f"http://{target['private_ip']}:4445"
                    for target in inventory["dataplanes"]
                ),
                "",
            ]
        )
        write_remote_file(
            remote,
            inventory["locust"]["public_ip"],
            locust_env,
            "~/cf-fyre/benchmark.secret.env",
        )
        direct_env = "\n".join(
            [
                f"MCPGATEWAY_BEARER_TOKEN={token}",
                "MCP_PROTOCOL_VERSION=2026-07-28",
                "MCP_STACK_MODE=controlplane",
                "MCP_FYRE_WORKLOAD=true",
                "MCP_SKIP_TOOL_LIST=true",
                "MCP_EXPLICIT_ZERO_DELAY=true",
                "MCP_TOOL_NAMES=convert_time,echo,get_stats,get_system_time,schema_success,verify-protocol",
                "LOCUST_REQUEST_TIMEOUT_SECONDS=30",
                f"MCP_BASE_URLS=http://{inventory['fast_time']['private_ip']}:9080",
                "",
            ]
        )
        write_remote_file(
            remote,
            inventory["locust"]["public_ip"],
            direct_env,
            "~/cf-fyre/direct.secret.env",
        )
        remote.copy_to(
            inventory["locust"]["public_ip"], token_file, "~/cf-fyre/state/token"
        )
        remote.ssh(inventory["locust"]["public_ip"], "chmod 600 ~/cf-fyre/state/token")
    finally:
        token_file.unlink(missing_ok=True)
    urls = [
        f"http://{target['private_ip']}:4445/contextforge-rs/servers/fyre-fast-time/mcp"
        for target in inventory["dataplanes"]
    ]
    return token, urls


def start_monitor(remote: Remote, host: str, role: str, name: str) -> int:
    command = f"cd ~/cf-fyre && nohup python3 monitor.py --role {shlex.quote(role)} --output telemetry/{shlex.quote(name)}.jsonl >telemetry/{shlex.quote(name)}.log 2>&1 & echo $!"
    return int(remote.ssh(host, command, capture=True).stdout.strip())


def stop_monitor(remote: Remote, host: str, pid: int) -> None:
    remote.ssh(
        host,
        f"kill -TERM {pid} 2>/dev/null || true; wait {pid} 2>/dev/null || true",
        check=False,
    )


def smoke(remote: Remote, locust: dict, urls: list[str], locust_image: str) -> None:
    command = " ".join(
        [
            "cd ~/cf-fyre && docker run --rm --network host --entrypoint python",
            "-v $HOME/cf-fyre:/work -w /work",
            shlex.quote(locust_image),
            "python smoke.py --urls",
            shlex.quote(",".join(urls)),
            "--token-file state/token",
        ]
    )
    remote.ssh(locust["public_ip"], command, timeout=120)


def read_stats(path: Path, use_aggregate: bool = False) -> dict:
    with path.open(newline="", encoding="utf-8") as stream:
        rows = list(csv.DictReader(stream))
    tool_rows = [
        row for row in rows if row.get("Name", "").startswith("MCP tools/call")
    ]
    if not tool_rows:
        raise RuntimeError(f"Locust report {path} contains no measured tool traffic")
    aggregate = next((row for row in rows if row.get("Name") == "Aggregated"), None)
    selected = [aggregate] if use_aggregate and aggregate is not None else tool_rows
    failures = sum(int(float(row.get("Failure Count") or 0)) for row in selected)
    requests = sum(int(float(row.get("Request Count") or 0)) for row in selected)
    rps = sum(float(row.get("Requests/s") or 0) for row in selected)
    per_replica = {row["Name"]: float(row.get("Requests/s") or 0) for row in tool_rows}

    def weighted(column: str) -> float:
        return (
            sum(
                float(row.get(column) or 0) * int(float(row.get("Request Count") or 0))
                for row in selected
            )
            / requests
            if requests
            else 0.0
        )

    return {
        "requests": requests,
        "failures": failures,
        "rps": rps,
        "p50_ms": weighted("50%"),
        "p95_ms": weighted("95%"),
        "p99_ms": weighted("99%"),
        "per_replica_rps": per_replica,
    }


def kernel_counter(text: str, name: str) -> int:
    lines = text.splitlines()
    for header, values in zip(lines[0::2], lines[1::2]):
        header_fields = header.split()
        value_fields = values.split()
        if not header_fields or not value_fields or header_fields[0] != value_fields[0]:
            continue
        try:
            return int(value_fields[header_fields.index(name)])
        except (ValueError, IndexError):
            continue
    return 0


def docker_pressure(text: str) -> bool:
    for line in text.splitlines():
        try:
            state = json.loads(line)
        except ValueError:
            continue
        health = state.get("Health") or {}
        if (
            state.get("OOMKilled") is True
            or state.get("Status") == "dead"
            or (state.get("Status") == "exited" and state.get("ExitCode") != 0)
            or health.get("Status") == "unhealthy"
        ):
            return True
    return False


def pressure(path: Path, after: float | None = None) -> dict[str, float | bool]:
    samples = []
    for line in path.read_text(encoding="utf-8").splitlines():
        item = json.loads(line)
        if item.get("kind") == "sample" and (
            after is None or item.get("time", 0) >= after
        ):
            samples.append(item)
    busy = [
        item["cpu"]["cpu"]["busy_percent"]
        for item in samples
        if "cpu" in item.get("cpu", {})
    ]
    memory = [item["memory"]["used_percent"] for item in samples]
    core_names = {
        key for item in samples for key in item.get("cpu", {}) if key != "cpu"
    }
    core_means = [
        statistics.fmean(
            item["cpu"][key]["busy_percent"]
            for item in samples
            if key in item.get("cpu", {})
        )
        for key in core_names
    ]
    steal = [
        item["cpu"]["cpu"]["steal_percent"]
        for item in samples
        if "cpu" in item.get("cpu", {})
    ]
    network_counters = [
        sum(
            kernel_counter(item.get("netstat", ""), counter)
            for counter in ("ListenOverflows", "ListenDrops", "TCPBacklogDrop")
        )
        for item in samples
    ]
    docker_unhealthy = any(
        docker_pressure(item.get("docker_state", "")) for item in samples
    )
    return {
        "mean_cpu_percent": statistics.fmean(busy) if busy else 0.0,
        "max_memory_percent": max(memory, default=0.0),
        "max_mean_core_percent": max(core_means, default=0.0),
        "mean_steal_percent": statistics.fmean(steal) if steal else 0.0,
        "worker_or_network_pressure": docker_unhealthy
        or (len(network_counters) > 1 and network_counters[-1] > network_counters[0]),
    }


def one_phase(
    remote: Remote,
    config: dict,
    inventory: dict,
    urls: list[str],
    output: Path,
    users: int,
    seconds: int,
    label: str,
    env_file: str = "benchmark.secret.env",
    measurement: bool = False,
) -> dict:
    locust = inventory["locust"]
    workers = max(2, int(config["active_helper"]["locust_cpu"]) - 1)
    spawn_rate = max(1.0, users / config["workload"]["ramp_seconds"])
    total_seconds = seconds + config["workload"]["ramp_seconds"]
    remote_output = f"reports/{label}"
    monitors: list[tuple[str, int]] = []
    monitor_hosts = [
        (locust, "locust"),
        (inventory["fast_time"], "fast-time"),
        *[
            (target, f"dataplane-{index + 1}")
            for index, target in enumerate(inventory["dataplanes"])
        ],
    ]
    for host, role in monitor_hosts:
        monitors.append(
            (host["public_ip"], start_monitor(remote, host["public_ip"], role, label))
        )
    try:
        command = " ".join(
            [
                "cd ~/cf-fyre && python3 run_locust.py",
                "--image",
                shlex.quote(config["images"]["locust"]),
                "--users",
                str(users),
                "--spawn-rate",
                str(spawn_rate),
                "--seconds",
                str(total_seconds),
                "--workers",
                str(workers),
                "--output",
                shlex.quote(remote_output),
                "--env-file",
                shlex.quote(env_file),
            ]
        )
        if measurement:
            command += f" --reset-stats --measurement-seconds {seconds}"
        result = remote.ssh(
            locust["public_ip"], command, check=False, timeout=total_seconds + 180
        )
    finally:
        for host, pid in monitors:
            stop_monitor(remote, host, pid)
    local = output / label
    local.mkdir(parents=True, exist_ok=True)
    remote.copy_from(
        locust["public_ip"],
        f"~/cf-fyre/{remote_output}/.",
        local,
        recursive=True,
        check=False,
    )
    pressures = {}
    measurement_start = None
    marker = local / "measurement-start.txt"
    if measurement:
        if not marker.is_file():
            return {
                "passed": False,
                "reason": "Locust did not record the measurement-window start",
                "pressure": pressures,
            }
        measurement_start = float(marker.read_text(encoding="utf-8").strip())
    for host, role in monitor_hosts:
        path = local / f"{role}.jsonl"
        remote.copy_from(
            host["public_ip"], f"~/cf-fyre/telemetry/{label}.jsonl", path, check=False
        )
        if path.exists():
            pressures[role] = pressure(path, after=measurement_start)
    if result.returncode != 0:
        return {
            "passed": False,
            "reason": f"Locust exited {result.returncode}",
            "pressure": pressures,
        }
    stats = read_stats(local / "locust_stats.csv", use_aggregate=measurement)
    stats.update(
        {"passed": stats["failures"] == 0, "pressure": pressures, "users": users}
    )
    return stats


def helper_saturation(config: dict, result: dict) -> str | None:
    workload = config["workload"]
    for role in ("locust", "fast-time"):
        values = result.get("pressure", {}).get(role, {})
        if values.get("mean_cpu_percent", 0) > workload["helper_cpu_percent"]:
            return role
        if values.get("max_memory_percent", 0) > workload["helper_memory_percent"]:
            return role
        if values.get("max_mean_core_percent", 0) > workload["worker_core_percent"]:
            return role
        if values.get("worker_or_network_pressure", False):
            return role
    return None


def measured_step(
    remote: Remote,
    config: dict,
    inventory: dict,
    urls: list[str],
    output: Path,
    users: int,
    name: str,
) -> dict:
    smoke(remote, inventory["locust"], urls, config["images"]["locust"])
    warmup = one_phase(
        remote,
        config,
        inventory,
        urls,
        output,
        users,
        config["workload"]["warmup_seconds"],
        f"{name}-warmup",
    )
    if not warmup.get("passed"):
        return warmup
    result = one_phase(
        remote,
        config,
        inventory,
        urls,
        output,
        users,
        config["workload"]["measure_seconds"],
        name,
        measurement=True,
    )
    saturated = helper_saturation(config, result)
    if saturated:
        (output / "helper-request.json").write_text(
            json.dumps({"role": saturated}, indent=2) + "\n", encoding="utf-8"
        )
        raise SystemExit(HELPER_SATURATED)
    return result


def capacity_search(
    remote: Remote, config: dict, inventory: dict, urls: list[str], output: Path
) -> dict:
    workload = config["workload"]
    started = time.monotonic()
    passing: list[dict] = []
    failing: dict | None = None
    improvements: list[float] = []
    users = workload["first_users"]
    step = 0
    while (
        users <= workload["maximum_users"]
        and time.monotonic() - started < workload["maximum_campaign_seconds"]
    ):
        step += 1
        result = measured_step(
            remote, config, inventory, urls, output, users, f"search-{step}-{users}"
        )
        if not result.get("passed"):
            failing = {"users": users, **result}
            break
        if passing:
            improvements.append(100.0 * (result["rps"] / passing[-1]["rps"] - 1.0))
        passing.append(result)
        if len(improvements) >= 2 and all(
            value < workload["plateau_improvement_percent"]
            for value in improvements[-2:]
        ):
            break
        if users == workload["maximum_users"]:
            break
        users = min(workload["maximum_users"], users * 2)

    if not passing:
        return {
            "status": "failed",
            "reason": "no zero-error concurrency passed",
            "failing": failing,
        }
    if failing:
        low = passing[-1]["users"]
        high = failing["users"]
        while (high - low) / high > workload["boundary_percent"] / 100.0:
            users = (low + high) // 2
            result = measured_step(
                remote, config, inventory, urls, output, users, f"refine-{users}"
            )
            if result.get("passed"):
                passing.append(result)
                low = users
            else:
                failing = {"users": users, **result}
                high = users

    candidate = max(passing, key=lambda item: item["users"])
    confirmations = []
    for repetition in range(workload["repetitions"]):
        result = measured_step(
            remote,
            config,
            inventory,
            urls,
            output,
            candidate["users"],
            f"confirm-{repetition + 1}-{candidate['users']}",
        )
        if not result.get("passed"):
            return {
                "status": "failed-confirmation",
                "candidate": candidate,
                "confirmations": confirmations,
                "failure": result,
            }
        confirmations.append(result)
    direct_url = f"http://{inventory['fast_time']['private_ip']}:9080/mcp"
    smoke(remote, inventory["locust"], [direct_url], config["images"]["locust"])
    direct_warmup = one_phase(
        remote,
        config,
        inventory,
        [direct_url],
        output,
        candidate["users"],
        workload["warmup_seconds"],
        "calibration-warmup",
        "direct.secret.env",
    )
    if not direct_warmup.get("passed"):
        return {
            "status": "inconclusive",
            "reason": "direct Fast Time calibration warmup failed",
            "calibration": direct_warmup,
        }
    calibration = one_phase(
        remote,
        config,
        inventory,
        [direct_url],
        output,
        candidate["users"],
        workload["measure_seconds"],
        "calibration",
        "direct.secret.env",
        measurement=True,
    )
    saturated = helper_saturation(config, calibration)
    if saturated:
        (output / "helper-request.json").write_text(
            json.dumps({"role": saturated}, indent=2) + "\n", encoding="utf-8"
        )
        raise SystemExit(HELPER_SATURATED)
    if (
        not calibration.get("passed")
        or calibration.get("rps", 0) < min(item["rps"] for item in confirmations) * 1.05
    ):
        return {
            "status": "inconclusive",
            "reason": "direct Fast Time calibration did not demonstrate five percent upstream headroom",
            "calibration": calibration,
        }
    rps_values = [result["rps"] for result in confirmations]
    imbalances = []
    for result in confirmations:
        replicas = list(result["per_replica_rps"].values())
        mean = statistics.fmean(replicas) if replicas else 0.0
        imbalances.append(
            100.0 * (max(replicas) - min(replicas)) / mean
            if mean and len(replicas) > 1
            else 0.0
        )
    best = min(confirmations, key=lambda item: item["rps"])
    return {
        "status": "confirmed",
        "users": candidate["users"],
        "search": passing,
        "failing": failing,
        "confirmations": confirmations,
        "rps": statistics.fmean(rps_values),
        "rps_min": min(rps_values),
        "rps_max": max(rps_values),
        "rps_cv_percent": 100.0
        * statistics.pstdev(rps_values)
        / statistics.fmean(rps_values)
        if len(rps_values) > 1
        else 0.0,
        "replica_imbalance_percent": statistics.fmean(imbalances),
        "p50_ms": statistics.fmean(item["p50_ms"] for item in confirmations),
        "p95_ms": statistics.fmean(item["p95_ms"] for item in confirmations),
        "p99_ms": statistics.fmean(item["p99_ms"] for item in confirmations),
        "lower_bound": candidate["users"] == workload["maximum_users"]
        and failing is None,
        "conservative_confirmation": best,
        "direct_backend_calibration": calibration,
    }


def collect_recovery(remote: Remote, inventory: dict, output: Path) -> None:
    recovery = output / "recovery"
    recovery.mkdir(parents=True, exist_ok=True)
    for host in [inventory["locust"], inventory["fast_time"], *inventory["dataplanes"]]:
        remote.ssh(
            host["public_ip"],
            "pkill -TERM -f 'python3 monitor.py' 2>/dev/null || true; docker ps --filter name=cf-fyre --format '{{.ID}}' | xargs -r docker rm -f >/dev/null 2>&1 || true",
            check=False,
        )
        destination = recovery / host["name"]
        destination.mkdir(exist_ok=True)
        remote.copy_from(
            host["public_ip"],
            "cf-fyre/reports/.",
            destination / "reports",
            recursive=True,
            check=False,
        )
        remote.copy_from(
            host["public_ip"],
            "cf-fyre/telemetry/.",
            destination / "telemetry",
            recursive=True,
            check=False,
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    parser.add_argument("--inventory", required=True)
    parser.add_argument("--scenario", required=True)
    parser.add_argument("--deploy", required=True)
    parser.add_argument("--ansible", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--collect-only", action="store_true")
    args = parser.parse_args()
    config = json.loads(Path(args.config).read_text(encoding="utf-8"))
    inventory = json.loads(Path(args.inventory).read_text(encoding="utf-8"))
    scenario = next(item for item in config["scenarios"] if item["id"] == args.scenario)
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    known_hosts = output.parent.parent / "known_hosts"
    known_hosts.touch(exist_ok=True)
    remote = Remote(
        config["infrastructure"]["ssh_user"],
        Path(config["resolved_ssh_private_key"]),
        known_hosts,
    )
    if args.collect_only:
        collect_recovery(remote, inventory, output)
        return
    _, urls = prepare_hosts(
        config,
        inventory,
        remote,
        Path(args.deploy),
        Path(args.ansible),
        known_hosts,
        output,
    )
    result = capacity_search(remote, config, inventory, urls, output)
    result["scenario"] = scenario
    result["inventory"] = inventory
    (output / "result.json").write_text(
        json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    if result["status"] != "confirmed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
