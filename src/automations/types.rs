use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::conditions::{default_condition_poll_seconds, default_http_method, default_status_match};
use crate::hooks::HookEvent;
use crate::schedules::default_true;
use crate::state::ScheduleAction;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum AutomationNodeConfig {
    CronTrigger {
        cron_trigger: CronTriggerCfg,
    },
    IntervalTrigger {
        interval_trigger: IntervalTriggerCfg,
    },
    DeviceEventTrigger {
        device_event_trigger: DeviceEventTriggerCfg,
    },
    /// Legacy: polled HTTP probe. Kept so existing automations still
    /// deserialise; freshly-built graphs use `HttpRequest` instead.
    HttpProbe {
        http_probe: HttpProbeCfg,
    },
    /// Action node: runs a single HTTP request on each input pulse and
    /// records `last_value = matched`. Use Interval/Cron upstream to
    /// schedule it.
    HttpRequest {
        http_request: HttpRequestCfg,
    },
    LogicAnd,
    LogicOr,
    LogicNot,
    /// Branches based on a target node's last recorded value. Has two
    /// outputs: `yes` fires when the referenced node's last value was
    /// `true`, `no` when it was `false` (or has never produced a value).
    IfCondition {
        if_condition: IfConditionCfg,
    },
    Debounce {
        debounce: DebounceCfg,
    },
    SetDevice {
        set_device: SetDeviceCfg,
    },
    ToggleDevice {
        toggle_device: ToggleDeviceCfg,
    },
    FireHook {
        fire_hook: FireHookCfg,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CronTriggerCfg {
    pub(crate) cron: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct IntervalTriggerCfg {
    pub(crate) on_seconds: u64,
    pub(crate) off_seconds: u64,
    pub(crate) start_action: ScheduleAction,
    #[serde(default)]
    pub(crate) starts_at_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DeviceEventTriggerCfg {
    pub(crate) device_name: String,
    pub(crate) event: HookEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HttpProbeCfg {
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
    #[serde(default)]
    pub(crate) min_stable_seconds: u64,
}

/// On-demand HTTP request used as an action. Same matching rules as the
/// legacy HttpProbe but no internal polling — runs once per input pulse.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HttpRequestCfg {
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct IfConditionCfg {
    /// Logical node id whose `last_value` we inspect when an input pulse
    /// arrives. Empty string = always routes to `no` (unconfigured).
    #[serde(default)]
    pub(crate) target_node: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DebounceCfg {
    pub(crate) hold_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SetDeviceCfg {
    pub(crate) device_name: String,
    pub(crate) action: ScheduleAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ToggleDeviceCfg {
    pub(crate) device_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FireHookCfg {
    pub(crate) hook_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AutomationNode {
    pub(crate) id: String,
    pub(crate) config: AutomationNodeConfig,
    #[serde(default)]
    pub(crate) x: f64,
    #[serde(default)]
    pub(crate) y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AutomationEdge {
    pub(crate) id: String,
    pub(crate) source_node: String,
    pub(crate) target_node: String,
    #[serde(default = "default_socket_out")]
    pub(crate) source_socket: String,
    #[serde(default = "default_socket_in")]
    pub(crate) target_socket: String,
}

fn default_socket_out() -> String {
    "out".to_string()
}
fn default_socket_in() -> String {
    "in".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct NodeRuntimeState {
    #[serde(default)]
    pub(crate) last_value: Option<bool>,
    #[serde(default)]
    pub(crate) last_fired_at_ms: Option<u128>,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
    // Internal state for cron/interval/probe/debounce — not exposed in the
    // public view JSON. Reset on server restart.
    #[serde(skip)]
    pub(crate) last_checked_at_ms: Option<u128>,
    #[serde(skip)]
    pub(crate) pending_value: Option<bool>,
    #[serde(skip)]
    pub(crate) pending_since_ms: Option<u128>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AutomationStatus {
    #[serde(default)]
    pub(crate) last_fired_at_ms: Option<u128>,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
    #[serde(default)]
    pub(crate) node_states: BTreeMap<String, NodeRuntimeState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Automation {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) nodes: Vec<AutomationNode>,
    #[serde(default)]
    pub(crate) edges: Vec<AutomationEdge>,
    #[serde(default)]
    pub(crate) created_at_ms: u128,
    #[serde(default)]
    pub(crate) status: AutomationStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CreateAutomationRequest {
    pub(crate) name: String,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) nodes: Vec<AutomationNode>,
    #[serde(default)]
    pub(crate) edges: Vec<AutomationEdge>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UpdateAutomationRequest {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) enabled: Option<bool>,
    #[serde(default)]
    pub(crate) nodes: Option<Vec<AutomationNode>>,
    #[serde(default)]
    pub(crate) edges: Option<Vec<AutomationEdge>>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AutomationListResponse {
    pub(crate) automations: Vec<Automation>,
}
