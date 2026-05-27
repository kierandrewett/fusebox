use crate::legacy::{
    Automation, AutomationEdge, AutomationNode, AutomationNodeConfig, AutomationStatus,
    ConditionAction, CronTriggerCfg, HttpProbeCfg, IntervalTriggerCfg, SetDeviceCfg,
};
use crate::state::{PersistedState, ScheduleAction, ScheduleKind};
use crate::time::now_ms;

/// Convert legacy `ScheduleConfig` and `ConditionConfig` entries into
/// equivalent `Automation` flowcharts. The original collections are left in
/// place inside `persisted` so the next save still records them as a backup;
/// the engine no longer reads them once automations exist.
pub(crate) fn migrate_to_automations(persisted: &mut PersistedState) {
    if !persisted.automations.is_empty() {
        return;
    }

    let now = now_ms();
    let mut next_id = 0u64;
    let mut new_id = || {
        next_id += 1;
        format!("mig-{}-{}", now, next_id)
    };

    // 1) Each ConditionConfig becomes an Automation:
    //    [HttpProbe] -> [LogicNot?] -> [SetDevice(action_on_pass/fail)]
    //
    // We always emit a Set Device action when at least one action is
    // configured. Pass-action and fail-action are split into two parallel
    // branches off the probe so both can fire.
    for (id, cond) in &persisted.conditions {
        let mut nodes: Vec<AutomationNode> = Vec::new();
        let mut edges: Vec<AutomationEdge> = Vec::new();

        let probe_id = format!("probe-{id}");
        nodes.push(AutomationNode {
            id: probe_id.clone(),
            x: 80.0,
            y: 80.0,
            config: AutomationNodeConfig::HttpProbe {
                http_probe: HttpProbeCfg {
                    url: cond.url.clone(),
                    method: cond.method.clone(),
                    headers: cond.headers.clone(),
                    body: cond.body.clone(),
                    status_match: cond.status_match.clone(),
                    body_contains: cond.body_contains.clone(),
                    poll_seconds: cond.poll_seconds,
                    min_stable_seconds: cond.min_stable_seconds,
                },
            },
        });

        let mut next_y = 80.0_f64;
        if let Some(pass_action) = cond.action_on_pass {
            let act_id = format!("act-pass-{id}");
            nodes.push(AutomationNode {
                id: act_id.clone(),
                x: 420.0,
                y: next_y,
                config: AutomationNodeConfig::SetDevice {
                    set_device: SetDeviceCfg {
                        device_name: cond.device_name.clone(),
                        action: match pass_action {
                            ConditionAction::On => ScheduleAction::On,
                            ConditionAction::Off => ScheduleAction::Off,
                        },
                    },
                },
            });
            edges.push(AutomationEdge {
                id: new_id(),
                source_node: probe_id.clone(),
                target_node: act_id,
            });
            next_y += 160.0;
        }

        if let Some(fail_action) = cond.action_on_fail {
            let not_id = format!("not-fail-{id}");
            nodes.push(AutomationNode {
                id: not_id.clone(),
                x: 250.0,
                y: next_y,
                config: AutomationNodeConfig::LogicNot,
            });
            edges.push(AutomationEdge {
                id: new_id(),
                source_node: probe_id.clone(),
                target_node: not_id.clone(),
            });
            let act_id = format!("act-fail-{id}");
            nodes.push(AutomationNode {
                id: act_id.clone(),
                x: 420.0,
                y: next_y,
                config: AutomationNodeConfig::SetDevice {
                    set_device: SetDeviceCfg {
                        device_name: cond.device_name.clone(),
                        action: match fail_action {
                            ConditionAction::On => ScheduleAction::On,
                            ConditionAction::Off => ScheduleAction::Off,
                        },
                    },
                },
            });
            edges.push(AutomationEdge {
                id: new_id(),
                source_node: not_id,
                target_node: act_id,
            });
        }

        let auto_id = format!("auto-cond-{id}");
        persisted.automations.insert(
            auto_id.clone(),
            Automation {
                id: auto_id,
                name: if cond.name.is_empty() {
                    format!("Condition: {}", cond.device_name)
                } else {
                    cond.name.clone()
                },
                enabled: cond.enabled,
                nodes,
                edges,
                created_at_ms: if cond.created_at_ms == 0 {
                    now
                } else {
                    cond.created_at_ms
                },
                status: AutomationStatus::default(),
            },
        );
    }

    // 2) Each ScheduleConfig becomes an Automation:
    //    [CronTrigger | IntervalTrigger] -> [SetDevice]
    for (id, sched) in &persisted.schedules {
        let mut nodes: Vec<AutomationNode> = Vec::new();
        let mut edges: Vec<AutomationEdge> = Vec::new();

        let trigger_id = format!("trig-{id}");
        match sched.kind {
            ScheduleKind::Cron => {
                let Some(cron) = sched.cron.clone() else {
                    continue;
                };
                nodes.push(AutomationNode {
                    id: trigger_id.clone(),
                    x: 80.0,
                    y: 80.0,
                    config: AutomationNodeConfig::CronTrigger {
                        cron_trigger: CronTriggerCfg { cron },
                    },
                });
                let Some(action) = sched.action else {
                    continue;
                };
                let act_id = format!("act-{id}");
                nodes.push(AutomationNode {
                    id: act_id.clone(),
                    x: 380.0,
                    y: 80.0,
                    config: AutomationNodeConfig::SetDevice {
                        set_device: SetDeviceCfg {
                            device_name: sched.device_name.clone(),
                            action,
                        },
                    },
                });
                edges.push(AutomationEdge {
                    id: new_id(),
                    source_node: trigger_id,
                    target_node: act_id,
                });
            }
            ScheduleKind::Interval => {
                let on_seconds = sched.on_seconds.unwrap_or(0);
                let off_seconds = sched.off_seconds.unwrap_or(0);
                let start_action = sched.start_action.unwrap_or(ScheduleAction::On);
                nodes.push(AutomationNode {
                    id: trigger_id.clone(),
                    x: 80.0,
                    y: 80.0,
                    config: AutomationNodeConfig::IntervalTrigger {
                        interval_trigger: IntervalTriggerCfg {
                            on_seconds,
                            off_seconds,
                            start_action,
                            starts_at_ms: sched.starts_at_ms,
                        },
                    },
                });
                let act_id = format!("act-{id}");
                nodes.push(AutomationNode {
                    id: act_id.clone(),
                    x: 380.0,
                    y: 80.0,
                    config: AutomationNodeConfig::SetDevice {
                        set_device: SetDeviceCfg {
                            device_name: sched.device_name.clone(),
                            // Interval flips state internally; the
                            // SetDevice action just relays whatever the
                            // trigger emits.
                            action: ScheduleAction::Toggle,
                        },
                    },
                });
                edges.push(AutomationEdge {
                    id: new_id(),
                    source_node: trigger_id,
                    target_node: act_id,
                });
            }
        }

        let auto_id = format!("auto-sched-{id}");
        persisted.automations.insert(
            auto_id.clone(),
            Automation {
                id: auto_id,
                name: sched
                    .label
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("Schedule: {}", sched.device_name)),
                enabled: sched.enabled,
                nodes,
                edges,
                created_at_ms: if sched.created_at_ms == 0 {
                    now
                } else {
                    sched.created_at_ms
                },
                status: AutomationStatus::default(),
            },
        );
    }

    // Original schedules/conditions are left in `persisted` as historical
    // backup but emptied out for the runtime once the automation engine is
    // the source of truth. Clear them now so they don't double-fire.
    persisted.schedules.clear();
    persisted.conditions.clear();
}
