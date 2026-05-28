use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Local};
use tokio::time::sleep;
use tracing::{info, warn};

use serde_json::Value;

use crate::automations::expr::{self, EvalContext};
use crate::automations::types::{
    Automation, AutomationEdge, AutomationNode, AutomationNodeConfig, IfConditionCfg, IfOp,
    NodeRuntimeState,
};
use crate::conditions::{ConditionConfig, parse_status_match, probe_condition_once, status_matches};
use crate::hooks::{HookEvent, HookSource, HookTemplateContext, fire_hook};
use crate::devices::reconcile::{reconcile_device, set_schedule_intent};
use crate::schedules::parse_cron;
use crate::state::{AppState, ScheduleAction, save_persisted_state};
use crate::time::now_ms;

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

    // incoming: target_id → list of (source_id, source_socket)
    let mut incoming: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for n in &automation.nodes {
        incoming.entry(n.id.clone()).or_default();
    }
    for e in &automation.edges {
        incoming
            .entry(e.target_node.clone())
            .or_default()
            .push((e.source_node.clone(), e.source_socket.clone()));
    }

    let mut outputs: BTreeMap<String, Option<bool>> = BTreeMap::new();
    let mut transitions: Vec<(String, AutomationNodeConfig)> = Vec::new();
    let mut node_state_updates: BTreeMap<String, NodeRuntimeState> = BTreeMap::new();

    // Live, mutable copy of the automation's variable store. SetVariable
    // blocks write into it; GetVariable/Expression read from it. Updates are
    // visible to later nodes in topological order within the same tick and
    // persisted at the end if anything changed.
    let mut variables = automation.variables.clone();

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

        // Pull each upstream node's value and source id. If the edge came
        // from a "no" socket (the IfCondition NO branch), invert the value
        // so downstream sees a rising edge when the condition routed there.
        let input_values: Vec<IncomingInput> = incoming
            .get(node_id)
            .map(|sources| {
                sources
                    .iter()
                    .map(|(src, socket)| {
                        let value = outputs.get(src).copied().unwrap_or(None);
                        let adjusted = if socket == "no" { value.map(|v| !v) } else { value };
                        IncomingInput {
                            source_node: src.clone(),
                            value: adjusted,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let (new_output, new_state) = evaluate_node(
            state,
            &automation.id,
            node,
            &input_values,
            &prev_state,
            &mut variables,
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
        // These nodes write a richer "value" output themselves (a body,
        // number, dictionary, …), so the loop must not clobber it with the
        // boolean pulse.
        let manages_own_value = matches!(
            node.config,
            AutomationNodeConfig::HttpRequest { .. }
                | AutomationNodeConfig::SetVariable { .. }
                | AutomationNodeConfig::GetVariable { .. }
                | AutomationNodeConfig::Expression { .. }
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
        if !manages_own_value {
            if let Some(v) = new_output {
                // Every node exposes "value" (the boolean pulse it propagates).
                // Node-specific outputs (body, status_code, ...) are written
                // by the handler itself; we don't touch them here.
                merged.outputs.insert("value".to_string(), v.to_string());
            }
        }
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
            if a.variables != variables {
                dirty = true;
                a.variables = variables;
            }
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

/// One edge feeding into the node being evaluated, with its (possibly
/// branch-inverted) boolean value and the source node id (so IF blocks
/// can look up the source's recorded body/status).
pub(crate) struct IncomingInput {
    pub(crate) source_node: String,
    pub(crate) value: Option<bool>,
}

pub(crate) async fn evaluate_node(
    state: &AppState,
    automation_id: &str,
    node: &AutomationNode,
    inputs: &[IncomingInput],
    prev: &NodeRuntimeState,
    variables: &mut BTreeMap<String, Value>,
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
        AutomationNodeConfig::HttpProbe { http_probe: _ } => {
            // Legacy variant — converted at load time. If one slips through,
            // treat as a no-op so we don't panic.
            (None, next)
        }
        AutomationNodeConfig::HttpRequest { http_request } => {
            // Action variant: only fire a request on the rising edge of an
            // input pulse. last_value = matched status; last_body and
            // last_status_code are populated so downstream If blocks can
            // branch on the response.
            let rising = matches!(
                (prev.last_value, inputs.iter().any(|i| matches!(i.value, Some(true)))),
                (None, true) | (Some(false), true)
            );
            if !rising {
                return (prev.last_value, next);
            }
            if http_request.url.is_empty() {
                next.last_error = Some("URL not configured".to_string());
                return (Some(false), next);
            }

            let probe = ConditionConfig {
                id: format!("auto/{automation_id}/node/{}", node.id),
                name: node.id.clone(),
                device_name: String::new(),
                url: http_request.url.clone(),
                method: http_request.method.clone(),
                headers: http_request.headers.clone(),
                body: http_request.body.clone(),
                status_match: http_request.status_match.clone(),
                body_contains: None,
                poll_seconds: 60,
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
            let outcome = probe_condition_once(&state.http_client, &probe).await;
            next.last_checked_at_ms = Some(tick_ms);
            next.last_status_code = outcome.status_code;
            next.last_body = outcome.body.clone();
            next.outputs.clear();
            next.outputs
                .insert("value".to_string(), outcome.passing.to_string());
            next.outputs
                .insert("succeeded".to_string(), outcome.passing.to_string());
            if let Some(body) = outcome.body.as_ref() {
                next.outputs.insert("body".to_string(), body.clone());
            }
            if let Some(code) = outcome.status_code {
                next.outputs
                    .insert("status_code".to_string(), code.to_string());
            }
            if let Some(error) = outcome.error {
                next.last_error = Some(error);
            } else {
                next.last_error = None;
            }
            (Some(outcome.passing), next)
        }
        AutomationNodeConfig::IfCondition { if_condition } => {
            // Re-evaluate on the rising edge of an input pulse. The route
            // value holds afterwards so downstream pulses keep their route.
            let rising = matches!(
                (prev.last_value, inputs.iter().any(|i| matches!(i.value, Some(true)))),
                (None, true) | (Some(false), true)
            );
            if !rising {
                return (prev.last_value, next);
            }
            // Find the upstream block we should inspect. If there's no IN
            // connection yet, route to NO.
            let source_id = match inputs.first() {
                Some(input) => input.source_node.clone(),
                None => return (Some(false), next),
            };
            let source_state: Option<NodeRuntimeState> = {
                let automations = state.automations.read().await;
                automations
                    .get(automation_id)
                    .and_then(|a| a.status.node_states.get(&source_id))
                    .cloned()
            };
            let matched = evaluate_if_check(if_condition, source_state.as_ref());
            (Some(matched), next)
        }
        AutomationNodeConfig::LogicAnd => {
            if inputs.is_empty() {
                return (Some(false), next);
            }
            let mut all_true = true;
            for input in inputs {
                match input.value {
                    Some(true) => continue,
                    _ => {
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
            let any_true = inputs.iter().any(|i| matches!(i.value, Some(true)));
            (Some(any_true), next)
        }
        AutomationNodeConfig::LogicNot => {
            let v = inputs.first().and_then(|i| i.value);
            (v.map(|b| !b), next)
        }
        AutomationNodeConfig::Debounce { debounce } => {
            let new_value = inputs.first().and_then(|i| i.value);
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
        AutomationNodeConfig::Expression { expression } => {
            // Re-evaluate on the rising edge of an input pulse; the result
            // holds afterwards so downstream reads stay stable.
            let rising = matches!(
                (prev.last_value, inputs.iter().any(|i| matches!(i.value, Some(true)))),
                (None, true) | (Some(false), true)
            );
            if !rising {
                return (prev.last_value, next);
            }
            let input = fetch_source_outputs(state, automation_id, inputs).await;
            let result = {
                let ctx = EvalContext {
                    variables,
                    input,
                };
                expr::evaluate(&expression.expression, &ctx)
            };
            match result {
                Ok(value) => {
                    write_value_outputs(&mut next, &value);
                    (Some(expr::truthy(&value)), next)
                }
                Err(error) => {
                    next.last_error = Some(error);
                    (Some(false), next)
                }
            }
        }
        AutomationNodeConfig::SetVariable { set_variable } => {
            let rising = matches!(
                (prev.last_value, inputs.iter().any(|i| matches!(i.value, Some(true)))),
                (None, true) | (Some(false), true)
            );
            if !rising {
                return (prev.last_value, next);
            }
            if set_variable.key.is_empty() {
                next.last_error = Some("variable key is empty".to_string());
                return (Some(false), next);
            }
            let input = fetch_source_outputs(state, automation_id, inputs).await;
            let result = {
                let ctx = EvalContext {
                    variables,
                    input,
                };
                expr::evaluate(&set_variable.expression, &ctx)
            };
            match result {
                Ok(value) => {
                    variables.insert(set_variable.key.clone(), value.clone());
                    write_value_outputs(&mut next, &value);
                    (Some(true), next)
                }
                Err(error) => {
                    next.last_error = Some(error);
                    (Some(false), next)
                }
            }
        }
        AutomationNodeConfig::GetVariable { get_variable } => {
            // Source-style: always expose the current variable value. The
            // boolean it propagates is just its input pulse passed through,
            // so a downstream IF fires when the upstream trigger fires.
            let value = variables
                .get(&get_variable.key)
                .cloned()
                .unwrap_or(Value::Null);
            write_value_outputs(&mut next, &value);
            let pulse = inputs.iter().any(|i| matches!(i.value, Some(true)));
            (Some(pulse), next)
        }
        AutomationNodeConfig::SetDevice { .. }
        | AutomationNodeConfig::ToggleDevice { .. }
        | AutomationNodeConfig::FireHook { .. } => {
            // Actions just propagate the maximum of their inputs so they
            // can in turn drive other actions if connected.
            let active = inputs.iter().any(|i| matches!(i.value, Some(true)));
            (Some(active), next)
        }
    }
}

/// Build the `input` dictionary an expression sees: the named outputs of
/// the node wired to this block's IN socket, as a JSON object of strings.
/// Reads committed state (one tick behind, same as the IF block).
async fn fetch_source_outputs(
    state: &AppState,
    automation_id: &str,
    inputs: &[IncomingInput],
) -> Value {
    let source_id = match inputs.first() {
        Some(input) => &input.source_node,
        None => return Value::Object(Default::default()),
    };
    let outputs = {
        let automations = state.automations.read().await;
        automations
            .get(automation_id)
            .and_then(|a| a.status.node_states.get(source_id))
            .map(|s| s.outputs.clone())
            .unwrap_or_default()
    };
    let map: serde_json::Map<String, Value> = outputs
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    Value::Object(map)
}

/// Write an evaluated value into a node's runtime outputs: "value" carries
/// the stringified result; if it's a dictionary, each entry is also exposed
/// as its own output key so an IF block can read individual fields.
fn write_value_outputs(next: &mut NodeRuntimeState, value: &Value) {
    next.last_error = None;
    next.outputs.clear();
    next.outputs
        .insert("value".to_string(), expr::to_text(value));
    if let Value::Object(map) = value {
        for (k, v) in map {
            if k != "value" {
                next.outputs.insert(k.clone(), expr::to_text(v));
            }
        }
    }
}

/// Evaluate an IF block's predicate. Reads `cfg.field` from the source
/// node's `outputs` map and applies `cfg.op` against `cfg.value`.
pub(crate) fn evaluate_if_check(
    cfg: &IfConditionCfg,
    source: Option<&NodeRuntimeState>,
) -> bool {
    let source = match source {
        Some(s) => s,
        None => return false,
    };
    let field_value = lookup_output(source, &cfg.field);
    match cfg.op {
        IfOp::IsTrue => matches!(field_value.as_deref(), Some("true")),
        IfOp::Equals => match field_value.as_deref() {
            Some(actual) => actual.trim() == cfg.value.trim(),
            None => false,
        },
        IfOp::Contains => {
            if cfg.value.is_empty() {
                return false;
            }
            field_value
                .as_deref()
                .is_some_and(|actual| actual.contains(&cfg.value))
        }
        IfOp::InRange => {
            let code: u16 = match field_value.as_deref().and_then(|s| s.parse().ok()) {
                Some(n) => n,
                None => return false,
            };
            status_matches_range(&cfg.value, code).unwrap_or(false)
        }
    }
}

/// Resolve `field` against a node's outputs, with sensible legacy
/// fall-throughs so old persisted state (which only had `last_body` /
/// `last_status_code` / `last_value`) keeps working until it gets
/// re-evaluated and populates `outputs`.
fn lookup_output(state: &NodeRuntimeState, field: &str) -> Option<String> {
    if let Some(v) = state.outputs.get(field) {
        return Some(v.clone());
    }
    match field {
        "value" => state.last_value.map(|b| b.to_string()),
        "body" => state.last_body.clone(),
        "status_code" => state.last_status_code.map(|c| c.to_string()),
        "succeeded" => state.last_value.map(|b| b.to_string()),
        _ => None,
    }
}

fn status_matches_range(spec: &str, code: u16) -> Option<bool> {
    let ranges = parse_status_match(spec).ok()?;
    Some(status_matches(&ranges, code))
}

pub(crate) async fn execute_action(
    state: &AppState,
    automation_id: &str,
    node_id: &str,
    config: &AutomationNodeConfig,
) -> Result<Option<String>> {
    match config {
        AutomationNodeConfig::SetDevice { set_device } => {
            if set_device.device_name.is_empty() {
                return Ok(None);
            }
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
            if toggle_device.device_name.is_empty() {
                return Ok(None);
            }
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
            if fire_hook_cfg.hook_id.is_empty() {
                return Ok(None);
            }
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
