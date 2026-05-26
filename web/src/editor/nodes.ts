import type { NodeKind, NodeConfig } from "../types";

export interface NodeTemplate {
  kind: NodeKind;
  label: string;
  category: "trigger" | "logic" | "action";
  description: string;
  defaultConfig: () => NodeConfig;
  hasInput: boolean;
  hasOutput: boolean;
}

export const NODE_TEMPLATES: NodeTemplate[] = [
  {
    kind: "cron_trigger",
    label: "Cron",
    category: "trigger",
    description: "Emits a pulse on a cron schedule (5-field).",
    hasInput: false,
    hasOutput: true,
    defaultConfig: () => ({ kind: "cron_trigger", cron_trigger: { cron: "0 8 * * *" } }),
  },
  {
    kind: "interval_trigger",
    label: "Interval",
    category: "trigger",
    description: "Alternates on/off pulses at a fixed cadence.",
    hasInput: false,
    hasOutput: true,
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
    hasOutput: true,
    defaultConfig: () => ({
      kind: "device_event_trigger",
      device_event_trigger: { device_name: "", event: "on" },
    }),
  },
  {
    kind: "http_probe",
    label: "HTTP probe",
    category: "trigger",
    description: "Polls a URL; outputs true when the response matches.",
    hasInput: false,
    hasOutput: true,
    defaultConfig: () => ({
      kind: "http_probe",
      http_probe: {
        url: "",
        method: "GET",
        headers: {},
        body: null,
        status_match: "200-299",
        body_contains: null,
        poll_seconds: 60,
        min_stable_seconds: 0,
      },
    }),
  },
  {
    kind: "logic_and",
    label: "AND",
    category: "logic",
    description: "Outputs true when all connected inputs are true.",
    hasInput: true,
    hasOutput: true,
    defaultConfig: () => ({ kind: "logic_and" }),
  },
  {
    kind: "logic_or",
    label: "OR",
    category: "logic",
    description: "Outputs true when any connected input is true.",
    hasInput: true,
    hasOutput: true,
    defaultConfig: () => ({ kind: "logic_or" }),
  },
  {
    kind: "logic_not",
    label: "NOT",
    category: "logic",
    description: "Inverts the connected input.",
    hasInput: true,
    hasOutput: true,
    defaultConfig: () => ({ kind: "logic_not" }),
  },
  {
    kind: "debounce",
    label: "Debounce",
    category: "logic",
    description: "Holds an input value steady for N seconds before passing it on.",
    hasInput: true,
    hasOutput: true,
    defaultConfig: () => ({ kind: "debounce", debounce: { hold_seconds: 30 } }),
  },
  {
    kind: "set_device",
    label: "Set device",
    category: "action",
    description: "Sets a device on/off/toggle when the input fires.",
    hasInput: true,
    hasOutput: false,
    defaultConfig: () => ({
      kind: "set_device",
      set_device: { device_name: "", action: "on" },
    }),
  },
  {
    kind: "toggle_device",
    label: "Toggle device",
    category: "action",
    description: "Toggles a device every time the input fires.",
    hasInput: true,
    hasOutput: false,
    defaultConfig: () => ({
      kind: "toggle_device",
      toggle_device: { device_name: "" },
    }),
  },
  {
    kind: "fire_hook",
    label: "Fire hook",
    category: "action",
    description: "Triggers a named hook when the input fires.",
    hasInput: true,
    hasOutput: false,
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
