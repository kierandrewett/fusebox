use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Datelike, Days, Duration as ChronoDuration, NaiveDate, Utc};
use rust_xlsxwriter::{Format, FormatAlign, FormatBorder, Workbook};
use serde::{Deserialize, Serialize};
use tapo::{ApiClient, requests::EnergyDataInterval, requests::PowerDataInterval};
use tapoctl::{
    Config as TapoConfig, DeviceConfig, DeviceModel, DeviceSnapshot, TapoController,
    TapoCredentials, discovery_add_candidates,
};
use tokio::sync::{Mutex, RwLock, watch};
use tokio::time::sleep;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const STATE_VERSION: u32 = 1;
const DEFAULT_ENERGY_PRICE_PENCE_PER_KWH: f64 = 27.03;

#[derive(Debug, Clone)]
struct Settings {
    bind_address: SocketAddr,
    username: String,
    password: String,
    refresh_seconds: u64,
    scan_seconds: u64,
    discovery_timeout_seconds: u64,
    energy_price_pence_per_kwh: f64,
    state_path: PathBuf,
}

#[derive(Debug, Clone)]
struct AppState {
    controller: TapoController,
    credentials: TapoCredentials,
    devices: Arc<RwLock<BTreeMap<String, ManagedDevice>>>,
    device_locks: Arc<RwLock<BTreeMap<IpAddr, Arc<Mutex<()>>>>>,
    device_events: watch::Sender<DeviceListResponse>,
    discovery_timeout_seconds: u64,
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
}

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
    power_w: f64,
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
    duration: ChronoDuration,
    range_limit: ChronoDuration,
    interval: PowerExportInterval,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedState {
    version: u32,
    devices: BTreeMap<String, DeviceConfig>,
}

#[derive(Debug, Clone, Serialize)]
struct ErrorResponse {
    error: ApiErrorBody,
}

#[derive(Debug, Clone, Serialize)]
struct ApiErrorBody {
    code: &'static str,
    message: String,
}

struct AppError(anyhow::Error);

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

    let app = Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(favicon))
        .route("/health", get(health))
        .route("/api/devices", get(list_devices))
        .route("/api/energy/history.json", get(energy_history))
        .route("/api/energy/export.xlsx", get(export_energy_workbook))
        .route("/ws/devices", get(devices_websocket))
        .route("/api/scan", post(scan_devices))
        .route("/api/devices/{name}/toggle", post(toggle_device))
        .route("/api/devices/{name}/power", post(set_device_power))
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

impl Settings {
    fn from_env() -> Result<Self> {
        let bind_address = std::env::var("FUSEBOX_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8787".to_string())
            .parse::<SocketAddr>()
            .context("FUSEBOX_BIND must be a socket address, for example 127.0.0.1:8787")?;
        let username = required_env("TAPO_USERNAME")?;
        let password = required_env("TAPO_PASSWORD")?;
        let refresh_seconds = optional_u64_env("FUSEBOX_REFRESH_SECONDS", 10)?;
        let scan_seconds = optional_u64_env("FUSEBOX_SCAN_SECONDS", 60)?;
        let discovery_timeout_seconds = optional_u64_env("FUSEBOX_DISCOVERY_TIMEOUT_SECONDS", 5)?;
        let energy_price_pence_per_kwh = optional_f64_env(
            "FUSEBOX_ENERGY_PRICE_PENCE_PER_KWH",
            DEFAULT_ENERGY_PRICE_PENCE_PER_KWH,
        )?;
        let state_path = optional_path_env("FUSEBOX_STATE_PATH")
            .unwrap_or(default_state_path().context("failed to resolve default state path")?);

        if !(1..=60).contains(&discovery_timeout_seconds) {
            return Err(anyhow!(
                "FUSEBOX_DISCOVERY_TIMEOUT_SECONDS must be between 1 and 60"
            ));
        }

        if !(10..=3600).contains(&scan_seconds) {
            return Err(anyhow!("FUSEBOX_SCAN_SECONDS must be between 10 and 3600"));
        }

        if !energy_price_pence_per_kwh.is_finite()
            || !(0.0..=1000.0).contains(&energy_price_pence_per_kwh)
        {
            return Err(anyhow!(
                "FUSEBOX_ENERGY_PRICE_PENCE_PER_KWH must be between 0 and 1000",
            ));
        }

        Ok(Self {
            bind_address,
            username,
            password,
            refresh_seconds,
            scan_seconds,
            discovery_timeout_seconds,
            energy_price_pence_per_kwh,
            state_path,
        })
    }
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

        Self {
            controller,
            credentials,
            devices: Arc::new(RwLock::new(BTreeMap::new())),
            device_locks: Arc::new(RwLock::new(BTreeMap::new())),
            device_events,
            discovery_timeout_seconds: settings.discovery_timeout_seconds,
            refresh_seconds: settings.refresh_seconds,
            scan_seconds: settings.scan_seconds,
            energy_price_pence_per_kwh: settings.energy_price_pence_per_kwh,
            state_path: settings.state_path.clone(),
        }
    }
}

impl ManagedDevice {
    fn view(&self, energy_price_pence_per_kwh: f64) -> DeviceView {
        let snapshot = self.snapshot.as_ref();

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
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = ErrorResponse {
            error: ApiErrorBody {
                code: "FUSEBOX_ERROR",
                message: self.0.to_string(),
            },
        };

        (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(error: anyhow::Error) -> Self {
        Self(error)
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
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
) -> Result<Json<DeviceView>, AppError> {
    let device = get_device_config(&state, &name).await?;
    let operation_lock = device_operation_lock(&state, &device).await;
    let _operation_guard = operation_lock.lock().await;
    let snapshot = state.controller.toggle_power(&device).await?;
    update_device_snapshot(&state, &name, snapshot, None).await;

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
    let device = get_device_config(&state, &name).await?;
    let operation_lock = device_operation_lock(&state, &device).await;
    let _operation_guard = operation_lock.lock().await;
    state.controller.set_power(&device, request.on).await?;
    let snapshot = state.controller.read_device(&device).await?;
    update_device_snapshot(&state, &name, snapshot, None).await;

    get_device_view(&state, &name)
        .await
        .map(Json)
        .map_err(AppError)
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
    let discovered = state
        .controller
        .discover(&[], &[], state.discovery_timeout_seconds)
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

    let persisted: PersistedState =
        serde_json::from_str(&contents).context("failed to parse persisted state")?;

    if persisted.version != STATE_VERSION {
        return Err(anyhow!(
            "unsupported state version {}; expected {}",
            persisted.version,
            STATE_VERSION,
        ));
    }

    let loaded_count = persisted.devices.len();
    let mut devices = state.devices.write().await;

    for (name, config) in persisted.devices {
        devices.insert(name.clone(), managed_device_from_config(name, config));
    }

    info!(loaded_count, path = %state.state_path.display(), "loaded persisted devices");
    Ok(())
}

async fn save_persisted_state(state: &AppState) -> Result<()> {
    let persisted = {
        let devices = state.devices.read().await;

        PersistedState {
            version: STATE_VERSION,
            devices: devices
                .iter()
                .map(|(name, device)| (name.clone(), device.config.clone()))
                .collect(),
        }
    };

    write_json_atomically(&state.state_path, &persisted)
}

fn managed_device_from_config(name: String, config: DeviceConfig) -> ManagedDevice {
    ManagedDevice {
        name,
        config,
        snapshot: None,
        last_error: None,
        discovered_at_ms: now_ms(),
        updated_at_ms: None,
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

    match state.controller.read_device(&device).await {
        Ok(snapshot) => update_device_snapshot(state, name, snapshot, None).await,
        Err(error) => update_device_error(state, name, error.to_string()).await,
    }
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
) {
    {
        let mut devices = state.devices.write().await;

        if let Some(device) = devices.get_mut(name) {
            device.snapshot = Some(snapshot);
            device.last_error = last_error;
            device.updated_at_ms = Some(now_ms());
        }
    }

    publish_device_list(state, None).await;
}

async fn update_device_error(state: &AppState, name: &str, error: String) {
    {
        let mut devices = state.devices.write().await;

        if let Some(device) = devices.get_mut(name) {
            device.last_error = Some(error);
            device.updated_at_ms = Some(now_ms());
        }
    }

    publish_device_list(state, None).await;
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
    let devices = state.devices.read().await;

    devices
        .values()
        .map(|device| device.view(state.energy_price_pence_per_kwh))
        .collect()
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
    let devices = state.devices.read().await;

    devices
        .get(name)
        .map(|device| device.view(state.energy_price_pence_per_kwh))
        .ok_or_else(|| anyhow!("device '{name}' was not found"))
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} must be set"))
}

fn optional_u64_env(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .with_context(|| format!("{name} must be an integer")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("failed to read {name}")),
    }
}

fn optional_f64_env(name: &str, default: f64) -> Result<f64> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<f64>()
            .with_context(|| format!("{name} must be a number")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("failed to read {name}")),
    }
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
    let start = now.checked_sub_signed(range.duration).unwrap_or(now);
    let ranges = split_datetime_ranges(start, now, range.range_limit);
    let mut series = Vec::with_capacity(devices.len());
    let mut totals_by_timestamp: BTreeMap<DateTime<Utc>, f64> = BTreeMap::new();
    let mut errors = Vec::new();

    for device in devices {
        match read_power_entries(state, &device.config, &ranges, range.interval).await {
            Ok(entries) => {
                let mut points = Vec::new();

                for (timestamp, power) in entries {
                    if let Some(power_w) = power {
                        points.push(UsageHistoryPoint {
                            timestamp_ms: timestamp.timestamp_millis(),
                            power_w,
                        });
                        *totals_by_timestamp.entry(timestamp).or_default() += power_w;
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
        .map(|(timestamp, power_w)| UsageHistoryPoint {
            timestamp_ms: timestamp.timestamp_millis(),
            power_w,
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
        unit: "W",
    }
}

fn usage_history_range(range_key: Option<&str>) -> UsageHistoryRange {
    match range_key {
        Some("5m") => UsageHistoryRange {
            key: "5m",
            label: "5 minutes",
            interval_label: "5-minute",
            duration: ChronoDuration::minutes(5),
            range_limit: ChronoDuration::hours(12),
            interval: PowerExportInterval::Every5Minutes,
        },
        Some("30m") => UsageHistoryRange {
            key: "30m",
            label: "30 minutes",
            interval_label: "5-minute",
            duration: ChronoDuration::minutes(30),
            range_limit: ChronoDuration::hours(12),
            interval: PowerExportInterval::Every5Minutes,
        },
        Some("1h") => UsageHistoryRange {
            key: "1h",
            label: "1 hour",
            interval_label: "5-minute",
            duration: ChronoDuration::hours(1),
            range_limit: ChronoDuration::hours(12),
            interval: PowerExportInterval::Every5Minutes,
        },
        Some("6h") => UsageHistoryRange {
            key: "6h",
            label: "6 hours",
            interval_label: "5-minute",
            duration: ChronoDuration::hours(6),
            range_limit: ChronoDuration::hours(12),
            interval: PowerExportInterval::Every5Minutes,
        },
        Some("12h") => UsageHistoryRange {
            key: "12h",
            label: "12 hours",
            interval_label: "5-minute",
            duration: ChronoDuration::hours(12),
            range_limit: ChronoDuration::hours(12),
            interval: PowerExportInterval::Every5Minutes,
        },
        Some("1d") => UsageHistoryRange {
            key: "1d",
            label: "1 day",
            interval_label: "5-minute",
            duration: ChronoDuration::days(1),
            range_limit: ChronoDuration::hours(12),
            interval: PowerExportInterval::Every5Minutes,
        },
        Some("3d") => UsageHistoryRange {
            key: "3d",
            label: "3 days",
            interval_label: "hourly",
            duration: ChronoDuration::days(3),
            range_limit: ChronoDuration::days(6),
            interval: PowerExportInterval::Hourly,
        },
        Some("30d") => UsageHistoryRange {
            key: "30d",
            label: "30 days",
            interval_label: "hourly",
            duration: ChronoDuration::days(30),
            range_limit: ChronoDuration::days(6),
            interval: PowerExportInterval::Hourly,
        },
        _ => UsageHistoryRange {
            key: "7d",
            label: "7 days",
            interval_label: "hourly",
            duration: ChronoDuration::days(7),
            range_limit: ChronoDuration::days(6),
            interval: PowerExportInterval::Hourly,
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

fn optional_path_env(name: &str) -> Option<PathBuf> {
    match std::env::var_os(name) {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => None,
    }
}

fn default_state_path() -> Result<PathBuf> {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(config_home)
            .join("fusebox")
            .join("state.json"));
    }

    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home)
            .join(".config")
            .join("fusebox")
            .join("state.json"));
    }

    Err(anyhow!(
        "HOME or XDG_CONFIG_HOME must be set, or set FUSEBOX_STATE_PATH explicitly",
    ))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
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
            --graph-line: #66d18c;
            --graph-fill: rgba(102, 209, 140, 0.16);
            --graph-grid: rgba(34, 29, 23, 0.18);
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
                rgba(0, 0, 0, 0.08);
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

        <p class="notice" id="notice" role="status" hidden></p>

        <section class="cabinet" aria-live="polite">
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
                </div>
                <div class="usage-chart-container">
                    <canvas class="usage-chart" id="usage-chart" aria-label="Hourly power draw in watts for each energy-monitoring plug over the last seven days." role="img"></canvas>
                </div>
                <p class="usage-empty" id="usage-empty">Loading power history from Tapo.</p>
            </section>
            <div class="breaker-grid" id="devices"></div>
        </section>
    </main>

    <script src="https://cdn.jsdelivr.net/npm/chart.js@4.5.1/dist/chart.umd.min.js"></script>
    <script>
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
        const deviceStreamReconnectMs = 2000;
        const chartPalette = ["#e5b75b", "#7bb7ff", "#f06b5c", "#c99cff", "#62d6d1", "#ff9d66", "#b6e36a", "#f38ad3"];
        const defaultHistoryRange = "7d";
        let selectedHistoryRange = defaultHistoryRange;
        let powerChart = null;
        let deviceRequestInFlight = false;
        let historyRequestInFlight = false;
        let deviceSocket = null;
        let deviceSocketReconnect = null;
        let switchAudioContext = null;

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

            devicesEl.innerHTML = devices.map(renderDevice).join("");
            devicesEl.querySelectorAll("button[data-device]").forEach((button) => {
                button.addEventListener("click", async () => {
                    const wasOn = button.dataset.on === "true";
                    const nextIsOn = !wasOn;

                    playSwitchClick(nextIsOn);
                    button.disabled = true;
                    button.dataset.on = String(nextIsOn);
                    button.setAttribute("aria-pressed", String(nextIsOn));
                    button.classList.add("is-switching");

                    try {
                        await requestJson(`/api/devices/${encodeURIComponent(button.dataset.device)}/toggle`, { method: "POST" });
                        await loadDevices();
                    } catch (error) {
                        button.dataset.on = String(wasOn);
                        button.setAttribute("aria-pressed", String(wasOn));
                        renderNotice(error.message);
                    } finally {
                        button.disabled = false;
                        window.setTimeout(() => {
                            button.classList.remove("is-switching");
                        }, 180);
                    }
                });
            });

            return { totalPower, todayEnergy, todayCost };
        }

        function playSwitchClick(nextIsOn) {
            const AudioContextClass = window.AudioContext ?? window.webkitAudioContext;
            if (!AudioContextClass) return;

            try {
                if (switchAudioContext === null) {
                    switchAudioContext = new AudioContextClass();
                }

                if (switchAudioContext.state === "suspended") {
                    void switchAudioContext.resume();
                }

                const now = switchAudioContext.currentTime;
                const primaryClickAt = nextIsOn ? now : now + 0.006;
                const releaseClickAt = nextIsOn ? now + 0.028 : now + 0.022;

                playSwitchClack(primaryClickAt, nextIsOn ? 165 : 135, 0.09);
                playSwitchSnap(primaryClickAt, 0.018, 0.06);
                playSwitchClack(releaseClickAt, nextIsOn ? 86 : 74, 0.045);
            } catch (_error) {
                // Browsers can reject audio creation under stricter policies; the switch should still work.
            }
        }

        function playSwitchClack(startTime, frequency, volume) {
            const oscillator = switchAudioContext.createOscillator();
            const gain = switchAudioContext.createGain();

            oscillator.type = "triangle";
            oscillator.frequency.setValueAtTime(frequency, startTime);
            oscillator.frequency.exponentialRampToValueAtTime(Math.max(20, frequency * 0.45), startTime + 0.026);

            gain.gain.setValueAtTime(0.0001, startTime);
            gain.gain.exponentialRampToValueAtTime(volume, startTime + 0.002);
            gain.gain.exponentialRampToValueAtTime(0.0001, startTime + 0.034);

            oscillator.connect(gain);
            gain.connect(switchAudioContext.destination);
            oscillator.addEventListener("ended", () => {
                oscillator.disconnect();
                gain.disconnect();
            }, { once: true });
            oscillator.start(startTime);
            oscillator.stop(startTime + 0.04);
        }

        function playSwitchSnap(startTime, duration, volume) {
            const sampleRate = switchAudioContext.sampleRate;
            const frameCount = Math.max(1, Math.floor(sampleRate * duration));
            const noiseBuffer = switchAudioContext.createBuffer(1, frameCount, sampleRate);
            const noise = noiseBuffer.getChannelData(0);
            const filter = switchAudioContext.createBiquadFilter();
            const gain = switchAudioContext.createGain();

            for (let index = 0; index < frameCount; index += 1) {
                const envelope = 1 - (index / frameCount);
                noise[index] = (Math.random() * 2 - 1) * envelope;
            }

            const source = switchAudioContext.createBufferSource();
            source.buffer = noiseBuffer;
            filter.type = "bandpass";
            filter.frequency.setValueAtTime(1800, startTime);
            filter.Q.setValueAtTime(1.4, startTime);

            gain.gain.setValueAtTime(0.0001, startTime);
            gain.gain.exponentialRampToValueAtTime(volume, startTime + 0.001);
            gain.gain.exponentialRampToValueAtTime(0.0001, startTime + duration);

            source.connect(filter);
            filter.connect(gain);
            gain.connect(switchAudioContext.destination);
            source.addEventListener("ended", () => {
                source.disconnect();
                filter.disconnect();
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

            const labels = totals.map((point) => formatHistoryLabel(point.timestamp_ms));
            const timestamps = totals.map((point) => point.timestamp_ms);
            const maxWatts = Math.max(...totals.map((point) => point.power_w), 1);
            const chartTheme = powerChartTheme();
            const datasets = [{
                label: "Total",
                data: totals.map((point) => point.power_w),
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
                const pointsByTimestamp = new Map((series.points ?? []).map((point) => [point.timestamp_ms, point.power_w]));
                const color = chartPalette[index % chartPalette.length];
                datasets.push({
                    label: series.device_name,
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
                });
            }

            powerChart.data.labels = labels;
            powerChart.data.datasets = datasets;
            powerChart.options.scales.y.suggestedMax = Math.ceil(maxWatts * 1.15);
            powerChart.update("none");
            usageTitleEl.textContent = `${history.range_label ?? selectedHistoryRange} usage`;
            usageRangeEl.textContent = `${history.interval ?? "power"} readings / ${formatPower(maxWatts)} peak`;

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
                                label: (context) => ` ${formatPower(context.parsed.y)}`,
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
                                callback: (value) => `${value} W`,
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
            };
        }

        function formatHistoryLabel(timestampMs) {
            return new Date(timestampMs).toLocaleString([], {
                weekday: "short",
                day: "2-digit",
                month: "short",
                hour: "2-digit",
                minute: "2-digit",
            });
        }

        function renderDevice(device) {
            const isOn = device.device_on === true;
            const isOffline = device.last_error !== null;
            const energy = device.energy;
            const statusClass = isOffline ? "offline" : isOn ? "on" : "off";
            const statusText = isOffline ? "offline" : isOn ? "on" : "off";

            return `
                <article class="breaker ${isOffline ? "offline" : ""}">
                    <div class="label-card">
                        <h2 class="device-name">${escapeHtml(device.nickname)}</h2>
                        <p class="device-meta">${escapeHtml(device.name)} / ${escapeHtml(device.ip)} / ${escapeHtml(device.model)}</p>
                    </div>
                    <div class="toggle-wrap">
                        <button class="toggle" type="button" data-device="${escapeHtml(device.name)}" data-on="${isOn}" aria-pressed="${isOn}" aria-label="Toggle ${escapeHtml(device.nickname)}">
                            <span class="lever" aria-hidden="true"></span>
                        </button>
                    </div>
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
                    ${isOffline ? `<p class="device-meta">${escapeHtml(device.last_error)}</p>` : ""}
                </article>
            `;
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
            if (hours < 48) return `${hours}h ${totalMinutes % 60}m`;

            const days = Math.floor(hours / 24);
            const remainingHours = hours % 24;
            if (days < 60) return `${days}d ${remainingHours}h`;

            const months = Math.floor(days / 30);
            const remainingDays = days % 30;
            if (months < 24) return `${months}mo ${remainingDays}d`;

            const years = Math.floor(months / 12);
            const remainingMonths = months % 12;
            return `${years}y ${remainingMonths}mo`;
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
        connectDeviceStream();
    </script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_settings_without_optional_values() {
        assert_eq!(optional_u64_env("FUSEBOX_TEST_MISSING", 42).unwrap(), 42);
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
        };

        let view = device.view(30.0);

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
            energy_price_pence_per_kwh: DEFAULT_ENERGY_PRICE_PENCE_PER_KWH,
            state_path,
        }
    }
}
