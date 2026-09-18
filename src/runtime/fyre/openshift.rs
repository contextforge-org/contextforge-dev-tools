//! FYRE OpenShift lifecycle for the isolated parallel comparison.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};
use url::Url;

use super::{FyreConfig, OpenShiftConfig, RunState, validate_run_id, write_json, write_state};
use crate::infrastructure::process::CommandSpec;
use crate::runtime::{AppFailure, AppResult, ProcessRunner, RuntimeContext};

const DEFAULT_API_BASE: &str = "https://ocpapi.svl.ibm.com/v1";
const PROVISION_POLL: Duration = Duration::from_secs(120);
const PROVISION_TIMEOUT: Duration = Duration::from_secs(7_200);
const DELETE_POLL: Duration = Duration::from_secs(30);
const DELETE_TIMEOUT: Duration = Duration::from_secs(1_800);

struct FyreOpenShiftApi {
    http: Client,
    base: Url,
    username: String,
    api_key: String,
    site: String,
    product_group_id: String,
}

impl FyreOpenShiftApi {
    fn new<R: ProcessRunner>(runtime: &RuntimeContext<R>) -> Result<Self> {
        let username = runtime
            .fyre_text("FYRE_USERNAME")
            .context("FYRE_USERNAME is required for FYRE provisioning")?
            .to_owned();
        let api_key = runtime
            .fyre_text("FYRE_API_KEY")
            .context("FYRE_API_KEY is required for FYRE provisioning")?
            .to_owned();
        let site = runtime.fyre_text("FYRE_SITE").unwrap_or("svl").to_owned();
        ensure!(
            matches!(site.as_str(), "svl" | "rtp"),
            "FYRE_SITE must be svl or rtp"
        );
        let product_group_id = runtime
            .fyre_text("FYRE_PRODUCT_GROUP_ID")
            .context("FYRE_PRODUCT_GROUP_ID is required for 40 GB OpenShift nodes")?
            .to_owned();
        let base = runtime
            .fyre_text("FYRE_OCP_API_URL")
            .unwrap_or(DEFAULT_API_BASE)
            .trim_end_matches('/');
        let base =
            Url::parse(&format!("{base}/")).context("FYRE_OCP_API_URL must be an absolute URL")?;
        let http = Client::builder()
            .danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to create FYRE OpenShift API client")?;
        Ok(Self {
            http,
            base,
            username,
            api_key,
            site,
            product_group_id,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut url = self
            .base
            .join(path.trim_start_matches('/'))
            .expect("validated FYRE API base must accept relative paths");
        url.query_pairs_mut().append_pair("site", &self.site);
        self.http
            .request(method, url)
            .basic_auth(&self.username, Some(&self.api_key))
    }

    async fn hostname_available(&self, cluster: &str) -> Result<bool> {
        let response = self
            .request(reqwest::Method::GET, &format!("/check_hostname/{cluster}"))
            .send()
            .await
            .context("failed to check FYRE OpenShift cluster name")?;
        let value = response_json(response, "check FYRE OpenShift cluster name").await?;
        Ok(value["status"] == "success")
    }

    async fn create(&self, cluster: &str, config: &FyreConfig) -> Result<()> {
        ensure!(
            self.hostname_available(cluster).await?,
            "FYRE OpenShift cluster {cluster} already exists"
        );
        let openshift = config
            .infrastructure
            .openshift
            .as_ref()
            .context("OpenShift settings are missing")?;
        let payload = cluster_payload(
            cluster,
            config,
            openshift,
            &self.product_group_id,
            &self.site,
        );
        for attempt in 0..10 {
            let response = self
                .request(reqwest::Method::POST, "/ocp/x")
                .json(&payload)
                .send()
                .await
                .context("failed to request FYRE OpenShift cluster")?;
            if response.status() == StatusCode::TOO_MANY_REQUESTS && attempt < 9 {
                tokio::time::sleep(PROVISION_POLL).await;
                continue;
            }
            response_json(response, "create FYRE OpenShift cluster").await?;
            return Ok(());
        }
        bail!("FYRE OpenShift provisioning remained rate limited after ten attempts")
    }

    async fn wait_deployed(&self, cluster: &str) -> Result<Value> {
        let operation = async {
            loop {
                let response = self
                    .request(reqwest::Method::GET, &format!("/ocp/{cluster}/status"))
                    .send()
                    .await
                    .context("failed to read FYRE OpenShift deployment status")?;
                let (http_status, value) =
                    decode_response(response, "read FYRE OpenShift deployment status").await?;
                let details = value["details"].as_str().unwrap_or_default();
                if http_status == StatusCode::BAD_REQUEST && details.contains("does not exist") {
                    tokio::time::sleep(PROVISION_POLL).await;
                    continue;
                }
                ensure_success(http_status, &value, "read FYRE OpenShift deployment status")?;
                let status = value["deployed_status"]
                    .as_str()
                    .or_else(|| value["cluster_status"].as_str())
                    .or_else(|| value["status"].as_str())
                    .unwrap_or("unknown");
                if status == "deployed" {
                    return self.details(cluster).await;
                }
                if matches!(status, "failed" | "error" | "deleted") {
                    bail!("FYRE OpenShift deployment entered {status} state")
                }
                tokio::time::sleep(PROVISION_POLL).await;
            }
        };
        tokio::time::timeout(PROVISION_TIMEOUT, operation)
            .await
            .context("FYRE OpenShift cluster did not deploy within two hours")?
    }

    async fn details(&self, cluster: &str) -> Result<Value> {
        let response = self
            .request(reqwest::Method::GET, &format!("/ocp/{cluster}"))
            .send()
            .await
            .context("failed to read FYRE OpenShift cluster details")?;
        response_json(response, "read FYRE OpenShift cluster details").await
    }

    async fn delete(&self, cluster: &str) -> Result<()> {
        if self.hostname_available(cluster).await? {
            return Ok(());
        }
        let response = self
            .request(reqwest::Method::DELETE, &format!("/ocp/{cluster}"))
            .send()
            .await
            .context("failed to delete FYRE OpenShift cluster")?;
        response_json(response, "delete FYRE OpenShift cluster").await?;
        tokio::time::timeout(DELETE_TIMEOUT, async {
            loop {
                if self.hostname_available(cluster).await? {
                    return Ok(());
                }
                tokio::time::sleep(DELETE_POLL).await;
            }
        })
        .await
        .context("FYRE OpenShift cluster deletion did not finish within 30 minutes")?
    }
}

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn run_fyre_openshift(
        &self,
        source: PathBuf,
        config: FyreConfig,
        run_id: Option<String>,
    ) -> AppResult<()> {
        let run_id = run_id
            .unwrap_or_else(|| format!("ocp-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]));
        validate_run_id(&run_id).map_err(AppFailure::from)?;
        let cluster_name = format!("cf-{run_id}");
        if cluster_name.len() > 32 {
            return Err(AppFailure::from(anyhow::anyhow!(
                "OpenShift run ID must be at most 29 characters"
            )));
        }
        let root = self.config.integration_dir().join("fyre").join(&run_id);
        if root.exists() {
            return Err(AppFailure::from(anyhow::anyhow!(
                "FYRE run {run_id} already exists at {}; use status or destroy with this run ID",
                root.display()
            )));
        }
        fs::create_dir_all(root.join("results/comparison"))
            .with_context(|| format!("failed to create FYRE run directory {}", root.display()))
            .map_err(AppFailure::from)?;
        let config_path = root.join("config.json");
        write_json(&config_path, &config).map_err(AppFailure::from)?;
        let mut state = RunState {
            schema_version: 1,
            run_id: run_id.clone(),
            infrastructure_kind: "openshift".to_owned(),
            cluster_name: Some(cluster_name.clone()),
            phase: "provisioning".to_owned(),
            config_file: source,
            current_scenario: Some("comparison".to_owned()),
            locust_helper_size: 0,
            fast_time_helper_size: 0,
            completed_scenarios: Vec::new(),
            cleanup_required: true,
        };
        write_state(&root, &state).map_err(AppFailure::from)?;
        let api = FyreOpenShiftApi::new(self).map_err(AppFailure::from)?;

        let primary = async {
            let provision = async {
                api.create(&cluster_name, &config).await?;
                api.wait_deployed(&cluster_name).await
            };
            tokio::pin!(provision);
            let details = tokio::select! {
                result = &mut provision => result.map_err(AppFailure::from)?,
                signal = tokio::signal::ctrl_c() => {
                    signal
                        .context("failed to install interrupt handler")
                        .map_err(AppFailure::from)?;
                    return Err(AppFailure::from(anyhow::anyhow!(
                        "FYRE OpenShift provisioning interrupted"
                    )));
                }
            };
            write_redacted_details(&root.join("cluster.json"), &details)
                .map_err(AppFailure::from)?;
            state.phase = "authenticating".to_owned();
            write_state(&root, &state).map_err(AppFailure::from)?;
            self.openshift_login(&root, &cluster_name, &config, &details)
                .await?;
            state.phase = "benchmarking".to_owned();
            write_state(&root, &state).map_err(AppFailure::from)?;
            let scenario_root = root.join("results/comparison");
            let command = CommandSpec::new("python3")
                .arg(
                    self.config
                        .asset_root()
                        .join("benchmarks/fyre/openshift_campaign.py"),
                )
                .arg("--config")
                .arg(&config_path)
                .arg("--kubeconfig")
                .arg(root.join("kubeconfig"))
                .arg("--output")
                .arg(&scenario_root)
                .arg("--run-id")
                .arg(&run_id)
                .arg("--assets")
                .arg(self.config.asset_root().join("benchmarks/fyre"));
            self.run_cancellable(&command).await?;
            state.completed_scenarios.push("comparison".to_owned());
            state.current_scenario = None;
            write_state(&root, &state).map_err(AppFailure::from)?;
            self.generate_fyre_report(&root, &config_path).await?;
            write_json(
                &root.join("manifest.json"),
                &json!({
                    "schema_version": 2,
                    "run_id": run_id,
                    "cluster_name": cluster_name,
                    "configuration": config,
                    "state": state,
                    "cluster": redacted_details(&details),
                }),
            )
            .map_err(AppFailure::from)
        }
        .await;

        state.phase = "destroying".to_owned();
        let _ = write_state(&root, &state);
        let cleanup = retry_delete(&api, &cluster_name)
            .await
            .map_err(AppFailure::from);
        let _ = fs::remove_file(root.join("kubeconfig"));
        if cleanup.is_ok() {
            state.cleanup_required = false;
            state.phase = if primary.is_ok() {
                "complete"
            } else {
                "failed"
            }
            .to_owned();
        } else {
            state.phase = "cleanup-failed".to_owned();
        }
        let _ = write_state(&root, &state);
        super::super::finish_with_cleanup(primary.err(), cleanup)
    }

    pub(super) async fn destroy_fyre_openshift(
        &self,
        root: &Path,
        state: &mut RunState,
    ) -> AppResult<()> {
        let cluster = state.cluster_name.as_deref().ok_or_else(|| {
            AppFailure::from(anyhow::anyhow!(
                "FYRE OpenShift cluster ownership is missing"
            ))
        })?;
        let api = FyreOpenShiftApi::new(self).map_err(AppFailure::from)?;
        retry_delete(&api, cluster)
            .await
            .map_err(AppFailure::from)?;
        state.cleanup_required = false;
        state.phase = "destroyed".to_owned();
        write_state(root, state).map_err(AppFailure::from)
    }

    async fn openshift_login(
        &self,
        root: &Path,
        cluster: &str,
        config: &FyreConfig,
        details: &Value,
    ) -> AppResult<()> {
        let password = cluster_record(details, cluster)
            .and_then(|record| record["kubeadmin_password"].as_str())
            .filter(|value| !value.is_empty())
            .context("FYRE cluster details did not include kubeadmin_password")
            .map_err(AppFailure::from)?;
        let image = &config
            .infrastructure
            .openshift
            .as_ref()
            .expect("validated OpenShift configuration")
            .oc_image;
        let mount = format!("{}:/work", root.display());
        let command = CommandSpec::new("docker")
            .args([
                "run",
                "--rm",
                "--platform",
                "linux/amd64",
                "--entrypoint",
                "/bin/sh",
            ])
            .arg("-v")
            .arg(mount)
            .arg("-e")
            .arg("OCP_PASSWORD")
            .env("OCP_PASSWORD", password)
            .arg(image)
            .args([
                "-ceu",
                "for attempt in 1 2 3 4 5 6 7 8 9 10; do oc login -u kubeadmin -p \"$OCP_PASSWORD\" --server \"$1\" --insecure-skip-tls-verify=true --kubeconfig /work/kubeconfig && exit 0; [ \"$attempt\" = 10 ] || sleep 30; done; exit 1",
                "oc-login",
            ])
            .arg(format!("https://api.{cluster}.cp.fyre.ibm.com:6443"));
        self.run_cancellable(&command).await?;
        set_private_permissions(&root.join("kubeconfig")).map_err(AppFailure::from)
    }
}

fn cluster_payload(
    cluster: &str,
    config: &FyreConfig,
    openshift: &OpenShiftConfig,
    product_group: &str,
    site: &str,
) -> Value {
    let mut worker_pools = BTreeMap::<(u32, u32), u32>::new();
    for pool in &openshift.worker_pools {
        *worker_pools.entry((pool.cpu, pool.memory_gb)).or_default() += pool.count;
    }
    json!({
        "name": cluster,
        "description": "ContextForge parallel built-in/external dataplane benchmark",
        "quota_type": "product_group",
        "site": site,
        "product_group_id": product_group,
        "ocp_version": openshift.version,
        "expiration": format!("{} hours", config.infrastructure.expiry_hours),
        "ipv6_test": false,
        "fips": "no",
        "master": {
            "count": 3,
            "cpu": openshift.master.cpu,
            "memory": openshift.master.memory_gb,
            "disk": openshift.base_disk_gb,
        },
        "infra": {
            "cpu": openshift.api.cpu,
            "memory": openshift.api.memory_gb,
            "disk": openshift.base_disk_gb,
        },
        "worker": worker_pools.into_iter().map(|((cpu, memory), count)| json!({
            "count": count,
            "cpu": cpu,
            "memory": memory,
            "os_disk": openshift.base_disk_gb,
        })).collect::<Vec<_>>(),
    })
}

async fn response_json(response: Response, operation: &str) -> Result<Value> {
    let (status, value) = decode_response(response, operation).await?;
    ensure_success(status, &value, operation)?;
    Ok(value)
}

async fn decode_response(response: Response, operation: &str) -> Result<(StatusCode, Value)> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed to read response for {operation}"))?;
    let value = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(
        |_| json!({"details": String::from_utf8_lossy(&bytes[..bytes.len().min(2_048)])}),
    );
    Ok((status, value))
}

fn ensure_success(status: StatusCode, value: &Value, operation: &str) -> Result<()> {
    let details = value
        .get("details")
        .filter(|details| !details.is_null())
        .map(|details| {
            details
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| details.to_string())
        })
        .unwrap_or_else(|| value.to_string());
    ensure!(
        status.is_success(),
        "{operation} failed with HTTP {status}: {details}"
    );
    Ok(())
}

fn cluster_record<'a>(details: &'a Value, cluster: &str) -> Option<&'a Value> {
    details["clusters"]
        .as_array()?
        .iter()
        .find(|record| record["cluster_name"] == cluster || record["name"] == cluster)
}

fn redacted_details(details: &Value) -> Value {
    fn redact(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.retain(|key, _| {
                    let key = key.to_ascii_lowercase();
                    !["password", "token", "secret", "api_key", "kubeconfig"]
                        .iter()
                        .any(|sensitive| key.contains(sensitive))
                });
                for child in object.values_mut() {
                    redact(child);
                }
            }
            Value::Array(values) => {
                for child in values {
                    redact(child);
                }
            }
            _ => {}
        }
    }
    let mut redacted = details.clone();
    redact(&mut redacted);
    redacted
}

fn write_redacted_details(path: &Path, details: &Value) -> Result<()> {
    write_json(path, &redacted_details(details))
}

async fn retry_delete(api: &FyreOpenShiftApi, cluster: &str) -> Result<()> {
    let mut last = None;
    for attempt in 0..3 {
        match api.delete(cluster).await {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(2_u64.pow(attempt + 1))).await;
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("FYRE OpenShift deletion failed")))
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_permissions(path: &Path) -> Result<()> {
    ensure!(
        path.is_file(),
        "kubeconfig {} was not created",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_removes_cluster_credentials() {
        let value = json!({"clusters": [{
            "cluster_name": "cf-run",
            "kubeadmin_password": "secret",
            "token": "secret",
            "vms": [{"nested_secret": "secret", "address": "private"}]
        }]});
        let redacted = redacted_details(&value);
        assert!(redacted["clusters"][0].get("kubeadmin_password").is_none());
        assert!(redacted["clusters"][0].get("token").is_none());
        assert!(
            redacted["clusters"][0]["vms"][0]
                .get("nested_secret")
                .is_none()
        );
        assert_eq!(redacted["clusters"][0]["vms"][0]["address"], "private");
        assert_eq!(redacted["clusters"][0]["cluster_name"], "cf-run");
    }

    #[test]
    fn packaged_profile_uses_three_pairs_of_40_gb_workers() {
        let config = super::super::read_config(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("benchmarks/fyre/openshift.yaml")
                .as_path(),
        )
        .expect("packaged OpenShift profile");
        super::super::validate_config(&config).expect("valid OpenShift profile");
        let openshift = config
            .infrastructure
            .openshift
            .as_ref()
            .expect("OpenShift settings");
        let payload = cluster_payload("cf-test", &config, openshift, "808", "svl");
        assert_eq!(payload["master"]["disk"], 40);
        let workers = payload["worker"].as_array().expect("worker pools");
        assert_eq!(workers.len(), 3);
        assert!(workers.iter().all(|pool| pool["count"] == 2));
        assert!(workers.iter().all(|pool| pool["os_disk"] == 40));
    }

    #[test]
    fn parallel_2v2_profile_fits_three_40_gb_workers() {
        let config = super::super::read_config(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("benchmarks/fyre/openshift-2v2-parallel.yaml")
                .as_path(),
        )
        .expect("packaged parallel 2v2 OpenShift profile");
        super::super::validate_config(&config).expect("valid parallel 2v2 profile");
        let openshift = config
            .infrastructure
            .openshift
            .as_ref()
            .expect("OpenShift settings");
        let payload = cluster_payload("cf-test", &config, openshift, "808", "svl");
        let workers = payload["worker"].as_array().expect("worker pools");
        assert_eq!(payload["master"]["count"], 3);
        assert_eq!(payload["infra"]["disk"], 40);
        assert_eq!(workers.len(), 2);
        assert_eq!(
            workers
                .iter()
                .map(|pool| pool["count"].as_u64().expect("worker count"))
                .sum::<u64>(),
            3
        );
        assert!(workers.iter().all(|pool| pool["os_disk"] == 40));
        let worker_cpu: u64 = workers
            .iter()
            .map(|pool| {
                pool["count"].as_u64().expect("worker count")
                    * pool["cpu"].as_u64().expect("worker cpu")
            })
            .sum();
        let worker_memory: u64 = workers
            .iter()
            .map(|pool| {
                pool["count"].as_u64().expect("worker count")
                    * pool["memory"].as_u64().expect("worker memory")
            })
            .sum();
        let worker_disk: u64 = workers
            .iter()
            .map(|pool| {
                pool["count"].as_u64().expect("worker count")
                    * pool["os_disk"].as_u64().expect("worker disk")
            })
            .sum();
        assert_eq!(worker_cpu + 3 * 4 + 4, 60);
        assert_eq!(worker_memory + 3 * 16 + 8, 96);
        assert_eq!(worker_disk + 3 * 40 + 40, 280);
    }

    #[test]
    fn nested_api_errors_remain_actionable() {
        let error = ensure_success(
            StatusCode::BAD_REQUEST,
            &json!({"status": "error", "details": {"errors": ["disk quota exceeded"]}}),
            "create cluster",
        )
        .expect_err("bad request must fail");
        assert!(error.to_string().contains("disk quota exceeded"));
    }
}
