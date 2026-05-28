import type { NodeKind, NodeConfig } from "../types";

export interface SocketSpec {
  key: string;
  label: string;
  variant?: "default" | "yes" | "no";
}

/** A named data field that a node exposes for downstream IF blocks. */
export interface DataOutputSpec {
  key: string;
  label: string;
}

export interface NodeTemplate {
  kind: NodeKind;
  label: string;
  category: "trigger" | "logic" | "action";
  description: string;
  defaultConfig: () => NodeConfig;
  hasInput: boolean;
  /** Output sockets. Empty for terminal actions. Most nodes have a single
   *  "OK" output. IfCondition has two — yes/no. */
  outputs: SocketSpec[];
  /** Data outputs the IF block can branch on. Every node exposes "value"
   *  (the boolean pulse); http_request additionally exposes the response
   *  body, status_code, and succeeded flag. */
  dataOutputs: DataOutputSpec[];
  /** Hide from the palette but still render in the editor for legacy data. */
  hidden?: boolean;
}

const SINGLE_OK: SocketSpec[] = [{ key: "out", label: "OK", variant: "default" }];
const DEFAULT_DATA_OUTPUTS: DataOutputSpec[] = [{ key: "value", label: "Value (true/false)" }];

export const NODE_TEMPLATES: NodeTemplate[] = [
  {
    kind: "immediate_trigger",
    label: "Immediate",
    category: "trigger",
    description:
      "Fires once when Fusebox starts. Use it to prime caches or set initial state. Runs before scheduled triggers.",
    hasInput: false,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({ kind: "immediate_trigger" }),
  },
  {
    kind: "cron_trigger",
    label: "Cron",
    category: "trigger",
    description: "Emits a pulse on a cron schedule (5-field).",
    hasInput: false,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({ kind: "cron_trigger", cron_trigger: { cron: "0 8 * * *" } }),
  },
  {
    kind: "interval_trigger",
    label: "Interval",
    category: "trigger",
    description: "Alternates on/off pulses at a fixed cadence.",
    hasInput: false,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({
      kind: "interval_trigger",
      interval_trigger: {
        on_seconds: 600,
        off_seconds: 600,
        start_action: "on",
        starts_at_ms: null,
      },
    }),
  },
  {
    kind: "device_event_trigger",
    label: "Device event",
    category: "trigger",
    description: "Fires when a device goes on/off/online/offline.",
    hasInput: false,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({
      kind: "device_event_trigger",
      device_event_trigger: { device_name: "", event: "on" },
    }),
  },
  {
    kind: "between",
    label: "Between",
    category: "trigger",
    description:
      "Routes YES while the current time is between two times of day (windows may wrap past midnight), otherwise NO.",
    hasInput: false,
    outputs: [
      { key: "yes", label: "YES", variant: "yes" },
      { key: "no", label: "NO", variant: "no" },
    ],
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({
      kind: "between",
      between: { start: "07:30", end: "22:00" },
    }),
  },
  {
    kind: "http_request",
    label: "HTTP request",
    category: "action",
    description:
      "Sends an HTTP request when triggered, then records the response. Pair with an If block to branch on body or status.",
    hasInput: true,
    outputs: SINGLE_OK,
    dataOutputs: [
      { key: "value", label: "Matched (true/false)" },
      { key: "body", label: "Response body" },
      { key: "status_code", label: "Status code" },
      { key: "succeeded", label: "Succeeded (true/false)" },
    ],
    defaultConfig: () => ({
      kind: "http_request",
      http_request: {
        url: "",
        method: "GET",
        headers: {},
        body: null,
        status_match: "200-299",
      },
    }),
  },
  {
    kind: "if_condition",
    label: "If",
    category: "logic",
    description:
      "Routes the input pulse to YES or NO based on a named output of the upstream block.",
    hasInput: true,
    outputs: [
      { key: "yes", label: "YES", variant: "yes" },
      { key: "no", label: "NO", variant: "no" },
    ],
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({
      kind: "if_condition",
      if_condition: { expression: "", field: "value", op: "is_true", value: "" },
    }),
  },
  {
    kind: "logic_and",
    label: "AND",
    category: "logic",
    description: "Outputs true when all connected inputs are true.",
    hasInput: true,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({ kind: "logic_and" }),
  },
  {
    kind: "logic_or",
    label: "OR",
    category: "logic",
    description: "Outputs true when any connected input is true.",
    hasInput: true,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({ kind: "logic_or" }),
  },
  {
    kind: "logic_not",
    label: "NOT",
    category: "logic",
    description: "Inverts the connected input.",
    hasInput: true,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({ kind: "logic_not" }),
  },
  {
    kind: "debounce",
    label: "Debounce",
    category: "logic",
    description: "Holds an input value steady for N seconds before passing it on.",
    hasInput: true,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({ kind: "debounce", debounce: { hold_seconds: 30 } }),
  },
  {
    kind: "expression",
    label: "Expression",
    category: "logic",
    description:
      "Evaluates an expression (math, strings, jsonEncode/decode, …) and exposes the result as 'value'.",
    hasInput: true,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({ kind: "expression", expression: { expression: "" } }),
  },
  {
    kind: "set_variable",
    label: "Set variable",
    category: "logic",
    description:
      "Stores the result of an expression in a named variable when the input fires.",
    hasInput: true,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({
      kind: "set_variable",
      set_variable: { key: "", expression: "" },
    }),
  },
  {
    kind: "get_variable",
    label: "Get variable",
    category: "logic",
    description: "Reads a stored variable and exposes it as 'value' for downstream blocks.",
    hasInput: true,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({ kind: "get_variable", get_variable: { key: "" } }),
  },
  {
    kind: "set_device",
    label: "Set device",
    category: "action",
    description: "Sets a device on/off/toggle when the input fires. OK fires after, so you can chain actions.",
    hasInput: true,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({
      kind: "set_device",
      set_device: { device_name: "", action: "on" },
    }),
  },
  {
    kind: "toggle_device",
    label: "Toggle device",
    category: "action",
    description: "Toggles a device every time the input fires. OK fires after, so you can chain actions.",
    hasInput: true,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({
      kind: "toggle_device",
      toggle_device: { device_name: "" },
    }),
  },
  {
    kind: "fire_hook",
    label: "Fire hook",
    category: "action",
    description: "Triggers a named hook when the input fires. OK fires after, so you can chain actions.",
    hasInput: true,
    outputs: SINGLE_OK,
    dataOutputs: DEFAULT_DATA_OUTPUTS,
    defaultConfig: () => ({
      kind: "fire_hook",
      fire_hook: { hook_id: "" },
    }),
  },
];

export function templateFor(kind: NodeKind): NodeTemplate {
  const tpl = NODE_TEMPLATES.find((t) => t.kind === kind);
  if (!tpl) throw new Error(`unknown node kind: ${kind}`);
  return tpl;
}

const NODE_ICONS: Record<NodeKind, string> = {
  immediate_trigger: "⚡",
  cron_trigger: "⏰",
  interval_trigger: "↻",
  device_event_trigger: "⚡",
  between: "⏲",
  http_request: "🌐",
  if_condition: "?",
  logic_and: "∧",
  logic_or: "∨",
  logic_not: "¬",
  debounce: "⏳",
  expression: "ƒ",
  set_variable: "=",
  get_variable: "x",
  set_device: "▶",
  toggle_device: "⇄",
  fire_hook: "🔔",
};

export function iconFor(kind: NodeKind): string {
  return NODE_ICONS[kind] ?? "•";
}
