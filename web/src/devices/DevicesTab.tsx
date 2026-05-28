import { useEffect, useMemo, useReducer } from "react";
import { energyHistory, listDeviceResponse, releaseDeviceOverride, setDevicePower, toggleDevice } from "../api";
import type { DeviceListResponse, UsageHistoryResponse } from "../types";
import { subscribeDevices } from "../ws";
import { DeviceCard } from "./DeviceCard";
import { UsageChart, type HistoryRange } from "./UsageChart";
import { UpcomingChanges } from "./UpcomingChanges";
import { formatCost, formatEnergy } from "../format";

interface Props {
  // The Scan button lives in the App header. When it produces a payload it
  // pushes it here via this callback registrar; we install our handler once
  // on mount.
  registerScanSink: (sink: (payload: DeviceListResponse) => void) => void;
}

interface State {
  data: DeviceListResponse | null;
  history: UsageHistoryResponse | null;
  historyRange: HistoryRange;
  error: string | null;
  pending: string | null;
}

type Action =
  | { type: "data"; data: DeviceListResponse }
  | { type: "history"; history: UsageHistoryResponse }
  | { type: "range"; range: HistoryRange }
  | { type: "error"; error: string }
  | { type: "pending-start"; name: string }
  | { type: "pending-end" };

function reducer(state: State, action: Action): State {
  switch (action.type) {
    case "data": return { ...state, data: action.data };
    case "history": return { ...state, history: action.history };
    case "range": return { ...state, historyRange: action.range };
    case "error": return { ...state, error: action.error };
    case "pending-start": return { ...state, pending: action.name, error: null };
    case "pending-end": return { ...state, pending: null };
  }
}

const INITIAL_STATE: State = {
  data: null,
  history: null,
  historyRange: "7d",
  error: null,
  pending: null,
};

export function DevicesTab({ registerScanSink }: Props) {
  const [state, dispatch] = useReducer(reducer, INITIAL_STATE);

  useEffect(() => {
    let mounted = true;
    listDeviceResponse()
      .then((d) => { if (mounted) dispatch({ type: "data", data: d }); })
      .catch((err) => { if (mounted) dispatch({ type: "error", error: String(err) }); });
    const unsubscribe = subscribeDevices((next) => {
      if (mounted) dispatch({ type: "data", data: next });
    });
    registerScanSink((payload) => {
      if (mounted) dispatch({ type: "data", data: payload });
    });
    return () => { mounted = false; unsubscribe(); };
  }, [registerScanSink]);

  useEffect(() => {
    let mounted = true;
    energyHistory(state.historyRange)
      .then((u) => { if (mounted) dispatch({ type: "history", history: u }); })
      .catch((err) => { if (mounted) dispatch({ type: "error", error: String(err) }); });
    return () => { mounted = false; };
  }, [state.historyRange]);

  const summary = useMemo(() => {
    const list = state.data?.devices ?? [];
    const power = list.reduce((acc, d) => acc + (d.energy?.current_power_w ?? 0), 0);
    const energy = list.reduce((acc, d) => acc + (d.energy?.today_energy_wh ?? 0), 0);
    const cost = list.reduce((acc, d) => acc + (d.energy?.today_cost_pence ?? 0), 0);
    return { count: list.length, power, energy, cost };
  }, [state.data]);

  const devices = state.data?.devices ?? [];

  const mutate = async (name: string, action: () => Promise<unknown>) => {
    dispatch({ type: "pending-start", name });
    try {
      await action();
      const fresh = await listDeviceResponse();
      dispatch({ type: "data", data: fresh });
    } catch (err) {
      dispatch({ type: "error", error: String(err) });
    } finally {
      dispatch({ type: "pending-end" });
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

      <UsageChart
        history={state.history}
        range={state.historyRange}
        onRangeChange={(range) => dispatch({ type: "range", range })}
      />

      <UpcomingChanges devices={devices} />

      {state.error ? <p className="notice" role="alert">{state.error}</p> : null}

      <div className="breaker-grid">
        {devices.length === 0 ? (
          <div className="empty">No supported Tapo plugs found yet. Check credentials and LAN access, or press Scan now.</div>
        ) : (
          devices.map((device) => (
            <DeviceCard
              key={device.name}
              device={device}
              pending={state.pending === device.name}
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
