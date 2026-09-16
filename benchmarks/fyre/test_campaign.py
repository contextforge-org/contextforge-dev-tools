"""Unit coverage for FYRE capacity-search and report-input behavior."""

from __future__ import annotations

import csv
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import campaign

sys.path.insert(0, str(Path(__file__).parent / "deploy"))
import run_locust
import smoke


def passed(users: int, rps: float) -> dict:
    return {
        "passed": True,
        "users": users,
        "rps": rps,
        "failures": 0,
        "p50_ms": 1.0,
        "p95_ms": 2.0,
        "p99_ms": 3.0,
        "per_replica_rps": {"MCP tools/call [replica-1]": rps},
        "pressure": {},
    }


def config() -> dict:
    return {
        "workload": {
            "first_users": 125,
            "maximum_users": 32_000,
            "maximum_campaign_seconds": 21_600,
            "plateau_improvement_percent": 5.0,
            "boundary_percent": 12.5,
            "repetitions": 3,
            "warmup_seconds": 30,
            "measure_seconds": 120,
            "helper_cpu_percent": 70.0,
            "helper_memory_percent": 80.0,
            "worker_core_percent": 85.0,
        },
        "images": {"locust": "locust@sha256:test"},
    }


class CapacityTests(unittest.TestCase):
    @mock.patch.object(campaign, "smoke")
    @mock.patch.object(campaign, "one_phase")
    @mock.patch.object(campaign, "measured_step")
    def test_first_failure_never_advances_above_the_failed_load(
        self, measured, phase, _smoke
    ):
        measured.side_effect = lambda _r, _c, _i, _u, _o, users, name: (
            passed(users, float(users))
            if name.startswith("confirm") or users <= 202
            else {"passed": False, "users": users, "reason": "first error"}
        )
        phase.return_value = passed(202, 1000.0)
        with tempfile.TemporaryDirectory() as directory:
            result = campaign.capacity_search(
                None,
                config(),
                {
                    "locust": {},
                    "fast_time": {"private_ip": "10.0.0.2"},
                    "dataplanes": [],
                },
                [],
                Path(directory),
            )
        calls = [
            call.args[5]
            for call in measured.call_args_list
            if not call.args[6].startswith("confirm")
        ]
        first_failure = calls.index(250)
        self.assertTrue(all(users <= 250 for users in calls[first_failure + 1 :]))
        self.assertEqual(result["status"], "confirmed")

    @mock.patch.object(campaign, "smoke")
    @mock.patch.object(campaign, "one_phase")
    @mock.patch.object(campaign, "measured_step")
    def test_two_sub_five_percent_steps_stop_at_plateau(self, measured, phase, _smoke):
        rates = {125: 100.0, 250: 103.0, 500: 106.0}
        measured.side_effect = lambda _r, _c, _i, _u, _o, users, _name: passed(
            users, rates[users]
        )
        phase.return_value = passed(500, 1000.0)
        with tempfile.TemporaryDirectory() as directory:
            result = campaign.capacity_search(
                None,
                config(),
                {
                    "locust": {},
                    "fast_time": {"private_ip": "10.0.0.2"},
                    "dataplanes": [],
                },
                [],
                Path(directory),
            )
        self.assertEqual(result["users"], 500)
        self.assertNotIn(1000, [call.args[5] for call in measured.call_args_list])

    def test_helper_saturation_uses_sustained_thresholds(self):
        result = {"pressure": {"locust": {"mean_cpu_percent": 71.0}}}
        self.assertEqual(campaign.helper_saturation(config(), result), "locust")
        result["pressure"]["locust"]["mean_cpu_percent"] = 69.0
        self.assertIsNone(campaign.helper_saturation(config(), result))

    @mock.patch.object(campaign, "smoke")
    @mock.patch.object(campaign, "one_phase")
    def test_warmup_and_measurement_are_separate_phases(self, phase, _smoke):
        phase.side_effect = [passed(125, 90.0), passed(125, 100.0)]
        result = campaign.measured_step(
            None, config(), {"locust": {}}, [], Path("unused"), 125, "step"
        )
        self.assertTrue(result["passed"])
        self.assertEqual(
            [(call.args[7], call.args[6]) for call in phase.call_args_list],
            [("step-warmup", 30), ("step", 120)],
        )
        self.assertNotIn("measurement", phase.call_args_list[0].kwargs)
        self.assertTrue(phase.call_args_list[1].kwargs["measurement"])

    def test_smoke_passes_script_once_to_python_entrypoint(self):
        remote = mock.Mock()
        campaign.smoke(
            remote,
            {"public_ip": "192.0.2.10"},
            ["http://192.0.2.20:4445/mcp"],
            "locust@sha256:test",
        )
        command = remote.ssh.call_args.args[1]
        self.assertIn("--entrypoint python", command)
        self.assertIn("--user 0:0", command)
        self.assertIn("locust@sha256:test smoke.py --urls", command)
        self.assertNotIn("locust@sha256:test python smoke.py", command)

    def test_smoke_uses_valid_convert_time_datetime(self):
        self.assertEqual(smoke.TOOLS["convert_time"]["time"], "2025-06-21T16:00:00Z")

    def test_monitor_detaches_all_standard_streams_from_ssh(self):
        remote = mock.Mock()
        remote.ssh.return_value.stdout = "123\n"
        self.assertEqual(
            campaign.start_monitor(remote, "192.0.2.10", "locust", "phase-1"), 123
        )
        command = remote.ssh.call_args.args[1]
        self.assertNotIn("cd ", command)
        self.assertIn("</dev/null", command)
        self.assertIn(">cf-fyre/telemetry/phase-1.log 2>&1 & echo $!", command)

    @mock.patch.object(run_locust, "wait_for_cluster", return_value=0)
    @mock.patch.object(run_locust, "container_state", return_value=("exited", 0))
    @mock.patch.object(run_locust, "docker")
    def test_locust_containers_can_write_root_owned_reports(
        self, docker, _state, _wait
    ):
        with tempfile.TemporaryDirectory() as directory:
            args = [
                "run_locust.py",
                "--image",
                "locust@sha256:test",
                "--users",
                "1",
                "--spawn-rate",
                "1",
                "--seconds",
                "1",
                "--workers",
                "1",
                "--output",
                directory,
                "--env-file",
                "benchmark.secret.env",
            ]
            with (
                mock.patch.object(sys, "argv", args),
                mock.patch.object(run_locust.signal, "signal"),
                mock.patch.object(run_locust.time, "sleep"),
                mock.patch.object(run_locust, "cleanup"),
                self.assertRaises(SystemExit) as exit_status,
            ):
                run_locust.main()
        self.assertEqual(exit_status.exception.code, 0)
        for call in docker.call_args_list[:2]:
            arguments = call.args
            user_index = arguments.index("--user")
            self.assertEqual(arguments[user_index + 1], "0:0")

    def test_pressure_excludes_ramp_and_warmup_samples(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "host.jsonl"
            samples = [
                {
                    "kind": "sample",
                    "time": 100.0,
                    "cpu": {
                        "cpu": {"busy_percent": 100.0, "steal_percent": 30.0},
                        "cpu0": {"busy_percent": 100.0, "steal_percent": 30.0},
                    },
                    "memory": {"used_percent": 99.0},
                    "netstat": "TcpExt: ListenOverflows ListenDrops\nTcpExt: 0 0",
                    "docker_state": '{"Status":"running","OOMKilled":false}',
                },
                {
                    "kind": "sample",
                    "time": 200.0,
                    "cpu": {
                        "cpu": {"busy_percent": 40.0, "steal_percent": 2.0},
                        "cpu0": {"busy_percent": 45.0, "steal_percent": 2.0},
                    },
                    "memory": {"used_percent": 50.0},
                    "netstat": "TcpExt: ListenOverflows ListenDrops\nTcpExt: 0 0",
                    "docker_state": '{"Status":"running","OOMKilled":false}',
                },
            ]
            path.write_text(
                "".join(json.dumps(sample) + "\n" for sample in samples),
                encoding="utf-8",
            )
            result = campaign.pressure(path, after=150.0)
        self.assertEqual(result["mean_cpu_percent"], 40.0)
        self.assertEqual(result["max_memory_percent"], 50.0)
        self.assertEqual(result["max_mean_core_percent"], 45.0)
        self.assertEqual(result["mean_steal_percent"], 2.0)
        self.assertFalse(result["worker_or_network_pressure"])

    def test_pressure_detects_network_drops(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "host.jsonl"
            samples = []
            for timestamp, drops in ((100.0, 0), (101.0, 1)):
                samples.append(
                    {
                        "kind": "sample",
                        "time": timestamp,
                        "cpu": {},
                        "memory": {"used_percent": 10.0},
                        "netstat": (
                            "TcpExt: ListenOverflows ListenDrops TCPBacklogDrop\n"
                            f"TcpExt: 0 {drops} 0"
                        ),
                        "docker_state": "",
                    }
                )
            path.write_text(
                "".join(json.dumps(sample) + "\n" for sample in samples),
                encoding="utf-8",
            )
            result = campaign.pressure(path)
        self.assertTrue(result["worker_or_network_pressure"])

    def test_docker_pressure_ignores_clean_exit_and_detects_oom(self):
        self.assertFalse(
            campaign.docker_pressure(
                '{"Status":"exited","ExitCode":0,"OOMKilled":false}'
            )
        )
        self.assertTrue(
            campaign.docker_pressure(
                '{"Status":"exited","ExitCode":137,"OOMKilled":true}'
            )
        )

    @mock.patch.object(run_locust.time, "sleep")
    @mock.patch.object(run_locust, "docker")
    @mock.patch.object(run_locust, "container_state")
    def test_worker_exit_stops_the_master_immediately(self, state, docker, _sleep):
        state.side_effect = [("running", 0), ("exited", 2)]
        self.assertEqual(run_locust.wait_for_cluster("master", ["worker"]), 1)
        docker.assert_called_once_with(
            "stop", "--time", "1", "master", check=False, capture=True
        )

    def test_stats_preserve_replica_rates_and_exclude_discovery(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "stats.csv"
            fields = [
                "Type",
                "Name",
                "Request Count",
                "Failure Count",
                "Requests/s",
                "50%",
                "95%",
                "99%",
            ]
            with path.open("w", newline="", encoding="utf-8") as stream:
                writer = csv.DictWriter(stream, fieldnames=fields)
                writer.writeheader()
                writer.writerow(
                    {
                        "Name": "MCP server/discover",
                        "Request Count": 100,
                        "Failure Count": 0,
                        "Requests/s": 50,
                        "50%": 50,
                        "95%": 80,
                        "99%": 90,
                    }
                )
                writer.writerow(
                    {
                        "Name": "MCP tools/call [replica-1]",
                        "Request Count": 1000,
                        "Failure Count": 0,
                        "Requests/s": 500,
                        "50%": 2,
                        "95%": 4,
                        "99%": 5,
                    }
                )
                writer.writerow(
                    {
                        "Name": "MCP tools/call [replica-2]",
                        "Request Count": 900,
                        "Failure Count": 0,
                        "Requests/s": 450,
                        "50%": 3,
                        "95%": 5,
                        "99%": 7,
                    }
                )
                writer.writerow(
                    {
                        "Name": "Aggregated",
                        "Request Count": 1900,
                        "Failure Count": 0,
                        "Requests/s": 950,
                        "50%": 2.5,
                        "95%": 4.5,
                        "99%": 6,
                    }
                )
            result = campaign.read_stats(path)
            aggregate = campaign.read_stats(path, use_aggregate=True)
        self.assertEqual(result["requests"], 1900)
        self.assertEqual(result["rps"], 950.0)
        self.assertEqual(len(result["per_replica_rps"]), 2)
        self.assertLess(result["p95_ms"], 5.0)

        self.assertEqual(aggregate["p50_ms"], 2.5)
        self.assertEqual(aggregate["p95_ms"], 4.5)
        self.assertEqual(aggregate["p99_ms"], 6.0)


if __name__ == "__main__":
    unittest.main()
