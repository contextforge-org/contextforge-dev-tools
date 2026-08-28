# cf-integration

`cf-integration` is the standalone Rust CLI for exercising `cf-controlplane`
with either its built-in Python data plane or the external Rust
`cf-dataplane`.

The routing contract is fixed:

- `/servers/{virtual_host_id}/mcp` routes through `cf-dataplane`.
- Raw `/mcp`, UI, and API traffic route to `cf-controlplane`.
- The external dataplane fails closed and never falls back to the
  control plane.

The CLI owns Docker Compose overlays, nginx routing, source checkout
orchestration, MCP probes, Locust load tests, upstream live tests, and official
MCP conformance runs.

## Install

Release archives cover ARM64 and x86-64 Linux, macOS, and Windows.

```bash
cargo binstall cf-integration
cf-integration --help
```

To compile from crates.io or this checkout:

```bash
cargo install cf-integration --locked
cargo install --path . --locked
```

The installed binary is repository-independent. Required Compose overlays,
runtime scripts, and conformance baselines are embedded in the executable.

## Runtime assets and workspace resolution

The CLI resolves the action before initializing state. Runtime-backed actions
resolve assets in this order:

1. explicit `CF_INTEGRATION_ROOT`, which must be a valid developer checkout;
2. the current directory, when it contains a valid checkout;
3. a versioned embedded-asset tree beneath `CF_INTEGRATION_DIR`.

Embedded assets are materialized atomically, verified byte-for-byte, marked
read-only, and reused. Concurrent first runs converge on one complete tree. A
corrupt or incomplete versioned tree fails closed.

`.env` is loaded from `CF_INTEGRATION_ROOT` when set, otherwise the current
directory. Relative paths resolve from that workspace. Generated checkouts,
assets, secrets, reports, and runtime state default to `.integration/`.

`conformance report` and `debug token` do not materialize assets or generate
local Compose secrets. Compose-backed actions initialize them lazily.

## Requirements

Runtime requirements depend on the command:

- Docker Engine with Docker Compose v2 for stack-backed workflows;
- Git for managed source checkouts;
- Node.js 22.7.5 or newer with `npx` for Inspector, live, and conformance;
- the control-plane checkout's Python/Locust dependencies for load tests;
- Rust 1.97 only when compiling the CLI or local source images.

Published control-plane and data-plane images are used by default. Local
data-plane builds require an explicit `CF_DATAPLANE_REF`.

## CLI

```text
cf-integration
├── stack
│   ├── up
│   ├── down
│   ├── status
│   ├── logs
│   └── config
├── probe
├── load
├── live
├── conformance
│   ├── run
│   └── report
└── debug
    ├── inspect
    └── token
```

Use `--help` at any level for the authoritative interface.

Every resolved command reports its lifecycle on standard error using the same
description: `⠋` while active, `✓` in green on success, and `✗` in red on
failure. Test results use aligned nextest-style labels: green `PASS`, yellow
`XFAIL`, red `XPASS` and `FAIL`, and yellow `SKIP`. `NO_COLOR` and
`CARGO_TERM_COLOR` control ANSI output. Command data such as tokens, Compose
configuration, and report paths remains on standard output for scripting.

Stack commands use physical `--topology controlplane|dataplane`:

```bash
cf-integration stack up --topology dataplane
cf-integration stack up --topology dataplane --fresh
cf-integration stack status --topology dataplane
cf-integration stack config --topology dataplane
cf-integration stack down --topology all
cf-integration stack down --topology all --volumes
```

`stack down --volumes` is the explicit destructive reset. Managed workflows
preserve the primary failure, attempt every token and stack cleanup, and report
all cleanup failures.

Probe, load, and Inspector use physical lanes:

```bash
cf-integration probe --lane dataplane --protocol-version 2026-07-28
cf-integration load --lane dataplane --smoke
cf-integration debug inspect --lane dataplane --method tools/list
```

Live and conformance share semantic lanes: `fixture-direct`,
`built-in-data-plane`, and `external-data-plane`.

```bash
cf-integration live --lane external-data-plane --group mcp
cf-integration live --lane fixture-direct --group protocol \
  --protocol-version 2025-06-18

cf-integration conformance run
cf-integration conformance run \
  --protocol-version 2025-11-25 \
  --protocol-version 2026-07-28 \
  --server-era legacy \
  --server-era modern
cf-integration conformance run --server-era dual --bless
cf-integration conformance report
cf-integration --version
```

Workflows accept only `--lane`; `--topology` is reserved for stack commands.
The direct fixture spelling is only `fixture-direct`. Protocol selection is
only `--protocol-version`.

## MCP and conformance behavior

One MCP client owns endpoint construction, authorization, sessions, stateful
and stateless headers, JSON/SSE parsing, backend identity validation, response
limits, timeouts, and secret redaction.

The session-oriented probe performs initialize, `notifications/initialized`,
`tools/list`, and one safe `tools/call`. The stateless probe performs
`server/discover`, attaches `Mcp-Method` and `Mcp-Name` routing headers, and
performs the same safe checks without a session. Both verify unauthenticated
rejection and external dataplane backend identity.

The official runner is pinned to
`@modelcontextprotocol/conformance@0.2.0-alpha.11`. Its TypeScript fixture is
built from revision `c321dd32035556e6769d3724a8ee97d87c3faaac`. A default run
starts workflow-owned stacks and runs both conformance directions. The server
suite sends the official client directly to the fixture and through the
built-in and external dataplane routes. For protocol `2026-07-28`, the client
suite also makes the external dataplane send requests to the official scenario
servers. The four downstream scenarios are `tools_call`, `request-metadata`,
`http-standard-headers`, and `http-custom-headers`; they run automatically
whenever `external-data-plane` is selected. The workflow records raw official
results without suppression, writes deterministic comparisons, and continues
through every selected client-version/server-era combination before returning
one aggregated result. `dual` is supported only when selected explicitly.

The client protocol and fixture server era are independent:

```bash
cf-integration conformance run \
  --protocol-version 2025-11-25 \
  --protocol-version 2026-07-28 \
  --server-era legacy \
  --server-era modern
```

The repeated client and server selections form a Cartesian product. Artifacts
default below `CF_INTEGRATION_DIR/conformance/<client-version>/<server-era>/`
and reports below `reports/conformance/<client-version>/<server-era>/`.
Server artifacts retain the lane directly below the era. Client artifacts and
reports use `client/external-data-plane/` below the era. `--results-dir`,
`--baseline-dir`, and `--output-dir` override those roots.

Baselines use this strict layout:

```text
tests/conformance/baselines/
  <client-version>/
    <server-era>/
      fixture-direct.yml
      built-in-data-plane.yml
      external-data-plane.yml
      client/
        external-data-plane.yml
```

Each file contains sorted `FAILURE` and `WARNING` check identities. They are
required to distinguish expected failures from regressions and are embedded
for installed binaries. Every completed lane is printed in nextest style even
when a later lane fails operationally. The direct fixture is gated
independently; findings reproduced there are subtracted from routed lanes
before server comparison. Client findings are gated independently without
fixture subtraction. Unexpected, stale, unknown, malformed, incomplete,
missing, and operational results fail the matrix. `--bless` replaces all
selected server and client baselines in one directory transaction only after
every combination succeeds. Outside a developer checkout, an omitted
`--baseline-dir` writes blessed baselines beneath the current workspace rather
than modifying embedded assets. Server comparison regeneration discovers every
protocol/era partition beneath the selected result root and accepts
`--results-dir` and `--output-dir`.

## Canonical configuration

Copy `.env.example` to `.env`. Process values override the file.

```bash
CF_INTEGRATION_ROOT=/path/to/contextforge-dev-tools
CF_INTEGRATION_DIR=.integration
CF_MCP_STACK_MODE=dataplane

CF_CONTROLPLANE_REPO=https://github.com/IBM/mcp-context-forge.git
CF_CONTROLPLANE_REF=main
CF_CONTROLPLANE_VERSION=main

CF_DATAPLANE_REPO=https://github.com/contextforge-org/contextforge-data-plane.git
CF_DATAPLANE_REF=
CF_DATAPLANE_IMAGE=ghcr.io/contextforge-org/contextforge-data-plane:latest
CF_DATAPLANE_PLATFORM=auto

CF_COMPOSE_BUILD=auto
CF_FAST_TIME_EXPECTED_IMAGE=ghcr.io/ibm/cfex-mcp-fast-time-server:latest
CF_FAST_TIME_SERVER_ID=9779b6698cbd4b4995ee04a4fab38737

MCP_CLI_BASE_URL=http://127.0.0.1:8080
MCP_PROTOCOL_VERSION=2026-07-28
MCP_SERVER_ID=9779b6698cbd4b4995ee04a4fab38737

PLATFORM_ADMIN_EMAIL=admin@example.com
PLATFORM_ADMIN_PASSWORD=<local-integration-password>
MCPGATEWAY_BEARER_TOKEN=<optional-pre-minted-token>
```

`CF_COMPOSE_BUILD=auto` pulls or reuses prebuilt images and builds only an
explicit source data plane when required. `true` always builds; `false` never
builds. Published mode tracks both repositories' main-branch images. The
dataplane uses its floating `:latest` tag. The control plane uses the
commit-tagged image for the freshly fetched `origin/main` revision because
upstream reserves `:latest` for releases. Stack startup pulls changes;
incompatible main images make the workflow fail instead of selecting an older
pair.

Compose requires `JWT_SECRET_KEY` and `AUTH_ENCRYPTION_SECRET`. If either is
unset, a runtime-backed action generates stable values under
`CF_INTEGRATION_DIR`. Canonical configuration is exported internally as the
upstream Compose adapter names `IMAGE_LOCAL` and `FAST_TIME_IMAGE`; those names
are not accepted as inputs.

Without `MCPGATEWAY_BEARER_TOKEN`, dataplane workflows issue a one-day
server-scoped catalog token and revoke it during session cleanup. A caller
supplied token is never revoked by the harness.

## Package layout

One root package publishes exactly one binary, `cf-integration`. All concern
modules remain private implementation details:

```text
src/infrastructure/       config, assets, processes, checkouts, Compose plans
src/mcp/                  unified MCP client, protocol, auth proxy, probe
src/conformance/          fixture, strict baselines, results, comparisons
src/performance/          Locust settings, commands, and report auditing
src/runtime/live/         upstream live-test workflow
src/runtime/stack/        stack lifecycle and source ownership
src/runtime/conformance/  conformance orchestration and reports
src/runtime/performance/  performance workflow orchestration
src/runtime/probe.rs       probe workflow orchestration
src/runtime/session.rs     shared managed stack and credential scope
src/runtime/mod.rs         thin action dispatcher
docker/                   embedded Compose and nginx assets
scripts/                  embedded runtime adapters
tests/conformance/        embedded expected-result baselines
```

The Bruno collection under `manual-tests/mcp-manual-test-tools/` is an
intentional lower stack layer for manual diagnosis. It remains in the
repository and is excluded from the published crate payload.

## Development and release

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo package --locked
```

Pull requests run this quality gate plus native tests on Linux, macOS, and
Windows. Releases build and smoke-test all six ARM64/x86-64 Linux, macOS, and
Windows candidates before publishing the crate or tag. Prevalidated archives,
SHA-256 files, and GitHub artifact attestations are published afterward.
