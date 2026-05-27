use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::anyhow;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use reqwest::Method as HttpMethod;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::api_error::AppError;
use crate::legacy::{default_http_method, default_true, non_empty_label, validate_http_method, validate_url};
use crate::state::{AppState, save_persisted_state};
use crate::time::{deserialize_optional_label, now_ms};

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

    pub(crate) fn render(&self, input: &str) -> String {
        render_hook_template(input, &self.vars())
    }

    pub(crate) fn default_payload_json(&self) -> serde_json::Value {
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
