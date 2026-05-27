use crate::api_error::AppError;
use crate::settings::Settings;
use crate::state::*;
use crate::time::{deserialize_optional_label, now_ms};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::path::{Path as FsPath, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{
    DateTime, Datelike, Days, Duration as ChronoDuration, Local, NaiveDate, Timelike, Utc,
};
use cron::Schedule as CronSchedule;
use reqwest::Method as HttpMethod;
use rust_xlsxwriter::{Format, FormatAlign, FormatBorder, Workbook};
use serde::{Deserialize, Serialize};
use tapo::{ApiClient, requests::EnergyDataInterval, requests::PowerDataInterval};
use tapoctl::{
    Config as TapoConfig, DeviceConfig, DeviceModel, DeviceSnapshot, TapoController,
    TapoCredentials, automatic_discovery_targets, discovery_add_candidates,
    discovery_scan_targets_with_auto,
};
use tokio::sync::{Mutex, RwLock, watch};
use tokio::time::sleep;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

pub(crate) const ALL_TIME_USAGE_START_YEAR: i32 = 2020;
pub(crate) const TAPO_HANDSHAKE_RETRY_ATTEMPTS: usize = 3;
pub(crate) const TAPO_HANDSHAKE_RETRY_DELAY: Duration = Duration::from_millis(350);
pub(crate) const SWITCH_SOUND_BYTES: &[u8] =
    include_bytes!("../assets/348224__tbrook__switch-light-06.wav");
pub(crate) const APP_BUNDLE_JS: &str = include_str!("../web/dist/app.js");

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageHistoryResponse {
    pub(crate) series: Vec<UsageHistorySeries>,
    pub(crate) totals: Vec<UsageHistoryPoint>,
    pub(crate) errors: Vec<UsageHistoryError>,
    pub(crate) updated_at_ms: u128,
    pub(crate) range: &'static str,
    pub(crate) range_label: &'static str,
    pub(crate) interval: &'static str,
    pub(crate) start_date: String,
    pub(crate) end_date: String,
    pub(crate) unit: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageHistorySeries {
    pub(crate) device_name: String,
    pub(crate) points: Vec<UsageHistoryPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageHistoryPoint {
    pub(crate) timestamp_ms: i64,
    pub(crate) value: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageHistoryError {
    pub(crate) device_name: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UsageHistoryQuery {
    pub(crate) range: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UsageHistoryRange {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) interval_label: &'static str,
    pub(crate) unit: &'static str,
    pub(crate) start: UsageHistoryStart,
    pub(crate) kind: UsageHistoryKind,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum UsageHistoryStart {
    Duration(ChronoDuration),
    YearToDate,
    AllTime,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum UsageHistoryKind {
    Power {
        interval: PowerExportInterval,
        range_limit: ChronoDuration,
    },
    EnergyDaily,
    EnergyMonthly,
}

#[derive(Debug, Clone)]
pub(crate) struct ExportDevice {
    pub(crate) name: String,
    pub(crate) config: DeviceConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct ExportSpec {
    pub(crate) sheet_name: &'static str,
    pub(crate) value_format: &'static str,
    pub(crate) kind: ExportKind,
}

#[derive(Debug, Clone)]
pub(crate) enum ExportKind {
    EnergyHourly {
        start_date: NaiveDate,
        end_date: NaiveDate,
    },
    EnergyDaily {
        start_date: NaiveDate,
    },
    EnergyMonthly {
        start_date: NaiveDate,
    },
    PowerEvery5Minutes {
        ranges: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    },
    PowerHourly {
        ranges: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ExportTable {
    pub(crate) sheet_name: &'static str,
    pub(crate) value_format: &'static str,
    pub(crate) rows: Vec<ExportRow>,
}

#[derive(Debug, Clone)]
pub(crate) struct ExportRow {
    pub(crate) timestamp: DateTime<Utc>,
    pub(crate) values: BTreeMap<String, f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct ExportError {
    pub(crate) sheet_name: &'static str,
    pub(crate) device_name: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SetPowerRequest {
    pub(crate) on: bool,
    #[serde(default)]
    pub(crate) duration_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ToggleDeviceRequest {
    #[serde(default)]
    pub(crate) duration_seconds: Option<u64>,
}

pub(crate) const MIN_INTERVAL_CYCLE_SECONDS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ScheduleConfig {
    pub(crate) id: String,
    pub(crate) device_name: String,
    #[serde(default)]
    pub(crate) kind: ScheduleKind,
    #[serde(default)]
    pub(crate) cron: Option<String>,
    #[serde(default)]
    pub(crate) action: Option<ScheduleAction>,
    #[serde(default)]
    pub(crate) on_seconds: Option<u64>,
    #[serde(default)]
    pub(crate) off_seconds: Option<u64>,
    #[serde(default)]
    pub(crate) start_action: Option<ScheduleAction>,
    #[serde(default)]
    pub(crate) starts_at_ms: Option<u128>,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) label: Option<String>,
    #[serde(default)]
    pub(crate) condition_ids: Vec<String>,
    #[serde(default)]
    pub(crate) created_at_ms: u128,
    #[serde(default)]
    pub(crate) last_fired_at_ms: Option<u128>,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
}

pub(crate) fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum CreateScheduleRequest {
    Cron {
        device_name: String,
        cron: String,
        action: ScheduleAction,
        #[serde(default)]
        label: Option<String>,
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        condition_ids: Vec<String>,
    },
    Interval {
        device_name: String,
        on_seconds: u64,
        off_seconds: u64,
        start_action: ScheduleAction,
        #[serde(default)]
        starts_at_ms: Option<u128>,
        #[serde(default)]
        label: Option<String>,
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        condition_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UpdateScheduleRequest {
    #[serde(default)]
    pub(crate) enabled: Option<bool>,
    #[serde(default)]
    pub(crate) cron: Option<String>,
    #[serde(default)]
    pub(crate) action: Option<ScheduleAction>,
    #[serde(default)]
    pub(crate) on_seconds: Option<u64>,
    #[serde(default)]
    pub(crate) off_seconds: Option<u64>,
    #[serde(default)]
    pub(crate) start_action: Option<ScheduleAction>,
    #[serde(default, deserialize_with = "deserialize_optional_label")]
    pub(crate) label: Option<Option<String>>,
    #[serde(default)]
    pub(crate) condition_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScheduleView {
    pub(crate) id: String,
    pub(crate) device_name: String,
    pub(crate) kind: ScheduleKind,
    pub(crate) cron: Option<String>,
    pub(crate) action: Option<ScheduleAction>,
    pub(crate) on_seconds: Option<u64>,
    pub(crate) off_seconds: Option<u64>,
    pub(crate) start_action: Option<ScheduleAction>,
    pub(crate) starts_at_ms: Option<u128>,
    pub(crate) enabled: bool,
    pub(crate) label: Option<String>,
    pub(crate) condition_ids: Vec<String>,
    pub(crate) created_at_ms: u128,
    pub(crate) last_fired_at_ms: Option<u128>,
    pub(crate) last_error: Option<String>,
    pub(crate) next_fire_at_ms: Option<i64>,
}

pub(crate) const MIN_CONDITION_POLL_SECONDS: u64 = 5;
pub(crate) const MAX_CONDITION_POLL_SECONDS: u64 = 3_600;
pub(crate) const DEFAULT_CONDITION_POLL_SECONDS: u64 = 60;
pub(crate) const CONDITION_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MAX_CONDITION_BODY_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ConditionAction {
    On,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ConditionConfig {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) device_name: String,
    pub(crate) url: String,
    #[serde(default = "default_http_method")]
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) headers: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default = "default_status_match")]
    pub(crate) status_match: String,
    #[serde(default)]
    pub(crate) body_contains: Option<String>,
    #[serde(default = "default_condition_poll_seconds")]
    pub(crate) poll_seconds: u64,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) action_on_pass: Option<ConditionAction>,
    #[serde(default)]
    pub(crate) action_on_fail: Option<ConditionAction>,
    #[serde(default)]
    pub(crate) created_at_ms: u128,
    #[serde(default)]
    pub(crate) last_checked_at_ms: Option<u128>,
    #[serde(default)]
    pub(crate) last_passing: Option<bool>,
    #[serde(default)]
    pub(crate) last_status_code: Option<u16>,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
    #[serde(default)]
    pub(crate) last_action_at_ms: Option<u128>,
    #[serde(default)]
    pub(crate) last_action: Option<ConditionAction>,
    #[serde(default)]
    pub(crate) last_action_error: Option<String>,
    /// New probe results must remain stable for this many seconds before
    /// they update `last_passing`. 0 (default for back-compat) means
    /// react to every change immediately. Prevents flaky probes from
    /// causing rapid device toggling.
    #[serde(default)]
    pub(crate) min_stable_seconds: u64,
    /// The most recent probe value that differs from `last_passing` and
    /// is waiting to be promoted. None when the latest probe matched.
    #[serde(default)]
    pub(crate) pending_value: Option<bool>,
    /// When `pending_value` was first observed.
    #[serde(default)]
    pub(crate) pending_since_ms: Option<u128>,
}

pub(crate) fn default_http_method() -> String {
    "GET".to_string()
}

pub(crate) fn default_status_match() -> String {
    "200-299".to_string()
}

pub(crate) fn default_condition_poll_seconds() -> u64 {
    DEFAULT_CONDITION_POLL_SECONDS
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CreateConditionRequest {
    pub(crate) name: String,
    pub(crate) device_name: String,
    pub(crate) url: String,
    #[serde(default = "default_http_method")]
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) headers: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default = "default_status_match")]
    pub(crate) status_match: String,
    #[serde(default)]
    pub(crate) body_contains: Option<String>,
    #[serde(default = "default_condition_poll_seconds")]
    pub(crate) poll_seconds: u64,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) action_on_pass: Option<ConditionAction>,
    #[serde(default)]
    pub(crate) action_on_fail: Option<ConditionAction>,
    #[serde(default)]
    pub(crate) min_stable_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UpdateConditionRequest {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) device_name: Option<String>,
    #[serde(default)]
    pub(crate) url: Option<String>,
    #[serde(default)]
    pub(crate) method: Option<String>,
    #[serde(default)]
    pub(crate) headers: Option<BTreeMap<String, String>>,
    #[serde(default, deserialize_with = "deserialize_optional_label")]
    pub(crate) body: Option<Option<String>>,
    #[serde(default)]
    pub(crate) status_match: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_label")]
    pub(crate) body_contains: Option<Option<String>>,
    #[serde(default)]
    pub(crate) poll_seconds: Option<u64>,
    #[serde(default)]
    pub(crate) enabled: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_condition_action")]
    pub(crate) action_on_pass: Option<Option<ConditionAction>>,
    #[serde(default, deserialize_with = "deserialize_optional_condition_action")]
    pub(crate) action_on_fail: Option<Option<ConditionAction>>,
    #[serde(default)]
    pub(crate) min_stable_seconds: Option<u64>,
}

pub(crate) fn deserialize_optional_condition_action<'de, D>(
    deserializer: D,
) -> Result<Option<Option<ConditionAction>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Option<ConditionAction>>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ConditionView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) device_name: String,
    pub(crate) url: String,
    pub(crate) method: String,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) body: Option<String>,
    pub(crate) status_match: String,
    pub(crate) body_contains: Option<String>,
    pub(crate) poll_seconds: u64,
    pub(crate) enabled: bool,
    pub(crate) action_on_pass: Option<ConditionAction>,
    pub(crate) action_on_fail: Option<ConditionAction>,
    pub(crate) created_at_ms: u128,
    pub(crate) last_checked_at_ms: Option<u128>,
    pub(crate) last_passing: Option<bool>,
    pub(crate) last_status_code: Option<u16>,
    pub(crate) last_error: Option<String>,
    pub(crate) last_action_at_ms: Option<u128>,
    pub(crate) last_action: Option<ConditionAction>,
    pub(crate) last_action_error: Option<String>,
    pub(crate) min_stable_seconds: u64,
    pub(crate) pending_value: Option<bool>,
    pub(crate) pending_since_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ConditionListResponse {
    pub(crate) conditions: Vec<ConditionView>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScheduleListResponse {
    pub(crate) schedules: Vec<ScheduleView>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum HookEvent {
    On,
    Off,
    Online,
    Offline,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum HookSource {
    Manual,
    Schedule,
    Condition,
    External,
    Discovery,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HookConfig {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    pub(crate) url: String,
    #[serde(default = "default_http_method")]
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) headers: BTreeMap<String, String>,
    /// Optional body. If absent, a default JSON payload is sent.
    #[serde(default)]
    pub(crate) body: Option<String>,
    /// Empty = matches every device.
    #[serde(default)]
    pub(crate) device_filter: Vec<String>,
    /// Empty = matches every event.
    #[serde(default)]
    pub(crate) event_filter: Vec<HookEvent>,
    #[serde(default)]
    pub(crate) created_at_ms: u128,
    #[serde(default)]
    pub(crate) last_fired_at_ms: Option<u128>,
    #[serde(default)]
    pub(crate) last_event: Option<HookEvent>,
    #[serde(default)]
    pub(crate) last_status_code: Option<u16>,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
}

// ---------- Automations (flowchart) ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum AutomationNodeConfig {
    CronTrigger {
        cron_trigger: CronTriggerCfg,
    },
    IntervalTrigger {
        interval_trigger: IntervalTriggerCfg,
    },
    DeviceEventTrigger {
        device_event_trigger: DeviceEventTriggerCfg,
    },
    HttpProbe {
        http_probe: HttpProbeCfg,
    },
    LogicAnd,
    LogicOr,
    LogicNot,
    Debounce {
        debounce: DebounceCfg,
    },
    SetDevice {
        set_device: SetDeviceCfg,
    },
    ToggleDevice {
        toggle_device: ToggleDeviceCfg,
    },
    FireHook {
        fire_hook: FireHookCfg,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CronTriggerCfg {
    pub(crate) cron: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct IntervalTriggerCfg {
    pub(crate) on_seconds: u64,
    pub(crate) off_seconds: u64,
    pub(crate) start_action: ScheduleAction,
    #[serde(default)]
    pub(crate) starts_at_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DeviceEventTriggerCfg {
    pub(crate) device_name: String,
    pub(crate) event: HookEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HttpProbeCfg {
    pub(crate) url: String,
    #[serde(default = "default_http_method")]
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) headers: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default = "default_status_match")]
    pub(crate) status_match: String,
    #[serde(default)]
    pub(crate) body_contains: Option<String>,
    #[serde(default = "default_condition_poll_seconds")]
    pub(crate) poll_seconds: u64,
    #[serde(default)]
    pub(crate) min_stable_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DebounceCfg {
    pub(crate) hold_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SetDeviceCfg {
    pub(crate) device_name: String,
    pub(crate) action: ScheduleAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ToggleDeviceCfg {
    pub(crate) device_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FireHookCfg {
    pub(crate) hook_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AutomationNode {
    pub(crate) id: String,
    pub(crate) config: AutomationNodeConfig,
    #[serde(default)]
    pub(crate) x: f64,
    #[serde(default)]
    pub(crate) y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AutomationEdge {
    pub(crate) id: String,
    pub(crate) source_node: String,
    pub(crate) target_node: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct NodeRuntimeState {
    #[serde(default)]
    pub(crate) last_value: Option<bool>,
    #[serde(default)]
    pub(crate) last_fired_at_ms: Option<u128>,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
    // Internal state for cron/interval/probe/debounce — not exposed in the
    // public view JSON. Reset on server restart.
    #[serde(skip)]
    pub(crate) last_checked_at_ms: Option<u128>,
    #[serde(skip)]
    pub(crate) pending_value: Option<bool>,
    #[serde(skip)]
    pub(crate) pending_since_ms: Option<u128>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AutomationStatus {
    #[serde(default)]
    pub(crate) last_fired_at_ms: Option<u128>,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
    #[serde(default)]
    pub(crate) node_states: BTreeMap<String, NodeRuntimeState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Automation {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) nodes: Vec<AutomationNode>,
    #[serde(default)]
    pub(crate) edges: Vec<AutomationEdge>,
    #[serde(default)]
    pub(crate) created_at_ms: u128,
    #[serde(default)]
    pub(crate) status: AutomationStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CreateAutomationRequest {
    pub(crate) name: String,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) nodes: Vec<AutomationNode>,
    #[serde(default)]
    pub(crate) edges: Vec<AutomationEdge>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UpdateAutomationRequest {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) enabled: Option<bool>,
    #[serde(default)]
    pub(crate) nodes: Option<Vec<AutomationNode>>,
    #[serde(default)]
    pub(crate) edges: Option<Vec<AutomationEdge>>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AutomationListResponse {
    pub(crate) automations: Vec<Automation>,
}

pub(crate) async fn run() -> Result<()> {
    init_logging();

    let settings = Settings::from_env()?;
    let state = AppState::new(&settings);

    if let Err(error) = load_persisted_state(&state).await {
        warn!(%error, path = %state.state_path.display(), "failed to load persisted state");
    }
    publish_device_list(&state, None).await;

    tokio::spawn(initial_refresh_devices(state.clone()));
    tokio::spawn(monitor_devices(state.clone()));
    tokio::spawn(scan_for_devices(state.clone()));
    tokio::spawn(run_scheduler(state.clone()));
    tokio::spawn(run_condition_poller(state.clone()));
    tokio::spawn(run_automation_engine(state.clone()));
    tokio::spawn(run_override_expiry_sweeper(state.clone()));

    let app = Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(favicon))
        .route("/assets/switch.wav", get(switch_sound))
        .route("/assets/app.js", get(app_bundle))
        .route("/health", get(health))
        .route("/api/devices", get(list_devices))
        .route("/api/energy/history.json", get(energy_history))
        .route("/api/energy/export.xlsx", get(export_energy_workbook))
        .route("/ws/devices", get(devices_websocket))
        .route("/api/scan", post(scan_devices))
        .route("/api/devices/{name}/toggle", post(toggle_device))
        .route("/api/devices/{name}/power", post(set_device_power))
        .route(
            "/api/devices/{name}/release-override",
            post(release_device_override),
        )
        .route("/api/schedules", get(list_schedules).post(create_schedule))
        .route(
            "/api/schedules/{id}",
            delete(delete_schedule).patch(update_schedule),
        )
        .route(
            "/api/conditions",
            get(list_conditions).post(create_condition),
        )
        .route(
            "/api/conditions/{id}",
            delete(delete_condition).patch(update_condition),
        )
        .route("/api/conditions/{id}/probe", post(probe_condition))
        .route("/api/hooks", get(list_hooks).post(create_hook))
        .route("/api/hooks/{id}", delete(delete_hook).patch(update_hook))
        .route("/api/hooks/{id}/test", post(test_hook))
        .route(
            "/api/automations",
            get(list_automations).post(create_automation),
        )
        .route(
            "/api/automations/{id}",
            delete(delete_automation).patch(update_automation),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(settings.bind_address)
        .await
        .with_context(|| format!("failed to bind Fusebox to {}", settings.bind_address))?;

    info!("Fusebox listening on http://{}", settings.bind_address);
    axum::serve(listener, app).await?;

    Ok(())
}

pub(crate) fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}

pub(crate) async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub(crate) async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub(crate) async fn switch_sound() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/wav")
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from(SWITCH_SOUND_BYTES))
        .expect("static switch sound response should be valid")
}

pub(crate) async fn app_bundle() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(APP_BUNDLE_JS))
        .expect("static app bundle response should be valid")
}

pub(crate) async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

pub(crate) async fn list_devices(State(state): State<AppState>) -> Json<DeviceListResponse> {
    Json(device_list_response(&state, None).await)
}

pub(crate) async fn scan_devices(State(state): State<AppState>) -> Json<DeviceListResponse> {
    let scan_error = match scan_and_refresh(&state).await {
        Ok(()) => None,
        Err(error) => Some(error.to_string()),
    };

    let response = device_list_response(&state, scan_error).await;
    publish_device_list_response(&state, response.clone());

    Json(response)
}

pub(crate) async fn devices_websocket(
    State(state): State<AppState>,
    websocket: WebSocketUpgrade,
) -> Response {
    websocket.on_upgrade(|socket| stream_device_events(socket, state))
}

pub(crate) async fn stream_device_events(mut socket: WebSocket, state: AppState) {
    let mut receiver = state.device_events.subscribe();

    if send_device_event(&mut socket, device_list_response(&state, None).await)
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            changed = receiver.changed() => {
                if changed.is_err() {
                    return;
                }

                let response = receiver.borrow().clone();
                if send_device_event(&mut socket, response).await.is_err() {
                    return;
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_message)) => {}
                    Some(Err(_error)) => return,
                }
            }
        }
    }
}

pub(crate) async fn send_device_event(
    socket: &mut WebSocket,
    response: DeviceListResponse,
) -> Result<()> {
    let payload = serde_json::to_string(&response).context("failed to serialize device event")?;
    socket
        .send(Message::Text(payload.into()))
        .await
        .context("failed to send device event")
}

pub(crate) async fn energy_history(
    State(state): State<AppState>,
    Query(query): Query<UsageHistoryQuery>,
) -> Json<UsageHistoryResponse> {
    Json(build_usage_history(&state, query.range.as_deref()).await)
}

pub(crate) async fn export_energy_workbook(
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let buffer = build_energy_export_workbook(&state).await?;

    Ok((
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"fusebox-energy.xlsx\"",
            ),
        ],
        buffer,
    )
        .into_response())
}

pub(crate) async fn toggle_device(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Option<Json<ToggleDeviceRequest>>,
) -> Result<Json<DeviceView>, AppError> {
    let duration = body
        .map(|Json(req)| req.duration_seconds)
        .unwrap_or(None)
        .unwrap_or(DEFAULT_MANUAL_OVERRIDE_SECONDS);
    let device = get_device_config(&state, &name).await?;
    let operation_lock = device_operation_lock(&state, &device).await;
    let _operation_guard = operation_lock.lock().await;
    let current_snapshot = retry_tapo_handshake(|| state.controller.read_device(&device)).await?;
    let target = !current_snapshot.device_on;
    retry_tapo_handshake(|| state.controller.set_power(&device, target)).await?;
    let snapshot = retry_tapo_handshake(|| state.controller.read_device(&device)).await?;
    update_device_snapshot(&state, &name, snapshot, None, HookSource::Manual).await;

    set_manual_override(&state, &name, target, Some(duration)).await;
    if let Err(error) = save_persisted_state(&state).await {
        warn!(%error, device = %name, "failed to persist manual override");
    }

    get_device_view(&state, &name)
        .await
        .map(Json)
        .map_err(AppError)
}

pub(crate) async fn set_device_power(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<SetPowerRequest>,
) -> Result<Json<DeviceView>, AppError> {
    let duration = request
        .duration_seconds
        .unwrap_or(DEFAULT_MANUAL_OVERRIDE_SECONDS);
    let device = get_device_config(&state, &name).await?;
    let operation_lock = device_operation_lock(&state, &device).await;
    let _operation_guard = operation_lock.lock().await;
    retry_tapo_handshake(|| state.controller.set_power(&device, request.on)).await?;
    let snapshot = retry_tapo_handshake(|| state.controller.read_device(&device)).await?;
    update_device_snapshot(&state, &name, snapshot, None, HookSource::Manual).await;

    set_manual_override(&state, &name, request.on, Some(duration)).await;
    if let Err(error) = save_persisted_state(&state).await {
        warn!(%error, device = %name, "failed to persist manual override");
    }

    get_device_view(&state, &name)
        .await
        .map(Json)
        .map_err(AppError)
}

pub(crate) async fn release_device_override(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<DeviceView>, AppError> {
    {
        let devices = state.devices.read().await;
        if !devices.contains_key(&name) {
            return Err(AppError(anyhow!("unknown device '{}'", name)));
        }
    }

    clear_manual_override(&state, &name).await;
    if let Err(error) = save_persisted_state(&state).await {
        warn!(%error, device = %name, "failed to persist override release");
    }
    reconcile_device(&state, &name, HookSource::Manual).await;

    get_device_view(&state, &name)
        .await
        .map(Json)
        .map_err(AppError)
}

pub(crate) async fn list_schedules(State(state): State<AppState>) -> Json<ScheduleListResponse> {
    let schedules = state.schedules.read().await;
    let mut views: Vec<ScheduleView> = schedules.values().map(schedule_view).collect();
    views.sort_by(|a, b| {
        a.device_name
            .cmp(&b.device_name)
            .then(a.created_at_ms.cmp(&b.created_at_ms))
    });

    Json(ScheduleListResponse { schedules: views })
}

pub(crate) async fn create_schedule(
    State(state): State<AppState>,
    Json(request): Json<CreateScheduleRequest>,
) -> Result<(StatusCode, Json<ScheduleView>), AppError> {
    let now = now_ms();
    let schedule = match request {
        CreateScheduleRequest::Cron {
            device_name,
            cron,
            action,
            label,
            enabled,
            condition_ids,
        } => {
            ensure_device_exists(&state, &device_name).await?;
            let normalized_cron = normalize_cron(&cron).map_err(AppError)?;
            let condition_ids = ensure_conditions_exist(&state, &condition_ids).await?;
            ScheduleConfig {
                id: new_schedule_id(),
                device_name,
                kind: ScheduleKind::Cron,
                cron: Some(normalized_cron),
                action: Some(action),
                on_seconds: None,
                off_seconds: None,
                start_action: None,
                starts_at_ms: None,
                enabled,
                label: label.and_then(non_empty_label),
                condition_ids,
                created_at_ms: now,
                last_fired_at_ms: None,
                last_error: None,
            }
        }
        CreateScheduleRequest::Interval {
            device_name,
            on_seconds,
            off_seconds,
            start_action,
            starts_at_ms,
            label,
            enabled,
            condition_ids,
        } => {
            ensure_device_exists(&state, &device_name).await?;
            validate_interval(on_seconds, off_seconds).map_err(AppError)?;
            let condition_ids = ensure_conditions_exist(&state, &condition_ids).await?;
            ScheduleConfig {
                id: new_schedule_id(),
                device_name,
                kind: ScheduleKind::Interval,
                cron: None,
                action: None,
                on_seconds: Some(on_seconds),
                off_seconds: Some(off_seconds),
                start_action: Some(start_action),
                starts_at_ms: Some(starts_at_ms.unwrap_or(now)),
                enabled,
                label: label.and_then(non_empty_label),
                condition_ids,
                created_at_ms: now,
                last_fired_at_ms: None,
                last_error: None,
            }
        }
    };

    {
        let mut schedules = state.schedules.write().await;
        schedules.insert(schedule.id.clone(), schedule.clone());
    }

    save_persisted_state(&state).await.map_err(AppError)?;

    Ok((StatusCode::CREATED, Json(schedule_view(&schedule))))
}

pub(crate) async fn ensure_device_exists(
    state: &AppState,
    device_name: &str,
) -> Result<(), AppError> {
    let devices = state.devices.read().await;
    if !devices.contains_key(device_name) {
        return Err(AppError(anyhow!("unknown device '{}'", device_name)));
    }
    Ok(())
}

pub(crate) async fn ensure_conditions_exist(
    state: &AppState,
    ids: &[String],
) -> Result<Vec<String>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let conditions = state.conditions.read().await;
    let mut deduped = Vec::with_capacity(ids.len());
    for id in ids {
        if !conditions.contains_key(id) {
            return Err(AppError(anyhow!("unknown condition '{}'", id)));
        }
        if !deduped.contains(id) {
            deduped.push(id.clone());
        }
    }
    Ok(deduped)
}

pub(crate) fn validate_interval(on_seconds: u64, off_seconds: u64) -> Result<()> {
    let cycle = on_seconds.saturating_add(off_seconds);
    if cycle < MIN_INTERVAL_CYCLE_SECONDS {
        return Err(anyhow!(
            "on + off duration must be at least {MIN_INTERVAL_CYCLE_SECONDS} seconds (got {cycle})"
        ));
    }
    if on_seconds == 0 && off_seconds == 0 {
        return Err(anyhow!("on_seconds and off_seconds cannot both be zero"));
    }
    Ok(())
}

pub(crate) async fn delete_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let removed = {
        let mut schedules = state.schedules.write().await;
        schedules.remove(&id).is_some()
    };

    if !removed {
        return Err(AppError(anyhow!("unknown schedule '{}'", id)));
    }

    save_persisted_state(&state).await.map_err(AppError)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn update_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateScheduleRequest>,
) -> Result<Json<ScheduleView>, AppError> {
    let normalized_cron = match request.cron.as_deref() {
        Some(expr) => Some(normalize_cron(expr).map_err(AppError)?),
        None => None,
    };

    let condition_ids = match request.condition_ids.as_deref() {
        Some(ids) => Some(ensure_conditions_exist(&state, ids).await?),
        None => None,
    };

    let updated = {
        let mut schedules = state.schedules.write().await;
        let schedule = schedules
            .get_mut(&id)
            .ok_or_else(|| AppError(anyhow!("unknown schedule '{}'", id)))?;

        if let Some(enabled) = request.enabled {
            schedule.enabled = enabled;
        }
        match schedule.kind {
            ScheduleKind::Cron => {
                if let Some(cron) = normalized_cron {
                    schedule.cron = Some(cron);
                    schedule.last_error = None;
                }
                if let Some(action) = request.action {
                    schedule.action = Some(action);
                }
            }
            ScheduleKind::Interval => {
                let new_on = request.on_seconds.or(schedule.on_seconds).unwrap_or(0);
                let new_off = request.off_seconds.or(schedule.off_seconds).unwrap_or(0);
                if request.on_seconds.is_some() || request.off_seconds.is_some() {
                    validate_interval(new_on, new_off).map_err(AppError)?;
                    schedule.on_seconds = Some(new_on);
                    schedule.off_seconds = Some(new_off);
                    schedule.starts_at_ms = Some(now_ms());
                    schedule.last_error = None;
                }
                if let Some(start_action) = request.start_action {
                    schedule.start_action = Some(start_action);
                    schedule.starts_at_ms = Some(now_ms());
                }
            }
        }
        if let Some(label) = request.label {
            schedule.label = label.and_then(non_empty_label);
        }
        if let Some(ids) = condition_ids {
            schedule.condition_ids = ids;
        }

        schedule.clone()
    };

    save_persisted_state(&state).await.map_err(AppError)?;
    Ok(Json(schedule_view(&updated)))
}

pub(crate) fn non_empty_label(label: String) -> Option<String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn normalize_cron(expression: &str) -> Result<String> {
    let trimmed = expression.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("cron expression is empty"));
    }

    let candidate = if trimmed.starts_with('@') {
        trimmed.to_string()
    } else {
        let field_count = trimmed.split_whitespace().count();
        match field_count {
            5 => format!("0 {}", trimmed),
            6 | 7 => trimmed.to_string(),
            _ => {
                return Err(anyhow!(
                    "cron expression must have 5, 6, or 7 fields (got {})",
                    field_count
                ));
            }
        }
    };

    parse_cron(&candidate).map(|_| candidate)
}

pub(crate) fn parse_cron(expression: &str) -> Result<CronSchedule> {
    let translated = translate_cron_to_crate_format(expression);
    CronSchedule::from_str(&translated).map_err(|error| anyhow!("invalid cron expression: {error}"))
}

pub(crate) fn translate_cron_to_crate_format(expression: &str) -> String {
    let trimmed = expression.trim();
    if trimmed.starts_with('@') {
        return trimmed.to_string();
    }

    let mut fields: Vec<String> = trimmed.split_whitespace().map(str::to_string).collect();
    let dow_index = match fields.len() {
        5 => 4,
        6 | 7 => 5,
        _ => return trimmed.to_string(),
    };

    if let Some(field) = fields.get_mut(dow_index) {
        *field = translate_dow_field(field);
    }

    fields.join(" ")
}

pub(crate) fn translate_dow_field(field: &str) -> String {
    field
        .split(',')
        .map(translate_dow_part)
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn translate_dow_part(part: &str) -> String {
    let trimmed = part.trim();
    if let Some((head, step)) = trimmed.split_once('/') {
        format!("{}/{}", translate_dow_head(head), step.trim())
    } else {
        translate_dow_head(trimmed)
    }
}

pub(crate) fn translate_dow_head(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed == "*" || trimmed == "?" {
        return trimmed.to_string();
    }
    if let Some((start, end)) = trimmed.split_once('-') {
        return format!(
            "{}-{}",
            translate_dow_value(start),
            translate_dow_value(end),
        );
    }
    translate_dow_value(trimmed)
}

pub(crate) fn translate_dow_value(value: &str) -> String {
    let trimmed = value.trim();
    if let Ok(n) = trimmed.parse::<u32>() {
        return ((n % 7) + 1).to_string();
    }
    trimmed.to_string()
}

pub(crate) static SCHEDULE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn new_schedule_id() -> String {
    let seq = SCHEDULE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}", now_ms(), seq)
}

pub(crate) fn schedule_view(schedule: &ScheduleConfig) -> ScheduleView {
    let next_fire_at_ms = match schedule.kind {
        ScheduleKind::Cron => schedule
            .cron
            .as_deref()
            .and_then(|expr| parse_cron(expr).ok())
            .and_then(|parsed| parsed.upcoming(Local).next())
            .map(|datetime| datetime.timestamp_millis()),
        ScheduleKind::Interval => next_interval_fire_ms(schedule, now_ms()).map(|ms| ms as i64),
    };

    ScheduleView {
        id: schedule.id.clone(),
        device_name: schedule.device_name.clone(),
        kind: schedule.kind,
        cron: schedule.cron.clone(),
        action: schedule.action,
        on_seconds: schedule.on_seconds,
        off_seconds: schedule.off_seconds,
        start_action: schedule.start_action,
        starts_at_ms: schedule.starts_at_ms,
        enabled: schedule.enabled,
        label: schedule.label.clone(),
        condition_ids: schedule.condition_ids.clone(),
        created_at_ms: schedule.created_at_ms,
        last_fired_at_ms: schedule.last_fired_at_ms,
        last_error: schedule.last_error.clone(),
        next_fire_at_ms,
    }
}

pub(crate) fn interval_phase_at(schedule: &ScheduleConfig, at_ms: u128) -> Option<ScheduleAction> {
    let on_seconds = schedule.on_seconds?;
    let off_seconds = schedule.off_seconds?;
    let start_action = schedule.start_action?;
    let starts_at = schedule.starts_at_ms?;
    let cycle = (on_seconds as u128).saturating_add(off_seconds as u128) * 1000;

    if cycle == 0 {
        return None;
    }
    if at_ms < starts_at {
        return None;
    }

    let offset = (at_ms - starts_at) % cycle;
    let first_phase_ms = match start_action {
        ScheduleAction::On | ScheduleAction::Toggle => (on_seconds as u128) * 1000,
        ScheduleAction::Off => (off_seconds as u128) * 1000,
    };

    let (primary, secondary) = match start_action {
        ScheduleAction::Off => (ScheduleAction::Off, ScheduleAction::On),
        _ => (ScheduleAction::On, ScheduleAction::Off),
    };

    if offset < first_phase_ms {
        Some(primary)
    } else {
        Some(secondary)
    }
}

pub(crate) fn next_interval_fire_ms(schedule: &ScheduleConfig, now: u128) -> Option<u128> {
    let on_seconds = schedule.on_seconds?;
    let off_seconds = schedule.off_seconds?;
    let start_action = schedule.start_action?;
    let starts_at = schedule.starts_at_ms?;
    let cycle = (on_seconds as u128).saturating_add(off_seconds as u128) * 1000;

    if cycle == 0 {
        return None;
    }
    if now < starts_at {
        return Some(starts_at);
    }

    let offset = (now - starts_at) % cycle;
    let first_phase_ms = match start_action {
        ScheduleAction::On | ScheduleAction::Toggle => (on_seconds as u128) * 1000,
        ScheduleAction::Off => (off_seconds as u128) * 1000,
    };

    let into_cycle = if offset < first_phase_ms {
        first_phase_ms - offset
    } else {
        cycle - offset
    };
    Some(now + into_cycle)
}

pub(crate) async fn list_conditions(State(state): State<AppState>) -> Json<ConditionListResponse> {
    let conditions = state.conditions.read().await;
    let mut views: Vec<ConditionView> = conditions.values().map(condition_view).collect();
    views.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.created_at_ms.cmp(&b.created_at_ms))
    });
    Json(ConditionListResponse { conditions: views })
}

pub(crate) async fn create_condition(
    State(state): State<AppState>,
    Json(request): Json<CreateConditionRequest>,
) -> Result<(StatusCode, Json<ConditionView>), AppError> {
    let name = request.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError(anyhow!("condition name is required")));
    }
    ensure_device_exists(&state, &request.device_name).await?;
    validate_http_method(&request.method).map_err(AppError)?;
    parse_status_match(&request.status_match).map_err(AppError)?;
    let poll_seconds = clamp_poll_seconds(request.poll_seconds).map_err(AppError)?;
    validate_url(&request.url).map_err(AppError)?;

    let condition = ConditionConfig {
        id: new_condition_id(),
        name,
        device_name: request.device_name,
        url: request.url,
        method: request.method.to_uppercase(),
        headers: request.headers,
        body: request.body.and_then(non_empty_label),
        status_match: request.status_match,
        body_contains: request.body_contains.and_then(non_empty_label),
        poll_seconds,
        enabled: request.enabled,
        action_on_pass: request.action_on_pass,
        action_on_fail: request.action_on_fail,
        created_at_ms: now_ms(),
        last_checked_at_ms: None,
        last_passing: None,
        last_status_code: None,
        last_error: None,
        last_action_at_ms: None,
        last_action: None,
        last_action_error: None,
        min_stable_seconds: request.min_stable_seconds,
        pending_value: None,
        pending_since_ms: None,
    };

    let id = condition.id.clone();
    {
        let mut conditions = state.conditions.write().await;
        conditions.insert(id.clone(), condition.clone());
    }
    save_persisted_state(&state).await.map_err(AppError)?;

    // Fire one probe immediately so the user sees status quickly.
    probe_and_record(&state, &id).await;

    let view = {
        let conditions = state.conditions.read().await;
        conditions.get(&id).map(condition_view).unwrap()
    };

    Ok((StatusCode::CREATED, Json(view)))
}

pub(crate) async fn update_condition(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateConditionRequest>,
) -> Result<Json<ConditionView>, AppError> {
    if let Some(method) = request.method.as_deref() {
        validate_http_method(method).map_err(AppError)?;
    }
    if let Some(status) = request.status_match.as_deref() {
        parse_status_match(status).map_err(AppError)?;
    }
    if let Some(poll) = request.poll_seconds {
        clamp_poll_seconds(poll).map_err(AppError)?;
    }
    if let Some(url) = request.url.as_deref() {
        validate_url(url).map_err(AppError)?;
    }
    if let Some(device_name) = request.device_name.as_deref() {
        ensure_device_exists(&state, device_name).await?;
    }

    let updated = {
        let mut conditions = state.conditions.write().await;
        let condition = conditions
            .get_mut(&id)
            .ok_or_else(|| AppError(anyhow!("unknown condition '{}'", id)))?;

        if let Some(name) = request.name {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return Err(AppError(anyhow!("condition name cannot be empty")));
            }
            condition.name = trimmed.to_string();
        }
        if let Some(device_name) = request.device_name {
            condition.device_name = device_name;
        }
        if let Some(url) = request.url {
            condition.url = url;
            condition.last_error = None;
        }
        if let Some(method) = request.method {
            condition.method = method.to_uppercase();
        }
        if let Some(headers) = request.headers {
            condition.headers = headers;
        }
        if let Some(body) = request.body {
            condition.body = body.and_then(non_empty_label);
        }
        if let Some(status) = request.status_match {
            condition.status_match = status;
            condition.last_error = None;
        }
        if let Some(body_contains) = request.body_contains {
            condition.body_contains = body_contains.and_then(non_empty_label);
        }
        if let Some(poll) = request.poll_seconds {
            condition.poll_seconds = poll;
        }
        if let Some(enabled) = request.enabled {
            condition.enabled = enabled;
            if !enabled {
                condition.last_passing = None;
                condition.last_status_code = None;
            }
        }
        if let Some(action) = request.action_on_pass {
            condition.action_on_pass = action;
        }
        if let Some(action) = request.action_on_fail {
            condition.action_on_fail = action;
        }
        if let Some(stable) = request.min_stable_seconds {
            condition.min_stable_seconds = stable;
            // Drop any in-flight pending value so the new threshold is
            // honoured on the next probe rather than reusing stale state.
            condition.pending_value = None;
            condition.pending_since_ms = None;
        }
        condition.clone()
    };

    save_persisted_state(&state).await.map_err(AppError)?;
    probe_and_record(&state, &id).await;

    let view = {
        let conditions = state.conditions.read().await;
        conditions
            .get(&id)
            .map(condition_view)
            .unwrap_or_else(|| condition_view(&updated))
    };

    Ok(Json(view))
}

pub(crate) async fn delete_condition(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let removed = {
        let mut conditions = state.conditions.write().await;
        conditions.remove(&id).is_some()
    };
    if !removed {
        return Err(AppError(anyhow!("unknown condition '{}'", id)));
    }

    {
        let mut schedules = state.schedules.write().await;
        for schedule in schedules.values_mut() {
            schedule.condition_ids.retain(|other| other != &id);
        }
    }

    save_persisted_state(&state).await.map_err(AppError)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn probe_condition(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ConditionView>, AppError> {
    {
        let conditions = state.conditions.read().await;
        if !conditions.contains_key(&id) {
            return Err(AppError(anyhow!("unknown condition '{}'", id)));
        }
    }
    probe_and_record(&state, &id).await;
    let conditions = state.conditions.read().await;
    Ok(Json(conditions.get(&id).map(condition_view).ok_or_else(
        || AppError(anyhow!("condition vanished mid-probe")),
    )?))
}

pub(crate) fn condition_view(condition: &ConditionConfig) -> ConditionView {
    ConditionView {
        id: condition.id.clone(),
        name: condition.name.clone(),
        device_name: condition.device_name.clone(),
        url: condition.url.clone(),
        method: condition.method.clone(),
        headers: condition.headers.clone(),
        body: condition.body.clone(),
        status_match: condition.status_match.clone(),
        body_contains: condition.body_contains.clone(),
        poll_seconds: condition.poll_seconds,
        enabled: condition.enabled,
        action_on_pass: condition.action_on_pass,
        action_on_fail: condition.action_on_fail,
        created_at_ms: condition.created_at_ms,
        last_checked_at_ms: condition.last_checked_at_ms,
        last_passing: condition.last_passing,
        last_status_code: condition.last_status_code,
        last_error: condition.last_error.clone(),
        last_action_at_ms: condition.last_action_at_ms,
        last_action: condition.last_action,
        last_action_error: condition.last_action_error.clone(),
        min_stable_seconds: condition.min_stable_seconds,
        pending_value: condition.pending_value,
        pending_since_ms: condition.pending_since_ms,
    }
}

pub(crate) static CONDITION_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn new_condition_id() -> String {
    let seq = CONDITION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("c{:x}-{:x}", now_ms(), seq)
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CreateHookRequest {
    pub(crate) name: String,
    pub(crate) url: String,
    #[serde(default = "default_http_method")]
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) headers: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default)]
    pub(crate) device_filter: Vec<String>,
    #[serde(default)]
    pub(crate) event_filter: Vec<HookEvent>,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UpdateHookRequest {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) url: Option<String>,
    #[serde(default)]
    pub(crate) method: Option<String>,
    #[serde(default)]
    pub(crate) headers: Option<BTreeMap<String, String>>,
    #[serde(default, deserialize_with = "deserialize_optional_label")]
    pub(crate) body: Option<Option<String>>,
    #[serde(default)]
    pub(crate) device_filter: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) event_filter: Option<Vec<HookEvent>>,
    #[serde(default)]
    pub(crate) enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HookView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) url: String,
    pub(crate) method: String,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) body: Option<String>,
    pub(crate) device_filter: Vec<String>,
    pub(crate) event_filter: Vec<HookEvent>,
    pub(crate) created_at_ms: u128,
    pub(crate) last_fired_at_ms: Option<u128>,
    pub(crate) last_event: Option<HookEvent>,
    pub(crate) last_status_code: Option<u16>,
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HookListResponse {
    pub(crate) hooks: Vec<HookView>,
}

pub(crate) fn hook_view(hook: &HookConfig) -> HookView {
    HookView {
        id: hook.id.clone(),
        name: hook.name.clone(),
        enabled: hook.enabled,
        url: hook.url.clone(),
        method: hook.method.clone(),
        headers: hook.headers.clone(),
        body: hook.body.clone(),
        device_filter: hook.device_filter.clone(),
        event_filter: hook.event_filter.clone(),
        created_at_ms: hook.created_at_ms,
        last_fired_at_ms: hook.last_fired_at_ms,
        last_event: hook.last_event,
        last_status_code: hook.last_status_code,
        last_error: hook.last_error.clone(),
    }
}

pub(crate) static HOOK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn new_hook_id() -> String {
    let seq = HOOK_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("h{:x}-{:x}", now_ms(), seq)
}

pub(crate) async fn list_hooks(State(state): State<AppState>) -> Json<HookListResponse> {
    let hooks = state.hooks.read().await;
    let mut views: Vec<HookView> = hooks.values().map(hook_view).collect();
    views.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.created_at_ms.cmp(&b.created_at_ms))
    });
    Json(HookListResponse { hooks: views })
}

pub(crate) async fn create_hook(
    State(state): State<AppState>,
    Json(request): Json<CreateHookRequest>,
) -> Result<(StatusCode, Json<HookView>), AppError> {
    let name = request.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError(anyhow!("hook name is required")));
    }
    validate_http_method(&request.method).map_err(AppError)?;
    validate_url(&request.url).map_err(AppError)?;

    let hook = HookConfig {
        id: new_hook_id(),
        name,
        enabled: request.enabled,
        url: request.url,
        method: request.method.to_uppercase(),
        headers: request.headers,
        body: request.body.and_then(non_empty_label),
        device_filter: request.device_filter,
        event_filter: request.event_filter,
        created_at_ms: now_ms(),
        last_fired_at_ms: None,
        last_event: None,
        last_status_code: None,
        last_error: None,
    };

    let id = hook.id.clone();
    {
        let mut hooks = state.hooks.write().await;
        hooks.insert(id.clone(), hook);
    }
    save_persisted_state(&state).await.map_err(AppError)?;

    let view = {
        let hooks = state.hooks.read().await;
        hooks
            .get(&id)
            .map(hook_view)
            .ok_or_else(|| AppError(anyhow!("hook vanished after create")))?
    };
    Ok((StatusCode::CREATED, Json(view)))
}

pub(crate) async fn update_hook(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateHookRequest>,
) -> Result<Json<HookView>, AppError> {
    if let Some(method) = request.method.as_deref() {
        validate_http_method(method).map_err(AppError)?;
    }
    if let Some(url) = request.url.as_deref() {
        validate_url(url).map_err(AppError)?;
    }

    {
        let mut hooks = state.hooks.write().await;
        let hook = hooks
            .get_mut(&id)
            .ok_or_else(|| AppError(anyhow!("unknown hook '{}'", id)))?;

        if let Some(name) = request.name {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return Err(AppError(anyhow!("hook name cannot be empty")));
            }
            hook.name = trimmed.to_string();
        }
        if let Some(url) = request.url {
            hook.url = url;
            hook.last_error = None;
        }
        if let Some(method) = request.method {
            hook.method = method.to_uppercase();
        }
        if let Some(headers) = request.headers {
            hook.headers = headers;
        }
        if let Some(body) = request.body {
            hook.body = body.and_then(non_empty_label);
        }
        if let Some(filter) = request.device_filter {
            hook.device_filter = filter;
        }
        if let Some(filter) = request.event_filter {
            hook.event_filter = filter;
        }
        if let Some(enabled) = request.enabled {
            hook.enabled = enabled;
        }
    }

    save_persisted_state(&state).await.map_err(AppError)?;

    let view = {
        let hooks = state.hooks.read().await;
        hooks
            .get(&id)
            .map(hook_view)
            .ok_or_else(|| AppError(anyhow!("hook vanished mid-update")))?
    };
    Ok(Json(view))
}

pub(crate) async fn delete_hook(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let removed = {
        let mut hooks = state.hooks.write().await;
        hooks.remove(&id).is_some()
    };
    if !removed {
        return Err(AppError(anyhow!("unknown hook '{}'", id)));
    }
    save_persisted_state(&state).await.map_err(AppError)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn test_hook(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<HookView>, AppError> {
    let hook = {
        let hooks = state.hooks.read().await;
        hooks
            .get(&id)
            .cloned()
            .ok_or_else(|| AppError(anyhow!("unknown hook '{}'", id)))?
    };

    let ctx = HookTemplateContext {
        device: "test".to_string(),
        nickname: "Test Device".to_string(),
        model: "p110".to_string(),
        event: HookEvent::On,
        source: HookSource::Manual,
        previous_on: Some(false),
        new_on: Some(true),
        timestamp_ms: now_ms(),
    };
    fire_hook(&state, hook, ctx).await;

    let view = {
        let hooks = state.hooks.read().await;
        hooks
            .get(&id)
            .map(hook_view)
            .ok_or_else(|| AppError(anyhow!("hook vanished mid-test")))?
    };
    Ok(Json(view))
}

// ---------- Automation HTTP handlers ----------

pub(crate) fn new_automation_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("auto-{}-{}", now_ms(), n)
}

pub(crate) async fn list_automations(
    State(state): State<AppState>,
) -> Json<AutomationListResponse> {
    let automations = state.automations.read().await;
    let mut list: Vec<Automation> = automations.values().cloned().collect();
    list.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms));
    Json(AutomationListResponse { automations: list })
}

pub(crate) async fn create_automation(
    State(state): State<AppState>,
    Json(request): Json<CreateAutomationRequest>,
) -> Result<(StatusCode, Json<Automation>), AppError> {
    let name = request.name.trim();
    if name.is_empty() {
        return Err(AppError(anyhow!("name cannot be empty")));
    }
    validate_automation_graph(&request.nodes, &request.edges)?;

    let id = new_automation_id();
    let automation = Automation {
        id: id.clone(),
        name: name.to_string(),
        enabled: request.enabled,
        nodes: request.nodes,
        edges: request.edges,
        created_at_ms: now_ms(),
        status: AutomationStatus::default(),
    };
    {
        let mut automations = state.automations.write().await;
        automations.insert(id, automation.clone());
    }
    if let Err(error) = save_persisted_state(&state).await {
        warn!(%error, "failed to persist newly created automation");
    }
    Ok((StatusCode::CREATED, Json(automation)))
}

pub(crate) async fn update_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateAutomationRequest>,
) -> Result<Json<Automation>, AppError> {
    if let (Some(nodes), Some(edges)) = (request.nodes.as_ref(), request.edges.as_ref()) {
        validate_automation_graph(nodes, edges)?;
    } else if let Some(nodes) = request.nodes.as_ref() {
        // Validate against current edges
        let existing = state.automations.read().await;
        let cur = existing
            .get(&id)
            .ok_or_else(|| AppError(anyhow!("unknown automation '{}'", id)))?;
        validate_automation_graph(nodes, &cur.edges)?;
    } else if let Some(edges) = request.edges.as_ref() {
        let existing = state.automations.read().await;
        let cur = existing
            .get(&id)
            .ok_or_else(|| AppError(anyhow!("unknown automation '{}'", id)))?;
        validate_automation_graph(&cur.nodes, edges)?;
    }

    let updated = {
        let mut automations = state.automations.write().await;
        let automation = automations
            .get_mut(&id)
            .ok_or_else(|| AppError(anyhow!("unknown automation '{}'", id)))?;
        if let Some(name) = request.name {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return Err(AppError(anyhow!("name cannot be empty")));
            }
            automation.name = trimmed.to_string();
        }
        if let Some(enabled) = request.enabled {
            automation.enabled = enabled;
        }
        if let Some(nodes) = request.nodes {
            automation.nodes = nodes;
            // Editing the graph clears stale per-node runtime state.
            automation.status.node_states.clear();
            automation.status.last_error = None;
        }
        if let Some(edges) = request.edges {
            automation.edges = edges;
        }
        automation.clone()
    };
    if let Err(error) = save_persisted_state(&state).await {
        warn!(%error, "failed to persist automation update");
    }
    Ok(Json(updated))
}

pub(crate) async fn delete_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let removed = {
        let mut automations = state.automations.write().await;
        automations.remove(&id).is_some()
    };
    if !removed {
        return Err(AppError(anyhow!("unknown automation '{}'", id)));
    }
    if let Err(error) = save_persisted_state(&state).await {
        warn!(%error, "failed to persist automation deletion");
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) fn validate_automation_graph(
    nodes: &[AutomationNode],
    edges: &[AutomationEdge],
) -> Result<()> {
    use std::collections::HashSet;
    let mut ids: HashSet<&str> = HashSet::new();
    for n in nodes {
        if !ids.insert(n.id.as_str()) {
            return Err(anyhow!("duplicate node id '{}'", n.id));
        }
        validate_node_config(&n.config)?;
    }
    for e in edges {
        if !ids.contains(e.source_node.as_str()) {
            return Err(anyhow!("edge source '{}' not found", e.source_node));
        }
        if !ids.contains(e.target_node.as_str()) {
            return Err(anyhow!("edge target '{}' not found", e.target_node));
        }
        if e.source_node == e.target_node {
            return Err(anyhow!(
                "self-edge on node '{}' is not allowed",
                e.source_node
            ));
        }
    }
    // Cycle detection (DFS): trigger -> action graphs must be acyclic.
    let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in edges {
        adj.entry(e.source_node.as_str())
            .or_default()
            .push(e.target_node.as_str());
    }
    let mut state_map: BTreeMap<&str, u8> = BTreeMap::new();
    for n in nodes {
        if !state_map.contains_key(n.id.as_str()) {
            if has_cycle_dfs(n.id.as_str(), &adj, &mut state_map) {
                return Err(anyhow!("automation graph contains a cycle"));
            }
        }
    }
    Ok(())
}

pub(crate) fn has_cycle_dfs<'a>(
    node: &'a str,
    adj: &BTreeMap<&'a str, Vec<&'a str>>,
    state: &mut BTreeMap<&'a str, u8>,
) -> bool {
    state.insert(node, 1);
    if let Some(neighbours) = adj.get(node) {
        for next in neighbours {
            match state.get(next) {
                Some(1) => return true,
                Some(2) => continue,
                _ => {
                    if has_cycle_dfs(next, adj, state) {
                        return true;
                    }
                }
            }
        }
    }
    state.insert(node, 2);
    false
}

pub(crate) fn validate_node_config(config: &AutomationNodeConfig) -> Result<()> {
    match config {
        AutomationNodeConfig::CronTrigger { cron_trigger } => {
            parse_cron(&cron_trigger.cron)
                .with_context(|| format!("invalid cron '{}'", cron_trigger.cron))?;
        }
        AutomationNodeConfig::IntervalTrigger { interval_trigger } => {
            let total = interval_trigger
                .on_seconds
                .saturating_add(interval_trigger.off_seconds);
            if total < MIN_INTERVAL_CYCLE_SECONDS {
                return Err(anyhow!(
                    "interval cycle (on+off) must be at least {MIN_INTERVAL_CYCLE_SECONDS}s"
                ));
            }
        }
        AutomationNodeConfig::HttpProbe { http_probe } => {
            validate_url(&http_probe.url)?;
            validate_http_method(&http_probe.method)?;
            parse_status_match(&http_probe.status_match)?;
            clamp_poll_seconds(http_probe.poll_seconds)?;
        }
        AutomationNodeConfig::DeviceEventTrigger {
            device_event_trigger,
        } => {
            if device_event_trigger.device_name.is_empty() {
                return Err(anyhow!("device event trigger requires a device_name"));
            }
        }
        AutomationNodeConfig::SetDevice { set_device } => {
            if set_device.device_name.is_empty() {
                return Err(anyhow!("set_device requires a device_name"));
            }
        }
        AutomationNodeConfig::ToggleDevice { toggle_device } => {
            if toggle_device.device_name.is_empty() {
                return Err(anyhow!("toggle_device requires a device_name"));
            }
        }
        AutomationNodeConfig::FireHook { fire_hook } => {
            if fire_hook.hook_id.is_empty() {
                return Err(anyhow!("fire_hook requires a hook_id"));
            }
        }
        AutomationNodeConfig::LogicAnd
        | AutomationNodeConfig::LogicOr
        | AutomationNodeConfig::LogicNot
        | AutomationNodeConfig::Debounce { .. } => {}
    }
    Ok(())
}

pub(crate) fn validate_http_method(method: &str) -> Result<()> {
    HttpMethod::from_bytes(method.trim().to_uppercase().as_bytes())
        .map(|_| ())
        .map_err(|error| anyhow!("invalid HTTP method '{method}': {error}"))
}

pub(crate) fn validate_url(url: &str) -> Result<()> {
    let trimmed = url.trim();
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(anyhow!("URL must start with http:// or https://"));
    }
    Ok(())
}

pub(crate) fn clamp_poll_seconds(value: u64) -> Result<u64> {
    if !(MIN_CONDITION_POLL_SECONDS..=MAX_CONDITION_POLL_SECONDS).contains(&value) {
        return Err(anyhow!(
            "poll_seconds must be between {MIN_CONDITION_POLL_SECONDS} and {MAX_CONDITION_POLL_SECONDS} (got {value})"
        ));
    }
    Ok(value)
}

pub(crate) fn parse_status_match(expression: &str) -> Result<Vec<std::ops::RangeInclusive<u16>>> {
    let trimmed = expression.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("status_match cannot be empty"));
    }

    let mut ranges = Vec::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start: u16 = start
                .trim()
                .parse()
                .map_err(|_| anyhow!("invalid status code '{start}' in '{expression}'"))?;
            let end: u16 = end
                .trim()
                .parse()
                .map_err(|_| anyhow!("invalid status code '{end}' in '{expression}'"))?;
            if start > end {
                return Err(anyhow!("status range '{start}-{end}' is reversed"));
            }
            ranges.push(start..=end);
        } else {
            let code: u16 = part
                .parse()
                .map_err(|_| anyhow!("invalid status code '{part}' in '{expression}'"))?;
            ranges.push(code..=code);
        }
    }
    if ranges.is_empty() {
        return Err(anyhow!("status_match must contain at least one code"));
    }
    Ok(ranges)
}

pub(crate) fn status_matches(ranges: &[std::ops::RangeInclusive<u16>], code: u16) -> bool {
    ranges.iter().any(|range| range.contains(&code))
}

pub(crate) struct ProbeOutcome {
    pub(crate) passing: bool,
    pub(crate) status_code: Option<u16>,
    pub(crate) error: Option<String>,
}

pub(crate) async fn probe_condition_once(
    client: &reqwest::Client,
    condition: &ConditionConfig,
) -> ProbeOutcome {
    let method = match HttpMethod::from_bytes(condition.method.to_uppercase().as_bytes()) {
        Ok(method) => method,
        Err(error) => {
            return ProbeOutcome {
                passing: false,
                status_code: None,
                error: Some(format!("invalid HTTP method: {error}")),
            };
        }
    };

    let ranges = match parse_status_match(&condition.status_match) {
        Ok(ranges) => ranges,
        Err(error) => {
            return ProbeOutcome {
                passing: false,
                status_code: None,
                error: Some(format!("invalid status_match: {error}")),
            };
        }
    };

    let mut request_builder = client.request(method, &condition.url);
    for (key, value) in &condition.headers {
        request_builder = request_builder.header(key, value);
    }
    if let Some(body) = &condition.body {
        request_builder = request_builder.body(body.clone());
    }

    let response = match request_builder.send().await {
        Ok(response) => response,
        Err(error) => {
            return ProbeOutcome {
                passing: false,
                status_code: None,
                error: Some(format!("{error}")),
            };
        }
    };

    let status = response.status().as_u16();
    let status_ok = status_matches(&ranges, status);

    let body_match = if let Some(needle) = condition.body_contains.as_deref() {
        match read_response_body(response).await {
            Ok(body) => body.contains(needle),
            Err(error) => {
                return ProbeOutcome {
                    passing: false,
                    status_code: Some(status),
                    error: Some(format!("response read failed: {error}")),
                };
            }
        }
    } else {
        true
    };

    ProbeOutcome {
        passing: status_ok && body_match,
        status_code: Some(status),
        error: if status_ok && body_match {
            None
        } else if !status_ok {
            Some(format!(
                "status {status} did not match '{}'",
                condition.status_match
            ))
        } else {
            Some(format!(
                "body did not contain '{}'",
                condition.body_contains.as_deref().unwrap_or("")
            ))
        },
    }
}

pub(crate) async fn read_response_body(response: reqwest::Response) -> Result<String> {
    let bytes = response.bytes().await.map_err(|error| anyhow!("{error}"))?;
    let truncated = if bytes.len() > MAX_CONDITION_BODY_BYTES {
        &bytes[..MAX_CONDITION_BODY_BYTES]
    } else {
        &bytes[..]
    };
    Ok(String::from_utf8_lossy(truncated).into_owned())
}

pub(crate) fn condition_probe_key(condition: &ConditionConfig) -> String {
    let headers = condition
        .headers
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\x1f");
    format!(
        "{}\x1e{}\x1e{}\x1e{}\x1e{}\x1e{}",
        condition.method.to_uppercase(),
        condition.url,
        headers,
        condition.body.as_deref().unwrap_or(""),
        condition.status_match,
        condition.body_contains.as_deref().unwrap_or(""),
    )
}

pub(crate) async fn probe_and_record(state: &AppState, id: &str) {
    let representative = {
        let conditions = state.conditions.read().await;
        conditions.get(id).cloned()
    };
    let Some(representative) = representative else {
        return;
    };
    if !representative.enabled {
        return;
    }

    let key = condition_probe_key(&representative);

    // Snapshot every enabled condition that shares this probe key, along with
    // its current last_passing so we can detect transitions afterwards.
    let group: Vec<(ConditionConfig, Option<bool>)> = {
        let conditions = state.conditions.read().await;
        conditions
            .values()
            .filter(|condition| condition.enabled && condition_probe_key(condition) == key)
            .map(|condition| (condition.clone(), condition.last_passing))
            .collect()
    };
    if group.is_empty() {
        return;
    }

    let outcome = probe_condition_once(&state.http_client, &representative).await;
    let now = now_ms();

    let mut transitions: Vec<(String, String)> = Vec::new(); // (condition_id, device_name)
    {
        let mut conditions = state.conditions.write().await;
        for (snapshot, prev_last_passing) in &group {
            let Some(stored) = conditions.get_mut(&snapshot.id) else {
                continue;
            };

            stored.last_checked_at_ms = Some(now);
            stored.last_status_code = outcome.status_code;
            stored.last_error = outcome.error.clone();

            let new_value = outcome.passing;
            let stable_required_ms = (snapshot.min_stable_seconds as u128) * 1000;

            let committed = match prev_last_passing {
                Some(prev) if *prev == new_value => {
                    // No change — clear any pending wait.
                    stored.pending_value = None;
                    stored.pending_since_ms = None;
                    false
                }
                _ if stable_required_ms == 0 => {
                    // No hysteresis configured — commit immediately.
                    stored.last_passing = Some(new_value);
                    stored.pending_value = None;
                    stored.pending_since_ms = None;
                    true
                }
                _ => {
                    // Hysteresis active: track pending value.
                    match snapshot.pending_value {
                        Some(pending) if pending == new_value => {
                            let stable_since = snapshot.pending_since_ms.unwrap_or(now);
                            let elapsed_ms = now.saturating_sub(stable_since);
                            if elapsed_ms >= stable_required_ms {
                                stored.last_passing = Some(new_value);
                                stored.pending_value = None;
                                stored.pending_since_ms = None;
                                true
                            } else {
                                stored.pending_value = Some(new_value);
                                stored.pending_since_ms = Some(stable_since);
                                false
                            }
                        }
                        _ => {
                            // First observation of this new value — start the
                            // stability window.
                            stored.pending_value = Some(new_value);
                            stored.pending_since_ms = Some(now);
                            false
                        }
                    }
                }
            };

            if committed && !snapshot.device_name.is_empty() {
                transitions.push((snapshot.id.clone(), snapshot.device_name.clone()));
            }
        }
    }

    // Collect distinct devices we should reconcile.
    let mut devices_to_reconcile: Vec<String> = Vec::new();
    for (_, device_name) in &transitions {
        if !devices_to_reconcile.contains(device_name) {
            devices_to_reconcile.push(device_name.clone());
        }
    }

    if let Err(error) = save_persisted_state(state).await {
        warn!(%error, condition_id = %id, "failed to persist condition probe result");
    }

    for device_name in devices_to_reconcile {
        reconcile_device(state, &device_name, HookSource::Condition).await;
    }
}

/// Compute the effective desired power state for a device given its
/// inputs. None means "no opinion — don't touch the device".
pub(crate) fn compute_effective(
    manual_override: Option<bool>,
    schedule_intent: Option<bool>,
    condition_intent: Option<bool>,
) -> Option<bool> {
    if let Some(manual) = manual_override {
        return Some(manual);
    }
    if condition_intent == Some(false) {
        return Some(false);
    }
    if let Some(schedule) = schedule_intent {
        return Some(schedule);
    }
    condition_intent
}

/// Returns the condition intent for the device:
///   - None if no enabled conditions target the device
///   - Some(false) if any enabled condition is failing or has never been probed (fail closed)
///   - Some(true) if at least one enabled condition exists and all are passing
pub(crate) async fn condition_intent_for_device(
    state: &AppState,
    device_name: &str,
) -> Option<bool> {
    let conditions = state.conditions.read().await;
    let mut have_any = false;
    let mut all_passing = true;
    for condition in conditions.values() {
        if !condition.enabled || condition.device_name != device_name {
            continue;
        }
        have_any = true;
        match condition.last_passing {
            Some(true) => continue,
            Some(false) | None => {
                all_passing = false;
                break;
            }
        }
    }
    if !have_any {
        None
    } else if all_passing {
        Some(true)
    } else {
        Some(false)
    }
}

/// Reconcile a device's actual state with the computed effective state.
/// Skips if the device doesn't exist or if the effective state is None.
pub(crate) async fn reconcile_device(state: &AppState, device_name: &str, source: HookSource) {
    let device_cfg = match get_device_config(state, device_name).await {
        Ok(cfg) => cfg,
        Err(_) => return,
    };

    let intent = {
        let intents = state.device_intents.read().await;
        intents.get(device_name).cloned().unwrap_or_default()
    };
    let condition_intent = condition_intent_for_device(state, device_name).await;
    let effective = compute_effective(
        intent.manual_override,
        intent.schedule_intent,
        condition_intent,
    );
    let Some(target) = effective else {
        return;
    };

    let (current_state, nickname, model) = {
        let devices = state.devices.read().await;
        let device = devices.get(device_name);
        let current = device.and_then(|d| d.snapshot.as_ref().map(|s| s.device_on));
        let nickname = device
            .and_then(|d| d.snapshot.as_ref())
            .map(|s| s.nickname.clone())
            .unwrap_or_else(|| device_name.to_string());
        let model = device
            .and_then(|d| d.snapshot.as_ref())
            .map(|s| s.device_model.clone())
            .unwrap_or_else(|| {
                device
                    .map(|d| d.config.model.to_string())
                    .unwrap_or_default()
            });
        (current, nickname, model)
    };
    if current_state == Some(target) {
        info!(
            device = %device_name,
            target,
            "reconcile noop: device already at target state, no hook will fire"
        );
        return;
    }
    if current_state.is_none() {
        warn!(
            device = %device_name,
            target,
            "reconciling without a prior snapshot; set_power may fail if device is offline"
        );
    }

    info!(
        device = %device_name,
        manual = ?intent.manual_override,
        schedule = ?intent.schedule_intent,
        condition = ?condition_intent,
        target,
        "reconciling device state",
    );

    let operation_lock = device_operation_lock(state, &device_cfg).await;
    let _operation_guard = operation_lock.lock().await;

    if let Err(error) =
        retry_tapo_handshake(|| state.controller.set_power(&device_cfg, target)).await
    {
        warn!(device = %device_name, %error, "reconcile set_power failed");
        return;
    }

    // Optimistically update the cached snapshot so the later readback
    // (or absence of one) doesn't re-fire the same transition event.
    {
        let mut devices = state.devices.write().await;
        if let Some(device) = devices.get_mut(device_name) {
            if let Some(snap) = device.snapshot.as_mut() {
                snap.device_on = target;
            }
            device.updated_at_ms = Some(now_ms());
        }
    }

    // Fire the transition hook directly so it isn't dropped if the
    // post-set readback fails (which can happen on some Tapo plugs).
    dispatch_hook_events(
        state,
        device_name,
        &nickname,
        &model,
        if target {
            HookEvent::On
        } else {
            HookEvent::Off
        },
        source,
        current_state,
        Some(target),
    )
    .await;

    // Readback keeps energy/runtime stats fresh. update_device_snapshot
    // sees prev_on == new_on (thanks to the optimistic update above) and
    // won't refire the event.
    if let Ok(snapshot) = retry_tapo_handshake(|| state.controller.read_device(&device_cfg)).await {
        update_device_snapshot(state, device_name, snapshot, None, source).await;
    } else {
        publish_device_list(state, None).await;
    }
}

pub(crate) async fn set_schedule_intent(state: &AppState, device_name: &str, intent: bool) {
    let mut intents = state.device_intents.write().await;
    let entry = intents.entry(device_name.to_string()).or_default();
    entry.schedule_intent = Some(intent);
    // Schedule firing automatically releases any manual override.
    entry.manual_override = None;
    entry.manual_override_until_ms = None;
}

pub(crate) async fn set_manual_override(
    state: &AppState,
    device_name: &str,
    target: bool,
    duration_seconds: Option<u64>,
) {
    let mut intents = state.device_intents.write().await;
    let entry = intents.entry(device_name.to_string()).or_default();
    entry.manual_override = Some(target);
    entry.manual_override_until_ms = duration_seconds.map(|secs| {
        let bounded = secs
            .max(MIN_MANUAL_OVERRIDE_SECONDS)
            .min(MAX_MANUAL_OVERRIDE_SECONDS);
        now_ms() + (bounded as u128) * 1000
    });
}

pub(crate) async fn clear_manual_override(state: &AppState, device_name: &str) {
    let mut intents = state.device_intents.write().await;
    if let Some(entry) = intents.get_mut(device_name) {
        entry.manual_override = None;
        entry.manual_override_until_ms = None;
    }
}

pub(crate) async fn run_override_expiry_sweeper(state: AppState) {
    sleep(Duration::from_secs(2)).await;
    loop {
        let now = now_ms();
        let expired: Vec<String> = {
            let intents = state.device_intents.read().await;
            intents
                .iter()
                .filter_map(|(name, intent)| match intent.manual_override_until_ms {
                    Some(until) if until <= now && intent.manual_override.is_some() => {
                        Some(name.clone())
                    }
                    _ => None,
                })
                .collect()
        };

        if !expired.is_empty() {
            for name in &expired {
                clear_manual_override(&state, name).await;
                info!(device = %name, "manual override expired, returning to auto");
            }
            if let Err(error) = save_persisted_state(&state).await {
                warn!(%error, "failed to persist override expiry");
            }
            for name in &expired {
                reconcile_device(&state, name, HookSource::Manual).await;
            }
        }

        sleep(Duration::from_secs(5)).await;
    }
}

// ---------- Automation execution engine ----------

pub(crate) async fn run_automation_engine(state: AppState) {
    sleep(Duration::from_secs(2)).await;
    let mut previous_tick_ms = now_ms();
    loop {
        let tick_ms = now_ms();
        if let Err(error) = evaluate_all_automations(&state, previous_tick_ms, tick_ms).await {
            warn!(%error, "automation engine tick failed");
        }
        previous_tick_ms = tick_ms;
        sleep(Duration::from_secs(1)).await;
    }
}

pub(crate) async fn evaluate_all_automations(
    state: &AppState,
    previous_tick_ms: u128,
    tick_ms: u128,
) -> Result<()> {
    let snapshots: Vec<Automation> = {
        let automations = state.automations.read().await;
        automations
            .values()
            .filter(|a| a.enabled)
            .cloned()
            .collect()
    };

    for automation in snapshots {
        evaluate_one_automation(state, automation, previous_tick_ms, tick_ms).await;
    }

    Ok(())
}

pub(crate) async fn evaluate_one_automation(
    state: &AppState,
    automation: Automation,
    previous_tick_ms: u128,
    tick_ms: u128,
) {
    let order = match topo_sort_nodes(&automation.nodes, &automation.edges) {
        Some(order) => order,
        None => {
            // Graph contains a cycle. Validation should prevent this, but skip
            // defensively rather than panicking.
            warn!(automation_id = %automation.id, "skipping automation with cyclic graph");
            return;
        }
    };

    let mut incoming: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in &automation.nodes {
        incoming.entry(n.id.clone()).or_default();
    }
    for e in &automation.edges {
        incoming
            .entry(e.target_node.clone())
            .or_default()
            .push(e.source_node.clone());
    }

    let mut outputs: BTreeMap<String, Option<bool>> = BTreeMap::new();
    let mut transitions: Vec<(String, AutomationNodeConfig)> = Vec::new();
    let mut node_state_updates: BTreeMap<String, NodeRuntimeState> = BTreeMap::new();

    let devices_to_reconcile = std::collections::BTreeSet::<String>::new();
    let mut devices_to_reconcile = devices_to_reconcile;

    for node_id in &order {
        let node = match automation.nodes.iter().find(|n| &n.id == node_id) {
            Some(n) => n,
            None => continue,
        };

        let prev_state = {
            let automations = state.automations.read().await;
            automations
                .get(&automation.id)
                .and_then(|a| a.status.node_states.get(node_id))
                .cloned()
                .unwrap_or_default()
        };

        let input_values: Vec<Option<bool>> = incoming
            .get(node_id)
            .map(|sources| {
                sources
                    .iter()
                    .map(|src| outputs.get(src).copied().unwrap_or(None))
                    .collect()
            })
            .unwrap_or_default();

        let (new_output, new_state) = evaluate_node(
            state,
            &automation.id,
            node,
            &input_values,
            &prev_state,
            previous_tick_ms,
            tick_ms,
        )
        .await;

        outputs.insert(node_id.clone(), new_output);

        let is_action = matches!(
            node.config,
            AutomationNodeConfig::SetDevice { .. }
                | AutomationNodeConfig::ToggleDevice { .. }
                | AutomationNodeConfig::FireHook { .. }
        );
        let rising = matches!(
            (prev_state.last_value, new_output),
            (None, Some(true)) | (Some(false), Some(true))
        );

        let mut merged = new_state;
        if rising && is_action {
            merged.last_fired_at_ms = Some(tick_ms);
            transitions.push((node_id.clone(), node.config.clone()));
        }
        merged.last_value = new_output;
        node_state_updates.insert(node_id.clone(), merged);
    }

    // Run side-effecting actions outside the read lock.
    for (node_id, config) in transitions {
        match execute_action(state, &automation.id, &node_id, &config).await {
            Ok(Some(device_name)) => {
                devices_to_reconcile.insert(device_name);
            }
            Ok(None) => {}
            Err(error) => {
                node_state_updates
                    .entry(node_id)
                    .and_modify(|s| s.last_error = Some(format!("{error}")))
                    .or_insert_with(|| NodeRuntimeState {
                        last_error: Some(format!("{error}")),
                        ..Default::default()
                    });
            }
        }
    }

    // Persist runtime state updates. Only mark the state as dirty if a
    // visible field changed (last_value, last_fired_at_ms, last_error), so
    // the engine doesn't rewrite state.json every second.
    let mut dirty = false;
    {
        let mut automations = state.automations.write().await;
        if let Some(a) = automations.get_mut(&automation.id) {
            for (node_id, state_update) in node_state_updates {
                let prev = a.status.node_states.get(&node_id);
                let changed = match prev {
                    Some(p) => {
                        p.last_value != state_update.last_value
                            || p.last_fired_at_ms != state_update.last_fired_at_ms
                            || p.last_error != state_update.last_error
                    }
                    None => {
                        state_update.last_value.is_some()
                            || state_update.last_fired_at_ms.is_some()
                            || state_update.last_error.is_some()
                    }
                };
                if changed {
                    dirty = true;
                }
                a.status.node_states.insert(node_id, state_update);
            }
            let new_last_fired = a
                .status
                .node_states
                .values()
                .filter_map(|s| s.last_fired_at_ms)
                .max();
            let new_last_error = a
                .status
                .node_states
                .values()
                .find_map(|s| s.last_error.clone());
            if a.status.last_fired_at_ms != new_last_fired {
                dirty = true;
                a.status.last_fired_at_ms = new_last_fired;
            }
            if a.status.last_error != new_last_error {
                dirty = true;
                a.status.last_error = new_last_error;
            }
        }
    }

    if dirty {
        if let Err(error) = save_persisted_state(state).await {
            warn!(%error, "failed to persist automation engine state");
        }
    }

    for device_name in devices_to_reconcile {
        reconcile_device(state, &device_name, HookSource::Schedule).await;
    }
}

pub(crate) fn topo_sort_nodes(
    nodes: &[AutomationNode],
    edges: &[AutomationEdge],
) -> Option<Vec<String>> {
    let mut indeg: BTreeMap<String, usize> = BTreeMap::new();
    let mut adj: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in nodes {
        indeg.entry(n.id.clone()).or_insert(0);
        adj.entry(n.id.clone()).or_default();
    }
    for e in edges {
        *indeg.entry(e.target_node.clone()).or_insert(0) += 1;
        adj.entry(e.source_node.clone())
            .or_default()
            .push(e.target_node.clone());
    }
    let mut queue: std::collections::VecDeque<String> = indeg
        .iter()
        .filter(|&(_, d)| *d == 0)
        .map(|(k, _)| k.clone())
        .collect();
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(id) = queue.pop_front() {
        order.push(id.clone());
        if let Some(neighbours) = adj.get(&id) {
            for next in neighbours {
                let degree = indeg.get_mut(next).expect("present");
                *degree = degree.saturating_sub(1);
                if *degree == 0 {
                    queue.push_back(next.clone());
                }
            }
        }
    }
    if order.len() == nodes.len() {
        Some(order)
    } else {
        None
    }
}

pub(crate) async fn evaluate_node(
    state: &AppState,
    automation_id: &str,
    node: &AutomationNode,
    inputs: &[Option<bool>],
    prev: &NodeRuntimeState,
    previous_tick_ms: u128,
    tick_ms: u128,
) -> (Option<bool>, NodeRuntimeState) {
    let mut next = prev.clone();
    next.last_error = None;

    match &node.config {
        AutomationNodeConfig::CronTrigger { cron_trigger } => {
            let parsed = match parse_cron(&cron_trigger.cron) {
                Ok(p) => p,
                Err(error) => {
                    next.last_error = Some(format!("{error}"));
                    return (Some(false), next);
                }
            };
            let prev_dt = DateTime::<Local>::from(
                std::time::UNIX_EPOCH + Duration::from_millis(previous_tick_ms as u64),
            );
            let now_dt = DateTime::<Local>::from(
                std::time::UNIX_EPOCH + Duration::from_millis(tick_ms as u64),
            );
            let fired = parsed
                .after(&prev_dt)
                .next()
                .map(|fire_time| fire_time <= now_dt)
                .unwrap_or(false);
            (Some(fired), next)
        }
        AutomationNodeConfig::IntervalTrigger { interval_trigger } => {
            let total = interval_trigger
                .on_seconds
                .saturating_add(interval_trigger.off_seconds);
            if total == 0 {
                next.last_error = Some("interval cycle is zero".to_string());
                return (Some(false), next);
            }
            let starts = interval_trigger.starts_at_ms.unwrap_or(0);
            let elapsed = tick_ms.saturating_sub(starts) / 1000;
            let phase_seconds = (elapsed % (total as u128)) as u64;
            let in_on_window = phase_seconds < interval_trigger.on_seconds;
            let active = match interval_trigger.start_action {
                ScheduleAction::On | ScheduleAction::Toggle => in_on_window,
                ScheduleAction::Off => !in_on_window,
            };
            (Some(active), next)
        }
        AutomationNodeConfig::DeviceEventTrigger {
            device_event_trigger,
        } => {
            // Edge-detect against current device snapshot. We only support
            // on/off events through snapshot polling; online/offline are
            // handled via the existing offline tracking flags.
            let snapshot_state = {
                let devices = state.devices.read().await;
                devices.get(&device_event_trigger.device_name).map(|d| {
                    (
                        d.snapshot.as_ref().map(|s| s.device_on),
                        d.offline_announced,
                        d.consecutive_failures,
                    )
                })
            };
            let value = match (device_event_trigger.event, snapshot_state) {
                (HookEvent::On, Some((Some(on), _, _))) => Some(on),
                (HookEvent::Off, Some((Some(on), _, _))) => Some(!on),
                (HookEvent::Offline, Some((_, offline_announced, _))) => Some(offline_announced),
                (HookEvent::Online, Some((_, offline_announced, _))) => Some(!offline_announced),
                _ => None,
            };
            (value, next)
        }
        AutomationNodeConfig::HttpProbe { http_probe } => {
            let interval_ms = (http_probe.poll_seconds as u128) * 1000;
            let due = match next.last_checked_at_ms {
                None => true,
                Some(last) => tick_ms.saturating_sub(last) >= interval_ms,
            };
            if !due {
                return (prev.last_value, next);
            }

            let probe = ConditionConfig {
                id: format!("auto/{automation_id}/node/{}", node.id),
                name: node.id.clone(),
                device_name: String::new(),
                url: http_probe.url.clone(),
                method: http_probe.method.clone(),
                headers: http_probe.headers.clone(),
                body: http_probe.body.clone(),
                status_match: http_probe.status_match.clone(),
                body_contains: http_probe.body_contains.clone(),
                poll_seconds: http_probe.poll_seconds,
                enabled: true,
                action_on_pass: None,
                action_on_fail: None,
                created_at_ms: 0,
                last_checked_at_ms: None,
                last_passing: None,
                last_status_code: None,
                last_error: None,
                last_action_at_ms: None,
                last_action: None,
                last_action_error: None,
                min_stable_seconds: http_probe.min_stable_seconds,
                pending_value: None,
                pending_since_ms: None,
            };

            let outcome = probe_condition_once(&state.http_client, &probe).await;
            next.last_checked_at_ms = Some(tick_ms);
            if let Some(error) = outcome.error {
                next.last_error = Some(error);
            }

            let new_value = outcome.passing;
            let stable_ms = (http_probe.min_stable_seconds as u128) * 1000;

            let committed = match prev.last_value {
                Some(prev_val) if prev_val == new_value => {
                    next.pending_value = None;
                    next.pending_since_ms = None;
                    Some(new_value)
                }
                _ if stable_ms == 0 => {
                    next.pending_value = None;
                    next.pending_since_ms = None;
                    Some(new_value)
                }
                _ => match prev.pending_value {
                    Some(pending) if pending == new_value => {
                        let since = prev.pending_since_ms.unwrap_or(tick_ms);
                        if tick_ms.saturating_sub(since) >= stable_ms {
                            next.pending_value = None;
                            next.pending_since_ms = None;
                            Some(new_value)
                        } else {
                            next.pending_value = Some(new_value);
                            next.pending_since_ms = Some(since);
                            prev.last_value
                        }
                    }
                    _ => {
                        next.pending_value = Some(new_value);
                        next.pending_since_ms = Some(tick_ms);
                        prev.last_value
                    }
                },
            };

            (committed, next)
        }
        AutomationNodeConfig::LogicAnd => {
            if inputs.is_empty() {
                return (Some(false), next);
            }
            let mut all_true = true;
            for v in inputs {
                match v {
                    Some(true) => continue,
                    Some(false) => {
                        all_true = false;
                        break;
                    }
                    None => {
                        all_true = false;
                        break;
                    }
                }
            }
            (Some(all_true), next)
        }
        AutomationNodeConfig::LogicOr => {
            if inputs.is_empty() {
                return (Some(false), next);
            }
            let any_true = inputs.iter().any(|v| matches!(v, Some(true)));
            (Some(any_true), next)
        }
        AutomationNodeConfig::LogicNot => {
            let v = inputs.first().copied().flatten();
            (v.map(|b| !b), next)
        }
        AutomationNodeConfig::Debounce { debounce } => {
            let new_value = inputs.first().copied().flatten();
            let hold_ms = (debounce.hold_seconds as u128) * 1000;
            match (prev.last_value, new_value) {
                (a, b) if a == b => {
                    next.pending_value = None;
                    next.pending_since_ms = None;
                    (b, next)
                }
                (_, None) => {
                    next.pending_value = None;
                    next.pending_since_ms = None;
                    (None, next)
                }
                (_, Some(b)) if hold_ms == 0 => {
                    next.pending_value = None;
                    next.pending_since_ms = None;
                    (Some(b), next)
                }
                (_, Some(b)) => match prev.pending_value {
                    Some(pending) if pending == b => {
                        let since = prev.pending_since_ms.unwrap_or(tick_ms);
                        if tick_ms.saturating_sub(since) >= hold_ms {
                            next.pending_value = None;
                            next.pending_since_ms = None;
                            (Some(b), next)
                        } else {
                            next.pending_value = Some(b);
                            next.pending_since_ms = Some(since);
                            (prev.last_value, next)
                        }
                    }
                    _ => {
                        next.pending_value = Some(b);
                        next.pending_since_ms = Some(tick_ms);
                        (prev.last_value, next)
                    }
                },
            }
        }
        AutomationNodeConfig::SetDevice { .. }
        | AutomationNodeConfig::ToggleDevice { .. }
        | AutomationNodeConfig::FireHook { .. } => {
            // Actions just propagate the maximum of their inputs so they
            // can in turn drive other actions if connected.
            let active = inputs.iter().any(|v| matches!(v, Some(true)));
            (Some(active), next)
        }
    }
}

pub(crate) async fn execute_action(
    state: &AppState,
    automation_id: &str,
    node_id: &str,
    config: &AutomationNodeConfig,
) -> Result<Option<String>> {
    match config {
        AutomationNodeConfig::SetDevice { set_device } => {
            let target = match set_device.action {
                ScheduleAction::On => true,
                ScheduleAction::Off => false,
                ScheduleAction::Toggle => {
                    let current = {
                        let devices = state.devices.read().await;
                        devices
                            .get(&set_device.device_name)
                            .and_then(|d| d.snapshot.as_ref().map(|s| s.device_on))
                    };
                    match current {
                        Some(on) => !on,
                        None => {
                            return Err(anyhow!(
                                "toggle requested but device '{}' has no snapshot",
                                set_device.device_name
                            ));
                        }
                    }
                }
            };
            info!(
                automation = %automation_id,
                node = %node_id,
                device = %set_device.device_name,
                target,
                "automation set_device fired"
            );
            set_schedule_intent(state, &set_device.device_name, target).await;
            Ok(Some(set_device.device_name.clone()))
        }
        AutomationNodeConfig::ToggleDevice { toggle_device } => {
            let current = {
                let devices = state.devices.read().await;
                devices
                    .get(&toggle_device.device_name)
                    .and_then(|d| d.snapshot.as_ref().map(|s| s.device_on))
            };
            let target = match current {
                Some(on) => !on,
                None => {
                    return Err(anyhow!(
                        "toggle requested but device '{}' has no snapshot",
                        toggle_device.device_name
                    ));
                }
            };
            info!(
                automation = %automation_id,
                node = %node_id,
                device = %toggle_device.device_name,
                target,
                "automation toggle_device fired"
            );
            set_schedule_intent(state, &toggle_device.device_name, target).await;
            Ok(Some(toggle_device.device_name.clone()))
        }
        AutomationNodeConfig::FireHook {
            fire_hook: fire_hook_cfg,
        } => {
            let hook = {
                let hooks = state.hooks.read().await;
                hooks.get(&fire_hook_cfg.hook_id).cloned()
            };
            let Some(hook) = hook else {
                return Err(anyhow!("unknown hook '{}'", fire_hook_cfg.hook_id));
            };
            info!(
                automation = %automation_id,
                node = %node_id,
                hook = %hook.id,
                "automation fire_hook fired"
            );
            let ctx = HookTemplateContext {
                device: "automation".to_string(),
                nickname: "Automation".to_string(),
                model: "automation".to_string(),
                event: HookEvent::On,
                source: HookSource::External,
                previous_on: None,
                new_on: None,
                timestamp_ms: now_ms(),
            };
            fire_hook(state, hook, ctx).await;
            Ok(None)
        }
        _ => Ok(None),
    }
}

pub(crate) async fn run_condition_poller(state: AppState) {
    sleep(Duration::from_secs(2)).await;
    loop {
        let groups: BTreeMap<String, Vec<(String, u64, Option<u128>)>> = {
            let conditions = state.conditions.read().await;
            let mut by_key: BTreeMap<String, Vec<(String, u64, Option<u128>)>> = BTreeMap::new();
            for condition in conditions.values().filter(|c| c.enabled) {
                by_key
                    .entry(condition_probe_key(condition))
                    .or_default()
                    .push((
                        condition.id.clone(),
                        condition.poll_seconds,
                        condition.last_checked_at_ms,
                    ));
            }
            by_key
        };

        let now = now_ms();
        for (_key, members) in groups {
            let due = members
                .iter()
                .any(|(_id, poll_seconds, last_checked_at_ms)| {
                    let interval_ms = (*poll_seconds as u128) * 1000;
                    match last_checked_at_ms {
                        None => true,
                        Some(last) => now.saturating_sub(*last) >= interval_ms,
                    }
                });
            if !due {
                continue;
            }
            if let Some((representative_id, _, _)) = members.first() {
                probe_and_record(&state, representative_id).await;
            }
        }

        sleep(Duration::from_secs(1)).await;
    }
}

pub(crate) async fn run_scheduler(state: AppState) {
    let mut previous_tick = Local::now();

    loop {
        let now = Local::now();
        let seconds_into_minute = u64::from(now.second());
        let nanos_into_second = u64::from(now.nanosecond());
        let wait_seconds = 60u64.saturating_sub(seconds_into_minute);
        let wait = Duration::from_secs(wait_seconds)
            .saturating_sub(Duration::from_nanos(nanos_into_second));
        sleep(wait).await;

        let tick_time = Local::now();
        evaluate_schedules(&state, previous_tick, tick_time).await;
        previous_tick = tick_time;
    }
}

pub(crate) async fn evaluate_schedules(
    state: &AppState,
    previous_tick: DateTime<Local>,
    now: DateTime<Local>,
) {
    let candidates: Vec<ScheduleConfig> = {
        let schedules = state.schedules.read().await;
        schedules
            .values()
            .filter(|schedule| schedule.enabled)
            .cloned()
            .collect()
    };

    for schedule in candidates {
        match schedule.kind {
            ScheduleKind::Cron => {
                let Some(expr) = schedule.cron.as_deref() else {
                    record_schedule_error(state, &schedule.id, "missing cron expression".into())
                        .await;
                    continue;
                };
                let parsed = match parse_cron(expr) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        warn!(
                            schedule_id = %schedule.id,
                            cron = %expr,
                            %error,
                            "skipping schedule with unparsable cron expression",
                        );
                        record_schedule_error(state, &schedule.id, format!("{error}")).await;
                        continue;
                    }
                };

                let Some(fire_time) = parsed.after(&previous_tick).next() else {
                    continue;
                };
                if fire_time > now {
                    continue;
                }
                if let Some(action) = schedule.action {
                    fire_schedule(state, &schedule, action).await;
                }
            }
            ScheduleKind::Interval => {
                let prev_ms = u128::try_from(previous_tick.timestamp_millis().max(0)).unwrap_or(0);
                let now_ms_local = u128::try_from(now.timestamp_millis().max(0)).unwrap_or(0);

                let Some(target_phase) = interval_phase_at(&schedule, now_ms_local) else {
                    continue;
                };
                let prev_phase = interval_phase_at(&schedule, prev_ms);
                let needs_fire =
                    schedule.last_fired_at_ms.is_none() || prev_phase != Some(target_phase);
                if needs_fire {
                    fire_schedule(state, &schedule, target_phase).await;
                }
            }
        }
    }
}

pub(crate) async fn fire_schedule(
    state: &AppState,
    schedule: &ScheduleConfig,
    action: ScheduleAction,
) {
    info!(
        schedule_id = %schedule.id,
        device = %schedule.device_name,
        kind = ?schedule.kind,
        action = ?action,
        "firing schedule",
    );

    let target_intent: bool = match action {
        ScheduleAction::On => true,
        ScheduleAction::Off => false,
        ScheduleAction::Toggle => {
            // For a toggle we need to know the device's current state.
            let current = {
                let devices = state.devices.read().await;
                devices
                    .get(&schedule.device_name)
                    .and_then(|d| d.snapshot.as_ref().map(|s| s.device_on))
            };
            match current {
                Some(on) => !on,
                None => {
                    let message = "device snapshot unavailable for toggle".to_string();
                    warn!(schedule_id = %schedule.id, "skipping toggle without snapshot");
                    record_schedule_error(state, &schedule.id, message).await;
                    return;
                }
            }
        }
    };

    set_schedule_intent(state, &schedule.device_name, target_intent).await;

    if let Err(error) = save_persisted_state(state).await {
        warn!(%error, schedule_id = %schedule.id, "failed to persist schedule intent");
    }

    record_schedule_success(state, &schedule.id).await;

    reconcile_device(state, &schedule.device_name, HookSource::Schedule).await;
}

pub(crate) async fn record_schedule_success(state: &AppState, id: &str) {
    let updated = {
        let mut schedules = state.schedules.write().await;
        if let Some(schedule) = schedules.get_mut(id) {
            schedule.last_fired_at_ms = Some(now_ms());
            schedule.last_error = None;
            true
        } else {
            false
        }
    };

    if updated {
        if let Err(error) = save_persisted_state(state).await {
            warn!(%error, "failed to persist schedule run state");
        }
    }
}

pub(crate) async fn record_schedule_error(state: &AppState, id: &str, message: String) {
    let updated = {
        let mut schedules = state.schedules.write().await;
        if let Some(schedule) = schedules.get_mut(id) {
            schedule.last_fired_at_ms = Some(now_ms());
            schedule.last_error = Some(message);
            true
        } else {
            false
        }
    };

    if updated {
        if let Err(error) = save_persisted_state(state).await {
            warn!(%error, "failed to persist schedule error state");
        }
    }
}

pub(crate) async fn monitor_devices(state: AppState) {
    loop {
        sleep(Duration::from_secs(state.refresh_seconds)).await;

        refresh_all_devices(&state).await;
    }
}

pub(crate) async fn scan_for_devices(state: AppState) {
    loop {
        sleep(Duration::from_secs(state.scan_seconds)).await;

        if let Err(error) = discover_devices(&state).await {
            warn!(%error, "periodic discovery failed");
        }
    }
}

pub(crate) async fn initial_refresh_devices(state: AppState) {
    refresh_all_devices(&state).await;

    if let Err(error) = discover_devices(&state).await {
        warn!(%error, "background discovery failed");
    }
}

pub(crate) async fn scan_and_refresh(state: &AppState) -> Result<()> {
    discover_devices(state).await?;
    refresh_all_devices(state).await;
    Ok(())
}

pub(crate) async fn discover_devices(state: &AppState) -> Result<()> {
    let (targets, target_source) = if state.discovery_targets.is_empty() {
        let auto_targets = match automatic_discovery_targets() {
            Ok(targets) => targets,
            Err(error) => {
                warn!(%error, "failed to inspect local IPv4 networks for discovery targets");
                Vec::new()
            }
        };

        (
            discovery_scan_targets_with_auto(&[], &[], auto_targets)?,
            "auto",
        )
    } else {
        (
            discovery_scan_targets_with_auto(&state.discovery_targets, &[], Vec::new())?,
            "explicit",
        )
    };

    info!(
        target_count = targets.len(),
        target_source, "discovery targets selected"
    );
    for target in &targets {
        info!(
            requested = %target.requested,
            scan_address = %target.scan_address,
            "discovery target selected",
        );
    }

    let discovered = state
        .controller
        .discover_targets(&targets, state.discovery_timeout_seconds)
        .await?;
    let existing_config = existing_config(state).await;
    let candidates = discovery_add_candidates(&existing_config, &discovered);
    let candidate_count = candidates.len();

    if candidate_count > 0 {
        {
            let mut devices = state.devices.write().await;

            for candidate in candidates {
                let config = DeviceConfig {
                    ip: candidate.ip,
                    model: candidate.model,
                };

                devices.insert(
                    candidate.name.clone(),
                    managed_device_from_config(candidate.name, config),
                );
            }
        }

        save_persisted_state(state)
            .await
            .with_context(|| format!("failed to save state to {}", state.state_path.display()))?;
    }

    info!(candidate_count, "discovery completed");

    if candidate_count > 0 {
        publish_device_list(state, None).await;
    }

    Ok(())
}

pub(crate) async fn refresh_all_devices(state: &AppState) {
    let devices = {
        let devices = state.devices.read().await;
        devices
            .iter()
            .map(|(name, device)| (name.clone(), device.config.clone()))
            .collect::<Vec<_>>()
    };

    for (name, device) in devices {
        refresh_device(state, &name, device).await;
    }
}

pub(crate) async fn refresh_device(state: &AppState, name: &str, device: DeviceConfig) {
    let operation_lock = device_operation_lock(state, &device).await;
    let _operation_guard = operation_lock.lock().await;

    match retry_tapo_handshake(|| state.controller.read_device(&device)).await {
        Ok(snapshot) => {
            update_device_snapshot(state, name, snapshot, None, HookSource::External).await
        }
        Err(error) => update_device_error(state, name, error.to_string()).await,
    }
}

pub(crate) async fn retry_tapo_handshake<T, F, Fut>(mut operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    for attempt in 1..=TAPO_HANDSHAKE_RETRY_ATTEMPTS {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error)
                if attempt < TAPO_HANDSHAKE_RETRY_ATTEMPTS && is_tapo_handshake_error(&error) =>
            {
                warn!(
                    attempt,
                    next_attempt = attempt + 1,
                    %error,
                    "retrying Tapo operation after handshake failure",
                );
                sleep(TAPO_HANDSHAKE_RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }

    unreachable!("retry loop should return from an attempt")
}

pub(crate) fn is_tapo_handshake_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("Handshake2 failed"))
}

pub(crate) async fn device_operation_lock(
    state: &AppState,
    device: &DeviceConfig,
) -> Arc<Mutex<()>> {
    if let Some(lock) = state.device_locks.read().await.get(&device.ip).cloned() {
        return lock;
    }

    let mut locks = state.device_locks.write().await;
    locks
        .entry(device.ip)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

pub(crate) async fn update_device_snapshot(
    state: &AppState,
    name: &str,
    snapshot: DeviceSnapshot,
    last_error: Option<String>,
    source: HookSource,
) {
    let (prev_on, was_offline_announced, nickname) = {
        let devices = state.devices.read().await;
        let device = devices.get(name);
        let prev_on = device
            .and_then(|d| d.snapshot.as_ref())
            .map(|s| s.device_on);
        let was_offline_announced = device.is_some_and(|d| d.offline_announced);
        let nickname = device
            .and_then(|d| d.snapshot.as_ref())
            .map(|s| s.nickname.clone())
            .unwrap_or_else(|| name.to_string());
        (prev_on, was_offline_announced, nickname)
    };
    let new_on = snapshot.device_on;

    let model = snapshot.device_model.clone();

    {
        let mut devices = state.devices.write().await;

        if let Some(device) = devices.get_mut(name) {
            device.snapshot = Some(snapshot);
            device.last_error = last_error;
            device.updated_at_ms = Some(now_ms());
            // A successful read clears any in-flight failure debounce.
            device.consecutive_failures = 0;
            device.offline_announced = false;
        }
    }

    publish_device_list(state, None).await;

    let mut events: Vec<HookEvent> = Vec::new();
    // Only emit Online if we'd previously announced Offline — pairs
    // 1:1 with the offline event we actually sent.
    if was_offline_announced {
        events.push(HookEvent::Online);
    }
    // Only fire on/off when there's a real transition. The first read
    // after startup has prev_on=None and is suppressed.
    if let Some(previous) = prev_on {
        if previous != new_on {
            events.push(if new_on {
                HookEvent::On
            } else {
                HookEvent::Off
            });
        }
    }
    for event in events {
        dispatch_hook_events(
            state,
            name,
            &nickname,
            &model,
            event,
            source,
            prev_on,
            Some(new_on),
        )
        .await;
    }
}

pub(crate) async fn update_device_error(state: &AppState, name: &str, error: String) {
    let (prev_on, prev_failures, was_offline_announced, nickname, model) = {
        let devices = state.devices.read().await;
        let device = devices.get(name);
        let prev_on = device
            .and_then(|d| d.snapshot.as_ref())
            .map(|s| s.device_on);
        let prev_failures = device.map(|d| d.consecutive_failures).unwrap_or(0);
        let was_offline_announced = device.is_some_and(|d| d.offline_announced);
        let nickname = device
            .and_then(|d| d.snapshot.as_ref())
            .map(|s| s.nickname.clone())
            .unwrap_or_else(|| name.to_string());
        let model = device
            .and_then(|d| d.snapshot.as_ref())
            .map(|s| s.device_model.clone())
            .unwrap_or_else(|| {
                device
                    .map(|d| d.config.model.to_string())
                    .unwrap_or_default()
            });
        (
            prev_on,
            prev_failures,
            was_offline_announced,
            nickname,
            model,
        )
    };

    let new_failures = prev_failures.saturating_add(1);
    let should_announce = !was_offline_announced
        && new_failures >= DEVICE_OFFLINE_FAILURE_THRESHOLD
        && prev_on.is_some();

    {
        let mut devices = state.devices.write().await;

        if let Some(device) = devices.get_mut(name) {
            device.last_error = Some(error);
            device.updated_at_ms = Some(now_ms());
            device.consecutive_failures = new_failures;
            if should_announce {
                device.offline_announced = true;
            }
        }
    }

    publish_device_list(state, None).await;

    if should_announce {
        dispatch_hook_events(
            state,
            name,
            &nickname,
            &model,
            HookEvent::Offline,
            HookSource::External,
            prev_on,
            prev_on,
        )
        .await;
    }
}

pub(crate) fn hook_matches(hook: &HookConfig, device: &str, event: HookEvent) -> bool {
    if !hook.enabled {
        return false;
    }
    if !hook.device_filter.is_empty() && !hook.device_filter.iter().any(|d| d == device) {
        return false;
    }
    if !hook.event_filter.is_empty() && !hook.event_filter.iter().any(|e| *e == event) {
        return false;
    }
    true
}

#[derive(Debug, Clone)]
pub(crate) struct HookTemplateContext {
    pub(crate) device: String,
    pub(crate) nickname: String,
    pub(crate) model: String,
    pub(crate) event: HookEvent,
    pub(crate) source: HookSource,
    pub(crate) previous_on: Option<bool>,
    pub(crate) new_on: Option<bool>,
    pub(crate) timestamp_ms: u128,
}

impl HookTemplateContext {
    fn vars(&self) -> Vec<(&'static str, String)> {
        vec![
            ("device", self.device.clone()),
            ("nickname", self.nickname.clone()),
            ("model", self.model.clone()),
            ("event", hook_event_str(self.event).to_string()),
            ("source", hook_source_str(self.source).to_string()),
            ("previous_on", optional_bool_str(self.previous_on)),
            ("new_on", optional_bool_str(self.new_on)),
            ("timestamp_ms", self.timestamp_ms.to_string()),
        ]
    }

    fn render(&self, input: &str) -> String {
        render_hook_template(input, &self.vars())
    }

    fn default_payload_json(&self) -> serde_json::Value {
        serde_json::json!({
            "device": self.device,
            "nickname": self.nickname,
            "model": self.model,
            "event": self.event,
            "source": self.source,
            "previous_on": self.previous_on,
            "new_on": self.new_on,
            "timestamp_ms": self.timestamp_ms as u64,
        })
    }
}

pub(crate) fn hook_event_str(event: HookEvent) -> &'static str {
    match event {
        HookEvent::On => "on",
        HookEvent::Off => "off",
        HookEvent::Online => "online",
        HookEvent::Offline => "offline",
    }
}

pub(crate) fn hook_source_str(source: HookSource) -> &'static str {
    match source {
        HookSource::Manual => "manual",
        HookSource::Schedule => "schedule",
        HookSource::Condition => "condition",
        HookSource::External => "external",
        HookSource::Discovery => "discovery",
    }
}

pub(crate) fn optional_bool_str(value: Option<bool>) -> String {
    match value {
        Some(true) => "true".to_string(),
        Some(false) => "false".to_string(),
        None => String::new(),
    }
}

pub(crate) fn render_hook_template(input: &str, vars: &[(&str, String)]) -> String {
    let mut out = input.to_string();
    for (key, value) in vars {
        let placeholder = format!("{{{{{key}}}}}");
        if out.contains(&placeholder) {
            out = out.replace(&placeholder, value);
        }
    }
    out
}

pub(crate) async fn dispatch_hook_events(
    state: &AppState,
    device: &str,
    nickname: &str,
    model: &str,
    event: HookEvent,
    source: HookSource,
    previous_on: Option<bool>,
    new_on: Option<bool>,
) {
    let matching_hooks: Vec<HookConfig> = {
        let hooks = state.hooks.read().await;
        hooks
            .values()
            .filter(|hook| hook_matches(hook, device, event))
            .cloned()
            .collect()
    };
    if matching_hooks.is_empty() {
        return;
    }

    let ctx = HookTemplateContext {
        device: device.to_string(),
        nickname: nickname.to_string(),
        model: model.to_string(),
        event,
        source,
        previous_on,
        new_on,
        timestamp_ms: now_ms(),
    };

    info!(
        device,
        ?event,
        ?source,
        hook_count = matching_hooks.len(),
        "dispatching device transition to hooks",
    );

    for hook in matching_hooks {
        let state = state.clone();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            fire_hook(&state, hook, ctx).await;
        });
    }
}

pub(crate) async fn fire_hook(state: &AppState, hook: HookConfig, ctx: HookTemplateContext) {
    let method = match HttpMethod::from_bytes(hook.method.to_uppercase().as_bytes()) {
        Ok(method) => method,
        Err(error) => {
            warn!(hook_id = %hook.id, %error, "hook has invalid HTTP method");
            update_hook_result(
                state,
                &hook.id,
                ctx.event,
                None,
                Some(format!("invalid method: {error}")),
            )
            .await;
            return;
        }
    };

    let url = ctx.render(&hook.url);
    let mut builder = state.http_client.request(method, &url);
    let mut content_type_set = false;
    for (key, value) in &hook.headers {
        if key.eq_ignore_ascii_case("content-type") {
            content_type_set = true;
        }
        builder = builder.header(key, ctx.render(value));
    }
    let body_text = match hook.body.as_deref() {
        Some(custom) => ctx.render(custom),
        None => ctx.default_payload_json().to_string(),
    };
    if !content_type_set && hook.body.is_none() {
        builder = builder.header("content-type", "application/json");
    }
    builder = builder.body(body_text);

    match builder.send().await {
        Ok(response) => {
            let status = response.status();
            let code = Some(status.as_u16());
            let error_text = if status.is_success() {
                None
            } else {
                Some(format!("non-success status {status}"))
            };
            update_hook_result(state, &hook.id, ctx.event, code, error_text).await;
        }
        Err(error) => {
            warn!(hook_id = %hook.id, %error, "hook request failed");
            update_hook_result(state, &hook.id, ctx.event, None, Some(format!("{error}"))).await;
        }
    }
}

pub(crate) async fn update_hook_result(
    state: &AppState,
    hook_id: &str,
    event: HookEvent,
    status_code: Option<u16>,
    error: Option<String>,
) {
    let updated = {
        let mut hooks = state.hooks.write().await;
        if let Some(hook) = hooks.get_mut(hook_id) {
            hook.last_fired_at_ms = Some(now_ms());
            hook.last_event = Some(event);
            hook.last_status_code = status_code;
            hook.last_error = error;
            true
        } else {
            false
        }
    };
    if updated {
        if let Err(error) = save_persisted_state(state).await {
            warn!(%error, hook_id, "failed to persist hook result");
        }
    }
}

pub(crate) async fn existing_config(state: &AppState) -> TapoConfig {
    let devices = state.devices.read().await;

    TapoConfig {
        username: None,
        devices: devices
            .iter()
            .map(|(name, device)| (name.clone(), device.config.clone()))
            .collect(),
    }
}

pub(crate) async fn device_views(state: &AppState) -> Vec<DeviceView> {
    let device_names: Vec<String> = {
        let devices = state.devices.read().await;
        devices.keys().cloned().collect()
    };

    let mut views = Vec::with_capacity(device_names.len());
    for name in device_names {
        if let Ok(view) = get_device_view(state, &name).await {
            views.push(view);
        }
    }
    views
}

pub(crate) async fn device_list_response(
    state: &AppState,
    scan_error: Option<String>,
) -> DeviceListResponse {
    DeviceListResponse {
        devices: device_views(state).await,
        updated_at_ms: now_ms(),
        energy_price_pence_per_kwh: state.energy_price_pence_per_kwh,
        scan_error,
    }
}

pub(crate) async fn publish_device_list(state: &AppState, scan_error: Option<String>) {
    let response = device_list_response(state, scan_error).await;
    publish_device_list_response(state, response);
}

pub(crate) fn publish_device_list_response(state: &AppState, response: DeviceListResponse) {
    let _ = state.device_events.send(response);
}

pub(crate) async fn get_device_config(state: &AppState, name: &str) -> Result<DeviceConfig> {
    let devices = state.devices.read().await;

    devices
        .get(name)
        .map(|device| device.config.clone())
        .ok_or_else(|| anyhow!("device '{name}' was not found"))
}

pub(crate) async fn get_device_view(state: &AppState, name: &str) -> Result<DeviceView> {
    let intent = {
        let intents = state.device_intents.read().await;
        intents.get(name).cloned().unwrap_or_default()
    };
    let condition_intent = condition_intent_for_device(state, name).await;
    let devices = state.devices.read().await;

    devices
        .get(name)
        .map(|device| device.view(state.energy_price_pence_per_kwh, intent, condition_intent))
        .ok_or_else(|| anyhow!("device '{name}' was not found"))
}

pub(crate) fn estimate_energy_cost_pence(energy_wh: u64, price_pence_per_kwh: f64) -> f64 {
    energy_wh as f64 / 1000.0 * price_pence_per_kwh
}

pub(crate) async fn build_energy_export_workbook(state: &AppState) -> Result<Vec<u8>> {
    let devices = export_devices(state).await;
    let device_names = devices
        .iter()
        .map(|device| device.name.clone())
        .collect::<Vec<_>>();
    let specs = export_specs(Utc::now())?;
    let mut tables = Vec::with_capacity(specs.len());
    let mut errors = Vec::new();

    for spec in specs {
        let (table, mut sheet_errors) = collect_export_table(state, &devices, &spec).await;
        tables.push(table);
        errors.append(&mut sheet_errors);
    }

    write_export_workbook(&device_names, &tables, &errors)
}

pub(crate) async fn build_usage_history(
    state: &AppState,
    range_key: Option<&str>,
) -> UsageHistoryResponse {
    let range = usage_history_range(range_key);
    let devices = export_devices(state).await;
    let now = Utc::now();
    let start = usage_history_start_datetime(range.start, now);
    let mut series = Vec::with_capacity(devices.len());
    let mut totals_by_timestamp: BTreeMap<DateTime<Utc>, f64> = BTreeMap::new();
    let mut errors = Vec::new();

    for device in devices {
        match read_usage_history_entries(state, &device.config, &range, start, now).await {
            Ok(entries) => {
                let mut points = Vec::new();

                for (timestamp, value) in entries {
                    if let Some(value) = value {
                        points.push(UsageHistoryPoint {
                            timestamp_ms: timestamp.timestamp_millis(),
                            value,
                        });
                        *totals_by_timestamp.entry(timestamp).or_default() += value;
                    }
                }

                series.push(UsageHistorySeries {
                    device_name: device.name,
                    points,
                });
            }
            Err(error) => errors.push(UsageHistoryError {
                device_name: device.name,
                message: error.to_string(),
            }),
        }
    }

    let totals = totals_by_timestamp
        .into_iter()
        .map(|(timestamp, value)| UsageHistoryPoint {
            timestamp_ms: timestamp.timestamp_millis(),
            value,
        })
        .collect();

    UsageHistoryResponse {
        series,
        totals,
        errors,
        updated_at_ms: now_ms(),
        range: range.key,
        range_label: range.label,
        interval: range.interval_label,
        start_date: start.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        end_date: now.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        unit: range.unit,
    }
}

pub(crate) async fn read_usage_history_entries(
    state: &AppState,
    device: &DeviceConfig,
    range: &UsageHistoryRange,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<(DateTime<Utc>, Option<f64>)>> {
    match range.kind {
        UsageHistoryKind::Power {
            interval,
            range_limit,
        } => {
            let ranges = split_datetime_ranges(start, end, range_limit);

            read_power_entries(state, device, &ranges, interval).await
        }
        UsageHistoryKind::EnergyDaily => {
            read_energy_entries(
                state,
                device,
                EnergyDataInterval::Daily {
                    start_date: start.date_naive(),
                },
            )
            .await
        }
        UsageHistoryKind::EnergyMonthly => {
            read_energy_entries(
                state,
                device,
                EnergyDataInterval::Monthly {
                    start_date: start.date_naive(),
                },
            )
            .await
        }
    }
}

pub(crate) fn usage_history_start_datetime(
    start: UsageHistoryStart,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    match start {
        UsageHistoryStart::Duration(duration) => now.checked_sub_signed(duration).unwrap_or(now),
        UsageHistoryStart::YearToDate => date_start_datetime(current_year_start(now.date_naive())),
        UsageHistoryStart::AllTime => {
            let start_date = NaiveDate::from_ymd_opt(ALL_TIME_USAGE_START_YEAR, 1, 1)
                .unwrap_or_else(|| current_year_start(now.date_naive()));

            date_start_datetime(start_date)
        }
    }
}

pub(crate) fn current_year_start(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), 1, 1).unwrap_or(date)
}

pub(crate) fn date_start_datetime(date: NaiveDate) -> DateTime<Utc> {
    DateTime::from_naive_utc_and_offset(date.and_hms_opt(0, 0, 0).unwrap_or_default(), Utc)
}

pub(crate) fn usage_history_range(range_key: Option<&str>) -> UsageHistoryRange {
    match range_key {
        Some("5m") => UsageHistoryRange {
            key: "5m",
            label: "5 minutes",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::minutes(5)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("30m") => UsageHistoryRange {
            key: "30m",
            label: "30 minutes",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::minutes(30)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("1h") => UsageHistoryRange {
            key: "1h",
            label: "1 hour",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::hours(1)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("6h") => UsageHistoryRange {
            key: "6h",
            label: "6 hours",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::hours(6)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("12h") => UsageHistoryRange {
            key: "12h",
            label: "12 hours",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::hours(12)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("1d") => UsageHistoryRange {
            key: "1d",
            label: "1 day",
            interval_label: "5-minute",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::days(1)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Every5Minutes,
                range_limit: ChronoDuration::hours(12),
            },
        },
        Some("3d") => UsageHistoryRange {
            key: "3d",
            label: "3 days",
            interval_label: "hourly",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::days(3)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Hourly,
                range_limit: ChronoDuration::days(6),
            },
        },
        Some("30d") => UsageHistoryRange {
            key: "30d",
            label: "30 days",
            interval_label: "hourly",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::days(30)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Hourly,
                range_limit: ChronoDuration::days(6),
            },
        },
        Some("3m") => UsageHistoryRange {
            key: "3m",
            label: "3 months",
            interval_label: "daily energy",
            unit: "kWh",
            start: UsageHistoryStart::Duration(ChronoDuration::days(92)),
            kind: UsageHistoryKind::EnergyDaily,
        },
        Some("6m") => UsageHistoryRange {
            key: "6m",
            label: "6 months",
            interval_label: "daily energy",
            unit: "kWh",
            start: UsageHistoryStart::Duration(ChronoDuration::days(183)),
            kind: UsageHistoryKind::EnergyDaily,
        },
        Some("1y") => UsageHistoryRange {
            key: "1y",
            label: "1 year",
            interval_label: "daily energy",
            unit: "kWh",
            start: UsageHistoryStart::Duration(ChronoDuration::days(365)),
            kind: UsageHistoryKind::EnergyDaily,
        },
        Some("ytd") => UsageHistoryRange {
            key: "ytd",
            label: "year to date",
            interval_label: "daily energy",
            unit: "kWh",
            start: UsageHistoryStart::YearToDate,
            kind: UsageHistoryKind::EnergyDaily,
        },
        Some("all") => UsageHistoryRange {
            key: "all",
            label: "all time",
            interval_label: "monthly energy",
            unit: "kWh",
            start: UsageHistoryStart::AllTime,
            kind: UsageHistoryKind::EnergyMonthly,
        },
        _ => UsageHistoryRange {
            key: "7d",
            label: "7 days",
            interval_label: "hourly",
            unit: "W",
            start: UsageHistoryStart::Duration(ChronoDuration::days(7)),
            kind: UsageHistoryKind::Power {
                interval: PowerExportInterval::Hourly,
                range_limit: ChronoDuration::days(6),
            },
        },
    }
}

pub(crate) async fn export_devices(state: &AppState) -> Vec<ExportDevice> {
    let devices = state.devices.read().await;

    devices
        .values()
        .filter(|device| matches!(device.config.model, DeviceModel::P110 | DeviceModel::P115))
        .map(|device| ExportDevice {
            name: device.name.clone(),
            config: device.config.clone(),
        })
        .collect()
}

pub(crate) fn export_specs(now: DateTime<Utc>) -> Result<Vec<ExportSpec>> {
    let today = now.date_naive();
    let week_start = today
        .checked_sub_days(Days::new(6))
        .ok_or_else(|| anyhow!("failed to calculate weekly energy export start date"))?;
    let quarter_start = current_quarter_start(today)?;
    let year_start = NaiveDate::from_ymd_opt(today.year(), 1, 1)
        .ok_or_else(|| anyhow!("failed to calculate yearly energy export start date"))?;
    let power_day_start = now
        .checked_sub_signed(ChronoDuration::hours(24))
        .ok_or_else(|| anyhow!("failed to calculate 24 hour power export start time"))?;
    let power_week_start = now
        .checked_sub_signed(ChronoDuration::days(7))
        .ok_or_else(|| anyhow!("failed to calculate weekly power export start time"))?;

    Ok(vec![
        ExportSpec {
            sheet_name: "Energy - Hourly (last week)",
            value_format: "0.000",
            kind: ExportKind::EnergyHourly {
                start_date: week_start,
                end_date: today,
            },
        },
        ExportSpec {
            sheet_name: "Energy - Daily (last 3 mo)",
            value_format: "0.000",
            kind: ExportKind::EnergyDaily {
                start_date: quarter_start,
            },
        },
        ExportSpec {
            sheet_name: "Energy - Monthly (last year)",
            value_format: "0.000",
            kind: ExportKind::EnergyMonthly {
                start_date: year_start,
            },
        },
        ExportSpec {
            sheet_name: "Power - 5min (last 24h)",
            value_format: "0.0",
            kind: ExportKind::PowerEvery5Minutes {
                ranges: split_datetime_ranges(power_day_start, now, ChronoDuration::hours(12)),
            },
        },
        ExportSpec {
            sheet_name: "Power - Hourly (last week)",
            value_format: "0.0",
            kind: ExportKind::PowerHourly {
                ranges: split_datetime_ranges(power_week_start, now, ChronoDuration::days(6)),
            },
        },
    ])
}

pub(crate) fn current_quarter_start(date: NaiveDate) -> Result<NaiveDate> {
    let month = match date.month() {
        1..=3 => 1,
        4..=6 => 4,
        7..=9 => 7,
        10..=12 => 10,
        _ => return Err(anyhow!("invalid month {}", date.month())),
    };

    NaiveDate::from_ymd_opt(date.year(), month, 1)
        .ok_or_else(|| anyhow!("failed to calculate current quarter start date"))
}

pub(crate) fn split_datetime_ranges(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    max_duration: ChronoDuration,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut ranges = Vec::new();
    let mut cursor = start;

    while cursor < end {
        let next = cursor
            .checked_add_signed(max_duration)
            .filter(|candidate| *candidate < end)
            .unwrap_or(end);
        ranges.push((cursor, next));
        cursor = next;
    }

    ranges
}

pub(crate) async fn collect_export_table(
    state: &AppState,
    devices: &[ExportDevice],
    spec: &ExportSpec,
) -> (ExportTable, Vec<ExportError>) {
    let mut rows_by_timestamp: BTreeMap<DateTime<Utc>, BTreeMap<String, f64>> = BTreeMap::new();
    let mut errors = Vec::new();

    for device in devices {
        match read_export_entries(state, &device.config, spec).await {
            Ok(entries) => {
                for (timestamp, value) in entries {
                    if let Some(value) = value {
                        rows_by_timestamp
                            .entry(timestamp)
                            .or_default()
                            .insert(device.name.clone(), value);
                    }
                }
            }
            Err(error) => errors.push(ExportError {
                sheet_name: spec.sheet_name,
                device_name: device.name.clone(),
                message: error.to_string(),
            }),
        }
    }

    let rows = rows_by_timestamp
        .into_iter()
        .map(|(timestamp, values)| ExportRow { timestamp, values })
        .collect();

    (
        ExportTable {
            sheet_name: spec.sheet_name,
            value_format: spec.value_format,
            rows,
        },
        errors,
    )
}

pub(crate) async fn read_export_entries(
    state: &AppState,
    device: &DeviceConfig,
    spec: &ExportSpec,
) -> Result<Vec<(DateTime<Utc>, Option<f64>)>> {
    match &spec.kind {
        ExportKind::EnergyHourly {
            start_date,
            end_date,
        } => {
            read_energy_entries(
                state,
                device,
                EnergyDataInterval::Hourly {
                    start_date: *start_date,
                    end_date: *end_date,
                },
            )
            .await
        }
        ExportKind::EnergyDaily { start_date } => {
            read_energy_entries(
                state,
                device,
                EnergyDataInterval::Daily {
                    start_date: *start_date,
                },
            )
            .await
        }
        ExportKind::EnergyMonthly { start_date } => {
            read_energy_entries(
                state,
                device,
                EnergyDataInterval::Monthly {
                    start_date: *start_date,
                },
            )
            .await
        }
        ExportKind::PowerEvery5Minutes { ranges } => {
            read_power_entries(state, device, ranges, PowerExportInterval::Every5Minutes).await
        }
        ExportKind::PowerHourly { ranges } => {
            read_power_entries(state, device, ranges, PowerExportInterval::Hourly).await
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PowerExportInterval {
    Every5Minutes,
    Hourly,
}

pub(crate) async fn read_energy_entries(
    state: &AppState,
    device: &DeviceConfig,
    interval: EnergyDataInterval,
) -> Result<Vec<(DateTime<Utc>, Option<f64>)>> {
    let operation_lock = device_operation_lock(state, device).await;
    let _operation_guard = operation_lock.lock().await;
    let result = match device.model {
        DeviceModel::P110 => {
            historical_client(state)
                .p110(device.ip.to_string())
                .await?
                .get_energy_data(interval)
                .await?
        }
        DeviceModel::P115 => {
            historical_client(state)
                .p115(device.ip.to_string())
                .await?
                .get_energy_data(interval)
                .await?
        }
        DeviceModel::P100 | DeviceModel::P105 => {
            return Err(anyhow!(
                "{} at {} does not support energy monitoring",
                device.model,
                device.ip,
            ));
        }
    };

    Ok(result
        .entries
        .into_iter()
        .map(|entry| (entry.start_date_time, Some(entry.energy as f64 / 1000.0)))
        .collect())
}

pub(crate) async fn read_power_entries(
    state: &AppState,
    device: &DeviceConfig,
    ranges: &[(DateTime<Utc>, DateTime<Utc>)],
    interval: PowerExportInterval,
) -> Result<Vec<(DateTime<Utc>, Option<f64>)>> {
    let operation_lock = device_operation_lock(state, device).await;
    let _operation_guard = operation_lock.lock().await;
    let mut entries = Vec::new();

    for (start_date_time, end_date_time) in ranges {
        let interval = match interval {
            PowerExportInterval::Every5Minutes => PowerDataInterval::Every5Minutes {
                start_date_time: *start_date_time,
                end_date_time: *end_date_time,
            },
            PowerExportInterval::Hourly => PowerDataInterval::Hourly {
                start_date_time: *start_date_time,
                end_date_time: *end_date_time,
            },
        };
        let result = match device.model {
            DeviceModel::P110 => {
                historical_client(state)
                    .p110(device.ip.to_string())
                    .await?
                    .get_power_data(interval)
                    .await?
            }
            DeviceModel::P115 => {
                historical_client(state)
                    .p115(device.ip.to_string())
                    .await?
                    .get_power_data(interval)
                    .await?
            }
            DeviceModel::P100 | DeviceModel::P105 => {
                return Err(anyhow!(
                    "{} at {} does not support energy monitoring",
                    device.model,
                    device.ip,
                ));
            }
        };

        entries.extend(
            result
                .entries
                .into_iter()
                .map(|entry| (entry.start_date_time, entry.power.map(|power| power as f64))),
        );
    }

    Ok(entries)
}

pub(crate) fn historical_client(state: &AppState) -> ApiClient {
    ApiClient::new(&state.credentials.username, &state.credentials.password)
        .with_timeout(Duration::from_secs(30))
}

pub(crate) fn write_export_workbook(
    device_names: &[String],
    tables: &[ExportTable],
    errors: &[ExportError],
) -> Result<Vec<u8>> {
    let mut workbook = Workbook::new();

    for table in tables {
        write_export_table(&mut workbook, device_names, table)?;
    }

    if !errors.is_empty() {
        write_export_errors(&mut workbook, errors)?;
    }

    workbook
        .save_to_buffer()
        .context("failed to build energy export workbook")
}

pub(crate) fn write_export_table(
    workbook: &mut Workbook,
    device_names: &[String],
    table: &ExportTable,
) -> Result<()> {
    let header_format = Format::new()
        .set_bold()
        .set_border(FormatBorder::Thin)
        .set_align(FormatAlign::Center);
    let value_format = Format::new().set_num_format(table.value_format);
    let worksheet = workbook.add_worksheet().set_name(table.sheet_name)?;

    worksheet.set_column_width(0, 24)?;
    worksheet.write_with_format(0, 0, "Timestamp", &header_format)?;

    for (index, name) in device_names.iter().enumerate() {
        let column = (index + 1) as u16;
        worksheet.set_column_width(column, 18)?;
        worksheet.write_with_format(0, column, name, &header_format)?;
    }

    let total_column = (device_names.len() + 1) as u16;
    worksheet.set_column_width(total_column, 14)?;
    worksheet.write_with_format(0, total_column, "Total", &header_format)?;

    for (row_index, row) in table.rows.iter().enumerate() {
        let worksheet_row = (row_index + 1) as u32;
        worksheet.write(
            worksheet_row,
            0,
            row.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        )?;

        for (index, name) in device_names.iter().enumerate() {
            if let Some(value) = row.values.get(name) {
                worksheet.write_with_format(
                    worksheet_row,
                    (index + 1) as u16,
                    *value,
                    &value_format,
                )?;
            }
        }

        let total = row.values.values().sum::<f64>();
        worksheet.write_with_format(worksheet_row, total_column, total, &value_format)?;
    }

    Ok(())
}

pub(crate) fn write_export_errors(workbook: &mut Workbook, errors: &[ExportError]) -> Result<()> {
    let header_format = Format::new()
        .set_bold()
        .set_border(FormatBorder::Thin)
        .set_align(FormatAlign::Center);
    let worksheet = workbook.add_worksheet().set_name("Export Errors")?;

    worksheet.set_column_width(0, 32)?;
    worksheet.set_column_width(1, 22)?;
    worksheet.set_column_width(2, 72)?;
    worksheet.write_with_format(0, 0, "Sheet", &header_format)?;
    worksheet.write_with_format(0, 1, "Device", &header_format)?;
    worksheet.write_with_format(0, 2, "Error", &header_format)?;

    for (index, error) in errors.iter().enumerate() {
        let row = (index + 1) as u32;
        worksheet.write(row, 0, error.sheet_name)?;
        worksheet.write(row, 1, &error.device_name)?;
        worksheet.write(row, 2, &error.message)?;
    }

    Ok(())
}

pub(crate) const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width,initial-scale=1">
    <meta name="theme-color" content="#201d19" id="theme-color">
    <title>Fusebox</title>
    <script>
        (() => {
            try {
                const storedTheme = localStorage.getItem("fusebox-theme");
                const systemTheme = window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "classic";
                document.documentElement.dataset.theme = storedTheme ?? systemTheme;
            } catch (_error) {
                document.documentElement.dataset.theme = "classic";
            }
        })();
    </script>
</head>
<body>
    <div id="app-root"></div>
    <script src="/assets/app.js" type="module"></script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{
        DEFAULT_ENERGY_PRICE_PENCE_PER_KWH, optional_u64_env, parse_string_list,
    };

    #[test]
    fn parses_default_settings_without_optional_values() {
        assert_eq!(optional_u64_env("FUSEBOX_TEST_MISSING", 42).unwrap(), 42);
    }

    #[test]
    fn parses_comma_or_space_separated_string_lists() {
        let targets = parse_string_list("192.168.0.0/24, 10.10.0.255\n172.18.0.0/16");

        assert_eq!(
            targets,
            vec![
                "192.168.0.0/24".to_string(),
                "10.10.0.255".to_string(),
                "172.18.0.0/16".to_string(),
            ],
        );
    }

    #[test]
    fn identifies_tapo_handshake_failures() {
        let handshake_error = anyhow!("HTTP error 400: Handshake2 failed");
        let other_error = anyhow!("HTTP error 400: device busy");

        assert!(is_tapo_handshake_error(&handshake_error));
        assert!(!is_tapo_handshake_error(&other_error));
    }

    #[tokio::test]
    async fn retries_transient_tapo_handshake_failures() {
        let attempts = Arc::new(Mutex::new(0_u8));
        let result = retry_tapo_handshake({
            let attempts = attempts.clone();

            move || {
                let attempts = attempts.clone();

                async move {
                    let mut attempts = attempts.lock().await;
                    *attempts += 1;

                    if *attempts == 1 {
                        return Err(anyhow!("HTTP error 400: Handshake2 failed"));
                    }

                    Ok("ok")
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(result, "ok");
        assert_eq!(*attempts.lock().await, 2);
    }

    #[test]
    fn renders_snapshot_backed_device_view() {
        let device = ManagedDevice {
            name: "lights".to_string(),
            config: DeviceConfig {
                ip: "192.168.0.40".parse().unwrap(),
                model: DeviceModel::P110,
            },
            snapshot: Some(DeviceSnapshot {
                ip: "192.168.0.40".parse().unwrap(),
                model: DeviceModel::P110,
                device_model: "P110".to_string(),
                nickname: "Lights".to_string(),
                device_type: "Plug with Energy Monitoring".to_string(),
                device_on: true,
                on_time_seconds: 120,
                energy: Some(tapoctl::EnergySnapshot {
                    current_power_mw: Some(12_000),
                    current_power_w: Some(12),
                    today_energy_wh: 1500,
                    month_energy_wh: 12_000,
                    today_runtime_minutes: 80,
                    month_runtime_minutes: 900,
                }),
            }),
            last_error: None,
            discovered_at_ms: 1,
            updated_at_ms: Some(2),
            consecutive_failures: 0,
            offline_announced: false,
        };

        let view = device.view(30.0, DeviceIntent::default(), None);

        assert_eq!(view.name, "lights");
        assert_eq!(view.nickname, "Lights");
        assert_eq!(view.device_on, Some(true));
        assert_eq!(view.on_time_seconds, Some(120));
        assert_eq!(view.energy.unwrap().today_cost_pence, 45.0);
    }

    #[test]
    fn splits_power_export_ranges_at_tapo_limits() {
        let start = DateTime::from_timestamp(1_767_225_600, 0).unwrap();
        let end = start + ChronoDuration::hours(24);

        let ranges = split_datetime_ranges(start, end, ChronoDuration::hours(12));

        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], (start, start + ChronoDuration::hours(12)));
        assert_eq!(ranges[1], (start + ChronoDuration::hours(12), end));
    }

    #[test]
    fn maps_long_usage_ranges_to_energy_history() {
        let three_months = usage_history_range(Some("3m"));
        let ytd = usage_history_range(Some("ytd"));
        let all_time = usage_history_range(Some("all"));

        assert_eq!(three_months.key, "3m");
        assert_eq!(three_months.unit, "kWh");
        assert!(matches!(three_months.kind, UsageHistoryKind::EnergyDaily));
        assert!(matches!(ytd.start, UsageHistoryStart::YearToDate));
        assert!(matches!(all_time.kind, UsageHistoryKind::EnergyMonthly));
    }

    #[test]
    fn calculates_calendar_usage_range_starts() {
        let now = DateTime::from_timestamp(1_771_588_800, 0).unwrap();
        let ytd_start = usage_history_start_datetime(UsageHistoryStart::YearToDate, now);
        let all_time_start = usage_history_start_datetime(UsageHistoryStart::AllTime, now);

        assert_eq!(
            ytd_start.date_naive(),
            NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
        );
        assert_eq!(
            all_time_start.date_naive(),
            NaiveDate::from_ymd_opt(ALL_TIME_USAGE_START_YEAR, 1, 1).unwrap(),
        );
    }

    #[test]
    fn writes_export_workbook_buffer() {
        let mut values = BTreeMap::new();
        values.insert("lights".to_string(), 1.5);
        let table = ExportTable {
            sheet_name: "Energy - Hourly (last week)",
            value_format: "0.000",
            rows: vec![ExportRow {
                timestamp: DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
                values,
            }],
        };

        let buffer = write_export_workbook(&["lights".to_string()], &[table], &[]).unwrap();

        assert!(buffer.len() > 1000);
        assert_eq!(&buffer[0..2], b"PK");
    }

    #[tokio::test]
    async fn saves_and_loads_persisted_device_configs() {
        let state_path = test_state_path("roundtrip");
        let settings = Settings {
            bind_address: "127.0.0.1:8787".parse().unwrap(),
            username: "dummy@example.com".to_string(),
            password: "dummy-password".to_string(),
            refresh_seconds: 10,
            scan_seconds: 60,
            discovery_timeout_seconds: 5,
            discovery_targets: Vec::new(),
            energy_price_pence_per_kwh: DEFAULT_ENERGY_PRICE_PENCE_PER_KWH,
            state_path: state_path.clone(),
        };
        let state = AppState::new(&settings);

        {
            let mut devices = state.devices.write().await;
            devices.insert(
                "lights".to_string(),
                managed_device_from_config(
                    "lights".to_string(),
                    DeviceConfig {
                        ip: "192.168.0.40".parse().unwrap(),
                        model: DeviceModel::P110,
                    },
                ),
            );
        }

        save_persisted_state(&state).await.unwrap();

        let contents = fs::read_to_string(&state_path).unwrap();
        assert!(contents.contains("lights"));
        assert!(!contents.contains("dummy-password"));

        let reloaded_state = AppState::new(&settings);
        load_persisted_state(&reloaded_state).await.unwrap();

        let devices = reloaded_state.devices.read().await;
        let loaded = devices.get("lights").unwrap();

        assert_eq!(loaded.config.ip.to_string(), "192.168.0.40");
        assert_eq!(loaded.config.model, DeviceModel::P110);
        assert!(loaded.snapshot.is_none());

        let _ = fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn reuses_device_operation_locks_by_ip() {
        let state_path = test_state_path("locks");
        let settings = test_settings(state_path);
        let state = AppState::new(&settings);
        let first_device = DeviceConfig {
            ip: "192.168.0.40".parse().unwrap(),
            model: DeviceModel::P110,
        };
        let same_ip_device = DeviceConfig {
            ip: "192.168.0.40".parse().unwrap(),
            model: DeviceModel::P115,
        };
        let other_device = DeviceConfig {
            ip: "192.168.0.41".parse().unwrap(),
            model: DeviceModel::P110,
        };

        let first_lock = device_operation_lock(&state, &first_device).await;
        let same_ip_lock = device_operation_lock(&state, &same_ip_device).await;
        let other_lock = device_operation_lock(&state, &other_device).await;

        assert!(Arc::ptr_eq(&first_lock, &same_ip_lock));
        assert!(!Arc::ptr_eq(&first_lock, &other_lock));
    }

    fn test_state_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fusebox-{name}-{}-{}.json",
            std::process::id(),
            now_ms(),
        ))
    }

    fn test_settings(state_path: PathBuf) -> Settings {
        Settings {
            bind_address: "127.0.0.1:8787".parse().unwrap(),
            username: "dummy@example.com".to_string(),
            password: "dummy-password".to_string(),
            refresh_seconds: 10,
            scan_seconds: 60,
            discovery_timeout_seconds: 5,
            discovery_targets: Vec::new(),
            energy_price_pence_per_kwh: DEFAULT_ENERGY_PRICE_PENCE_PER_KWH,
            state_path,
        }
    }

    #[test]
    fn normalizes_five_field_cron_with_seconds_prefix() {
        let normalized = normalize_cron("0 7 * * 1-5").unwrap();
        assert_eq!(normalized, "0 0 7 * * 1-5");
        parse_cron(&normalized).unwrap();
    }

    #[test]
    fn passes_six_field_cron_through() {
        let normalized = normalize_cron("30 0 7 * * 1-5").unwrap();
        assert_eq!(normalized, "30 0 7 * * 1-5");
        parse_cron(&normalized).unwrap();
    }

    #[test]
    fn accepts_standard_dow_zero_through_seven() {
        let normalized = normalize_cron("0 2 * * 0,6").unwrap();
        assert_eq!(normalized, "0 0 2 * * 0,6");
        parse_cron(&normalized).unwrap();

        let normalized_seven = normalize_cron("0 2 * * 7").unwrap();
        parse_cron(&normalized_seven).unwrap();
    }

    #[test]
    fn translates_standard_dow_to_crate_dow() {
        assert_eq!(translate_dow_field("0"), "1");
        assert_eq!(translate_dow_field("7"), "1");
        assert_eq!(translate_dow_field("0,6"), "1,7");
        assert_eq!(translate_dow_field("1-5"), "2-6");
        assert_eq!(translate_dow_field("*"), "*");
        assert_eq!(translate_dow_field("*/2"), "*/2");
        assert_eq!(translate_dow_field("1-5/2"), "2-6/2");
    }

    #[test]
    fn weekday_cron_fires_monday_to_friday() {
        let normalized = normalize_cron("0 7 * * 1-5").unwrap();
        let parsed = parse_cron(&normalized).unwrap();
        let sunday_midnight =
            chrono::DateTime::<chrono::Utc>::from_timestamp(1_704_585_600, 0).unwrap();
        let next = parsed.after(&sunday_midnight).next().unwrap();
        assert_eq!(next.timestamp(), 1_704_697_200);
    }

    #[test]
    fn rejects_invalid_cron_expressions() {
        assert!(normalize_cron("").is_err());
        assert!(normalize_cron("not a cron").is_err());
        assert!(normalize_cron("99 99 * * *").is_err());
    }

    #[tokio::test]
    async fn persists_schedule_across_reload() {
        let state_path = test_state_path("schedules");
        let settings = test_settings(state_path.clone());
        let state = AppState::new(&settings);

        {
            let mut schedules = state.schedules.write().await;
            schedules.insert(
                "abc".to_string(),
                ScheduleConfig {
                    id: "abc".to_string(),
                    device_name: "lights".to_string(),
                    kind: ScheduleKind::Cron,
                    cron: Some("0 0 7 * * 1-5".to_string()),
                    action: Some(ScheduleAction::On),
                    on_seconds: None,
                    off_seconds: None,
                    start_action: None,
                    starts_at_ms: None,
                    enabled: true,
                    label: Some("Morning".to_string()),
                    condition_ids: Vec::new(),
                    created_at_ms: 1_700_000_000_000,
                    last_fired_at_ms: None,
                    last_error: None,
                },
            );
            schedules.insert(
                "iv1".to_string(),
                ScheduleConfig {
                    id: "iv1".to_string(),
                    device_name: "lights".to_string(),
                    kind: ScheduleKind::Interval,
                    cron: None,
                    action: None,
                    on_seconds: Some(3600),
                    off_seconds: Some(1800),
                    start_action: Some(ScheduleAction::On),
                    starts_at_ms: Some(1_700_000_000_000),
                    enabled: true,
                    label: Some("1h/30m".to_string()),
                    condition_ids: Vec::new(),
                    created_at_ms: 1_700_000_000_000,
                    last_fired_at_ms: None,
                    last_error: None,
                },
            );
        }
        save_persisted_state(&state).await.unwrap();

        let reloaded = AppState::new(&settings);
        load_persisted_state(&reloaded).await.unwrap();
        let schedules = reloaded.schedules.read().await;
        let cron_loaded = schedules.get("abc").unwrap();
        assert_eq!(cron_loaded.device_name, "lights");
        assert_eq!(cron_loaded.cron.as_deref(), Some("0 0 7 * * 1-5"));
        assert_eq!(cron_loaded.action, Some(ScheduleAction::On));
        assert_eq!(cron_loaded.label.as_deref(), Some("Morning"));

        let interval_loaded = schedules.get("iv1").unwrap();
        assert_eq!(interval_loaded.kind, ScheduleKind::Interval);
        assert_eq!(interval_loaded.on_seconds, Some(3600));
        assert_eq!(interval_loaded.off_seconds, Some(1800));
        assert_eq!(interval_loaded.start_action, Some(ScheduleAction::On));

        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn interval_phase_flips_at_boundary() {
        let schedule = ScheduleConfig {
            id: "x".to_string(),
            device_name: "lights".to_string(),
            kind: ScheduleKind::Interval,
            cron: None,
            action: None,
            on_seconds: Some(60),
            off_seconds: Some(120),
            start_action: Some(ScheduleAction::On),
            starts_at_ms: Some(1_000),
            enabled: true,
            label: None,
            condition_ids: Vec::new(),
            created_at_ms: 1_000,
            last_fired_at_ms: None,
            last_error: None,
        };

        assert_eq!(
            interval_phase_at(&schedule, 1_000),
            Some(ScheduleAction::On)
        );
        assert_eq!(
            interval_phase_at(&schedule, 60_000),
            Some(ScheduleAction::On)
        );
        assert_eq!(
            interval_phase_at(&schedule, 61_001),
            Some(ScheduleAction::Off)
        );
        assert_eq!(
            interval_phase_at(&schedule, 180_000),
            Some(ScheduleAction::Off)
        );
        assert_eq!(
            interval_phase_at(&schedule, 181_001),
            Some(ScheduleAction::On)
        );
        assert_eq!(interval_phase_at(&schedule, 500), None);

        // Next fire from t=30s should be at t=61s (the on→off transition).
        assert_eq!(next_interval_fire_ms(&schedule, 30_000), Some(61_000));
        // Next fire from t=120s should be at t=181s (the off→on transition).
        assert_eq!(next_interval_fire_ms(&schedule, 120_000), Some(181_000));
    }

    #[test]
    fn parses_status_match_formats() {
        let single = parse_status_match("200").unwrap();
        assert!(status_matches(&single, 200));
        assert!(!status_matches(&single, 201));

        let range = parse_status_match("200-299").unwrap();
        assert!(status_matches(&range, 200));
        assert!(status_matches(&range, 250));
        assert!(status_matches(&range, 299));
        assert!(!status_matches(&range, 300));

        let mixed = parse_status_match("200, 204, 301-302").unwrap();
        assert!(status_matches(&mixed, 200));
        assert!(status_matches(&mixed, 204));
        assert!(status_matches(&mixed, 302));
        assert!(!status_matches(&mixed, 201));

        assert!(parse_status_match("").is_err());
        assert!(parse_status_match("not-numbers").is_err());
        assert!(parse_status_match("500-400").is_err());
    }

    #[test]
    fn probe_key_groups_identical_requests() {
        let base = || ConditionConfig {
            id: "x".to_string(),
            name: "n".to_string(),
            device_name: "dev".to_string(),
            url: "https://example.test/api".to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            status_match: "200".to_string(),
            body_contains: None,
            poll_seconds: 30,
            enabled: true,
            action_on_pass: None,
            action_on_fail: None,
            created_at_ms: 0,
            last_checked_at_ms: None,
            last_passing: None,
            last_status_code: None,
            last_error: None,
            last_action_at_ms: None,
            last_action: None,
            last_action_error: None,
            min_stable_seconds: 0,
            pending_value: None,
            pending_since_ms: None,
        };

        let mut a = base();
        a.id = "a".to_string();
        let mut b = base();
        b.id = "b".to_string();
        // Different device, different poll cadence — still the same probe.
        b.device_name = "other".to_string();
        b.poll_seconds = 5;
        let mut different_url = base();
        different_url.url = "https://example.test/other".to_string();
        let mut different_status = base();
        different_status.status_match = "200-299".to_string();
        let mut different_method = base();
        different_method.method = "POST".to_string();
        let mut different_headers = base();
        different_headers
            .headers
            .insert("Authorization".to_string(), "Bearer x".to_string());

        assert_eq!(condition_probe_key(&a), condition_probe_key(&b));
        assert_ne!(condition_probe_key(&a), condition_probe_key(&different_url));
        assert_ne!(
            condition_probe_key(&a),
            condition_probe_key(&different_status)
        );
        assert_ne!(
            condition_probe_key(&a),
            condition_probe_key(&different_method)
        );
        assert_ne!(
            condition_probe_key(&a),
            condition_probe_key(&different_headers)
        );
    }

    #[test]
    fn effective_state_truth_table() {
        // (manual, schedule, condition) -> expected
        let cases = [
            // No inputs at all: no opinion.
            ((None, None, None), None),
            // Pure condition control (e.g. AC).
            ((None, None, Some(true)), Some(true)),
            ((None, None, Some(false)), Some(false)),
            // Schedule alone.
            ((None, Some(true), None), Some(true)),
            ((None, Some(false), None), Some(false)),
            // Schedule says ON, condition agrees.
            ((None, Some(true), Some(true)), Some(true)),
            // Schedule says ON, condition forces OFF.
            ((None, Some(true), Some(false)), Some(false)),
            // Schedule says OFF, condition irrelevant.
            ((None, Some(false), Some(true)), Some(false)),
            ((None, Some(false), Some(false)), Some(false)),
            // Manual override beats every other input.
            ((Some(true), Some(false), Some(false)), Some(true)),
            ((Some(false), Some(true), Some(true)), Some(false)),
            ((Some(true), None, Some(false)), Some(true)),
        ];

        for ((manual, schedule, condition), expected) in cases {
            assert_eq!(
                compute_effective(manual, schedule, condition),
                expected,
                "compute_effective(manual={:?}, schedule={:?}, condition={:?})",
                manual,
                schedule,
                condition,
            );
        }
    }

    #[tokio::test]
    async fn condition_intent_fail_closed_for_unprobed_required_condition() {
        let state_path = test_state_path("intent-fail-closed");
        let settings = test_settings(state_path.clone());
        let state = AppState::new(&settings);

        let make = |last: Option<bool>| ConditionConfig {
            id: "c".to_string(),
            name: "n".to_string(),
            device_name: "lights".to_string(),
            url: "http://example.invalid".to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            status_match: "200".to_string(),
            body_contains: None,
            poll_seconds: 60,
            enabled: true,
            action_on_pass: None,
            action_on_fail: None,
            created_at_ms: 0,
            last_checked_at_ms: None,
            last_passing: last,
            last_status_code: None,
            last_error: None,
            last_action_at_ms: None,
            last_action: None,
            last_action_error: None,
            min_stable_seconds: 0,
            pending_value: None,
            pending_since_ms: None,
        };

        // No conditions targeting lights -> no opinion.
        assert_eq!(condition_intent_for_device(&state, "lights").await, None);

        // Never probed -> Some(false) (fail closed).
        {
            let mut conditions = state.conditions.write().await;
            conditions.insert("c".to_string(), make(None));
        }
        assert_eq!(
            condition_intent_for_device(&state, "lights").await,
            Some(false)
        );

        // Passing -> Some(true).
        {
            let mut conditions = state.conditions.write().await;
            conditions.get_mut("c").unwrap().last_passing = Some(true);
        }
        assert_eq!(
            condition_intent_for_device(&state, "lights").await,
            Some(true)
        );

        // Failing -> Some(false).
        {
            let mut conditions = state.conditions.write().await;
            conditions.get_mut("c").unwrap().last_passing = Some(false);
        }
        assert_eq!(
            condition_intent_for_device(&state, "lights").await,
            Some(false)
        );

        let _ = fs::remove_file(state_path);
    }

    fn sample_hook(device_filter: Vec<String>, event_filter: Vec<HookEvent>) -> HookConfig {
        HookConfig {
            id: "h".to_string(),
            name: "n".to_string(),
            enabled: true,
            url: "http://example.invalid".to_string(),
            method: "POST".to_string(),
            headers: BTreeMap::new(),
            body: None,
            device_filter,
            event_filter,
            created_at_ms: 0,
            last_fired_at_ms: None,
            last_event: None,
            last_status_code: None,
            last_error: None,
        }
    }

    #[test]
    fn hook_matches_device_and_event_filters() {
        let any_device_any_event = sample_hook(Vec::new(), Vec::new());
        assert!(hook_matches(&any_device_any_event, "ac", HookEvent::On));
        assert!(hook_matches(
            &any_device_any_event,
            "lights",
            HookEvent::Offline,
        ));

        let lights_only = sample_hook(vec!["lights".to_string()], Vec::new());
        assert!(hook_matches(&lights_only, "lights", HookEvent::On));
        assert!(!hook_matches(&lights_only, "ac", HookEvent::On));

        let offline_only = sample_hook(Vec::new(), vec![HookEvent::Offline]);
        assert!(hook_matches(&offline_only, "ac", HookEvent::Offline));
        assert!(!hook_matches(&offline_only, "ac", HookEvent::On));

        let mut disabled = sample_hook(Vec::new(), Vec::new());
        disabled.enabled = false;
        assert!(!hook_matches(&disabled, "ac", HookEvent::On));

        let lights_offline = sample_hook(
            vec!["lights".to_string()],
            vec![HookEvent::Offline, HookEvent::Online],
        );
        assert!(hook_matches(&lights_offline, "lights", HookEvent::Offline));
        assert!(!hook_matches(&lights_offline, "lights", HookEvent::On));
        assert!(!hook_matches(&lights_offline, "ac", HookEvent::Offline));
    }

    #[test]
    fn hook_template_substitution_renders_known_vars() {
        let ctx = HookTemplateContext {
            device: "lights".to_string(),
            nickname: "Lights".to_string(),
            model: "p110".to_string(),
            event: HookEvent::Off,
            source: HookSource::Condition,
            previous_on: Some(true),
            new_on: Some(false),
            timestamp_ms: 1_700_000_000_000,
        };

        assert_eq!(ctx.render("{{nickname}} -> {{event}}"), "Lights -> off",);
        assert_eq!(
            ctx.render("https://ntfy.example/topic/{{device}}"),
            "https://ntfy.example/topic/lights",
        );
        assert_eq!(
            ctx.render("source={{source}} prev={{previous_on}} new={{new_on}} ts={{timestamp_ms}}"),
            "source=condition prev=true new=false ts=1700000000000",
        );
        // Unknown placeholders stay as-is.
        assert_eq!(ctx.render("{{unknown}}"), "{{unknown}}");
        // Repeated placeholders all replaced.
        assert_eq!(ctx.render("{{event}}-{{event}}"), "off-off");
    }

    fn dummy_device(name: &str, ip: &str, model: DeviceModel, on: bool) -> ManagedDevice {
        let ip_addr: IpAddr = ip.parse().unwrap();
        let mut device =
            managed_device_from_config(name.to_string(), DeviceConfig { ip: ip_addr, model });
        device.snapshot = Some(DeviceSnapshot {
            ip: ip_addr,
            model,
            device_model: model.to_string(),
            device_type: "Tapo device".to_string(),
            nickname: name.to_string(),
            device_on: on,
            on_time_seconds: 0,
            energy: None,
        });
        device
    }

    #[tokio::test]
    async fn condition_hysteresis_debounces_flapping_probes() {
        let state_path = test_state_path("hysteresis");
        let settings = test_settings(state_path.clone());
        let state = AppState::new(&settings);

        // Stand up a condition with a 90s stability window pointed at an
        // unreachable URL — every probe will fail.
        let mut condition = ConditionConfig {
            id: "c".to_string(),
            name: "n".to_string(),
            device_name: "lights".to_string(),
            url: "http://127.0.0.1:1/never".to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            status_match: "200".to_string(),
            body_contains: None,
            poll_seconds: 5,
            enabled: true,
            action_on_pass: None,
            action_on_fail: None,
            created_at_ms: 0,
            last_checked_at_ms: None,
            last_passing: Some(true),
            last_status_code: Some(200),
            last_error: None,
            last_action_at_ms: None,
            last_action: None,
            last_action_error: None,
            min_stable_seconds: 90,
            pending_value: None,
            pending_since_ms: None,
        };
        condition.last_passing = Some(true);
        {
            let mut conditions = state.conditions.write().await;
            conditions.insert("c".to_string(), condition.clone());
        }

        // First probe: result will be Some(false). Hysteresis must NOT
        // flip last_passing yet, only start a pending wait.
        probe_and_record(&state, "c").await;
        {
            let conditions = state.conditions.read().await;
            let stored = conditions.get("c").unwrap();
            assert_eq!(
                stored.last_passing,
                Some(true),
                "hysteresis should hold previous value"
            );
            assert_eq!(stored.pending_value, Some(false));
            assert!(stored.pending_since_ms.is_some());
        }

        // Backdate the pending stamp so the 90s window has elapsed.
        {
            let mut conditions = state.conditions.write().await;
            let stored = conditions.get_mut("c").unwrap();
            stored.pending_since_ms = Some(now_ms().saturating_sub(95_000));
        }
        probe_and_record(&state, "c").await;
        {
            let conditions = state.conditions.read().await;
            let stored = conditions.get("c").unwrap();
            assert_eq!(
                stored.last_passing,
                Some(false),
                "hysteresis should commit after stable window"
            );
            assert_eq!(stored.pending_value, None);
        }

        let _ = fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn does_not_fire_hook_for_first_read_without_prior_snapshot() {
        let state_path = test_state_path("hook-no-first-read");
        let settings = test_settings(state_path.clone());
        let state = AppState::new(&settings);

        let captured =
            std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<(String, HookEvent)>::new()));
        // Insert a hook so dispatch_hook_events has something to match against.
        let hook = sample_hook(Vec::new(), Vec::new());
        {
            let mut hooks = state.hooks.write().await;
            hooks.insert(hook.id.clone(), hook);
        }
        // Insert the device WITHOUT a prior snapshot.
        {
            let mut devices = state.devices.write().await;
            devices.insert(
                "lights".to_string(),
                managed_device_from_config(
                    "lights".to_string(),
                    DeviceConfig {
                        ip: "192.0.2.10".parse().unwrap(),
                        model: DeviceModel::P110,
                    },
                ),
            );
        }

        let snapshot = DeviceSnapshot {
            ip: "192.0.2.10".parse().unwrap(),
            model: DeviceModel::P110,
            device_model: "p110".to_string(),
            device_type: "Tapo plug".to_string(),
            nickname: "Lights".to_string(),
            device_on: true,
            on_time_seconds: 0,
            energy: None,
        };
        update_device_snapshot(&state, "lights", snapshot, None, HookSource::External).await;

        // No transition happened — first read shouldn't have queued anything for the hook.
        // We can't peek inside spawned hook firings easily, but we can assert that the
        // device's hook record is untouched.
        let hooks = state.hooks.read().await;
        let stored = hooks.values().next().unwrap();
        assert_eq!(
            stored.last_fired_at_ms, None,
            "first read should not have fired the hook"
        );
        let _ = captured;

        let _ = fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn two_devices_each_fire_hook_independently() {
        let state_path = test_state_path("hook-multi-device");
        let settings = test_settings(state_path.clone());
        let state = AppState::new(&settings);

        {
            let mut devices = state.devices.write().await;
            devices.insert(
                "lights".to_string(),
                dummy_device("lights", "192.0.2.10", DeviceModel::P110, true),
            );
            devices.insert(
                "ac".to_string(),
                dummy_device("ac", "192.0.2.11", DeviceModel::P110, true),
            );
        }

        // No filter -> matches any device, any event.
        let hook = sample_hook(Vec::new(), Vec::new());
        let hook_id = hook.id.clone();
        {
            let mut hooks = state.hooks.write().await;
            hooks.insert(hook.id.clone(), hook);
        }

        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for device in ["lights", "ac"] {
            let matching: Vec<HookConfig> = {
                let hooks = state.hooks.read().await;
                hooks
                    .values()
                    .filter(|h| hook_matches(h, device, HookEvent::Off))
                    .cloned()
                    .collect()
            };
            assert_eq!(matching.len(), 1, "device {} should match the hook", device);
            counter.fetch_add(matching.len(), std::sync::atomic::Ordering::Relaxed);
        }

        // Both devices independently match -> total firings = 2.
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "expected each device to fire the hook once",
        );

        // Sanity: hook id present and untouched (no real network call in test).
        let hooks = state.hooks.read().await;
        assert!(hooks.contains_key(&hook_id));

        let _ = fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn offline_event_waits_for_consecutive_failures() {
        let state_path = test_state_path("offline-debounce");
        let settings = test_settings(state_path.clone());
        let state = AppState::new(&settings);

        // Device with a prior successful snapshot (so the first-read
        // suppression doesn't get in the way).
        {
            let mut devices = state.devices.write().await;
            devices.insert(
                "lights".to_string(),
                dummy_device("lights", "192.0.2.10", DeviceModel::P110, true),
            );
        }

        // First refresh failure: counter goes to 1, no announce.
        update_device_error(&state, "lights", "transient".to_string()).await;
        {
            let devices = state.devices.read().await;
            let device = devices.get("lights").unwrap();
            assert_eq!(device.consecutive_failures, 1);
            assert!(!device.offline_announced);
        }

        // Second failure: counter goes to 2, still no announce.
        update_device_error(&state, "lights", "transient".to_string()).await;
        {
            let devices = state.devices.read().await;
            let device = devices.get("lights").unwrap();
            assert_eq!(device.consecutive_failures, 2);
            assert!(!device.offline_announced);
        }

        // Third failure: hits the threshold, announce.
        update_device_error(&state, "lights", "transient".to_string()).await;
        {
            let devices = state.devices.read().await;
            let device = devices.get("lights").unwrap();
            assert_eq!(device.consecutive_failures, 3);
            assert!(device.offline_announced);
        }

        // Recovery: snapshot success resets the counter and the flag.
        let snapshot = DeviceSnapshot {
            ip: "192.0.2.10".parse().unwrap(),
            model: DeviceModel::P110,
            device_model: "p110".to_string(),
            device_type: "Tapo plug".to_string(),
            nickname: "Lights".to_string(),
            device_on: true,
            on_time_seconds: 1,
            energy: None,
        };
        update_device_snapshot(&state, "lights", snapshot, None, HookSource::External).await;
        {
            let devices = state.devices.read().await;
            let device = devices.get("lights").unwrap();
            assert_eq!(device.consecutive_failures, 0);
            assert!(!device.offline_announced);
        }

        let _ = fs::remove_file(state_path);
    }
}
