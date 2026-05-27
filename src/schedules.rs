use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Local, Timelike};
use cron::Schedule as CronSchedule;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::api_error::AppError;
use crate::hooks::HookSource;
use crate::legacy::{reconcile_device, set_schedule_intent};
use crate::state::{AppState, ScheduleAction, ScheduleKind, save_persisted_state};
use crate::time::{deserialize_optional_label, non_empty_label, now_ms};

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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScheduleListResponse {
    pub(crate) schedules: Vec<ScheduleView>,
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
