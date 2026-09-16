"""Sample Linux host pressure as JSON lines without third-party packages."""

from __future__ import annotations

import argparse
import json
import os
import platform
import socket
import subprocess
import time
from pathlib import Path


def read(path: str) -> str:
    try:
        return Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError as error:
        return f"error={error}"


def cpu_times() -> dict[str, list[int]]:
    rows: dict[str, list[int]] = {}
    for line in read("/proc/stat").splitlines():
        fields = line.split()
        if fields and fields[0].startswith("cpu"):
            rows[fields[0]] = [int(value) for value in fields[1:]]
    return rows


def cpu_percent(
    previous: dict[str, list[int]], current: dict[str, list[int]]
) -> dict[str, dict[str, float]]:
    result = {}
    for name, values in current.items():
        before = previous.get(name, values)
        deltas = [max(0, after - old) for old, after in zip(before, values)]
        total = sum(deltas) or 1
        idle = sum(deltas[index] for index in (3, 4) if index < len(deltas))
        steal = deltas[7] if len(deltas) > 7 else 0
        result[name] = {
            "busy_percent": round(100.0 * (total - idle) / total, 3),
            "steal_percent": round(100.0 * steal / total, 3),
        }
    return result


def memory() -> dict[str, int | float]:
    values = {}
    for line in read("/proc/meminfo").splitlines():
        key, _, rest = line.partition(":")
        try:
            values[key] = int(rest.split()[0])
        except (IndexError, ValueError):
            continue
    total = values.get("MemTotal", 0)
    available = values.get("MemAvailable", 0)
    return {
        "total_kib": total,
        "available_kib": available,
        "used_percent": round(100.0 * (total - available) / total, 3) if total else 0.0,
        "swap_total_kib": values.get("SwapTotal", 0),
        "swap_free_kib": values.get("SwapFree", 0),
    }


def command_output(command: list[str]) -> str:
    try:
        return subprocess.run(
            command, check=False, text=True, capture_output=True, timeout=3
        ).stdout.strip()
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"error={error}"


def docker_state() -> str:
    container_ids = [
        container_id
        for container_id in command_output(
            ["docker", "ps", "--all", "--quiet"]
        ).splitlines()
        if container_id and not container_id.startswith("error=")
    ]
    if not container_ids:
        return "[]"
    return command_output(
        [
            "docker",
            "inspect",
            "--format",
            "{{json .State}}",
            *container_ids,
        ]
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True)
    parser.add_argument("--role", required=True)
    parser.add_argument("--interval", type=float, default=1.0)
    args = parser.parse_args()
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    previous = cpu_times()
    with output.open("a", encoding="utf-8", buffering=1) as stream:
        stream.write(
            json.dumps(
                {
                    "kind": "host",
                    "role": args.role,
                    "hostname": socket.gethostname(),
                    "cpu_model": next(
                        (
                            line.partition(":")[2].strip()
                            for line in read("/proc/cpuinfo").splitlines()
                            if line.startswith("model name")
                        ),
                        platform.processor(),
                    ),
                    "logical_cpus": os.cpu_count(),
                    "kernel": platform.release(),
                },
                sort_keys=True,
            )
            + "\n"
        )
        while True:
            time.sleep(args.interval)
            current = cpu_times()
            stream.write(
                json.dumps(
                    {
                        "kind": "sample",
                        "time": time.time(),
                        "cpu": cpu_percent(previous, current),
                        "memory": memory(),
                        "loadavg": read("/proc/loadavg").strip(),
                        "pressure_cpu": read("/proc/pressure/cpu").strip(),
                        "pressure_memory": read("/proc/pressure/memory").strip(),
                        "vmstat": read("/proc/vmstat").strip(),
                        "sockstat": read("/proc/net/sockstat").strip(),
                        "netstat": read("/proc/net/netstat").strip(),
                        "snmp": read("/proc/net/snmp").strip(),
                        "network": read("/proc/net/dev").strip(),
                        "ss": command_output(["ss", "-s"]),
                        "processes": command_output(
                            [
                                "ps",
                                "-eo",
                                "pid,ppid,comm,%cpu,%mem,rss,vsz,stat",
                                "--sort=-%cpu",
                            ]
                        ),
                        "docker": command_output(
                            ["docker", "stats", "--no-stream", "--format", "{{json .}}"]
                        ),
                        "docker_state": docker_state(),
                    },
                    sort_keys=True,
                )
                + "\n"
            )
            previous = current


if __name__ == "__main__":
    main()
