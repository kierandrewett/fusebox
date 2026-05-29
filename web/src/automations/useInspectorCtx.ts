import type { RefObject } from "react";
import { previewExpression, type RunNodeResult } from "../api";
import type { DeviceSummary, HookSummary } from "../types";
import type { CreateEditorResult } from "./createEditor";
import type { EditorCtx } from "./NodeView";
import { mergeVariableNames } from "./variableNames";

interface Refs {
  devicesRef: RefObject<DeviceSummary[]>;
  hooksRef: RefObject<HookSummary[]>;
  variableNamesRef: RefObject<string[]>;
  selectedIdRef: RefObject<string | null>;
  editorApiRef: RefObject<CreateEditorResult | null>;
  liveResultsRef: RefObject<Map<string, RunNodeResult>>;
}

/** The EditorCtx the inspector's NodeBody uses: reads device/hook/variable
 *  pools from refs and delegates upstream lookups + preview to the live
 *  editor api. */
export function useInspectorCtx({
  devicesRef,
  hooksRef,
  variableNamesRef,
  selectedIdRef,
  editorApiRef,
  liveResultsRef,
}: Refs): EditorCtx {
  return {
    devices: () => devicesRef.current ?? [],
    hooks: () => hooksRef.current ?? [],
    variableNames: () =>
      mergeVariableNames(variableNamesRef.current, editorApiRef.current?.variableKeys()),
    listNodes: () => editorApiRef.current?.listNodes() ?? [],
    findUpstreamKind: (rid) => editorApiRef.current?.findUpstreamKind(rid) ?? null,
    findUpstreamLogicalId: (rid) => editorApiRef.current?.findUpstreamLogicalId(rid) ?? null,
    previewExpression: (upstreamId, expression) => {
      const id = selectedIdRef.current;
      if (!id) {
        return Promise.resolve({ ok: false, error: "no automation selected", input_fields: [] });
      }
      return previewExpression(id, upstreamId, expression);
    },
    liveResultFor: (reteId) => {
      const api = editorApiRef.current;
      const map = liveResultsRef.current;
      if (!api || !map) return null;
      return map.get(api.logicalIdFor(reteId)) ?? null;
    },
  };
}
