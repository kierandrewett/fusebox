import { getDeviceForecast, type ForecastEvent } from "../api";

// External store for the device forecast, polled while anything is
// subscribed. Owning the cache outside React lets the panel initialise via
// useSyncExternalStore (no mount-only effect that sets state) and keeps a
// single shared poll regardless of how many components read it.
let cache: ForecastEvent[] | undefined = undefined;
let lastError: string | null = null;
const listeners = new Set<() => void>();
let timer: number | null = null;

function notify() {
  for (const l of listeners) l();
}

async function refresh(): Promise<void> {
  try {
    cache = (await getDeviceForecast()).events;
    lastError = null;
  } catch (err) {
    lastError = String(err);
  } finally {
    notify();
  }
}

export function subscribeForecast(listener: () => void): () => void {
  listeners.add(listener);
  if (listeners.size === 1) {
    void refresh();
    // Refresh each minute so relative times and the rolling 4h window stay
    // current.
    timer = window.setInterval(() => void refresh(), 60_000);
  }
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0 && timer !== null) {
      window.clearInterval(timer);
      timer = null;
    }
  };
}

export function getForecastSnapshot(): ForecastEvent[] | undefined {
  return cache;
}

export function getForecastError(): string | null {
  return lastError;
}
