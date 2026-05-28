export type NodeKind =
  | "cron_trigger"
  | "interval_trigger"
  | "device_event_trigger"
  | "http_request"
  | "if_condition"
  | "logic_and"
  | "logic_or"
  | "logic_not"
  | "debounce"
  | "set_device"
  | "toggle_device"
  | "fire_hook";

export type ScheduleAction = "on" | "off" | "toggle";
export type DeviceEvent = "on" | "off" | "online" | "offline";

export interface CronTriggerConfig {
  cron: string;
}
export interface IntervalTriggerConfig {
  on_seconds: number;
  off_seconds: number;
  start_action: ScheduleAction;
  starts_at_ms?: number | null;
}
export interface DeviceEventTriggerConfig {
  device_name: string;
  event: DeviceEvent;
}
export interface HttpProbeConfig {
  url: string;
  method: string;
  headers: Record<string, string>;
  body?: string | null;
  status_match: string;
  body_contains?: string | null;
  poll_seconds: number;
  min_stable_seconds: number;
}

export interface HttpRequestConfig {
  url: string;
  method: string;
  headers: Record<string, string>;
  body?: string | null;
  status_match: string;
}

export type IfOp = "is_true" | "equals" | "contains" | "in_range";

export interface IfConditionConfig {
  /** Name of the upstream node's output to inspect (e.g. "value", "body"). */
  field: string;
  op: IfOp;
  value: string;
}
export interface DebounceConfig {
  hold_seconds: number;
}
export interface SetDeviceConfig {
  device_name: string;
  action: ScheduleAction;
}
export interface ToggleDeviceConfig {
  device_name: string;
}
export interface FireHookConfig {
  hook_id: string;
}

export type NodeConfig =
  | { kind: "cron_trigger"; cron_trigger: CronTriggerConfig }
  | { kind: "interval_trigger"; interval_trigger: IntervalTriggerConfig }
  | { kind: "device_event_trigger"; device_event_trigger: DeviceEventTriggerConfig }
  | { kind: "http_request"; http_request: HttpRequestConfig }
  | { kind: "if_condition"; if_condition: IfConditionConfig }
  | { kind: "logic_and" }
  | { kind: "logic_or" }
  | { kind: "logic_not" }
  | { kind: "debounce"; debounce: DebounceConfig }
  | { kind: "set_device"; set_device: SetDeviceConfig }
  | { kind: "toggle_device"; toggle_device: ToggleDeviceConfig }
  | { kind: "fire_hook"; fire_hook: FireHookConfig };

export interface AutomationNode {
  id: string;
  config: NodeConfig;
  x: number;
  y: number;
}

export interface AutomationEdge {
  id: string;
  source_node: string;
  target_node: string;
  source_socket?: string;
  target_socket?: string;
}

export interface AutomationStatus {
  last_fired_at_ms?: number | null;
  last_error?: string | null;
  node_states: Record<string, NodeRuntimeState>;
}

export interface NodeRuntimeState {
  last_value?: boolean | null;
  last_fired_at_ms?: number | null;
  last_error?: string | null;
}

export interface Automation {
  id: string;
  name: string;
  enabled: boolean;
  nodes: AutomationNode[];
  edges: AutomationEdge[];
  created_at_ms: number;
  status: AutomationStatus;
}

export interface DeviceSummary {
  name: string;
  nickname: string;
  ip: string;
  model: string;
}

export interface HookSummary {
  id: string;
  name: string;
}


export interface EnergyView {
  current_power_mw?: number | null;
  current_power_w?: number | null;
  today_energy_wh: number;
  month_energy_wh: number;
  today_cost_pence: number;
  month_cost_pence: number;
  today_runtime_minutes: number;
  month_runtime_minutes: number;
}

export interface Device extends DeviceSummary {
  configured_model: string;
  device_type: string;
  device_on?: boolean | null;
  on_time_seconds?: number | null;
  energy?: EnergyView | null;
  last_error?: string | null;
  discovered_at_ms: number;
  updated_at_ms?: number | null;
  manual_override?: boolean | null;
  manual_override_until_ms?: number | null;
  schedule_intent?: boolean | null;
  condition_intent?: boolean | null;
  effective_intent?: boolean | null;
}

export interface DeviceListResponse {
  devices: Device[];
  updated_at_ms: number;
  energy_price_pence_per_kwh: number;
  scan_error?: string | null;
}

export interface Hook extends HookSummary {
  url: string;
  method: string;
  headers: Record<string, string>;
  body?: string | null;
  enabled: boolean;
  event_filter: string[];
  device_filter: string[];
  created_at_ms: number;
  last_fired_at_ms?: number | null;
  last_event?: string | null;
  last_status_code?: number | null;
  last_error?: string | null;
}

export interface UsageHistoryPoint { timestamp_ms: number; value: number }
export interface UsageHistorySeries { device_name: string; points: UsageHistoryPoint[] }
export interface UsageHistoryResponse { series: UsageHistorySeries[]; totals: UsageHistoryPoint[]; errors: Array<{ device_name: string; message: string }>; range: string }
