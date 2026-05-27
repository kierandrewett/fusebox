import type { Device } from "../types";
import { EnergyMeters } from "./EnergyMeters";

export function DeviceCard({ device, pending, onToggle, onRelease }: { device: Device; pending: boolean; onToggle: () => void; onRelease: () => void }) {
  const isOn = device.device_on === true;
  return (
    <article className={`device-card ${isOn ? "on" : "off"}`}>
      <header><div><h3>{device.nickname || device.name}</h3><p>{device.ip} · {device.model}</p></div><span className="status-pill">{isOn ? "On" : "Off"}</span></header>
      <EnergyMeters energy={device.energy} />
      {device.last_error ? <p className="error">{device.last_error}</p> : null}
      <footer>
        <button type="button" disabled={pending} onClick={onToggle}>{pending ? "Working..." : isOn ? "Switch off" : "Switch on"}</button>
        {device.manual_override !== null && device.manual_override !== undefined ? <button type="button" disabled={pending} onClick={onRelease}>Release override</button> : null}
      </footer>
    </article>
  );
}
