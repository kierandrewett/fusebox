import { useCallback, useRef } from "react";
import type { RunNodeResult } from "../api";

export type RunState = "on" | "off" | "error";

/** Holds the last test run's per-node outcome so the canvas can light up the
 *  path it took, keyed by logical node id. Empty when no run is shown. */
export function useRunHighlight(notifyCtx: () => void) {
  const runHighlightRef = useRef(new Map<string, RunState>());

  const showRun = useCallback(
    (results: RunNodeResult[]) => {
      const map = new Map<string, RunState>();
      for (const r of results) map.set(r.node_id, r.error ? "error" : r.value ? "on" : "off");
      runHighlightRef.current = map;
      notifyCtx();
    },
    [notifyCtx],
  );

  const clearRun = useCallback(() => {
    if (runHighlightRef.current.size > 0) {
      runHighlightRef.current = new Map();
      notifyCtx();
    }
  }, [notifyCtx]);

  return { runHighlightRef, showRun, clearRun };
}
