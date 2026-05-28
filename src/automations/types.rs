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
    /// Legacy: kept so old state.json files still deserialise. Auto-
    /// converted to HttpRequest at load time; new graphs never write this.
    HttpProbe {
        http_probe: HttpProbeCfg,
    },
    /// Action node: runs a single HTTP request on each input pulse and
    /// records the response (body, status) for downstream If blocks to
    /// inspect. Use Interval/Cron upstream to schedule it.
    HttpRequest {
        http_request: HttpRequestCfg,
    },
    LogicAnd,
    LogicOr,
    LogicNot,
    /// Branches on a property of the connected input block. Reads the
    /// last recorded value/body/status of whatever upstream node is wired
    /// to IN, applies the configured check, fires YES on match else NO.
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

/// On-demand HTTP request used as an action. Runs once per input pulse;
/// records body + status_code in its runtime state so downstream If
/// blocks can branch on them.
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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IfOp {
    /// Field's value is the boolean `true`.
    #[default]
    IsTrue,
    /// Field's stringified value equals `value` (trimmed).
    Equals,
    /// Field's stringified value contains `value`.
    Contains,
    /// Field's value (parsed as integer) is in the range expression `value`
    /// (e.g. "200-299" or "200,404").
    InRange,
}

/// IF block routes a pulse based on a named output of its upstream node.
/// `field` is the output key (e.g. "value", "body", "status_code"); the
/// available keys depend on the upstream node kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct IfConditionCfg {
    #[serde(default = "default_if_field")]
    pub(crate) field: String,
    #[serde(default)]
    pub(crate) op: IfOp,
    #[serde(default)]
    pub(crate) value: String,
    /// Legacy field for the old `{check, value}` shape. Read on load,
    /// translated to `(field, op)` by `migrate_if_blocks`, then dropped on
    /// the next save (skip_serializing).
    #[serde(default, skip_serializing)]
    pub(crate) check: Option<String>,
}

fn default_if_field() -> String {
    "value".to_string()
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
    /// HTTP request: most recent response body (truncated by the probe
    /// reader). Empty for non-HTTP nodes. Persisted so an If node can
    /// branch on it across server restarts.
    #[serde(default)]
    pub(crate) last_body: Option<String>,
    /// HTTP request: most recent status code.
    #[serde(default)]
    pub(crate) last_status_code: Option<u16>,
    /// Named output fields exposed for downstream IF blocks. All values are
    /// stringified — numbers like "200" are parsed back by ops that need
    /// them numeric (e.g. InRange). Different node kinds expose different
    /// keys: most nodes expose "value" (true/false); http_request also
    /// exposes "body", "status_code", and "succeeded".
    #[serde(default)]
    pub(crate) outputs: BTreeMap<String, String>,
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
