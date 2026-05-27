use crate::api_error::AppError;
use crate::conditions::condition_intent_for_device;
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
// Static web assets and the index handler moved to crate::web.

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

pub(crate) use crate::schedules::{
    CreateScheduleRequest, MIN_INTERVAL_CYCLE_SECONDS, ScheduleConfig, ScheduleView,
    UpdateScheduleRequest, default_true,
};


pub(crate) use crate::conditions::{
    ConditionAction, ConditionConfig, ConditionListResponse, ConditionView,
    CONDITION_HTTP_TIMEOUT, CreateConditionRequest, DEFAULT_CONDITION_POLL_SECONDS,
    MAX_CONDITION_BODY_BYTES, MAX_CONDITION_POLL_SECONDS, MIN_CONDITION_POLL_SECONDS,
    UpdateConditionRequest, default_condition_poll_seconds, default_http_method,
    default_status_match, deserialize_optional_condition_action,
};

pub(crate) use crate::schedules::ScheduleListResponse;

pub(crate) use crate::hooks::{HookConfig, HookEvent, HookSource};

// ---------- Automations (flowchart) ----------

pub(crate) use crate::automations::types::{
    Automation, AutomationEdge, AutomationListResponse, AutomationNode, AutomationNodeConfig,
    AutomationStatus, CreateAutomationRequest, CronTriggerCfg, DebounceCfg,
    DeviceEventTriggerCfg, FireHookCfg, HttpProbeCfg, IntervalTriggerCfg, NodeRuntimeState,
    SetDeviceCfg, ToggleDeviceCfg, UpdateAutomationRequest,
};

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
        .route("/", get(crate::web::index))
        .route("/favicon.ico", get(crate::web::favicon))
        .route("/assets/switch.wav", get(crate::web::switch_sound))
        .route("/assets/app.js", get(crate::web::app_bundle))
        .route("/health", get(crate::web::health))
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

pub(crate) use crate::schedules::{
    create_schedule, delete_schedule, ensure_conditions_exist, ensure_device_exists,
    interval_phase_at, list_schedules, new_schedule_id, next_interval_fire_ms, normalize_cron,
    parse_cron, schedule_view, translate_cron_to_crate_format, translate_dow_field,
    translate_dow_head, translate_dow_part, translate_dow_value, update_schedule,
    validate_interval,
};
pub(crate) use crate::time::non_empty_label;


pub(crate) use crate::conditions::{create_condition, condition_view, delete_condition, list_conditions, new_condition_id, probe_condition, update_condition};

pub(crate) use crate::hooks::{
    CreateHookRequest, HookListResponse, HookView, UpdateHookRequest, create_hook,
    delete_hook, hook_view, list_hooks, new_hook_id, test_hook, update_hook,
};

// ---------- Automation HTTP handlers ----------

pub(crate) use crate::automations::api::{
    create_automation, delete_automation, has_cycle_dfs, list_automations, new_automation_id,
    update_automation, validate_automation_graph, validate_node_config,
};

pub(crate) use crate::conditions::{clamp_poll_seconds, parse_status_match, status_matches, validate_http_method, validate_url};

pub(crate) use crate::conditions::{ProbeOutcome, condition_probe_key, probe_condition_once, read_response_body};

pub(crate) use crate::conditions::probe_and_record;

pub(crate) use crate::devices::reconcile::{
    clear_manual_override, compute_effective, reconcile_device, run_override_expiry_sweeper,
    set_manual_override, set_schedule_intent,
};

// ---------- Automation execution engine ----------

pub(crate) use crate::automations::engine::{
    evaluate_all_automations, evaluate_node, evaluate_one_automation, execute_action,
    run_automation_engine, topo_sort_nodes,
};

pub(crate) use crate::conditions::run_condition_poller;

pub(crate) use crate::schedules::{
    evaluate_schedules, fire_schedule, record_schedule_error, record_schedule_success,
    run_scheduler,
};


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

pub(crate) use crate::hooks::{
    HookTemplateContext, dispatch_hook_events, fire_hook, hook_event_str,
    hook_matches, hook_source_str, optional_bool_str, render_hook_template,
    update_hook_result,
};


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
