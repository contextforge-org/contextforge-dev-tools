# FYRE built-in dataplane versus external dataplane benchmark

The default FYRE workflow runs the same modern MCP client against the built-in
dataplane and the external dataplane on the same target VM. It
provisions three standalone Ubuntu 24.04 VMs and runs the two target stacks
sequentially so the target hardware is identical.

| Role | Default allocation | Purpose |
| --- | ---: | --- |
| Locust | 4 vCPU / 16 GB | Three distributed `FastHttpUser` workers with zero wait |
| Target | 4 vCPU / 4 GB | Built-in dataplane or external dataplane, one lane at a time |
| Fast Time | 8 vCPU / 32 GB | Six nonfailure tools with explicit zero backend delay |

The eight default measurements are built-in dataplane and external dataplane at 125, 250, 500, and
1,000 users. Every measurement ramps for 30 seconds, warms up for 30 seconds,
resets statistics, and records one hour. Both lanes send the same stateless
`2026-07-28` requests from the same Locust file. The built-in gateway owns any
session or backend protocol translation.

The built-in dataplane target includes the Python gateway, PostgreSQL, and Redis. The
external dataplane target includes Rust, Redis, and the loopback JWKS helper. Both use
the same remote Fast Time VM and private FYRE network.

The built-in image is pinned by digest and built from
`IBM/mcp-context-forge` commit
`33e2dd93a53a9cc2c5088b731822dfec4852fa2e` on the MCP SDK v2 branch. That
revision accepts the same `2026-07-28` stateless client used by the external dataplane lane.

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

The OpenShift profile requires `FYRE_PRODUCT_GROUP_ID`, Docker on the
orchestration host, and access to the FYRE OpenShift API and cluster DNS. The
CLI runs a digest-pinned OpenShift client container, so a host `oc` installation
is not required.

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
allocation are identical between lanes. The report calculates `external
dataplane RPS ÷ built-in dataplane RPS` at each matching user count and includes request totals, errors,
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
times. It is separate from the default eight-run built-in-dataplane-versus-external-dataplane CI
comparison.

Use the packaged two-core comparison profile to run the same eight measurements
on one 2 vCPU / 2 GB target VM:

```bash
cf-integration load fyre run \
  --file benchmarks/fyre/comparison-2v2.yaml \
  --run-id builtin-external-2v2
```

Comparison reports derive the target allocation from the selected profile; both
lanes always run sequentially on that same VM.

## Parallel OpenShift profile with 40 GB disks

`openshift.yaml` runs the same eight comparison measurements on a FYRE
OpenShift cluster while reducing every master and worker root disk to 40 GB.
The built-in and external lanes run concurrently and remain isolated on six
dedicated workers:

| Lane role | Workers | Pod allocation on each worker |
| --- | ---: | ---: |
| Built-in and external Locust | 2 | 4 vCPU / 16 GiB each |
| Built-in and external target | 2 | 4 vCPU / 4 GiB each |
| Built-in and external Fast Time | 2 | 8 vCPU / 32 GiB each |

Each Locust pod contains one master and three workers. Each target allocation
includes its supporting PostgreSQL/Redis or Redis/JWKS containers. Each lane
has a separate zero-delay Fast Time service. No measured target, load generator,
or backend shares a worker node with the other lane.

Run the complete parallel comparison with:

```bash
cf-integration load fyre run \
  --file benchmarks/fyre/openshift.yaml \
  --run-id openshift-builtin-external
```

The short form is:

```bash
cf-integration l f r -f benchmarks/fyre/openshift.yaml -i openshift-builtin-external
```

The command creates the cluster through the FYRE OpenShift API, authenticates
with the generated kubeadmin credential, assigns the six workers by their
configured CPU and memory, runs both lanes in parallel at 125, 250, 500, and
1,000 users, downloads every phase before deleting its pods, writes the final
report, deletes the benchmark namespace, and deletes only the run-owned
cluster. A failed or interrupted campaign retains its local run state and
retries cluster cleanup three times.

OpenShift artifacts use the same
`$CF_INTEGRATION_DIR/fyre/<run-id>/results/` layout and add `report.md`, a
self-contained report with the result table, memory averages and peaks,
request mix, pinned images, and Mermaid architecture. The cluster record and
manifest recursively omit passwords, tokens, pull secrets, API keys, and
kubeconfig data.

The built-in lane remains pinned to the MCP SDK v2 fixture image until that SDK
change is available in the main gateway image. The profile must not be changed
back to the main image before that merge because both lanes use the same modern
`2026-07-28` client.

### Fully parallel 2 vCPU / 2 GiB comparison

`openshift-2v2-parallel.yaml` runs all eight measurements at the same time:
built-in and external dataplane lanes at 125, 250, 500, and 1,000 users. Each
measurement gets its own target, Locust, and Fast Time pods with identical
requests and limits. Pods share only with pods serving the same role.

| Dedicated worker role | Worker size | Pods | Reserved per measurement |
| --- | ---: | ---: | ---: |
| Target | 18 vCPU / 18 GiB | 8 | 2 vCPU / 2 GiB |
| Locust | 14 vCPU / 12 GiB | 8 | 1.5 vCPU / 1.25 GiB |
| Fast Time | 14 vCPU / 13 GiB | 8 | 1.5 vCPU / 1.375 GiB |

Each target reservation includes its supporting PostgreSQL and Redis
containers for the built-in dataplane, or Redis and loopback JWKS containers
for the external dataplane. Each Locust pod has one master and three workers.
The helper pressure gate rejects the campaign if the shared helper workers or
individual helper pods become the bottleneck.

Run the full comparison with one command:

```bash
cf-integration load fyre run \
  --file benchmarks/fyre/openshift-2v2-parallel.yaml \
  --run-id openshift-2v2-parallel
```

The short form is:

```bash
cf-integration l f r -f benchmarks/fyre/openshift-2v2-parallel.yaml -i openshift-2v2-parallel
```

The OpenShift control plane and all three workers use 40 GB root disks. The
orchestration command may run on a persistent VM or CI worker; the benchmark
continues if the developer laptop sleeps. Artifacts are downloaded to
`$CF_INTEGRATION_DIR/fyre/<run-id>/results/` before the run-owned cluster is
deleted.
