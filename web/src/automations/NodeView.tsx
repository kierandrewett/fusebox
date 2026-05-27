import { useEffect, useRef, useState } from "react";
import { Presets } from "rete-react-plugin";
import type { FlowNode } from "./createEditor";
import type { NodeConfig, ScheduleAction, DeviceEvent } from "../types";
import { templateFor } from "./nodes";

const { RefSocket } = Presets.classic;

interface Props {
  data: FlowNode;
  emit: (event: any) => void;
}

export function NodeView({ data, emit }: Props) {
  const [, force] = useState(0);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const tpl = templateFor(data.config.kind);
  const inputs = Object.entries(data.inputs);
  const outputs = Object.entries(data.outputs);

  // Read shared editor context attached by createEditor.
  const ctx = (() => {
    let el: HTMLElement | null = containerRef.current;
    while (el) {
      if ((el as any).__fuseboxCtx) return (el as any).__fuseboxCtx as {
        devices: () => { name: string; nickname: string }[];
        hooks: () => { id: string; name: string }[];
      };
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
  // originates inside a form control. Without this, clicks on inputs/selects
  // start a node-drag instead of focusing the field. Letting the event bubble
  // for clicks on node chrome keeps node dragging working.
  const swallowOnFormControls = (event: React.PointerEvent) => {
    const target = event.target as HTMLElement;
    if (target.closest("input, select, textarea, button")) {
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

function renderBody(
  config: NodeConfig,
  update: (c: NodeConfig) => void,
  ctx: {
    devices: () => { name: string; nickname: string }[];
    hooks: () => { id: string; name: string }[];
  },
) {
  switch (config.kind) {
    case "cron_trigger":
      return (
        <Field label="Cron (5 fields)">
          <input
            type="text"
            value={config.cron_trigger.cron}
            onChange={(e) =>
              update({ ...config, cron_trigger: { cron: e.target.value } })
            }
            placeholder="0 8 * * *"
          />
        </Field>
      );
    case "interval_trigger":
      return (
        <>
          <Field label="On (s)">
            <input
              type="number"
              min={0}
              value={config.interval_trigger.on_seconds}
              onChange={(e) =>
                update({
                  ...config,
                  interval_trigger: {
                    ...config.interval_trigger,
                    on_seconds: numberOr(e.target.value, 0),
                  },
                })
              }
            />
          </Field>
          <Field label="Off (s)">
            <input
              type="number"
              min={0}
              value={config.interval_trigger.off_seconds}
              onChange={(e) =>
                update({
                  ...config,
                  interval_trigger: {
                    ...config.interval_trigger,
                    off_seconds: numberOr(e.target.value, 0),
                  },
                })
              }
            />
          </Field>
          <Field label="Start with">
            <select
              value={config.interval_trigger.start_action}
              onChange={(e) =>
                update({
                  ...config,
                  interval_trigger: {
                    ...config.interval_trigger,
                    start_action: e.target.value as ScheduleAction,
                  },
                })
              }
            >
              <option value="on">On</option>
              <option value="off">Off</option>
            </select>
          </Field>
        </>
      );
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
            <select
              value={config.device_event_trigger.event}
              onChange={(e) =>
                update({
                  ...config,
                  device_event_trigger: {
                    ...config.device_event_trigger,
                    event: e.target.value as DeviceEvent,
                  },
                })
              }
            >
              <option value="on">on</option>
              <option value="off">off</option>
              <option value="online">online</option>
              <option value="offline">offline</option>
            </select>
          </Field>
        </>
      );
    case "http_probe":
      return (
        <>
          <Field label="URL">
            <input
              type="text"
              value={config.http_probe.url}
              onChange={(e) =>
                update({ ...config, http_probe: { ...config.http_probe, url: e.target.value } })
              }
              placeholder="https://example.com/status"
            />
          </Field>
          <Field label="Method">
            <select
              value={config.http_probe.method}
              onChange={(e) =>
                update({ ...config, http_probe: { ...config.http_probe, method: e.target.value } })
              }
            >
              <option>GET</option>
              <option>POST</option>
              <option>PUT</option>
              <option>HEAD</option>
            </select>
          </Field>
          <Field label="Status match">
            <input
              type="text"
              value={config.http_probe.status_match}
              onChange={(e) =>
                update({
                  ...config,
                  http_probe: { ...config.http_probe, status_match: e.target.value },
                })
              }
              placeholder="200-299"
            />
          </Field>
          <Field label="Poll (s)">
            <input
              type="number"
              min={5}
              value={config.http_probe.poll_seconds}
              onChange={(e) =>
                update({
                  ...config,
                  http_probe: {
                    ...config.http_probe,
                    poll_seconds: numberOr(e.target.value, 60),
                  },
                })
              }
            />
          </Field>
          <Field label="Stable for (s)">
            <input
              type="number"
              min={0}
              value={config.http_probe.min_stable_seconds}
              onChange={(e) =>
                update({
                  ...config,
                  http_probe: {
                    ...config.http_probe,
                    min_stable_seconds: numberOr(e.target.value, 0),
                  },
                })
              }
            />
          </Field>
        </>
      );
    case "logic_and":
    case "logic_or":
    case "logic_not":
      return <p className="fb-node-hint">{templateFor(config.kind).description}</p>;
    case "debounce":
      return (
        <Field label="Hold (s)">
          <input
            type="number"
            min={0}
            value={config.debounce.hold_seconds}
            onChange={(e) =>
              update({
                ...config,
                debounce: { hold_seconds: numberOr(e.target.value, 0) },
              })
            }
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
            <select
              value={config.set_device.action}
              onChange={(e) =>
                update({
                  ...config,
                  set_device: { ...config.set_device, action: e.target.value as ScheduleAction },
                })
              }
            >
              <option value="on">On</option>
              <option value="off">Off</option>
              <option value="toggle">Toggle</option>
            </select>
          </Field>
        </>
      );
    case "toggle_device":
      return (
        <Field label="Device">
          <DevicePicker
            value={config.toggle_device.device_name}
            devices={ctx.devices()}
            onChange={(name) =>
              update({ ...config, toggle_device: { device_name: name } })
            }
          />
        </Field>
      );
    case "fire_hook":
      return (
        <Field label="Hook">
          <select
            value={config.fire_hook.hook_id}
            onChange={(e) => update({ ...config, fire_hook: { hook_id: e.target.value } })}
          >
            <option value="">— select hook —</option>
            {ctx.hooks().map((h) => (
              <option key={h.id} value={h.id}>
                {h.name}
              </option>
            ))}
          </select>
        </Field>
      );
  }
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="fb-field">
      <span className="fb-field-label">{label}</span>
      {children}
    </label>
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

function numberOr(v: string, fallback: number) {
  const n = Number(v);
  return Number.isFinite(n) ? n : fallback;
}
