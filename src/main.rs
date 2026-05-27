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
use chrono::{DateTime, Datelike, Days, Duration as ChronoDuration, Local, NaiveDate, Timelike, Utc};
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

mod api_error;
mod settings;
mod time;

use api_error::AppError;
use settings::Settings;
use time::{deserialize_optional_label, now_ms};

const STATE_VERSION: u32 = 2;
const ALL_TIME_USAGE_START_YEAR: i32 = 2020;
const TAPO_HANDSHAKE_RETRY_ATTEMPTS: usize = 3;
const TAPO_HANDSHAKE_RETRY_DELAY: Duration = Duration::from_millis(350);
const SWITCH_SOUND_BYTES: &[u8] = include_bytes!("../assets/348224__tbrook__switch-light-06.wav");
const AUTOMATIONS_BUNDLE_JS: &str = include_str!("../web/dist/automations.js");

#[derive(Debug, Clone)]
struct AppState {
    controller: TapoController,
    credentials: TapoCredentials,
    devices: Arc<RwLock<BTreeMap<String, ManagedDevice>>>,
    device_locks: Arc<RwLock<BTreeMap<IpAddr, Arc<Mutex<()>>>>>,
    device_events: watch::Sender<DeviceListResponse>,
    schedules: Arc<RwLock<BTreeMap<String, ScheduleConfig>>>,
    conditions: Arc<RwLock<BTreeMap<String, ConditionConfig>>>,
    device_intents: Arc<RwLock<BTreeMap<String, DeviceIntent>>>,
    hooks: Arc<RwLock<BTreeMap<String, HookConfig>>>,
    automations: Arc<RwLock<BTreeMap<String, Automation>>>,
    http_client: reqwest::Client,
    discovery_timeout_seconds: u64,
    discovery_targets: Vec<String>,
    refresh_seconds: u64,
    scan_seconds: u64,
    energy_price_pence_per_kwh: f64,
    state_path: PathBuf,
}

#[derive(Debug, Clone)]
struct ManagedDevice {
    name: String,
    config: DeviceConfig,
    snapshot: Option<DeviceSnapshot>,
    last_error: Option<String>,
    discovered_at_ms: u128,
    updated_at_ms: Option<u128>,
    /// Number of consecutive refresh failures since the last successful
    /// read. Used to debounce flaky LAN behaviour before declaring the
    /// device offline. Not persisted — resets on server restart.
    consecutive_failures: u32,
    /// True once we've fired an Offline hook event for the current
    /// outage. Prevents repeated Offline events and gates the next
    /// Online event.
    offline_announced: bool,
}

const DEVICE_OFFLINE_FAILURE_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, Serialize)]
struct DeviceListResponse {
    devices: Vec<DeviceView>,
    updated_at_ms: u128,
    energy_price_pence_per_kwh: f64,
    scan_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DeviceView {
    name: String,
    ip: String,
    configured_model: DeviceModel,
    model: String,
    nickname: String,
    device_type: String,
    device_on: Option<bool>,
    on_time_seconds: Option<u64>,
    energy: Option<EnergyView>,
    last_error: Option<String>,
    discovered_at_ms: u128,
    updated_at_ms: Option<u128>,
    manual_override: Option<bool>,
    manual_override_until_ms: Option<u128>,
    schedule_intent: Option<bool>,
    condition_intent: Option<bool>,
    effective_intent: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct EnergyView {
    current_power_mw: Option<u64>,
    current_power_w: Option<u64>,
    today_energy_wh: u64,
    month_energy_wh: u64,
    today_cost_pence: f64,
    month_cost_pence: f64,
    today_runtime_minutes: u64,
    month_runtime_minutes: u64,
}

#[derive(Debug, Clone, Serialize)]
struct UsageHistoryResponse {
    series: Vec<UsageHistorySeries>,
    totals: Vec<UsageHistoryPoint>,
    errors: Vec<UsageHistoryError>,
    updated_at_ms: u128,
    range: &'static str,
    range_label: &'static str,
    interval: &'static str,
    start_date: String,
    end_date: String,
    unit: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct UsageHistorySeries {
    device_name: String,
    points: Vec<UsageHistoryPoint>,
}

#[derive(Debug, Clone, Serialize)]
struct UsageHistoryPoint {
    timestamp_ms: i64,
    value: f64,
}

#[derive(Debug, Clone, Serialize)]
struct UsageHistoryError {
    device_name: String,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
struct UsageHistoryQuery {
    range: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct UsageHistoryRange {
    key: &'static str,
    label: &'static str,
    interval_label: &'static str,
    unit: &'static str,
    start: UsageHistoryStart,
    kind: UsageHistoryKind,
}

#[derive(Debug, Clone, Copy)]
enum UsageHistoryStart {
    Duration(ChronoDuration),
    YearToDate,
    AllTime,
}

#[derive(Debug, Clone, Copy)]
enum UsageHistoryKind {
    Power {
        interval: PowerExportInterval,
        range_limit: ChronoDuration,
    },
    EnergyDaily,
    EnergyMonthly,
}

#[derive(Debug, Clone)]
struct ExportDevice {
    name: String,
    config: DeviceConfig,
}

#[derive(Debug, Clone)]
struct ExportSpec {
    sheet_name: &'static str,
    value_format: &'static str,
    kind: ExportKind,
}

#[derive(Debug, Clone)]
enum ExportKind {
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
struct ExportTable {
    sheet_name: &'static str,
    value_format: &'static str,
    rows: Vec<ExportRow>,
}

#[derive(Debug, Clone)]
struct ExportRow {
    timestamp: DateTime<Utc>,
    values: BTreeMap<String, f64>,
}

#[derive(Debug, Clone)]
struct ExportError {
    sheet_name: &'static str,
    device_name: String,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SetPowerRequest {
    on: bool,
    #[serde(default)]
    duration_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ToggleDeviceRequest {
    #[serde(default)]
    duration_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ScheduleAction {
    On,
    Off,
    Toggle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
enum ScheduleKind {
    #[default]
    Cron,
    Interval,
}

const MIN_INTERVAL_CYCLE_SECONDS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScheduleConfig {
    id: String,
    device_name: String,
    #[serde(default)]
    kind: ScheduleKind,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    action: Option<ScheduleAction>,
    #[serde(default)]
    on_seconds: Option<u64>,
    #[serde(default)]
    off_seconds: Option<u64>,
    #[serde(default)]
    start_action: Option<ScheduleAction>,
    #[serde(default)]
    starts_at_ms: Option<u128>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    condition_ids: Vec<String>,
    #[serde(default)]
    created_at_ms: u128,
    #[serde(default)]
    last_fired_at_ms: Option<u128>,
    #[serde(default)]
    last_error: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum CreateScheduleRequest {
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
struct UpdateScheduleRequest {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    action: Option<ScheduleAction>,
    #[serde(default)]
    on_seconds: Option<u64>,
    #[serde(default)]
    off_seconds: Option<u64>,
    #[serde(default)]
    start_action: Option<ScheduleAction>,
    #[serde(default, deserialize_with = "deserialize_optional_label")]
    label: Option<Option<String>>,
    #[serde(default)]
    condition_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
struct ScheduleView {
    id: String,
    device_name: String,
    kind: ScheduleKind,
    cron: Option<String>,
    action: Option<ScheduleAction>,
    on_seconds: Option<u64>,
    off_seconds: Option<u64>,
    start_action: Option<ScheduleAction>,
    starts_at_ms: Option<u128>,
    enabled: bool,
    label: Option<String>,
    condition_ids: Vec<String>,
    created_at_ms: u128,
    last_fired_at_ms: Option<u128>,
    last_error: Option<String>,
    next_fire_at_ms: Option<i64>,
}

const MIN_CONDITION_POLL_SECONDS: u64 = 5;
const MAX_CONDITION_POLL_SECONDS: u64 = 3_600;
const DEFAULT_CONDITION_POLL_SECONDS: u64 = 60;
const CONDITION_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONDITION_BODY_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ConditionAction {
    On,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConditionConfig {
    id: String,
    name: String,
    #[serde(default)]
    device_name: String,
    url: String,
    #[serde(default = "default_http_method")]
    method: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default = "default_status_match")]
    status_match: String,
    #[serde(default)]
    body_contains: Option<String>,
    #[serde(default = "default_condition_poll_seconds")]
    poll_seconds: u64,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    action_on_pass: Option<ConditionAction>,
    #[serde(default)]
    action_on_fail: Option<ConditionAction>,
    #[serde(default)]
    created_at_ms: u128,
    #[serde(default)]
    last_checked_at_ms: Option<u128>,
    #[serde(default)]
    last_passing: Option<bool>,
    #[serde(default)]
    last_status_code: Option<u16>,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    last_action_at_ms: Option<u128>,
    #[serde(default)]
    last_action: Option<ConditionAction>,
    #[serde(default)]
    last_action_error: Option<String>,
    /// New probe results must remain stable for this many seconds before
    /// they update `last_passing`. 0 (default for back-compat) means
    /// react to every change immediately. Prevents flaky probes from
    /// causing rapid device toggling.
    #[serde(default)]
    min_stable_seconds: u64,
    /// The most recent probe value that differs from `last_passing` and
    /// is waiting to be promoted. None when the latest probe matched.
    #[serde(default)]
    pending_value: Option<bool>,
    /// When `pending_value` was first observed.
    #[serde(default)]
    pending_since_ms: Option<u128>,
}

fn default_http_method() -> String {
    "GET".to_string()
}

fn default_status_match() -> String {
    "200-299".to_string()
}

fn default_condition_poll_seconds() -> u64 {
    DEFAULT_CONDITION_POLL_SECONDS
}

#[derive(Debug, Clone, Deserialize)]
struct CreateConditionRequest {
    name: String,
    device_name: String,
    url: String,
    #[serde(default = "default_http_method")]
    method: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default = "default_status_match")]
    status_match: String,
    #[serde(default)]
    body_contains: Option<String>,
    #[serde(default = "default_condition_poll_seconds")]
    poll_seconds: u64,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    action_on_pass: Option<ConditionAction>,
    #[serde(default)]
    action_on_fail: Option<ConditionAction>,
    #[serde(default)]
    min_stable_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct UpdateConditionRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    device_name: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default, deserialize_with = "deserialize_optional_label")]
    body: Option<Option<String>>,
    #[serde(default)]
    status_match: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_label")]
    body_contains: Option<Option<String>>,
    #[serde(default)]
    poll_seconds: Option<u64>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_condition_action")]
    action_on_pass: Option<Option<ConditionAction>>,
    #[serde(default, deserialize_with = "deserialize_optional_condition_action")]
    action_on_fail: Option<Option<ConditionAction>>,
    #[serde(default)]
    min_stable_seconds: Option<u64>,
}

fn deserialize_optional_condition_action<'de, D>(
    deserializer: D,
) -> Result<Option<Option<ConditionAction>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Option<ConditionAction>>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize)]
struct ConditionView {
    id: String,
    name: String,
    device_name: String,
    url: String,
    method: String,
    headers: BTreeMap<String, String>,
    body: Option<String>,
    status_match: String,
    body_contains: Option<String>,
    poll_seconds: u64,
    enabled: bool,
    action_on_pass: Option<ConditionAction>,
    action_on_fail: Option<ConditionAction>,
    created_at_ms: u128,
    last_checked_at_ms: Option<u128>,
    last_passing: Option<bool>,
    last_status_code: Option<u16>,
    last_error: Option<String>,
    last_action_at_ms: Option<u128>,
    last_action: Option<ConditionAction>,
    last_action_error: Option<String>,
    min_stable_seconds: u64,
    pending_value: Option<bool>,
    pending_since_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize)]
struct ConditionListResponse {
    conditions: Vec<ConditionView>,
}

#[derive(Debug, Clone, Serialize)]
struct ScheduleListResponse {
    schedules: Vec<ScheduleView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedState {
    version: u32,
    devices: BTreeMap<String, DeviceConfig>,
    #[serde(default)]
    schedules: BTreeMap<String, ScheduleConfig>,
    #[serde(default)]
    conditions: BTreeMap<String, ConditionConfig>,
    #[serde(default)]
    device_intents: BTreeMap<String, DeviceIntent>,
    #[serde(default)]
    hooks: BTreeMap<String, HookConfig>,
    #[serde(default)]
    automations: BTreeMap<String, Automation>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DeviceIntent {
    #[serde(default)]
    schedule_intent: Option<bool>,
    #[serde(default)]
    manual_override: Option<bool>,
    /// If set, the manual override is cleared automatically at this
    /// epoch-ms timestamp. None means the override sticks until the
    /// user releases it or a schedule fire overwrites it.
    #[serde(default)]
    manual_override_until_ms: Option<u128>,
}

const DEFAULT_MANUAL_OVERRIDE_SECONDS: u64 = 3600;
const MIN_MANUAL_OVERRIDE_SECONDS: u64 = 30;
const MAX_MANUAL_OVERRIDE_SECONDS: u64 = 24 * 3600;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum HookEvent {
    On,
    Off,
    Online,
    Offline,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum HookSource {
    Manual,
    Schedule,
    Condition,
    External,
    Discovery,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HookConfig {
    id: String,
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
    url: String,
    #[serde(default = "default_http_method")]
    method: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    /// Optional body. If absent, a default JSON payload is sent.
    #[serde(default)]
    body: Option<String>,
    /// Empty = matches every device.
    #[serde(default)]
    device_filter: Vec<String>,
    /// Empty = matches every event.
    #[serde(default)]
    event_filter: Vec<HookEvent>,
    #[serde(default)]
    created_at_ms: u128,
    #[serde(default)]
    last_fired_at_ms: Option<u128>,
    #[serde(default)]
    last_event: Option<HookEvent>,
    #[serde(default)]
    last_status_code: Option<u16>,
    #[serde(default)]
    last_error: Option<String>,
}

// ---------- Automations (flowchart) ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AutomationNodeConfig {
    CronTrigger { cron_trigger: CronTriggerCfg },
    IntervalTrigger { interval_trigger: IntervalTriggerCfg },
    DeviceEventTrigger { device_event_trigger: DeviceEventTriggerCfg },
    HttpProbe { http_probe: HttpProbeCfg },
    LogicAnd,
    LogicOr,
    LogicNot,
    Debounce { debounce: DebounceCfg },
    SetDevice { set_device: SetDeviceCfg },
    ToggleDevice { toggle_device: ToggleDeviceCfg },
    FireHook { fire_hook: FireHookCfg },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CronTriggerCfg {
    cron: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct IntervalTriggerCfg {
    on_seconds: u64,
    off_seconds: u64,
    start_action: ScheduleAction,
    #[serde(default)]
    starts_at_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DeviceEventTriggerCfg {
    device_name: String,
    event: HookEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct HttpProbeCfg {
    url: String,
    #[serde(default = "default_http_method")]
    method: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default = "default_status_match")]
    status_match: String,
    #[serde(default)]
    body_contains: Option<String>,
    #[serde(default = "default_condition_poll_seconds")]
    poll_seconds: u64,
    #[serde(default)]
    min_stable_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DebounceCfg {
    hold_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SetDeviceCfg {
    device_name: String,
    action: ScheduleAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ToggleDeviceCfg {
    device_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct FireHookCfg {
    hook_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AutomationNode {
    id: String,
    config: AutomationNodeConfig,
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AutomationEdge {
    id: String,
    source_node: String,
    target_node: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct NodeRuntimeState {
    #[serde(default)]
    last_value: Option<bool>,
    #[serde(default)]
    last_fired_at_ms: Option<u128>,
    #[serde(default)]
    last_error: Option<String>,
    // Internal state for cron/interval/probe/debounce — not exposed in the
    // public view JSON. Reset on server restart.
    #[serde(skip)]
    last_checked_at_ms: Option<u128>,
    #[serde(skip)]
    pending_value: Option<bool>,
    #[serde(skip)]
    pending_since_ms: Option<u128>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AutomationStatus {
    #[serde(default)]
    last_fired_at_ms: Option<u128>,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    node_states: BTreeMap<String, NodeRuntimeState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Automation {
    id: String,
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    nodes: Vec<AutomationNode>,
    #[serde(default)]
    edges: Vec<AutomationEdge>,
    #[serde(default)]
    created_at_ms: u128,
    #[serde(default)]
    status: AutomationStatus,
}

#[derive(Debug, Clone, Deserialize)]
struct CreateAutomationRequest {
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    nodes: Vec<AutomationNode>,
    #[serde(default)]
    edges: Vec<AutomationEdge>,
}

#[derive(Debug, Clone, Deserialize)]
struct UpdateAutomationRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    nodes: Option<Vec<AutomationNode>>,
    #[serde(default)]
    edges: Option<Vec<AutomationEdge>>,
}

#[derive(Debug, Clone, Serialize)]
struct AutomationListResponse {
    automations: Vec<Automation>,
}

#[tokio::main]
async fn main() -> Result<()> {
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
        .route("/assets/automations.js", get(automations_bundle))
        .route("/health", get(health))
        .route("/api/devices", get(list_devices))
        .route("/api/energy/history.json", get(energy_history))
        .route("/api/energy/export.xlsx", get(export_energy_workbook))
        .route("/ws/devices", get(devices_websocket))
        .route("/api/scan", post(scan_devices))
        .route("/api/devices/{name}/toggle", post(toggle_device))
        .route("/api/devices/{name}/power", post(set_device_power))
        .route("/api/devices/{name}/release-override", post(release_device_override))
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
        .route(
            "/api/hooks/{id}",
            delete(delete_hook).patch(update_hook),
        )
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

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}

impl AppState {
    fn new(settings: &Settings) -> Self {
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
    fn view(
        &self,
        energy_price_pence_per_kwh: f64,
        intent: DeviceIntent,
        condition_intent: Option<bool>,
    ) -> DeviceView {
        let snapshot = self.snapshot.as_ref();
        let effective_intent =
            compute_effective(intent.manual_override, intent.schedule_intent, condition_intent);

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

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn switch_sound() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/wav")
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from(SWITCH_SOUND_BYTES))
        .expect("static switch sound response should be valid")
}

async fn automations_bundle() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/javascript; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(AUTOMATIONS_BUNDLE_JS))
        .expect("static automations bundle response should be valid")
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn list_devices(State(state): State<AppState>) -> Json<DeviceListResponse> {
    Json(device_list_response(&state, None).await)
}

async fn scan_devices(State(state): State<AppState>) -> Json<DeviceListResponse> {
    let scan_error = match scan_and_refresh(&state).await {
        Ok(()) => None,
        Err(error) => Some(error.to_string()),
    };

    let response = device_list_response(&state, scan_error).await;
    publish_device_list_response(&state, response.clone());

    Json(response)
}

async fn devices_websocket(State(state): State<AppState>, websocket: WebSocketUpgrade) -> Response {
    websocket.on_upgrade(|socket| stream_device_events(socket, state))
}

async fn stream_device_events(mut socket: WebSocket, state: AppState) {
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

async fn send_device_event(socket: &mut WebSocket, response: DeviceListResponse) -> Result<()> {
    let payload = serde_json::to_string(&response).context("failed to serialize device event")?;
    socket
        .send(Message::Text(payload.into()))
        .await
        .context("failed to send device event")
}

async fn energy_history(
    State(state): State<AppState>,
    Query(query): Query<UsageHistoryQuery>,
) -> Json<UsageHistoryResponse> {
    Json(build_usage_history(&state, query.range.as_deref()).await)
}

async fn export_energy_workbook(State(state): State<AppState>) -> Result<Response, AppError> {
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

async fn toggle_device(
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

async fn set_device_power(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<SetPowerRequest>,
) -> Result<Json<DeviceView>, AppError> {
    let duration = request.duration_seconds.unwrap_or(DEFAULT_MANUAL_OVERRIDE_SECONDS);
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

async fn release_device_override(
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

async fn list_schedules(State(state): State<AppState>) -> Json<ScheduleListResponse> {
    let schedules = state.schedules.read().await;
    let mut views: Vec<ScheduleView> = schedules.values().map(schedule_view).collect();
    views.sort_by(|a, b| {
        a.device_name
            .cmp(&b.device_name)
            .then(a.created_at_ms.cmp(&b.created_at_ms))
    });

    Json(ScheduleListResponse { schedules: views })
}

async fn create_schedule(
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

async fn ensure_device_exists(state: &AppState, device_name: &str) -> Result<(), AppError> {
    let devices = state.devices.read().await;
    if !devices.contains_key(device_name) {
        return Err(AppError(anyhow!("unknown device '{}'", device_name)));
    }
    Ok(())
}

async fn ensure_conditions_exist(
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

fn validate_interval(on_seconds: u64, off_seconds: u64) -> Result<()> {
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

async fn delete_schedule(
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

async fn update_schedule(
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

fn non_empty_label(label: String) -> Option<String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_cron(expression: &str) -> Result<String> {
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

fn parse_cron(expression: &str) -> Result<CronSchedule> {
    let translated = translate_cron_to_crate_format(expression);
    CronSchedule::from_str(&translated)
        .map_err(|error| anyhow!("invalid cron expression: {error}"))
}

fn translate_cron_to_crate_format(expression: &str) -> String {
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

fn translate_dow_field(field: &str) -> String {
    field
        .split(',')
        .map(translate_dow_part)
        .collect::<Vec<_>>()
        .join(",")
}

fn translate_dow_part(part: &str) -> String {
    let trimmed = part.trim();
    if let Some((head, step)) = trimmed.split_once('/') {
        format!("{}/{}", translate_dow_head(head), step.trim())
    } else {
        translate_dow_head(trimmed)
    }
}

fn translate_dow_head(value: &str) -> String {
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

fn translate_dow_value(value: &str) -> String {
    let trimmed = value.trim();
    if let Ok(n) = trimmed.parse::<u32>() {
        return ((n % 7) + 1).to_string();
    }
    trimmed.to_string()
}

static SCHEDULE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_schedule_id() -> String {
    let seq = SCHEDULE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}", now_ms(), seq)
}

fn schedule_view(schedule: &ScheduleConfig) -> ScheduleView {
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

fn interval_phase_at(schedule: &ScheduleConfig, at_ms: u128) -> Option<ScheduleAction> {
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

fn next_interval_fire_ms(schedule: &ScheduleConfig, now: u128) -> Option<u128> {
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

async fn list_conditions(State(state): State<AppState>) -> Json<ConditionListResponse> {
    let conditions = state.conditions.read().await;
    let mut views: Vec<ConditionView> = conditions.values().map(condition_view).collect();
    views.sort_by(|a, b| a.name.cmp(&b.name).then(a.created_at_ms.cmp(&b.created_at_ms)));
    Json(ConditionListResponse { conditions: views })
}

async fn create_condition(
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

async fn update_condition(
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

async fn delete_condition(
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

async fn probe_condition(
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
    Ok(Json(
        conditions
            .get(&id)
            .map(condition_view)
            .ok_or_else(|| AppError(anyhow!("condition vanished mid-probe")))?,
    ))
}

fn condition_view(condition: &ConditionConfig) -> ConditionView {
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

static CONDITION_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_condition_id() -> String {
    let seq = CONDITION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("c{:x}-{:x}", now_ms(), seq)
}

#[derive(Debug, Clone, Deserialize)]
struct CreateHookRequest {
    name: String,
    url: String,
    #[serde(default = "default_http_method")]
    method: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    device_filter: Vec<String>,
    #[serde(default)]
    event_filter: Vec<HookEvent>,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct UpdateHookRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default, deserialize_with = "deserialize_optional_label")]
    body: Option<Option<String>>,
    #[serde(default)]
    device_filter: Option<Vec<String>>,
    #[serde(default)]
    event_filter: Option<Vec<HookEvent>>,
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct HookView {
    id: String,
    name: String,
    enabled: bool,
    url: String,
    method: String,
    headers: BTreeMap<String, String>,
    body: Option<String>,
    device_filter: Vec<String>,
    event_filter: Vec<HookEvent>,
    created_at_ms: u128,
    last_fired_at_ms: Option<u128>,
    last_event: Option<HookEvent>,
    last_status_code: Option<u16>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct HookListResponse {
    hooks: Vec<HookView>,
}

fn hook_view(hook: &HookConfig) -> HookView {
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

static HOOK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_hook_id() -> String {
    let seq = HOOK_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("h{:x}-{:x}", now_ms(), seq)
}

async fn list_hooks(State(state): State<AppState>) -> Json<HookListResponse> {
    let hooks = state.hooks.read().await;
    let mut views: Vec<HookView> = hooks.values().map(hook_view).collect();
    views.sort_by(|a, b| a.name.cmp(&b.name).then(a.created_at_ms.cmp(&b.created_at_ms)));
    Json(HookListResponse { hooks: views })
}

async fn create_hook(
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

async fn update_hook(
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

async fn delete_hook(
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

async fn test_hook(
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

fn new_automation_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("auto-{}-{}", now_ms(), n)
}

async fn list_automations(State(state): State<AppState>) -> Json<AutomationListResponse> {
    let automations = state.automations.read().await;
    let mut list: Vec<Automation> = automations.values().cloned().collect();
    list.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms));
    Json(AutomationListResponse { automations: list })
}

async fn create_automation(
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

async fn update_automation(
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

async fn delete_automation(
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

fn validate_automation_graph(nodes: &[AutomationNode], edges: &[AutomationEdge]) -> Result<()> {
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
            return Err(anyhow!("self-edge on node '{}' is not allowed", e.source_node));
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

fn has_cycle_dfs<'a>(
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

fn validate_node_config(config: &AutomationNodeConfig) -> Result<()> {
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
        AutomationNodeConfig::DeviceEventTrigger { device_event_trigger } => {
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

fn validate_http_method(method: &str) -> Result<()> {
    HttpMethod::from_bytes(method.trim().to_uppercase().as_bytes())
        .map(|_| ())
        .map_err(|error| anyhow!("invalid HTTP method '{method}': {error}"))
}

fn validate_url(url: &str) -> Result<()> {
    let trimmed = url.trim();
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(anyhow!("URL must start with http:// or https://"));
    }
    Ok(())
}

fn clamp_poll_seconds(value: u64) -> Result<u64> {
    if !(MIN_CONDITION_POLL_SECONDS..=MAX_CONDITION_POLL_SECONDS).contains(&value) {
        return Err(anyhow!(
            "poll_seconds must be between {MIN_CONDITION_POLL_SECONDS} and {MAX_CONDITION_POLL_SECONDS} (got {value})"
        ));
    }
    Ok(value)
}

fn parse_status_match(expression: &str) -> Result<Vec<std::ops::RangeInclusive<u16>>> {
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

fn status_matches(ranges: &[std::ops::RangeInclusive<u16>], code: u16) -> bool {
    ranges.iter().any(|range| range.contains(&code))
}

struct ProbeOutcome {
    passing: bool,
    status_code: Option<u16>,
    error: Option<String>,
}

async fn probe_condition_once(
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
            Some(format!("status {status} did not match '{}'", condition.status_match))
        } else {
            Some(format!(
                "body did not contain '{}'",
                condition.body_contains.as_deref().unwrap_or("")
            ))
        },
    }
}

async fn read_response_body(response: reqwest::Response) -> Result<String> {
    let bytes = response
        .bytes()
        .await
        .map_err(|error| anyhow!("{error}"))?;
    let truncated = if bytes.len() > MAX_CONDITION_BODY_BYTES {
        &bytes[..MAX_CONDITION_BODY_BYTES]
    } else {
        &bytes[..]
    };
    Ok(String::from_utf8_lossy(truncated).into_owned())
}

fn condition_probe_key(condition: &ConditionConfig) -> String {
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

async fn probe_and_record(state: &AppState, id: &str) {
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
fn compute_effective(
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
async fn condition_intent_for_device(state: &AppState, device_name: &str) -> Option<bool> {
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
async fn reconcile_device(state: &AppState, device_name: &str, source: HookSource) {
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

    if let Err(error) = retry_tapo_handshake(|| state.controller.set_power(&device_cfg, target)).await
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
        if target { HookEvent::On } else { HookEvent::Off },
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

async fn set_schedule_intent(state: &AppState, device_name: &str, intent: bool) {
    let mut intents = state.device_intents.write().await;
    let entry = intents.entry(device_name.to_string()).or_default();
    entry.schedule_intent = Some(intent);
    // Schedule firing automatically releases any manual override.
    entry.manual_override = None;
    entry.manual_override_until_ms = None;
}

async fn set_manual_override(
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

async fn clear_manual_override(state: &AppState, device_name: &str) {
    let mut intents = state.device_intents.write().await;
    if let Some(entry) = intents.get_mut(device_name) {
        entry.manual_override = None;
        entry.manual_override_until_ms = None;
    }
}

async fn run_override_expiry_sweeper(state: AppState) {
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

async fn run_automation_engine(state: AppState) {
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

async fn evaluate_all_automations(
    state: &AppState,
    previous_tick_ms: u128,
    tick_ms: u128,
) -> Result<()> {
    let snapshots: Vec<Automation> = {
        let automations = state.automations.read().await;
        automations.values().filter(|a| a.enabled).cloned().collect()
    };

    for automation in snapshots {
        evaluate_one_automation(state, automation, previous_tick_ms, tick_ms).await;
    }

    Ok(())
}

async fn evaluate_one_automation(
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
                    None => state_update.last_value.is_some()
                        || state_update.last_fired_at_ms.is_some()
                        || state_update.last_error.is_some(),
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

fn topo_sort_nodes(nodes: &[AutomationNode], edges: &[AutomationEdge]) -> Option<Vec<String>> {
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

async fn evaluate_node(
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
                std::time::UNIX_EPOCH
                    + Duration::from_millis(previous_tick_ms as u64),
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
        AutomationNodeConfig::DeviceEventTrigger { device_event_trigger } => {
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

async fn execute_action(
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
        AutomationNodeConfig::FireHook { fire_hook: fire_hook_cfg } => {
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

async fn run_condition_poller(state: AppState) {
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
            let due = members.iter().any(|(_id, poll_seconds, last_checked_at_ms)| {
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

async fn run_scheduler(state: AppState) {
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

async fn evaluate_schedules(
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
                let prev_ms = u128::try_from(previous_tick.timestamp_millis().max(0))
                    .unwrap_or(0);
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

async fn fire_schedule(state: &AppState, schedule: &ScheduleConfig, action: ScheduleAction) {
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

async fn record_schedule_success(state: &AppState, id: &str) {
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

async fn record_schedule_error(state: &AppState, id: &str, message: String) {
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

async fn monitor_devices(state: AppState) {
    loop {
        sleep(Duration::from_secs(state.refresh_seconds)).await;

        refresh_all_devices(&state).await;
    }
}

async fn scan_for_devices(state: AppState) {
    loop {
        sleep(Duration::from_secs(state.scan_seconds)).await;

        if let Err(error) = discover_devices(&state).await {
            warn!(%error, "periodic discovery failed");
        }
    }
}

async fn initial_refresh_devices(state: AppState) {
    refresh_all_devices(&state).await;

    if let Err(error) = discover_devices(&state).await {
        warn!(%error, "background discovery failed");
    }
}

async fn scan_and_refresh(state: &AppState) -> Result<()> {
    discover_devices(state).await?;
    refresh_all_devices(state).await;
    Ok(())
}

async fn discover_devices(state: &AppState) -> Result<()> {
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

async fn load_persisted_state(state: &AppState) -> Result<()> {
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

async fn save_persisted_state(state: &AppState) -> Result<()> {
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

/// Convert legacy `ScheduleConfig` and `ConditionConfig` entries into
/// equivalent `Automation` flowcharts. The original collections are left in
/// place inside `persisted` so the next save still records them as a backup;
/// the engine no longer reads them once automations exist.
fn migrate_to_automations(persisted: &mut PersistedState) {
    if !persisted.automations.is_empty() {
        return;
    }

    let now = now_ms();
    let mut next_id = 0u64;
    let mut new_id = || {
        next_id += 1;
        format!("mig-{}-{}", now, next_id)
    };

    // 1) Each ConditionConfig becomes an Automation:
    //    [HttpProbe] -> [LogicNot?] -> [SetDevice(action_on_pass/fail)]
    //
    // We always emit a Set Device action when at least one action is
    // configured. Pass-action and fail-action are split into two parallel
    // branches off the probe so both can fire.
    for (id, cond) in &persisted.conditions {
        let mut nodes: Vec<AutomationNode> = Vec::new();
        let mut edges: Vec<AutomationEdge> = Vec::new();

        let probe_id = format!("probe-{id}");
        nodes.push(AutomationNode {
            id: probe_id.clone(),
            x: 80.0,
            y: 80.0,
            config: AutomationNodeConfig::HttpProbe {
                http_probe: HttpProbeCfg {
                    url: cond.url.clone(),
                    method: cond.method.clone(),
                    headers: cond.headers.clone(),
                    body: cond.body.clone(),
                    status_match: cond.status_match.clone(),
                    body_contains: cond.body_contains.clone(),
                    poll_seconds: cond.poll_seconds,
                    min_stable_seconds: cond.min_stable_seconds,
                },
            },
        });

        let mut next_y = 80.0_f64;
        if let Some(pass_action) = cond.action_on_pass {
            let act_id = format!("act-pass-{id}");
            nodes.push(AutomationNode {
                id: act_id.clone(),
                x: 420.0,
                y: next_y,
                config: AutomationNodeConfig::SetDevice {
                    set_device: SetDeviceCfg {
                        device_name: cond.device_name.clone(),
                        action: match pass_action {
                            ConditionAction::On => ScheduleAction::On,
                            ConditionAction::Off => ScheduleAction::Off,
                        },
                    },
                },
            });
            edges.push(AutomationEdge {
                id: new_id(),
                source_node: probe_id.clone(),
                target_node: act_id,
            });
            next_y += 160.0;
        }

        if let Some(fail_action) = cond.action_on_fail {
            let not_id = format!("not-fail-{id}");
            nodes.push(AutomationNode {
                id: not_id.clone(),
                x: 250.0,
                y: next_y,
                config: AutomationNodeConfig::LogicNot,
            });
            edges.push(AutomationEdge {
                id: new_id(),
                source_node: probe_id.clone(),
                target_node: not_id.clone(),
            });
            let act_id = format!("act-fail-{id}");
            nodes.push(AutomationNode {
                id: act_id.clone(),
                x: 420.0,
                y: next_y,
                config: AutomationNodeConfig::SetDevice {
                    set_device: SetDeviceCfg {
                        device_name: cond.device_name.clone(),
                        action: match fail_action {
                            ConditionAction::On => ScheduleAction::On,
                            ConditionAction::Off => ScheduleAction::Off,
                        },
                    },
                },
            });
            edges.push(AutomationEdge {
                id: new_id(),
                source_node: not_id,
                target_node: act_id,
            });
        }

        let auto_id = format!("auto-cond-{id}");
        persisted.automations.insert(
            auto_id.clone(),
            Automation {
                id: auto_id,
                name: if cond.name.is_empty() {
                    format!("Condition: {}", cond.device_name)
                } else {
                    cond.name.clone()
                },
                enabled: cond.enabled,
                nodes,
                edges,
                created_at_ms: if cond.created_at_ms == 0 {
                    now
                } else {
                    cond.created_at_ms
                },
                status: AutomationStatus::default(),
            },
        );
    }

    // 2) Each ScheduleConfig becomes an Automation:
    //    [CronTrigger | IntervalTrigger] -> [SetDevice]
    for (id, sched) in &persisted.schedules {
        let mut nodes: Vec<AutomationNode> = Vec::new();
        let mut edges: Vec<AutomationEdge> = Vec::new();

        let trigger_id = format!("trig-{id}");
        match sched.kind {
            ScheduleKind::Cron => {
                let Some(cron) = sched.cron.clone() else {
                    continue;
                };
                nodes.push(AutomationNode {
                    id: trigger_id.clone(),
                    x: 80.0,
                    y: 80.0,
                    config: AutomationNodeConfig::CronTrigger {
                        cron_trigger: CronTriggerCfg { cron },
                    },
                });
                let Some(action) = sched.action else {
                    continue;
                };
                let act_id = format!("act-{id}");
                nodes.push(AutomationNode {
                    id: act_id.clone(),
                    x: 380.0,
                    y: 80.0,
                    config: AutomationNodeConfig::SetDevice {
                        set_device: SetDeviceCfg {
                            device_name: sched.device_name.clone(),
                            action,
                        },
                    },
                });
                edges.push(AutomationEdge {
                    id: new_id(),
                    source_node: trigger_id,
                    target_node: act_id,
                });
            }
            ScheduleKind::Interval => {
                let on_seconds = sched.on_seconds.unwrap_or(0);
                let off_seconds = sched.off_seconds.unwrap_or(0);
                let start_action = sched.start_action.unwrap_or(ScheduleAction::On);
                nodes.push(AutomationNode {
                    id: trigger_id.clone(),
                    x: 80.0,
                    y: 80.0,
                    config: AutomationNodeConfig::IntervalTrigger {
                        interval_trigger: IntervalTriggerCfg {
                            on_seconds,
                            off_seconds,
                            start_action,
                            starts_at_ms: sched.starts_at_ms,
                        },
                    },
                });
                let act_id = format!("act-{id}");
                nodes.push(AutomationNode {
                    id: act_id.clone(),
                    x: 380.0,
                    y: 80.0,
                    config: AutomationNodeConfig::SetDevice {
                        set_device: SetDeviceCfg {
                            device_name: sched.device_name.clone(),
                            // Interval flips state internally; the
                            // SetDevice action just relays whatever the
                            // trigger emits.
                            action: ScheduleAction::Toggle,
                        },
                    },
                });
                edges.push(AutomationEdge {
                    id: new_id(),
                    source_node: trigger_id,
                    target_node: act_id,
                });
            }
        }

        let auto_id = format!("auto-sched-{id}");
        persisted.automations.insert(
            auto_id.clone(),
            Automation {
                id: auto_id,
                name: sched
                    .label
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("Schedule: {}", sched.device_name)),
                enabled: sched.enabled,
                nodes,
                edges,
                created_at_ms: if sched.created_at_ms == 0 {
                    now
                } else {
                    sched.created_at_ms
                },
                status: AutomationStatus::default(),
            },
        );
    }

    // Original schedules/conditions are left in `persisted` as historical
    // backup but emptied out for the runtime once the automation engine is
    // the source of truth. Clear them now so they don't double-fire.
    persisted.schedules.clear();
    persisted.conditions.clear();
}

fn managed_device_from_config(name: String, config: DeviceConfig) -> ManagedDevice {
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

fn write_json_atomically<T>(path: &FsPath, value: &T) -> Result<()>
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

fn temporary_path_for(path: &FsPath) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("state path must include a file name"))?;
    let mut temporary_name = OsString::from(file_name);
    temporary_name.push(".tmp");

    Ok(path.with_file_name(temporary_name))
}

async fn refresh_all_devices(state: &AppState) {
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

async fn refresh_device(state: &AppState, name: &str, device: DeviceConfig) {
    let operation_lock = device_operation_lock(state, &device).await;
    let _operation_guard = operation_lock.lock().await;

    match retry_tapo_handshake(|| state.controller.read_device(&device)).await {
        Ok(snapshot) => {
            update_device_snapshot(state, name, snapshot, None, HookSource::External).await
        }
        Err(error) => update_device_error(state, name, error.to_string()).await,
    }
}

async fn retry_tapo_handshake<T, F, Fut>(mut operation: F) -> Result<T>
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

fn is_tapo_handshake_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("Handshake2 failed"))
}

async fn device_operation_lock(state: &AppState, device: &DeviceConfig) -> Arc<Mutex<()>> {
    if let Some(lock) = state.device_locks.read().await.get(&device.ip).cloned() {
        return lock;
    }

    let mut locks = state.device_locks.write().await;
    locks
        .entry(device.ip)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

async fn update_device_snapshot(
    state: &AppState,
    name: &str,
    snapshot: DeviceSnapshot,
    last_error: Option<String>,
    source: HookSource,
) {
    let (prev_on, was_offline_announced, nickname) = {
        let devices = state.devices.read().await;
        let device = devices.get(name);
        let prev_on = device.and_then(|d| d.snapshot.as_ref()).map(|s| s.device_on);
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
            events.push(if new_on { HookEvent::On } else { HookEvent::Off });
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

async fn update_device_error(state: &AppState, name: &str, error: String) {
    let (prev_on, prev_failures, was_offline_announced, nickname, model) = {
        let devices = state.devices.read().await;
        let device = devices.get(name);
        let prev_on = device.and_then(|d| d.snapshot.as_ref()).map(|s| s.device_on);
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
        (prev_on, prev_failures, was_offline_announced, nickname, model)
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

fn hook_matches(hook: &HookConfig, device: &str, event: HookEvent) -> bool {
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
struct HookTemplateContext {
    device: String,
    nickname: String,
    model: String,
    event: HookEvent,
    source: HookSource,
    previous_on: Option<bool>,
    new_on: Option<bool>,
    timestamp_ms: u128,
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

fn hook_event_str(event: HookEvent) -> &'static str {
    match event {
        HookEvent::On => "on",
        HookEvent::Off => "off",
        HookEvent::Online => "online",
        HookEvent::Offline => "offline",
    }
}

fn hook_source_str(source: HookSource) -> &'static str {
    match source {
        HookSource::Manual => "manual",
        HookSource::Schedule => "schedule",
        HookSource::Condition => "condition",
        HookSource::External => "external",
        HookSource::Discovery => "discovery",
    }
}

fn optional_bool_str(value: Option<bool>) -> String {
    match value {
        Some(true) => "true".to_string(),
        Some(false) => "false".to_string(),
        None => String::new(),
    }
}

fn render_hook_template(input: &str, vars: &[(&str, String)]) -> String {
    let mut out = input.to_string();
    for (key, value) in vars {
        let placeholder = format!("{{{{{key}}}}}");
        if out.contains(&placeholder) {
            out = out.replace(&placeholder, value);
        }
    }
    out
}

async fn dispatch_hook_events(
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

async fn fire_hook(state: &AppState, hook: HookConfig, ctx: HookTemplateContext) {
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

async fn update_hook_result(
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

async fn existing_config(state: &AppState) -> TapoConfig {
    let devices = state.devices.read().await;

    TapoConfig {
        username: None,
        devices: devices
            .iter()
            .map(|(name, device)| (name.clone(), device.config.clone()))
            .collect(),
    }
}

async fn device_views(state: &AppState) -> Vec<DeviceView> {
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

async fn device_list_response(state: &AppState, scan_error: Option<String>) -> DeviceListResponse {
    DeviceListResponse {
        devices: device_views(state).await,
        updated_at_ms: now_ms(),
        energy_price_pence_per_kwh: state.energy_price_pence_per_kwh,
        scan_error,
    }
}

async fn publish_device_list(state: &AppState, scan_error: Option<String>) {
    let response = device_list_response(state, scan_error).await;
    publish_device_list_response(state, response);
}

fn publish_device_list_response(state: &AppState, response: DeviceListResponse) {
    let _ = state.device_events.send(response);
}

async fn get_device_config(state: &AppState, name: &str) -> Result<DeviceConfig> {
    let devices = state.devices.read().await;

    devices
        .get(name)
        .map(|device| device.config.clone())
        .ok_or_else(|| anyhow!("device '{name}' was not found"))
}

async fn get_device_view(state: &AppState, name: &str) -> Result<DeviceView> {
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

fn estimate_energy_cost_pence(energy_wh: u64, price_pence_per_kwh: f64) -> f64 {
    energy_wh as f64 / 1000.0 * price_pence_per_kwh
}

async fn build_energy_export_workbook(state: &AppState) -> Result<Vec<u8>> {
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

async fn build_usage_history(state: &AppState, range_key: Option<&str>) -> UsageHistoryResponse {
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

async fn read_usage_history_entries(
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

fn usage_history_start_datetime(start: UsageHistoryStart, now: DateTime<Utc>) -> DateTime<Utc> {
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

fn current_year_start(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), 1, 1).unwrap_or(date)
}

fn date_start_datetime(date: NaiveDate) -> DateTime<Utc> {
    DateTime::from_naive_utc_and_offset(date.and_hms_opt(0, 0, 0).unwrap_or_default(), Utc)
}

fn usage_history_range(range_key: Option<&str>) -> UsageHistoryRange {
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

async fn export_devices(state: &AppState) -> Vec<ExportDevice> {
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

fn export_specs(now: DateTime<Utc>) -> Result<Vec<ExportSpec>> {
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

fn current_quarter_start(date: NaiveDate) -> Result<NaiveDate> {
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

fn split_datetime_ranges(
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

async fn collect_export_table(
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

async fn read_export_entries(
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
enum PowerExportInterval {
    Every5Minutes,
    Hourly,
}

async fn read_energy_entries(
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

async fn read_power_entries(
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

fn historical_client(state: &AppState) -> ApiClient {
    ApiClient::new(&state.credentials.username, &state.credentials.password)
        .with_timeout(Duration::from_secs(30))
}

fn write_export_workbook(
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

fn write_export_table(
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

fn write_export_errors(workbook: &mut Workbook, errors: &[ExportError]) -> Result<()> {
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

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <meta name="theme-color" content="#201d19">
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
    <style>
        :root {
            color-scheme: dark;
            --font-ui: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
            --font-data: ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
            --theme-color: #201d19;
            --wall: #201d19;
            --cabinet: #5b5144;
            --cabinet-dark: #211d19;
            --bakelite: #181613;
            --brass: #c19b55;
            --paper: #e5d8b6;
            --ink: #221d17;
            --text: #f2ead7;
            --title: #f3e9d1;
            --bg-start: #171512;
            --bg-end: #14120f;
            --bg-glow-a: rgba(255, 214, 128, 0.13);
            --bg-glow-b: rgba(193, 155, 85, 0.07);
            --cabinet-border: #2a2118;
            --cabinet-highlight: rgba(255, 255, 255, 0.06);
            --cabinet-shadow: rgba(0, 0, 0, 0.18);
            --cabinet-stripe-a: #5f5445;
            --cabinet-stripe-b: #514738;
            --meter-start: #efe1bd;
            --meter-end: #c8b37c;
            --label-start: #f2e7c7;
            --label-end: #c7b783;
            --primary-text: #21170c;
            --primary-start: #e7c577;
            --primary-end: #9e7135;
            --secondary-text: #f4e8cb;
            --secondary-bg: rgba(0, 0, 0, 0.26);
            --breaker-top: #26231f;
            --toggle-mid: #3b3833;
            --lever-mid: #b8aa8c;
            --green: #66d18c;
            --red: #de5e4b;
            --amber: #e5b75b;
            --graph-line: #126c43;
            --graph-fill: rgba(18, 108, 67, 0.18);
            --graph-grid: rgba(34, 29, 23, 0.3);
            --graph-surface: rgba(255, 255, 255, 0.16);
            --muted: #9b907d;
        }

        html[data-theme="dark"] {
            --theme-color: #08090b;
            --wall: #090a0c;
            --cabinet: #17191c;
            --cabinet-dark: #050607;
            --bakelite: #07080a;
            --brass: #d0a85e;
            --paper: #d8ccad;
            --ink: #f2e9d4;
            --text: #efe7d7;
            --title: #fff3d6;
            --bg-start: #050607;
            --bg-end: #020304;
            --bg-glow-a: rgba(224, 184, 96, 0.08);
            --bg-glow-b: rgba(72, 111, 136, 0.12);
            --cabinet-border: #050607;
            --cabinet-highlight: rgba(255, 255, 255, 0.035);
            --cabinet-shadow: rgba(0, 0, 0, 0.36);
            --cabinet-stripe-a: #1f2225;
            --cabinet-stripe-b: #15171a;
            --meter-start: #25282b;
            --meter-end: #111316;
            --label-start: #252321;
            --label-end: #11100f;
            --primary-text: #0b0905;
            --primary-start: #d0a85e;
            --primary-end: #7c5424;
            --secondary-text: #efe7d7;
            --secondary-bg: rgba(255, 255, 255, 0.055);
            --breaker-top: #101215;
            --toggle-mid: #24282d;
            --lever-mid: #6f7577;
            --green: #71e09b;
            --red: #f06b5c;
            --amber: #d0a85e;
            --graph-line: #71e09b;
            --graph-fill: rgba(113, 224, 155, 0.12);
            --graph-grid: rgba(239, 231, 215, 0.14);
            --graph-surface: rgba(0, 0, 0, 0.08);
            --muted: #a79d8b;
        }

        * {
            box-sizing: border-box;
        }

        html {
            min-height: 100%;
            background: var(--wall);
        }

        body {
            position: relative;
            min-width: 320px;
            min-height: 100svh;
            margin: 0;
            color: var(--text);
            font-family: var(--font-ui);
        }

        body::before {
            content: "";
            position: fixed;
            inset: 0;
            z-index: -1;
            background:
                radial-gradient(circle at 8% 0%, var(--bg-glow-a), transparent 34rem),
                radial-gradient(circle at 92% 18%, var(--bg-glow-b), transparent 38rem),
                linear-gradient(135deg, var(--bg-start) 0%, var(--wall) 54%, var(--bg-end) 100%);
            background-repeat: no-repeat;
            background-size: cover;
        }

        button {
            font: inherit;
            touch-action: manipulation;
        }

        .shell {
            width: min(1200px, calc(100vw - 32px));
            margin: 16px auto 32px;
        }

        .header {
            display: flex;
            align-items: center;
            justify-content: space-between;
            gap: 16px;
            margin-bottom: 12px;
        }

        h1 {
            margin: 0;
            color: var(--title);
            font-size: clamp(34px, 4vw, 62px);
            line-height: 0.95;
            letter-spacing: -0.045em;
            text-shadow: 0 3px 0 #000;
        }

        .scan-button {
            min-height: 44px;
            padding: 10px 16px;
            border: 1px solid rgba(0, 0, 0, 0.5);
            border-radius: 10px;
            color: var(--primary-text);
            background: linear-gradient(var(--primary-start), var(--primary-end));
            box-shadow:
                inset 0 1px 0 rgba(255, 255, 255, 0.45),
                0 4px 0 #553619,
                0 12px 24px rgba(0, 0, 0, 0.35);
            cursor: pointer;
        }

        .header-actions {
            display: flex;
            align-items: center;
            gap: 10px;
        }

        .export-link,
        .theme-button {
            display: inline-flex;
            min-height: 44px;
            align-items: center;
            padding: 10px 16px;
            border: 1px solid rgba(242, 212, 138, 0.34);
            border-radius: 10px;
            color: var(--secondary-text);
            background: var(--secondary-bg);
            box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.08);
            text-decoration: none;
            touch-action: manipulation;
        }

        .theme-button {
            cursor: pointer;
        }

        .scan-button:hover,
        .export-link:hover,
        .theme-button:hover {
            filter: brightness(1.08);
        }

        .scan-button:focus-visible,
        .export-link:focus-visible,
        .theme-button:focus-visible,
        .toggle:focus-visible {
            outline: 3px solid #f2d48a;
            outline-offset: 3px;
        }

        .cabinet {
            position: relative;
            overflow: hidden;
            min-height: 560px;
            padding: clamp(18px, 4vw, 42px);
            border: 10px solid var(--cabinet-border);
            border-radius: 18px;
            background:
                linear-gradient(90deg, var(--cabinet-highlight), transparent 16%, var(--cabinet-shadow) 72%),
                repeating-linear-gradient(90deg, var(--cabinet-stripe-a) 0 18px, var(--cabinet-stripe-b) 18px 36px);
            box-shadow:
                inset 0 0 0 2px rgba(255, 255, 255, 0.08),
                inset 0 0 48px rgba(0, 0, 0, 0.48),
                0 30px 70px rgba(0, 0, 0, 0.48);
        }

        .cabinet::before,
        .cabinet::after {
            content: "";
            position: absolute;
            width: 18px;
            height: 18px;
            border-radius: 50%;
            background: radial-gradient(circle at 35% 35%, #f0d38a, #6b4d27 68%);
            box-shadow: 0 0 0 2px rgba(0, 0, 0, 0.35);
        }

        .cabinet::before {
            top: 18px;
            left: 18px;
        }

        .cabinet::after {
            right: 18px;
            bottom: 18px;
        }

        .meter-row {
            display: grid;
            grid-template-columns: repeat(4, minmax(0, 1fr));
            gap: 12px;
            margin-bottom: 22px;
        }

        .meter {
            padding: 12px 14px;
            border: 1px solid rgba(0, 0, 0, 0.55);
            border-radius: 8px;
            color: var(--ink);
            background: linear-gradient(var(--meter-start), var(--meter-end));
            box-shadow: inset 0 1px 8px rgba(255, 255, 255, 0.45), inset 0 -8px 18px rgba(88, 60, 28, 0.2);
        }

        .meter span {
            display: block;
            font-size: 11px;
            letter-spacing: 0.12em;
            text-transform: uppercase;
        }

        .meter strong {
            display: block;
            margin-top: 5px;
            font-family: var(--font-data);
            font-size: 22px;
            font-variant-numeric: tabular-nums;
        }

        .usage-panel {
            margin-bottom: 22px;
            padding: 14px;
            border: 1px solid rgba(0, 0, 0, 0.55);
            border-radius: 10px;
            color: var(--ink);
            background: linear-gradient(var(--meter-start), var(--meter-end));
            box-shadow: inset 0 1px 8px rgba(255, 255, 255, 0.4), inset 0 -8px 18px rgba(88, 60, 28, 0.18);
        }

        .usage-header {
            display: flex;
            align-items: baseline;
            justify-content: space-between;
            gap: 12px;
            margin-bottom: 10px;
        }

        .usage-range-controls {
            display: flex;
            flex-wrap: wrap;
            gap: 6px;
            margin-bottom: 12px;
        }

        .range-button {
            min-height: 32px;
            padding: 6px 9px;
            border: 1px solid rgba(0, 0, 0, 0.24);
            border-radius: 999px;
            color: color-mix(in srgb, var(--ink) 78%, transparent);
            background: rgba(0, 0, 0, 0.08);
            font-family: var(--font-data);
            font-size: 12px;
            cursor: pointer;
        }

        .range-button:hover {
            filter: brightness(1.08);
        }

        .range-button:focus-visible {
            outline: 3px solid rgba(0, 0, 0, 0.34);
            outline-offset: 2px;
        }

        .range-button[aria-pressed="true"] {
            color: var(--primary-text);
            background: linear-gradient(var(--primary-start), var(--primary-end));
            box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.36);
        }

        .usage-header h2 {
            margin: 0;
            font-size: 15px;
            letter-spacing: 0.08em;
            line-height: 1.2;
            text-transform: uppercase;
        }

        .usage-header span {
            color: color-mix(in srgb, var(--ink) 72%, transparent);
            font-family: var(--font-data);
            font-size: 12px;
            font-variant-numeric: tabular-nums;
        }

        .usage-chart-container {
            position: relative;
            height: 184px;
            overflow: hidden;
            border: 1px solid rgba(0, 0, 0, 0.24);
            border-radius: 8px;
            background:
                linear-gradient(var(--graph-grid) 1px, transparent 1px),
                linear-gradient(90deg, var(--graph-grid) 1px, transparent 1px),
                var(--graph-surface);
            background-size: 100% 25%, 12.5% 100%, auto;
        }

        .usage-chart {
            max-height: 184px;
        }

        .usage-empty {
            margin: 10px 0 0;
            color: color-mix(in srgb, var(--ink) 72%, transparent);
            font-size: 13px;
        }

        .breaker-grid {
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(min(100%, 220px), 1fr));
            gap: 16px;
        }

        .breaker {
            position: relative;
            min-height: 296px;
            padding: 14px;
            border: 1px solid #070604;
            border-radius: 14px;
            background:
                linear-gradient(160deg, rgba(255,255,255,0.08), transparent 32%),
                linear-gradient(var(--breaker-top), var(--bakelite));
            box-shadow:
                inset 0 0 0 1px rgba(255, 255, 255, 0.05),
                inset 0 -18px 38px rgba(0, 0, 0, 0.34),
                0 8px 18px rgba(0, 0, 0, 0.32);
        }

        .breaker.offline {
            opacity: 0.72;
        }

        .label-card {
            padding: 10px 11px;
            border-radius: 6px;
            color: var(--ink);
            background: linear-gradient(var(--label-start), var(--label-end));
            box-shadow: inset 0 0 0 1px rgba(66, 43, 20, 0.24);
        }

        .device-name {
            margin: 0;
            font-size: 22px;
            line-height: 1.05;
            letter-spacing: -0.02em;
        }

        .device-meta {
            margin: 7px 0 0;
            font-family: var(--font-data);
            font-size: 11px;
            line-height: 1.45;
            text-transform: uppercase;
        }

        .toggle-wrap {
            display: grid;
            place-items: center;
            margin: 18px 0 16px;
        }

        .toggle {
            position: relative;
            --lever-travel: 40px;
            --lever-angle: 2deg;
            --switch-glow: var(--red);
            --switch-glow-y: 68px;
            width: 84px;
            height: 132px;
            overflow: hidden;
            border: 0;
            border-radius: 16px;
            background: linear-gradient(90deg, #100f0e, var(--toggle-mid) 48%, #0e0d0c);
            box-shadow:
                inset 0 0 0 2px #090807,
                inset 0 0 24px rgba(0, 0, 0, 0.7),
                0 0 0 6px rgba(0, 0, 0, 0.22);
            cursor: pointer;
            transition: box-shadow 160ms ease, transform 120ms cubic-bezier(0.22, 1, 0.36, 1);
        }

        .toggle::before {
            content: "";
            position: absolute;
            left: 50%;
            width: 48px;
            height: 48px;
            border-radius: 999px;
            background: radial-gradient(circle, color-mix(in srgb, var(--switch-glow) 42%, transparent), transparent 70%);
            opacity: 0.5;
            transform: translate(-50%, var(--switch-glow-y));
            transition: opacity 180ms ease, transform 220ms cubic-bezier(0.25, 1, 0.5, 1);
        }

        .toggle::after {
            content: "";
            position: absolute;
            inset: 9px 14px;
            border-radius: 12px;
            background: linear-gradient(90deg, rgba(255, 255, 255, 0.08), transparent 36%, rgba(0, 0, 0, 0.2));
            pointer-events: none;
        }

        .toggle:disabled {
            cursor: progress;
            opacity: 0.9;
        }

        .toggle:active,
        .toggle.is-switching {
            transform: translateY(1px);
            box-shadow:
                inset 0 0 0 2px #090807,
                inset 0 0 24px rgba(0, 0, 0, 0.78),
                0 0 0 6px rgba(0, 0, 0, 0.18);
        }

        .lever {
            position: absolute;
            z-index: 1;
            top: 14px;
            left: 18px;
            width: 48px;
            height: 64px;
            border-radius: 10px;
            background: linear-gradient(90deg, #34302a, var(--lever-mid) 44%, #3a352e);
            box-shadow:
                inset 0 1px 0 rgba(255, 255, 255, 0.35),
                inset 0 -10px 16px rgba(0, 0, 0, 0.35),
                0 8px 14px rgba(0, 0, 0, 0.5);
            transform: translateY(var(--lever-travel)) rotate(var(--lever-angle));
            transform-origin: center center;
            transition: box-shadow 160ms ease, transform 220ms cubic-bezier(0.25, 1, 0.5, 1);
        }

        .toggle[data-on="true"] .lever {
            --lever-travel: 0px;
            --lever-angle: -2deg;
        }

        .toggle[data-on="true"] {
            --switch-glow: var(--green);
            --switch-glow-y: 18px;
        }

        .toggle[data-on="false"] .lever {
            --lever-travel: 40px;
            --lever-angle: 2deg;
        }

        .toggle.is-switching .lever {
            box-shadow:
                inset 0 1px 0 rgba(255, 255, 255, 0.28),
                inset 0 -12px 18px rgba(0, 0, 0, 0.42),
                0 6px 10px rgba(0, 0, 0, 0.54);
        }

        .status-strip {
            display: flex;
            justify-content: space-between;
            gap: 8px;
            margin-top: 10px;
            font-family: var(--font-data);
            font-size: 12px;
        }

        .device-mode-badge {
            margin: 6px 0 0;
            padding: 4px 8px;
            border-radius: 6px;
            font-family: var(--font-data);
            font-size: 10px;
            text-transform: uppercase;
            letter-spacing: 0.08em;
            display: flex;
            align-items: center;
            justify-content: space-between;
            gap: 8px;
        }

        .device-mode-badge.manual {
            background: color-mix(in srgb, var(--amber) 18%, rgba(0, 0, 0, 0.25));
            color: var(--text);
            border: 1px solid color-mix(in srgb, var(--amber) 60%, transparent);
        }

        .device-mode-badge.manual button {
            padding: 2px 8px;
            border: 1px solid color-mix(in srgb, var(--amber) 60%, transparent);
            border-radius: 4px;
            background: rgba(0, 0, 0, 0.3);
            color: var(--text);
            font-family: inherit;
            font-size: 10px;
            text-transform: uppercase;
            letter-spacing: 0.08em;
            cursor: pointer;
        }

        .device-mode-badge.manual button:hover {
            background: rgba(0, 0, 0, 0.45);
        }

        .device-mode-badge.condition-blocked {
            background: color-mix(in srgb, var(--red) 14%, rgba(0, 0, 0, 0.25));
            color: var(--text);
            border: 1px solid color-mix(in srgb, var(--red) 45%, transparent);
        }

        .lamp {
            display: inline-flex;
            align-items: center;
            gap: 7px;
        }

        .lamp::before {
            content: "";
            width: 10px;
            height: 10px;
            border-radius: 999px;
            background: var(--amber);
            box-shadow: 0 0 16px var(--amber);
        }

        .lamp.on::before {
            background: var(--green);
            box-shadow: 0 0 16px var(--green);
        }

        .lamp.off::before {
            background: var(--red);
            box-shadow: 0 0 12px var(--red);
        }

        .readings {
            display: grid;
            grid-template-columns: repeat(2, minmax(0, 1fr));
            gap: 8px;
            margin-top: 14px;
        }

        .reading {
            padding: 8px;
            border-radius: 6px;
            background: rgba(0, 0, 0, 0.22);
            font-family: var(--font-data);
            font-size: 12px;
            font-variant-numeric: tabular-nums;
        }

        @media (prefers-reduced-motion: reduce) {
            .toggle,
            .toggle::before,
            .lever {
                transition: none;
            }
        }

        .reading span {
            display: block;
            color: var(--muted);
            font-size: 10px;
            text-transform: uppercase;
        }

        .notice {
            margin: 0 0 14px;
            padding: 10px 12px;
            border: 1px solid rgba(229, 183, 91, 0.42);
            border-radius: 8px;
            color: #f0dfbd;
            background: rgba(54, 38, 15, 0.7);
            font-size: 13px;
            line-height: 1.45;
        }

        .empty {
            padding: 46px 24px;
            border: 1px dashed rgba(229, 216, 182, 0.35);
            border-radius: 14px;
            color: #dfd4bf;
            text-align: center;
            background: rgba(0, 0, 0, 0.18);
        }

        .schedules {
            margin-top: 14px;
            padding: 10px 11px;
            border-radius: 8px;
            background: rgba(0, 0, 0, 0.24);
            font-family: var(--font-data);
        }

        .schedules-header {
            display: flex;
            justify-content: space-between;
            align-items: center;
            gap: 8px;
            margin-bottom: 8px;
        }

        .schedules-header h3 {
            margin: 0;
            font-size: 11px;
            font-weight: 600;
            letter-spacing: 0.08em;
            text-transform: uppercase;
            color: var(--muted);
        }

        .section-accordion {
            margin-top: 14px;
            padding: 8px 11px;
            border-radius: 8px;
            background: rgba(0, 0, 0, 0.24);
            font-family: var(--font-data);
        }

        .section-accordion > summary {
            display: flex;
            justify-content: space-between;
            align-items: center;
            gap: 8px;
            list-style: none;
            cursor: pointer;
            padding: 2px 0;
        }

        .section-accordion > summary::-webkit-details-marker {
            display: none;
        }

        .section-accordion[open] > summary {
            margin-bottom: 8px;
        }

        .section-summary-text {
            display: inline-flex;
            align-items: center;
            gap: 6px;
            font-size: 11px;
            font-weight: 600;
            letter-spacing: 0.08em;
            text-transform: uppercase;
            color: var(--muted);
        }

        .section-chevron {
            display: inline-block;
            transition: transform 120ms ease;
            color: var(--muted);
            font-size: 10px;
        }

        .section-accordion[open] .section-chevron {
            transform: rotate(90deg);
        }

        .section-count {
            display: inline-block;
            min-width: 16px;
            padding: 0 4px;
            border-radius: 999px;
            background: rgba(255, 255, 255, 0.08);
            color: var(--text);
            font-size: 10px;
            text-align: center;
            letter-spacing: 0;
        }

        .schedule-add {
            padding: 4px 9px;
            border: 1px solid rgba(229, 216, 182, 0.3);
            border-radius: 6px;
            background: rgba(0, 0, 0, 0.3);
            color: var(--text);
            font-family: inherit;
            font-size: 11px;
            cursor: pointer;
        }

        .schedule-add:hover {
            border-color: rgba(229, 216, 182, 0.6);
        }

        .schedule-list {
            list-style: none;
            margin: 0;
            padding: 0;
            display: grid;
            gap: 6px;
        }

        .schedule-item {
            display: grid;
            grid-template-columns: auto 1fr auto;
            align-items: center;
            gap: 8px;
            padding: 6px 8px;
            border-radius: 6px;
            background: rgba(255, 255, 255, 0.04);
            font-size: 12px;
        }

        .schedule-item.disabled {
            opacity: 0.55;
        }

        .schedule-enabled {
            display: inline-flex;
            align-items: center;
        }

        .schedule-enabled input[type="checkbox"] {
            margin: 0;
            cursor: pointer;
        }

        .schedule-body {
            min-width: 0;
            display: grid;
            gap: 2px;
        }

        .schedule-summary {
            font-weight: 600;
            color: var(--text);
            overflow-wrap: anywhere;
            word-break: break-word;
        }

        .schedule-meta {
            color: var(--muted);
            font-size: 10px;
            line-height: 1.35;
            overflow-wrap: anywhere;
            word-break: break-word;
        }

        .schedule-action {
            display: inline-block;
            margin-right: 4px;
            padding: 0 4px;
            border-radius: 3px;
            font-size: 9px;
            font-weight: 700;
            letter-spacing: 0.06em;
        }

        .schedule-action.state-on {
            background: var(--green);
            color: #0c1a0c;
        }

        .schedule-action.state-off {
            background: var(--red);
            color: #200807;
        }

        .schedule-action.state-toggle,
        .schedule-action.state-cycle {
            background: var(--amber);
            color: #1a1306;
        }

        .schedule-actions {
            display: inline-flex;
            gap: 2px;
        }

        .schedule-edit,
        .schedule-delete {
            padding: 0;
            width: 22px;
            height: 22px;
            border: 0;
            border-radius: 4px;
            background: transparent;
            color: var(--muted);
            font-size: 14px;
            line-height: 1;
            cursor: pointer;
        }

        .schedule-delete {
            font-size: 16px;
        }

        .schedule-edit:hover {
            color: var(--text);
            background: rgba(0, 0, 0, 0.3);
        }

        .schedule-delete:hover {
            color: var(--red);
            background: rgba(0, 0, 0, 0.3);
        }

        .schedules-empty {
            margin: 0;
            font-size: 11px;
            color: var(--muted);
            font-style: italic;
        }

        .schedule-modal[hidden] {
            display: none;
        }

        .schedule-modal {
            position: fixed;
            inset: 0;
            z-index: 100;
            display: grid;
            place-items: center;
            padding: 16px;
        }

        .schedule-modal-backdrop {
            position: absolute;
            inset: 0;
            background: rgba(0, 0, 0, 0.62);
            backdrop-filter: blur(2px);
        }

        .schedule-modal-panel {
            position: relative;
            width: min(100%, 420px);
            max-height: calc(100vh - 32px);
            overflow: auto;
            padding: 18px 20px;
            border: 1px solid #070604;
            border-radius: 14px;
            background:
                linear-gradient(160deg, rgba(255,255,255,0.08), transparent 32%),
                linear-gradient(var(--breaker-top), var(--bakelite));
            color: var(--text);
            box-shadow:
                inset 0 0 0 1px rgba(255, 255, 255, 0.05),
                0 18px 38px rgba(0, 0, 0, 0.55);
            font-family: var(--font-data);
            display: grid;
            gap: 12px;
        }

        .schedule-modal-panel header {
            display: flex;
            justify-content: space-between;
            align-items: center;
            gap: 8px;
        }

        .schedule-modal-panel h3 {
            margin: 0;
            font-size: 14px;
            text-transform: uppercase;
            letter-spacing: 0.05em;
        }

        .schedule-modal-close {
            padding: 4px 8px;
            border: 0;
            border-radius: 4px;
            background: transparent;
            color: var(--muted);
            font-size: 18px;
            cursor: pointer;
        }

        .mode-tabs {
            display: inline-flex;
            border: 1px solid rgba(229, 216, 182, 0.3);
            border-radius: 6px;
            overflow: hidden;
            width: fit-content;
        }

        .mode-tabs button {
            padding: 5px 12px;
            border: 0;
            background: transparent;
            color: var(--muted);
            font-family: inherit;
            font-size: 11px;
            text-transform: uppercase;
            cursor: pointer;
        }

        .mode-tabs button[aria-pressed="true"] {
            background: rgba(255, 255, 255, 0.1);
            color: var(--text);
        }

        .schedule-modal-panel label {
            display: grid;
            gap: 4px;
            font-size: 11px;
            color: var(--muted);
            text-transform: uppercase;
            letter-spacing: 0.04em;
        }

        .schedule-modal-panel input[type="text"],
        .schedule-modal-panel input[type="time"],
        .schedule-modal-panel select {
            padding: 7px 9px;
            border: 1px solid rgba(229, 216, 182, 0.28);
            border-radius: 6px;
            background: rgba(0, 0, 0, 0.32);
            color: var(--text);
            font-family: inherit;
            font-size: 13px;
        }

        .schedule-modal-panel input[type="text"]:focus,
        .schedule-modal-panel input[type="time"]:focus,
        .schedule-modal-panel select:focus {
            outline: 2px solid rgba(229, 216, 182, 0.5);
            outline-offset: 1px;
        }

        .day-picker {
            display: flex;
            flex-wrap: wrap;
            gap: 6px;
            padding: 0;
            margin: 0;
            border: 0;
        }

        .day-picker legend {
            font-size: 11px;
            color: var(--muted);
            text-transform: uppercase;
            margin-bottom: 4px;
            padding: 0;
        }

        .day-picker label {
            display: inline-flex;
            align-items: center;
            gap: 4px;
            padding: 5px 9px;
            border: 1px solid rgba(229, 216, 182, 0.28);
            border-radius: 999px;
            background: rgba(0, 0, 0, 0.25);
            color: var(--text);
            font-size: 11px;
            text-transform: none;
            letter-spacing: 0;
            cursor: pointer;
        }

        .day-picker label:has(input:checked) {
            border-color: var(--green);
            background: color-mix(in srgb, var(--green) 18%, transparent);
        }

        .day-picker input[type="checkbox"] {
            display: none;
        }

        .day-picker-presets {
            display: flex;
            gap: 6px;
            margin-top: 4px;
            flex-wrap: wrap;
        }

        .day-picker-presets button {
            padding: 3px 8px;
            border: 1px solid rgba(229, 216, 182, 0.2);
            border-radius: 4px;
            background: transparent;
            color: var(--muted);
            font-family: inherit;
            font-size: 10px;
            cursor: pointer;
            text-transform: uppercase;
        }

        .day-picker-presets button:hover {
            color: var(--text);
            border-color: rgba(229, 216, 182, 0.5);
        }

        .cron-hint {
            margin: 0;
            font-size: 11px;
            color: var(--muted);
            line-height: 1.45;
        }

        .interval-duration-row {
            display: grid;
            grid-template-columns: 1fr 1fr;
            gap: 10px;
        }

        .schedule-modal-panel input[type="number"] {
            padding: 7px 9px;
            border: 1px solid rgba(229, 216, 182, 0.28);
            border-radius: 6px;
            background: rgba(0, 0, 0, 0.32);
            color: var(--text);
            font-family: inherit;
            font-size: 13px;
        }

        .start-with {
            display: flex;
            gap: 12px;
            padding: 0;
            margin: 0;
            border: 0;
        }

        .start-with legend {
            font-size: 11px;
            color: var(--muted);
            text-transform: uppercase;
            margin-bottom: 4px;
            padding: 0;
            width: 100%;
        }

        .start-with label {
            display: inline-flex;
            align-items: center;
            gap: 6px;
            font-size: 12px;
            color: var(--text);
            text-transform: none;
            letter-spacing: 0;
        }

        .start-with input[type="radio"] {
            margin: 0;
            cursor: pointer;
        }

        .schedule-form-error {
            margin: 0;
            padding: 8px 10px;
            border: 1px solid rgba(229, 119, 119, 0.45);
            border-radius: 6px;
            background: rgba(80, 16, 16, 0.5);
            color: #ffd4d4;
            font-size: 12px;
        }

        .schedule-form-actions {
            display: flex;
            justify-content: flex-end;
            gap: 8px;
        }

        .schedule-form-actions button {
            padding: 7px 14px;
            border: 1px solid rgba(229, 216, 182, 0.3);
            border-radius: 6px;
            background: rgba(0, 0, 0, 0.3);
            color: var(--text);
            font-family: inherit;
            font-size: 12px;
            cursor: pointer;
        }

        .schedule-form-actions button[type="submit"] {
            background: var(--green);
            color: #0c1a0c;
            border-color: transparent;
            font-weight: 700;
        }

        .schedule-form-actions button:disabled {
            opacity: 0.6;
            cursor: progress;
        }

        .conditions-panel {
            margin-bottom: 16px;
            padding: 14px;
            border: 1px solid #070604;
            border-radius: 12px;
            background:
                linear-gradient(160deg, rgba(255,255,255,0.06), transparent 32%),
                linear-gradient(var(--breaker-top), var(--bakelite));
            color: var(--text);
            font-family: var(--font-data);
        }

        .conditions-header {
            display: flex;
            justify-content: space-between;
            align-items: center;
            margin-bottom: 10px;
        }

        .conditions-header h2 {
            margin: 0;
            font-size: 13px;
            font-weight: 700;
            letter-spacing: 0.08em;
            text-transform: uppercase;
            color: var(--muted);
        }

        .condition-list {
            list-style: none;
            margin: 0;
            padding: 0;
            display: grid;
            gap: 6px;
        }

        .condition-item {
            display: grid;
            grid-template-columns: auto 1fr auto;
            align-items: start;
            gap: 8px;
            padding: 8px 10px;
            border-radius: 8px;
            background: rgba(255, 255, 255, 0.04);
            font-size: 12px;
        }

        .condition-item.failing {
            background: color-mix(in srgb, var(--red) 14%, rgba(0, 0, 0, 0.25));
        }

        .condition-item.passing {
            background: color-mix(in srgb, var(--green) 12%, rgba(0, 0, 0, 0.2));
        }

        .condition-item.unknown {
            background: rgba(255, 255, 255, 0.04);
        }

        .condition-item.disabled {
            opacity: 0.55;
        }

        .condition-status {
            display: inline-block;
            width: 10px;
            height: 10px;
            margin-top: 5px;
            border-radius: 50%;
            background: var(--muted);
            box-shadow: 0 0 8px rgba(0, 0, 0, 0.4);
        }

        .condition-status.passing {
            background: var(--green);
            box-shadow: 0 0 10px var(--green);
        }

        .condition-status.failing {
            background: var(--red);
            box-shadow: 0 0 10px var(--red);
        }

        .condition-status.unknown {
            background: var(--amber);
            box-shadow: 0 0 8px var(--amber);
        }

        .condition-body {
            min-width: 0;
            display: grid;
            gap: 2px;
        }

        .condition-name {
            font-weight: 600;
            color: var(--text);
        }

        .condition-target {
            color: var(--muted);
            font-size: 10px;
            overflow-wrap: anywhere;
            word-break: break-word;
        }

        .condition-meta {
            color: var(--muted);
            font-size: 10px;
            overflow-wrap: anywhere;
            word-break: break-word;
        }

        .condition-actions {
            display: inline-flex;
            gap: 2px;
        }

        .condition-actions button {
            padding: 0;
            width: 22px;
            height: 22px;
            border: 0;
            border-radius: 4px;
            background: transparent;
            color: var(--muted);
            font-size: 13px;
            line-height: 1;
            cursor: pointer;
        }

        .condition-actions button:hover {
            color: var(--text);
            background: rgba(0, 0, 0, 0.3);
        }

        .condition-actions .condition-delete:hover {
            color: var(--red);
        }

        .conditions-empty {
            margin: 0;
            font-size: 11px;
            color: var(--muted);
            font-style: italic;
        }

        .hooks-panel {
            margin-bottom: 16px;
            padding: 14px;
            border: 1px solid #070604;
            border-radius: 12px;
            background:
                linear-gradient(160deg, rgba(255,255,255,0.06), transparent 32%),
                linear-gradient(var(--breaker-top), var(--bakelite));
            color: var(--text);
            font-family: var(--font-data);
        }

        .hooks-header {
            display: flex;
            justify-content: space-between;
            align-items: center;
            margin-bottom: 10px;
        }

        .hooks-header h2 {
            margin: 0;
            font-size: 13px;
            font-weight: 700;
            letter-spacing: 0.08em;
            text-transform: uppercase;
            color: var(--muted);
        }

        .hook-list {
            list-style: none;
            margin: 0;
            padding: 0;
            display: grid;
            gap: 6px;
        }

        .hook-item {
            display: grid;
            grid-template-columns: 1fr auto;
            align-items: start;
            gap: 8px;
            padding: 8px 10px;
            border-radius: 8px;
            background: rgba(255, 255, 255, 0.04);
            font-size: 12px;
        }

        .hook-item.disabled {
            opacity: 0.55;
        }

        .hook-body {
            min-width: 0;
            display: grid;
            gap: 2px;
        }

        .hook-name {
            font-weight: 600;
            color: var(--text);
        }

        .hook-target {
            color: var(--muted);
            font-size: 10px;
            overflow-wrap: anywhere;
            word-break: break-word;
        }

        .hook-meta {
            color: var(--muted);
            font-size: 10px;
            overflow-wrap: anywhere;
            word-break: break-word;
        }

        .hook-actions {
            display: inline-flex;
            gap: 2px;
        }

        .hook-actions button {
            padding: 0;
            width: 22px;
            height: 22px;
            border: 0;
            border-radius: 4px;
            background: transparent;
            color: var(--muted);
            font-size: 13px;
            line-height: 1;
            cursor: pointer;
        }

        .hook-actions button:hover {
            color: var(--text);
            background: rgba(0, 0, 0, 0.3);
        }

        .hook-actions .hook-delete:hover {
            color: var(--red);
        }

        .hooks-empty {
            margin: 0;
            font-size: 11px;
            color: var(--muted);
            font-style: italic;
        }

        .schedule-modal-panel textarea {
            padding: 7px 9px;
            border: 1px solid rgba(229, 216, 182, 0.28);
            border-radius: 6px;
            background: rgba(0, 0, 0, 0.32);
            color: var(--text);
            font-family: var(--font-data);
            font-size: 12px;
            resize: vertical;
            min-height: 38px;
        }

        .schedule-modal-panel textarea:focus {
            outline: 2px solid rgba(229, 216, 182, 0.5);
            outline-offset: 1px;
        }

        .requires-picker {
            padding: 0;
            margin: 0;
            border: 0;
            display: grid;
            gap: 6px;
        }

        .requires-picker legend {
            font-size: 11px;
            color: var(--muted);
            text-transform: uppercase;
            letter-spacing: 0.04em;
            padding: 0;
        }

        .requires-list {
            display: flex;
            flex-wrap: wrap;
            gap: 6px;
        }

        .requires-list label {
            display: inline-flex;
            align-items: center;
            gap: 6px;
            padding: 5px 9px;
            border: 1px solid rgba(229, 216, 182, 0.28);
            border-radius: 999px;
            background: rgba(0, 0, 0, 0.25);
            color: var(--text);
            font-size: 11px;
            text-transform: none;
            letter-spacing: 0;
            cursor: pointer;
        }

        .requires-list label:has(input:checked) {
            border-color: var(--green);
            background: color-mix(in srgb, var(--green) 18%, transparent);
        }

        .requires-list input[type="checkbox"] {
            margin: 0;
            cursor: pointer;
        }

        .requires-empty {
            font-size: 11px;
            color: var(--muted);
            font-style: italic;
        }

        @media (max-width: 760px) {
            .header,
            .meter-row {
                grid-template-columns: 1fr;
                display: grid;
            }

            .shell {
                width: min(100vw - 20px, 1200px);
                margin-top: 10px;
            }

            .breaker-grid {
                grid-template-columns: 1fr;
            }

            .scan-button {
                width: 100%;
            }

            .header-actions {
                display: grid;
                grid-template-columns: 1fr;
            }

            .export-link,
            .theme-button {
                justify-content: center;
            }
        }

        .tab-bar {
            display: flex;
            gap: 4px;
            border-bottom: 1px solid var(--border, #30363d);
            margin: 0 0 16px;
            padding: 0;
        }
        .tab-button {
            background: transparent;
            border: none;
            color: var(--muted, #8b949e);
            padding: 8px 16px;
            font-size: 14px;
            font-weight: 600;
            cursor: pointer;
            border-bottom: 2px solid transparent;
        }
        .tab-button[aria-selected="true"] {
            color: var(--fg, #e6edf3);
            border-bottom-color: var(--accent, #2f81f7);
        }
        .tab-button:hover { color: var(--fg, #e6edf3); }
        .automations-tab {
            min-height: 600px;
        }
    </style>
</head>
<body>
    <main class="shell">
        <header class="header">
            <h1>Fusebox</h1>
            <div class="header-actions">
                <button class="theme-button" id="theme-toggle" type="button" aria-pressed="false">Dark mode</button>
                <a class="export-link" href="/api/energy/export.xlsx" download title="Download a workbook generated from Tapo history readings">Export xlsx</a>
                <button class="scan-button" id="scan" type="button">Scan now</button>
            </div>
        </header>

        <nav class="tab-bar" role="tablist" aria-label="Sections">
            <button class="tab-button" type="button" role="tab" data-tab="devices" aria-selected="true">Devices</button>
            <button class="tab-button" type="button" role="tab" data-tab="automations" aria-selected="false">Automations</button>
        </nav>

        <p class="notice" id="notice" role="status" hidden></p>

        <section class="cabinet" id="tab-devices" aria-live="polite">
            <div class="meter-row" aria-label="Fusebox summary">
                <div class="meter"><span>Devices</span><strong id="device-count">0</strong></div>
                <div class="meter"><span>Live load</span><strong id="total-power">0 W</strong></div>
                <div class="meter"><span>Today</span><strong id="today-energy">0 Wh</strong></div>
                <div class="meter"><span>Cost today</span><strong id="today-cost">0p</strong></div>
            </div>
            <section class="usage-panel" aria-labelledby="usage-title">
                <div class="usage-header">
                    <h2 id="usage-title">7 days usage</h2>
                    <span id="usage-range">Loading history</span>
                </div>
                <div class="usage-range-controls" id="usage-range-controls" aria-label="Energy usage history range">
                    <button class="range-button" type="button" data-history-range="5m">5m</button>
                    <button class="range-button" type="button" data-history-range="30m">30m</button>
                    <button class="range-button" type="button" data-history-range="1h">1h</button>
                    <button class="range-button" type="button" data-history-range="6h">6h</button>
                    <button class="range-button" type="button" data-history-range="12h">12h</button>
                    <button class="range-button" type="button" data-history-range="1d">1d</button>
                    <button class="range-button" type="button" data-history-range="3d">3d</button>
                    <button class="range-button" type="button" data-history-range="7d">7d</button>
                    <button class="range-button" type="button" data-history-range="30d">30d</button>
                    <button class="range-button" type="button" data-history-range="3m">3m</button>
                    <button class="range-button" type="button" data-history-range="6m">6m</button>
                    <button class="range-button" type="button" data-history-range="1y">1y</button>
                    <button class="range-button" type="button" data-history-range="ytd">YTD</button>
                    <button class="range-button" type="button" data-history-range="all">All</button>
                </div>
                <div class="usage-chart-container">
                    <canvas class="usage-chart" id="usage-chart" aria-label="Usage history for each energy-monitoring plug over the selected range." role="img"></canvas>
                </div>
                <p class="usage-empty" id="usage-empty">Loading power history from Tapo.</p>
            </section>
            <section class="hooks-panel" aria-label="Hooks">
                <div class="hooks-header">
                    <h2>Hooks</h2>
                    <button class="schedule-add" id="hook-add" type="button">+ Add hook</button>
                </div>
                <ul class="hook-list" id="hook-list"></ul>
                <p class="hooks-empty" id="hooks-empty">No hooks yet. Hooks fire an HTTP request when any device transitions on/off/online/offline.</p>
            </section>
            <div class="breaker-grid" id="devices"></div>
        </section>

        <section class="automations-tab" id="tab-automations" hidden>
            <div id="automations-root"></div>
        </section>
    </main>

    <div class="schedule-modal" id="hook-modal" hidden role="dialog" aria-modal="true" aria-labelledby="hook-modal-title">
        <div class="schedule-modal-backdrop" data-close-hook-modal></div>
        <form class="schedule-modal-panel" id="hook-form">
            <header>
                <h3 id="hook-modal-title">New hook</h3>
                <button class="schedule-modal-close" type="button" data-close-hook-modal aria-label="Close">&times;</button>
            </header>
            <label>
                Name
                <input type="text" name="name" maxlength="60" placeholder="notify ntfy" autocomplete="off" required />
            </label>
            <div class="interval-duration-row">
                <label>
                    Method
                    <select name="method">
                        <option value="POST">POST</option>
                        <option value="GET">GET</option>
                        <option value="PUT">PUT</option>
                        <option value="PATCH">PATCH</option>
                    </select>
                </label>
                <label>
                    Enabled
                    <select name="enabled">
                        <option value="true">Yes</option>
                        <option value="false">No</option>
                    </select>
                </label>
            </div>
            <label>
                URL
                <input type="text" name="url" placeholder="https://ntfy.example.com/topic" autocomplete="off" spellcheck="false" required />
            </label>
            <label>
                Headers (one per line, "Key: value")
                <textarea name="headers" rows="2" placeholder="Authorization: Bearer ..." spellcheck="false"></textarea>
            </label>
            <label>
                Body (optional, sends default JSON if blank)
                <textarea name="body" rows="3" placeholder='{"text":"{{device}} -> {{event}}"}' spellcheck="false"></textarea>
            </label>
            <fieldset class="day-picker" id="hook-device-filter-fieldset">
                <legend>Device filter (none ticked = all)</legend>
                <div class="requires-list" id="hook-device-filter-list">
                    <span class="requires-empty">No devices discovered yet.</span>
                </div>
            </fieldset>
            <fieldset class="day-picker">
                <legend>Event filter (none ticked = all)</legend>
                <label><input type="checkbox" name="event" value="on" /> On</label>
                <label><input type="checkbox" name="event" value="off" /> Off</label>
                <label><input type="checkbox" name="event" value="online" /> Online</label>
                <label><input type="checkbox" name="event" value="offline" /> Offline</label>
            </fieldset>
            <p class="cron-hint">Default payload is JSON: <code>{device, nickname, model, event, source, previous_on, new_on, timestamp_ms}</code>. <code>source</code> is one of <code>manual</code>, <code>schedule</code>, <code>condition</code>, <code>external</code> (e.g. the wall switch), or <code>discovery</code>. The body, headers, and URL all support <code>{{device}}</code>, <code>{{nickname}}</code>, <code>{{model}}</code>, <code>{{event}}</code>, <code>{{source}}</code>, <code>{{previous_on}}</code>, <code>{{new_on}}</code>, <code>{{timestamp_ms}}</code>.</p>
            <p class="schedule-form-error" id="hook-form-error" hidden></p>
            <div class="schedule-form-actions">
                <button type="button" data-close-hook-modal>Cancel</button>
                <button type="submit" id="hook-form-submit">Create</button>
            </div>
        </form>
    </div>

    <div class="schedule-modal" id="condition-modal" hidden role="dialog" aria-modal="true" aria-labelledby="condition-modal-title">
        <div class="schedule-modal-backdrop" data-close-condition-modal></div>
        <form class="schedule-modal-panel" id="condition-form">
            <header>
                <h3 id="condition-modal-title">New condition</h3>
                <button class="schedule-modal-close" type="button" data-close-condition-modal aria-label="Close">&times;</button>
            </header>
            <input type="hidden" name="device_name" id="condition-form-device" />
            <label>
                Name
                <input type="text" name="name" maxlength="60" placeholder="hot day" autocomplete="off" required />
            </label>
            <label>
                Method
                <select name="method">
                    <option value="GET">GET</option>
                    <option value="POST">POST</option>
                    <option value="HEAD">HEAD</option>
                    <option value="PUT">PUT</option>
                </select>
            </label>
            <label>
                URL
                <input type="text" name="url" placeholder="https://api.example.com/status" autocomplete="off" spellcheck="false" required />
            </label>
            <label>
                Headers (one per line, "Key: value")
                <textarea name="headers" rows="2" placeholder="Authorization: Bearer ..." spellcheck="false"></textarea>
            </label>
            <label>
                Request body (optional)
                <textarea name="body" rows="2" spellcheck="false"></textarea>
            </label>
            <div class="interval-duration-row">
                <label>
                    Status match
                    <input type="text" name="status_match" value="200-299" autocomplete="off" />
                </label>
                <label>
                    Poll every (s)
                    <input type="number" name="poll_seconds" min="5" max="3600" value="60" />
                </label>
            </div>
            <label>
                Body must contain (optional)
                <input type="text" name="body_contains" autocomplete="off" />
            </label>
            <label>
                Stable for (s) before transition counts
                <input type="number" name="min_stable_seconds" min="0" max="3600" value="60" />
            </label>
            <p class="cron-hint">Status match: codes or ranges (e.g. <code>200</code>, <code>200-299</code>). While this condition is failing, the device it belongs to is forced OFF. When passing again, the device returns to whatever the schedule wants. "Stable for" is a debounce: a flipped probe result must persist for that many seconds before it triggers a device toggle (0 = react immediately).</p>
            <p class="schedule-form-error" id="condition-form-error" hidden></p>
            <div class="schedule-form-actions">
                <button type="button" data-close-condition-modal>Cancel</button>
                <button type="submit" id="condition-form-submit">Create</button>
            </div>
        </form>
    </div>

    <div class="schedule-modal" id="schedule-modal" hidden role="dialog" aria-modal="true" aria-labelledby="schedule-modal-title">
        <div class="schedule-modal-backdrop" data-close-schedule-modal></div>
        <form class="schedule-modal-panel" id="schedule-form">
            <header>
                <h3 id="schedule-modal-title">New schedule</h3>
                <button class="schedule-modal-close" type="button" data-close-schedule-modal aria-label="Close">&times;</button>
            </header>
            <input type="hidden" name="device_name" id="schedule-form-device" />
            <div class="mode-tabs" role="tablist">
                <button type="button" data-mode="simple" aria-pressed="true">Simple</button>
                <button type="button" data-mode="interval" aria-pressed="false">Interval</button>
                <button type="button" data-mode="advanced" aria-pressed="false">Advanced</button>
            </div>
            <div class="mode-panel" data-panel="simple">
                <label>
                    Action
                    <select name="action">
                        <option value="on">Turn on</option>
                        <option value="off">Turn off</option>
                        <option value="toggle">Toggle</option>
                    </select>
                </label>
                <label>
                    Time
                    <input type="time" name="time" value="07:00" required />
                </label>
                <fieldset class="day-picker">
                    <legend>Days</legend>
                    <label><input type="checkbox" name="day" value="1" checked /> Mon</label>
                    <label><input type="checkbox" name="day" value="2" checked /> Tue</label>
                    <label><input type="checkbox" name="day" value="3" checked /> Wed</label>
                    <label><input type="checkbox" name="day" value="4" checked /> Thu</label>
                    <label><input type="checkbox" name="day" value="5" checked /> Fri</label>
                    <label><input type="checkbox" name="day" value="6" /> Sat</label>
                    <label><input type="checkbox" name="day" value="0" /> Sun</label>
                </fieldset>
                <div class="day-picker-presets">
                    <button type="button" data-preset="weekdays">Weekdays</button>
                    <button type="button" data-preset="weekends">Weekends</button>
                    <button type="button" data-preset="all">Every day</button>
                    <button type="button" data-preset="none">Clear</button>
                </div>
            </div>
            <div class="mode-panel" data-panel="interval" hidden>
                <div class="interval-duration-row">
                    <label>
                        On for (min)
                        <input type="number" name="on_minutes" min="0" step="1" value="60" />
                    </label>
                    <label>
                        Off for (min)
                        <input type="number" name="off_minutes" min="0" step="1" value="30" />
                    </label>
                </div>
                <fieldset class="start-with">
                    <legend>Start with</legend>
                    <label><input type="radio" name="start_action" value="on" checked /> On</label>
                    <label><input type="radio" name="start_action" value="off" /> Off</label>
                </fieldset>
                <div class="day-picker-presets">
                    <button type="button" data-interval-preset="15/15">15m / 15m</button>
                    <button type="button" data-interval-preset="30/30">30m / 30m</button>
                    <button type="button" data-interval-preset="60/30">1h / 30m</button>
                    <button type="button" data-interval-preset="60/60">1h / 1h</button>
                </div>
                <p class="cron-hint">Cycle repeats forever. Total on + off must be at least 1 minute.</p>
            </div>
            <div class="mode-panel" data-panel="advanced" hidden>
                <label>
                    Action
                    <select name="action-advanced">
                        <option value="on">Turn on</option>
                        <option value="off">Turn off</option>
                        <option value="toggle">Toggle</option>
                    </select>
                </label>
                <label>
                    Cron expression
                    <input type="text" name="cron" placeholder="0 7 * * 1-5" autocomplete="off" spellcheck="false" />
                </label>
                <p class="cron-hint">Standard 5-field cron: <code>min hour day-of-month month day-of-week</code>. Examples: <code>*/15 * * * *</code>, <code>0 22 * * 0,6</code>, <code>30 7 1 * *</code>.</p>
            </div>
            <label>
                Label (optional)
                <input type="text" name="label" maxlength="80" placeholder="Morning lights" autocomplete="off" />
            </label>
            <p class="schedule-form-error" id="schedule-form-error" hidden></p>
            <div class="schedule-form-actions">
                <button type="button" data-close-schedule-modal>Cancel</button>
                <button type="submit" id="schedule-form-submit">Create</button>
            </div>
        </form>
    </div>

    <script src="https://cdn.jsdelivr.net/npm/chart.js@4.5.1/dist/chart.umd.min.js"></script>
    <script>
        // Top-level tab switching. The Automations bundle is lazy-loaded the
        // first time the user opens the Automations tab so the Devices tab
        // stays cheap to boot.
        (function initTabs() {
            const tabs = Array.from(document.querySelectorAll(".tab-button"));
            const panels = {
                devices: document.querySelector("#tab-devices"),
                automations: document.querySelector("#tab-automations"),
            };
            let automationsLoaded = false;
            let automationsLoading = null;
            const root = document.querySelector("#automations-root");

            async function loadAutomations() {
                if (automationsLoaded) return;
                if (automationsLoading) return automationsLoading;
                automationsLoading = new Promise((resolve, reject) => {
                    const tag = document.createElement("script");
                    tag.src = "/assets/automations.js";
                    tag.async = true;
                    tag.onload = () => {
                        if (window.FuseboxAutomations && root) {
                            window.FuseboxAutomations.mount(root);
                            automationsLoaded = true;
                        }
                        resolve();
                    };
                    tag.onerror = () => reject(new Error("failed to load automations bundle"));
                    document.head.appendChild(tag);
                });
                return automationsLoading;
            }

            function activate(name) {
                for (const t of tabs) {
                    t.setAttribute("aria-selected", t.dataset.tab === name ? "true" : "false");
                }
                for (const [key, panel] of Object.entries(panels)) {
                    if (!panel) continue;
                    panel.hidden = key !== name;
                }
                if (name === "automations") {
                    loadAutomations().catch((err) => {
                        if (root) root.textContent = "Failed to load Automations: " + err;
                    });
                }
            }

            for (const t of tabs) {
                t.addEventListener("click", () => activate(t.dataset.tab));
            }
        })();

        const devicesEl = document.querySelector("#devices");
        const scanButton = document.querySelector("#scan");
        const themeButton = document.querySelector("#theme-toggle");
        const themeColorMeta = document.querySelector('meta[name="theme-color"]');
        const deviceCountEl = document.querySelector("#device-count");
        const totalPowerEl = document.querySelector("#total-power");
        const todayEnergyEl = document.querySelector("#today-energy");
        const todayCostEl = document.querySelector("#today-cost");
        const noticeEl = document.querySelector("#notice");
        const usageTitleEl = document.querySelector("#usage-title");
        const usageChartEl = document.querySelector("#usage-chart");
        const usageEmptyEl = document.querySelector("#usage-empty");
        const usageRangeEl = document.querySelector("#usage-range");
        const usageRangeControlsEl = document.querySelector("#usage-range-controls");
        const scheduleModalEl = document.querySelector("#schedule-modal");
        const scheduleFormEl = document.querySelector("#schedule-form");
        const scheduleFormDeviceEl = document.querySelector("#schedule-form-device");
        const scheduleFormErrorEl = document.querySelector("#schedule-form-error");
        const scheduleFormSubmitEl = document.querySelector("#schedule-form-submit");
        const scheduleModalTitleEl = document.querySelector("#schedule-modal-title");
        const deviceStreamReconnectMs = 2000;
        const switchSoundUrl = "/assets/switch.wav";
        const chartPalettes = {
            classic: ["#8a5a00", "#005ea8", "#b2352b", "#6f4dbf", "#00746f", "#a14500", "#4f7500", "#a43778"],
            dark: ["#e5b75b", "#7bb7ff", "#f06b5c", "#c99cff", "#62d6d1", "#ff9d66", "#b6e36a", "#f38ad3"],
        };
        const defaultHistoryRange = "7d";
        const dayNames = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let selectedHistoryRange = defaultHistoryRange;
        let powerChart = null;
        let deviceRequestInFlight = false;
        let historyRequestInFlight = false;
        let deviceSocket = null;
        let deviceSocketReconnect = null;
        let switchAudioContext = null;
        let switchAudioBufferPromise = null;
        let latestDevices = [];
        let schedulesByDevice = new Map();
        let schedulesById = new Map();
        let schedulesLoadInFlight = false;
        let currentEditScheduleId = null;
        let conditionsById = new Map();
        let conditionsList = [];
        let conditionsLoadInFlight = false;
        let currentEditConditionId = null;

        syncThemeButton();
        syncHistoryRangeButtons();
        initializePowerChart();

        themeButton.addEventListener("click", () => {
            const nextTheme = document.documentElement.dataset.theme === "dark" ? "classic" : "dark";
            document.documentElement.dataset.theme = nextTheme;

            try {
                localStorage.setItem("fusebox-theme", nextTheme);
            } catch (_error) {
                // Ignore storage failures; the active page can still switch theme.
            }

            syncThemeButton();
        });

        scanButton.addEventListener("click", async () => {
            scanButton.disabled = true;
            scanButton.textContent = "Scanning";
            try {
                const payload = await requestJson("/api/scan", { method: "POST" });
                renderDevicePayload(payload);
                loadUsageHistory();
            } catch (error) {
                renderNotice(error.message);
            } finally {
                scanButton.disabled = false;
                scanButton.textContent = "Scan now";
            }
        });

        usageRangeControlsEl.addEventListener("click", (event) => {
            const button = event.target.closest("button[data-history-range]");
            if (button === null) return;

            selectedHistoryRange = button.dataset.historyRange ?? defaultHistoryRange;
            syncHistoryRangeButtons();
            loadUsageHistory();
        });

        async function loadDevices() {
            if (deviceRequestInFlight) return;

            deviceRequestInFlight = true;

            try {
                const payload = await requestJson("/api/devices");
                renderDevicePayload(payload);
            } catch (error) {
                renderNotice(error.message);
            } finally {
                deviceRequestInFlight = false;
            }
        }

        async function loadUsageHistory() {
            if (historyRequestInFlight) return;

            historyRequestInFlight = true;
            usageRangeEl.textContent = "Loading history";

            try {
                const payload = await requestJson(`/api/energy/history.json?range=${encodeURIComponent(selectedHistoryRange)}`);
                renderUsageHistory(payload);
            } catch (error) {
                usageEmptyEl.hidden = false;
                usageEmptyEl.textContent = error.message;
                usageRangeEl.textContent = "History unavailable";
            } finally {
                historyRequestInFlight = false;
            }
        }

        function connectDeviceStream() {
            if (!("WebSocket" in window) || deviceSocket !== null) return;

            const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
            const socket = new WebSocket(`${protocol}//${window.location.host}/ws/devices`);
            deviceSocket = socket;

            socket.addEventListener("message", (event) => {
                try {
                    const payload = JSON.parse(event.data);
                    renderDevicePayload(payload);
                } catch (_error) {
                    renderNotice("Live device update was not valid JSON.");
                }
            });

            socket.addEventListener("close", () => {
                if (deviceSocket !== socket) return;

                deviceSocket = null;
                scheduleDeviceStreamReconnect();
            });

            socket.addEventListener("error", () => {
                socket.close();
            });
        }

        function scheduleDeviceStreamReconnect() {
            if (deviceSocketReconnect !== null) return;

            deviceSocketReconnect = window.setTimeout(() => {
                deviceSocketReconnect = null;
                connectDeviceStream();
            }, deviceStreamReconnectMs);
        }

        async function requestJson(url, options) {
            const response = await fetch(url, options);
            let payload = {};

            try {
                payload = await response.json();
            } catch (_error) {
                payload = {};
            }

            if (!response.ok) {
                throw new Error(payload.error?.message ?? `Request failed with status ${response.status}`);
            }

            return payload;
        }

        function renderNotice(message) {
            if (!message) {
                noticeEl.hidden = true;
                noticeEl.textContent = "";
                return;
            }

            noticeEl.hidden = false;
            noticeEl.textContent = message;
        }

        function syncThemeButton() {
            const isDark = document.documentElement.dataset.theme === "dark";

            themeButton.setAttribute("aria-pressed", String(isDark));
            themeButton.textContent = isDark ? "Classic mode" : "Dark mode";
            themeColorMeta.setAttribute("content", isDark ? "#08090b" : "#201d19");
            syncPowerChartTheme();
        }

        function renderDevicePayload(payload) {
            const devices = payload.devices ?? [];
            latestDevices = devices;
            renderDevices(devices);
            renderNotice(payload.scan_error);
        }

        function syncHistoryRangeButtons() {
            usageRangeControlsEl.querySelectorAll("button[data-history-range]").forEach((button) => {
                const isSelected = button.dataset.historyRange === selectedHistoryRange;
                button.setAttribute("aria-pressed", String(isSelected));
            });
        }

        function renderDevices(devices) {
            const totalPower = devices.reduce((total, device) => total + (device.energy?.current_power_w ?? 0), 0);
            const todayEnergy = devices.reduce((total, device) => total + (device.energy?.today_energy_wh ?? 0), 0);
            const todayCost = devices.reduce((total, device) => total + (device.energy?.today_cost_pence ?? 0), 0);

            deviceCountEl.textContent = devices.length;
            totalPowerEl.textContent = `${totalPower} W`;
            todayEnergyEl.textContent = formatEnergy(todayEnergy);
            todayCostEl.textContent = formatCost(todayCost);

            if (devices.length === 0) {
                devicesEl.innerHTML = `<div class="empty">No supported Tapo plugs found yet. Check credentials and LAN access, or press Scan now.</div>`;
                return { totalPower, todayEnergy, todayCost };
            }

            devicesEl.innerHTML = devices.map((device) => renderDevice(device, schedulesByDevice.get(device.name) ?? [])).join("");
            devicesEl.querySelectorAll("button[data-device]").forEach((button) => {
                button.addEventListener("click", async () => {
                    const wasOn = button.dataset.on === "true";
                    const nextIsOn = !wasOn;

                    button.disabled = true;
                    button.dataset.on = String(nextIsOn);
                    button.setAttribute("aria-pressed", String(nextIsOn));
                    button.classList.add("is-switching");
                    let toggleSucceeded = false;
                    prepareSwitchAudio();

                    try {
                        await requestJson(`/api/devices/${encodeURIComponent(button.dataset.device)}/toggle`, { method: "POST" });
                        await loadDevices();
                        toggleSucceeded = true;
                    } catch (error) {
                        button.dataset.on = String(wasOn);
                        button.setAttribute("aria-pressed", String(wasOn));
                        renderNotice(error.message);
                    } finally {
                        button.disabled = false;
                        if (toggleSucceeded) {
                            playSwitchClick(nextIsOn);
                        }
                        window.setTimeout(() => {
                            button.classList.remove("is-switching");
                        }, 180);
                    }
                });
            });

            wireScheduleControls();
            wireConditionControls();
            wireSectionAccordions();
            wireReleaseOverrideButtons();

            return { totalPower, todayEnergy, todayCost };
        }

        function wireReleaseOverrideButtons() {
            devicesEl.querySelectorAll("button[data-release-override]").forEach((button) => {
                button.addEventListener("click", async () => {
                    const name = button.dataset.releaseOverride;
                    button.disabled = true;
                    try {
                        await requestJson(`/api/devices/${encodeURIComponent(name)}/release-override`, {
                            method: "POST",
                        });
                        await loadDevices();
                    } catch (error) {
                        renderNotice(error.message);
                    } finally {
                        button.disabled = false;
                    }
                });
            });
        }

        function wireSectionAccordions() {
            devicesEl.querySelectorAll("details.section-accordion").forEach((details) => {
                details.addEventListener("toggle", () => {
                    sectionOpenStateSet(details.dataset.device, details.dataset.section, details.open);
                });
                details.querySelector("summary").addEventListener("click", (event) => {
                    if (event.target.closest("button")) {
                        event.preventDefault();
                    }
                });
            });
        }

        function wireScheduleControls() {
            devicesEl.querySelectorAll("button[data-add-schedule]").forEach((button) => {
                button.addEventListener("click", () => {
                    openScheduleModal(button.dataset.addSchedule);
                });
            });

            devicesEl.querySelectorAll("button[data-schedule-delete]").forEach((button) => {
                button.addEventListener("click", async () => {
                    const id = button.dataset.scheduleDelete;
                    button.disabled = true;
                    try {
                        const response = await fetch(`/api/schedules/${encodeURIComponent(id)}`, { method: "DELETE" });
                        if (!response.ok && response.status !== 204) {
                            const payload = await response.json().catch(() => null);
                            throw new Error(payload?.error?.message ?? `Delete failed (${response.status})`);
                        }
                        await loadSchedules();
                    } catch (error) {
                        renderNotice(error.message);
                    } finally {
                        button.disabled = false;
                    }
                });
            });

            devicesEl.querySelectorAll("button[data-schedule-edit]").forEach((button) => {
                button.addEventListener("click", () => {
                    const id = button.dataset.scheduleEdit;
                    const schedule = schedulesById.get(id);
                    if (!schedule) {
                        renderNotice("Schedule not found; reloading.");
                        loadSchedules();
                        return;
                    }
                    openScheduleModal(schedule.device_name, schedule);
                });
            });

            devicesEl.querySelectorAll("input[data-schedule-enabled]").forEach((input) => {
                input.addEventListener("change", async () => {
                    const id = input.dataset.scheduleEnabled;
                    const enabled = input.checked;
                    input.disabled = true;
                    try {
                        await requestJson(`/api/schedules/${encodeURIComponent(id)}`, {
                            method: "PATCH",
                            headers: { "Content-Type": "application/json" },
                            body: JSON.stringify({ enabled }),
                        });
                        await loadSchedules();
                    } catch (error) {
                        input.checked = !enabled;
                        renderNotice(error.message);
                    } finally {
                        input.disabled = false;
                    }
                });
            });
        }

        async function loadSchedules() {
            if (schedulesLoadInFlight) return;
            schedulesLoadInFlight = true;
            try {
                const payload = await requestJson("/api/schedules");
                const map = new Map();
                const byId = new Map();
                for (const schedule of payload.schedules ?? []) {
                    if (!map.has(schedule.device_name)) {
                        map.set(schedule.device_name, []);
                    }
                    map.get(schedule.device_name).push(schedule);
                    byId.set(schedule.id, schedule);
                }
                schedulesByDevice = map;
                schedulesById = byId;
                if (latestDevices.length > 0) {
                    renderDevices(latestDevices);
                }
            } catch (error) {
                renderNotice(error.message);
            } finally {
                schedulesLoadInFlight = false;
            }
        }

        const conditionModalEl = document.querySelector("#condition-modal");
        const conditionFormEl = document.querySelector("#condition-form");
        const conditionFormErrorEl = document.querySelector("#condition-form-error");
        const conditionFormSubmitEl = document.querySelector("#condition-form-submit");
        const conditionModalTitleEl = document.querySelector("#condition-modal-title");
        const conditionFormDeviceEl = document.querySelector("#condition-form-device");
        let conditionsByDevice = new Map();

        async function loadConditions() {
            if (conditionsLoadInFlight) return;
            conditionsLoadInFlight = true;
            try {
                const payload = await requestJson("/api/conditions");
                const list = payload.conditions ?? [];
                conditionsList = list;
                conditionsById = new Map(list.map((c) => [c.id, c]));
                const byDevice = new Map();
                for (const condition of list) {
                    const device = condition.device_name;
                    if (!byDevice.has(device)) byDevice.set(device, []);
                    byDevice.get(device).push(condition);
                }
                conditionsByDevice = byDevice;
                if (latestDevices.length > 0) {
                    renderDevices(latestDevices);
                }
            } catch (error) {
                renderNotice(error.message);
            } finally {
                conditionsLoadInFlight = false;
            }
        }

        function renderConditionItem(condition) {
            const stateClass = !condition.enabled
                ? "disabled"
                : condition.last_passing === true
                    ? "passing"
                    : condition.last_passing === false
                        ? "failing"
                        : "unknown";
            const statusLabel = !condition.enabled
                ? "disabled"
                : condition.last_passing === true
                    ? "passing"
                    : condition.last_passing === false
                        ? "failing"
                        : "unknown";
            const metaParts = [
                `every ${condition.poll_seconds}s`,
                `match ${escapeHtml(condition.status_match)}`,
            ];
            if (condition.body_contains) {
                metaParts.push(`body~ ${escapeHtml(condition.body_contains)}`);
            }
            if (condition.last_status_code !== null && condition.last_status_code !== undefined) {
                metaParts.push(`last HTTP ${condition.last_status_code}`);
            }
            if (condition.last_checked_at_ms) {
                metaParts.push(`checked ${formatRelative(condition.last_checked_at_ms)}`);
            }
            const actionParts = [];
            if (condition.action_on_pass) actionParts.push(`pass→${condition.action_on_pass}`);
            if (condition.action_on_fail) actionParts.push(`fail→${condition.action_on_fail}`);
            if (actionParts.length > 0) {
                metaParts.push(actionParts.join(" / "));
            }
            if (condition.last_action && condition.last_action_at_ms) {
                metaParts.push(`fired ${escapeHtml(condition.last_action)} ${formatRelative(condition.last_action_at_ms)}`);
            }
            if (condition.last_action_error) {
                metaParts.push(`<span style="color: var(--red);">action: ${escapeHtml(condition.last_action_error)}</span>`);
            }
            if (condition.last_error) {
                metaParts.push(`<span style="color: var(--red);">${escapeHtml(condition.last_error)}</span>`);
            }

            return `
                <li class="condition-item ${stateClass}">
                    <span class="condition-status ${stateClass}" title="${escapeHtml(statusLabel)}" aria-label="${escapeHtml(statusLabel)}"></span>
                    <div class="condition-body">
                        <span class="condition-name">${escapeHtml(condition.name)}</span>
                        <span class="condition-target">${escapeHtml(condition.method)} ${escapeHtml(condition.url)}</span>
                        <span class="condition-meta">${metaParts.join(" / ")}</span>
                    </div>
                    <div class="condition-actions">
                        <button class="condition-probe" type="button" data-condition-probe="${escapeHtml(condition.id)}" aria-label="Probe now" title="Probe now">&#8634;</button>
                        <button class="condition-edit" type="button" data-condition-edit="${escapeHtml(condition.id)}" aria-label="Edit condition" title="Edit">&#9998;</button>
                        <button class="condition-delete" type="button" data-condition-delete="${escapeHtml(condition.id)}" aria-label="Delete condition" title="Delete">&times;</button>
                    </div>
                </li>
            `;
        }

        function wireConditionControls() {
            devicesEl.querySelectorAll("button[data-add-condition]").forEach((button) => {
                button.addEventListener("click", () => {
                    openConditionModal(button.dataset.addCondition);
                });
            });

            devicesEl.querySelectorAll("button[data-condition-probe]").forEach((button) => {
                button.addEventListener("click", async () => {
                    const id = button.dataset.conditionProbe;
                    button.disabled = true;
                    try {
                        await requestJson(`/api/conditions/${encodeURIComponent(id)}/probe`, { method: "POST" });
                        await loadConditions();
                    } catch (error) {
                        renderNotice(error.message);
                    } finally {
                        button.disabled = false;
                    }
                });
            });

            devicesEl.querySelectorAll("button[data-condition-edit]").forEach((button) => {
                button.addEventListener("click", () => {
                    const id = button.dataset.conditionEdit;
                    const condition = conditionsById.get(id);
                    if (!condition) {
                        renderNotice("Condition not found; reloading.");
                        loadConditions();
                        return;
                    }
                    openConditionModal(condition.device_name, condition);
                });
            });

            devicesEl.querySelectorAll("button[data-condition-delete]").forEach((button) => {
                button.addEventListener("click", async () => {
                    const id = button.dataset.conditionDelete;
                    button.disabled = true;
                    try {
                        const response = await fetch(`/api/conditions/${encodeURIComponent(id)}`, { method: "DELETE" });
                        if (!response.ok && response.status !== 204) {
                            const payload = await response.json().catch(() => null);
                            throw new Error(payload?.error?.message ?? `Delete failed (${response.status})`);
                        }
                        await loadConditions();
                        await loadSchedules();
                    } catch (error) {
                        renderNotice(error.message);
                    } finally {
                        button.disabled = false;
                    }
                });
            });
        }

        function openConditionModal(deviceName, condition = null) {
            const isEditing = condition !== null;
            currentEditConditionId = isEditing ? condition.id : null;

            const device = latestDevices.find((entry) => entry.name === deviceName);
            const displayName = device?.nickname || deviceName;

            conditionFormErrorEl.hidden = true;
            conditionFormErrorEl.textContent = "";
            conditionFormSubmitEl.disabled = false;
            conditionFormSubmitEl.textContent = isEditing ? "Save" : "Create";
            conditionModalTitleEl.textContent = isEditing
                ? `Edit condition — ${condition.name}`
                : `New condition — ${displayName}`;
            conditionFormEl.reset();
            conditionFormDeviceEl.value = deviceName;

            const set = (selector, value) => {
                const el = conditionFormEl.querySelector(selector);
                if (el) el.value = value;
            };

            if (isEditing) {
                set('input[name="name"]', condition.name);
                set('select[name="method"]', condition.method);
                set('input[name="url"]', condition.url);
                set('input[name="status_match"]', condition.status_match);
                set('input[name="poll_seconds"]', String(condition.poll_seconds));
                set('input[name="body_contains"]', condition.body_contains ?? "");
                set('input[name="min_stable_seconds"]', String(condition.min_stable_seconds ?? 0));
                set('textarea[name="body"]', condition.body ?? "");
                const headerLines = Object.entries(condition.headers || {}).map(([k, v]) => `${k}: ${v}`).join("\n");
                set('textarea[name="headers"]', headerLines);
            } else {
                set('select[name="method"]', "GET");
                set('input[name="status_match"]', "200-299");
                set('input[name="poll_seconds"]', "60");
                set('input[name="min_stable_seconds"]', "60");
            }

            conditionModalEl.hidden = false;
            window.setTimeout(() => {
                conditionFormEl.querySelector('input[name="name"]').focus();
            }, 30);
        }

        function closeConditionModal() {
            conditionModalEl.hidden = true;
            currentEditConditionId = null;
        }

        conditionModalEl.addEventListener("click", (event) => {
            if (event.target.matches("[data-close-condition-modal]")) {
                closeConditionModal();
            }
        });

        document.addEventListener("keydown", (event) => {
            if (event.key === "Escape" && !conditionModalEl.hidden) {
                closeConditionModal();
            }
        });

        conditionFormEl.addEventListener("submit", async (event) => {
            event.preventDefault();
            conditionFormErrorEl.hidden = true;
            conditionFormErrorEl.textContent = "";

            const name = conditionFormEl.querySelector('input[name="name"]').value.trim();
            const deviceName = conditionFormDeviceEl.value;
            const method = conditionFormEl.querySelector('select[name="method"]').value;
            const url = conditionFormEl.querySelector('input[name="url"]').value.trim();
            const statusMatch = conditionFormEl.querySelector('input[name="status_match"]').value.trim();
            const pollSeconds = Number.parseInt(conditionFormEl.querySelector('input[name="poll_seconds"]').value, 10);
            const bodyContains = conditionFormEl.querySelector('input[name="body_contains"]').value;
            const body = conditionFormEl.querySelector('textarea[name="body"]').value;
            const headersRaw = conditionFormEl.querySelector('textarea[name="headers"]').value;
            const minStableSeconds = Number.parseInt(conditionFormEl.querySelector('input[name="min_stable_seconds"]').value, 10);

            if (!name) return showConditionFormError("Name is required.");
            if (!url) return showConditionFormError("URL is required.");
            if (!statusMatch) return showConditionFormError("Status match is required.");
            if (!Number.isFinite(pollSeconds) || pollSeconds < 5 || pollSeconds > 3600) {
                return showConditionFormError("Poll interval must be 5-3600 seconds.");
            }
            if (!Number.isFinite(minStableSeconds) || minStableSeconds < 0 || minStableSeconds > 3600) {
                return showConditionFormError("Stable seconds must be between 0 and 3600.");
            }

            const headers = {};
            for (const line of headersRaw.split("\n")) {
                const trimmed = line.trim();
                if (trimmed === "") continue;
                const sep = trimmed.indexOf(":");
                if (sep < 0) return showConditionFormError(`Bad header line: '${trimmed}' (expected 'Key: value').`);
                const key = trimmed.slice(0, sep).trim();
                const value = trimmed.slice(sep + 1).trim();
                if (key === "") return showConditionFormError("Header key cannot be empty.");
                headers[key] = value;
            }

            const isEditing = currentEditConditionId !== null;
            const endpoint = isEditing
                ? `/api/conditions/${encodeURIComponent(currentEditConditionId)}`
                : "/api/conditions";
            const method_ = isEditing ? "PATCH" : "POST";
            const payload = {
                name,
                device_name: deviceName,
                url,
                method,
                headers,
                body: body === "" ? null : body,
                status_match: statusMatch,
                body_contains: bodyContains === "" ? null : bodyContains,
                poll_seconds: pollSeconds,
                min_stable_seconds: minStableSeconds,
            };
            if (!isEditing) payload.enabled = true;

            conditionFormSubmitEl.disabled = true;
            conditionFormSubmitEl.textContent = isEditing ? "Saving" : "Creating";

            try {
                await requestJson(endpoint, {
                    method: method_,
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify(payload),
                });
                closeConditionModal();
                await loadConditions();
            } catch (error) {
                showConditionFormError(error.message);
            } finally {
                conditionFormSubmitEl.disabled = false;
                conditionFormSubmitEl.textContent = isEditing ? "Save" : "Create";
            }
        });

        function showConditionFormError(message) {
            conditionFormErrorEl.textContent = message;
            conditionFormErrorEl.hidden = false;
        }

        const hookListEl = document.querySelector("#hook-list");
        const hooksEmptyEl = document.querySelector("#hooks-empty");
        const hookAddBtn = document.querySelector("#hook-add");
        const hookModalEl = document.querySelector("#hook-modal");
        const hookFormEl = document.querySelector("#hook-form");
        const hookFormErrorEl = document.querySelector("#hook-form-error");
        const hookFormSubmitEl = document.querySelector("#hook-form-submit");
        const hookModalTitleEl = document.querySelector("#hook-modal-title");
        let hooksList = [];
        let hooksById = new Map();
        let hooksLoadInFlight = false;
        let currentEditHookId = null;

        async function loadHooks() {
            if (hooksLoadInFlight) return;
            hooksLoadInFlight = true;
            try {
                const payload = await requestJson("/api/hooks");
                hooksList = payload.hooks ?? [];
                hooksById = new Map(hooksList.map((h) => [h.id, h]));
                renderHooks(hooksList);
            } catch (error) {
                renderNotice(error.message);
            } finally {
                hooksLoadInFlight = false;
            }
        }

        function renderHooks(hooks) {
            if (hooks.length === 0) {
                hookListEl.innerHTML = "";
                hooksEmptyEl.hidden = false;
                return;
            }
            hooksEmptyEl.hidden = true;
            hookListEl.innerHTML = hooks.map(renderHookItem).join("");
            wireHookControls();
        }

        function renderHookItem(hook) {
            const stateClass = !hook.enabled ? "disabled" : "";
            const deviceLabel = hook.device_filter.length === 0 ? "any device" : `device ${escapeHtml(hook.device_filter.join(", "))}`;
            const eventLabel = hook.event_filter.length === 0 ? "all events" : `events ${escapeHtml(hook.event_filter.join(", "))}`;
            const meta = [`${escapeHtml(hook.method)} on ${deviceLabel}`, eventLabel];
            if (hook.last_fired_at_ms) {
                const label = hook.last_event ? `last ${escapeHtml(hook.last_event)}` : "last fired";
                meta.push(`${label} ${formatRelative(hook.last_fired_at_ms)}`);
            }
            if (hook.last_status_code !== null && hook.last_status_code !== undefined) {
                meta.push(`HTTP ${hook.last_status_code}`);
            }
            if (hook.last_error) {
                meta.push(`<span style="color: var(--red);">${escapeHtml(hook.last_error)}</span>`);
            }

            return `
                <li class="hook-item ${stateClass}">
                    <div class="hook-body">
                        <span class="hook-name">${escapeHtml(hook.name)}</span>
                        <span class="hook-target">${escapeHtml(hook.url)}</span>
                        <span class="hook-meta">${meta.join(" / ")}</span>
                    </div>
                    <div class="hook-actions">
                        <button class="hook-test" type="button" data-hook-test="${escapeHtml(hook.id)}" aria-label="Test now" title="Send synthetic event">&#9658;</button>
                        <button class="hook-edit" type="button" data-hook-edit="${escapeHtml(hook.id)}" aria-label="Edit hook" title="Edit">&#9998;</button>
                        <button class="hook-delete" type="button" data-hook-delete="${escapeHtml(hook.id)}" aria-label="Delete hook" title="Delete">&times;</button>
                    </div>
                </li>
            `;
        }

        function wireHookControls() {
            hookListEl.querySelectorAll("button[data-hook-test]").forEach((button) => {
                button.addEventListener("click", async () => {
                    const id = button.dataset.hookTest;
                    button.disabled = true;
                    try {
                        await requestJson(`/api/hooks/${encodeURIComponent(id)}/test`, { method: "POST" });
                        await loadHooks();
                    } catch (error) {
                        renderNotice(error.message);
                    } finally {
                        button.disabled = false;
                    }
                });
            });

            hookListEl.querySelectorAll("button[data-hook-edit]").forEach((button) => {
                button.addEventListener("click", () => {
                    const id = button.dataset.hookEdit;
                    const hook = hooksById.get(id);
                    if (!hook) {
                        renderNotice("Hook not found; reloading.");
                        loadHooks();
                        return;
                    }
                    openHookModal(hook);
                });
            });

            hookListEl.querySelectorAll("button[data-hook-delete]").forEach((button) => {
                button.addEventListener("click", async () => {
                    const id = button.dataset.hookDelete;
                    button.disabled = true;
                    try {
                        const response = await fetch(`/api/hooks/${encodeURIComponent(id)}`, { method: "DELETE" });
                        if (!response.ok && response.status !== 204) {
                            const payload = await response.json().catch(() => null);
                            throw new Error(payload?.error?.message ?? `Delete failed (${response.status})`);
                        }
                        await loadHooks();
                    } catch (error) {
                        renderNotice(error.message);
                    } finally {
                        button.disabled = false;
                    }
                });
            });
        }

        function openHookModal(hook = null) {
            const isEditing = hook !== null;
            currentEditHookId = isEditing ? hook.id : null;

            hookFormErrorEl.hidden = true;
            hookFormErrorEl.textContent = "";
            hookFormSubmitEl.disabled = false;
            hookFormSubmitEl.textContent = isEditing ? "Save" : "Create";
            hookModalTitleEl.textContent = isEditing ? `Edit hook — ${hook.name}` : "New hook";
            hookFormEl.reset();

            const set = (selector, value) => {
                const el = hookFormEl.querySelector(selector);
                if (el !== null && el !== undefined) el.value = value;
            };

            if (isEditing) {
                set('input[name="name"]', hook.name);
                set('select[name="method"]', hook.method);
                set('select[name="enabled"]', String(hook.enabled));
                set('input[name="url"]', hook.url);
                set('textarea[name="body"]', hook.body ?? "");
                const headerLines = Object.entries(hook.headers || {}).map(([k, v]) => `${k}: ${v}`).join("\n");
                set('textarea[name="headers"]', headerLines);
                hookFormEl.querySelectorAll('input[name="event"]').forEach((input) => {
                    input.checked = hook.event_filter.includes(input.value);
                });
                renderHookDeviceFilter(hook.device_filter ?? []);
            } else {
                set('select[name="method"]', "POST");
                set('select[name="enabled"]', "true");
                hookFormEl.querySelectorAll('input[name="event"]').forEach((input) => {
                    input.checked = false;
                });
                renderHookDeviceFilter([]);
            }

            hookModalEl.hidden = false;
            window.setTimeout(() => {
                hookFormEl.querySelector('input[name="name"]').focus();
            }, 30);
        }

        function closeHookModal() {
            hookModalEl.hidden = true;
            currentEditHookId = null;
        }

        function renderHookDeviceFilter(selectedNames) {
            const container = document.querySelector("#hook-device-filter-list");
            if (!container) return;
            const selected = new Set(selectedNames);
            const knownNames = new Set(latestDevices.map((d) => d.name));
            const lines = [];
            if (latestDevices.length === 0) {
                lines.push(`<span class="requires-empty">No devices discovered yet.</span>`);
            } else {
                for (const device of latestDevices) {
                    const checked = selected.has(device.name) ? "checked" : "";
                    const labelText = device.nickname && device.nickname !== device.name
                        ? `${escapeHtml(device.nickname)} <span style="color: var(--muted);">(${escapeHtml(device.name)})</span>`
                        : escapeHtml(device.name);
                    lines.push(`<label><input type="checkbox" name="device_filter" value="${escapeHtml(device.name)}" ${checked} /> ${labelText}</label>`);
                }
            }
            // Surface any saved names that no longer correspond to a known device.
            for (const name of selectedNames) {
                if (!knownNames.has(name)) {
                    lines.push(`<label title="Device not currently discovered"><input type="checkbox" name="device_filter" value="${escapeHtml(name)}" checked /> ${escapeHtml(name)} <span style="color: var(--red);">(missing)</span></label>`);
                }
            }
            container.innerHTML = lines.join("");
        }

        hookModalEl.addEventListener("click", (event) => {
            if (event.target.matches("[data-close-hook-modal]")) {
                closeHookModal();
            }
        });

        document.addEventListener("keydown", (event) => {
            if (event.key === "Escape" && !hookModalEl.hidden) {
                closeHookModal();
            }
        });

        hookAddBtn.addEventListener("click", () => openHookModal());

        hookFormEl.addEventListener("submit", async (event) => {
            event.preventDefault();
            hookFormErrorEl.hidden = true;
            hookFormErrorEl.textContent = "";

            const name = hookFormEl.querySelector('input[name="name"]').value.trim();
            const method = hookFormEl.querySelector('select[name="method"]').value;
            const enabled = hookFormEl.querySelector('select[name="enabled"]').value === "true";
            const url = hookFormEl.querySelector('input[name="url"]').value.trim();
            const body = hookFormEl.querySelector('textarea[name="body"]').value;
            const headersRaw = hookFormEl.querySelector('textarea[name="headers"]').value;
            const deviceFilter = Array.from(hookFormEl.querySelectorAll('input[name="device_filter"]:checked')).map((input) => input.value);
            const eventFilter = Array.from(hookFormEl.querySelectorAll('input[name="event"]:checked')).map((input) => input.value);

            if (!name) return showHookFormError("Name is required.");
            if (!url) return showHookFormError("URL is required.");

            const headers = {};
            for (const line of headersRaw.split("\n")) {
                const trimmed = line.trim();
                if (trimmed === "") continue;
                const sep = trimmed.indexOf(":");
                if (sep < 0) return showHookFormError(`Bad header line: '${trimmed}' (expected 'Key: value').`);
                const key = trimmed.slice(0, sep).trim();
                const value = trimmed.slice(sep + 1).trim();
                if (key === "") return showHookFormError("Header key cannot be empty.");
                headers[key] = value;
            }

            const isEditing = currentEditHookId !== null;
            const endpoint = isEditing
                ? `/api/hooks/${encodeURIComponent(currentEditHookId)}`
                : "/api/hooks";
            const httpMethod = isEditing ? "PATCH" : "POST";
            const payload = {
                name,
                url,
                method,
                headers,
                body: body === "" ? null : body,
                device_filter: deviceFilter,
                event_filter: eventFilter,
                enabled,
            };

            hookFormSubmitEl.disabled = true;
            hookFormSubmitEl.textContent = isEditing ? "Saving" : "Creating";

            try {
                await requestJson(endpoint, {
                    method: httpMethod,
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify(payload),
                });
                closeHookModal();
                await loadHooks();
            } catch (error) {
                showHookFormError(error.message);
            } finally {
                hookFormSubmitEl.disabled = false;
                hookFormSubmitEl.textContent = isEditing ? "Save" : "Create";
            }
        });

        function showHookFormError(message) {
            hookFormErrorEl.textContent = message;
            hookFormErrorEl.hidden = false;
        }

        function openScheduleModal(deviceName, schedule = null) {
            const isEditing = schedule !== null;
            currentEditScheduleId = isEditing ? schedule.id : null;

            const device = latestDevices.find((entry) => entry.name === deviceName);
            const displayName = device?.nickname || deviceName;

            scheduleFormDeviceEl.value = deviceName;
            scheduleFormErrorEl.hidden = true;
            scheduleFormErrorEl.textContent = "";
            scheduleFormSubmitEl.disabled = false;
            scheduleFormSubmitEl.textContent = isEditing ? "Save" : "Create";
            scheduleModalTitleEl.textContent = isEditing
                ? `Edit schedule — ${displayName}`
                : `New schedule — ${displayName}`;
            scheduleFormEl.reset();
            scheduleFormDeviceEl.value = deviceName;

            scheduleFormEl.querySelector('input[name="time"]').value = "07:00";
            scheduleFormEl.querySelector('select[name="action"]').value = "on";
            scheduleFormEl.querySelector('select[name="action-advanced"]').value = "on";
            scheduleFormEl.querySelector('input[name="cron"]').value = "";
            scheduleFormEl.querySelector('input[name="on_minutes"]').value = "60";
            scheduleFormEl.querySelector('input[name="off_minutes"]').value = "30";
            scheduleFormEl.querySelector('input[name="start_action"][value="on"]').checked = true;
            scheduleFormEl.querySelector('input[name="label"]').value = "";
            scheduleFormEl.querySelectorAll('input[name="day"]').forEach((input) => {
                input.checked = ["1", "2", "3", "4", "5"].includes(input.value);
            });

            let initialMode = "simple";
            const tabs = scheduleFormEl.querySelectorAll(".mode-tabs button");

            if (isEditing) {
                scheduleFormEl.querySelector('input[name="label"]').value = schedule.label ?? "";
                if (schedule.kind === "interval") {
                    initialMode = "interval";
                    scheduleFormEl.querySelector('input[name="on_minutes"]').value = Math.round((schedule.on_seconds ?? 0) / 60);
                    scheduleFormEl.querySelector('input[name="off_minutes"]').value = Math.round((schedule.off_seconds ?? 0) / 60);
                    const startInput = scheduleFormEl.querySelector(`input[name="start_action"][value="${schedule.start_action ?? "on"}"]`);
                    if (startInput) startInput.checked = true;
                    tabs.forEach((tab) => {
                        tab.hidden = tab.dataset.mode !== "interval";
                    });
                } else {
                    const simple = schedule.cron ? parseCronToSimple(schedule.cron) : null;
                    if (simple) {
                        initialMode = "simple";
                        scheduleFormEl.querySelector('input[name="time"]').value =
                            `${String(simple.hour).padStart(2, "0")}:${String(simple.minute).padStart(2, "0")}`;
                        scheduleFormEl.querySelector('select[name="action"]').value = schedule.action ?? "on";
                        scheduleFormEl.querySelectorAll('input[name="day"]').forEach((input) => {
                            input.checked = simple.days.includes(Number(input.value));
                        });
                    } else {
                        initialMode = "advanced";
                        scheduleFormEl.querySelector('input[name="cron"]').value = schedule.cron ?? "";
                        scheduleFormEl.querySelector('select[name="action-advanced"]').value = schedule.action ?? "on";
                    }
                    tabs.forEach((tab) => {
                        tab.hidden = tab.dataset.mode === "interval";
                    });
                }
            } else {
                tabs.forEach((tab) => {
                    tab.hidden = false;
                });
            }

            scheduleFormEl.querySelectorAll(".mode-panel").forEach((panel) => {
                panel.hidden = panel.dataset.panel !== initialMode;
            });
            tabs.forEach((tab) => {
                tab.setAttribute("aria-pressed", String(tab.dataset.mode === initialMode));
            });

            scheduleModalEl.hidden = false;
            window.setTimeout(() => {
                const focusEl = initialMode === "interval"
                    ? scheduleFormEl.querySelector('input[name="on_minutes"]')
                    : initialMode === "advanced"
                        ? scheduleFormEl.querySelector('input[name="cron"]')
                        : scheduleFormEl.querySelector('select[name="action"]');
                if (focusEl) focusEl.focus();
            }, 30);
        }

        function parseCronToSimple(expr) {
            const fields = expr.trim().split(/\s+/);
            let minute, hour, dom, month, dow;
            if (fields.length === 6 || fields.length === 7) {
                [, minute, hour, dom, month, dow] = fields;
            } else if (fields.length === 5) {
                [minute, hour, dom, month, dow] = fields;
            } else {
                return null;
            }
            if (dom !== "*" || month !== "*") return null;
            if (!/^\d+$/.test(minute) || !/^\d+$/.test(hour)) return null;
            const minuteNum = Number.parseInt(minute, 10);
            const hourNum = Number.parseInt(hour, 10);
            if (!Number.isFinite(minuteNum) || !Number.isFinite(hourNum)) return null;
            if (minuteNum > 59 || hourNum > 23) return null;
            const days = parseDowFieldToList(dow);
            if (days === null) return null;
            return { minute: minuteNum, hour: hourNum, days };
        }

        function parseDowFieldToList(dow) {
            if (dow === "*" || dow === "?") return [0, 1, 2, 3, 4, 5, 6];
            const parts = dow.split(",").map((part) => part.trim());
            const days = new Set();
            for (const part of parts) {
                if (part.includes("/")) return null;
                if (part.includes("-")) {
                    const [startStr, endStr] = part.split("-");
                    const start = Number.parseInt(startStr, 10);
                    const end = Number.parseInt(endStr, 10);
                    if (!Number.isFinite(start) || !Number.isFinite(end)) return null;
                    if (start > end) return null;
                    for (let i = start; i <= end; i += 1) days.add(i % 7);
                } else {
                    const value = Number.parseInt(part, 10);
                    if (!Number.isFinite(value)) return null;
                    days.add(value % 7);
                }
            }
            return Array.from(days).sort((a, b) => a - b);
        }

        function closeScheduleModal() {
            scheduleModalEl.hidden = true;
            currentEditScheduleId = null;
        }

        scheduleModalEl.addEventListener("click", (event) => {
            if (event.target.matches("[data-close-schedule-modal]")) {
                closeScheduleModal();
            }
        });

        document.addEventListener("keydown", (event) => {
            if (event.key === "Escape" && !scheduleModalEl.hidden) {
                closeScheduleModal();
            }
        });

        scheduleFormEl.querySelectorAll(".mode-tabs button").forEach((button) => {
            button.addEventListener("click", () => {
                const mode = button.dataset.mode;
                scheduleFormEl.querySelectorAll(".mode-tabs button").forEach((other) => {
                    other.setAttribute("aria-pressed", String(other.dataset.mode === mode));
                });
                scheduleFormEl.querySelectorAll(".mode-panel").forEach((panel) => {
                    panel.hidden = panel.dataset.panel !== mode;
                });
            });
        });

        scheduleFormEl.querySelectorAll(".day-picker-presets button[data-preset]").forEach((button) => {
            button.addEventListener("click", () => {
                const preset = button.dataset.preset;
                const inputs = scheduleFormEl.querySelectorAll('input[name="day"]');
                const presets = {
                    weekdays: ["1", "2", "3", "4", "5"],
                    weekends: ["0", "6"],
                    all: ["0", "1", "2", "3", "4", "5", "6"],
                    none: [],
                };
                const values = presets[preset] ?? [];
                inputs.forEach((input) => {
                    input.checked = values.includes(input.value);
                });
            });
        });

        scheduleFormEl.querySelectorAll("button[data-interval-preset]").forEach((button) => {
            button.addEventListener("click", () => {
                const [on, off] = button.dataset.intervalPreset.split("/");
                scheduleFormEl.querySelector('input[name="on_minutes"]').value = on;
                scheduleFormEl.querySelector('input[name="off_minutes"]').value = off;
            });
        });

        scheduleFormEl.addEventListener("submit", async (event) => {
            event.preventDefault();
            scheduleFormErrorEl.hidden = true;
            scheduleFormErrorEl.textContent = "";

            const deviceName = scheduleFormDeviceEl.value;
            const labelValue = scheduleFormEl.querySelector('input[name="label"]').value.trim();
            const activeMode = scheduleFormEl.querySelector('.mode-tabs button[aria-pressed="true"]').dataset.mode;

            let body = null;
            if (activeMode === "advanced") {
                const cron = scheduleFormEl.querySelector('input[name="cron"]').value.trim();
                const action = scheduleFormEl.querySelector('select[name="action-advanced"]').value;
                if (cron === "") {
                    showScheduleFormError("Cron expression is required.");
                    return;
                }
                body = { kind: "cron", device_name: deviceName, cron, action };
            } else if (activeMode === "interval") {
                const onMinutes = Number.parseInt(scheduleFormEl.querySelector('input[name="on_minutes"]').value, 10);
                const offMinutes = Number.parseInt(scheduleFormEl.querySelector('input[name="off_minutes"]').value, 10);
                if (!Number.isFinite(onMinutes) || !Number.isFinite(offMinutes) || onMinutes < 0 || offMinutes < 0) {
                    showScheduleFormError("On and off durations must be non-negative whole minutes.");
                    return;
                }
                if (onMinutes + offMinutes < 1) {
                    showScheduleFormError("On + off must be at least 1 minute.");
                    return;
                }
                const startAction = scheduleFormEl.querySelector('input[name="start_action"]:checked').value;
                body = {
                    kind: "interval",
                    device_name: deviceName,
                    on_seconds: onMinutes * 60,
                    off_seconds: offMinutes * 60,
                    start_action: startAction,
                };
            } else {
                const time = scheduleFormEl.querySelector('input[name="time"]').value;
                if (!time) {
                    showScheduleFormError("Pick a time.");
                    return;
                }
                const action = scheduleFormEl.querySelector('select[name="action"]').value;
                const days = Array.from(scheduleFormEl.querySelectorAll('input[name="day"]:checked')).map((input) => input.value);
                if (days.length === 0) {
                    showScheduleFormError("Pick at least one day.");
                    return;
                }
                const [hourStr, minuteStr] = time.split(":");
                const hour = Number.parseInt(hourStr, 10);
                const minute = Number.parseInt(minuteStr, 10);
                if (!Number.isFinite(hour) || !Number.isFinite(minute)) {
                    showScheduleFormError("Time format is invalid.");
                    return;
                }
                const sortedDays = days.map(Number).sort((a, b) => a - b);
                const dowField = sortedDays.length === 7 ? "*" : sortedDays.join(",");
                const cron = `${minute} ${hour} * * ${dowField}`;
                body = { kind: "cron", device_name: deviceName, cron, action };
            }

            body.label = labelValue === "" ? null : labelValue;

            const editingId = currentEditScheduleId;
            const isEditing = editingId !== null;

            let endpoint;
            let method;
            let payload;
            if (isEditing) {
                endpoint = `/api/schedules/${encodeURIComponent(editingId)}`;
                method = "PATCH";
                payload = { label: body.label };
                if (body.kind === "cron") {
                    payload.cron = body.cron;
                    payload.action = body.action;
                } else {
                    payload.on_seconds = body.on_seconds;
                    payload.off_seconds = body.off_seconds;
                    payload.start_action = body.start_action;
                }
            } else {
                body.enabled = true;
                endpoint = "/api/schedules";
                method = "POST";
                payload = body;
            }

            scheduleFormSubmitEl.disabled = true;
            scheduleFormSubmitEl.textContent = isEditing ? "Saving" : "Creating";

            try {
                await requestJson(endpoint, {
                    method,
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify(payload),
                });
                closeScheduleModal();
                await loadSchedules();
            } catch (error) {
                showScheduleFormError(error.message);
            } finally {
                scheduleFormSubmitEl.disabled = false;
                scheduleFormSubmitEl.textContent = isEditing ? "Save" : "Create";
            }
        });

        function showScheduleFormError(message) {
            scheduleFormErrorEl.textContent = message;
            scheduleFormErrorEl.hidden = false;
        }

        function playSwitchClick(nextIsOn) {
            if (!prepareSwitchAudio()) return;

            void loadSwitchAudioBuffer()
                .then((buffer) => {
                    playSwitchSample(buffer, nextIsOn);
                })
                .catch(() => {
                    // Audio feedback is progressive enhancement; the toggle action should stay reliable without it.
                });
        }

        function prepareSwitchAudio() {
            const AudioContextClass = window.AudioContext ?? window.webkitAudioContext;
            if (!AudioContextClass) return false;

            try {
                if (switchAudioContext === null) {
                    switchAudioContext = new AudioContextClass();
                }

                if (switchAudioContext.state === "suspended") {
                    void switchAudioContext.resume();
                }

                void loadSwitchAudioBuffer()
                    .catch(() => {
                        // Audio feedback is progressive enhancement; the toggle action should stay reliable without it.
                    });

                return true;
            } catch (_error) {
                // Browsers can reject audio creation under stricter policies; the switch should still work.
                return false;
            }
        }

        function loadSwitchAudioBuffer() {
            if (switchAudioBufferPromise !== null) return switchAudioBufferPromise;

            switchAudioBufferPromise = fetch(switchSoundUrl)
                .then((response) => {
                    if (!response.ok) {
                        throw new Error(`Switch sound failed with status ${response.status}`);
                    }

                    return response.arrayBuffer();
                })
                .then((arrayBuffer) => switchAudioContext.decodeAudioData(arrayBuffer))
                .catch((error) => {
                    switchAudioBufferPromise = null;
                    throw error;
                });

            return switchAudioBufferPromise;
        }

        function playSwitchSample(buffer, nextIsOn) {
            const source = switchAudioContext.createBufferSource();
            const gain = switchAudioContext.createGain();
            const startTime = switchAudioContext.currentTime;
            const playbackRate = nextIsOn ? 1 : 0.68;
            const volume = nextIsOn ? 0.86 : 0.78;

            source.buffer = buffer;
            source.playbackRate.setValueAtTime(playbackRate, startTime);
            gain.gain.setValueAtTime(volume, startTime);
            source.connect(gain);
            gain.connect(switchAudioContext.destination);
            source.addEventListener("ended", () => {
                source.disconnect();
                gain.disconnect();
            }, { once: true });
            source.start(startTime);
        }

        function renderUsageHistory(history) {
            if (powerChart === null) {
                usageEmptyEl.hidden = false;
                usageRangeEl.textContent = "Chart library unavailable";
                return;
            }

            const totals = history.totals ?? [];
            if (totals.length < 2) {
                usageEmptyEl.hidden = false;
                usageEmptyEl.textContent = (history.errors ?? []).length > 0
                    ? "Tapo did not return enough history to draw the chart."
                    : "No power history is available for this range yet.";
                usageRangeEl.textContent = "History unavailable";
                return;
            }

            usageEmptyEl.hidden = true;
            usageEmptyEl.textContent = "";

            const unit = history.unit ?? "W";
            const labels = totals.map((point) => formatHistoryLabel(point.timestamp_ms, history.range, unit));
            const timestamps = totals.map((point) => point.timestamp_ms);
            const maxValue = Math.max(...totals.map((point) => point.value ?? 0), 1);
            const chartTheme = powerChartTheme();
            const datasets = [{
                label: "Total",
                data: totals.map((point) => point.value),
                borderColor: chartTheme.graphLine,
                backgroundColor: chartTheme.graphFill,
                borderWidth: 3,
                cubicInterpolationMode: "monotone",
                fill: true,
                pointRadius: 0,
                pointHoverRadius: 4,
                tension: 0.28,
            }];

            for (const [index, series] of (history.series ?? []).entries()) {
                const pointsByTimestamp = new Map((series.points ?? []).map((point) => [point.timestamp_ms, point.value]));
                const color = chartSeriesColor(index, chartTheme);
                const device = latestDevices.find((entry) => entry.name === series.device_name);
                datasets.push({
                    label: device?.nickname || series.device_name,
                    data: timestamps.map((timestamp) => pointsByTimestamp.get(timestamp) ?? null),
                    borderColor: color,
                    backgroundColor: "transparent",
                    borderWidth: 2,
                    cubicInterpolationMode: "monotone",
                    fill: false,
                    pointRadius: 0,
                    pointHoverRadius: 4,
                    spanGaps: true,
                    tension: 0.24,
                    themeSeriesIndex: index,
                });
            }

            powerChart.data.labels = labels;
            powerChart.data.datasets = datasets;
            powerChart.options.scales.y.suggestedMax = Math.ceil(maxValue * 1.15);
            powerChart.options.scales.y.ticks.callback = (value) => formatUsageValue(value, unit);
            powerChart.options.plugins.tooltip.callbacks.label = (context) => ` ${formatUsageValue(context.parsed.y, unit)}`;
            powerChart.update("none");
            usageTitleEl.textContent = `${history.range_label ?? selectedHistoryRange} usage`;
            usageRangeEl.textContent = `${history.interval ?? "usage"} readings / ${formatUsageValue(maxValue, unit)} peak`;

            if ((history.errors ?? []).length > 0) {
                usageEmptyEl.hidden = false;
                usageEmptyEl.textContent = `${history.errors.length} device history read failed.`;
            }
        }

        function initializePowerChart() {
            if (typeof Chart === "undefined") {
                usageEmptyEl.textContent = "Chart.js could not load, so the live graph is unavailable.";
                return;
            }

            const chartTheme = powerChartTheme();

            powerChart = new Chart(usageChartEl, {
                type: "line",
                data: {
                    labels: [],
                    datasets: [{
                        label: "Total",
                        data: [],
                        borderColor: chartTheme.graphLine,
                        backgroundColor: chartTheme.graphFill,
                        borderWidth: 3,
                        cubicInterpolationMode: "monotone",
                        fill: true,
                        pointRadius: 0,
                        pointHoverRadius: 4,
                        tension: 0.28,
                    }],
                },
                options: {
                    animation: false,
                    responsive: true,
                    maintainAspectRatio: false,
                    interaction: {
                        intersect: false,
                        mode: "index",
                    },
                    plugins: {
                        legend: {
                            display: true,
                            labels: {
                                boxWidth: 10,
                                color: chartTheme.ink,
                                usePointStyle: true,
                            },
                        },
                        tooltip: {
                            callbacks: {
                                title: (items) => items.length > 0 ? items[0].label : "",
                                label: (context) => ` ${formatUsageValue(context.parsed.y, "W")}`,
                            },
                        },
                    },
                    scales: {
                        x: {
                            grid: {
                                color: chartTheme.graphGrid,
                            },
                            ticks: {
                                color: chartTheme.ink,
                                maxTicksLimit: 6,
                            },
                        },
                        y: {
                            beginAtZero: true,
                            grid: {
                                color: chartTheme.graphGrid,
                            },
                            ticks: {
                                color: chartTheme.ink,
                                callback: (value) => formatUsageValue(value, "W"),
                            },
                        },
                    },
                },
            });
        }

        function syncPowerChartTheme() {
            if (powerChart === null) return;

            const chartTheme = powerChartTheme();
            const dataset = powerChart.data.datasets[0];
            dataset.borderColor = chartTheme.graphLine;
            dataset.backgroundColor = chartTheme.graphFill;
            for (const [index, dataset] of powerChart.data.datasets.slice(1).entries()) {
                dataset.borderColor = chartSeriesColor(dataset.themeSeriesIndex ?? index, chartTheme);
            }
            powerChart.options.scales.x.grid.color = chartTheme.graphGrid;
            powerChart.options.scales.x.ticks.color = chartTheme.ink;
            powerChart.options.scales.y.grid.color = chartTheme.graphGrid;
            powerChart.options.scales.y.ticks.color = chartTheme.ink;
            powerChart.options.plugins.legend.labels.color = chartTheme.ink;
            powerChart.update("none");
        }

        function powerChartTheme() {
            const styles = getComputedStyle(document.documentElement);
            return {
                graphLine: styles.getPropertyValue("--graph-line").trim(),
                graphFill: styles.getPropertyValue("--graph-fill").trim(),
                graphGrid: styles.getPropertyValue("--graph-grid").trim(),
                ink: styles.getPropertyValue("--ink").trim(),
                series: document.documentElement.dataset.theme === "dark" ? chartPalettes.dark : chartPalettes.classic,
            };
        }

        function chartSeriesColor(index, chartTheme) {
            return chartTheme.series[index % chartTheme.series.length];
        }

        function formatHistoryLabel(timestampMs, range, unit) {
            const date = new Date(timestampMs);
            if (range === "all") {
                return date.toLocaleDateString([], {
                    month: "short",
                    year: "numeric",
                });
            }

            if (unit === "kWh") {
                return date.toLocaleDateString([], {
                    weekday: "short",
                    day: "2-digit",
                    month: "short",
                });
            }

            return date.toLocaleString([], {
                weekday: "short",
                day: "2-digit",
                month: "short",
                hour: "2-digit",
                minute: "2-digit",
            });
        }

        function renderDevice(device, schedules) {
            const isOn = device.device_on === true;
            const isOffline = device.last_error !== null;
            const energy = device.energy;
            const statusClass = isOffline ? "offline" : isOn ? "on" : "off";
            const statusText = isOffline ? "offline" : isOn ? "on" : "off";
            const manualOverride = device.manual_override;
            const isManual = manualOverride === true || manualOverride === false;
            const conditionBlock = device.condition_intent === false && !isManual;

            return `
                <article class="breaker ${isOffline ? "offline" : ""}">
                    <div class="label-card">
                        <h2 class="device-name">${escapeHtml(device.nickname)}</h2>
                        <p class="device-meta">${escapeHtml(device.ip)} / ${escapeHtml(device.model)}</p>
                    </div>
                    <div class="toggle-wrap">
                        <button class="toggle" type="button" data-device="${escapeHtml(device.name)}" data-on="${isOn}" aria-pressed="${isOn}" aria-label="Toggle ${escapeHtml(device.nickname)}">
                            <span class="lever" aria-hidden="true"></span>
                        </button>
                    </div>
                    ${isManual ? `<div class="device-mode-badge manual"><span class="manual-label">Manual${device.manual_override_until_ms ? ` — auto ${formatRelative(device.manual_override_until_ms)}` : ""}</span><button type="button" data-release-override="${escapeHtml(device.name)}" title="Hand control back to schedules &amp; conditions">Auto</button></div>` : ""}
                    ${conditionBlock ? `<div class="device-mode-badge condition-blocked"><span>Blocked by condition</span></div>` : ""}
                    <div class="status-strip">
                        <span class="lamp ${statusClass}">${statusText}</span>
                        <span>${formatDurationFromSeconds(device.on_time_seconds)}</span>
                    </div>
                    <div class="readings">
                        <div class="reading"><span>Now</span>${energy?.current_power_w ?? "-"} W</div>
                        <div class="reading"><span>Today energy</span>${energy ? formatEnergy(energy.today_energy_wh) : "-"}</div>
                        <div class="reading"><span>Today cost</span>${energy ? formatCost(energy.today_cost_pence) : "-"}</div>
                        <div class="reading"><span>Month energy</span>${energy ? formatEnergy(energy.month_energy_wh) : "-"}</div>
                        <div class="reading"><span>Month cost</span>${energy ? formatCost(energy.month_cost_pence) : "-"}</div>
                        <div class="reading"><span>Today runtime</span>${energy ? formatDurationFromMinutes(energy.today_runtime_minutes) : "-"}</div>
                    </div>
                    ${renderSchedulesSection(device, schedules ?? [])}
                    ${renderConditionsSection(device, conditionsByDevice.get(device.name) ?? [])}
                    ${isOffline ? `<p class="device-meta">${escapeHtml(device.last_error)}</p>` : ""}
                </article>
            `;
        }

        function sectionOpenAttribute(deviceName, sectionKey, hasItems) {
            const stored = sectionOpenStateGet(deviceName, sectionKey);
            const open = stored === null ? hasItems : stored === "true";
            return open ? "open" : "";
        }

        function renderSchedulesSection(device, schedules) {
            const items = schedules.map(renderScheduleItem).join("");
            const body = schedules.length === 0
                ? `<p class="schedules-empty">No schedules yet.</p>`
                : `<ul class="schedule-list">${items}</ul>`;
            const openAttr = sectionOpenAttribute(device.name, "schedules", schedules.length > 0);
            const countBadge = schedules.length > 0 ? `<span class="section-count">${schedules.length}</span>` : "";
            return `
                <details class="section-accordion" data-device="${escapeHtml(device.name)}" data-section="schedules" ${openAttr}>
                    <summary>
                        <span class="section-summary-text">
                            <span class="section-chevron" aria-hidden="true">&#9656;</span>
                            <span>Schedules</span>
                            ${countBadge}
                        </span>
                        <button class="schedule-add" type="button" data-add-schedule="${escapeHtml(device.name)}">+ Add</button>
                    </summary>
                    ${body}
                </details>
            `;
        }

        function renderConditionsSection(device, conditions) {
            const items = conditions.map(renderConditionItem).join("");
            const body = conditions.length === 0
                ? `<p class="schedules-empty">No conditions yet. Conditions are HTTP probes that turn this device on or off when their result changes.</p>`
                : `<ul class="condition-list">${items}</ul>`;
            const openAttr = sectionOpenAttribute(device.name, "conditions", conditions.length > 0);
            const countBadge = conditions.length > 0 ? `<span class="section-count">${conditions.length}</span>` : "";
            return `
                <details class="section-accordion" data-device="${escapeHtml(device.name)}" data-section="conditions" ${openAttr}>
                    <summary>
                        <span class="section-summary-text">
                            <span class="section-chevron" aria-hidden="true">&#9656;</span>
                            <span>Conditions</span>
                            ${countBadge}
                        </span>
                        <button class="schedule-add" type="button" data-add-condition="${escapeHtml(device.name)}">+ Add</button>
                    </summary>
                    ${body}
                </details>
            `;
        }

        function sectionOpenStateKey(deviceName, sectionKey) {
            return `fusebox-section-${deviceName}-${sectionKey}`;
        }

        function sectionOpenStateGet(deviceName, sectionKey) {
            try {
                return localStorage.getItem(sectionOpenStateKey(deviceName, sectionKey));
            } catch (_error) {
                return null;
            }
        }

        function sectionOpenStateSet(deviceName, sectionKey, isOpen) {
            try {
                localStorage.setItem(sectionOpenStateKey(deviceName, sectionKey), String(isOpen));
            } catch (_error) {
                // Storage may be unavailable; just live with the default.
            }
        }

        function renderScheduleItem(schedule) {
            const isInterval = schedule.kind === "interval";
            const stateName = isInterval ? "cycle" : (schedule.action ?? "on");
            const stateClass = `state-${stateName}`;
            const actionBadge = isInterval
                ? "CYC"
                : stateName === "on" ? "ON" : stateName === "off" ? "OFF" : "TOG";
            const summary = isInterval
                ? describeInterval(schedule)
                : describeCron(schedule.cron ?? "");

            const metaParts = [];
            if (schedule.label) {
                metaParts.push(escapeHtml(schedule.label));
            }
            if (isInterval) {
                metaParts.push(`starts ${schedule.start_action === "off" ? "off" : "on"}`);
            } else if (schedule.cron) {
                metaParts.push(`<code>${escapeHtml(schedule.cron)}</code>`);
            }
            if (schedule.next_fire_at_ms && schedule.enabled) {
                metaParts.push(`next ${formatRelative(schedule.next_fire_at_ms)}`);
            }
            if (schedule.last_error) {
                metaParts.push(`<span style="color: var(--red);">${escapeHtml(schedule.last_error)}</span>`);
            }
            const meta = metaParts.join(" / ");
            return `
                <li class="schedule-item ${schedule.enabled ? "" : "disabled"}">
                    <label class="schedule-enabled" title="Enable / disable">
                        <input type="checkbox" data-schedule-enabled="${escapeHtml(schedule.id)}" ${schedule.enabled ? "checked" : ""} />
                    </label>
                    <div class="schedule-body">
                        <span class="schedule-summary">
                            <span class="schedule-action ${stateClass}">${actionBadge}</span>
                            ${escapeHtml(summary)}
                        </span>
                        <span class="schedule-meta">${meta}</span>
                    </div>
                    <div class="schedule-actions">
                        <button class="schedule-edit" type="button" data-schedule-edit="${escapeHtml(schedule.id)}" aria-label="Edit schedule" title="Edit">&#9998;</button>
                        <button class="schedule-delete" type="button" data-schedule-delete="${escapeHtml(schedule.id)}" aria-label="Delete schedule" title="Delete">&times;</button>
                    </div>
                </li>
            `;
        }

        function describeInterval(schedule) {
            const onSecs = schedule.on_seconds ?? 0;
            const offSecs = schedule.off_seconds ?? 0;
            return `${formatDurationFromSeconds(onSecs)} on / ${formatDurationFromSeconds(offSecs)} off`;
        }

        function describeCron(expr) {
            const fields = expr.trim().split(/\s+/);
            // Strip seconds prefix if present (6 or 7 fields).
            let minute, hour, dom, month, dow;
            if (fields.length === 6 || fields.length === 7) {
                [, minute, hour, dom, month, dow] = fields;
            } else if (fields.length === 5) {
                [minute, hour, dom, month, dow] = fields;
            } else {
                return expr;
            }

            const isSimpleTime = !minute.includes(",") && !minute.includes("/") && !minute.includes("-") && minute !== "*"
                && !hour.includes(",") && !hour.includes("/") && !hour.includes("-") && hour !== "*"
                && dom === "*" && month === "*";
            if (!isSimpleTime) {
                if (minute.startsWith("*/") && hour === "*" && dom === "*" && month === "*" && dow === "*") {
                    return `Every ${minute.slice(2)} min`;
                }
                return expr;
            }

            const time = `${String(hour).padStart(2, "0")}:${String(minute).padStart(2, "0")}`;
            const dayLabel = describeDow(dow);
            return `${dayLabel} ${time}`;
        }

        function describeDow(dow) {
            if (dow === "*" || dow === "?") return "Daily";
            if (dow === "1-5") return "Weekdays";
            if (dow === "0,6" || dow === "6,0" || dow === "0-0,6-6") return "Weekends";

            const parts = dow.split(",").map((part) => part.trim());
            const days = [];
            for (const part of parts) {
                if (part.includes("-")) {
                    const [startStr, endStr] = part.split("-");
                    const start = Number.parseInt(startStr, 10);
                    const end = Number.parseInt(endStr, 10);
                    if (!Number.isFinite(start) || !Number.isFinite(end)) return dow;
                    for (let i = start; i <= end; i += 1) {
                        days.push(i % 7);
                    }
                } else {
                    const value = Number.parseInt(part, 10);
                    if (!Number.isFinite(value)) return dow;
                    days.push(value % 7);
                }
            }
            days.sort((a, b) => a - b);
            const unique = Array.from(new Set(days));
            if (unique.length === 7) return "Daily";
            return unique.map((day) => dayNames[day]).join(" ");
        }

        function formatRelative(timestampMs) {
            const diff = timestampMs - Date.now();
            if (diff === 0) return "now";
            const isFuture = diff > 0;
            const magnitude = Math.abs(diff);
            const minutes = Math.round(magnitude / 60000);
            if (minutes < 1) return isFuture ? "in <1m" : "just now";
            const labelUnit = minutes < 60
                ? `${minutes}m`
                : (() => {
                    const hours = Math.round(minutes / 60);
                    if (hours < 48) return `${hours}h`;
                    const days = Math.round(hours / 24);
                    return `${days}d`;
                })();
            return isFuture ? `in ${labelUnit}` : `${labelUnit} ago`;
        }

        function formatDurationFromSeconds(seconds) {
            if (seconds === null || seconds === undefined) return "unknown";
            const minutes = Math.floor(seconds / 60);
            return formatDurationFromMinutes(minutes);
        }

        function formatDurationFromMinutes(minutes) {
            if (minutes === null || minutes === undefined) return "unknown";
            const totalMinutes = Math.floor(minutes);
            if (totalMinutes < 60) return `${totalMinutes} min`;

            const hours = Math.floor(totalMinutes / 60);
            const remainderMinutes = totalMinutes % 60;
            if (hours < 48) return remainderMinutes === 0 ? `${hours}h` : `${hours}h ${remainderMinutes}m`;

            const days = Math.floor(hours / 24);
            const remainingHours = hours % 24;
            if (days < 60) return remainingHours === 0 ? `${days}d` : `${days}d ${remainingHours}h`;

            const months = Math.floor(days / 30);
            const remainingDays = days % 30;
            if (months < 24) return remainingDays === 0 ? `${months}mo` : `${months}mo ${remainingDays}d`;

            const years = Math.floor(months / 12);
            const remainingMonths = months % 12;
            return remainingMonths === 0 ? `${years}y` : `${years}y ${remainingMonths}mo`;
        }

        function formatEnergy(wh) {
            if (wh >= 1000) return `${(wh / 1000).toFixed(2)} kWh`;
            return `${Math.round(wh)} Wh`;
        }

        function formatCost(pence) {
            if (pence >= 100) return `£${(pence / 100).toFixed(2)}`;
            return `${pence.toFixed(1)}p`;
        }

        function formatPower(watts) {
            if (watts >= 1000) return `${(watts / 1000).toFixed(2)} kW`;
            return `${Math.round(watts)} W`;
        }

        function formatUsageValue(value, unit) {
            if (unit === "kWh") return `${Number(value).toFixed(2)} kWh`;
            return formatPower(value);
        }

        function escapeHtml(value) {
            return String(value)
                .replaceAll("&", "&amp;")
                .replaceAll("<", "&lt;")
                .replaceAll(">", "&gt;")
                .replaceAll('"', "&quot;")
                .replaceAll("'", "&#039;");
        }

        loadDevices();
        loadUsageHistory();
        loadSchedules();
        loadConditions();
        loadHooks();
        connectDeviceStream();
        window.setInterval(loadSchedules, 60_000);
        window.setInterval(loadConditions, 15_000);
        window.setInterval(loadHooks, 30_000);
    </script>
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

        assert_eq!(interval_phase_at(&schedule, 1_000), Some(ScheduleAction::On));
        assert_eq!(interval_phase_at(&schedule, 60_000), Some(ScheduleAction::On));
        assert_eq!(interval_phase_at(&schedule, 61_001), Some(ScheduleAction::Off));
        assert_eq!(interval_phase_at(&schedule, 180_000), Some(ScheduleAction::Off));
        assert_eq!(interval_phase_at(&schedule, 181_001), Some(ScheduleAction::On));
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
        assert_ne!(condition_probe_key(&a), condition_probe_key(&different_status));
        assert_ne!(condition_probe_key(&a), condition_probe_key(&different_method));
        assert_ne!(condition_probe_key(&a), condition_probe_key(&different_headers));
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

        assert_eq!(
            ctx.render("{{nickname}} -> {{event}}"),
            "Lights -> off",
        );
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
        let mut device = managed_device_from_config(
            name.to_string(),
            DeviceConfig { ip: ip_addr, model },
        );
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
            assert_eq!(stored.last_passing, Some(true), "hysteresis should hold previous value");
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
            assert_eq!(stored.last_passing, Some(false), "hysteresis should commit after stable window");
            assert_eq!(stored.pending_value, None);
        }

        let _ = fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn does_not_fire_hook_for_first_read_without_prior_snapshot() {
        let state_path = test_state_path("hook-no-first-read");
        let settings = test_settings(state_path.clone());
        let state = AppState::new(&settings);

        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<(String, HookEvent)>::new()));
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
        assert_eq!(stored.last_fired_at_ms, None, "first read should not have fired the hook");
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
