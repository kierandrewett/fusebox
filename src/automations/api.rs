use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use tracing::warn;

use crate::api_error::AppError;
use crate::automations::engine::simulate_forecast;
use crate::automations::expr::{self, EvalContext};
use crate::automations::types::{
    Automation, AutomationEdge, AutomationExport, AutomationListResponse, AutomationNode,
    AutomationNodeConfig, AutomationStatus, CreateAutomationRequest, ForecastResponse,
    PreviewRequest, PreviewResponse, RunNodeRequest, RunNodeResponse, UpdateAutomationRequest,
    export_kind, export_version,
};
use crate::conditions::{clamp_poll_seconds, parse_status_match, validate_http_method, validate_url};
use crate::schedules::{MIN_INTERVAL_CYCLE_SECONDS, parse_cron};
use crate::state::{AppState, save_persisted_state};
use crate::time::now_ms;

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
        variables: Default::default(),
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

pub(crate) async fn export_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AutomationExport>, AppError> {
    let automations = state.automations.read().await;
    let automation = automations
        .get(&id)
        .ok_or_else(|| AppError(anyhow!("unknown automation '{}'", id)))?;
    Ok(Json(AutomationExport {
        kind: export_kind(),
        version: export_version(),
        name: automation.name.clone(),
        enabled: automation.enabled,
        nodes: automation.nodes.clone(),
        edges: automation.edges.clone(),
        variables: automation.variables.clone(),
    }))
}

pub(crate) async fn import_automation(
    State(state): State<AppState>,
    Json(payload): Json<AutomationExport>,
) -> Result<(StatusCode, Json<Automation>), AppError> {
    if payload.kind != export_kind() {
        return Err(AppError(anyhow!(
            "not a Fusebox automation export (kind '{}')",
            payload.kind
        )));
    }
    if payload.version > export_version() {
        return Err(AppError(anyhow!(
            "export is version {} but this server only understands up to {}",
            payload.version,
            export_version()
        )));
    }
    validate_automation_graph(&payload.nodes, &payload.edges)?;

    let name = {
        let trimmed = payload.name.trim();
        if trimmed.is_empty() {
            "Imported automation".to_string()
        } else {
            trimmed.to_string()
        }
    };
    let id = new_automation_id();
    let automation = Automation {
        id: id.clone(),
        name,
        enabled: payload.enabled,
        nodes: payload.nodes,
        edges: payload.edges,
        created_at_ms: now_ms(),
        status: AutomationStatus::default(),
        variables: payload.variables,
    };
    {
        let mut automations = state.automations.write().await;
        automations.insert(id, automation.clone());
    }
    if let Err(error) = save_persisted_state(&state).await {
        warn!(%error, "failed to persist imported automation");
    }
    Ok((StatusCode::CREATED, Json(automation)))
}

pub(crate) async fn device_forecast(State(state): State<AppState>) -> Json<ForecastResponse> {
    const HORIZON_MS: u128 = 12 * 60 * 60 * 1000;
    const STEP_MS: u128 = 60 * 1000;
    let now = now_ms();
    let horizon = now + HORIZON_MS;
    let events = simulate_forecast(&state, now, horizon, STEP_MS).await;
    Json(ForecastResponse {
        generated_at_ms: now,
        horizon_ms: horizon,
        events,
    })
}

pub(crate) async fn preview_expression(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PreviewRequest>,
) -> Result<Json<PreviewResponse>, AppError> {
    if req.expression.trim().is_empty() {
        return Ok(Json(PreviewResponse {
            ok: false,
            result: None,
            result_text: None,
            error: Some("enter an expression to preview".to_string()),
            input_fields: Vec::new(),
        }));
    }

    let (variables, input, input_fields) = {
        let automations = state.automations.read().await;
        let automation = automations
            .get(&id)
            .ok_or_else(|| AppError(anyhow!("unknown automation '{}'", id)))?;
        let variables = automation.variables.clone();
        let outputs = req
            .upstream_id
            .as_ref()
            .and_then(|uid| automation.status.node_states.get(uid))
            .map(|s| s.outputs.clone())
            .unwrap_or_default();
        let fields: Vec<String> = outputs.keys().cloned().collect();
        let map: serde_json::Map<String, serde_json::Value> = outputs
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect();
        (variables, serde_json::Value::Object(map), fields)
    };

    let device_states = crate::automations::engine::collect_device_states(&state).await;
    let ctx = EvalContext {
        variables: &variables,
        input,
        devices: &device_states,
        now_ms: now_ms(),
    };
    let response = match expr::evaluate(&req.expression, &ctx) {
        Ok(value) => {
            let result_text = match &value {
                serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| expr::to_text(&value))
                }
                other => expr::to_text(other),
            };
            PreviewResponse {
                ok: true,
                result: Some(value),
                result_text: Some(result_text),
                error: None,
                input_fields,
            }
        }
        Err(error) => PreviewResponse {
            ok: false,
            result: None,
            result_text: None,
            error: Some(error),
            input_fields,
        },
    };
    Ok(Json(response))
}

pub(crate) async fn run_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RunNodeRequest>,
) -> Result<Json<RunNodeResponse>, AppError> {
    // Build a transient automation from the graph in the request (so unsaved
    // editor changes can be tested) while reusing the persisted variables and
    // runtime state as the baseline.
    let automation = {
        let automations = state.automations.read().await;
        let saved = automations
            .get(&id)
            .ok_or_else(|| AppError(anyhow!("unknown automation '{}'", id)))?;
        Automation {
            id: saved.id.clone(),
            name: saved.name.clone(),
            enabled: saved.enabled,
            nodes: req.nodes,
            edges: req.edges,
            created_at_ms: saved.created_at_ms,
            status: saved.status.clone(),
            variables: saved.variables.clone(),
        }
    };

    let target = req.node_id.as_deref().filter(|s| !s.is_empty());
    let response =
        match crate::automations::engine::dry_run_node(&state, &automation, target, req.live).await {
            Ok(nodes) => RunNodeResponse { ok: true, nodes, error: None },
            Err(error) => RunNodeResponse { ok: false, nodes: Vec::new(), error: Some(error) },
        };
    Ok(Json(response))
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
    // Validation is for "obviously wrong" shapes only — invalid cron syntax,
    // a zero-length interval cycle, an unparseable URL. Empty pickers
    // (device_name, hook_id) are allowed so users can draft a node now and
    // wire it up later; the engine simply skips actions with missing targets.
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
            if !http_probe.url.is_empty() {
                validate_url(&http_probe.url)?;
            }
            validate_http_method(&http_probe.method)?;
            parse_status_match(&http_probe.status_match)?;
            clamp_poll_seconds(http_probe.poll_seconds)?;
        }
        AutomationNodeConfig::HttpRequest { http_request } => {
            if !http_request.url.is_empty() {
                validate_url(&http_request.url)?;
            }
            validate_http_method(&http_request.method)?;
            parse_status_match(&http_request.status_match)?;
        }
        AutomationNodeConfig::Expression { expression } => {
            if !expression.expression.trim().is_empty() {
                crate::automations::expr::validate(&expression.expression)
                    .map_err(|e| anyhow!("invalid expression: {e}"))?;
            }
        }
        AutomationNodeConfig::SetVariable { set_variable } => {
            if !set_variable.expression.trim().is_empty() {
                crate::automations::expr::validate(&set_variable.expression)
                    .map_err(|e| anyhow!("invalid expression: {e}"))?;
            }
        }
        AutomationNodeConfig::IfCondition { if_condition } => {
            if !if_condition.expression.trim().is_empty() {
                crate::automations::expr::validate(&if_condition.expression)
                    .map_err(|e| anyhow!("invalid expression: {e}"))?;
            }
        }
        AutomationNodeConfig::DeviceEventTrigger { .. }
        | AutomationNodeConfig::SetDevice { .. }
        | AutomationNodeConfig::ToggleDevice { .. }
        | AutomationNodeConfig::FireHook { .. }
        | AutomationNodeConfig::GetVariable { .. }
        | AutomationNodeConfig::Between { .. }
        | AutomationNodeConfig::VariableChanged { .. }
        | AutomationNodeConfig::ImmediateTrigger => {
            // Picker may be empty while the user is still wiring things up.
        }
        AutomationNodeConfig::LogicAnd
        | AutomationNodeConfig::LogicOr
        | AutomationNodeConfig::LogicNot
        | AutomationNodeConfig::Debounce { .. } => {}
    }
    Ok(())
}
