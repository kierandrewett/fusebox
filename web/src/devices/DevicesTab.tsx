import { useEffect, useState } from "react";
import { energyHistory, listDeviceResponse, releaseDeviceOverride, scanDevices, toggleDevice } from "../api";
import type { DeviceListResponse, UsageHistoryResponse } from "../types";
import { subscribeDevices } from "../ws";
import { DeviceCard } from "./DeviceCard";
import { ScanButton } from "./ScanButton";
import { UsageChart } from "./UsageChart";

export function DevicesTab() {
  const [data, setData] = useState<DeviceListResponse | null>(null);
  const [history, setHistory] = useState<UsageHistoryResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [scanning, setScanning] = useState(false);
  const [pending, setPending] = useState<string | null>(null);

  const load = async () => {
    const [devices, usage] = await Promise.all([listDeviceResponse(), energyHistory("7d")]);
    setData(devices);
    setHistory(usage);
  };

  useEffect(() => {
    load().catch((err) => setError(String(err)));
    return subscribeDevices(setData);
  }, []);

  const scan = async () => {
    setScanning(true);
    setError(null);
    try { setData(await scanDevices()); } catch (err) { setError(String(err)); } finally { setScanning(false); }
  };

  const mutate = async (name: string, action: () => Promise<unknown>) => {
    setPending(name);
    setError(null);
    try { await action(); setData(await listDeviceResponse()); } catch (err) { setError(String(err)); } finally { setPending(null); }
  };

  return <section className="panel"><header className="panel-header"><div><h2>Devices</h2><p>{data ? `${data.devices.length} devices` : "Loading devices"}</p></div><ScanButton scanning={scanning} onScan={scan} /></header>{error ? <p className="error">{error}</p> : null}<div className="device-grid">{data?.devices.map((device) => <DeviceCard key={device.name} device={device} pending={pending === device.name} onToggle={() => mutate(device.name, () => toggleDevice(device.name))} onRelease={() => mutate(device.name, () => releaseDeviceOverride(device.name))} />)}</div><section className="chart-card"><h2>Usage</h2><UsageChart history={history} /></section></section>;
}
