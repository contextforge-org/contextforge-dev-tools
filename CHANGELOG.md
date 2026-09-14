# Changelog

All notable changes to `cf-integration` are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-09-14

### Added

- Visible one-letter aliases for every public command and short flags for all
  public options, including `-s` for standalone, `-l` for lane, and `-c` for
  client era. `--help` lists the complete mappings.

### Changed

- Pull versioned conformance fixture, tooling, and helper images published by the
  release workflow instead of compiling them during normal runs. Reuse images
  already present on the Docker daemon; `CF_COMPOSE_BUILD=true` explicitly
  rebuilds them for local harness development.

- Load result labels and report folders now use `builtin` and `external`.
  Reports are separated by client era so a legacy fallback preserves modern-run
  evidence.

- Conformance legacy client runs now select only
  `2025-11-25`. Dual combines it with `2026-07-28`; older exact client revision
  selectors are rejected.
- Align load with conformance's command and flag style: use `load run` with
  `--client-era legacy|modern` (default modern), `--lane`, and `--standalone`.
  Remove the former flat load command and its `--protocol-version` flag and
  `MCP_PROTOCOL_VERSION` override. Backend protocol support remains server-owned.

### Fixed

- Use Fast Time for every load lane, including standalone external, with a shared
  `CF_FAST_TIME_EXPECTED_IMAGE` override. Call only echo with the same payload in both lanes;
  fail if it is missing. Reserve the conformance fixture for protocol workflows.

- Bootstrap full-stack external authentication in the CLI: generate an RSA key,
  configure the control plane to issue RS256 tokens, serve matching public JWKS
  on dataplane loopback, and map control-plane subjects to the local test tenant.
- Generate and persist a strong local admin password; supply the required
  `DEFAULT_USER_PASSWORD` while preserving explicit credential overrides.
- Register Fast Time through control-plane login instead of upstream's hard-coded
  HS256 token minting. Do not print credentials during registration.
- Reject incompatible control-plane publisher snapshots before launching load.
- Pool public and fixture proxy upstream connections with DNS refresh to prevent
  ephemeral-port exhaustion during sustained load. Pin nginx 1.30.4 and preserve
  final JSON statistics for early-stopped runs. Builtin load uses pooled
  connections directly to port 4444, avoiding the inactive 8787 listener and
  expiring idle pooled connections before the backend closes them.
- Pin every load lane to Locust 2.46.2 for comparable client measurements.
- Skip control-plane credential construction when cleaning up standalone tokens.

- Stop load immediately after the first request or user error and retain failure
  reports. Do not start workload tasks after a failed initialized notification.
- Apply the builtin load proxy only to load runs, require successful Fast Time
  registration before exposing the external public route, and keep shared
  publisher-schema errors workflow-neutral.
- Legacy load clients now use the revision negotiated during initialization,
  reject unsupported revisions, and skip workload requests after failed
  initialization or discovery.
  Modern clients do not adopt legacy session IDs.

## [0.3.3] - 2026-09-09

### Added

- Select exact conformance client revisions with repeatable `--client-version`
  arguments, without including every revision in a protocol era.

### Changed

- Run official conformance and Inspector inside a Docker tooling image, removing
  the host Node/npm requirement. Client drivers publish directly to Redis, and
  runner containers are removed after completion, failure, or interruption.

- Moved standalone auth and config publishing into the Rust CLI, replacing the Node
  helper image and npm dependencies. Fixture customization uses a checked patch.

- Consolidated server and client conformance artifact validation, baseline gates,
  and reporting into one direction-aware path.
- Removed unused MCP transport features and stack command wrappers; tests now
  exercise the same MCP POST client used by probes and conformance.
- Shared asynchronous child-process execution and CI/release quality checks.

### Fixed

- Preserve configured Docker connection settings when removing conformance and
  Inspector containers, including after failure or interruption.

## [0.3.2] - 2026-09-07

### Changed

- Standalone workflows now run against production dataplane images without
  `with_tools`. The harness signs ephemeral JWTs, serves loopback JWKS, and
  publishes MessagePack routing snapshots directly to Redis.

- Simplified runtime dispatch and shared authenticated workflow setup, removing
  forwarding wrappers while preserving token revocation and stack cleanup.

### Fixed

- Removed control-plane checkout, worker-configuration, and secret-generation
  dependencies from standalone workflows, including installed-binary load tests.
- Kept captured command data, including test tokens, out of conformance setup
  logs while retaining helper diagnostics when stdout capture fails.
- Stopped the entire conformance matrix after Ctrl-C cleanup and prevented
  interrupted runs from updating baselines.
- Discover standalone conformance routes and input schemas from the running
  fixture, including paginated diagnostic tools. Fixture discovery errors stop
  setup before a baseline can be blessed.
- Made the dataplane config writer available to normal external client conformance
  and preserved schemas for its scenario tools.
- Embedded the ClickStack collector configuration required by installed-binary
  conformance runs.
- Accepted empty pagination cursors and legacy SSE keepalives during discovery,
  and used the fixture's protocol era when configuring backends for clients from
  a different era.
- Rejected `live --standalone`, which previously reported live-group success after
  only a probe. Isolated route checks remain available through `probe --standalone`.

## [0.3.1] - 2026-09-04

### Added

- Added global standalone external-dataplane mode across stack, probe, load,
  conformance, and debug-token workflows, backed by mocked Redis,
  ephemeral RSA authentication, and no control-plane services.

### Changed

- Published every standalone route through the running dataplane's serializer
  so mocked Redis snapshots always use that image's current MessagePack schema.
- Added protocol-mode selection to `stack up`.
- Left the independent ClickStack service running during explicit stack cleanup
  as well as managed workflow cleanup.

### Fixed

- Avoided a Linux `ETXTBSY` race in the native process-runner test.
- Embedded the complete standalone and observability asset set in installed
  binaries and updated standalone authentication for the dataplane JWKS model.
- Skipped unsupported external-dataplane catalog fan-out during standalone
  probes and load tests while retaining routed tool-call coverage.

## [0.3.0] - 2026-09-03

### Added

- Added semantic `modern` and `legacy` MCP protocol selectors across commands.
- Standardized routed workflow selection on `builtin`, `external`, and, where
  applicable, `fixture-direct` lanes.
- Added standalone external-dataplane load tests that disable the control plane
  during the measured phase and populate a per-run mocked Redis snapshot using
  the dataplane's current routing schema.
- Added no-login ClickStack observability with control-plane and dataplane
  traces, native dataplane HTTP metrics, and routed-service logs.
- Added a concise command guide covering stack, probe, load, live, conformance,
  CI, and debug workflows.

### Changed

- Reused the external conformance stack between compatible server and client
  phases while preserving setup, execution, and cleanup failures.
- Moved ClickStack into an independent Compose lifecycle so managed test cleanup
  leaves telemetry available for inspection.
- Made ClickStack the default for non-performance workflows and an explicit
  opt-in for load tests to avoid skewing benchmark results.

### Fixed

- Flushed conformance results before propagating a failed child-process exit.
- Prevented duplicate ClickStack trace and metric ingestion by using its built-in
  OTLP pipelines once.

## [0.2.0] - 2026-09-01

### Added

- Added embedded runtime assets so the published crate works outside its source
  checkout.
- Added the three-lane MCP conformance matrix, checked-in baselines, isolated
  official fixtures, and client-conformance coverage for the external dataplane.
- Added native release binaries and automated crate publishing.

### Changed

- Consolidated the harness into the `cf-integration` package and decomposed its
  runtime into focused stack, MCP, conformance, and performance workflows.
- Standardized terminal progress and conformance result reporting.

### Fixed

- Made runtime paths, Docker Compose invocation, image pulls, and source-image
  builds portable across supported hosts.
- Preserved all conformance lane and cleanup failures in final results.

## [0.1.0] - 2026-08-28

### Added

- Initial Rust CLI release for ContextForge stack orchestration, routed MCP
  probing, live tests, load tests, and official conformance execution.
- Added builtin and external dataplane routing through reusable Docker Compose
  overlays.

[Unreleased]: https://github.com/contextforge-org/contextforge-dev-tools/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/contextforge-org/contextforge-dev-tools/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/contextforge-org/contextforge-dev-tools/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/contextforge-org/contextforge-dev-tools/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/contextforge-org/contextforge-dev-tools/releases/tag/v0.1.0
