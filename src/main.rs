use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tapoctl::{
    Config as TapoConfig, DeviceConfig, DeviceModel, DeviceSnapshot, TapoController,
    TapoCredentials, discovery_add_candidates,
};
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone)]
struct Settings {
    bind_address: SocketAddr,
    username: String,
    password: String,
    refresh_seconds: u64,
    scan_seconds: u64,
    discovery_timeout_seconds: u64,
    state_path: PathBuf,
}

#[derive(Debug, Clone)]
struct AppState {
    controller: TapoController,
    devices: Arc<RwLock<BTreeMap<String, ManagedDevice>>>,
    discovery_timeout_seconds: u64,
    refresh_seconds: u64,
    scan_seconds: u64,
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
    today_runtime_minutes: u64,
    month_runtime_minutes: u64,
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

    tokio::spawn(initial_refresh_devices(state.clone()));
    tokio::spawn(monitor_devices(state.clone()));
    tokio::spawn(scan_for_devices(state.clone()));

    let app = Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(favicon))
        .route("/health", get(health))
        .route("/api/devices", get(list_devices))
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

        Ok(Self {
            bind_address,
            username,
            password,
            refresh_seconds,
            scan_seconds,
            discovery_timeout_seconds,
            state_path,
        })
    }
}

impl AppState {
    fn new(settings: &Settings) -> Self {
        let controller = TapoController::new(TapoCredentials {
            username: settings.username.clone(),
            password: settings.password.clone(),
        });

        Self {
            controller,
            devices: Arc::new(RwLock::new(BTreeMap::new())),
            discovery_timeout_seconds: settings.discovery_timeout_seconds,
            refresh_seconds: settings.refresh_seconds,
            scan_seconds: settings.scan_seconds,
            state_path: settings.state_path.clone(),
        }
    }
}

impl ManagedDevice {
    fn view(&self) -> DeviceView {
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
    Json(DeviceListResponse {
        devices: device_views(&state).await,
        updated_at_ms: now_ms(),
        scan_error: None,
    })
}

async fn scan_devices(State(state): State<AppState>) -> Json<DeviceListResponse> {
    let scan_error = match scan_and_refresh(&state).await {
        Ok(()) => None,
        Err(error) => Some(error.to_string()),
    };

    Json(DeviceListResponse {
        devices: device_views(&state).await,
        updated_at_ms: now_ms(),
        scan_error,
    })
}

async fn toggle_device(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<DeviceView>, AppError> {
    let device = get_device_config(&state, &name).await?;
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
    state.controller.set_power(&device, request.on).await?;
    refresh_device(&state, &name, device).await;

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
    match state.controller.read_device(&device).await {
        Ok(snapshot) => update_device_snapshot(state, name, snapshot, None).await,
        Err(error) => update_device_error(state, name, error.to_string()).await,
    }
}

async fn update_device_snapshot(
    state: &AppState,
    name: &str,
    snapshot: DeviceSnapshot,
    last_error: Option<String>,
) {
    let mut devices = state.devices.write().await;

    if let Some(device) = devices.get_mut(name) {
        device.snapshot = Some(snapshot);
        device.last_error = last_error;
        device.updated_at_ms = Some(now_ms());
    }
}

async fn update_device_error(state: &AppState, name: &str, error: String) {
    let mut devices = state.devices.write().await;

    if let Some(device) = devices.get_mut(name) {
        device.last_error = Some(error);
        device.updated_at_ms = Some(now_ms());
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
    let devices = state.devices.read().await;

    devices.values().map(ManagedDevice::view).collect()
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
        .map(ManagedDevice::view)
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
    <title>Fusebox</title>
    <style>
        :root {
            color-scheme: dark;
            --wall: #201d19;
            --cabinet: #5b5144;
            --cabinet-dark: #211d19;
            --bakelite: #181613;
            --brass: #c19b55;
            --paper: #e5d8b6;
            --ink: #221d17;
            --green: #66d18c;
            --red: #de5e4b;
            --amber: #e5b75b;
            --muted: #9b907d;
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
            color: #f2ead7;
            font-family: ui-serif, Georgia, Cambria, "Times New Roman", serif;
        }

        body::before {
            content: "";
            position: fixed;
            inset: 0;
            z-index: -1;
            background:
                radial-gradient(circle at 8% 0%, rgba(255, 214, 128, 0.13), transparent 34rem),
                radial-gradient(circle at 92% 18%, rgba(193, 155, 85, 0.07), transparent 38rem),
                linear-gradient(135deg, #171512 0%, var(--wall) 54%, #14120f 100%);
            background-repeat: no-repeat;
            background-size: cover;
        }

        button {
            font: inherit;
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
            color: #f3e9d1;
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
            color: #21170c;
            background: linear-gradient(#e7c577, #9e7135);
            box-shadow:
                inset 0 1px 0 rgba(255, 255, 255, 0.45),
                0 4px 0 #553619,
                0 12px 24px rgba(0, 0, 0, 0.35);
            cursor: pointer;
        }

        .scan-button:focus-visible,
        .toggle:focus-visible {
            outline: 3px solid #f2d48a;
            outline-offset: 3px;
        }

        .cabinet {
            position: relative;
            overflow: hidden;
            min-height: 560px;
            padding: clamp(18px, 4vw, 42px);
            border: 10px solid #2a2118;
            border-radius: 18px;
            background:
                linear-gradient(90deg, rgba(255,255,255,0.06), transparent 16%, rgba(0,0,0,0.18) 72%),
                repeating-linear-gradient(90deg, #5f5445 0 18px, #514738 18px 36px);
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
            grid-template-columns: repeat(3, minmax(0, 1fr));
            gap: 12px;
            margin-bottom: 22px;
        }

        .meter {
            padding: 12px 14px;
            border: 1px solid rgba(0, 0, 0, 0.55);
            border-radius: 8px;
            color: var(--ink);
            background: linear-gradient(#efe1bd, #c8b37c);
            box-shadow: inset 0 1px 8px rgba(255, 255, 255, 0.45), inset 0 -8px 18px rgba(88, 60, 28, 0.2);
        }

        .meter span {
            display: block;
            font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
            font-size: 11px;
            letter-spacing: 0.12em;
            text-transform: uppercase;
        }

        .meter strong {
            display: block;
            margin-top: 5px;
            font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
            font-size: 22px;
        }

        .breaker-grid {
            display: grid;
            grid-template-columns: repeat(auto-fill, minmax(220px, 280px));
            justify-content: start;
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
                linear-gradient(#26231f, var(--bakelite));
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
            background: linear-gradient(#f2e7c7, #c7b783);
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
            font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
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
            width: 84px;
            height: 132px;
            border: 0;
            border-radius: 16px;
            background: linear-gradient(90deg, #100f0e, #3b3833 48%, #0e0d0c);
            box-shadow:
                inset 0 0 0 2px #090807,
                inset 0 0 24px rgba(0, 0, 0, 0.7),
                0 0 0 6px rgba(0, 0, 0, 0.22);
            cursor: pointer;
        }

        .lever {
            position: absolute;
            left: 18px;
            width: 48px;
            height: 64px;
            border-radius: 10px;
            background: linear-gradient(90deg, #34302a, #b8aa8c 44%, #3a352e);
            box-shadow:
                inset 0 1px 0 rgba(255, 255, 255, 0.35),
                inset 0 -10px 16px rgba(0, 0, 0, 0.35),
                0 8px 14px rgba(0, 0, 0, 0.5);
            transition: top 180ms ease, transform 180ms ease;
        }

        .toggle[data-on="true"] .lever {
            top: 14px;
            transform: rotate(-2deg);
        }

        .toggle[data-on="false"] .lever {
            top: 54px;
            transform: rotate(2deg);
        }

        .status-strip {
            display: flex;
            justify-content: space-between;
            gap: 8px;
            margin-top: 10px;
            font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
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
            font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
            font-size: 12px;
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
            font-family: ui-sans-serif, system-ui, sans-serif;
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
        }
    </style>
</head>
<body>
    <main class="shell">
        <header class="header">
            <h1>Fusebox</h1>
            <button class="scan-button" id="scan" type="button">Scan now</button>
        </header>

        <p class="notice" id="notice" role="status" hidden></p>

        <section class="cabinet" aria-live="polite">
            <div class="meter-row" aria-label="Fusebox summary">
                <div class="meter"><span>Devices</span><strong id="device-count">0</strong></div>
                <div class="meter"><span>Live load</span><strong id="total-power">0 W</strong></div>
                <div class="meter"><span>Today</span><strong id="today-energy">0 Wh</strong></div>
            </div>
            <div class="breaker-grid" id="devices"></div>
        </section>
    </main>

    <script>
        const devicesEl = document.querySelector("#devices");
        const scanButton = document.querySelector("#scan");
        const deviceCountEl = document.querySelector("#device-count");
        const totalPowerEl = document.querySelector("#total-power");
        const todayEnergyEl = document.querySelector("#today-energy");
        const noticeEl = document.querySelector("#notice");
        const devicePollMs = 500;
        let deviceRequestInFlight = false;

        scanButton.addEventListener("click", async () => {
            scanButton.disabled = true;
            scanButton.textContent = "Scanning";
            try {
                const payload = await requestJson("/api/scan", { method: "POST" });
                renderDevices(payload.devices ?? []);
                renderNotice(payload.scan_error);
            } catch (error) {
                renderNotice(error.message);
            } finally {
                scanButton.disabled = false;
                scanButton.textContent = "Scan now";
            }
        });

        async function loadDevices() {
            if (deviceRequestInFlight) return;

            deviceRequestInFlight = true;

            try {
                const payload = await requestJson("/api/devices");
                renderDevices(payload.devices ?? []);
                renderNotice(payload.scan_error);
            } catch (error) {
                renderNotice(error.message);
            } finally {
                deviceRequestInFlight = false;
            }
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

        function renderDevices(devices) {
            const totalPower = devices.reduce((total, device) => total + (device.energy?.current_power_w ?? 0), 0);
            const todayEnergy = devices.reduce((total, device) => total + (device.energy?.today_energy_wh ?? 0), 0);

            deviceCountEl.textContent = devices.length;
            totalPowerEl.textContent = `${totalPower} W`;
            todayEnergyEl.textContent = `${todayEnergy} Wh`;

            if (devices.length === 0) {
                devicesEl.innerHTML = `<div class="empty">No supported Tapo plugs found yet. Check credentials and LAN access, or press Scan now.</div>`;
                return;
            }

            devicesEl.innerHTML = devices.map(renderDevice).join("");
            devicesEl.querySelectorAll("button[data-device]").forEach((button) => {
                button.addEventListener("click", async () => {
                    button.disabled = true;
                    try {
                        await requestJson(`/api/devices/${encodeURIComponent(button.dataset.device)}/toggle`, { method: "POST" });
                        await loadDevices();
                    } catch (error) {
                        renderNotice(error.message);
                    } finally {
                        button.disabled = false;
                    }
                });
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
                        <button class="toggle" type="button" data-device="${escapeHtml(device.name)}" data-on="${isOn}" aria-label="Toggle ${escapeHtml(device.nickname)}">
                            <span class="lever" aria-hidden="true"></span>
                        </button>
                    </div>
                    <div class="status-strip">
                        <span class="lamp ${statusClass}">${statusText}</span>
                        <span>${formatRuntime(device.on_time_seconds)}</span>
                    </div>
                    <div class="readings">
                        <div class="reading"><span>Now</span>${energy?.current_power_w ?? "-"} W</div>
                        <div class="reading"><span>Today</span>${energy?.today_energy_wh ?? "-"} Wh</div>
                        <div class="reading"><span>Month</span>${energy?.month_energy_wh ?? "-"} Wh</div>
                        <div class="reading"><span>Runtime</span>${energy?.today_runtime_minutes ?? "-"} min</div>
                    </div>
                    ${isOffline ? `<p class="device-meta">${escapeHtml(device.last_error)}</p>` : ""}
                </article>
            `;
        }

        function formatRuntime(seconds) {
            if (seconds === null || seconds === undefined) return "unknown";
            const minutes = Math.floor(seconds / 60);
            if (minutes < 60) return `${minutes} min`;
            return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
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
        setInterval(loadDevices, devicePollMs);
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
                energy: None,
            }),
            last_error: None,
            discovered_at_ms: 1,
            updated_at_ms: Some(2),
        };

        let view = device.view();

        assert_eq!(view.name, "lights");
        assert_eq!(view.nickname, "Lights");
        assert_eq!(view.device_on, Some(true));
        assert_eq!(view.on_time_seconds, Some(120));
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

    fn test_state_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fusebox-{name}-{}-{}.json",
            std::process::id(),
            now_ms(),
        ))
    }
}
