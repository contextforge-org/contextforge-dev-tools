//! Official conformance artifact paths, loading, and report generation.

use super::*;

const CONFORMANCE_COMPLETION_MARKER: &[u8] = b"complete\n";

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) fn regenerate_conformance_report(
        &self,
        results_dir: Option<&Path>,
        output_dir: Option<&Path>,
    ) -> AppResult<()> {
        let artifact_root = results_dir.unwrap_or_else(|| self.config.integration_dir());
        let report_root = output_dir
            .map(Path::to_owned)
            .unwrap_or_else(|| self.config.root().join("reports"));
        let runs = discover_conformance_runs(artifact_root, &report_root)?;
        let mut failures = Vec::new();
        for paths in runs {
            match self.write_comparison_from_artifacts(&paths, None) {
                Ok(comparison) => println!(
                    "{} {}",
                    OutputStyle::stdout().info("Conformance comparison:"),
                    comparison.display()
                ),
                Err(error) => failures.push(format!("{}: {error}", paths.identity())),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(AppFailure::from(anyhow!(
                "conformance report regeneration completed with failures:\n- {}",
                failures.join("\n- ")
            )))
        }
    }

    pub(super) fn write_comparison_from_artifacts(
        &self,
        paths: &ConformancePaths,
        expected_run: Option<(&str, ConformanceServerEra, &str)>,
    ) -> AppResult<PathBuf> {
        let fixture = self.load_conformance_artifact(paths, ConformanceTarget::FixtureDirect)?;
        let built_in =
            self.load_conformance_artifact(paths, ConformanceTarget::BuiltInDataPlane)?;
        let external =
            self.load_conformance_artifact(paths, ConformanceTarget::ExternalDataPlane)?;
        if fixture.is_none() && built_in.is_none() && external.is_none() {
            return Err(AppFailure::from(anyhow!(
                "no official conformance artifacts found beneath {}",
                paths.conformance_root.display()
            )));
        }
        let missing = [
            (ConformanceTarget::FixtureDirect, fixture.is_none()),
            (ConformanceTarget::BuiltInDataPlane, built_in.is_none()),
            (ConformanceTarget::ExternalDataPlane, external.is_none()),
        ]
        .into_iter()
        .filter_map(|(lane, missing)| missing.then_some(lane.slug()))
        .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(AppFailure::from(anyhow!(
                "missing conformance lanes for {}: {}",
                paths.identity(),
                missing.join(", ")
            )));
        }

        let metadata = compatible_metadata(
            fixture.as_ref().map(|artifact| &artifact.metadata),
            built_in.as_ref().map(|artifact| &artifact.metadata),
            external.as_ref().map(|artifact| &artifact.metadata),
            expected_run,
        )?;
        let empty_results = ConformanceResults::default();
        let scenarios = compare_result_sets_with_fixture_trust(
            fixture
                .as_ref()
                .map_or(&empty_results, |artifact| &artifact.results),
            built_in
                .as_ref()
                .map_or(&empty_results, |artifact| &artifact.results),
            external
                .as_ref()
                .map_or(&empty_results, |artifact| &artifact.results),
            ComparisonFixtureTrust {
                fixture: is_trusted_official_fixture(
                    fixture
                        .as_ref()
                        .and_then(|artifact| artifact.metadata.fixture.as_ref()),
                ),
                built_in_data_plane: is_trusted_official_fixture(
                    built_in
                        .as_ref()
                        .and_then(|artifact| artifact.metadata.fixture.as_ref()),
                ),
                external_data_plane: is_trusted_official_fixture(
                    external
                        .as_ref()
                        .and_then(|artifact| artifact.metadata.fixture.as_ref()),
                ),
            },
        );
        let output = paths.report_output.join("mcp-conformance-comparison.md");
        write_comparison_report(
            &output,
            &ComparisonReport {
                client_version: metadata.client_version.clone(),
                server_era: metadata.server_era,
                suite: metadata.suite.clone(),
                fixture: metadata.fixture.clone(),
                scenarios,
            },
        )
        .map_err(AppFailure::from)?;
        Ok(output)
    }

    fn load_conformance_artifact(
        &self,
        paths: &ConformancePaths,
        target: ConformanceTarget,
    ) -> AppResult<Option<LoadedConformanceArtifact>> {
        let artifact = paths.conformance_lane(target);
        if !artifact.metadata.is_file()
            && !artifact.official_results.is_dir()
            && !artifact.completion.is_file()
        {
            return Ok(None);
        }
        if !artifact.metadata.is_file()
            || !artifact.official_results.is_dir()
            || !artifact.completion.is_file()
        {
            return Err(AppFailure::from(anyhow!(
                "incomplete conformance artifacts for {target} beneath {}",
                artifact.root.display()
            )));
        }
        verify_completion_marker(&artifact.completion)?;
        let metadata = read_run_metadata(&artifact.metadata)?;
        if metadata.target != target.label() {
            return Err(AppFailure::from(anyhow!(
                "conformance metadata target {:?} does not match {target}",
                metadata.target
            )));
        }
        if metadata.oracle != crate::conformance::results::OFFICIAL_CONFORMANCE_PACKAGE {
            return Err(AppFailure::from(anyhow!(
                "conformance artifacts used oracle {:?}, expected {:?}",
                metadata.oracle,
                crate::conformance::results::OFFICIAL_CONFORMANCE_PACKAGE
            )));
        }
        let results = load_server_results(&artifact.official_results).map_err(AppFailure::from)?;
        validate_server_scenario_set(&results, &metadata.suite, &metadata.client_version)
            .map_err(AppFailure::from)?;
        validate_scored_results(&results).map_err(AppFailure::from)?;
        Ok(Some(LoadedConformanceArtifact { results, metadata }))
    }

    pub(super) fn load_selected_conformance_results(
        &self,
        paths: &ConformancePaths,
        lanes: &[ConformanceTarget],
    ) -> AppResult<BTreeMap<ConformanceTarget, ConformanceResults>> {
        let mut results = BTreeMap::new();
        for lane in lanes {
            let artifact = self
                .load_conformance_artifact(paths, *lane)?
                .ok_or_else(|| {
                    AppFailure::from(anyhow!(
                        "missing selected conformance lane {} for {}",
                        lane.slug(),
                        paths.identity()
                    ))
                })?;
            results.insert(*lane, artifact.results);
        }
        Ok(results)
    }
}

#[derive(Debug, Clone)]
pub(super) struct ConformancePaths {
    pub(super) conformance_root: PathBuf,
    pub(super) report_output: PathBuf,
    client_version: String,
    server_era: ConformanceServerEra,
}

impl ConformancePaths {
    pub(super) fn new(
        artifact_root: &Path,
        report_root: PathBuf,
        client_version: &str,
        server_era: ConformanceServerEra,
    ) -> Self {
        Self {
            conformance_root: artifact_root
                .join("conformance")
                .join(client_version)
                .join(server_era.label()),
            report_output: report_root
                .join("conformance")
                .join(client_version)
                .join(server_era.label()),
            client_version: client_version.to_owned(),
            server_era,
        }
    }

    pub(super) fn identity(&self) -> String {
        format!("client {}, server {}", self.client_version, self.server_era)
    }

    pub(super) fn setup_log(&self) -> PathBuf {
        self.conformance_root.join("setup.log")
    }

    pub(super) fn baseline_report(&self, target: ConformanceTarget) -> PathBuf {
        self.report_output
            .join(target.slug())
            .join("baseline-comparison.yml")
    }

    pub(super) fn conformance_lane(&self, target: ConformanceTarget) -> ConformanceLanePaths {
        let root = self.conformance_root.join(target.slug());
        ConformanceLanePaths {
            official_results: root.join("official"),
            runner_log: root.join("runner.log"),
            expected_failures: root.join("expected-failures.yml"),
            metadata: root.join("metadata.json"),
            completion: root.join("complete"),
            root,
        }
    }

    pub(super) fn clear_conformance(&self) -> AppResult<()> {
        for target in [
            ConformanceTarget::FixtureDirect,
            ConformanceTarget::BuiltInDataPlane,
            ConformanceTarget::ExternalDataPlane,
        ] {
            remove_artifact_directory(&self.conformance_lane(target).root)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(super) struct ConformanceLanePaths {
    pub(super) root: PathBuf,
    pub(super) official_results: PathBuf,
    pub(super) runner_log: PathBuf,
    pub(super) expected_failures: PathBuf,
    pub(super) metadata: PathBuf,
    pub(super) completion: PathBuf,
}

struct LoadedConformanceArtifact {
    results: ConformanceResults,
    metadata: ConformanceRunMetadata,
}

fn discover_conformance_runs(
    artifact_root: &Path,
    report_root: &Path,
) -> AppResult<Vec<ConformancePaths>> {
    let root = artifact_root.join("conformance");
    let version_entries = strict_directories(&root, "client-version")?;
    let mut runs = Vec::new();
    for version_entry in version_entries {
        let client_version = version_entry
            .file_name()
            .into_string()
            .map_err(|_| AppFailure::from(anyhow!("client-version directory is not UTF-8")))?;
        ProtocolVersion::from_str(&client_version).map_err(|error| {
            AppFailure::from(anyhow!(
                "invalid conformance client-version directory {client_version:?}: {error}"
            ))
        })?;
        for era_entry in strict_directories(&version_entry.path(), "server-era")? {
            let label = era_entry
                .file_name()
                .into_string()
                .map_err(|_| AppFailure::from(anyhow!("server-era directory is not UTF-8")))?;
            let server_era = ConformanceServerEra::from_label(&label).ok_or_else(|| {
                AppFailure::from(anyhow!(
                    "unknown conformance server-era directory {label:?}"
                ))
            })?;
            runs.push(ConformancePaths::new(
                artifact_root,
                report_root.to_owned(),
                &client_version,
                server_era,
            ));
        }
    }
    if runs.is_empty() {
        return Err(AppFailure::from(anyhow!(
            "no partitioned official conformance artifacts found beneath {}",
            root.display()
        )));
    }
    Ok(runs)
}

fn strict_directories(path: &Path, dimension: &str) -> AppResult<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("failed to read conformance {dimension} directory {path:?}"))
        .map_err(AppFailure::from)?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to enumerate conformance directory {path:?}"))
        .map_err(AppFailure::from)?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in &entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect conformance entry {:?}", entry.path()))
            .map_err(AppFailure::from)?;
        if !file_type.is_dir() || file_type.is_symlink() {
            return Err(AppFailure::from(anyhow!(
                "unexpected non-directory entry in conformance {dimension} directory: {}",
                entry.path().display()
            )));
        }
    }
    Ok(entries)
}

pub(super) fn recreate_directory(path: &Path) -> AppResult<()> {
    if path.exists() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to clear result directory {path:?}"))
            .map_err(AppFailure::from)?;
    }
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create result directory {path:?}"))
        .map_err(AppFailure::from)
}

pub(super) fn remove_file_if_exists(path: &Path) -> AppResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppFailure::from(
            anyhow!(error).context(format!("failed to clear completion marker {path:?}")),
        )),
    }
}

fn write_completion_marker(path: &Path) -> AppResult<()> {
    fs::write(path, CONFORMANCE_COMPLETION_MARKER)
        .with_context(|| format!("failed to write conformance completion marker {path:?}"))
        .map_err(AppFailure::from)
}

pub(super) fn conformance_process_completed(process_result: &AppResult<()>) -> bool {
    match process_result {
        Ok(()) => true,
        Err(AppFailure::Infrastructure(InfrastructureError::ChildExit { status, .. })) => {
            status.code().is_some()
        }
        Err(AppFailure::Infrastructure(InfrastructureError::Native(_)))
        | Err(AppFailure::Native(_)) => false,
    }
}

pub(super) fn mark_conformance_complete(
    process_result: &AppResult<()>,
    results: &ConformanceResults,
    target: ConformanceTarget,
    suite: &str,
    spec_version: &str,
    path: &Path,
) -> AppResult<bool> {
    if !conformance_process_completed(process_result) {
        return Ok(false);
    }
    validate_server_scenario_set(results, suite, spec_version)
        .with_context(|| format!("official conformance did not complete for {target}"))
        .map_err(AppFailure::from)?;
    write_completion_marker(path)?;
    Ok(true)
}

fn verify_completion_marker(path: &Path) -> AppResult<()> {
    let marker = fs::read(path)
        .with_context(|| format!("failed to read conformance completion marker {path:?}"))
        .map_err(AppFailure::from)?;
    if marker != CONFORMANCE_COMPLETION_MARKER {
        return Err(AppFailure::from(anyhow!(
            "invalid conformance completion marker {path:?}"
        )));
    }
    Ok(())
}

pub(super) fn write_run_metadata(path: &Path, metadata: &ConformanceRunMetadata) -> AppResult<()> {
    let serialized = serde_json::to_vec_pretty(metadata)
        .context("failed to serialize conformance run metadata")
        .map_err(AppFailure::from)?;
    fs::write(path, serialized)
        .with_context(|| format!("failed to write conformance run metadata {path:?}"))
        .map_err(AppFailure::from)
}

fn read_run_metadata(path: &Path) -> AppResult<ConformanceRunMetadata> {
    let source = fs::read(path)
        .with_context(|| format!("failed to read conformance run metadata {path:?}"))
        .map_err(AppFailure::from)?;
    serde_json::from_slice(&source)
        .with_context(|| format!("failed to parse conformance run metadata {path:?}"))
        .map_err(AppFailure::from)
}

fn compatible_metadata<'a>(
    fixture: Option<&'a ConformanceRunMetadata>,
    built_in: Option<&'a ConformanceRunMetadata>,
    external: Option<&'a ConformanceRunMetadata>,
    expected_run: Option<(&str, ConformanceServerEra, &str)>,
) -> AppResult<&'a ConformanceRunMetadata> {
    let metadata = fixture.or(built_in).or(external).ok_or_else(|| {
        AppFailure::from(anyhow!(
            "no conformance metadata is available for reporting"
        ))
    })?;
    for candidate in [fixture, built_in, external].into_iter().flatten() {
        if candidate.fixture != metadata.fixture {
            return Err(AppFailure::from(anyhow!(
                "direct fixture, built-in data-plane, and external data-plane conformance fixture provenance mismatch"
            )));
        }
        if candidate.client_version != metadata.client_version
            || candidate.server_era != metadata.server_era
            || candidate.suite != metadata.suite
            || candidate.oracle != metadata.oracle
        {
            return Err(AppFailure::from(anyhow!(
                "direct fixture, built-in data-plane, and external data-plane conformance artifacts were produced by incompatible runs"
            )));
        }
    }
    if let Some((spec_version, server_era, suite)) = expected_run
        && (metadata.client_version != spec_version
            || metadata.server_era != server_era
            || metadata.suite != suite)
    {
        return Err(AppFailure::from(anyhow!(
            "conformance artifacts do not match requested client spec version {spec_version:?}, server era {server_era:?}, and suite {suite:?}"
        )));
    }
    Ok(metadata)
}

pub(super) const fn conformance_target(topology: StackMode) -> ConformanceTarget {
    match topology {
        StackMode::Controlplane => ConformanceTarget::BuiltInDataPlane,
        StackMode::Dataplane => ConformanceTarget::ExternalDataPlane,
    }
}

fn remove_artifact_directory(path: &Path) -> AppResult<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppFailure::from(
            anyhow!(error).context(format!("failed to clear conformance artifacts {path:?}")),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::fixture::{
        OFFICIAL_CONFORMANCE_REPOSITORY, OFFICIAL_CONFORMANCE_REVISION,
        OFFICIAL_CONFORMANCE_SERVER_ID,
    };
    use crate::conformance::results::OFFICIAL_CONFORMANCE_PACKAGE;

    fn metadata(target: ConformanceTarget) -> ConformanceRunMetadata {
        ConformanceRunMetadata {
            oracle: OFFICIAL_CONFORMANCE_PACKAGE.to_owned(),
            target: target.label().to_owned(),
            client_version: "2026-07-28".to_owned(),
            server_era: ConformanceServerEra::Dual,
            suite: "all".to_owned(),
            fixture: Some(ConformanceFixtureMetadata {
                repository: OFFICIAL_CONFORMANCE_REPOSITORY.to_owned(),
                revision: OFFICIAL_CONFORMANCE_REVISION.to_owned(),
                server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
            }),
        }
    }

    #[test]
    fn conformance_paths_partition_all_three_lanes() {
        let paths = ConformancePaths::new(
            Path::new("artifacts"),
            PathBuf::from("reports"),
            "2026-07-28",
            ConformanceServerEra::Modern,
        );

        assert_eq!(
            paths
                .conformance_lane(ConformanceTarget::FixtureDirect)
                .root,
            PathBuf::from("artifacts/conformance/2026-07-28/modern/fixture-direct")
        );
        assert_eq!(
            paths
                .conformance_lane(ConformanceTarget::BuiltInDataPlane)
                .root,
            PathBuf::from("artifacts/conformance/2026-07-28/modern/built-in-data-plane")
        );
        assert_eq!(
            paths
                .conformance_lane(ConformanceTarget::ExternalDataPlane)
                .root,
            PathBuf::from("artifacts/conformance/2026-07-28/modern/external-data-plane")
        );
        assert_eq!(
            paths.baseline_report(ConformanceTarget::BuiltInDataPlane),
            PathBuf::from(
                "reports/conformance/2026-07-28/modern/built-in-data-plane/baseline-comparison.yml"
            )
        );
    }

    #[test]
    fn clearing_a_run_removes_every_lane_to_prevent_stale_comparisons() {
        let directory = tempfile::tempdir().expect("temporary artifact root");
        let paths = ConformancePaths::new(
            directory.path(),
            PathBuf::from("reports"),
            "2026-07-28",
            ConformanceServerEra::Dual,
        );
        for target in [
            ConformanceTarget::FixtureDirect,
            ConformanceTarget::BuiltInDataPlane,
            ConformanceTarget::ExternalDataPlane,
        ] {
            fs::create_dir_all(paths.conformance_lane(target).root)
                .expect("lane directory should be created");
        }

        paths
            .clear_conformance()
            .expect("all old lanes should be removed");

        for target in [
            ConformanceTarget::FixtureDirect,
            ConformanceTarget::BuiltInDataPlane,
            ConformanceTarget::ExternalDataPlane,
        ] {
            assert!(!paths.conformance_lane(target).root.exists());
        }
    }

    #[test]
    fn partial_lane_metadata_is_reportable_when_provenance_matches() {
        let fixture = metadata(ConformanceTarget::FixtureDirect);
        let dataplane = metadata(ConformanceTarget::ExternalDataPlane);

        let selected = compatible_metadata(
            Some(&fixture),
            None,
            Some(&dataplane),
            Some(("2026-07-28", ConformanceServerEra::Dual, "all")),
        )
        .expect("selected lanes should be compatible");

        assert_eq!(selected.client_version, "2026-07-28");
    }

    #[test]
    fn mismatched_fixture_provenance_prevents_cross_lane_comparison() {
        let fixture = metadata(ConformanceTarget::FixtureDirect);
        let mut dataplane = metadata(ConformanceTarget::ExternalDataPlane);
        dataplane
            .fixture
            .as_mut()
            .expect("fixture metadata should exist")
            .revision = "different".to_owned();

        let error = compatible_metadata(Some(&fixture), None, Some(&dataplane), None)
            .expect_err("mismatched provenance must fail")
            .to_string();

        assert!(error.contains("provenance mismatch"));
        assert!(!error.contains("different"));
    }

    #[test]
    fn mismatched_server_eras_prevent_cross_lane_comparison() {
        let fixture = metadata(ConformanceTarget::FixtureDirect);
        let mut dataplane = metadata(ConformanceTarget::ExternalDataPlane);
        dataplane.server_era = ConformanceServerEra::Legacy;

        let error = compatible_metadata(Some(&fixture), None, Some(&dataplane), None)
            .expect_err("different server eras must not be compared")
            .to_string();

        assert!(error.contains("incompatible runs"));
    }
}
