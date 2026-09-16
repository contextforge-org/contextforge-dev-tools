# FYRE built-in versus Rust benchmark

The default FYRE workflow runs the same modern MCP client against the built-in
Python gateway and the external Rust dataplane on the same target VM. It
provisions three standalone Ubuntu 24.04 VMs and runs the two target stacks
sequentially so the target hardware is identical.

| Role | Default allocation | Purpose |
| --- | ---: | --- |
| Locust | 4 vCPU / 16 GB | Three distributed `FastHttpUser` workers with zero wait |
| Target | 4 vCPU / 4 GB | Built-in gateway or Rust dataplane, one lane at a time |
| Fast Time | 8 vCPU / 32 GB | Six nonfailure tools with explicit zero backend delay |

The eight default measurements are built-in and Rust at 125, 250, 500, and
1,000 users. Every measurement ramps for 30 seconds, warms up for 30 seconds,
resets statistics, and records one hour. Both lanes send the same stateless
`2026-07-28` requests from the same Locust file. The built-in gateway owns any
session or backend protocol translation.

The built-in target includes the Python gateway, PostgreSQL, and Redis. The
Rust target includes the dataplane, Redis, and loopback JWKS helper. Both use
the same remote Fast Time VM and private FYRE network.

The built-in image is pinned by digest and built from
`IBM/mcp-context-forge` commit
`33e2dd93a53a9cc2c5088b731822dfec4852fa2e` on the MCP SDK v2 branch. That
revision accepts the same `2026-07-28` stateless client used by the Rust lane.

## Prerequisites

- Terraform 1.8+, or `CF_TERRAFORM_BIN` pointing to a compatible binary.
- `python3`, `uv`, SSH, and SCP on the orchestration host. The CLI runs pinned
  `ansible-core` through `uv` for host bootstrap.
- The SSH key pair configured in `scaling.yaml`.
- `FYRE_USERNAME` and `FYRE_API_KEY`. `FYRE_PRODUCT_GROUP_ID` and `FYRE_SITE`
  are optional.

Credentials are inherited by Terraform and are not written to manifests,
reports, or command arguments. Runtime benchmark tokens stay in mode-600 files
on the ephemeral VMs.

FYRE's standalone VM API and the pinned provider allocate a 250 GB Ubuntu root
disk and expose no create-time root-disk setting. The default three-VM run
therefore needs 750 GB of FYRE disk quota. The CLI checks CPU, memory, disk, and
public-IP quota before it creates any benchmark VM.

## Run the complete comparison

The bare command is the CI entrypoint for the complete eight-run comparison:

```bash
cf-integration load fyre run
```

Assigning a run ID makes artifact collection and recovery deterministic:

```bash
cf-integration load fyre run --run-id builtin-rust-hourly
cf-integration l f r -i builtin-rust-hourly

cf-integration load fyre status --run-id builtin-rust-hourly
cf-integration l f s -i builtin-rust-hourly

cf-integration load fyre destroy --run-id builtin-rust-hourly
cf-integration l f d -i builtin-rust-hourly
```

A CI job only needs to expose the FYRE credentials, invoke the same command,
and upload `$CF_INTEGRATION_DIR/fyre/<run-id>/`. The command provisions the
VMs, bootstraps Docker, executes all eight measurements, downloads reports,
builds the comparison artifacts, and destroys only VMs owned by that run.

Generated state lives under `$CF_INTEGRATION_DIR/fyre/<run-id>/`. Each run has
isolated Terraform state, and cleanup verifies its ownership record. Existing
manually created VMs are outside that state.

The main outputs are:

- `results/summary.csv`
- `results/summary.json`
- `results/slack-comparison.png`
- `results/comparison/<lane>-<users>/` for Locust CSV, HTML, JSON, logs, and
  host telemetry from each measured phase
- `manifest.json` for the exact images, allocation, workload, and inventory

Artifacts are downloaded after every phase. On a request or worker error, the
campaign stops without advancing to the next load. On interruption or failure,
the CLI preserves downloaded artifacts, attempts a final remote recovery,
retries Terraform cleanup three times, and records `cleanup-failed` when a
manual `destroy` is still required. FYRE's 12-hour expiry is the final backstop
for the eight-hour measured campaign.

## Fairness and helper headroom

The client implementation, protocol revision, six-tool mix, request arguments,
zero delay, ramp, warmup, measurement window, request timeout, and target VM
allocation are identical between lanes. The report calculates `Rust RPS ÷
built-in RPS` at each matching user count and includes request totals, errors,
and p50/p95/p99 latency.

Telemetry covers CPU, per-core utilization, memory, swap and scheduling
pressure, socket counters, container health, worker exits, and virtualization
steal. Sustained helper pressure invalidates the partial comparison. The CLI
increases only the saturated helper through the configured sizes, archives the
partial run under `invalidated/`, and repeats all eight measurements so the
final rows use one helper allocation. Reaching 16 vCPU / 32 GB without helper
headroom leaves the campaign inconclusive.

Benchmark service ports and the distributed Locust coordinator bind only to
private or loopback addresses. The public FYRE interfaces are used for SSH
orchestration only.

## Optional capacity-search profile

`vertical-low-memory.yaml` retains the earlier Rust-only capacity-search mode
for one 2 vCPU / 2 GB dataplane followed by one 4 vCPU / 4 GB dataplane:

```bash
cf-integration load fyre run \
  --file benchmarks/fyre/vertical-low-memory.yaml \
  --run-id vertical-low-memory
```

That custom profile starts at 125 users, detects errors or a throughput plateau,
refines the boundary, and confirms the selected zero-error capacity three
times. It is separate from the default eight-run built-in-versus-Rust CI
comparison.
