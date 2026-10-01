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
import report

sys.path.insert(0, str(Path(__file__).parent / "deploy"))
import monitor
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
    @mock.patch.object(campaign, "reset_comparison_target")
    @mock.patch.object(campaign, "smoke")
    @mock.patch.object(campaign, "prepare_builtin_comparison")
    @mock.patch.object(campaign, "prepare_rust_comparison")
    @mock.patch.object(campaign, "one_phase")
    def test_fixed_comparison_runs_the_eight_default_benchmarks(
        self, phase, rust, builtin, _smoke, _reset
    ):
        rust.return_value = (["http://rust/mcp"], list(smoke.TOOLS), "rust.env")
        builtin.return_value = (
            ["http://builtin/mcp"],
            [f"fast_time_{name}" for name in smoke.TOOLS],
            "builtin.env",
        )
        phase.side_effect = lambda _r, _c, _i, _u, _o, users, *_args: passed(
            users, float(users)
        )
        test_config = config()
        test_config.update(
            {
                "scenarios": [{"id": "comparison"}],
                "workload": {
                    **test_config["workload"],
                    "protocol_version": "2026-07-28",
                    "user_levels": [125, 250, 500, 1000],
                },
            }
        )
        inventory = {"locust": {}, "fast_time": {}, "dataplanes": [{}]}
        with tempfile.TemporaryDirectory() as directory:
            result = campaign.fixed_comparison(
                None, test_config, inventory, Path(directory)
            )
        self.assertEqual(result["status"], "confirmed")
        self.assertEqual(
            [(item["lane"], item["users"]) for lane in result["runs"].values() for item in lane],
            [
                ("rust", 125),
                ("rust", 250),
                ("rust", 500),
                ("rust", 1000),
                ("builtin", 125),
                ("builtin", 250),
                ("builtin", 500),
                ("builtin", 1000),
            ],
        )

    @mock.patch.object(campaign, "reset_comparison_target")
    @mock.patch.object(campaign, "smoke")
    @mock.patch.object(campaign, "prepare_builtin_comparison")
    @mock.patch.object(campaign, "prepare_rust_comparison")
    @mock.patch.object(campaign, "one_phase")
    def test_fixed_comparison_stops_after_first_error(
        self, phase, rust, builtin, _smoke, _reset
    ):
        rust.return_value = (["http://rust/mcp"], list(smoke.TOOLS), "rust.env")
        phase.side_effect = [passed(125, 100.0), {"passed": False, "reason": "error"}]
        test_config = config()
        test_config.update(
            {
                "scenarios": [{"id": "comparison"}],
                "workload": {
                    **test_config["workload"],
                    "protocol_version": "2026-07-28",
                    "user_levels": [125, 250, 500, 1000],
                },
            }
        )
        inventory = {"locust": {}, "fast_time": {}, "dataplanes": [{}]}
        with tempfile.TemporaryDirectory() as directory:
            result = campaign.fixed_comparison(
                None, test_config, inventory, Path(directory)
            )
        self.assertEqual(result["status"], "failed")
        self.assertEqual(phase.call_count, 2)
        builtin.assert_not_called()

    @mock.patch.object(campaign, "reset_comparison_target")
    @mock.patch.object(campaign, "smoke")
    @mock.patch.object(campaign, "prepare_builtin_comparison")
    @mock.patch.object(campaign, "prepare_rust_comparison")
    @mock.patch.object(campaign, "one_phase")
    def test_fixed_comparison_requests_a_full_rerun_after_helper_saturation(
        self, phase, rust, builtin, _smoke, _reset
    ):
        rust.return_value = (["http://rust/mcp"], list(smoke.TOOLS), "rust.env")
        saturated = passed(125, 100.0)
        saturated["pressure"] = {"locust": {"mean_cpu_percent": 71.0}}
        phase.return_value = saturated
        test_config = config()
        test_config.update(
            {
                "scenarios": [{"id": "comparison"}],
                "workload": {
                    **test_config["workload"],
                    "protocol_version": "2026-07-28",
                    "user_levels": [125, 250, 500, 1000],
                },
            }
        )
        inventory = {"locust": {}, "fast_time": {}, "dataplanes": [{}]}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaises(SystemExit) as exit_status:
                campaign.fixed_comparison(None, test_config, inventory, root)
            request = json.loads((root / "helper-request.json").read_text())
            result = json.loads((root / "result.json").read_text())
        self.assertEqual(exit_status.exception.code, campaign.HELPER_SATURATED)
        self.assertEqual(request, {"role": "locust"})
        self.assertEqual(result["status"], "inconclusive")
        builtin.assert_not_called()

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
        search_rates = {125: 100.0, 250: 130.0, 500: 133.0, 1000: 134.0}

        def result(_r, _c, _i, _u, _o, users, name):
            if name.startswith("search"):
                return passed(users, search_rates[users])
            return passed(users, 132.0)

        measured.side_effect = result
        phase.return_value = passed(281, 1000.0)
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
        self.assertEqual(result["users"], 281)
        self.assertEqual(result["plateau_boundary"]["below"]["users"], 250)
        self.assertEqual(result["plateau_boundary"]["at_or_above"]["users"], 281)
        self.assertLessEqual(result["plateau_boundary"]["width_percent"], 12.5)
        calls = [(call.args[5], call.args[6]) for call in measured.call_args_list]
        self.assertIn((375, "plateau-refine-375"), calls)
        self.assertIn((312, "plateau-refine-312"), calls)
        self.assertIn((281, "plateau-refine-281"), calls)
        self.assertNotIn(2000, [users for users, _name in calls])

    @mock.patch.object(campaign, "smoke")
    @mock.patch.object(campaign, "one_phase")
    @mock.patch.object(campaign, "measured_step")
    def test_failed_candidate_confirmation_refines_and_confirms_a_lower_load(
        self, measured, phase, _smoke
    ):
        search_rates = {125: 100.0, 250: 150.0, 500: 200.0}
        test_config = config()
        test_config["workload"]["maximum_users"] = 500

        def result(_r, _c, _i, _u, _o, users, name):
            if name == "confirm-1-500":
                return {"passed": False, "users": users, "reason": "first error"}
            if name.startswith("search"):
                return passed(users, search_rates[users])
            if name == "confirm-refine-1-468":
                return {"passed": False, "users": users, "reason": "first error"}
            return passed(users, float(users))

        measured.side_effect = result
        phase.return_value = passed(437, 1000.0)
        with tempfile.TemporaryDirectory() as directory:
            result = campaign.capacity_search(
                None,
                test_config,
                {
                    "locust": {},
                    "fast_time": {"private_ip": "10.0.0.2"},
                    "dataplanes": [],
                },
                [],
                Path(directory),
            )

        self.assertEqual(result["status"], "confirmed")
        self.assertEqual(result["users"], 437)
        self.assertEqual(len(result["confirmation_failures"]), 1)
        calls = [(call.args[5], call.args[6]) for call in measured.call_args_list]
        self.assertIn((375, "confirm-refine-1-375"), calls)
        self.assertIn((437, "confirm-1-437"), calls)

    def test_helper_saturation_uses_sustained_thresholds(self):
        result = {"pressure": {"locust": {"mean_cpu_percent": 71.0}}}
        self.assertEqual(campaign.helper_saturation(config(), result), "locust")
        result["pressure"]["locust"]["mean_cpu_percent"] = 69.0
        self.assertIsNone(campaign.helper_saturation(config(), result))

    @mock.patch.object(campaign, "smoke")
    @mock.patch.object(campaign, "one_phase")
    def test_step_uses_one_continuous_warmup_and_measurement(self, phase, _smoke):
        phase.return_value = passed(125, 100.0)
        result = campaign.measured_step(
            None, config(), {"locust": {}}, [], Path("unused"), 125, "step"
        )
        self.assertTrue(result["passed"])
        phase.assert_called_once_with(
            None, config(), {"locust": {}}, [], Path("unused"), 125, 120, "step"
        )

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

    def test_prepare_hosts_quotes_inventory_backend_url(self):
        remote = mock.Mock()
        remote.ssh.return_value = mock.Mock(stdout="test-token\n", returncode=0)
        private_ip = "10.0.0.2; touch /tmp/unquoted"
        test_config = {
            "images": {
                "dataplane": "dataplane@sha256:test",
                "fast_time": "fast-time@sha256:test",
                "helpers": "helpers@sha256:test",
                "locust": "locust@sha256:test",
                "redis": "redis@sha256:test",
            },
            "workload": {
                "config_cache_seconds": 60,
                "protocol_version": "2026-07-28",
            },
        }
        inventory = {
            "locust": {"public_ip": "192.0.2.10", "private_ip": "10.0.0.10"},
            "fast_time": {"public_ip": "192.0.2.20", "private_ip": private_ip},
            "dataplanes": [
                {"public_ip": "192.0.2.30", "private_ip": "10.0.0.30"}
            ],
        }
        with (
            tempfile.TemporaryDirectory() as directory,
            mock.patch.object(campaign, "bootstrap_hosts"),
            mock.patch.object(campaign, "compose_up"),
            mock.patch.object(campaign, "write_remote_file"),
        ):
            campaign.prepare_hosts(
                test_config,
                inventory,
                remote,
                Path("deploy"),
                Path("bootstrap.yml"),
                Path("known_hosts"),
                Path(directory),
            )
        command = next(
            call.args[1]
            for call in remote.ssh.call_args_list
            if "config_writer fixture" in call.args[1]
        )
        backend_url = f"http://{private_ip}:9080/mcp"
        self.assertIn(campaign.shlex.quote(backend_url), command)

    def test_builtin_verify_protocol_alias_maps_to_fast_time_tool(self):
        self.assertEqual(
            smoke.base_tool_name("fast_time_verify_protocol"), "verify-protocol"
        )

    def test_compose_pull_retries_before_starting_containers(self):
        remote = mock.Mock()
        remote.ssh.side_effect = [
            mock.Mock(returncode=1),
            mock.Mock(returncode=0),
            mock.Mock(returncode=0),
        ]
        with mock.patch.object(campaign.time, "sleep") as sleep:
            campaign.compose_up(remote, "192.0.2.10", "dataplane.compose.yaml")
        self.assertEqual(remote.ssh.call_count, 3)
        self.assertIn(" pull", remote.ssh.call_args_list[0].args[1])
        self.assertIn(" pull", remote.ssh.call_args_list[1].args[1])
        self.assertIn(" up -d --wait", remote.ssh.call_args_list[2].args[1])
        sleep.assert_called_once_with(5)

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
                "--reset-stats",
                "--measurement-seconds",
                "1",
                "--warmup-seconds",
                "1",
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
        run_calls = [call for call in docker.call_args_list if call.args[0] == "run"]
        self.assertEqual(
            docker.call_args_list[0].args[:2], ("network", "create")
        )
        for call in run_calls[:2]:
            arguments = call.args
            user_index = arguments.index("--user")
            self.assertEqual(arguments[user_index + 1], "0:0")
        master_arguments = run_calls[0].args
        self.assertIn("MCP_WARMUP_SECONDS=1", master_arguments)
        self.assertNotIn("--master-bind-host", master_arguments)
        self.assertNotIn("--reset-stats", master_arguments)
        worker_arguments = run_calls[1].args
        master_index = worker_arguments.index("--master-host")
        name_index = master_arguments.index("--name")
        self.assertEqual(
            worker_arguments[master_index + 1], master_arguments[name_index + 1]
        )
        self.assertIn("MCP_REPLICA_OFFSET=0", worker_arguments)

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
        self.assertFalse(campaign.docker_pressure("[]"))
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
        self.assertTrue(
            campaign.docker_pressure(
                '[{"Status":"running","ExitCode":0,"OOMKilled":true}]'
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

    @mock.patch.object(run_locust.time, "sleep")
    @mock.patch.object(run_locust, "docker")
    @mock.patch.object(run_locust, "container_state")
    def test_clean_worker_exit_waits_for_clean_master(self, state, docker, _sleep):
        state.side_effect = [("running", 0), ("exited", 0), ("exited", 0)]
        self.assertEqual(run_locust.wait_for_cluster("master", ["worker"]), 0)
        docker.assert_not_called()

    @mock.patch.object(run_locust.time, "sleep")
    @mock.patch.object(run_locust, "docker")
    @mock.patch.object(run_locust, "container_state")
    def test_created_containers_are_allowed_to_finish_starting(
        self, state, docker, _sleep
    ):
        state.side_effect = [
            ("created", 0),
            ("running", 0),
            ("created", 0),
            ("running", 0),
            ("running", 0),
            ("exited", 0),
        ]
        self.assertEqual(run_locust.wait_for_cluster("master", ["worker"]), 0)
        docker.assert_not_called()

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

    def test_comparison_report_writes_machine_readable_lane_results(self):
        lane = {
            "users": 125,
            "requests": 1000,
            "failures": 0,
            "rps": 100.0,
            "p50_ms": 10.0,
            "p95_ms": 20.0,
            "p99_ms": 30.0,
        }
        result = {
            "status": "confirmed",
            "runs": {
                "builtin": [lane],
                "rust": [{**lane, "requests": 2500, "rps": 250.0}],
            },
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result_dir = root / "comparison"
            result_dir.mkdir()
            (result_dir / "result.json").write_text(json.dumps(result))
            report.comparison_report(
                {"workload": {"user_levels": [125]}}, root, render=False
            )
            summary = json.loads((root / "summary.json").read_text())
            markdown = (root / "report.md").read_text()
            with (root / "summary.csv").open(newline="") as stream:
                csv_rows = list(csv.DictReader(stream))
        self.assertEqual(summary["rows"][0]["external_vs_built_in"], 2.5)
        self.assertEqual(csv_rows[0]["external_dataplane_requests"], "2500")
        self.assertIn("External vs built-in", markdown)
        self.assertNotIn("](", markdown)

    def test_monitor_calculates_cpu_and_memory_pressure(self):
        cpu = monitor.cpu_percent(
            {"cpu": [100, 0, 0, 900, 0, 0, 0, 0]},
            {"cpu": [150, 0, 0, 950, 0, 0, 0, 10]},
        )
        self.assertEqual(cpu["cpu"]["busy_percent"], 54.545)
        self.assertEqual(cpu["cpu"]["steal_percent"], 9.091)
        with mock.patch.object(
            monitor,
            "read",
            return_value="MemTotal: 1000 kB\nMemAvailable: 250 kB\nSwapTotal: 100 kB\nSwapFree: 80 kB\n",
        ):
            memory = monitor.memory()
        self.assertEqual(memory["used_percent"], 75.0)
        self.assertEqual(memory["swap_free_kib"], 80)


if __name__ == "__main__":
    unittest.main()
