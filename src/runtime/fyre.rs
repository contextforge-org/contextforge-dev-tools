//! Repeatable FYRE infrastructure and scaling-campaign orchestration.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{AppFailure, AppResult, CommandSpec, ProcessRunner, RuntimeContext};
use crate::app::FyreAction;

const OWNERSHIP_FILE: &str = "run.json";
const TERRAFORM_DIRECTORY: &str = "terraform";
const TERRAFORM_VARIABLES: &str = "scenario.tfvars.json";
const HELPER_SATURATION_EXIT: i32 = 42;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FyreConfig {
    schema_version: u32,
    infrastructure: InfrastructureConfig,
    images: ImageConfig,
    workload: WorkloadConfig,
    scenarios: Vec<Scenario>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_helper: Option<ActiveHelper>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_ssh_private_key: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InfrastructureConfig {
    os: String,
    ssh_user: String,
    ssh_private_key: PathBuf,
    ssh_public_key: PathBuf,
    expiry_hours: u32,
    helper_sizes: Vec<MachineSize>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct MachineSize {
    cpu: u32,
    memory_gb: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveHelper {
    locust_cpu: u32,
    locust_memory_gb: u32,
    fast_time_cpu: u32,
    fast_time_memory_gb: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImageConfig {
    dataplane: String,
    fast_time: String,
    helpers: String,
    locust: String,
    redis: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkloadConfig {
    protocol_version: String,
    first_users: u32,
    maximum_users: u32,
    ramp_seconds: u32,
    warmup_seconds: u32,
    measure_seconds: u32,
    repetitions: u32,
    maximum_campaign_seconds: u32,
    plateau_improvement_percent: f64,
    boundary_percent: f64,
    config_cache_seconds: u32,
    helper_cpu_percent: f64,
    helper_memory_percent: f64,
    worker_core_percent: f64,
    tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Scenario {
    id: String,
    label: String,
    replicas: u32,
    cpu: u32,
    memory_gb: u32,
    multiplier: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunState {
    schema_version: u32,
    run_id: String,
    phase: String,
    config_file: PathBuf,
    current_scenario: Option<String>,
    locust_helper_size: usize,
    fast_time_helper_size: usize,
    completed_scenarios: Vec<String>,
    cleanup_required: bool,
}

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn execute_fyre(&self, action: FyreAction) -> AppResult<()> {
        match action {
            FyreAction::Run { file, run_id } => self.run_fyre(file, run_id).await,
            FyreAction::Status { run_id } => self.fyre_status(&run_id),
            FyreAction::Destroy { run_id } => self.destroy_fyre(&run_id).await,
        }
    }

    async fn run_fyre(&self, file: Option<PathBuf>, run_id: Option<String>) -> AppResult<()> {
        let source = file.unwrap_or_else(|| {
            self.config
                .asset_root()
                .join("benchmarks/fyre/scaling.yaml")
        });
        let mut config = read_config(&source).map_err(AppFailure::from)?;
        validate_config(&config).map_err(AppFailure::from)?;
        self.require_fyre_credentials()?;
        let private_key =
            expand_home(&config.infrastructure.ssh_private_key).map_err(AppFailure::from)?;
        let public_key =
            expand_home(&config.infrastructure.ssh_public_key).map_err(AppFailure::from)?;
        ensure_file(&private_key, "SSH private key").map_err(AppFailure::from)?;
        ensure_file(&public_key, "SSH public key").map_err(AppFailure::from)?;
        config.resolved_ssh_private_key = Some(private_key);

        let run_id = run_id
            .unwrap_or_else(|| format!("scale-{}", &Uuid::new_v4().simple().to_string()[..12]));
        validate_run_id(&run_id).map_err(AppFailure::from)?;
        let root = self.config.integration_dir().join("fyre").join(&run_id);
        if root.exists() {
            return Err(AppFailure::from(anyhow::anyhow!(
                "FYRE run {run_id} already exists at {}; use status or destroy with this run ID",
                root.display()
            )));
        }
        fs::create_dir_all(root.join("results"))
            .with_context(|| format!("failed to create FYRE run directory {}", root.display()))
            .map_err(AppFailure::from)?;
        copy_tree(
            &self.config.asset_root().join("benchmarks/fyre/terraform"),
            &root.join(TERRAFORM_DIRECTORY),
        )
        .map_err(AppFailure::from)?;
        let config_path = root.join("config.json");
        write_json(&config_path, &config).map_err(AppFailure::from)?;
        let mut state = RunState {
            schema_version: 1,
            run_id: run_id.clone(),
            phase: "initializing".to_owned(),
            config_file: source,
            current_scenario: None,
            locust_helper_size: 0,
            fast_time_helper_size: 0,
            completed_scenarios: Vec::new(),
            cleanup_required: true,
        };
        write_state(&root, &state).map_err(AppFailure::from)?;

        let terraform = terraform_binary().map_err(AppFailure::from)?;
        let init = self.fyre_environment(
            CommandSpec::new(&terraform)
                .args(["init", "-input=false"])
                .cwd(root.join(TERRAFORM_DIRECTORY)),
        );
        let primary = async {
            self.run_cancellable(&init).await?;
            let validate = self.fyre_environment(
                CommandSpec::new(&terraform)
                    .arg("validate")
                    .cwd(root.join(TERRAFORM_DIRECTORY)),
            );
            self.run_cancellable(&validate).await?;
            let matrix = tokio::time::timeout(
                Duration::from_secs(config.workload.maximum_campaign_seconds.into()),
                self.run_fyre_matrix(
                    &terraform,
                    &root,
                    &config_path,
                    &public_key,
                    &mut config,
                    &mut state,
                ),
            )
            .await;
            match matrix {
                Ok(result) => result?,
                Err(_) => {
                    if let Some(scenario) = state.current_scenario.as_deref() {
                        let _ = self
                            .collect_fyre_scenario(&root, &config_path, scenario)
                            .await;
                    }
                    return Err(AppFailure::from(anyhow::anyhow!(
                        "FYRE campaign exceeded its configured time bound"
                    )));
                }
            }
            write_json(
                &root.join("manifest.json"),
                &json!({
                    "schema_version": 1,
                    "run_id": run_id,
                    "configuration": config,
                    "state": state,
                    "terraform_lock": root.join(TERRAFORM_DIRECTORY).join(".terraform.lock.hcl"),
                }),
            )
            .map_err(AppFailure::from)
        }
        .await;

        state.phase = "collecting".to_owned();
        let _ = write_state(&root, &state);
        let report_result = if primary.is_ok() {
            self.generate_fyre_report(&root, &config_path).await
        } else {
            Ok(())
        };
        let primary = primary.and(report_result);
        state.phase = "destroying".to_owned();
        let _ = write_state(&root, &state);
        let cleanup = self.terraform_destroy(&terraform, &root).await;
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
        super::finish_with_cleanup(primary.err(), cleanup)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_fyre_matrix(
        &self,
        terraform: &OsString,
        root: &Path,
        config_path: &Path,
        public_key: &Path,
        config: &mut FyreConfig,
        state: &mut RunState,
    ) -> AppResult<()> {
        let public_key = fs::read_to_string(public_key)
            .context("failed to read FYRE SSH public key")
            .map_err(AppFailure::from)?;
        let mut index = 0;
        while index < config.scenarios.len() {
            let scenario = config.scenarios[index].clone();
            let locust = config.infrastructure.helper_sizes[state.locust_helper_size];
            let fast_time = config.infrastructure.helper_sizes[state.fast_time_helper_size];
            config.active_helper = Some(ActiveHelper {
                locust_cpu: locust.cpu,
                locust_memory_gb: locust.memory_gb,
                fast_time_cpu: fast_time.cpu,
                fast_time_memory_gb: fast_time.memory_gb,
            });
            write_json(config_path, config).map_err(AppFailure::from)?;
            state.current_scenario = Some(scenario.id.clone());
            state.phase = "provisioning".to_owned();
            write_state(root, state).map_err(AppFailure::from)?;
            let variables = terraform_variables(
                &state.run_id,
                config,
                &scenario,
                &public_key,
                self.fyre_text("FYRE_PRODUCT_GROUP_ID"),
                self.fyre_text("FYRE_SITE"),
            );
            write_json(&root.join(TERRAFORM_VARIABLES), &variables).map_err(AppFailure::from)?;
            let apply = self.fyre_environment(
                CommandSpec::new(terraform)
                    .args(["apply", "-input=false", "-auto-approve", "-var-file"])
                    .arg(root.join(TERRAFORM_VARIABLES))
                    .cwd(root.join(TERRAFORM_DIRECTORY)),
            );
            self.run_cancellable(&apply).await?;
            let inventory = self.terraform_inventory(terraform, root)?;
            let scenario_root = root.join("results").join(&scenario.id);
            if scenario_root.exists() {
                fs::remove_dir_all(&scenario_root)
                    .with_context(|| format!("failed to reset {}", scenario_root.display()))
                    .map_err(AppFailure::from)?;
            }
            fs::create_dir_all(&scenario_root)
                .with_context(|| format!("failed to create {}", scenario_root.display()))
                .map_err(AppFailure::from)?;
            let inventory_path = scenario_root.join("inventory.json");
            write_json(&inventory_path, &inventory).map_err(AppFailure::from)?;
            state.phase = "benchmarking".to_owned();
            write_state(root, state).map_err(AppFailure::from)?;
            let campaign = self.fyre_campaign_command(root, config_path, &scenario.id);
            let campaign_result = self.run_cancellable(&campaign).await;
            if campaign_result.is_err() {
                let _ = self
                    .collect_fyre_scenario(root, config_path, &scenario.id)
                    .await;
            }
            match campaign_result {
                Ok(()) => {
                    state.completed_scenarios.push(scenario.id);
                    write_state(root, state).map_err(AppFailure::from)?;
                    index += 1;
                }
                Err(AppFailure::Infrastructure(
                    crate::infrastructure::InfrastructureError::ChildExit { status, .. },
                )) if status.code() == Some(HELPER_SATURATION_EXIT) => {
                    let request: Value = serde_json::from_slice(
                        &fs::read(scenario_root.join("helper-request.json"))
                            .context("helper saturation did not produce helper-request.json")
                            .map_err(AppFailure::from)?,
                    )
                    .context("invalid helper saturation request")
                    .map_err(AppFailure::from)?;
                    let role = request["role"].as_str().ok_or_else(|| {
                        AppFailure::from(anyhow::anyhow!(
                            "helper saturation request has an unknown role"
                        ))
                    })?;
                    let size = match role {
                        "locust" => &mut state.locust_helper_size,
                        "fast-time" => &mut state.fast_time_helper_size,
                        _ => {
                            return Err(AppFailure::from(anyhow::anyhow!(
                                "helper saturation request has an unknown role"
                            )));
                        }
                    };
                    *size += 1;
                    if *size >= config.infrastructure.helper_sizes.len() {
                        return Err(AppFailure::from(anyhow::anyhow!(
                            "helper headroom is inconclusive: the saturated helper reached the configured 16 vCPU / 32 GB limit"
                        )));
                    }
                    let archive = root
                        .join("invalidated")
                        .join(format!("{role}-size-{}-at-{}", *size, scenario.id));
                    fs::create_dir_all(&archive)
                        .with_context(|| format!("failed to create {}", archive.display()))
                        .map_err(AppFailure::from)?;
                    for scenario in &config.scenarios {
                        let path = root.join("results").join(&scenario.id);
                        if path.exists() {
                            let archived = archive.join(&scenario.id);
                            fs::rename(&path, &archived)
                                .with_context(|| {
                                    format!(
                                        "failed to archive {} as {}",
                                        path.display(),
                                        archived.display()
                                    )
                                })
                                .map_err(AppFailure::from)?;
                        }
                    }
                    state.completed_scenarios.clear();
                    index = 0;
                }
                Err(error) => return Err(error),
            }
        }
        state.current_scenario = None;
        Ok(())
    }

    fn fyre_campaign_command(
        &self,
        root: &Path,
        config_path: &Path,
        scenario: &str,
    ) -> CommandSpec {
        let scenario_root = root.join("results").join(scenario);
        CommandSpec::new("python3")
            .arg(self.config.asset_root().join("benchmarks/fyre/campaign.py"))
            .arg("--config")
            .arg(config_path)
            .arg("--inventory")
            .arg(scenario_root.join("inventory.json"))
            .args(["--scenario", scenario])
            .arg("--deploy")
            .arg(self.config.asset_root().join("benchmarks/fyre/deploy"))
            .arg("--output")
            .arg(scenario_root)
    }

    async fn collect_fyre_scenario(
        &self,
        root: &Path,
        config_path: &Path,
        scenario: &str,
    ) -> AppResult<()> {
        let inventory = root.join("results").join(scenario).join("inventory.json");
        if !inventory.is_file() {
            return Ok(());
        }
        let collection = self
            .fyre_campaign_command(root, config_path, scenario)
            .arg("--collect-only");
        self.run_cancellable(&collection).await
    }

    fn terraform_inventory(&self, terraform: &OsString, root: &Path) -> AppResult<Value> {
        let command = self.fyre_environment(
            CommandSpec::new(terraform)
                .args(["output", "-json", "inventory"])
                .cwd(root.join(TERRAFORM_DIRECTORY)),
        );
        let output = self
            .runner
            .capture_stdout(&command)
            .map_err(AppFailure::from)?;
        serde_json::from_slice(&output)
            .context("Terraform inventory output is not valid JSON")
            .map_err(AppFailure::from)
    }

    async fn generate_fyre_report(&self, root: &Path, config: &Path) -> AppResult<()> {
        let command = CommandSpec::new("uv")
            .args(["run", "--with", "matplotlib==3.10.6"])
            .arg(self.config.asset_root().join("benchmarks/fyre/report.py"))
            .arg("--config")
            .arg(config)
            .arg("--results")
            .arg(root.join("results"));
        self.run_cancellable(&command).await
    }

    fn fyre_status(&self, run_id: &str) -> AppResult<()> {
        validate_run_id(run_id).map_err(AppFailure::from)?;
        let root = self.config.integration_dir().join("fyre").join(run_id);
        let state = read_owned_state(&root, run_id).map_err(AppFailure::from)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&state)
                .map_err(anyhow::Error::from)
                .map_err(AppFailure::from)?
        );
        Ok(())
    }

    async fn destroy_fyre(&self, run_id: &str) -> AppResult<()> {
        validate_run_id(run_id).map_err(AppFailure::from)?;
        let root = self.config.integration_dir().join("fyre").join(run_id);
        let mut state = read_owned_state(&root, run_id).map_err(AppFailure::from)?;
        let terraform = terraform_binary().map_err(AppFailure::from)?;
        state.phase = "destroying".to_owned();
        write_state(&root, &state).map_err(AppFailure::from)?;
        self.terraform_destroy(&terraform, &root).await?;
        state.phase = "destroyed".to_owned();
        state.cleanup_required = false;
        write_state(&root, &state).map_err(AppFailure::from)
    }

    async fn terraform_destroy(&self, terraform: &OsString, root: &Path) -> AppResult<()> {
        let variables = root.join(TERRAFORM_VARIABLES);
        if !variables.is_file() {
            return Ok(());
        }
        let command = self.fyre_environment(
            CommandSpec::new(terraform)
                .args(["destroy", "-input=false", "-auto-approve", "-var-file"])
                .arg(variables)
                .cwd(root.join(TERRAFORM_DIRECTORY)),
        );
        let mut last = None;
        for attempt in 0..3 {
            match self.run_cancellable(&command).await {
                Ok(()) => return Ok(()),
                Err(error) => last = Some(error),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_secs(2_u64.pow(attempt + 1))).await;
            }
        }
        Err(last.unwrap_or_else(|| AppFailure::from(anyhow::anyhow!("Terraform destroy failed"))))
    }

    async fn run_cancellable(&self, command: &CommandSpec) -> AppResult<()> {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let process = self.runner.run_async_cancellable(command, receiver);
        tokio::pin!(process);
        tokio::select! {
            result = &mut process => result.map_err(AppFailure::from),
            signal = tokio::signal::ctrl_c() => {
                signal.context("failed to install interrupt handler").map_err(AppFailure::from)?;
                sender.send_replace(true);
                process.await.map_err(AppFailure::from)
            }
        }
    }

    fn fyre_environment(&self, mut command: CommandSpec) -> CommandSpec {
        for key in [
            "FYRE_USERNAME",
            "FYRE_API_KEY",
            "FYRE_PRODUCT_GROUP_ID",
            "FYRE_SITE",
        ] {
            if let Some(value) = self
                .config
                .environment()
                .get(OsStr::new(key))
                .map(|value| value.value.clone())
            {
                command = command.env(key, value);
            }
        }
        command
    }

    fn fyre_text(&self, key: &str) -> Option<&str> {
        self.config
            .environment()
            .get(OsStr::new(key))
            .and_then(|value| value.value.to_str())
            .filter(|value| !value.is_empty())
    }

    fn require_fyre_credentials(&self) -> AppResult<()> {
        for key in ["FYRE_USERNAME", "FYRE_API_KEY"] {
            if self.fyre_text(key).is_none() {
                return Err(AppFailure::from(anyhow::anyhow!(
                    "{key} is required for FYRE provisioning"
                )));
            }
        }
        Ok(())
    }
}

fn terraform_variables(
    run_id: &str,
    config: &FyreConfig,
    scenario: &Scenario,
    public_key: &str,
    product_group_id: Option<&str>,
    site: Option<&str>,
) -> Value {
    let helpers = config
        .active_helper
        .as_ref()
        .expect("active helper must be set");
    json!({
        "run_id": run_id,
        "os": config.infrastructure.os,
        "ssh_public_key": public_key.trim(),
        "expiry_hours": config.infrastructure.expiry_hours,
        "dataplane_count": scenario.replicas,
        "dataplane_cpu": scenario.cpu,
        "dataplane_memory_gb": scenario.memory_gb,
        "locust_cpu": helpers.locust_cpu,
        "locust_memory_gb": helpers.locust_memory_gb,
        "fast_time_cpu": helpers.fast_time_cpu,
        "fast_time_memory_gb": helpers.fast_time_memory_gb,
        "product_group_id": product_group_id,
        "site": site,
    })
}

fn read_config(path: &Path) -> Result<FyreConfig> {
    let source = fs::read(path)
        .with_context(|| format!("failed to read FYRE configuration {}", path.display()))?;
    yaml_serde::from_slice(&source)
        .with_context(|| format!("failed to parse FYRE configuration {}", path.display()))
}

fn validate_config(config: &FyreConfig) -> Result<()> {
    ensure!(
        config.schema_version == 1,
        "unsupported FYRE configuration schema"
    );
    ensure!(
        config.infrastructure.os == "Ubuntu 24.04",
        "FYRE benchmark OS must be Ubuntu 24.04"
    );
    ensure!(
        config.infrastructure.expiry_hours == 8,
        "FYRE expiry must remain eight hours"
    );
    ensure!(
        config.workload.protocol_version == "2026-07-28",
        "FYRE load supports only modern 2026-07-28"
    );
    ensure!(
        config.workload.first_users >= 125,
        "FYRE load must start at 125 users or more"
    );
    ensure!(
        config.workload.maximum_users <= 32_000,
        "FYRE load must be bounded at 32,000 users"
    );
    ensure!(
        config.workload.maximum_campaign_seconds <= 21_600,
        "FYRE campaign must be bounded at six hours"
    );
    ensure!(
        config.workload.repetitions == 3,
        "candidate capacity must use three repetitions"
    );
    let expected_tools = BTreeSet::from([
        "convert_time",
        "echo",
        "get_stats",
        "get_system_time",
        "schema_success",
        "verify-protocol",
    ]);
    let actual_tools = config
        .workload
        .tools
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    ensure!(
        actual_tools == expected_tools,
        "FYRE workload must contain the six nonfailure Fast Time tools"
    );
    ensure!(
        !config.infrastructure.helper_sizes.is_empty(),
        "at least one helper size is required"
    );
    let maximum = config
        .infrastructure
        .helper_sizes
        .last()
        .expect("nonempty helper sizes");
    ensure!(
        maximum.cpu <= 16 && maximum.memory_gb <= 32,
        "helper resources exceed 16 vCPU / 32 GB"
    );
    let mut ids = BTreeSet::<&str>::new();
    for scenario in &config.scenarios {
        validate_run_id(&scenario.id)?;
        ensure!(
            ids.insert(scenario.id.as_str()),
            "duplicate FYRE scenario {}",
            scenario.id
        );
        ensure!(
            scenario.replicas > 0 && scenario.cpu > 0 && scenario.memory_gb > 0,
            "scenario {} has zero resources",
            scenario.id
        );
        ensure!(
            scenario.replicas * scenario.cpu == scenario.multiplier * 2,
            "scenario {} CPU total does not match its multiplier",
            scenario.id
        );
        ensure!(
            scenario.replicas * scenario.memory_gb == scenario.multiplier * 8,
            "scenario {} memory total does not match its multiplier",
            scenario.id
        );
    }
    ensure!(ids.contains("baseline"), "FYRE matrix requires baseline");
    for image in [
        &config.images.dataplane,
        &config.images.fast_time,
        &config.images.helpers,
        &config.images.locust,
        &config.images.redis,
    ] {
        ensure!(
            image.contains("@sha256:"),
            "all benchmark images must be pinned by digest"
        );
    }
    Ok(())
}

fn validate_run_id(run_id: &str) -> Result<()> {
    ensure!(
        !run_id.is_empty()
            && run_id.len() <= 48
            && run_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && run_id
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && run_id
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric),
        "run ID must contain only lowercase letters, digits, and internal hyphens"
    );
    Ok(())
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let text = path.to_str().context("SSH key path must be UTF-8")?;
    if text == "~" || text.starts_with("~/") {
        let home = std::env::var_os("HOME").context("HOME is required to expand SSH key paths")?;
        return Ok(PathBuf::from(home).join(text.trim_start_matches("~/")));
    }
    Ok(path.to_path_buf())
}

fn ensure_file(path: &Path, label: &str) -> Result<()> {
    ensure!(path.is_file(), "{label} {} does not exist", path.display());
    Ok(())
}

fn terraform_binary() -> Result<OsString> {
    if let Some(binary) = std::env::var_os("CF_TERRAFORM_BIN") {
        ensure!(!binary.is_empty(), "CF_TERRAFORM_BIN must not be empty");
        return Ok(binary);
    }
    if std::process::Command::new("terraform")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return Ok(OsString::from("terraform"));
    }
    bail!("Terraform is required; set CF_TERRAFORM_BIN to its executable")
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .with_context(|| format!("failed to copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("failed to activate {}", path.display()))
}

fn write_state(root: &Path, state: &RunState) -> Result<()> {
    write_json(&root.join(OWNERSHIP_FILE), state)
}

fn read_owned_state(root: &Path, expected_run_id: &str) -> Result<RunState> {
    let path = root.join(OWNERSHIP_FILE);
    let state: RunState = serde_json::from_slice(
        &fs::read(&path)
            .with_context(|| format!("FYRE run state {} does not exist", path.display()))?,
    )
    .context("invalid FYRE run state")?;
    ensure!(
        state.run_id == expected_run_id,
        "FYRE run ownership mismatch; refusing cleanup"
    );
    ensure!(
        root.join(TERRAFORM_DIRECTORY).is_dir(),
        "FYRE Terraform state directory is missing; refusing cleanup"
    );
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_matrix_is_valid_and_matched() {
        let config = read_config(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("benchmarks/fyre/scaling.yaml")
                .as_path(),
        )
        .expect("packaged FYRE config");
        validate_config(&config).expect("valid FYRE config");
        assert_eq!(config.scenarios.len(), 6);
    }

    #[test]
    fn cleanup_requires_matching_owned_state() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::create_dir(directory.path().join(TERRAFORM_DIRECTORY)).expect("terraform directory");
        let state = RunState {
            schema_version: 1,
            run_id: "owned-run".to_owned(),
            phase: "failed".to_owned(),
            config_file: PathBuf::from("config.yaml"),
            current_scenario: None,
            locust_helper_size: 0,
            fast_time_helper_size: 0,
            completed_scenarios: Vec::new(),
            cleanup_required: true,
        };
        write_state(directory.path(), &state).expect("state");
        let error =
            read_owned_state(directory.path(), "another-run").expect_err("ownership mismatch");
        assert!(error.to_string().contains("ownership mismatch"));
    }

    #[test]
    fn run_ids_reject_paths_and_uppercase() {
        for invalid in ["../manual-vm", "UPPER", "-leading", "trailing-"] {
            assert!(validate_run_id(invalid).is_err(), "{invalid}");
        }
    }
}
