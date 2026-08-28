//! Embedded runtime assets used by installed CLI binaries.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use uuid::Uuid;

const COMPLETE_MARKER: &str = ".complete";

struct EmbeddedAsset {
    path: &'static str,
    contents: &'static [u8],
}

macro_rules! asset {
    ($path:literal) => {
        EmbeddedAsset {
            path: $path,
            contents: include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/", $path)),
        }
    };
}

const ASSETS: &[EmbeddedAsset] = &[
    asset!("docker/docker-compose.cf-conformance-runtime.yaml"),
    asset!("docker/docker-compose.cf-conformance.yaml"),
    asset!("docker/docker-compose.cf-controlplane-build-labels.yaml"),
    asset!("docker/docker-compose.cf-dataplane-build.yaml"),
    asset!("docker/docker-compose.cf-dataplane.yaml"),
    asset!("docker/docker-compose.cf-integration.yaml"),
    asset!("docker/mcp-conformance-server.Dockerfile"),
    asset!("docker/nginx.cf-conformance-proxy.conf"),
    asset!("docker/nginx.cf-dataplane.conf"),
    asset!("docker/patch-mcp-conformance-hosts.mjs"),
    asset!("scripts/live_protocol/sitecustomize.py"),
    asset!("scripts/conformance/write_client_config.py"),
    asset!("scripts/locustfile_mcp.py"),
    asset!("tests/conformance/baselines/2026-07-28/legacy/built-in-data-plane.yml"),
    asset!("tests/conformance/baselines/2026-07-28/legacy/client/external-data-plane.yml"),
    asset!("tests/conformance/baselines/2026-07-28/legacy/external-data-plane.yml"),
    asset!("tests/conformance/baselines/2026-07-28/legacy/fixture-direct.yml"),
    asset!("tests/conformance/baselines/2026-07-28/modern/built-in-data-plane.yml"),
    asset!("tests/conformance/baselines/2026-07-28/modern/client/external-data-plane.yml"),
    asset!("tests/conformance/baselines/2026-07-28/modern/external-data-plane.yml"),
    asset!("tests/conformance/baselines/2026-07-28/modern/fixture-direct.yml"),
];

/// Returns whether `root` contains the complete runtime asset set.
#[must_use]
pub(crate) fn contains_runtime_assets(root: &Path) -> bool {
    ASSETS.iter().all(|asset| root.join(asset.path).is_file())
}

/// Materializes the embedded runtime files below the integration directory.
///
/// Existing versioned assets must exactly match the binary. A mismatch fails
/// closed instead of silently running Compose with a mixed asset set.
pub(crate) fn materialize_runtime_assets(integration_dir: &Path) -> Result<PathBuf> {
    let parent = integration_dir.join("assets");
    let destination = parent.join(env!("CARGO_PKG_VERSION"));
    if destination.exists() {
        validate_materialized_assets(&destination)?;
        return Ok(destination);
    }

    fs::create_dir_all(&parent).with_context(|| {
        format!(
            "failed to create embedded asset directory {}",
            parent.display()
        )
    })?;
    let temporary = parent.join(format!(".tmp-{}", Uuid::new_v4().simple()));
    let result =
        write_asset_tree(&temporary).and_then(|()| match fs::rename(&temporary, &destination) {
            Ok(()) => Ok(()),
            Err(_) if destination.exists() => validate_materialized_assets(&destination),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to activate embedded runtime assets at {}",
                    destination.display()
                )
            }),
        });
    if temporary.exists() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result?;
    validate_materialized_assets(&destination)?;
    Ok(destination)
}

fn write_asset_tree(root: &Path) -> Result<()> {
    fs::create_dir(root)
        .with_context(|| format!("failed to create temporary asset tree {}", root.display()))?;
    for asset in ASSETS {
        let path = root.join(asset.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create embedded asset path {}", parent.display())
            })?;
        }
        fs::write(&path, asset.contents)
            .with_context(|| format!("failed to write embedded asset {}", path.display()))?;
        let mut permissions = fs::metadata(&path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions)?;
    }
    fs::write(root.join(COMPLETE_MARKER), env!("CARGO_PKG_VERSION"))
        .context("failed to write embedded asset completion marker")?;
    Ok(())
}

fn validate_materialized_assets(root: &Path) -> Result<()> {
    let marker = fs::read_to_string(root.join(COMPLETE_MARKER)).unwrap_or_default();
    if marker != env!("CARGO_PKG_VERSION") {
        bail!(
            "embedded runtime assets at {} are incomplete; remove that versioned directory and retry",
            root.display()
        );
    }
    for asset in ASSETS {
        let path = root.join(asset.path);
        let contents = fs::read(&path).with_context(|| {
            format!(
                "embedded runtime asset {} is missing; remove {} and retry",
                path.display(),
                root.display()
            )
        })?;
        if contents != asset.contents {
            bail!(
                "embedded runtime asset {} does not match this binary; remove {} and retry",
                path.display(),
                root.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_and_reuses_complete_assets() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = materialize_runtime_assets(directory.path()).expect("materialize assets");
        let second = materialize_runtime_assets(directory.path()).expect("reuse assets");

        assert_eq!(first, second);
        assert!(contains_runtime_assets(&first));
    }

    #[test]
    fn concurrent_materialization_converges_on_one_tree() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let integration_dir = std::sync::Arc::new(directory.path().to_path_buf());
        let workers = (0..4)
            .map(|_| {
                let integration_dir = integration_dir.clone();
                std::thread::spawn(move || materialize_runtime_assets(&integration_dir))
            })
            .collect::<Vec<_>>();

        let roots = workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .expect("materialization worker should not panic")
                    .expect("materialization worker should succeed")
            })
            .collect::<Vec<_>>();
        assert!(roots.windows(2).all(|pair| pair[0] == pair[1]));
        assert!(contains_runtime_assets(&roots[0]));
    }

    #[test]
    fn rejects_corrupted_versioned_assets() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = materialize_runtime_assets(directory.path()).expect("materialize assets");
        let path = root.join(ASSETS[0].path);
        make_writable(&path);
        fs::write(&path, b"corrupt").expect("corrupt test asset");

        let error = materialize_runtime_assets(directory.path())
            .expect_err("corrupted assets must fail closed");
        assert!(error.to_string().contains("does not match this binary"));
    }

    #[test]
    fn repository_contains_every_embedded_asset() {
        assert!(contains_runtime_assets(Path::new(env!(
            "CARGO_MANIFEST_DIR"
        ))));
    }

    #[cfg(unix)]
    fn make_writable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o644))
            .expect("make test asset writable");
    }

    #[cfg(not(unix))]
    fn make_writable(path: &Path) {
        let mut permissions = fs::metadata(path).expect("asset metadata").permissions();
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions).expect("make test asset writable");
    }
}
