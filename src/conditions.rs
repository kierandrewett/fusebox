use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use reqwest::Method as HttpMethod;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::warn;

use crate::api_error::AppError;
use crate::hooks::HookSource;
use crate::devices::reconcile_device;
use crate::schedules::ensure_device_exists;
use crate::schedules::default_true;
use crate::state::{AppState, save_persisted_state};
use crate::time::{deserialize_optional_label, non_empty_label, now_ms};

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
    pub(crate) body: Option<String>,
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
                body: None,
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
                body: None,
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
                body: None,
                error: Some(format!("{error}")),
            };
        }
    };

    let status = response.status().as_u16();
    let status_ok = status_matches(&ranges, status);

    // Always read the body so callers (the HTTP request action) can
    // expose it to downstream If blocks. body_contains stays for the
    // legacy ConditionConfig probe path.
    let body_text = match read_response_body(response).await {
        Ok(b) => Some(b),
        Err(error) => {
            return ProbeOutcome {
                passing: false,
                status_code: Some(status),
                body: None,
                error: Some(format!("response read failed: {error}")),
            };
        }
    };
    let body_match = match condition.body_contains.as_deref() {
        Some(needle) => body_text.as_deref().is_some_and(|b| b.contains(needle)),
        None => true,
    };

    ProbeOutcome {
        passing: status_ok && body_match,
        status_code: Some(status),
        body: body_text,
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

