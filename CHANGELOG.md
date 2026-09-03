# Changelog

All notable changes to `cf-integration` are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
  leaves telemetry available for inspection; explicit `stack down` removes it.
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

[Unreleased]: https://github.com/contextforge-org/contextforge-dev-tools/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/contextforge-org/contextforge-dev-tools/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/contextforge-org/contextforge-dev-tools/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/contextforge-org/contextforge-dev-tools/releases/tag/v0.1.0
