# FYRE dataplane scaling benchmark

This benchmark compares vertical and horizontal Rust dataplane scaling with the
same total dataplane CPU and memory. It provisions a dedicated Locust VM, a
dedicated Fast Time VM, and one to three dataplane VMs. Each dataplane VM owns
its Redis and loopback JWKS helper, and receives the same routing snapshot and
ephemeral signing key.

| Scenario | Dataplane allocation | Total allocation |
| --- | --- | --- |
| Baseline | 1 × 2 vCPU / 8 GB | 2 vCPU / 8 GB |
| Vertical 2× | 1 × 4 vCPU / 16 GB | 4 vCPU / 16 GB |
| Horizontal 2× | 2 × 2 vCPU / 8 GB | 4 vCPU / 16 GB |
| Vertical 3× | 1 × 6 vCPU / 24 GB | 6 vCPU / 24 GB |
| Horizontal 3× | 3 × 2 vCPU / 8 GB | 6 vCPU / 24 GB |
| Vertical 4× extension | 1 × 8 vCPU / 32 GB | 8 vCPU / 32 GB |

## Prerequisites

- Terraform 1.8+, or set `CF_TERRAFORM_BIN` to a compatible Terraform binary.
- `python3`, `uv`, SSH, and SCP on the orchestration host.
- An SSH key pair at the paths configured in `scaling.yaml`.
- FYRE provider credentials in `FYRE_USERNAME` and `FYRE_API_KEY`.
- Optionally set `FYRE_PRODUCT_GROUP_ID` and `FYRE_SITE`. Without an explicit
  product group, the configuration uses the account default, then its sole
  product group, and finally quick-burn quota when the account permits it.
  Quick-burn VMs use an eight-hour TTL.

Credential values are inherited by Terraform and are never copied into the
run manifest, command arguments, reports, or logs.

## Run and recover

```bash
cf-integration load fyre run
cf-integration l f r -f benchmarks/fyre/scaling.yaml -i scale-candidate

cf-integration load fyre status --run-id scale-candidate
cf-integration l f s -i scale-candidate

cf-integration load fyre destroy --run-id scale-candidate
cf-integration l f d -i scale-candidate
```

Generated state lives under
`$CF_INTEGRATION_DIR/fyre/<run-id>/`. The CLI copies Terraform into that
directory, so each run has isolated state. Resource names begin with the run ID
and `destroy` verifies the ownership file before using that state. Existing
manually created VMs are outside the state and cannot be deleted by the command.

The run downloads each phase's Locust reports and host telemetry as it
finishes. It then builds `results/summary.json`, `results/summary.csv`, and
`results/slack-scaling.png` before destroying run-owned VMs. On error or
interrupt it retains already downloaded artifacts, retries Terraform cleanup
three times, and records `cleanup-failed` if manual `destroy` is needed.

## Capacity method

The workload uses modern MCP `2026-07-28`, `FastHttpUser`, multiple Locust
workers, and the six nonfailure Fast Time tools. Requests go directly to the
native endpoint of a dataplane replica; virtual users are assigned evenly
across replicas and the report retains per-replica request rates.

Each concurrency step smokes every tool through every replica, ramps within
30 seconds, warms the backend for 30 seconds, and measures for 120 seconds.
The measured Locust phase resets statistics when spawning completes, and the
telemetry summary uses the same recorded measurement-window boundary. It starts
at 125 users and doubles until the first error or a two-step throughput plateau.
After an error it only tests lower concurrency while refining the boundary to
12.5 percent. The selected capacity must pass three measured repetitions with
zero request and worker errors. Each scenario is bounded at 32,000 users, and
the full provision-and-benchmark matrix stops after six hours before recovery
and cleanup.

Locust and Fast Time start at 2 vCPU / 8 GB. Host and container telemetry
checks CPU, per-core use, memory, swap, pressure stalls, sockets, network
counters, worker exits, and virtualization steal. A saturated helper is grown
through the configured sizes. Any helper resize archives prior attempts under
`invalidated/` and restarts the matrix so final comparisons use the same helper
sizes. Reaching 16 vCPU / 32 GB without demonstrated headroom makes the
campaign inconclusive.

The final report includes confirmed zero-error RPS, p50/p95/p99, vertical and
horizontal speedups, scaling efficiency, matched horizontal advantage, RPS per
allocated dataplane vCPU, repetition variability, resource inventory, CPU
model, and steal time. Redis and authentication helpers run on each dataplane
VM, so the result measures the complete dataplane deployment allocation.
