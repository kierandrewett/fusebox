import { useCallback, useEffect, useRef, type RefObject } from "react";
import { runNode, type RunNodeResult } from "../api";
import type { CreateEditorResult } from "./createEditor";

interface Args {
  selectedId: string | null;
  selectedIdRef: RefObject<string | null>;
  editorApiRef: RefObject<CreateEditorResult | null>;
  editorReadyRef: RefObject<boolean>;
  notifyCtx: () => void;
}

const POLL_MS = 2500;

/** Continuously dry-runs the whole graph in the background (live mode: HTTP
 *  replays its snapshot, triggers are treated as firing) and exposes each
 *  node's latest result, so the canvas always shows the path the current
 *  conditions resolve to without the user running anything by hand. */
export function useLiveFlow({
  selectedId,
  selectedIdRef,
  editorApiRef,
  editorReadyRef,
  notifyCtx,
}: Args) {
  // Logical node id -> latest live result.
  const liveResultsRef = useRef(new Map<string, RunNodeResult>());
  const runningRef = useRef(false);

  const refreshLive = useCallback(async () => {
    const api = editorApiRef.current;
    const id = selectedIdRef.current;
    if (!id || !api || !editorReadyRef.current || runningRef.current) return;
    runningRef.current = true;
    try {
      const graph = api.serialize();
      const res = await runNode(id, null, graph.nodes, graph.edges, true);
      // Apply only if the user hasn't switched automations mid-flight.
      if (selectedIdRef.current === id) {
        const map = new Map<string, RunNodeResult>();
        if (res.ok) for (const r of res.nodes) map.set(r.node_id, r);
        liveResultsRef.current = map;
        notifyCtx();
      }
    } catch {
      // Best-effort: keep the last good highlight.
    } finally {
      runningRef.current = false;
    }
  }, [editorApiRef, selectedIdRef, editorReadyRef, notifyCtx]);

  useEffect(() => {
    if (!selectedId) {
      liveResultsRef.current = new Map();
      notifyCtx();
      return;
    }
    void refreshLive();
    const timer = window.setInterval(() => void refreshLive(), POLL_MS);
    return () => window.clearInterval(timer);
  }, [selectedId, refreshLive, notifyCtx]);

  return { liveResultsRef, refreshLive };
}
