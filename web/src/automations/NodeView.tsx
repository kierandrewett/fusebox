import { useRef, useState } from "react";
import { Presets } from "rete-react-plugin";
import type { FlowNode } from "./createEditor";
import type {
  DeviceEvent,
  IntervalTriggerConfig,
  NodeConfig,
  ScheduleAction,
} from "../types";
import { templateFor } from "./nodes";

const { RefSocket } = Presets.classic;

interface Props {
  data: FlowNode;
  emit: (event: any) => void;
}

interface EditorCtx {
  devices: () => { name: string; nickname: string }[];
  hooks: () => { id: string; name: string }[];
}

export function NodeView({ data, emit }: Props) {
  const [, force] = useState(0);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const tpl = templateFor(data.config.kind);
  const inputs = Object.entries(data.inputs);
  const outputs = Object.entries(data.outputs);

  const ctx: EditorCtx = (() => {
    let el: HTMLElement | null = containerRef.current;
    while (el) {
      if ((el as any).__fuseboxCtx) return (el as any).__fuseboxCtx as EditorCtx;
      el = el.parentElement;
    }
    return { devices: () => [], hooks: () => [] };
  })();

  const update = (next: NodeConfig) => {
    data.config = next;
    data.onChange?.();
    force((n) => n + 1);
  };

  // Stop pointerdown from bubbling to Rete's drag handler ONLY when it
  // originates inside an interactive control. Without this, clicks on
  // inputs/selects/<summary> elements start a node-drag instead of doing
  // the thing they should do. Letting the event bubble for clicks on plain
  // node chrome keeps node dragging working.
  const swallowOnFormControls = (event: React.PointerEvent) => {
    const target = event.target as HTMLElement;
    if (target.closest("input, select, textarea, button, summary, [role=button]")) {
      event.stopPropagation();
    }
  };

  return (
    <div
      ref={containerRef}
      className={`fb-node fb-node-${tpl.category} fb-node-${data.config.kind}`}
      style={{ width: data.width }}
      data-context-menu="ignore"
      onPointerDown={swallowOnFormControls}
      onDoubleClick={(e) => e.stopPropagation()}
    >
      <div className="fb-node-head">
        <span className="fb-node-title">{tpl.label}</span>
        <button
          type="button"
          className="fb-node-delete"
          title="Delete this block"
          aria-label="Delete block"
          onClick={() => {
            const container = containerRef.current as HTMLElement | null;
            let el: HTMLElement | null = container;
            while (el) {
              const editor = (el as any).__fuseboxEditor;
              if (editor && typeof editor.removeNode === "function") {
                void editor.removeNode(data.id);
                return;
              }
              el = el.parentElement;
            }
          }}
        >
          ×
        </button>
      </div>
      <div className="fb-node-body">{renderBody(data.config, update, ctx)}</div>
      <div className="fb-node-sockets">
        <div className="fb-sockets-in">
          {inputs.map(([key, input]) =>
            input ? (
              <div key={key} className="fb-socket fb-socket-in">
                <RefSocket
                  name="input-socket"
                  emit={emit}
                  side="input"
                  socketKey={key}
                  nodeId={data.id}
                  payload={input.socket}
                />
                <span className="fb-socket-label">{input.label}</span>
              </div>
            ) : null,
          )}
        </div>
        <div className="fb-sockets-out">
          {outputs.map(([key, output]) =>
            output ? (
              <div key={key} className="fb-socket fb-socket-out">
                <span className="fb-socket-label">{output.label}</span>
                <RefSocket
                  name="output-socket"
                  emit={emit}
                  side="output"
                  socketKey={key}
                  nodeId={data.id}
                  payload={output.socket}
                />
              </div>
            ) : null,
          )}
        </div>
      </div>
    </div>
  );
}

function renderBody(config: NodeConfig, update: (c: NodeConfig) => void, ctx: EditorCtx) {
  switch (config.kind) {
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
    case "http_probe":
      return <HttpProbeBody config={config.http_probe} onChange={(http_probe) => update({ kind: "http_probe", http_probe })} />;
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
              <option value="">— select hook —</option>
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

const CRON_PRESETS: Array<{ label: string; cron: string }> = [
  { label: "Weekdays 8am", cron: "0 8 * * 1-5" },
  { label: "Weekends 9am", cron: "0 9 * * 0,6" },
  { label: "Daily 7pm", cron: "0 19 * * *" },
  { label: "Hourly", cron: "0 * * * *" },
];

function CronBody({ config, onChange }: { config: { cron: string }; onChange: (cfg: { cron: string }) => void }) {
  const parsed = parseSimpleCron(config.cron);
  const [mode, setMode] = useState<"simple" | "advanced">(parsed ? "simple" : "advanced");

  return (
    <>
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
      <div className="fb-presets">
        {CRON_PRESETS.map((p) => (
          <button
            key={p.cron}
            type="button"
            className="fb-preset-chip"
            onClick={() => onChange({ cron: p.cron })}
            title={p.cron}
          >
            {p.label}
          </button>
        ))}
      </div>
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
  return [...set].sort();
}

function buildSimpleCron({ hour, minute, days }: SimpleCron): string {
  const dow = formatDowField(days);
  return `${minute} ${hour} * * ${dow}`;
}

function formatDowField(days: number[]): string {
  const sorted = [...new Set(days)].sort((a, b) => a - b);
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

// ---------- HTTP probe ----------

interface HttpProbeBodyConfig {
  url: string;
  method: string;
  headers: Record<string, string>;
  body?: string | null;
  status_match: string;
  body_contains?: string | null;
  poll_seconds: number;
  min_stable_seconds: number;
}

function HttpProbeBody({
  config,
  onChange,
}: {
  config: HttpProbeBodyConfig;
  onChange: (cfg: HttpProbeBodyConfig) => void;
}) {
  const [advancedOpen, setAdvancedOpen] = useState(false);
  return (
    <>
      <div className="fb-row">
        <Field label="Method">
          <select
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
          value={config.url}
          onChange={(e) => onChange({ ...config, url: e.target.value })}
          placeholder="https://example.com/status"
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
            rows={2}
            value={headersToText(config.headers)}
            onChange={(e) => onChange({ ...config, headers: textToHeaders(e.target.value) })}
            placeholder="Authorization: Bearer …"
            spellCheck={false}
          />
        </Field>
        <Field label="Request body (optional)">
          <textarea
            rows={2}
            value={config.body ?? ""}
            onChange={(e) => onChange({ ...config, body: e.target.value || null })}
            spellCheck={false}
          />
        </Field>
        <Field label="Body must contain (optional)">
          <input
            type="text"
            value={config.body_contains ?? ""}
            onChange={(e) => onChange({ ...config, body_contains: e.target.value || null })}
            spellCheck={false}
          />
        </Field>
      </details>
    </>
  );
}
function headersToText(headers: Record<string, string>): string {
  return Object.entries(headers)
    .map(([k, v]) => `${k}: ${v}`)
    .join("\n");
}

function textToHeaders(text: string): Record<string, string> {
  const result: Record<string, string> = {};
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const idx = trimmed.indexOf(":");
    if (idx <= 0) continue;
    const key = trimmed.slice(0, idx).trim();
    const value = trimmed.slice(idx + 1).trim();
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
          aria-pressed={value === opt.value}
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
      <option value="">— select device —</option>
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
        min={0}
        value={count}
        onChange={(e) => {
          const n = Number(e.target.value);
          onChange(Number.isFinite(n) ? n * unitToSeconds(unit) : 0);
        }}
      />
      <select
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
