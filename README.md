# cf-integration

`cf-integration` runs ContextForge stacks and tests against the built-in Python
dataplane or the external Rust dataplane. It manages Docker Compose, source
checkouts, MCP probes, Locust load tests, upstream live tests, and official MCP
conformance runs.

`/servers/{virtual_host_id}/mcp` routes through `cf-dataplane`; raw `/mcp`,
UI, and API traffic route to `cf-controlplane`. The external dataplane fails
closed and never falls back to the built-in dataplane.

## Install and requirements

```bash
cargo binstall cf-integration
# or
cargo install cf-integration --locked
```

To run the current checkout, put `cargo run --` before any command:

```bash
cargo run -- probe --lane external --protocol-version modern
```

Runtime requirements are Docker with Compose v2, Git, and Node.js 22.7.5 or
newer with `npx`. Load tests also need the control-plane checkout's Python and
Locust dependencies. Rust 1.97 is required only to compile the CLI or a local
source image.

Published images are used by default. Set `CF_DATAPLANE_REF` to build an
external dataplane source ref.

## Common selectors

Wherever `--lane` is accepted, use these values:

- `builtin`: Python built-in dataplane.
- `external`: external Rust dataplane.
- `fixture-direct`: reference fixture without ContextForge; available only to
  conformance and `live --group protocol`.

Routed commands default to `CF_MCP_LANE`, then `external`, and run one lane
at a time. Run them once per lane when comparing `builtin` and `external`.
`stack down` also accepts `all`; conformance accepts repeated `--lane`
options. No command accepts `--topology`.

`probe`, `load`, `live`, and `debug inspect` accept
`--protocol-version modern|legacy`:

- `modern`: latest per-request, stateless MCP revision.
- `legacy`: latest initialization-based MCP revision.

The default is `MCP_PROTOCOL_VERSION`, then `modern`. Dated revisions are
internal wire values, not operational CLI options.

Use `--help` at any level for the authoritative interface, such as
`cf-integration stack --help` or `cf-integration load --help`.

## Commands

Test workflows prepare and clean up their required stack. Use `stack` when you
want a persistent stack for manual work.

### `stack`

```bash
# Start one lane
cf-integration stack up --lane builtin
cf-integration stack up --lane external --fresh

# Inspect one lane
cf-integration stack status --lane external
cf-integration stack logs --lane external
cf-integration stack logs --lane external nginx
cf-integration stack config --lane external

# Stop one or both lanes
cf-integration stack down --lane builtin
cf-integration stack down --lane all
cf-integration stack down --lane all --volumes
```

`up --fresh` removes existing volumes before starting. `logs` follows all
services unless service names are supplied. `config` prints merged Compose
configuration. `down --volumes` also removes persistent volumes.

ClickStack starts by default for `stack`, `probe`, `live`, `conformance`, and
`debug inspect`. Open the no-login HyperDX UI at <http://127.0.0.1:3000> to
inspect traces and metrics. Managed test cleanup leaves ClickStack running, so
the UI remains available after a command finishes; `stack down --lane all`
removes it. The external dataplane exports the exact
HTTP counters, latency histograms, in-flight gauge, and body sizes recorded by
its `HttpMetricsLayer`; allow 30 seconds for its first export. All telemetry
storage is ephemeral and disappears when ClickStack is removed.

### `probe`

Probe one public MCP route, including discovery or initialization,
`tools/list`, a safe `tools/call`, authentication, and backend identity.

```bash
cf-integration probe [--lane builtin|external] \
  [--protocol-version modern|legacy]
```

### `load`

Run Locust against one public MCP route:

```bash
cf-integration load [--lane builtin|external] \
  [--protocol-version modern|legacy] [--standalone] [--smoke] \
  [--observability] [--users N] [--spawn-rate N] [--run-time DURATION]

# Compare both lanes for two minutes
cf-integration load --lane builtin --protocol-version legacy \
  --users 10 --spawn-rate 2 --run-time 2m
cf-integration load --lane external --protocol-version legacy \
  --users 10 --spawn-rate 2 --run-time 2m

# Measure only the external dataplane request path
cargo run -- load --lane external --protocol-version legacy --standalone \
  --users 10 --spawn-rate 2 --run-time 2m

# Inspect traces and native HTTP metrics while using the mocked Redis snapshot
cargo run -- load --lane external --protocol-version legacy --standalone \
  --observability --users 10 --spawn-rate 2 --run-time 2m
```

`--smoke` selects a short smoke workload. Duration accepts positive `h`,
`m`, and `s` groups such as `2m30s` or `1h30m`. Defaults come from
`LOCUST_USERS`, `LOCUST_SPAWN_RATE`, and `LOCUST_RUN_TIME`.
Observability is disabled for load tests by default to avoid skewing
performance results; pass `--observability` when diagnostics are more important
than an uncontaminated benchmark.

`--standalone` is valid only with `--lane external`. Each run starts the full
stack to issue a scoped token, starts an isolated current-protocol MCP fixture,
then stops the control-plane gateway before Locust begins. A fresh mock config
for the token subject is written through the running dataplane's own serializer
on every run, so Redis receives the dataplane's current MessagePack schema. The
snapshot is non-expiring for the load duration; traffic does not depend on the
control-plane publisher or its schema-sync timing.

### `live`

Run the managed upstream control-plane test groups: `mcp` for Fast Time MCP
routes, `rbac` for authorization and transports, `protocol` for
protocol-specific behavior, or `all` (the default).

```bash
cf-integration live [--lane fixture-direct|builtin|external] \
  [--protocol-version modern|legacy] [--group mcp|rbac|protocol|all]

cf-integration live --lane builtin --protocol-version legacy --group all
cf-integration live --lane fixture-direct \
  --protocol-version legacy --group protocol
```

### `conformance`

`run` executes the pinned official suite and compares it with checked-in
baselines. With no options it runs all three lanes using a modern client against
legacy and modern fixture servers.

```bash
cf-integration conformance run

# Repeat selectors to build a matrix
cf-integration conformance run \
  --lane fixture-direct --lane builtin --lane external \
  --client-era legacy --client-era modern \
  --server-era legacy --server-era modern

# Replace selected baselines only after every selected run succeeds
cf-integration conformance run --server-era dual --bless
```

`--client-era` and `--server-era` accept `legacy`, `modern`, or `dual`.
`--results-dir`, `--baseline-dir`, and `--output-dir` override artifact
locations.

`report` regenerates Markdown comparisons from existing results without
running the suite:

```bash
cf-integration conformance report
cf-integration conformance report \
  --results-dir .integration/conformance --output-dir reports/conformance
```

### `debug`

`inspect` runs an MCP Inspector method against one routed lane. The method
defaults to `tools/list`, and the server defaults to the Fast Time fixture.

```bash
cf-integration debug inspect --lane external \
  --protocol-version modern --method tools/list
cf-integration debug inspect --lane builtin \
  --protocol-version legacy --server-id <virtual-server-id>
```

`token` prints a token from an already-running control plane. `scoped`
creates the minimum catalog token used by public MCP tests; `admin` creates a
platform-admin session token. `--server-id` is valid only for `scoped`.

```bash
cf-integration debug token --kind scoped
cf-integration debug token --kind scoped --server-id <virtual-server-id>
cf-integration debug token --kind admin
```

## Configuration and artifacts

Copy `.env.example` to `.env`; process environment values override it.

| Variable | Purpose | Default |
| --- | --- | --- |
| `CF_MCP_LANE` | Routed lane | `external` |
| `MCP_PROTOCOL_VERSION` | Protocol mode | `modern` |
| `CF_INTEGRATION_DIR` | Checkouts, state, and load reports | `.integration` |
| `CF_DATAPLANE_REF` | Optional local dataplane Git ref | unset |
| `LOCUST_*` | Users, spawn rate, and duration | `100`, `10`, `5m` |

See [`.env.example`](.env.example) for every setting. Missing Compose secrets
are generated under `CF_INTEGRATION_DIR`. Workflow-created tokens are revoked
during cleanup; a caller-supplied `MCPGATEWAY_BEARER_TOKEN` is never revoked.

Installed binaries embed their runtime assets. Set `CF_INTEGRATION_ROOT` to
force a developer checkout. Load reports default below
`CF_INTEGRATION_DIR/reports/load`; conformance results below
`CF_INTEGRATION_DIR/conformance`; and conformance Markdown below
`reports/conformance`.
