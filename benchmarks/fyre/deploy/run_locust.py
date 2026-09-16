"""Run one distributed, headless Locust phase and propagate any worker failure."""

from __future__ import annotations

import argparse
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

CONTAINERS: list[str] = []


def docker(
    *arguments: str, check: bool = True, capture: bool = False
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["docker", *arguments],
        check=check,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.STDOUT if capture else None,
    )


def cleanup() -> None:
    if CONTAINERS:
        docker("rm", "--force", *CONTAINERS, check=False, capture=True)


def stop(_signal: int, _frame) -> None:
    cleanup()
    raise SystemExit(130)


def container_state(name: str) -> tuple[str, int]:
    result = docker(
        "inspect",
        "--format",
        "{{.State.Status}} {{.State.ExitCode}}",
        name,
        check=False,
        capture=True,
    )
    if result.returncode != 0:
        return "missing", 1
    fields = result.stdout.strip().split()
    if len(fields) != 2:
        return "invalid", 1
    try:
        return fields[0], int(fields[1])
    except ValueError:
        return "invalid", 1


def wait_for_cluster(master: str, workers: list[str]) -> int:
    while True:
        master_state, master_exit = container_state(master)
        if master_state in {"exited", "dead", "missing", "invalid"}:
            return master_exit
        for worker in workers:
            worker_state, worker_exit = container_state(worker)
            if worker_state in {"exited", "dead"} and worker_exit == 0:
                continue
            if worker_state != "running":
                docker("stop", "--time", "1", master, check=False, capture=True)
                return 1
        time.sleep(0.5)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", required=True)
    parser.add_argument("--users", type=int, required=True)
    parser.add_argument("--spawn-rate", type=float, required=True)
    parser.add_argument("--seconds", type=int, required=True)
    parser.add_argument("--workers", type=int, required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--env-file", required=True)
    parser.add_argument("--reset-stats", action="store_true")
    parser.add_argument("--measurement-seconds", type=int)
    parser.add_argument("--warmup-seconds", type=int)
    args = parser.parse_args()
    if min(args.users, args.spawn_rate, args.seconds, args.workers) <= 0:
        parser.error("users, spawn-rate, seconds, and workers must be positive")
    if args.measurement_seconds is not None and args.measurement_seconds <= 0:
        parser.error("measurement-seconds must be positive")
    if args.warmup_seconds is not None and args.warmup_seconds < 0:
        parser.error("warmup-seconds cannot be negative")
    measurement = args.measurement_seconds is not None
    if args.reset_stats != measurement or (args.warmup_seconds is not None) != measurement:
        parser.error(
            "reset-stats, measurement-seconds, and warmup-seconds must be used together"
        )

    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    prefix = f"cf-fyre-{os.getpid()}"
    master = f"{prefix}-master"
    CONTAINERS.append(master)
    common = [
        "--user",
        "0:0",
        "--network",
        "host",
        "--ulimit",
        "nofile=65536:65536",
        "--env-file",
        args.env_file,
        "--volume",
        f"{Path.cwd() / 'locustfile_mcp.py'}:/mnt/locust-cf/locustfile_mcp.py:ro",
        "--volume",
        f"{output.resolve()}:/mnt/reports",
    ]
    if args.reset_stats:
        common.extend(
            [
                "--env",
                "MCP_MEASUREMENT_MARKER=/mnt/reports/measurement-start.txt",
                "--env",
                f"MCP_MEASUREMENT_SECONDS={args.measurement_seconds}",
                "--env",
                f"MCP_WARMUP_SECONDS={args.warmup_seconds}",
            ]
        )
    run_seconds = args.seconds + 30 if args.measurement_seconds is not None else args.seconds
    master_args = [
        "run",
        "--detach",
        "--name",
        master,
        *common,
        args.image,
        "-f",
        "/mnt/locust-cf/locustfile_mcp.py",
        "--master",
        "--expect-workers",
        str(args.workers),
        "--headless",
        "--users",
        str(args.users),
        "--spawn-rate",
        str(args.spawn_rate),
        "--run-time",
        f"{run_seconds}s",
        "--stop-timeout",
        "1",
        "--host",
        "http://127.0.0.1",
        "--csv",
        "/mnt/reports/locust",
        "--csv-full-history",
        "--html",
        "/mnt/reports/locust.html",
        "--json-file",
        "/mnt/reports/locust.json",
        "--logfile",
        "/mnt/reports/locust.log",
    ]
    docker(*master_args)
    try:
        workers = []
        for index in range(args.workers):
            name = f"{prefix}-worker-{index + 1}"
            CONTAINERS.append(name)
            workers.append(name)
            docker(
                "run",
                "--detach",
                "--name",
                name,
                *common,
                args.image,
                "-f",
                "/mnt/locust-cf/locustfile_mcp.py",
                "--worker",
                "--master-host",
                "127.0.0.1",
            )
        status = wait_for_cluster(master, workers)
        for name in CONTAINERS:
            state, exit_code = container_state(name)
            if state in {"exited", "dead"} and exit_code != 0:
                status = 1
        sys.exit(status)
    finally:
        time.sleep(0.2)
        cleanup()


if __name__ == "__main__":
    main()
