# cf-integration

`cf-integration` manages ContextForge Docker stacks and runs probes, load tests,
live gateway checks, and official MCP conformance. Routed traffic uses one of
two lanes:

- `builtin`: the Python dataplane in `cf-controlplane`.
- `external`: the Rust `cf-dataplane`.

`/servers/{virtual_host_id}/mcp` routes to the external dataplane. Raw `/mcp`
and control-plane UI/API traffic route to `cf-controlplane`; there is no
fallback between them.

## Install

```bash
cargo binstall cf-integration
# or
cargo install cf-integration --locked
```

Use `cargo run --` before a command when running this checkout. Runtime use
requires Docker Compose v2 and Git. Node/npm are installed and run only inside
Docker images, including conformance and Inspector. The release workflow publishes
versioned fixture, tooling, and helper images; normal runs pull missing images
and reuse existing copies without compiling Rust or installing npm packages.
Use `CF_COMPOSE_BUILD=true` to rebuild images while developing the harness.
`CF_CONFORMANCE_IMAGE`, `CF_MCP_TOOLS_IMAGE`, and `CF_HELPERS_IMAGE` select
prebuilt alternatives, including images loaded by another CI workflow. Rust 1.97
is needed only for explicit source builds.

The release publishes amd64 and arm64 images after container smoke tests. The
`cf-integration-fixture`, `cf-integration-tools`, and `cf-integration-helpers`
GHCR packages must be public so normal runs can pull them without credentials.
The release checks anonymous image access and keeps the CLI release a draft
until image publication succeeds. On the first release, make these GHCR
packages public and rerun the release job if the access check fails.

Published images are the default. Set `CF_DATAPLANE_REF` to build and test a
local dataplane ref.

## Short forms

Every public command and option has a short form, shown in `--help`.

| Command | Alias | Subcommands |
| --- | --- | --- |
| `stack` | `s` | `up` → `u`, `down` → `d`, `status` → `s`, `logs` → `l`, `config` → `c` |
| `probe` | `p` | — |
| `load` | `l` | `run` → `r` |
| `live` | `v` | — |
| `conformance` | `c` | `run` → `r`, `report` → `p` |
| `debug` | `d` | `inspect` → `i`, `token` → `t` |

Common flags are `-l` for lane, `-s` for standalone, `-c` for client era,
`-e` for server era, and `-p` for operational protocol mode. Load uses `-u`
for users, `-r` for spawn rate, `-t` for duration, `-S` for smoke, and `-o`
for observability. For example:

```bash
cf-integration l r -l builtin -c legacy -u 20 -r 5 -t 2m
cf-integration l r -l external -c modern -s -u 20 -r 5 -t 2m
cf-integration c r -l external -s -c modern -e modern
```

## Selection

Routed commands accept `--lane builtin|external`; `external` is the default.
Conformance and protocol-only live tests also accept `fixture-direct`.
`stack down` accepts `all`. No command accepts the old `--topology` option.

Stack, probe, live, and debug inspect accept `--protocol-version modern|legacy`:

- `modern`: current per-request, stateless MCP.
- `legacy`: current initialization-based MCP.

These operational selectors use era names. Defaults may be set with
`CF_MCP_LANE` and `MCP_PROTOCOL_VERSION`. Load and conformance use `--client-era`
as described below.

Add the global `--standalone` flag to run the external lane without any control
plane. Standalone mode starts Redis, the Rust dataplane, nginx, and Fast Time
for load or the conformance fixture for protocol checks. A harness-owned auth
service generates an ephemeral RSA key and
serves public JWKS on the dataplane network namespace's loopback interface. The
config helper signs test tokens and writes named MessagePack routing snapshots
directly to Redis. Production dataplane images work without `with_tools`; that
feature is only for testing the dataplane's optional administrative helpers.
The published helper image contains this Rust CLI. Explicit source builds use
its embedded sources and Docker build cache. JWT/JWKS and Redis configuration
run as private CLI commands. The tooling image also contains pinned upstream conformance
and Inspector packages; Docker caches their installation without using the host npm
cache. Authentication proxies and the Rust client driver run in that container,
which joins the stack network without a Docker socket mount. Reports are written
to the integration directory. Python remains for Locust and upstream live-test
integration.
Standalone commands also work from an installed binary without control-plane
checkouts or generated control-plane secrets.
Routes and tool schemas are discovered from every catalog page of the running
fixture, including the selected protocol era's diagnostic tools and prompts.

Control-plane-backed external runs configure RS256 signing automatically and
serve its matching public JWKS on loopback. The private key is shared only with
the control plane through a Compose volume; the dataplane never mounts it.
A local principal mapping uses the control-plane user UUID and a single harness
tenant. The control plane still issues and revokes managed tokens and publishes
user routing configurations. Standalone runs use the harness token issuer.
The CLI rejects incompatible publisher schemas before starting the workload;
client era selection cannot repair a control-plane/dataplane schema mismatch.

The CLI generates a strong local admin password in `CF_INTEGRATION_DIR/admin-password`
(mode `0600`) and reuses it across runs. `PLATFORM_ADMIN_PASSWORD` overrides it;
`DEFAULT_USER_PASSWORD` defaults to that effective password. Existing databases
need their original admin password. Fast Time registration logs in through the
control-plane API and never prints tokens. All load lanes use Locust 2.46.2.
Builtin load uses the control plane's performance nginx configuration; external
proxies pool upstream connections and refresh Docker DNS after backend restarts.
Load stops on the first request or user error, saves failure reports, and exits
nonzero. `locust.json` contains the final statistics even when an early failure
stops the CSV sampling loop. Run a smoke before starting a measured load.

Use `cf-integration <command> --help` for the complete interface.

## Stack

Use a persistent stack for manual testing:

```bash
cf-integration stack up --lane builtin --protocol-version modern
cf-integration stack up --lane external --protocol-version legacy --fresh
cf-integration stack up --lane external --protocol-version legacy --standalone

cf-integration stack status --lane external --standalone
cf-integration stack logs --lane external --standalone
cf-integration stack logs --lane external --standalone dataplane nginx
cf-integration stack config --lane external --standalone

cf-integration stack down --lane external --standalone
cf-integration stack down --lane all
cf-integration stack down --lane all --volumes
```

`up --fresh` and `down --volumes` remove the selected stack's volumes.
ClickStack has an independent lifecycle and is intentionally left running by
all stack and managed-test cleanup.

## Probe

Probe authentication, protocol lifecycle, backend identity, catalog selection,
and a safe routed tool call:

```bash
cf-integration probe --lane builtin --protocol-version modern
cf-integration probe --lane external --protocol-version legacy
cf-integration probe --lane external --protocol-version legacy --standalone
```

For standalone external runs, the known catalog comes from the mocked Redis
snapshot because the Rust dataplane intentionally does not implement fan-out
`tools/list`.

## Load

`run` uses the same command and era naming as conformance. It runs one routed
lane with one client era at a time.

```bash
# Compare both lanes for two minutes
cf-integration load run --lane builtin --client-era legacy \
  --users 10 --spawn-rate 2 --run-time 2m
cf-integration load run --lane external --client-era legacy \
  --users 10 --spawn-rate 2 --run-time 2m

# Isolate the external dataplane and mocked Redis
cf-integration load run --lane external --client-era legacy --standalone \
  --users 10 --spawn-rate 2 --run-time 2m

# Include telemetry when diagnostic value matters more than benchmark purity
cf-integration load run --lane external --client-era modern --standalone \
  --observability --users 10 --spawn-rate 2 --run-time 2m
```

`--smoke` selects a short workload. Durations accept ordered positive `h`, `m`,
and `s` groups such as `2m30s`. Defaults are `100` users, `10` users/s, and
`5m`, overridable with `LOCUST_USERS`, `LOCUST_SPAWN_RATE`, and
`LOCUST_RUN_TIME`. Observability is opt-in for load tests to avoid skew.

`--client-era` accepts `legacy` or `modern` (default). The harness owns the
Locust client: legacy uses initialization and the server's negotiated revision;
modern uses discovery and per-request metadata. Exact revision selectors and
`MCP_PROTOCOL_VERSION` overrides are not part of the load interface.

Every load lane uses the Fast Time server with the same `FAST_TIME_IMAGE`
override and the same `echo` payload (`{"message":"cf-integration"}`). Pin that
image to a digest when comparing lanes. The measured workload contains only
`tools/call`; initialization/discovery and builtin tool-name discovery happen
once per user. Compare the `MCP tools/call` statistics to exclude setup traffic.
A missing echo tool fails the run instead of producing an empty benchmark.

There is no server-era selector: the backend must support the selected client
era. `--standalone` runs Fast Time with the external dataplane and a harness
routing snapshot in Redis, without the control plane. It discovers Fast Time's
catalog directly; it never starts the conformance fixture or its proxy.
Conformance, probes, and Inspector retain their protocol fixtures.

## Live gateway checks

Groups are `mcp`, `rbac`, `protocol`, and `all` (default):

```bash
cf-integration live --lane builtin --protocol-version legacy --group all
cf-integration live --lane external --protocol-version modern --group all
cf-integration live --lane fixture-direct --protocol-version legacy --group protocol
```

Routed runs execute the upstream control-plane live suites. `live --standalone`
is unsupported because these suites require the control plane. Use
`cf-integration probe --lane external --standalone` for isolated authentication,
protocol, and routed tool-call checks.

## Conformance

`run` executes the pinned official MCP suite and compares results with checked-in
baselines. With no selectors it runs all three lanes with a modern client
against legacy and modern fixtures.

```bash
cf-integration conformance run

cf-integration conformance run \
  --lane fixture-direct --lane builtin --lane external \
  --client-era legacy --client-era modern \
  --server-era legacy --server-era modern

cf-integration conformance run --lane external --standalone \
  --client-era modern --server-era modern

cf-integration conformance run --lane external --standalone \
  --client-era modern --server-era modern --bless
```

`--client-era` and `--server-era` accept `legacy`, `modern`, or `dual`.
Client legacy runs select only `2025-11-25`; modern selects only `2026-07-28`;
dual selects both. Older client revisions are not run. Exact `--client-version`
selectors accept only those two revisions. Server era selects the fixture's
legacy/modern behavior; its pinned SDK determines the exact supported revisions,
which are listed in the run summary.
`--bless` replaces only the selected baselines and only after every selected
run succeeds. `--standalone` permits the external lane only.
Ctrl-C finishes cleanup for the active run, skips the remaining matrix entries,
and leaves baselines unchanged.

Regenerate Markdown from existing results without running tests:

```bash
cf-integration conformance report
cf-integration conformance report \
  --results-dir .integration/conformance --output-dir reports/conformance
```

## Debug

```bash
cf-integration debug inspect --lane external \
  --protocol-version legacy --method tools/list
cf-integration debug inspect --lane builtin \
  --protocol-version legacy --server-id <virtual-server-id>

cf-integration debug token --kind scoped
cf-integration debug token --kind scoped --server-id <virtual-server-id>
cf-integration debug token --kind admin

# Issue a token from an already-running standalone external stack
cf-integration debug token --kind scoped --standalone
```

`inspect` uses the official MCP Inspector in Docker. The pinned Inspector uses
initialization, so select `--protocol-version legacy`. Control-plane tokens are revoked when
the workflow owns them; caller-supplied `MCPGATEWAY_BEARER_TOKEN` values are
never revoked.

## Observability and artifacts

ClickStack starts by default for stack, probe, live, conformance, and Inspector
workflows. Open the no-login HyperDX UI at <http://127.0.0.1:3000>. Logs open by
default; for metrics use **Chart Explorer**, choose the **Metrics** source and a
metric such as `http.server.request.duration`, then run the query. Allow at
least 60 seconds of traffic for multiple 30-second cumulative exports.
Telemetry storage is ephemeral inside ClickStack.

Load reports are written below
`CF_INTEGRATION_DIR/reports/load/<client-era>/<lane>/locust`, using lane names
`builtin` and `external`. Conformance
results below `CF_INTEGRATION_DIR/conformance`, and comparison Markdown below
`reports/conformance`. Copy [`.env.example`](.env.example) for the complete
configuration list. Process environment values override `.env`.
