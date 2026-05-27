use crate::automations::types::Automation;
use crate::conditions::{CONDITION_HTTP_TIMEOUT, ConditionConfig};
use crate::devices::reconcile::compute_effective;
use crate::energy::estimate_energy_cost_pence;
use crate::hooks::HookConfig;
use crate::schedules::ScheduleConfig;
use crate::migration::migrate_to_automations;
use crate::settings::Settings;
use crate::time::now_ms;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tapoctl::{DeviceConfig, DeviceModel, DeviceSnapshot, TapoController, TapoCredentials};
use tokio::sync::{Mutex, RwLock, watch};
use tracing::{info, warn};

pub(crate) const STATE_VERSION: u32 = 2;

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) controller: TapoController,
    pub(crate) credentials: TapoCredentials,
    pub(crate) devices: Arc<RwLock<BTreeMap<String, ManagedDevice>>>,
    pub(crate) device_locks: Arc<RwLock<BTreeMap<IpAddr, Arc<Mutex<()>>>>>,
    pub(crate) device_events: watch::Sender<DeviceListResponse>,
    pub(crate) schedules: Arc<RwLock<BTreeMap<String, ScheduleConfig>>>,
    pub(crate) conditions: Arc<RwLock<BTreeMap<String, ConditionConfig>>>,
    pub(crate) device_intents: Arc<RwLock<BTreeMap<String, DeviceIntent>>>,
    pub(crate) hooks: Arc<RwLock<BTreeMap<String, HookConfig>>>,
    pub(crate) automations: Arc<RwLock<BTreeMap<String, Automation>>>,
    pub(crate) http_client: reqwest::Client,
    pub(crate) discovery_timeout_seconds: u64,
    pub(crate) discovery_targets: Vec<String>,
    pub(crate) refresh_seconds: u64,
    pub(crate) scan_seconds: u64,
    pub(crate) energy_price_pence_per_kwh: f64,
    pub(crate) state_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedDevice {
    pub(crate) name: String,
    pub(crate) config: DeviceConfig,
    pub(crate) snapshot: Option<DeviceSnapshot>,
    pub(crate) last_error: Option<String>,
    pub(crate) discovered_at_ms: u128,
    pub(crate) updated_at_ms: Option<u128>,
    /// Number of consecutive refresh failures since the last successful
    /// read. Used to debounce flaky LAN behaviour before declaring the
    /// device offline. Not persisted — resets on server restart.
    pub(crate) consecutive_failures: u32,
    /// True once we've fired an Offline hook event for the current
    /// outage. Prevents repeated Offline events and gates the next
    /// Online event.
    pub(crate) offline_announced: bool,
}

pub(crate) const DEVICE_OFFLINE_FAILURE_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DeviceListResponse {
    pub(crate) devices: Vec<DeviceView>,
    pub(crate) updated_at_ms: u128,
    pub(crate) energy_price_pence_per_kwh: f64,
    pub(crate) scan_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DeviceView {
    pub(crate) name: String,
    pub(crate) ip: String,
    pub(crate) configured_model: DeviceModel,
    pub(crate) model: String,
    pub(crate) nickname: String,
    pub(crate) device_type: String,
    pub(crate) device_on: Option<bool>,
    pub(crate) on_time_seconds: Option<u64>,
    pub(crate) energy: Option<EnergyView>,
    pub(crate) last_error: Option<String>,
    pub(crate) discovered_at_ms: u128,
    pub(crate) updated_at_ms: Option<u128>,
    pub(crate) manual_override: Option<bool>,
    pub(crate) manual_override_until_ms: Option<u128>,
    pub(crate) schedule_intent: Option<bool>,
    pub(crate) condition_intent: Option<bool>,
    pub(crate) effective_intent: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EnergyView {
    pub(crate) current_power_mw: Option<u64>,
    pub(crate) current_power_w: Option<u64>,
    pub(crate) today_energy_wh: u64,
    pub(crate) month_energy_wh: u64,
    pub(crate) today_cost_pence: f64,
    pub(crate) month_cost_pence: f64,
    pub(crate) today_runtime_minutes: u64,
    pub(crate) month_runtime_minutes: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ScheduleAction {
    On,
    Off,
    Toggle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ScheduleKind {
    #[default]
    Cron,
    Interval,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedState {
    pub(crate) version: u32,
    pub(crate) devices: BTreeMap<String, DeviceConfig>,
    #[serde(default)]
    pub(crate) schedules: BTreeMap<String, ScheduleConfig>,
    #[serde(default)]
    pub(crate) conditions: BTreeMap<String, ConditionConfig>,
    #[serde(default)]
    pub(crate) device_intents: BTreeMap<String, DeviceIntent>,
    #[serde(default)]
    pub(crate) hooks: BTreeMap<String, HookConfig>,
    #[serde(default)]
    pub(crate) automations: BTreeMap<String, Automation>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct DeviceIntent {
    #[serde(default)]
    pub(crate) schedule_intent: Option<bool>,
    #[serde(default)]
    pub(crate) manual_override: Option<bool>,
    /// If set, the manual override is cleared automatically at this
    /// epoch-ms timestamp. None means the override sticks until the
    /// user releases it or a schedule fire overwrites it.
    #[serde(default)]
    pub(crate) manual_override_until_ms: Option<u128>,
}

pub(crate) const DEFAULT_MANUAL_OVERRIDE_SECONDS: u64 = 3600;

pub(crate) const MIN_MANUAL_OVERRIDE_SECONDS: u64 = 30;

pub(crate) const MAX_MANUAL_OVERRIDE_SECONDS: u64 = 24 * 3600;

impl AppState {
    pub(crate) fn new(settings: &Settings) -> Self {
        let credentials = TapoCredentials {
            username: settings.username.clone(),
            password: settings.password.clone(),
        };
        let controller = TapoController::new(credentials.clone());
        let (device_events, _device_event_receiver) = watch::channel(DeviceListResponse {
            devices: Vec::new(),
            updated_at_ms: now_ms(),
            energy_price_pence_per_kwh: settings.energy_price_pence_per_kwh,
            scan_error: None,
        });

        let http_client = reqwest::Client::builder()
            .timeout(CONDITION_HTTP_TIMEOUT)
            .user_agent("fusebox/condition-poller")
            .build()
            .expect("reqwest client builds with valid defaults");

        Self {
            controller,
            credentials,
            devices: Arc::new(RwLock::new(BTreeMap::new())),
            device_locks: Arc::new(RwLock::new(BTreeMap::new())),
            device_events,
            schedules: Arc::new(RwLock::new(BTreeMap::new())),
            conditions: Arc::new(RwLock::new(BTreeMap::new())),
            device_intents: Arc::new(RwLock::new(BTreeMap::new())),
            hooks: Arc::new(RwLock::new(BTreeMap::new())),
            automations: Arc::new(RwLock::new(BTreeMap::new())),
            http_client,
            discovery_timeout_seconds: settings.discovery_timeout_seconds,
            discovery_targets: settings.discovery_targets.clone(),
            refresh_seconds: settings.refresh_seconds,
            scan_seconds: settings.scan_seconds,
            energy_price_pence_per_kwh: settings.energy_price_pence_per_kwh,
            state_path: settings.state_path.clone(),
        }
    }
}

impl ManagedDevice {
    pub(crate) fn view(
        &self,
        energy_price_pence_per_kwh: f64,
        intent: DeviceIntent,
        condition_intent: Option<bool>,
    ) -> DeviceView {
        let snapshot = self.snapshot.as_ref();
        let effective_intent = compute_effective(
            intent.manual_override,
            intent.schedule_intent,
            condition_intent,
        );

        DeviceView {
            name: self.name.clone(),
            ip: self.config.ip.to_string(),
            configured_model: self.config.model,
            model: snapshot
                .map(|snapshot| snapshot.device_model.clone())
                .unwrap_or_else(|| self.config.model.to_string()),
            nickname: snapshot
                .map(|snapshot| snapshot.nickname.clone())
                .unwrap_or_else(|| self.name.clone()),
            device_type: snapshot
                .map(|snapshot| snapshot.device_type.clone())
                .unwrap_or_else(|| "Tapo device".to_string()),
            device_on: snapshot.map(|snapshot| snapshot.device_on),
            on_time_seconds: snapshot.map(|snapshot| snapshot.on_time_seconds),
            energy: snapshot.and_then(|snapshot| {
                snapshot.energy.as_ref().map(|energy| EnergyView {
                    current_power_mw: energy.current_power_mw,
                    current_power_w: energy.current_power_w,
                    today_energy_wh: energy.today_energy_wh,
                    month_energy_wh: energy.month_energy_wh,
                    today_cost_pence: estimate_energy_cost_pence(
                        energy.today_energy_wh,
                        energy_price_pence_per_kwh,
                    ),
                    month_cost_pence: estimate_energy_cost_pence(
                        energy.month_energy_wh,
                        energy_price_pence_per_kwh,
                    ),
                    today_runtime_minutes: energy.today_runtime_minutes,
                    month_runtime_minutes: energy.month_runtime_minutes,
                })
            }),
            last_error: self.last_error.clone(),
            discovered_at_ms: self.discovered_at_ms,
            updated_at_ms: self.updated_at_ms,
            manual_override: intent.manual_override,
            manual_override_until_ms: intent.manual_override_until_ms,
            schedule_intent: intent.schedule_intent,
            condition_intent,
            effective_intent,
        }
    }
}

pub(crate) async fn load_persisted_state(state: &AppState) -> Result<()> {
    let contents = match fs::read_to_string(&state.state_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            info!(path = %state.state_path.display(), "no persisted state found");
            return Ok(());
        }
        Err(error) => return Err(error).context("failed to read persisted state"),
    };

    let mut persisted: PersistedState =
        serde_json::from_str(&contents).context("failed to parse persisted state")?;

    let mut migrated_from: Option<u32> = None;
    if persisted.version < STATE_VERSION {
        migrated_from = Some(persisted.version);
        migrate_to_automations(&mut persisted);
        persisted.version = STATE_VERSION;
    } else if persisted.version > STATE_VERSION {
        return Err(anyhow!(
            "unsupported state version {}; expected {}",
            persisted.version,
            STATE_VERSION,
        ));
    }

    let loaded_count = persisted.devices.len();
    let loaded_schedule_count = persisted.schedules.len();
    let loaded_condition_count = persisted.conditions.len();
    let loaded_automation_count = persisted.automations.len();
    {
        let mut devices = state.devices.write().await;

        for (name, config) in persisted.devices {
            devices.insert(name.clone(), managed_device_from_config(name, config));
        }
    }

    {
        let mut schedules = state.schedules.write().await;
        for (id, schedule) in persisted.schedules {
            schedules.insert(id, schedule);
        }
    }

    {
        let mut conditions = state.conditions.write().await;
        for (id, condition) in persisted.conditions {
            conditions.insert(id, condition);
        }
    }

    {
        let mut intents = state.device_intents.write().await;
        for (name, intent) in persisted.device_intents {
            intents.insert(name, intent);
        }
    }

    {
        let mut hooks = state.hooks.write().await;
        for (id, hook) in persisted.hooks {
            hooks.insert(id, hook);
        }
    }

    {
        let mut automations = state.automations.write().await;
        for (id, automation) in persisted.automations {
            automations.insert(id, automation);
        }
    }

    info!(
        loaded_count,
        loaded_schedule_count,
        loaded_condition_count,
        loaded_automation_count,
        migrated_from = ?migrated_from,
        path = %state.state_path.display(),
        "loaded persisted state",
    );

    if migrated_from.is_some() {
        if let Err(error) = save_persisted_state(state).await {
            warn!(%error, "failed to persist state after automation migration");
        } else {
            info!("persisted migrated state at new version {STATE_VERSION}");
        }
    }
    Ok(())
}

pub(crate) async fn save_persisted_state(state: &AppState) -> Result<()> {
    let persisted = {
        let devices = state.devices.read().await;
        let schedules = state.schedules.read().await;
        let conditions = state.conditions.read().await;
        let intents = state.device_intents.read().await;
        let hooks = state.hooks.read().await;
        let automations = state.automations.read().await;

        PersistedState {
            version: STATE_VERSION,
            devices: devices
                .iter()
                .map(|(name, device)| (name.clone(), device.config.clone()))
                .collect(),
            schedules: schedules.clone(),
            conditions: conditions.clone(),
            device_intents: intents.clone(),
            hooks: hooks.clone(),
            automations: automations.clone(),
        }
    };

    write_json_atomically(&state.state_path, &persisted)
}

pub(crate) fn managed_device_from_config(name: String, config: DeviceConfig) -> ManagedDevice {
    ManagedDevice {
        name,
        config,
        snapshot: None,
        last_error: None,
        discovered_at_ms: now_ms(),
        updated_at_ms: None,
        consecutive_failures: 0,
        offline_announced: false,
    }
}

pub(crate) fn write_json_atomically<T>(path: &FsPath, value: &T) -> Result<()>
where
    T: Serialize,
{
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create state directory {}", parent.display()))?;
    }

    let temp_path = temporary_path_for(path)?;
    let mut contents = serde_json::to_string_pretty(value).context("failed to serialize state")?;
    contents.push('\n');

    fs::write(&temp_path, contents).with_context(|| {
        format!(
            "failed to write temporary state file {}",
            temp_path.display()
        )
    })?;
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to move temporary state file {} to {}",
            temp_path.display(),
            path.display(),
        )
    })?;

    Ok(())
}

pub(crate) fn temporary_path_for(path: &FsPath) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("state path must include a file name"))?;
    let mut temporary_name = OsString::from(file_name);
    temporary_name.push(".tmp");

    Ok(path.with_file_name(temporary_name))
}
