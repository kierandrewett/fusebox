import { useEffect, useMemo, useState } from "react";
import { energyHistory, listDeviceResponse, releaseDeviceOverride, setDevicePower, toggleDevice } from "../api";
import type { DeviceListResponse, UsageHistoryResponse } from "../types";
import { subscribeDevices } from "../ws";
import { DeviceCard } from "./DeviceCard";
import { UsageChart, type HistoryRange } from "./UsageChart";
import { formatCost, formatEnergy } from "../format";

interface Props {
  scanOverride: DeviceListResponse | null;
  onClearScanOverride: () => void;
}

export function DevicesTab({ scanOverride, onClearScanOverride }: Props) {
  const [data, setData] = useState<DeviceListResponse | null>(null);
  const [history, setHistory] = useState<UsageHistoryResponse | null>(null);
  const [historyRange, setHistoryRange] = useState<HistoryRange>("7d");
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState<string | null>(null);

  // Initial load + WebSocket subscription
  useEffect(() => {
    let mounted = true;
    listDeviceResponse()
      .then((d) => { if (mounted) setData(d); })
      .catch((err) => { if (mounted) setError(String(err)); });
    const unsubscribe = subscribeDevices((next) => { if (mounted) setData(next); });
    return () => { mounted = false; unsubscribe(); };
  }, []);

  // Apply scan results from header button
  useEffect(() => {
    if (scanOverride) {
      setData(scanOverride);
      onClearScanOverride();
    }
  }, [scanOverride, onClearScanOverride]);

  // Reload usage history when range changes
  useEffect(() => {
    let mounted = true;
    energyHistory(historyRange)
      .then((u) => { if (mounted) setHistory(u); })
      .catch((err) => { if (mounted) setError(String(err)); });
    return () => { mounted = false; };
  }, [historyRange]);

  const devices = data?.devices ?? [];

  const summary = useMemo(() => {
    const power = devices.reduce((acc, d) => acc + (d.energy?.current_power_w ?? 0), 0);
    const energy = devices.reduce((acc, d) => acc + (d.energy?.today_energy_wh ?? 0), 0);
    const cost = devices.reduce((acc, d) => acc + (d.energy?.today_cost_pence ?? 0), 0);
    return { count: devices.length, power, energy, cost };
  }, [devices]);

  const mutate = async (name: string, action: () => Promise<unknown>) => {
    setPending(name);
    setError(null);
    try {
      await action();
      const fresh = await listDeviceResponse();
      setData(fresh);
    } catch (err) {
      setError(String(err));
    } finally {
      setPending(null);
    }
  };

  return (
    <section className="cabinet" aria-live="polite">
      <div className="meter-row" aria-label="Fusebox summary">
        <div className="meter"><span>Devices</span><strong>{summary.count}</strong></div>
        <div className="meter"><span>Live load</span><strong>{summary.power} W</strong></div>
        <div className="meter"><span>Today</span><strong>{formatEnergy(summary.energy)}</strong></div>
        <div className="meter"><span>Cost today</span><strong>{formatCost(summary.cost)}</strong></div>
      </div>

      <UsageChart history={history} range={historyRange} onRangeChange={setHistoryRange} />

      {error ? <p className="notice" role="alert">{error}</p> : null}

      <div className="breaker-grid">
        {devices.length === 0 ? (
          <div className="empty">No supported Tapo plugs found yet. Check credentials and LAN access, or press Scan now.</div>
        ) : (
          devices.map((device) => (
            <DeviceCard
              key={device.name}
              device={device}
              pending={pending === device.name}
              onToggle={() => mutate(device.name, () => toggleDevice(device.name))}
              onSetPower={(on) => mutate(device.name, () => setDevicePower(device.name, on))}
              onRelease={() => mutate(device.name, () => releaseDeviceOverride(device.name))}
            />
          ))
        )}
      </div>
    </section>
  );
}
