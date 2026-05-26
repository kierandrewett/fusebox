export type NodeKind =
  | "cron_trigger"
  | "interval_trigger"
  | "device_event_trigger"
  | "http_probe"
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
  | { kind: "http_probe"; http_probe: HttpProbeConfig }
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
