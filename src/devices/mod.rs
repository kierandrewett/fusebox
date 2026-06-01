pub(crate) mod reconcile;

pub(crate) use reconcile::*;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use tapoctl::{
    Config as TapoConfig, DeviceConfig, DeviceSnapshot, automatic_discovery_targets,
    discovery_add_candidates, discovery_scan_targets_with_auto,
};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{info, warn};
use serde::Deserialize;

use crate::api_error::AppError;
use crate::conditions::condition_intent_for_device;
use crate::hooks::{HookEvent, HookSource, dispatch_hook_events};
use crate::state::{
    AppState, DEFAULT_MANUAL_OVERRIDE_SECONDS, DEVICE_OFFLINE_FAILURE_THRESHOLD, DeviceListResponse,
    DeviceView, managed_device_from_config, save_persisted_state,
};
use crate::time::now_ms;

pub(crate) const TAPO_HANDSHAKE_RETRY_ATTEMPTS: usize = 3;
pub(crate) const TAPO_HANDSHAKE_RETRY_DELAY: Duration = Duration::from_millis(350);

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


pub(crate) async fn toggle_device(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Option<Json<ToggleDeviceRequest>>,
) -> Result<Json<DeviceView>, AppError> {
    let requested_duration = body
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

    set_manual_override(&state, &name, target, override_duration(&state, &name, requested_duration).await).await;
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
    let requested_duration = request
        .duration_seconds
        .unwrap_or(DEFAULT_MANUAL_OVERRIDE_SECONDS);
    let device = get_device_config(&state, &name).await?;
    let operation_lock = device_operation_lock(&state, &device).await;
    let _operation_guard = operation_lock.lock().await;
    retry_tapo_handshake(|| state.controller.set_power(&device, request.on)).await?;
    let snapshot = retry_tapo_handshake(|| state.controller.read_device(&device)).await?;
    update_device_snapshot(&state, &name, snapshot, None, HookSource::Manual).await;

    set_manual_override(&state, &name, request.on, override_duration(&state, &name, requested_duration).await).await;
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

/// Resolve the auto-revert window for a fresh manual override. Some(duration)
/// when an automation/schedule/condition could take this device back, None
/// (permanent manual) when nothing would — so we don't show a "auto in 1h"
/// countdown that has nothing to revert to.
async fn override_duration(state: &AppState, name: &str, requested: u64) -> Option<u64> {
    if device_under_automatic_control(state, name).await {
        Some(requested)
    } else {
        None
    }
}

pub(crate) async fn extend_device_override(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Option<Json<ToggleDeviceRequest>>,
) -> Result<Json<DeviceView>, AppError> {
    {
        let devices = state.devices.read().await;
        if !devices.contains_key(&name) {
            return Err(AppError(anyhow!("unknown device '{}'", name)));
        }
    }

    let duration = body
        .map(|Json(req)| req.duration_seconds)
        .unwrap_or(None)
        .unwrap_or(DEFAULT_MANUAL_OVERRIDE_SECONDS);

    if !extend_manual_override(&state, &name, duration).await {
        return Err(AppError(anyhow!(
            "device '{}' has no manual override to extend",
            name
        )));
    }
    if let Err(error) = save_persisted_state(&state).await {
        warn!(%error, device = %name, "failed to persist override extension");
    }
    publish_device_list(&state, None).await;

    get_device_view(&state, &name)
        .await
        .map(Json)
        .map_err(AppError)
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
