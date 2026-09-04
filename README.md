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
requires Docker Compose v2, Git, and Node.js 22.7.5 or newer. Rust 1.97 is
needed only to compile the CLI or a local dataplane image.

Published images are the default. Set `CF_DATAPLANE_REF` to build and test a
local dataplane ref.

## Selection

Routed commands accept `--lane builtin|external`; `external` is the default.
Conformance and protocol-only live tests also accept `fixture-direct`.
`stack down` accepts `all`. No command accepts the old `--topology` option.

Commands that exercise MCP accept `--protocol-version modern|legacy`:

- `modern`: current per-request, stateless MCP.
- `legacy`: current initialization-based MCP.

The CLI deliberately does not expose dated wire revisions. Defaults may be set
with `CF_MCP_LANE` and `MCP_PROTOCOL_VERSION`.

Add the global `--standalone` flag to run the external lane without any control
plane. Standalone mode starts Redis, the Rust dataplane, nginx, and the required
test fixture. It generates an ephemeral RSA key, obtains a test token from the
dataplane's local tool endpoint, validates it through the dataplane's loopback
JWKS endpoint, and publishes a fresh config through the dataplane serializer.
Redis therefore always contains the schema understood by the image under test.

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

```bash
# Compare both lanes for two minutes
cf-integration load --lane builtin --protocol-version legacy \
  --users 10 --spawn-rate 2 --run-time 2m
cf-integration load --lane external --protocol-version legacy \
  --users 10 --spawn-rate 2 --run-time 2m

# Isolate the external dataplane and mocked Redis
cf-integration load --lane external --protocol-version legacy --standalone \
  --users 10 --spawn-rate 2 --run-time 2m

# Include telemetry when diagnostic value matters more than benchmark purity
cf-integration load --lane external --protocol-version modern --standalone \
  --observability --users 10 --spawn-rate 2 --run-time 2m
```

`--smoke` selects a short workload. Durations accept ordered positive `h`, `m`,
and `s` groups such as `2m30s`. Defaults are `100` users, `10` users/s, and
`5m`, overridable with `LOCUST_USERS`, `LOCUST_SPAWN_RATE`, and
`LOCUST_RUN_TIME`. Observability is opt-in for load tests to avoid skew.

## Live gateway checks

Groups are `mcp`, `rbac`, `protocol`, and `all` (default):

```bash
cf-integration live --lane builtin --protocol-version legacy --group all
cf-integration live --lane external --protocol-version modern --group all
cf-integration live --lane external --protocol-version legacy --group all --standalone
cf-integration live --lane fixture-direct --protocol-version legacy --group protocol
```

Normal routed runs execute the upstream control-plane live suites. Standalone
external runs execute the dataplane-native route/auth/protocol contract, with
no control-plane checkout or API calls.

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
`--bless` replaces only the selected baselines and only after every selected
run succeeds. `--standalone` permits the external lane only.

Regenerate Markdown from existing results without running tests:

```bash
cf-integration conformance report
cf-integration conformance report \
  --results-dir .integration/conformance --output-dir reports/conformance
```

## Debug

```bash
cf-integration debug inspect --lane external \
  --protocol-version modern --method tools/list
cf-integration debug inspect --lane builtin \
  --protocol-version legacy --server-id <virtual-server-id>

cf-integration debug token --kind scoped
cf-integration debug token --kind scoped --server-id <virtual-server-id>
cf-integration debug token --kind admin

# Issue a token from an already-running standalone external stack
cf-integration debug token --kind scoped --standalone
```

`inspect` uses the official MCP Inspector. Control-plane tokens are revoked when
the workflow owns them; caller-supplied `MCPGATEWAY_BEARER_TOKEN` values are
never revoked.

## Observability and artifacts

ClickStack starts by default for stack, probe, live, conformance, and Inspector
workflows. Open the no-login HyperDX UI at <http://127.0.0.1:3000>. Logs open by
default; for metrics use **Chart Explorer**, choose the **Metrics** source and a
metric such as `http.server.request.duration`, then run the query. Allow at
least 60 seconds of traffic for multiple 30-second cumulative exports.
Telemetry storage is ephemeral inside ClickStack.

Load reports are written below `CF_INTEGRATION_DIR/reports/load`, conformance
results below `CF_INTEGRATION_DIR/conformance`, and comparison Markdown below
`reports/conformance`. Copy [`.env.example`](.env.example) for the complete
configuration list. Process environment values override `.env`.
