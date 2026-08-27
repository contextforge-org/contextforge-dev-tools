# cf-integration

Rust 1.97 integration harness for `cf-controlplane` and the Rust
`cf-dataplane`.

The public routing contract is fixed:

- `/servers/{virtual_host_id}/mcp` routes through `cf-dataplane` as
  `/contextforge-rs/servers/{virtual_host_id}/mcp`.
- Raw `/mcp`, UI traffic, and API traffic stay on `cf-controlplane`.

The `/servers/{id}/mcp` route does not fall back to the Python control plane on
dataplane errors. This makes routing failures visible and keeps the harness
aligned with the planned split between legacy slow-path traffic and modern
Rust dataplane traffic.

The harness owns Docker Compose overlays, nginx routing, reproducible stack
lifecycle, public-route probes, Locust load tests, and official MCP
conformance orchestration. Generated checkout, build, and runtime state stays
under `.integration/` or `CF_INTEGRATION_DIR`.

## Requirements

- Rust 1.97 or newer and Cargo
- Docker Engine with Docker Compose v2
- Git
- Node.js 22.7.5 or newer with `npx`
- Python and Locust dependencies from the control-plane checkout when using
  the Locust load engine
- The control-plane development prerequisites (`uv`, pytest, Make, and
  Playwright where required) when running upstream live tests

The checked-in `rust-toolchain.toml` selects Rust 1.97.0 with rustfmt and
Clippy. Install the locked CLI from this checkout with:

```bash
rustup toolchain install 1.97.0 --profile minimal -c clippy -c rustfmt
cargo install --path . --locked
cf-integration --help
```

Cargo places the executable in `$CARGO_HOME/bin` (normally `~/.cargo/bin`).
Re-run the install command after updating the checkout.

## Workspace

The workspace has one application package and four internal libraries:

- `cf-integration`: CLI and workflow composition
- `cf-integration-platform`: configuration, processes, checkouts, Compose, and
  stack lifecycle
- `cf-integration-mcp`: MCP messages, HTTP transport, authentication proxy,
  gateway endpoints, and probes
- `cf-integration-compliance`: the official conformance fixture, result parser,
  and three-lane comparison report
- `cf-integration-load`: Locust load orchestration

The official TypeScript fixture is the conformance reference target. An
explicit `stack up` starts it for direct MCP access; conformance runs still own
their isolated fixture lifecycle. Fast Time remains the ordinary probe and load
fixture, and upstream live MCP tests start and register the profile-gated Fast
Test server on demand.

## Lanes and protocol versions

Probe, load, live, and Inspector use the same target options:
`--lane controlplane|dataplane` and `--protocol-version YYYY-MM-DD`.
`controlplane` targets the stock control-plane topology and raw `/mcp`;
`dataplane` targets nginx, the Rust dataplane, and the virtual-server route.

Single-lane commands resolve their lane in this order:

1. explicit `--lane`;
2. `CF_MCP_STACK_MODE`;
3. `dataplane`.

They resolve the protocol version from explicit `--protocol-version`, then
`MCP_PROTOCOL_VERSION`, then `2025-11-25`. That session-oriented default is
the working contract of the current `latest` dataplane image. Pass
`--protocol-version 2026-07-28` explicitly to exercise the implemented
stateless readiness path as the future architecture lands. Live protocol tests
and conformance also accept `fixture-direct`; other workflows reject it
because they have no direct-fixture execution path. Conformance defaults to
all three lanes and its pinned `2026-07-28` protocol version.

`--topology` remains a compatibility alias for `--lane` on workflows.
Conformance also retains `--client-version` and `--spec-version` as aliases for
`--protocol-version`. Stack lifecycle commands continue to use `--topology`
because they operate on physical stacks, not test lanes.

## Quick start

Probe the dataplane public MCP route:

```bash
cf-integration probe --lane dataplane
```

`stack up` synchronizes the required source checkouts, validates the Compose
contract, resolves local builds or published images, starts the selected
topology, and waits for its public endpoint. It preserves existing volumes by
default. Use `--fresh` when state must be discarded.

Probe, load, routed live-test, and Inspector commands start their selected
stack, wait for the fixture to be ready, and stop the stack when the command
succeeds or fails. The direct live fixture lane does not start a stack.
Explicit `stack` commands remain available when a persistent environment is
needed.

The Fast Time backend is registered as virtual server
`9779b6698cbd4b4995ee04a4fab38737`, so probe and load commands need no manual
UI setup.

## Manual Bruno tests

The vendored Bruno workspace under
`manual-tests/mcp-manual-test-tools/` provides requests for manually exercising
MCP gateway and server flows. Open that directory as a workspace in Bruno,
select an environment, and set a fresh token where the selected flow requires
authentication.

The collection was imported from
[`lucarlig/mcp-manual-test-tools`](https://github.com/lucarlig/mcp-manual-test-tools).
Its exact source revision is recorded in the vendored directory's
`UPSTREAM.md`.

## CLI

The public CLI contains only distinct workflows:

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

Use `--help` at any level for the authoritative flags.

### Stack lifecycle

```bash
cf-integration stack up --topology dataplane
cf-integration stack up --topology dataplane --fresh
cf-integration stack down --topology all
cf-integration stack down --topology all --volumes
cf-integration stack status --topology dataplane
cf-integration stack logs --topology dataplane cf-nginx cf-dataplane
cf-integration stack config --topology dataplane
```

`stack down --volumes` is the explicit destructive cleanup operation.
Diagnostic commands use the harness Compose project and overlays so callers do
not need to reconstruct its Compose invocation. The dataplane topology defaults
to the `cf` Compose project, so Docker resources use the `cf-*` prefix.
Container viewers expose concise `cf-*` display names, and `stack logs` accepts
those names while translating them to the underlying Compose service keys.

After readiness succeeds, `stack up` prints the public gateway/API origin, the
mode-correct public MCP endpoint, and the direct loopback address of the pinned
conformance server. Its host port is assigned by Docker and can change after a
fresh start.

### Probe

```bash
cf-integration probe --lane dataplane
```

The modern dataplane probe checks unauthenticated rejection,
`server/discover`, required per-request metadata and routing headers,
`tools/list`, and one known-safe `tools/call` without creating a session. The
legacy control-plane probe retains initialize, `notifications/initialized`,
and session reuse. It targets `/mcp` in controlplane topology and
`/servers/{id}/mcp` in dataplane topology.

### Locust

The load workflow exercises the MCP lifecycle through the framework-required
Python Locust adapter:

```bash
cf-integration load --lane dataplane \
  --smoke

cf-integration load --lane dataplane \
  --users 20 --spawn-rate 5 --run-time 2m
```

Default full-run settings are 100 users, 10 users/second, and five minutes.
CLI settings override `.env`; explicitly exported `LOCUST_USERS`,
`LOCUST_SPAWN_RATE`, and `LOCUST_RUN_TIME` remain authoritative. Smoke defaults
are one user, one user/second, and ten seconds.

On the modern dataplane lane Locust uses `server/discover`, attaches the
mandatory client `_meta` plus `Mcp-Method`/`Mcp-Name` headers to every request,
and avoids sessions and the removed `ping` method. The legacy control-plane
lane retains initialize, `notifications/initialized`, session cleanup, and
ping. The adapter calls only a finite allowlist of safe fixture tools and audits
generated artifacts for credential leakage.

### Upstream live tests

Run the control-plane repository's live gateway tests against either topology:

```bash
cf-integration live --lane dataplane --group mcp
cf-integration live --lane dataplane --group rbac
cf-integration live --lane dataplane --group protocol
cf-integration live --lane dataplane --group all

# Run the upstream protocol suite directly against its reference fixture.
cf-integration live \
  --lane fixture-direct \
  --group protocol \
  --protocol-version 2025-06-18
```

`--group all` is the exact union of the `mcp`, `rbac`, and `protocol` groups.
Upstream plugin and SSO suites are excluded because this harness does not
start their additional services.

The `mcp` and `all` groups start the upstream profile-gated `fast_test_server`,
run its one-shot registration job, and, for the dataplane topology, wait until
the publisher snapshot contains its fixed virtual server before launching the
tests. The base stack remains unchanged when other workflows run.

`--lane fixture-direct` is valid with `--group protocol` and runs the upstream
`test-protocol-compliance-reference` target without a gateway stack. The
selected date-formatted version is applied to MCP SDK initialization, and the
live run fails with the installed SDK's supported-version list when that SDK
cannot emit it.

## Official MCP conformance

The official runner is pinned to
`@modelcontextprotocol/conformance@0.2.0-alpha.11`. The official TypeScript
fixture is built from matching source revision
`c321dd32035556e6769d3724a8ee97d87c3faaac`.

The default command is intentionally complete and reproducible:

```bash
cf-integration conformance run
```

It always:

- starts fresh stacks owned by the conformance workflow;
- provisions the pinned official fixture;
- runs every applicable official server scenario;
- defaults to MCP `2026-07-28`;
- runs fixture-direct, controlplane, and dataplane lanes;
- passes an empty expected-failure file to the official runner;
- records raw failures without suppression;
- removes temporary API resources, fixture services, and stacks;
- writes a comparison report even when a lane reports protocol failures.

The official runner's protocol version and the upstream fixture's server era
are independent. The fixture defaults to `--server-era dual`, preserving the
existing behavior where it selects the matching lifecycle from the incoming
request.

Run the same-era baselines explicitly:

```bash
cf-integration conformance run \
  --protocol-version 2026-07-28 \
  --server-era modern
cf-integration conformance run \
  --protocol-version 2025-11-25 \
  --server-era legacy
```

Run the two cross-era paths:

```bash
# Modern client-facing traffic against a legacy-only upstream.
cf-integration conformance run \
  --protocol-version 2026-07-28 \
  --server-era legacy

# Legacy client-facing traffic against a modern-only upstream.
cf-integration conformance run \
  --protocol-version 2025-11-25 \
  --server-era modern
```

In a cross-era run, the fixture-direct lane is the expected incompatible
baseline. A routed lane that passes where fixture-direct fails demonstrates
that the gateway adapted the lifecycle across the boundary; the comparison
report records both axes. The official runner emits the selected client era
strictly. It does not itself test a general-purpose SDK client's automatic
dual-era fallback.

The three lanes are:

1. official oracle directly to the official TypeScript fixture;
2. official oracle through the control-plane public MCP route;
3. official oracle through nginx and the Rust dataplane route.

Select exact lanes by repeating `--lane`:

```bash
cf-integration conformance run \
  --lane fixture-direct \
  --lane dataplane
```

Supported client revisions are explicit and use the same pinned runner and
fixture:

```bash
cf-integration conformance run --protocol-version 2025-11-25
cf-integration conformance run --protocol-version 2025-06-18
```

Artifacts default below `CF_INTEGRATION_DIR`. Use `--results-dir` to place them
elsewhere. Regenerate only the official comparison report with:

```bash
cf-integration conformance report
cf-integration conformance report \
  --results-dir /path/to/results \
  --output-dir /path/to/reports
```

The official runner has no bearer-header option. The harness therefore uses a
random-path loopback proxy that injects authorization while keeping tokens out
of process arguments. Automatic fixture provisioning requires a loopback
`MCP_CLI_BASE_URL`.

## Debug utilities

Debug commands are useful for manual diagnosis but are not compliance gates.

```bash
cf-integration debug inspect \
  --lane dataplane \
  --method tools/list

cf-integration debug token \
  --kind scoped \
  --server-id <virtual-server-id>

cf-integration debug token --kind admin
```

Token generation now authenticates against a running control plane using
`PLATFORM_ADMIN_EMAIL` and `PLATFORM_ADMIN_PASSWORD`. Scoped debug tokens
are catalog-backed, restricted to the selected virtual server, expire after
one day, and are intentionally left active for manual use.

Inspector is pinned to `@modelcontextprotocol/inspector@2.2.0` and uses the
same loopback authentication proxy as conformance. Select `2026-07-28` to use
its modern MCP SDK path for stateless dataplane requests.

## Configuration

Copy `.env.example` to `.env`. Shell variables override `.env`, and relative
paths resolve from the repository root.

Common settings:

```bash
CF_MCP_STACK_MODE=dataplane
CF_INTEGRATION_DIR=.integration

CF_CONTROLPLANE_REPO=https://github.com/IBM/mcp-context-forge.git
CF_CONTROLPLANE_REF=v1.0.7
CF_CONTROLPLANE_IMAGE=ghcr.io/ibm/mcp-context-forge:latest
CF_CONTROLPLANE_VERSION=latest

CF_DATAPLANE_REPO=https://github.com/contextforge-org/contextforge-data-plane.git
CF_DATAPLANE_REF=
CF_DATAPLANE_IMAGE=ghcr.io/contextforge-org/contextforge-data-plane:latest
CF_DATAPLANE_PLATFORM=auto

CF_COMPOSE_BUILD=auto
CF_FAST_TIME_EXPECTED_IMAGE=ghcr.io/ibm/cfex-mcp-fast-time-server:latest
CF_FAST_TIME_SERVER_ID=9779b6698cbd4b4995ee04a4fab38737

MCP_CLI_BASE_URL=http://127.0.0.1:8080
# Optional global override; leave unset for the current 2025-11-25 default.
# MCP_PROTOCOL_VERSION=2026-07-28
NGINX_PORT=8080
```

Published control-plane and dataplane images are the defaults; the dataplane
uses its `latest` tag. The control-plane checkout defaults to v1.0.7, whose
publisher uses UUID token subjects and the current backend snapshot schema. Set
`CF_DATAPLANE_REF` to build an explicit local dataplane ref.
`CF_COMPOSE_BUILD=auto` pulls or reuses prebuilt images and rebuilds a missing
or revision-stale source dataplane; `true` always builds and `false` never
builds.

Token and endpoint overrides used by probe, load, and debug commands:

```bash
# Optional overrides. Without them, stable random local signing values are
# generated once under CF_INTEGRATION_DIR.
JWT_SECRET_KEY=<integration-secret>
AUTH_ENCRYPTION_SECRET=<integration-encryption-secret>
PLATFORM_ADMIN_EMAIL=admin@example.com
PLATFORM_ADMIN_PASSWORD=<local-integration-password>
MCPGATEWAY_BEARER_TOKEN=<pre-minted-token>
MCP_SERVER_ID=<virtual-server-id>
MCP_TOOL_NAMES=<comma-separated-safe-tool-names>
```

Managed workflows authenticate through the control-plane email-login endpoint.
Dataplane probe, load, Inspector, and conformance runs then request a one-day,
server-scoped API token from the token catalog and revoke it before stack
teardown. This ensures the token's UUID subject selects the same `UserConfig`
snapshot the publisher wrote. `MCPGATEWAY_BEARER_TOKEN` bypasses that
lifecycle and is never revoked by the harness.

Conformance ignores caller-managed fixture IDs and tokens so every lane uses
the same official fixture. Never commit `.env` or generated tokens.

## Future architecture alignment

The dataplane repository's tentative ContextForge 2.0 wiki describes a
management plane, a legacy Python MCP slow path, and a modern `2026-07-28`
Rust fast path consuming revisioned effective configuration from a shared
store. This harness prepares for that split by keeping management and raw
`/mcp` traffic on control-plane, routing `/servers/{id}/mcp` strictly to the
dataplane, providing explicit stateless modern probe/load/Inspector paths, and
obtaining dataplane credentials from the management plane. The ordinary
workflow default remains `2025-11-25` until the current upstream expected
failure baseline for stateless aggregate and targeted operations is retired.

The remaining boundary belongs upstream rather than in this harness:
control-plane must publish atomic compiled configuration and perform discovery,
catalog normalization, pagination, and liveness; dataplane must serve aggregate
catalog methods from that configuration and route targeted operations to one
backend without live fan-out. When those phases land, the harness should add
revision-isolation and tenant/principal partition tests instead of compatibility
fallbacks. See the
[`_context/wiki` architecture notes](https://github.com/contextforge-org/contextforge-data-plane/tree/main/_context/wiki).

## Repository layout

```text
Cargo.toml, Cargo.lock                     Rust workspace
.cargo/config.toml                        Cargo output under .integration/
src/                                      CLI and workflow composition
crates/platform/                          platform orchestration library
crates/mcp/                               MCP transport and probe library
crates/compliance/                        official conformance library
crates/load/                              Locust orchestration library
docker/docker-compose.cf-dataplane.yaml   dataplane service and nginx override
docker/docker-compose.cf-integration.yaml Fast Time and Locust overlay
docker/docker-compose.cf-conformance.yaml official fixture overlay
scripts/locustfile_mcp.py                  Locust MCP adapter
manual-tests/mcp-manual-test-tools/        vendored Bruno workspace
reports/mcp-conformance-comparison.md      tracked three-lane comparison
.integration/                              ignored checkout/build/runtime state
```
