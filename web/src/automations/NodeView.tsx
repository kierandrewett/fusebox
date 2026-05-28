import { useCallback, useEffect, useRef, useState } from "react";
import { Presets } from "rete-react-plugin";
import type { FlowNode } from "./createEditor";
import type {
  BetweenConfig,
  BetweenWindow,
  DeviceEvent,
  IfOp,
  IntervalTriggerConfig,
  NodeConfig,
  NodeKind,
  ScheduleAction,
} from "../types";
import { templateFor, iconFor, type DataOutputSpec } from "./nodes";
import { ExpressionInput } from "./ExpressionInput";

const { RefSocket } = Presets.classic;

interface Props {
  data: FlowNode;
  emit: (event: any) => void;
}

interface PreviewResult {
  ok: boolean;
  result_text?: string;
  error?: string;
  input_fields: string[];
}

export interface EditorCtx {
  devices: () => { name: string; nickname: string }[];
  hooks: () => { id: string; name: string }[];
  listNodes?: () => { id: string; kind: string; label: string }[];
  findUpstreamKind?: (targetReteId: string) => NodeKind | null;
  findUpstreamLogicalId?: (targetReteId: string) => string | null;
  variableNames?: () => string[];
  previewExpression?: (upstreamId: string | null, expression: string) => Promise<PreviewResult>;
  /** Open the inspector for this node (by Rete id). */
  selectNode?: (reteId: string) => void;
  /** Whether this node is currently selected, for the highlight. */
  isSelected?: (reteId: string) => boolean;
  subscribeContext?: (cb: () => void) => () => void;
}

const EMPTY_CTX: EditorCtx = { devices: () => [], hooks: () => [] };

export function NodeView({ data, emit }: Props) {
  const [ctx, setCtx] = useState<EditorCtx>(EMPTY_CTX);
  const [, force] = useState(0);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const downRef = useRef<{ x: number; y: number; onPin: boolean } | null>(null);
  const tpl = templateFor(data.config.kind);
  const inputs = Object.entries(data.inputs);
  const outputs = Object.entries(data.outputs);

  // Rete renders each node in its own React root, so the parent's React
  // context can't reach us. We bridge by attaching the editor context to a
  // DOM ancestor (`__fuseboxCtx`) and walking up to find it the moment the
  // ref binds.
  const setContainer = useCallback((el: HTMLDivElement | null) => {
    containerRef.current = el;
    if (!el) return;
    for (let cur: HTMLElement | null = el; cur; cur = cur.parentElement) {
      const found = (cur as any).__fuseboxCtx as EditorCtx | undefined;
      if (found) {
        setCtx(found);
        return;
      }
    }
  }, []);

  // Once ctx is found, subscribe to its change notifier so the summary and
  // the selected-highlight refresh when devices/hooks/selection change.
  useEffect(() => {
    if (!ctx.subscribeContext) return;
    return ctx.subscribeContext(() => force((n) => n + 1));
  }, [ctx]);

  // Distinguish a click (select → open inspector) from a drag (move the
  // node). A click on a socket starts a connection, so we ignore those.
  const onPointerDown = (e: React.PointerEvent) => {
    const onPin = !!(e.target as HTMLElement).closest(".fb-pin");
    downRef.current = { x: e.clientX, y: e.clientY, onPin };
  };
  const onPointerUp = (e: React.PointerEvent) => {
    const d = downRef.current;
    downRef.current = null;
    if (!d || d.onPin) return;
    const moved = Math.abs(e.clientX - d.x) + Math.abs(e.clientY - d.y);
    if (moved < 5) ctx.selectNode?.(data.id);
  };

  const summary = summarizeNode(data.config, ctx);
  const selected = ctx.isSelected?.(data.id) ?? false;

  return (
    <div
      ref={setContainer}
      className={`fb-node fb-node-${tpl.category} fb-node-${data.config.kind} compact ${selected ? "selected" : ""}`}
      style={{ width: data.width }}
      data-context-menu="ignore"
      onPointerDown={onPointerDown}
      onPointerUp={onPointerUp}
    >
      {/* IN socket: blue chip clinging to the top of the card (Automate style) */}
      <div className="fb-node-pins fb-node-pins-in">
        {inputs.map(([key, input]) =>
          input ? (
            <div key={key} className="fb-pin fb-pin-in">
              <RefSocket
                name="input-socket"
                emit={emit}
                side="input"
                socketKey={key}
                nodeId={data.id}
                payload={input.socket}
              />
              <span className="fb-pin-label">IN</span>
            </div>
          ) : null,
        )}
      </div>

      <div className="fb-node-head" title="Click to edit · drag to move">
        <span className={`fb-node-icon fb-node-icon-${tpl.category}`} aria-hidden="true">
          {iconFor(data.config.kind)}
        </span>
        <span className="fb-node-titles">
          <span className="fb-node-title">{tpl.label}</span>
          <span className="fb-node-summary">{summary}</span>
        </span>
      </div>

      {/* Output socket(s): bottom-of-card chip(s). Single OK pin for most
          nodes; If block exposes labelled yes/no branches. */}
      <div className="fb-node-pins fb-node-pins-out">
        {outputs.map(([key, output]) => {
          if (!output) return null;
          const spec = tpl.outputs.find((o) => o.key === key);
          const variant = spec?.variant ?? "default";
          const label = spec?.label ?? "OK";
          return (
            <div
              key={key}
              className={`fb-pin fb-pin-out fb-pin-out-${variant === "default" ? tpl.category : variant}`}
            >
              <span className="fb-pin-label">{label}</span>
              <RefSocket
                name="output-socket"
                emit={emit}
                side="output"
                socketKey={key}
                nodeId={data.id}
                payload={output.socket}
              />
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ---------- Compact summary for collapsed blocks (Automate-style) ----------

function summarizeNode(config: NodeConfig, ctx: EditorCtx): string {
  switch (config.kind) {
    case "immediate_trigger":
      return "On startup";
    case "cron_trigger":
      return describeCron(config.cron_trigger.cron) ?? config.cron_trigger.cron;
    case "interval_trigger": {
      const { on_seconds, off_seconds, start_action } = config.interval_trigger;
      const start = start_action === "off" ? "off" : "on";
      return `${humanDuration(on_seconds)} ${start} / ${humanDuration(off_seconds)}`;
    }
    case "between": {
      const wins =
        config.between.windows && config.between.windows.length > 0
          ? config.between.windows
          : config.between.start
            ? [{ days: [], start: config.between.start, end: config.between.end ?? "" }]
            : [];
      if (wins.length === 0) return "Set a time window…";
      const first = wins[0];
      const base = `${first.start} – ${first.end}`;
      return wins.length > 1 ? `${base} +${wins.length - 1}` : base;
    }
    case "variable_changed":
      return config.variable_changed.key
        ? `When $${config.variable_changed.key} changes`
        : "Name a variable…";
    case "device_event_trigger": {
      const dev = config.device_event_trigger.device_name;
      const evt = config.device_event_trigger.event;
      return dev ? `${deviceLabel(dev, ctx)} → ${evt}` : "Pick a device…";
    }
    case "http_request": {
      const url = config.http_request.url;
      if (!url) return "Pick a URL…";
      const short = url.length > 40 ? url.slice(0, 38) + "…" : url;
      return `${config.http_request.method} ${short}`;
    }
    case "if_condition": {
      const { expression, field, op, value } = config.if_condition;
      if (expression.trim()) return truncate(expression.trim(), 32);
      if (field.startsWith("device:")) {
        const dev = deviceLabel(field.slice(7), ctx);
        if (op === "is_true") return `${dev} is on`;
      }
      const fieldLabel = field.startsWith("device:")
        ? `${deviceLabel(field.slice(7), ctx)} on/off`
        : field || "value";
      switch (op) {
        case "is_true":
          return `${fieldLabel} is true`;
        case "equals":
          return value ? `${fieldLabel} = "${truncate(value, 20)}"` : `${fieldLabel} equals…`;
        case "contains":
          return value ? `${fieldLabel} contains "${truncate(value, 18)}"` : `${fieldLabel} contains…`;
        case "in_range":
          return value ? `${fieldLabel} in ${value}` : `${fieldLabel} in range…`;
        case "gt":
          return value ? `${fieldLabel} > ${value}` : `${fieldLabel} > …`;
        case "gte":
          return value ? `${fieldLabel} ≥ ${value}` : `${fieldLabel} ≥ …`;
        case "lt":
          return value ? `${fieldLabel} < ${value}` : `${fieldLabel} < …`;
        case "lte":
          return value ? `${fieldLabel} ≤ ${value}` : `${fieldLabel} ≤ …`;
      }
    }
    case "logic_and":
      return "All inputs true";
    case "logic_or":
      return "Any input true";
    case "logic_not":
      return "Invert input";
    case "debounce":
      return `Hold ${humanDuration(config.debounce.hold_seconds)}`;
    case "expression": {
      const e = config.expression.expression.trim();
      return e ? truncate(e, 32) : "Enter an expression…";
    }
    case "set_variable": {
      const { key, expression } = config.set_variable;
      if (!key) return "Name a variable…";
      return expression.trim() ? `${key} = ${truncate(expression.trim(), 22)}` : `Set ${key}…`;
    }
    case "get_variable":
      return config.get_variable.key ? `Read ${config.get_variable.key}` : "Name a variable…";
    case "set_device": {
      const dev = config.set_device.device_name;
      const verb =
        config.set_device.action === "on"
          ? "Turn on"
          : config.set_device.action === "off"
            ? "Turn off"
            : "Toggle";
      return dev ? `${deviceLabel(dev, ctx)} · ${verb}` : "Pick a device…";
    }
    case "toggle_device": {
      const dev = config.toggle_device.device_name;
      return dev ? `${deviceLabel(dev, ctx)} · Toggle` : "Pick a device…";
    }
    case "fire_hook": {
      const hookId = config.fire_hook.hook_id;
      if (!hookId) return "Pick a hook…";
      const hook = ctx.hooks().find((h) => h.id === hookId);
      return hook ? hook.name : "Unknown hook";
    }
  }
}

function describeDow(dow: string): string {
  if (dow === "*") return "";
  // Try common patterns
  if (dow === "1-5") return "Weekdays";
  if (dow === "0,6" || dow === "6,0") return "Weekends";
  if (dow === "0") return "Sunday";
  if (dow === "1") return "Monday";
  const NAMES = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
  const set = new Set<number>();
  for (const part of dow.split(",")) {
    if (/^\d$/.test(part)) {
      set.add(Number(part));
    } else if (/^\d-\d$/.test(part)) {
      const [a, b] = part.split("-").map(Number);
      for (let i = Math.min(a, b); i <= Math.max(a, b); i++) set.add(i);
    } else {
      return dow; // Give up, show raw
    }
  }
  if (set.size === 7) return "";
  return Array.from(set).toSorted().map((d) => NAMES[d]).join("·");
}

function humanDuration(seconds: number): string {
  if (seconds <= 0) return "0s";
  if (seconds % 3600 === 0) {
    const h = seconds / 3600;
    return `${h}h`;
  }
  if (seconds % 60 === 0) {
    const m = seconds / 60;
    return `${m}m`;
  }
  return `${seconds}s`;
}

function deviceLabel(name: string, ctx: EditorCtx): string {
  const d = ctx.devices().find((x) => x.name === name);
  return d?.nickname || name;
}

function truncate(s: string, max: number): string {
  return s.length > max ? s.slice(0, max - 1) + "…" : s;
}

export function NodeBody({
  nodeId,
  config,
  update,
  ctx,
}: {
  nodeId: string;
  config: NodeConfig;
  update: (c: NodeConfig) => void;
  ctx: EditorCtx;
}) {
  switch (config.kind) {
    case "immediate_trigger":
      return (
        <p className="fb-node-hint">
          Fires once when Fusebox starts (after a couple of seconds, so devices
          have connected). Wire it into an HTTP request / Set variable chain to
          prime caches, or into Set device to assert initial state.
        </p>
      );
    case "cron_trigger":
      return <CronBody config={config.cron_trigger} onChange={(cron_trigger) => update({ kind: "cron_trigger", cron_trigger })} />;
    case "interval_trigger":
      return <IntervalBody config={config.interval_trigger} onChange={(interval_trigger) => update({ kind: "interval_trigger", interval_trigger })} />;
    case "device_event_trigger":
      return (
        <>
          <Field label="Device">
            <DevicePicker
              value={config.device_event_trigger.device_name}
              devices={ctx.devices()}
              onChange={(name) =>
                update({
                  ...config,
                  device_event_trigger: { ...config.device_event_trigger, device_name: name },
                })
              }
            />
          </Field>
          <Field label="Event">
            <ChipRow
              options={[
                { value: "on", label: "On" },
                { value: "off", label: "Off" },
                { value: "online", label: "Online" },
                { value: "offline", label: "Offline" },
              ]}
              value={config.device_event_trigger.event}
              onChange={(v) =>
                update({
                  ...config,
                  device_event_trigger: {
                    ...config.device_event_trigger,
                    event: v as DeviceEvent,
                  },
                })
              }
            />
          </Field>
        </>
      );
    case "between":
      return (
        <BetweenBody
          config={config.between}
          onChange={(between) => update({ kind: "between", between })}
        />
      );
    case "http_request":
      return (
        <HttpRequestBody
          config={config.http_request}
          onChange={(http_request) => update({ kind: "http_request", http_request })}
        />
      );
    case "if_condition":
      return (
        <IfConditionBody
          nodeId={nodeId}
          config={config.if_condition}
          ctx={ctx}
          onChange={(if_condition) => update({ kind: "if_condition", if_condition })}
        />
      );
    case "logic_and":
    case "logic_or":
    case "logic_not":
      return <p className="fb-node-hint">{templateFor(config.kind).description}</p>;
    case "debounce":
      return (
        <Field label="Hold for">
          <SecondsInput
            value={config.debounce.hold_seconds}
            onChange={(v) => update({ ...config, debounce: { hold_seconds: v } })}
          />
        </Field>
      );
    case "expression":
      return (
        <ExpressionBody
          label="Expression"
          nodeId={nodeId}
          ctx={ctx}
          value={config.expression.expression}
          onChange={(expression) => update({ kind: "expression", expression: { expression } })}
        />
      );
    case "set_variable":
      return (
        <>
          <Field label="Variable name">
            <input
              type="text"
              aria-label="Variable name"
              value={config.set_variable.key}
              onChange={(e) =>
                update({ ...config, set_variable: { ...config.set_variable, key: e.target.value } })
              }
              placeholder="counter"
              spellCheck={false}
            />
          </Field>
          <ExpressionBody
            label="Set to"
            nodeId={nodeId}
            ctx={ctx}
            value={config.set_variable.expression}
            onChange={(expression) =>
              update({ ...config, set_variable: { ...config.set_variable, expression } })
            }
          />
        </>
      );
    case "get_variable":
      return (
        <>
          <Field label="Variable name">
            <input
              type="text"
              aria-label="Variable name"
              value={config.get_variable.key}
              onChange={(e) => update({ ...config, get_variable: { key: e.target.value } })}
              placeholder="counter"
              spellCheck={false}
            />
          </Field>
          <p className="fb-node-hint">
            Exposes the stored value as "value". Wire into an If block to branch on it.
          </p>
        </>
      );
    case "variable_changed":
      return (
        <>
          <Field label="Variable name">
            <input
              type="text"
              aria-label="Variable name"
              value={config.variable_changed.key}
              onChange={(e) => update({ ...config, variable_changed: { key: e.target.value } })}
              placeholder="counter"
              spellCheck={false}
            />
          </Field>
          <p className="fb-node-hint">
            Fires once each time this variable changes. The new value is on
            "value" — wire into an If to branch on it.
          </p>
        </>
      );
    case "set_device":
      return (
        <>
          <Field label="Device">
            <DevicePicker
              value={config.set_device.device_name}
              devices={ctx.devices()}
              onChange={(name) =>
                update({ ...config, set_device: { ...config.set_device, device_name: name } })
              }
            />
          </Field>
          <Field label="Action">
            <ChipRow
              options={[
                { value: "on", label: "Turn on" },
                { value: "off", label: "Turn off" },
                { value: "toggle", label: "Toggle" },
              ]}
              value={config.set_device.action}
              onChange={(v) =>
                update({
                  ...config,
                  set_device: { ...config.set_device, action: v as ScheduleAction },
                })
              }
            />
          </Field>
        </>
      );
    case "toggle_device":
      return (
        <Field label="Device">
          <DevicePicker
            value={config.toggle_device.device_name}
            devices={ctx.devices()}
            onChange={(name) => update({ ...config, toggle_device: { device_name: name } })}
          />
        </Field>
      );
    case "fire_hook": {
      const hooks = ctx.hooks();
      return (
        <Field label="Hook">
          {hooks.length === 0 ? (
            <p className="fb-node-hint">No hooks defined yet. Create one in the Hooks tab.</p>
          ) : (
            <select
              value={config.fire_hook.hook_id}
              onChange={(e) => update({ ...config, fire_hook: { hook_id: e.target.value } })}
            >
              <option value="">(select a hook)</option>
              {hooks.map((h) => (
                <option key={h.id} value={h.id}>
                  {h.name}
                </option>
              ))}
            </select>
          )}
        </Field>
      );
    }
  }
}

// ---------- Cron trigger: Simple (time + days) / Advanced (raw cron) ----------

const WEEKDAY_LABELS = [
  { day: 1, label: "Mon" },
  { day: 2, label: "Tue" },
  { day: 3, label: "Wed" },
  { day: 4, label: "Thu" },
  { day: 5, label: "Fri" },
  { day: 6, label: "Sat" },
  { day: 0, label: "Sun" },
];

const CRON_EXAMPLES: Array<{ group: string; items: Array<{ label: string; cron: string }> }> = [
  {
    group: "Intervals",
    items: [
      { label: "Every minute", cron: "* * * * *" },
      { label: "Every 5 minutes", cron: "*/5 * * * *" },
      { label: "Every 10 minutes", cron: "*/10 * * * *" },
      { label: "Every 15 minutes", cron: "*/15 * * * *" },
      { label: "Every 30 minutes", cron: "*/30 * * * *" },
      { label: "Every hour", cron: "0 * * * *" },
      { label: "Every 2 hours", cron: "0 */2 * * *" },
      { label: "Every 3 hours", cron: "0 */3 * * *" },
      { label: "Every 6 hours", cron: "0 */6 * * *" },
      { label: "Every 12 hours", cron: "0 */12 * * *" },
    ],
  },
  {
    group: "Every day",
    items: [
      { label: "Midnight (00:00)", cron: "0 0 * * *" },
      { label: "6:00 AM", cron: "0 6 * * *" },
      { label: "8:00 AM", cron: "0 8 * * *" },
      { label: "Noon (12:00)", cron: "0 12 * * *" },
      { label: "5:00 PM", cron: "0 17 * * *" },
      { label: "6:00 PM", cron: "0 18 * * *" },
      { label: "9:00 PM", cron: "0 21 * * *" },
      { label: "11:00 PM", cron: "0 23 * * *" },
    ],
  },
  {
    group: "Weekdays & weekends",
    items: [
      { label: "Weekdays 8:00 AM", cron: "0 8 * * 1-5" },
      { label: "Weekdays 9:00 AM", cron: "0 9 * * 1-5" },
      { label: "Weekdays 6:00 PM", cron: "0 18 * * 1-5" },
      { label: "Weekends 9:00 AM", cron: "0 9 * * 0,6" },
      { label: "Weekends 10:00 AM", cron: "0 10 * * 0,6" },
    ],
  },
  {
    group: "Weekly",
    items: [
      { label: "Mondays 8:00 AM", cron: "0 8 * * 1" },
      { label: "Wednesdays 9:00 AM", cron: "0 9 * * 3" },
      { label: "Fridays 5:00 PM", cron: "0 17 * * 5" },
      { label: "Saturdays 9:00 AM", cron: "0 9 * * 6" },
      { label: "Sundays midnight", cron: "0 0 * * 0" },
    ],
  },
  {
    group: "Monthly",
    items: [
      { label: "1st at midnight", cron: "0 0 1 * *" },
      { label: "1st at 9:00 AM", cron: "0 9 1 * *" },
      { label: "15th at noon", cron: "0 12 15 * *" },
      { label: "Last-ish (28th) 8 AM", cron: "0 8 28 * *" },
    ],
  },
];

// Best-effort plain-English description of a 5-field cron, covering the
// shapes our examples produce (intervals, daily/weekly times, monthly).
// Returns null for anything it can't confidently describe.
function describeCron(cron: string): string | null {
  const fields = cron.trim().split(/\s+/);
  if (fields.length !== 5) return null;
  const [min, hour, dom, month, dow] = fields;
  const allStar = (...xs: string[]) => xs.every((x) => x === "*");
  let m: RegExpMatchArray | null;

  if (allStar(min, hour, dom, month, dow)) return "Every minute";
  if ((m = min.match(/^\*\/(\d+)$/)) && allStar(hour, dom, month, dow)) {
    return `Every ${m[1]} minutes`;
  }
  if (/^\d+$/.test(min) && allStar(hour, dom, month, dow)) {
    return min === "0" ? "Every hour" : `Every hour at :${pad(Number(min))}`;
  }
  if (/^\d+$/.test(min) && (m = hour.match(/^\*\/(\d+)$/)) && allStar(dom, month, dow)) {
    return `Every ${m[1]} hours`;
  }
  if (/^\d+$/.test(min) && /^\d+$/.test(hour) && month === "*") {
    const time = `${pad(Number(hour))}:${pad(Number(min))}`;
    if (dom === "*") {
      const days = describeDow(dow);
      return days === "" ? `Daily at ${time}` : `${days} at ${time}`;
    }
    if (dow === "*" && /^\d+$/.test(dom)) {
      return `Monthly on day ${dom} at ${time}`;
    }
  }
  return null;
}

function CronBody({ config, onChange }: { config: { cron: string }; onChange: (cfg: { cron: string }) => void }) {
  const parsed = parseSimpleCron(config.cron);
  const [mode, setMode] = useState<"simple" | "advanced">(parsed ? "simple" : "advanced");
  const description = describeCron(config.cron);

  // Picking an example fills the expression and snaps the editor into the
  // mode that can represent it (Simple for daily/weekly times, Advanced for
  // intervals / monthly that the day+time picker can't express).
  const pickExample = (cron: string) => {
    onChange({ cron });
    setMode(parseSimpleCron(cron) ? "simple" : "advanced");
  };

  return (
    <>
      <Field label="Examples">
        <select
          aria-label="Cron examples"
          value=""
          onChange={(e) => {
            if (e.target.value) pickExample(e.target.value);
          }}
        >
          <option value="">Pick a schedule…</option>
          {CRON_EXAMPLES.map((g) => (
            <optgroup key={g.group} label={g.group}>
              {g.items.map((it) => (
                <option key={it.cron} value={it.cron}>
                  {it.label}
                </option>
              ))}
            </optgroup>
          ))}
        </select>
      </Field>

      <ModeTabs
        value={mode}
        onChange={setMode}
        options={[
          { value: "simple", label: "Simple" },
          { value: "advanced", label: "Advanced" },
        ]}
      />
      {mode === "simple" ? (
        <SimpleCronBody value={parsed ?? { hour: 8, minute: 0, days: [1, 2, 3, 4, 5] }} onChange={(v) => onChange({ cron: buildSimpleCron(v) })} />
      ) : (
        <>
          <Field label="Cron expression">
            <input
              type="text"
              aria-label="Cron expression"
              value={config.cron}
              onChange={(e) => onChange({ cron: e.target.value })}
              placeholder="0 8 * * 1-5"
              spellCheck={false}
            />
          </Field>
          <p className="fb-node-hint">
            5 fields: <code>min hour day month weekday</code>
          </p>
        </>
      )}

      <p className="fb-node-hint fb-cron-desc">
        {description ? `▶ ${description}` : "▶ Custom schedule"}
      </p>
    </>
  );
}

interface SimpleCron {
  hour: number;
  minute: number;
  days: number[];
}

function SimpleCronBody({ value, onChange }: { value: SimpleCron; onChange: (v: SimpleCron) => void }) {
  const timeValue = `${pad(value.hour)}:${pad(value.minute)}`;
  return (
    <>
      <Field label="Time">
        <input
          type="time"
          aria-label="Time of day"
          value={timeValue}
          onChange={(e) => {
            const [h, m] = e.target.value.split(":");
            onChange({ ...value, hour: Number(h) || 0, minute: Number(m) || 0 });
          }}
        />
      </Field>
      <Field label="Days">
        <DayPicker
          value={value.days}
          onChange={(days) => onChange({ ...value, days })}
        />
      </Field>
      <div className="fb-presets">
        <button type="button" className="fb-preset-chip" onClick={() => onChange({ ...value, days: [1, 2, 3, 4, 5] })}>
          Weekdays
        </button>
        <button type="button" className="fb-preset-chip" onClick={() => onChange({ ...value, days: [0, 6] })}>
          Weekends
        </button>
        <button type="button" className="fb-preset-chip" onClick={() => onChange({ ...value, days: [0, 1, 2, 3, 4, 5, 6] })}>
          Every day
        </button>
      </div>
    </>
  );
}

function DayPicker({ value, onChange }: { value: number[]; onChange: (days: number[]) => void }) {
  const toggle = (day: number) => {
    onChange(value.includes(day) ? value.filter((d) => d !== day) : [...value, day].sort());
  };
  return (
    <div className="fb-day-picker">
      {WEEKDAY_LABELS.map(({ day, label }) => {
        const active = value.includes(day);
        return (
          <button
            key={day}
            type="button"
            className={`fb-day-chip ${active ? "active" : ""}`}
            onClick={() => toggle(day)}
            aria-pressed={active}
          >
            {label}
          </button>
        );
      })}
    </div>
  );
}

function parseSimpleCron(cron: string): SimpleCron | null {
  const fields = cron.trim().split(/\s+/);
  if (fields.length !== 5) return null;
  const [min, hour, dom, month, dow] = fields;
  if (dom !== "*" || month !== "*") return null;
  const m = Number(min);
  const h = Number(hour);
  if (!Number.isInteger(m) || m < 0 || m > 59) return null;
  if (!Number.isInteger(h) || h < 0 || h > 23) return null;
  const days = parseDowField(dow);
  if (!days) return null;
  return { hour: h, minute: m, days };
}

function parseDowField(dow: string): number[] | null {
  if (dow === "*") return [0, 1, 2, 3, 4, 5, 6];
  const parts = dow.split(",");
  const set = new Set<number>();
  for (const part of parts) {
    if (/^[0-6]$/.test(part)) {
      set.add(Number(part));
    } else if (/^[0-6]-[0-6]$/.test(part)) {
      const [a, b] = part.split("-").map(Number);
      const lo = Math.min(a, b);
      const hi = Math.max(a, b);
      for (let i = lo; i <= hi; i++) set.add(i);
    } else {
      return null;
    }
  }
  return Array.from(set).toSorted();
}

function buildSimpleCron({ hour, minute, days }: SimpleCron): string {
  const dow = formatDowField(days);
  return `${minute} ${hour} * * ${dow}`;
}

function formatDowField(days: number[]): string {
  const sorted = Array.from(new Set(days)).toSorted((a, b) => a - b);
  if (sorted.length === 7) return "*";
  if (sorted.length === 0) return "*";
  // Collapse contiguous runs into ranges (e.g., 1,2,3,4,5 → 1-5).
  const runs: string[] = [];
  let i = 0;
  while (i < sorted.length) {
    let j = i;
    while (j + 1 < sorted.length && sorted[j + 1] === sorted[j] + 1) j++;
    runs.push(j === i ? `${sorted[i]}` : `${sorted[i]}-${sorted[j]}`);
    i = j + 1;
  }
  return runs.join(",");
}

function pad(n: number): string {
  return n < 10 ? `0${n}` : `${n}`;
}

// ---------- Interval trigger ----------

const INTERVAL_PRESETS: Array<{ label: string; on: number; off: number }> = [
  { label: "15m / 15m", on: 15 * 60, off: 15 * 60 },
  { label: "30m / 30m", on: 30 * 60, off: 30 * 60 },
  { label: "1h / 30m", on: 60 * 60, off: 30 * 60 },
  { label: "1h / 1h", on: 60 * 60, off: 60 * 60 },
];

function IntervalBody({
  config,
  onChange,
}: {
  config: IntervalTriggerConfig;
  onChange: (cfg: IntervalTriggerConfig) => void;
}) {
  return (
    <>
      <div className="fb-row">
        <Field label="On for">
          <SecondsInput
            value={config.on_seconds}
            onChange={(v) => onChange({ ...config, on_seconds: v })}
          />
        </Field>
        <Field label="Off for">
          <SecondsInput
            value={config.off_seconds}
            onChange={(v) => onChange({ ...config, off_seconds: v })}
          />
        </Field>
      </div>
      <Field label="Start with">
        <ChipRow
          options={[
            { value: "on", label: "On" },
            { value: "off", label: "Off" },
          ]}
          value={config.start_action}
          onChange={(v) => onChange({ ...config, start_action: v as ScheduleAction })}
        />
      </Field>
      <div className="fb-presets">
        {INTERVAL_PRESETS.map((p) => (
          <button
            key={p.label}
            type="button"
            className="fb-preset-chip"
            onClick={() => onChange({ ...config, on_seconds: p.on, off_seconds: p.off })}
          >
            {p.label}
          </button>
        ))}
      </div>
      <p className="fb-node-hint">Cycle repeats forever. Total on + off must be at least 1 minute.</p>
    </>
  );
}

// ---------- Between: time-of-day windows, optionally per weekday ----------

let betweenWindowSeq = 0;
const newWindowId = () => `bw${betweenWindowSeq++}`;

function BetweenBody({
  config,
  onChange,
}: {
  config: BetweenConfig;
  onChange: (cfg: BetweenConfig) => void;
}) {
  // Normalise legacy single-window configs into the windows array, and make
  // sure every window has a stable id for list rendering.
  const raw: BetweenWindow[] =
    config.windows && config.windows.length > 0
      ? config.windows
      : [{ days: [], start: config.start ?? "07:30", end: config.end ?? "22:00" }];
  const windows = raw.map((w, i) => (w.id ? w : { ...w, id: `w${i}` }));

  const commit = (next: BetweenWindow[]) => onChange({ windows: next });
  const patch = (i: number, w: Partial<BetweenWindow>) =>
    commit(windows.map((win, idx) => (idx === i ? { ...win, ...w } : win)));
  const addWindow = () =>
    commit([...windows, { id: newWindowId(), days: [], start: "10:00", end: "02:00" }]);

  return (
    <>
      {windows.map((w, i) => (
        <div key={w.id} className="fb-between-window">
          <div className="fb-row">
            <Field label="From">
              <input
                type="time"
                aria-label="Window start"
                value={w.start}
                onChange={(e) => patch(i, { start: e.target.value })}
              />
            </Field>
            <Field label="To">
              <input
                type="time"
                aria-label="Window end"
                value={w.end}
                onChange={(e) => patch(i, { end: e.target.value })}
              />
            </Field>
          </div>
          <Field label="Days (none = every day)">
            <DayPicker value={w.days} onChange={(days) => patch(i, { days })} />
          </Field>
          {windows.length > 1 ? (
            <button
              type="button"
              className="fb-preset-chip"
              onClick={() => commit(windows.filter((_, idx) => idx !== i))}
            >
              Remove window
            </button>
          ) : null}
        </div>
      ))}
      <button
        type="button"
        className="fb-preset-chip"
        onClick={addWindow}
      >
        + Add window
      </button>
      <p className="fb-node-hint">
        YES when the current time matches any window (and that day, if set);
        NO otherwise. A window like 07:30 → 01:00 wraps past midnight.
      </p>
    </>
  );
}

// ---------- HTTP request (action — runs on input pulse, records body+status) ----------

interface HttpRequestBodyConfig {
  url: string;
  method: string;
  headers: Record<string, string>;
  body?: string | null;
  status_match: string;
}

function HttpRequestBody({
  config,
  onChange,
}: {
  config: HttpRequestBodyConfig;
  onChange: (cfg: HttpRequestBodyConfig) => void;
}) {
  const [advancedOpen, setAdvancedOpen] = useState(false);
  return (
    <>
      <div className="fb-row">
        <Field label="Method">
          <select
            aria-label="HTTP method"
            value={config.method}
            onChange={(e) => onChange({ ...config, method: e.target.value })}
          >
            <option>GET</option>
            <option>POST</option>
            <option>HEAD</option>
            <option>PUT</option>
          </select>
        </Field>
        <Field label="Status match">
          <input
            type="text"
            aria-label="Status match"
            value={config.status_match}
            onChange={(e) => onChange({ ...config, status_match: e.target.value })}
            placeholder="200-299"
            spellCheck={false}
          />
        </Field>
      </div>
      <Field label="URL">
        <input
          type="text"
          aria-label="Request URL"
          value={config.url}
          onChange={(e) => onChange({ ...config, url: e.target.value })}
          placeholder="https://example.com/webhook"
          spellCheck={false}
        />
      </Field>
      <details
        className="fb-collapse"
        open={advancedOpen}
        onToggle={(e) => setAdvancedOpen((e.target as HTMLDetailsElement).open)}
      >
        <summary>Advanced</summary>
        <Field label="Headers (one per line, Key: value)">
          <textarea
            aria-label="Request headers"
            rows={2}
            value={headersToText(config.headers)}
            onChange={(e) => onChange({ ...config, headers: textToHeaders(e.target.value) })}
            placeholder="Authorization: Bearer …"
            spellCheck={false}
          />
        </Field>
        <Field label="Request body (optional)">
          <textarea
            aria-label="Request body"
            rows={2}
            value={config.body ?? ""}
            onChange={(e) => onChange({ ...config, body: e.target.value || null })}
            spellCheck={false}
          />
        </Field>
      </details>
      <p className="fb-node-hint">
        Sends one request each time the input pulse arrives. The OK socket
        emits true if the response matched. Wire into an If block to branch
        on the body or status.
      </p>
    </>
  );
}

// ---------- If: route on a named output of the upstream block ----------

const DEFAULT_OUTPUT: DataOutputSpec = { key: "value", label: "Value (true/false)" };

interface IfCfg {
  expression: string;
  field: string;
  op: IfOp;
  value: string;
}

function IfConditionBody({
  nodeId,
  config,
  ctx,
  onChange,
}: {
  nodeId: string;
  config: IfCfg;
  ctx: EditorCtx;
  onChange: (cfg: IfCfg) => void;
}) {
  const [mode, setMode] = useState<"builder" | "expression">(
    config.expression.trim() ? "expression" : "builder",
  );

  const upstreamKind = ctx.findUpstreamKind?.(nodeId) ?? null;
  const upstreamLabel = upstreamKind ? templateFor(upstreamKind).label : null;
  const availableOutputs: DataOutputSpec[] = upstreamKind
    ? templateFor(upstreamKind).dataOutputs
    : [DEFAULT_OUTPUT];
  const variableNames = ctx.variableNames?.() ?? [];
  const variableOptions: DataOutputSpec[] = variableNames.map((v) => ({ key: `$${v}`, label: `$${v}` }));
  const inputFields = availableOutputs.map((o) => o.key);
  // Devices: `device:NAME` reads on/off state. is_true → on (YES), NO → off.
  const deviceOptions: DataOutputSpec[] = ctx.devices().map((d) => ({
    key: `device:${d.name}`,
    label: `${d.nickname || d.name} (on/off)`,
  }));

  // Surface a saved field that's no longer in any list so it isn't lost.
  const known = [...availableOutputs, ...variableOptions, ...deviceOptions].some(
    (o) => o.key === config.field,
  );
  const extraOption: DataOutputSpec[] = known
    ? []
    : [{ key: config.field, label: `${config.field} (missing)` }];

  // Picking a device defaults the comparison to "is true" (= device on).
  const onFieldChange = (field: string) => {
    if (field.startsWith("device:")) onChange({ ...config, field, op: "is_true" });
    else onChange({ ...config, field });
  };

  const showValue = config.op !== "is_true";
  const isNumeric = config.op === "gt" || config.op === "gte" || config.op === "lt" || config.op === "lte";
  const placeholder =
    config.op === "equals" ? "yes"
    : config.op === "contains" ? "ok"
    : config.op === "in_range" ? "200-299"
    : isNumeric ? "15"
    : "";
  const valueLabel =
    config.op === "in_range" ? "Range (e.g. 200-299, 404)"
    : isNumeric ? "Number"
    : "Value";

  const switchMode = (m: "builder" | "expression") => {
    // Clearing the expression makes the engine use the builder fields again.
    if (m === "builder") onChange({ ...config, expression: "" });
    setMode(m);
  };

  return (
    <>
      <ModeTabs
        value={mode}
        onChange={switchMode}
        options={[
          { value: "builder", label: "Builder" },
          { value: "expression", label: "Expression" },
        ]}
      />
      {mode === "builder" ? (
        <>
          <Field label="Read field">
            <select
              aria-label="Field to read"
              value={config.field}
              onChange={(e) => onFieldChange(e.target.value)}
            >
              <optgroup label={upstreamLabel ? `From ${upstreamLabel}` : "Input"}>
                {availableOutputs.map((o) => (
                  <option key={o.key} value={o.key}>{o.label}</option>
                ))}
              </optgroup>
              {variableOptions.length > 0 ? (
                <optgroup label="Variables">
                  {variableOptions.map((o) => (
                    <option key={o.key} value={o.key}>{o.label}</option>
                  ))}
                </optgroup>
              ) : null}
              {deviceOptions.length > 0 ? (
                <optgroup label="Devices">
                  {deviceOptions.map((o) => (
                    <option key={o.key} value={o.key}>{o.label}</option>
                  ))}
                </optgroup>
              ) : null}
              {extraOption.map((o) => (
                <option key={o.key} value={o.key}>{o.label}</option>
              ))}
            </select>
          </Field>
          <Field label="Compare">
            <select
              aria-label="Comparison"
              value={config.op}
              onChange={(e) => onChange({ ...config, op: e.target.value as IfOp })}
            >
              <option value="is_true">is true</option>
              <option value="equals">equals</option>
              <option value="contains">contains</option>
              <option value="gt">&gt; greater than</option>
              <option value="gte">≥ greater or equal</option>
              <option value="lt">&lt; less than</option>
              <option value="lte">≤ less or equal</option>
              <option value="in_range">in range</option>
            </select>
          </Field>
          {showValue ? (
            <Field label={valueLabel}>
              <input
                type="text"
                aria-label={valueLabel}
                value={config.value}
                onChange={(e) => onChange({ ...config, value: e.target.value })}
                placeholder={placeholder}
                spellCheck={false}
              />
            </Field>
          ) : null}
          <p className="fb-node-hint">
            {config.field.startsWith("device:")
              ? `"is true" → device is on (YES), off routes to NO.`
              : "Reads one field (input, $variable, or a device's on/off state) and compares it. Switch to Expression to combine several."}
          </p>
        </>
      ) : (
        <>
          <Field label="Condition (true → YES)">
            <ExpressionInput
              ariaLabel="Condition expression"
              value={config.expression}
              onChange={(expression) => onChange({ ...config, expression })}
              inputFields={inputFields}
              variableNames={variableNames}
            />
          </Field>
          <p className="fb-node-hint">
            A boolean expression over <code>$variables</code> and{" "}
            <code>input.fields</code>, e.g.{" "}
            <code>$level &gt; 15 &amp;&amp; input.value == "true"</code>. YES when truthy.
          </p>
        </>
      )}
    </>
  );
}

// ---------- Expression (used by Expression + Set variable blocks) ----------

function ExpressionBody({
  label,
  nodeId,
  ctx,
  value,
  onChange,
}: {
  label: string;
  nodeId: string;
  ctx: EditorCtx;
  value: string;
  onChange: (expression: string) => void;
}) {
  const upstreamKind = ctx.findUpstreamKind?.(nodeId) ?? null;
  const upstreamLabel = upstreamKind ? templateFor(upstreamKind).label : null;
  const inputFields = upstreamKind
    ? templateFor(upstreamKind).dataOutputs.map((o) => o.key)
    : [];
  const variableNames = ctx.variableNames?.() ?? [];

  const [preview, setPreview] = useState<PreviewResult | null>(null);
  const [previewing, setPreviewing] = useState(false);

  const runPreview = async () => {
    if (!ctx.previewExpression) return;
    setPreviewing(true);
    try {
      const upstreamId = ctx.findUpstreamLogicalId?.(nodeId) ?? null;
      setPreview(await ctx.previewExpression(upstreamId, value));
    } catch (e) {
      setPreview({ ok: false, error: String(e), input_fields: [] });
    } finally {
      setPreviewing(false);
    }
  };

  // Insert a snippet at the end (textarea caret tracking lives in
  // ExpressionInput; chips just append a starting point to type from).
  const insert = (snippet: string) => {
    const sep = value && !value.endsWith(" ") ? " " : "";
    onChange(value + sep + snippet);
  };

  return (
    <>
      <Field label={label}>
        <ExpressionInput
          ariaLabel={label}
          value={value}
          onChange={onChange}
          inputFields={inputFields}
          variableNames={variableNames}
        />
      </Field>

      {(inputFields.length > 0 || variableNames.length > 0) && (
        <div className="fb-expr-refs">
          {upstreamLabel && inputFields.length > 0 ? (
            <div className="fb-expr-ref-group">
              <span className="fb-expr-ref-title">From {upstreamLabel}</span>
              <div className="fb-chip-row">
                {inputFields.map((f) => (
                  <button
                    key={f}
                    type="button"
                    className="fb-chip"
                    title={`Insert input.${f}`}
                    onClick={() => insert(`input.${f}`)}
                  >
                    input.{f}
                  </button>
                ))}
              </div>
            </div>
          ) : null}
          {variableNames.length > 0 ? (
            <div className="fb-expr-ref-group">
              <span className="fb-expr-ref-title">Variables</span>
              <div className="fb-chip-row">
                {variableNames.map((v) => (
                  <button
                    key={v}
                    type="button"
                    className="fb-chip"
                    title={`Insert $${v}`}
                    onClick={() => insert(`$${v}`)}
                  >
                    ${v}
                  </button>
                ))}
              </div>
            </div>
          ) : null}
        </div>
      )}

      <div className="fb-expr-preview">
        <button
          type="button"
          className="fb-chip fb-expr-preview-btn"
          onClick={runPreview}
          disabled={previewing || !value.trim()}
          title="Evaluate against the upstream block's last run"
        >
          {previewing ? "Running…" : "Preview"}
        </button>
        {preview ? (
          preview.input_fields.length === 0 && /\binput\b/.test(value) ? (
            // Upstream block has no recorded run, so any input.* is empty.
            // Lead with that instead of a confusing eval error.
            <div className="fb-expr-preview-error">
              The upstream {upstreamLabel ?? "block"} hasn't run yet, so{" "}
              <code>input</code> is empty. Save the automation and let it fire
              once (it needs a trigger wired to IN), then Preview again.
            </div>
          ) : preview.ok ? (
            <pre className="fb-expr-preview-result">{preview.result_text || "(empty)"}</pre>
          ) : (
            <div className="fb-expr-preview-error">{preview.error}</div>
          )
        ) : null}
        {preview && preview.input_fields.length > 0 ? (
          <p className="fb-node-hint">
            Available: {preview.input_fields.map((f) => `input.${f}`).join(", ")}
          </p>
        ) : null}
      </div>

      <p className="fb-node-hint">
        Type <code>$</code>, <code>input.</code>, or a function name for
        suggestions. {upstreamLabel ? `Reading from ${upstreamLabel}.` : "Connect a block to IN to read its outputs."}
      </p>
    </>
  );
}

function headersToText(headers: Record<string, string>): string {
  return Object.entries(headers)
    .map(([k, v]) => `${k}: ${v}`)
    .join("\n");
}

const HEADER_LINE_PATTERN = /^([^:]+):(.*)$/;

function textToHeaders(text: string): Record<string, string> {
  const result: Record<string, string> = {};
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const match = HEADER_LINE_PATTERN.exec(trimmed);
    if (!match) continue;
    const key = match[1].trim();
    const value = match[2].trim();
    if (key) result[key] = value;
  }
  return result;
}

// ---------- shared building blocks ----------

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="fb-field">
      <span className="fb-field-label">{label}</span>
      {children}
    </label>
  );
}

function ChipRow<T extends string>({
  options,
  value,
  onChange,
}: {
  options: Array<{ value: T; label: string }>;
  value: T;
  onChange: (v: T) => void;
}) {
  return (
    <div className="fb-chip-row">
      {options.map((opt) => (
        <button
          key={opt.value}
          type="button"
          className={`fb-chip ${value === opt.value ? "active" : ""}`}
          aria-pressed={value === opt.value}
          onClick={() => onChange(opt.value)}
        >
          {opt.label}
        </button>
      ))}
    </div>
  );
}

function ModeTabs<T extends string>({
  options,
  value,
  onChange,
}: {
  options: Array<{ value: T; label: string }>;
  value: T;
  onChange: (v: T) => void;
}) {
  return (
    <div className="fb-mode-tabs" role="tablist">
      {options.map((opt) => (
        <button
          key={opt.value}
          type="button"
          role="tab"
          className="fb-mode-tab"
          aria-selected={value === opt.value}
          onClick={() => onChange(opt.value)}
        >
          {opt.label}
        </button>
      ))}
    </div>
  );
}

function DevicePicker({
  value,
  devices,
  onChange,
}: {
  value: string;
  devices: { name: string; nickname: string }[];
  onChange: (name: string) => void;
}) {
  if (devices.length === 0) {
    return (
      <p className="fb-node-hint">
        No devices yet. Run a scan from the Devices tab.
      </p>
    );
  }
  return (
    <select value={value} onChange={(e) => onChange(e.target.value)}>
      <option value="">(select a device)</option>
      {devices.map((d) => (
        <option key={d.name} value={d.name}>
          {d.nickname || d.name}
        </option>
      ))}
    </select>
  );
}

// Render a seconds value as a paired (count, unit) input so users don't have
// to mentally convert hours/minutes to seconds.
function SecondsInput({ value, onChange }: { value: number; onChange: (v: number) => void }) {
  const { count, unit } = splitSeconds(value);
  return (
    <div className="fb-duration">
      <input
        type="number"
        aria-label="Duration value"
        min={0}
        value={count}
        onChange={(e) => {
          const n = Number(e.target.value);
          onChange(Number.isFinite(n) ? n * unitToSeconds(unit) : 0);
        }}
      />
      <select
        aria-label="Duration unit"
        value={unit}
        onChange={(e) => {
          const next = e.target.value as Unit;
          onChange(count * unitToSeconds(next));
        }}
      >
        <option value="s">sec</option>
        <option value="m">min</option>
        <option value="h">hr</option>
      </select>
    </div>
  );
}

type Unit = "s" | "m" | "h";
function unitToSeconds(unit: Unit): number {
  return unit === "h" ? 3600 : unit === "m" ? 60 : 1;
}
function splitSeconds(total: number): { count: number; unit: Unit } {
  if (total > 0 && total % 3600 === 0) return { count: total / 3600, unit: "h" };
  if (total > 0 && total % 60 === 0) return { count: total / 60, unit: "m" };
  return { count: total, unit: "s" };
}
