import type { RefObject } from "react";
import { previewExpression, runNode } from "../api";
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
    runNode: (reteId) => {
      const id = selectedIdRef.current;
      const api = editorApiRef.current;
      if (!id || !api) {
        return Promise.resolve({ ok: false, nodes: [], error: "no automation selected" });
      }
      const graph = api.serialize();
      return runNode(id, api.logicalIdFor(reteId), graph.nodes, graph.edges);
    },
  };
}
