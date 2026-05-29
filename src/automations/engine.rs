use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Datelike, Local, Timelike};
use tokio::time::sleep;
use tracing::{info, warn};

use serde_json::Value;

use crate::automations::expr::{self, EvalContext};
use crate::automations::types::{
    Automation, AutomationEdge, AutomationNode, AutomationNodeConfig, ForecastEvent,
    IfConditionCfg, IfOp, NodeRuntimeState, RunNodeResult,
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

    // Immediate triggers fire once at startup. On the tick they fire we clear
    // the rising-edge baseline of everything downstream of them, so a chain
    // like Immediate → HTTP → SetVariable actually re-runs even though those
    // nodes have a persisted last_value from before the restart.
    let reset_nodes = compute_immediate_reset_nodes(state, &automation).await;

    let mut outputs: BTreeMap<String, Option<bool>> = BTreeMap::new();
    let mut transitions: Vec<(String, AutomationNodeConfig)> = Vec::new();
    let mut node_state_updates: BTreeMap<String, NodeRuntimeState> = BTreeMap::new();

    // Live, mutable copy of the automation's variable store. SetVariable
    // blocks write into it; GetVariable/Expression read from it. Updates are
    // visible to later nodes in topological order within the same tick and
    // persisted at the end if anything changed.
    let mut variables = automation.variables.clone();

    // Snapshot of device power state for deviceOn/deviceOff/deviceState in
    // expressions. Built once per tick; keyed by name and nickname.
    let device_states = collect_device_states(state).await;

    let devices_to_reconcile = std::collections::BTreeSet::<String>::new();
    let mut devices_to_reconcile = devices_to_reconcile;

    for node_id in &order {
        let node = match automation.nodes.iter().find(|n| &n.id == node_id) {
            Some(n) => n,
            None => continue,
        };

        let mut prev_state = {
            let automations = state.automations.read().await;
            automations
                .get(&automation.id)
                .and_then(|a| a.status.node_states.get(node_id))
                .cloned()
                .unwrap_or_default()
        };
        if reset_nodes.contains(node_id) {
            // Make this node look freshly-started so its rising-edge logic
            // re-triggers from the immediate pulse.
            prev_state.last_value = None;
            prev_state.last_checked_at_ms = None;
            prev_state.pending_value = None;
            prev_state.pending_since_ms = None;
        }

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
            &device_states,
            previous_tick_ms,
            tick_ms,
            None,
            false,
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
                | AutomationNodeConfig::VariableChanged { .. }
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

/// The device + action an action node targets, if any.
fn device_action(config: &AutomationNodeConfig) -> Option<(String, ScheduleAction)> {
    match config {
        AutomationNodeConfig::SetDevice { set_device } if !set_device.device_name.is_empty() => {
            Some((set_device.device_name.clone(), set_device.action))
        }
        AutomationNodeConfig::ToggleDevice { toggle_device }
            if !toggle_device.device_name.is_empty() =>
        {
            Some((toggle_device.device_name.clone(), ScheduleAction::Toggle))
        }
        _ => None,
    }
}

/// Predict device state changes over (now_ms, horizon_ms] by fast-forwarding a
/// virtual clock through the automation graphs. Steps every `step_ms`,
/// evaluating each enabled automation with no side effects: time functions
/// resolve against the virtual clock, HTTP blocks replay their last response
/// (held constant for the horizon), and simulated device state evolves as
/// actions would fire. Conditional/expression/variable paths are followed, so
/// the result reflects the state the flowchart actually resolves to — not just
/// the deterministic time triggers.
pub(crate) async fn simulate_forecast(
    state: &AppState,
    now_ms: u128,
    horizon_ms: u128,
    step_ms: u128,
) -> Vec<ForecastEvent> {
    const MAX_EVENTS: usize = 500;
    if step_ms == 0 || horizon_ms <= now_ms {
        return Vec::new();
    }

    // Per-automation simulation state, evolving across ticks.
    struct SimAuto {
        automation: Automation,
        order: Vec<String>,
        incoming: BTreeMap<String, Vec<(String, String)>>,
        states: BTreeMap<String, NodeRuntimeState>,
        variables: BTreeMap<String, Value>,
    }
    let mut sims: Vec<SimAuto> = {
        let guard = state.automations.read().await;
        guard
            .values()
            .filter(|a| a.enabled)
            .filter_map(|a| {
                let order = topo_sort_nodes(&a.nodes, &a.edges)?;
                let mut incoming: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
                for e in &a.edges {
                    incoming
                        .entry(e.target_node.clone())
                        .or_default()
                        .push((e.source_node.clone(), e.source_socket.clone()));
                }
                Some(SimAuto {
                    states: a.status.node_states.clone(),
                    variables: a.variables.clone(),
                    order,
                    incoming,
                    automation: a.clone(),
                })
            })
            .collect()
    };

    // Simulated device on/off, seeded from the live snapshot (keyed by name and
    // nickname). Actions update the entry for their device name; deviceOn() in
    // expressions reads from this evolving map.
    let mut devices = collect_device_states(state).await;
    let mut events: Vec<ForecastEvent> = Vec::new();

    let mut t = now_ms + step_ms;
    while t <= horizon_ms && events.len() < MAX_EVENTS {
        let prev_tick = t - step_ms;
        for sim in &mut sims {
            let mut outputs: BTreeMap<String, Option<bool>> = BTreeMap::new();
            let mut fresh: BTreeMap<String, NodeRuntimeState> = BTreeMap::new();
            for node_id in &sim.order {
                let Some(node) = sim.automation.nodes.iter().find(|n| &n.id == node_id) else {
                    continue;
                };
                let prev_state = sim.states.get(node_id).cloned().unwrap_or_default();
                let input_values: Vec<IncomingInput> = sim
                    .incoming
                    .get(node_id)
                    .map(|sources| {
                        sources
                            .iter()
                            .map(|(src, socket)| {
                                let value = outputs.get(src).copied().unwrap_or(None);
                                let adjusted =
                                    if socket == "no" { value.map(|v| !v) } else { value };
                                IncomingInput { source_node: src.clone(), value: adjusted }
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let (new_output, new_state) = evaluate_node(
                    state,
                    &sim.automation.id,
                    node,
                    &input_values,
                    &prev_state,
                    &mut sim.variables,
                    &devices,
                    prev_tick,
                    t,
                    Some(&fresh),
                    true,
                )
                .await;

                outputs.insert(node_id.clone(), new_output);

                // Rising edge on an action → apply the change to sim state and
                // record it (only when the device actually changes).
                let rising = matches!(
                    (prev_state.last_value, new_output),
                    (None, Some(true)) | (Some(false), Some(true))
                );
                if rising {
                    if let Some((device, action)) = device_action(&node.config) {
                        let current = devices.get(&device).copied();
                        let target = match action {
                            ScheduleAction::On => true,
                            ScheduleAction::Off => false,
                            ScheduleAction::Toggle => !current.unwrap_or(false),
                        };
                        if current != Some(target) {
                            devices.insert(device.clone(), target);
                            events.push(ForecastEvent {
                                at_ms: t,
                                device_name: device,
                                action: if target {
                                    ScheduleAction::On
                                } else {
                                    ScheduleAction::Off
                                },
                                automation_id: sim.automation.id.clone(),
                                automation_name: sim.automation.name.clone(),
                            });
                        }
                    }
                }

                let manages_own_value = matches!(
                    node.config,
                    AutomationNodeConfig::HttpRequest { .. }
                        | AutomationNodeConfig::SetVariable { .. }
                        | AutomationNodeConfig::GetVariable { .. }
                        | AutomationNodeConfig::Expression { .. }
                        | AutomationNodeConfig::VariableChanged { .. }
                );
                let mut merged = new_state;
                merged.last_value = new_output;
                if !manages_own_value {
                    if let Some(v) = new_output {
                        merged.outputs.insert("value".to_string(), v.to_string());
                    }
                }
                fresh.insert(node_id.clone(), merged.clone());
                sim.states.insert(node_id.clone(), merged);
            }
        }
        t += step_ms;
    }

    events.sort_by_key(|e| e.at_ms);
    events.truncate(MAX_EVENTS);
    events
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
    device_states: &BTreeMap<String, bool>,
    previous_tick_ms: u128,
    tick_ms: u128,
    // During a dry run, freshly-computed upstream node states so a node reads
    // this run's outputs (body, status_code, …) instead of the persisted,
    // previous-tick values. `None` in the live engine.
    overrides: Option<&BTreeMap<String, NodeRuntimeState>>,
    // Forecast simulation: don't hit the network — HTTP blocks replay their
    // snapshot response instead of fetching.
    simulate: bool,
) -> (Option<bool>, NodeRuntimeState) {
    let mut next = prev.clone();
    next.last_error = None;

    match &node.config {
        AutomationNodeConfig::ImmediateTrigger => {
            // Fire true exactly once per process start. last_checked_at_ms is
            // transient (#[serde(skip)]), so it's None right after a restart
            // and Some on every subsequent tick this process.
            if prev.last_checked_at_ms.is_none() {
                next.last_checked_at_ms = Some(tick_ms);
                (Some(true), next)
            } else {
                (Some(false), next)
            }
        }
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
        AutomationNodeConfig::Between { between } => {
            let now_dt = DateTime::<Local>::from(
                std::time::UNIX_EPOCH + Duration::from_millis(tick_ms as u64),
            );
            let now_min = now_dt.hour() * 60 + now_dt.minute();
            let now_dow = now_dt.weekday().num_days_from_sunday() as u8; // 0=Sun..6=Sat
            let in_any = if !between.windows.is_empty() {
                between.windows.iter().any(|w| {
                    let day_ok = w.days.is_empty() || w.days.contains(&now_dow);
                    let time_ok = match (expr::parse_hhmm(&w.start), expr::parse_hhmm(&w.end)) {
                        (Some(s), Some(e)) => expr::time_in_window(now_min, s, e),
                        _ => false,
                    };
                    day_ok && time_ok
                })
            } else {
                // Legacy single window (all days).
                match (expr::parse_hhmm(&between.start), expr::parse_hhmm(&between.end)) {
                    (Some(s), Some(e)) => expr::time_in_window(now_min, s, e),
                    _ => false,
                }
            };
            (Some(in_any), next)
        }
        AutomationNodeConfig::VariableChanged { variable_changed } => {
            // Pulse when the variable's value differs from what we last saw.
            // last_body holds the previously-observed value; the first
            // observation just records it (so an existing value isn't an
            // "update").
            let current = variables
                .get(&variable_changed.key)
                .map(expr::to_text)
                .unwrap_or_default();
            let changed = match prev.last_body.as_deref() {
                Some(seen) => seen != current,
                // First observation: fire only if the variable already has a
                // value (it just appeared); an unset variable isn't an update.
                None => !current.is_empty(),
            };
            next.last_body = Some(current.clone());
            next.outputs.clear();
            next.outputs.insert("value".to_string(), current);
            (Some(changed), next)
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
            if simulate {
                // Forecast: replay the snapshot rather than fetching. `next`
                // already carries the snapshot outputs (cloned from prev);
                // treat it as having matched so downstream paths are explored.
                return (Some(prev.last_value.unwrap_or(true)), next);
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
            // Expression mode: evaluate a full boolean expression with access
            // to both $variables and input.* (the wired upstream's outputs).
            if !if_condition.expression.trim().is_empty() {
                let input = fetch_source_outputs(state, automation_id, inputs, overrides).await;
                let matched = {
                    let ctx = EvalContext {
                        variables,
                        input,
                        devices: device_states,
                        now_ms: tick_ms,
                    };
                    match expr::evaluate(&if_condition.expression, &ctx) {
                        Ok(value) => expr::truthy(&value),
                        Err(error) => {
                            next.last_error = Some(error);
                            false
                        }
                    }
                };
                return (Some(matched), next);
            }
            // Builder mode: read one field ($var or input.field) and compare.
            let source_state: Option<NodeRuntimeState> = match inputs.first() {
                Some(input) => match overrides.and_then(|o| o.get(&input.source_node)) {
                    Some(s) => Some(s.clone()),
                    None => {
                        let automations = state.automations.read().await;
                        automations
                            .get(automation_id)
                            .and_then(|a| a.status.node_states.get(&input.source_node))
                            .cloned()
                    }
                },
                None => None,
            };
            let matched =
                evaluate_if_check(if_condition, source_state.as_ref(), variables, device_states);
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
            let input = fetch_source_outputs(state, automation_id, inputs, overrides).await;
            let result = {
                let ctx = EvalContext {
                    variables,
                    input,
                    devices: device_states,
                    now_ms: tick_ms,
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
            let input = fetch_source_outputs(state, automation_id, inputs, overrides).await;
            let result = {
                let ctx = EvalContext {
                    variables,
                    input,
                    devices: device_states,
                    now_ms: tick_ms,
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

/// Snapshot of device power state for the deviceOn/deviceOff/deviceState
/// expression functions. Keyed by both the device name and its nickname (so
/// either works in an expression), for devices with a known snapshot.
pub(crate) async fn collect_device_states(state: &AppState) -> BTreeMap<String, bool> {
    let devices = state.devices.read().await;
    let mut map = BTreeMap::new();
    for (name, device) in devices.iter() {
        if let Some(snap) = device.snapshot.as_ref() {
            map.insert(name.clone(), snap.device_on);
            if !snap.nickname.is_empty() {
                map.insert(snap.nickname.clone(), snap.device_on);
            }
        }
    }
    map
}

/// Build the `input` dictionary an expression sees: the named outputs of
/// the node wired to this block's IN socket, as a JSON object of strings.
/// Reads committed state (one tick behind, same as the IF block).
async fn fetch_source_outputs(
    state: &AppState,
    automation_id: &str,
    inputs: &[IncomingInput],
    overrides: Option<&BTreeMap<String, NodeRuntimeState>>,
) -> Value {
    let source_id = match inputs.first() {
        Some(input) => &input.source_node,
        None => return Value::Object(Default::default()),
    };
    let outputs = match overrides.and_then(|o| o.get(source_id)) {
        Some(s) => s.outputs.clone(),
        None => {
            let automations = state.automations.read().await;
            automations
                .get(automation_id)
                .and_then(|a| a.status.node_states.get(source_id))
                .map(|s| s.outputs.clone())
                .unwrap_or_default()
        }
    };
    let map: serde_json::Map<String, Value> = outputs
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    Value::Object(map)
}

/// Returns the set of nodes downstream of any immediate trigger that is about
/// to fire this tick (i.e. hasn't fired yet this process). Their rising-edge
/// baseline is cleared so the startup pulse actually re-runs them.
async fn compute_immediate_reset_nodes(
    state: &AppState,
    automation: &Automation,
) -> std::collections::BTreeSet<String> {
    let firing: Vec<&str> = {
        let automations = state.automations.read().await;
        let states = automations.get(&automation.id).map(|a| &a.status.node_states);
        automation
            .nodes
            .iter()
            .filter(|n| matches!(n.config, AutomationNodeConfig::ImmediateTrigger))
            .filter(|n| {
                states
                    .and_then(|s| s.get(&n.id))
                    .and_then(|st| st.last_checked_at_ms)
                    .is_none()
            })
            .map(|n| n.id.as_str())
            .collect()
    };
    if firing.is_empty() {
        return std::collections::BTreeSet::new();
    }

    let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in &automation.edges {
        adj.entry(e.source_node.as_str())
            .or_default()
            .push(e.target_node.as_str());
    }
    let mut reset = std::collections::BTreeSet::new();
    let mut queue: std::collections::VecDeque<&str> = firing.into_iter().collect();
    while let Some(node) = queue.pop_front() {
        if let Some(targets) = adj.get(node) {
            for &t in targets {
                if reset.insert(t.to_string()) {
                    queue.push_back(t);
                }
            }
        }
    }
    reset
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

/// Parse an "HH:MM" time-of-day into minutes since midnight (0..1439).
/// Evaluate an IF block's predicate. Reads `cfg.field` from the source
/// node's `outputs` map and applies `cfg.op` against `cfg.value`.
pub(crate) fn evaluate_if_check(
    cfg: &IfConditionCfg,
    source: Option<&NodeRuntimeState>,
    variables: &BTreeMap<String, Value>,
    devices: &BTreeMap<String, bool>,
) -> bool {
    // Field resolution: `device:NAME` reads a device's power state
    // ("true"/"false"), `$name` reads a variable, anything else reads the
    // wired upstream block's output.
    let field_value: Option<String> = if let Some(name) = cfg.field.strip_prefix("device:") {
        devices.get(name.trim()).map(|on| on.to_string())
    } else if let Some(name) = cfg.field.strip_prefix('$') {
        variables.get(name).map(expr::to_text)
    } else {
        source.and_then(|s| lookup_output(s, &cfg.field))
    };
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
        IfOp::Gt | IfOp::Gte | IfOp::Lt | IfOp::Lte => {
            let actual: f64 = match field_value.as_deref().and_then(|s| s.trim().parse().ok()) {
                Some(n) => n,
                None => return false,
            };
            let threshold: f64 = match cfg.value.trim().parse() {
                Ok(n) => n,
                Err(_) => return false,
            };
            match cfg.op {
                IfOp::Gt => actual > threshold,
                IfOp::Gte => actual >= threshold,
                IfOp::Lt => actual < threshold,
                IfOp::Lte => actual <= threshold,
                _ => unreachable!(),
            }
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

/// Trigger blocks that fire at a moment rather than gating on a condition. In
/// the live flow view these are treated as firing so the path below them is
/// visible (Between is a condition gate, so it is NOT included here).
fn is_forced_trigger(config: &AutomationNodeConfig) -> bool {
    matches!(
        config,
        AutomationNodeConfig::ImmediateTrigger
            | AutomationNodeConfig::CronTrigger { .. }
            | AutomationNodeConfig::IntervalTrigger { .. }
            | AutomationNodeConfig::DeviceEventTrigger { .. }
            | AutomationNodeConfig::VariableChanged { .. }
    )
}

/// `target` plus all of its transitive upstream ancestors.
fn upstream_closure(target: &str, edges: &[AutomationEdge]) -> std::collections::BTreeSet<String> {
    let mut closure = std::collections::BTreeSet::new();
    let mut stack = vec![target.to_string()];
    while let Some(id) = stack.pop() {
        if !closure.insert(id.clone()) {
            continue;
        }
        for e in edges {
            if e.target_node == id {
                stack.push(e.source_node.clone());
            }
        }
    }
    closure
}

/// Human description of the side effect an action node would perform. Used
/// only to report intent in a dry run; performs no side effect.
async fn describe_action(state: &AppState, config: &AutomationNodeConfig) -> Option<String> {
    match config {
        AutomationNodeConfig::SetDevice { set_device } => {
            if set_device.device_name.is_empty() {
                return Some("set a device (none chosen)".to_string());
            }
            let verb = match set_device.action {
                ScheduleAction::On => "on",
                ScheduleAction::Off => "off",
                ScheduleAction::Toggle => "toggle",
            };
            Some(format!("would set {} {}", set_device.device_name, verb))
        }
        AutomationNodeConfig::ToggleDevice { toggle_device } => {
            if toggle_device.device_name.is_empty() {
                return Some("toggle a device (none chosen)".to_string());
            }
            Some(format!("would toggle {}", toggle_device.device_name))
        }
        AutomationNodeConfig::FireHook { fire_hook } => {
            if fire_hook.hook_id.is_empty() {
                return Some("fire a hook (none chosen)".to_string());
            }
            let name = {
                let hooks = state.hooks.read().await;
                hooks.get(&fire_hook.hook_id).map(|h| h.name.clone())
            };
            Some(format!(
                "would fire hook {}",
                name.unwrap_or_else(|| fire_hook.hook_id.clone())
            ))
        }
        _ => None,
    }
}

/// A short, distinguishing label for a block in the run panel, so two
/// "Set variable" blocks are told apart by the variable they touch.
fn node_title(config: &AutomationNodeConfig) -> String {
    use AutomationNodeConfig as C;
    let trunc = |s: &str, n: usize| -> String {
        let s = s.trim();
        if s.chars().count() > n {
            format!("{}…", s.chars().take(n).collect::<String>())
        } else {
            s.to_string()
        }
    };
    match config {
        C::ImmediateTrigger => "Immediate".to_string(),
        C::CronTrigger { cron_trigger } => format!("Cron · {}", cron_trigger.cron),
        C::IntervalTrigger { .. } => "Interval".to_string(),
        C::DeviceEventTrigger { device_event_trigger } => {
            if device_event_trigger.device_name.is_empty() {
                "Device event".to_string()
            } else {
                format!("Device · {}", device_event_trigger.device_name)
            }
        }
        C::Between { .. } => "Between".to_string(),
        C::VariableChanged { variable_changed } => {
            if variable_changed.key.is_empty() {
                "Variable changed".to_string()
            } else {
                format!("{} changed", variable_changed.key)
            }
        }
        C::HttpProbe { .. } => "HTTP probe".to_string(),
        C::HttpRequest { http_request } => {
            if http_request.url.is_empty() {
                "HTTP request".to_string()
            } else {
                format!("HTTP {} {}", http_request.method, trunc(&http_request.url, 40))
            }
        }
        C::IfCondition { if_condition } => {
            if !if_condition.expression.trim().is_empty() {
                format!("If {}", trunc(&if_condition.expression, 32))
            } else {
                "If".to_string()
            }
        }
        C::LogicAnd => "AND".to_string(),
        C::LogicOr => "OR".to_string(),
        C::LogicNot => "NOT".to_string(),
        C::Debounce { debounce } => format!("Debounce {}s", debounce.hold_seconds),
        C::SetVariable { set_variable } => {
            if set_variable.key.is_empty() {
                "Set variable".to_string()
            } else {
                format!("Set ${}", set_variable.key)
            }
        }
        C::GetVariable { get_variable } => {
            if get_variable.key.is_empty() {
                "Get variable".to_string()
            } else {
                format!("Get ${}", get_variable.key)
            }
        }
        C::Expression { expression } => {
            if expression.expression.trim().is_empty() {
                "Expression".to_string()
            } else {
                trunc(&expression.expression, 36)
            }
        }
        C::SetDevice { set_device } => {
            let verb = match set_device.action {
                ScheduleAction::On => "on",
                ScheduleAction::Off => "off",
                ScheduleAction::Toggle => "toggle",
            };
            if set_device.device_name.is_empty() {
                "Set device".to_string()
            } else {
                format!("{} → {}", set_device.device_name, verb)
            }
        }
        C::ToggleDevice { toggle_device } => {
            if toggle_device.device_name.is_empty() {
                "Toggle device".to_string()
            } else {
                format!("Toggle {}", toggle_device.device_name)
            }
        }
        C::FireHook { .. } => "Fire hook".to_string(),
    }
}

/// Dry-run the graph with no side effects (devices aren't switched, hooks
/// aren't fired). `target` None runs the whole graph (the always-on live flow
/// view); Some runs that node and everything upstream of it. Each node's
/// rising-edge baseline is reset so it behaves like a fresh trigger, and
/// downstream nodes read this run's freshly-computed outputs. When `simulate`,
/// HTTP blocks replay their snapshot instead of fetching and the pure trigger
/// blocks are treated as firing, so the path the current conditions resolve to
/// lights up. Nothing is persisted.
pub(crate) async fn dry_run_node(
    state: &AppState,
    automation: &Automation,
    target: Option<&str>,
    simulate: bool,
) -> Result<Vec<RunNodeResult>, String> {
    let full_order = topo_sort_nodes(&automation.nodes, &automation.edges)
        .ok_or_else(|| "the graph has a cycle".to_string())?;
    let order: Vec<String> = match target {
        Some(target_id) => {
            if !automation.nodes.iter().any(|n| n.id == target_id) {
                return Err(format!("node '{target_id}' is not in the graph"));
            }
            let closure = upstream_closure(target_id, &automation.edges);
            full_order.into_iter().filter(|id| closure.contains(id)).collect()
        }
        None => full_order,
    };

    let mut incoming: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for e in &automation.edges {
        incoming
            .entry(e.target_node.clone())
            .or_default()
            .push((e.source_node.clone(), e.source_socket.clone()));
    }

    let tick_ms = now_ms();
    let previous_tick_ms = tick_ms.saturating_sub(1000);
    let mut variables = automation.variables.clone();
    let device_states = collect_device_states(state).await;

    let mut outputs: BTreeMap<String, Option<bool>> = BTreeMap::new();
    let mut fresh: BTreeMap<String, NodeRuntimeState> = BTreeMap::new();
    let mut results: Vec<RunNodeResult> = Vec::with_capacity(order.len());

    for node_id in &order {
        let Some(node) = automation.nodes.iter().find(|n| &n.id == node_id) else {
            continue;
        };

        // Reset the rising-edge baseline so the node fires as if freshly
        // triggered (HTTP fetches, triggers pulse) rather than being gated by
        // whatever happened in the live engine.
        let mut prev_state = automation
            .status
            .node_states
            .get(node_id)
            .cloned()
            .unwrap_or_default();
        prev_state.last_value = None;
        prev_state.last_checked_at_ms = None;
        prev_state.pending_value = None;
        prev_state.pending_since_ms = None;

        let input_values: Vec<IncomingInput> = incoming
            .get(node_id)
            .map(|sources| {
                sources
                    .iter()
                    .map(|(src, socket)| {
                        let value = outputs.get(src).copied().unwrap_or(None);
                        let adjusted = if socket == "no" { value.map(|v| !v) } else { value };
                        IncomingInput { source_node: src.clone(), value: adjusted }
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
            &device_states,
            previous_tick_ms,
            tick_ms,
            Some(&fresh),
            simulate,
        )
        .await;

        // In the live view, treat pure triggers as firing so the downstream
        // path is visible; conditions (Between/If/expressions) still reflect
        // current state and decide where it routes.
        let new_output = if simulate && is_forced_trigger(&node.config) {
            Some(true)
        } else {
            new_output
        };

        outputs.insert(node_id.clone(), new_output);

        let is_action = matches!(
            node.config,
            AutomationNodeConfig::SetDevice { .. }
                | AutomationNodeConfig::ToggleDevice { .. }
                | AutomationNodeConfig::FireHook { .. }
        );
        let manages_own_value = matches!(
            node.config,
            AutomationNodeConfig::HttpRequest { .. }
                | AutomationNodeConfig::SetVariable { .. }
                | AutomationNodeConfig::GetVariable { .. }
                | AutomationNodeConfig::Expression { .. }
                | AutomationNodeConfig::VariableChanged { .. }
        );

        let mut out_map = new_state.outputs.clone();
        if !manages_own_value {
            if let Some(v) = new_output {
                out_map.insert("value".to_string(), v.to_string());
            }
        }
        let fired = new_output == Some(true);
        let action = if is_action && fired {
            describe_action(state, &node.config).await
        } else {
            None
        };

        results.push(RunNodeResult {
            node_id: node_id.clone(),
            title: node_title(&node.config),
            value: new_output,
            outputs: out_map,
            error: new_state.last_error.clone(),
            fired,
            action,
        });

        fresh.insert(node_id.clone(), new_state);
    }

    Ok(results)
}

#[cfg(test)]
mod between_tests {
    use crate::automations::expr::{parse_hhmm, time_in_window};

    #[test]
    fn parses_hhmm() {
        assert_eq!(parse_hhmm("07:30"), Some(450));
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("bad"), None);
    }

    #[test]
    fn same_day_window() {
        // 09:00–17:00
        let (s, e) = (540, 1020);
        assert!(time_in_window(600, s, e)); // 10:00 in
        assert!(!time_in_window(480, s, e)); // 08:00 out
        assert!(!time_in_window(1080, s, e)); // 18:00 out
        assert!(time_in_window(540, s, e)); // boundary start
        assert!(time_in_window(1020, s, e)); // boundary end
    }

    #[test]
    fn overnight_window_wraps_midnight() {
        // 07:30–01:00 (the user's example)
        let (s, e) = (450, 60);
        assert!(time_in_window(450, s, e)); // 07:30 in
        assert!(time_in_window(1439, s, e)); // 23:59 in
        assert!(time_in_window(0, s, e)); // 00:00 in
        assert!(time_in_window(60, s, e)); // 01:00 in
        assert!(!time_in_window(61, s, e)); // 01:01 out
        assert!(!time_in_window(449, s, e)); // 07:29 out
    }
}
