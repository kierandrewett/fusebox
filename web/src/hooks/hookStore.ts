import { listHookDetails } from "../api";
import type { Hook } from "../types";

// External store for the Hooks list. Owning the cache outside of React means
// the panel can initialise itself with `useSyncExternalStore` (no mount-only
// useEffect that initialises state).
let cache: Hook[] | undefined = undefined;
let lastError: string | null = null;
const listeners = new Set<() => void>();

function notify() {
  for (const l of listeners) l();
}

async function fetchOnce(): Promise<void> {
  try {
    cache = await listHookDetails();
    lastError = null;
  } catch (err) {
    lastError = String(err);
  } finally {
    notify();
  }
}

let inflight: Promise<void> | null = null;
function ensureLoaded() {
  if (inflight) return inflight;
  inflight = fetchOnce().finally(() => { inflight = null; });
  return inflight;
}

export function subscribe(listener: () => void): () => void {
  // Lazily kick off the fetch the first time anyone subscribes so the panel
  // is responsible for triggering its own load without an effect.
  if (cache === undefined && !inflight) ensureLoaded();
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function getSnapshot(): Hook[] | undefined {
  return cache;
}

export function getErrorSnapshot(): string | null {
  return lastError;
}

export function reloadHooks(): Promise<void> {
  inflight = fetchOnce().finally(() => { inflight = null; });
  return inflight;
}
