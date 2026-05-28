import { useSyncExternalStore } from "react";
import type { ForecastEvent } from "../api";
import type { DeviceSummary } from "../types";
import { getForecastError, getForecastSnapshot, subscribeForecast } from "./forecastStore";

interface Props {
  /** For mapping device_name → nickname. */
  devices: DeviceSummary[];
}

function fmtClock(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function fmtRelative(ms: number, now: number): string {
  const mins = Math.max(0, Math.round((ms - now) / 60000));
  if (mins < 1) return "now";
  if (mins < 60) return `in ${mins}m`;
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return m === 0 ? `in ${h}h` : `in ${h}h ${m}m`;
}

const ACTION_LABEL: Record<ForecastEvent["action"], string> = {
  on: "On",
  off: "Off",
  toggle: "Toggle",
};

export function UpcomingChanges({ devices }: Props) {
  const events = useSyncExternalStore(subscribeForecast, getForecastSnapshot);
  const error = useSyncExternalStore(subscribeForecast, getForecastError);

  const nickname = (name: string) =>
    devices.find((d) => d.name === name)?.nickname || name;

  const now = Date.now();

  return (
    <section className="forecast-panel" aria-labelledby="forecast-title">
      <div className="forecast-header">
        <h2 id="forecast-title">Upcoming changes</h2>
        <span>next 4 hours</span>
      </div>
      {error ? <p className="notice" role="alert">{error}</p> : null}
      {events === undefined ? (
        <p className="forecast-empty">Loading…</p>
      ) : events.length === 0 ? (
        <p className="forecast-empty">
          No scheduled changes in the next 4 hours. (Only time-based triggers
          are forecast; conditional automations aren't shown.)
        </p>
      ) : (
        <ul className="forecast-list">
          {events.map((e, i) => (
            <li key={`${e.at_ms}-${e.device_name}-${i}`} className="forecast-row">
              <span className="forecast-time">
                {fmtClock(e.at_ms)}
                <span className="forecast-rel">{fmtRelative(e.at_ms, now)}</span>
              </span>
              <span className="forecast-device">{nickname(e.device_name)}</span>
              <span className={`forecast-action action-${e.action}`}>
                {ACTION_LABEL[e.action]}
              </span>
              <span className="forecast-source">{e.automation_name}</span>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
