import { useMemo, type RefObject } from "react";
import { previewExpression } from "../api";
import type { DeviceSummary, HookSummary } from "../types";
import type { CreateEditorResult, EditorContext } from "./createEditor";
import { mergeVariableNames } from "./variableNames";

interface Args {
  devicesRef: RefObject<DeviceSummary[]>;
  hooksRef: RefObject<HookSummary[]>;
  variableNamesRef: RefObject<string[]>;
  selectedIdRef: RefObject<string | null>;
  selectedNodeIdsRef: RefObject<string[]>;
  editorApiRef: RefObject<CreateEditorResult | null>;
  ctxListenersRef: RefObject<Set<() => void>>;
  runHighlightRef: RefObject<Map<string, "on" | "off" | "error">>;
  selectNode: (reteId: string | null) => void;
  selectNodes: (ids: string[]) => void;
  deleteSelected: () => void;
  onContextMenu: (at: { x: number; y: number; onNode: boolean }) => void;
}

/** Builds the EditorContext passed to createEditor. Memoised on its stable
 *  callbacks so the editor-mount effect can depend on it without re-running. */
export function useEditorCtx({
  devicesRef,
  hooksRef,
  variableNamesRef,
  selectedIdRef,
  selectedNodeIdsRef,
  editorApiRef,
  ctxListenersRef,
  runHighlightRef,
  selectNode,
  selectNodes,
  deleteSelected,
  onContextMenu,
}: Args): EditorContext {
  return useMemo(
    () => ({
      devices: () => devicesRef.current ?? [],
      hooks: () => hooksRef.current ?? [],
      variableNames: () =>
        mergeVariableNames(variableNamesRef.current, editorApiRef.current?.variableKeys()),
      listNodes: () => editorApiRef.current?.listNodes() ?? [],
      previewExpression: (upstreamId, expression) => {
        const id = selectedIdRef.current;
        if (!id) {
          return Promise.resolve({ ok: false, error: "no automation selected", input_fields: [] });
        }
        return previewExpression(id, upstreamId, expression);
      },
      selectNode,
      selectNodes,
      isSelected: (reteId) => selectedNodeIdsRef.current?.includes(reteId) ?? false,
      runStateFor: (reteId) => {
        const map = runHighlightRef.current;
        if (!map || map.size === 0) return null;
        const logical = editorApiRef.current?.logicalIdFor(reteId);
        return (logical && map.get(logical)) || "idle";
      },
      deleteSelected,
      onContextMenu,
      subscribeContext: (cb) => {
        ctxListenersRef.current?.add(cb);
        return () => {
          ctxListenersRef.current?.delete(cb);
        };
      },
    }),
    // Refs are stable; only the callbacks matter for identity.
    [
      devicesRef,
      hooksRef,
      variableNamesRef,
      selectedIdRef,
      selectedNodeIdsRef,
      editorApiRef,
      ctxListenersRef,
      runHighlightRef,
      selectNode,
      selectNodes,
      deleteSelected,
      onContextMenu,
    ],
  );
}
