"""Focused tests for the OpenShift benchmark topology."""

from __future__ import annotations

import json
import subprocess
import unittest

import openshift_campaign


class OpenShiftCampaignTests(unittest.TestCase):
    def test_external_token_is_sent_over_stdin(self):
        class FakeOc:
            arguments = ()
            input_text = None

            def run(self, *arguments, input_text=None):
                self.arguments = arguments
                self.input_text = input_text
                return subprocess.CompletedProcess(
                    arguments, 0, json.dumps(["echo"]) + "\n", ""
                )

        oc = FakeOc()
        token = "private-token"
        tools = openshift_campaign.configure_external(
            oc,
            "target-external",
            "benchmark",
            token,
            "http://fast-time/mcp",
            "2026-07-28",
        )
        self.assertEqual(tools, ["echo"])
        self.assertEqual(oc.input_text, token + "\n")
        self.assertNotIn(token, " ".join(oc.arguments))

    def test_assigns_each_equal_lane_to_a_distinct_worker(self):
        pools = []
        nodes = []
        for memory, prefix in ((12, "target"), (20, "locust"), (40, "fast-time")):
            cpu = {12: 6, 20: 6, 40: 10}[memory]
            for lane in ("builtin", "external"):
                pools.append(
                    {
                        "role": f"{prefix}-{lane}",
                        "count": 1,
                        "cpu": cpu,
                        "memory_gb": memory,
                    }
                )
                nodes.append(
                    {
                        "metadata": {"name": f"{prefix}-{lane}", "labels": {}},
                        "status": {
                            "capacity": {
                                "cpu": str(cpu),
                                "memory": f"{memory * 1024 * 1024}Ki",
                            }
                        },
                    }
                )
        assigned = openshift_campaign.assign_nodes(
            {"infrastructure": {"openshift": {"worker_pools": pools}}},
            {"items": list(reversed(nodes))},
        )
        self.assertEqual(set(assigned), {pool["role"] for pool in pools})
        self.assertEqual(len(set(assigned.values())), 6)

    def test_locust_pod_reserves_four_cpus_and_sixteen_gib(self):
        config = {
            "infrastructure": {
                "openshift": {
                    "load_pod": {
                        "cpu_millicores": 4000,
                        "memory_mib": 16384,
                    }
                }
            },
            "images": {"locust": "locust@sha256:test"},
            "workload": {
                "protocol_version": "2026-07-28",
                "ramp_seconds": 30,
                "warmup_seconds": 30,
                "measure_seconds": 3600,
            },
        }
        pod = openshift_campaign.locust_pod(
            config,
            "benchmark",
            "external",
            "worker-1",
            125,
            "http://target:4445",
            ["echo"],
        )
        containers = pod["spec"]["containers"]
        cpu_milli = sum(
            int(item["resources"]["limits"]["cpu"].removesuffix("m"))
            if item["resources"]["limits"]["cpu"].endswith("m")
            else 1000 * int(item["resources"]["limits"]["cpu"])
            for item in containers
        )
        memory_mib = sum(
            int(item["resources"]["limits"]["memory"].removesuffix("Mi"))
            if item["resources"]["limits"]["memory"].endswith("Mi")
            else 1024 * int(item["resources"]["limits"]["memory"].removesuffix("Gi"))
            for item in containers
        )
        self.assertEqual(cpu_milli, 4000)
        self.assertEqual(memory_mib, 16384)
        self.assertEqual(len(containers), 4)

    def test_memory_summary_adds_all_gateway_sidecars(self):
        samples = [
            {
                "pods": "target-builtin gateway 100m 1200Mi\n"
                "builtin-db postgres 30m 400Mi\n"
                "builtin-db redis 10m 100Mi\n"
                "target-external dataplane 100m 900Mi\n"
            },
            {
                "pods": "target-builtin gateway 100m 1400Mi\n"
                "builtin-db postgres 30m 450Mi\n"
                "builtin-db redis 10m 110Mi\n"
            },
        ]
        memory = openshift_campaign.lane_memory(samples, "builtin")
        self.assertEqual(memory["average_mib"], 1830)
        self.assertEqual(memory["peak_mib"], 1960)
        self.assertEqual(memory["samples"], 2)

    def test_memory_summary_excludes_warmup_samples(self):
        samples = [
            {"time": 10, "pods": "target-external dataplane 100m 900Mi\n"},
            {"time": 20, "pods": "target-external dataplane 100m 400Mi\n"},
            {"time": 30, "pods": "target-external dataplane 100m 500Mi\n"},
        ]
        memory = openshift_campaign.lane_memory(
            samples, "external", start_time=20, end_time=30
        )
        self.assertEqual(memory["average_mib"], 450)
        self.assertEqual(memory["peak_mib"], 500)
        self.assertEqual(memory["samples"], 2)

    def test_helper_pressure_detects_sustained_locust_saturation(self):
        samples = [
            {
                "time": timestamp,
                "pods": "load-external-125 master 100m 100Mi\n"
                "load-external-125 worker-1 900m 100Mi\n"
                "load-external-125 worker-2 900m 100Mi\n"
                "load-external-125 worker-3 900m 100Mi\n"
                "fast-time-external-125 fast-time 1000m 100Mi\n",
            }
            for timestamp in (20, 30, 40)
        ]
        pressure = openshift_campaign.helper_pressure(
            samples,
            "external",
            {
                "infrastructure": {
                    "openshift": {
                        "load_pod": {
                            "cpu_millicores": 4000,
                            "memory_mib": 16384,
                        },
                        "backend_pod": {
                            "cpu_millicores": 8000,
                            "memory_mib": 32768,
                        },
                    }
                },
                "workload": {
                    "helper_cpu_percent": 70,
                    "helper_memory_percent": 80,
                    "worker_core_percent": 85,
                }
            },
            125,
            20,
            40,
        )
        self.assertIn("Locust worker CPU", pressure["saturated"])
        self.assertEqual(pressure["samples"], 3)


if __name__ == "__main__":
    unittest.main()
