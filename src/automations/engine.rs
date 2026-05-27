use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Local};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::automations::types::{
    Automation, AutomationEdge, AutomationNode, AutomationNodeConfig, NodeRuntimeState,
};
use crate::conditions::{ConditionConfig, probe_condition_once};
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

    let mut incoming: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in &automation.nodes {
        incoming.entry(n.id.clone()).or_default();
    }
    for e in &automation.edges {
        incoming
            .entry(e.target_node.clone())
            .or_default()
            .push(e.source_node.clone());
    }

    let mut outputs: BTreeMap<String, Option<bool>> = BTreeMap::new();
    let mut transitions: Vec<(String, AutomationNodeConfig)> = Vec::new();
    let mut node_state_updates: BTreeMap<String, NodeRuntimeState> = BTreeMap::new();

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

        let input_values: Vec<Option<bool>> = incoming
            .get(node_id)
            .map(|sources| {
                sources
                    .iter()
                    .map(|src| outputs.get(src).copied().unwrap_or(None))
                    .collect()
            })
            .unwrap_or_default();

        let (new_output, new_state) = evaluate_node(
            state,
            &automation.id,
            node,
            &input_values,
            &prev_state,
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

pub(crate) async fn evaluate_node(
    state: &AppState,
    automation_id: &str,
    node: &AutomationNode,
    inputs: &[Option<bool>],
    prev: &NodeRuntimeState,
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
        AutomationNodeConfig::HttpProbe { http_probe } => {
            let interval_ms = (http_probe.poll_seconds as u128) * 1000;
            let due = match next.last_checked_at_ms {
                None => true,
                Some(last) => tick_ms.saturating_sub(last) >= interval_ms,
            };
            if !due {
                return (prev.last_value, next);
            }

            let probe = ConditionConfig {
                id: format!("auto/{automation_id}/node/{}", node.id),
                name: node.id.clone(),
                device_name: String::new(),
                url: http_probe.url.clone(),
                method: http_probe.method.clone(),
                headers: http_probe.headers.clone(),
                body: http_probe.body.clone(),
                status_match: http_probe.status_match.clone(),
                body_contains: http_probe.body_contains.clone(),
                poll_seconds: http_probe.poll_seconds,
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
                min_stable_seconds: http_probe.min_stable_seconds,
                pending_value: None,
                pending_since_ms: None,
            };

            let outcome = probe_condition_once(&state.http_client, &probe).await;
            next.last_checked_at_ms = Some(tick_ms);
            if let Some(error) = outcome.error {
                next.last_error = Some(error);
            }

            let new_value = outcome.passing;
            let stable_ms = (http_probe.min_stable_seconds as u128) * 1000;

            let committed = match prev.last_value {
                Some(prev_val) if prev_val == new_value => {
                    next.pending_value = None;
                    next.pending_since_ms = None;
                    Some(new_value)
                }
                _ if stable_ms == 0 => {
                    next.pending_value = None;
                    next.pending_since_ms = None;
                    Some(new_value)
                }
                _ => match prev.pending_value {
                    Some(pending) if pending == new_value => {
                        let since = prev.pending_since_ms.unwrap_or(tick_ms);
                        if tick_ms.saturating_sub(since) >= stable_ms {
                            next.pending_value = None;
                            next.pending_since_ms = None;
                            Some(new_value)
                        } else {
                            next.pending_value = Some(new_value);
                            next.pending_since_ms = Some(since);
                            prev.last_value
                        }
                    }
                    _ => {
                        next.pending_value = Some(new_value);
                        next.pending_since_ms = Some(tick_ms);
                        prev.last_value
                    }
                },
            };

            (committed, next)
        }
        AutomationNodeConfig::LogicAnd => {
            if inputs.is_empty() {
                return (Some(false), next);
            }
            let mut all_true = true;
            for v in inputs {
                match v {
                    Some(true) => continue,
                    Some(false) => {
                        all_true = false;
                        break;
                    }
                    None => {
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
            let any_true = inputs.iter().any(|v| matches!(v, Some(true)));
            (Some(any_true), next)
        }
        AutomationNodeConfig::LogicNot => {
            let v = inputs.first().copied().flatten();
            (v.map(|b| !b), next)
        }
        AutomationNodeConfig::Debounce { debounce } => {
            let new_value = inputs.first().copied().flatten();
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
        AutomationNodeConfig::SetDevice { .. }
        | AutomationNodeConfig::ToggleDevice { .. }
        | AutomationNodeConfig::FireHook { .. } => {
            // Actions just propagate the maximum of their inputs so they
            // can in turn drive other actions if connected.
            let active = inputs.iter().any(|v| matches!(v, Some(true)));
            (Some(active), next)
        }
    }
}

pub(crate) async fn execute_action(
    state: &AppState,
    automation_id: &str,
    node_id: &str,
    config: &AutomationNodeConfig,
) -> Result<Option<String>> {
    match config {
        AutomationNodeConfig::SetDevice { set_device } => {
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
