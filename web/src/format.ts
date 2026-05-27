// Display formatters preserving the original Fusebox JS behaviour.

export function formatEnergy(wattHours: number | null | undefined): string {
  if (wattHours == null) return "-";
  if (wattHours >= 1000) return `${(wattHours / 1000).toFixed(2)} kWh`;
  return `${Math.round(wattHours)} Wh`;
}

export function formatCost(pence: number | null | undefined): string {
  if (pence == null) return "-";
  if (pence >= 100) return `£${(pence / 100).toFixed(2)}`;
  return `${Math.round(pence)}p`;
}

export function formatDurationFromSeconds(seconds: number | null | undefined): string {
  if (seconds == null) return "-";
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  const remMin = minutes % 60;
  if (hours < 24) return remMin ? `${hours}h ${remMin}m` : `${hours}h`;
  const days = Math.floor(hours / 24);
  const remHours = hours % 24;
  return remHours ? `${days}d ${remHours}h` : `${days}d`;
}

export function formatDurationFromMinutes(minutes: number | null | undefined): string {
  if (minutes == null) return "-";
  return formatDurationFromSeconds(minutes * 60);
}

export function formatRelative(epochMs: number): string {
  const delta = epochMs - Date.now();
  const abs = Math.abs(delta);
  const min = Math.round(abs / 60_000);
  if (min < 1) return delta >= 0 ? "now" : "now";
  if (min < 60) return delta >= 0 ? `in ${min}m` : `${min}m ago`;
  const h = Math.round(min / 60);
  if (h < 24) return delta >= 0 ? `in ${h}h` : `${h}h ago`;
  const d = Math.round(h / 24);
  return delta >= 0 ? `in ${d}d` : `${d}d ago`;
}
