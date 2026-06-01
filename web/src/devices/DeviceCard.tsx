import { playSwitchSound } from "../audio";
import { formatCost, formatDurationFromMinutes, formatDurationFromSeconds, formatEnergy, formatRelative } from "../format";
import type { Device } from "../types";

interface Props {
  device: Device;
  pending: boolean;
  onToggle: () => void;
  onSetPower: (on: boolean) => void;
  onRelease: () => void;
  onExtend: () => void;
}

export function DeviceCard({ device, pending, onToggle, onRelease, onExtend }: Props) {
  const isOffline = device.last_error != null;
  const isOn = device.device_on === true;
  const statusClass = isOffline ? "offline" : isOn ? "on" : "off";
  const statusText = isOffline ? "offline" : isOn ? "on" : "off";
  const isManual = device.manual_override === true || device.manual_override === false;
  const conditionBlock = device.condition_intent === false && !isManual;
  const energy = device.energy;

  const handleToggleClick = () => {
    if (pending || isOffline) return;
    playSwitchSound();
    onToggle();
  };

  return (
    <article className={`breaker ${isOffline ? "offline" : ""}`}>
      <div className="label-card">
        <h2 className="device-name">{device.nickname || device.name}</h2>
        <p className="device-meta">{device.ip} / {device.model}</p>
      </div>

      <div className="toggle-wrap">
        <button
          type="button"
          className="toggle"
          data-on={isOn}
          aria-pressed={isOn}
          aria-label={`Toggle ${device.nickname || device.name}`}
          disabled={pending || isOffline}
          onClick={handleToggleClick}
        >
          <span className="lever" aria-hidden="true" />
        </button>
      </div>

      {isManual ? (
        <div className="device-mode-badge manual">
          <span className="manual-label">
            Manual{device.manual_override_until_ms ? ` — auto ${formatRelative(device.manual_override_until_ms)}` : ""}
          </span>
          <span className="manual-actions">
            {device.manual_override_until_ms ? (
              <button
                type="button"
                disabled={pending}
                onClick={onExtend}
                title="Push back the automatic revert by another hour"
              >
                Extend
              </button>
            ) : null}
            <button
              type="button"
              disabled={pending}
              onClick={onRelease}
              title="Hand control back to schedules &amp; conditions"
            >
              Auto
            </button>
          </span>
        </div>
      ) : null}

      {conditionBlock ? (
        <div className="device-mode-badge condition-blocked">
          <span>Blocked by condition</span>
        </div>
      ) : null}

      <div className="status-strip">
        <span className={`lamp ${statusClass}`}>{statusText}</span>
        <span>{formatDurationFromSeconds(device.on_time_seconds ?? null)}</span>
      </div>

      <div className="readings">
        <div className="reading"><span>Now</span>{energy?.current_power_w != null ? `${energy.current_power_w} W` : "-"}</div>
        <div className="reading"><span>Today energy</span>{energy ? formatEnergy(energy.today_energy_wh) : "-"}</div>
        <div className="reading"><span>Today cost</span>{energy ? formatCost(energy.today_cost_pence) : "-"}</div>
        <div className="reading"><span>Month energy</span>{energy ? formatEnergy(energy.month_energy_wh) : "-"}</div>
        <div className="reading"><span>Month cost</span>{energy ? formatCost(energy.month_cost_pence) : "-"}</div>
        <div className="reading"><span>Today runtime</span>{energy ? formatDurationFromMinutes(energy.today_runtime_minutes) : "-"}</div>
      </div>

      {isOffline ? <p className="device-meta">{device.last_error}</p> : null}
    </article>
  );
}
