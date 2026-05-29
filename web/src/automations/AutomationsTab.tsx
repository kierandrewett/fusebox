import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import { listAutomations, listDevices, listHooks } from "../api";
import type { Automation, DeviceSummary, HookSummary } from "../types";
import { createEditor, type CreateEditorResult } from "./createEditor";
import { AutomationsSidebar } from "./AutomationsSidebar";
import { AutomationToolbar } from "./AutomationToolbar";
import { NodeInspector } from "./NodeInspector";
import { CanvasContextMenu } from "./CanvasContextMenu";
import { useAutomationFiles } from "./useAutomationFiles";
import { useAutomationCrud } from "./useAutomationCrud";
import { useNodeOps } from "./useNodeOps";
import { useInspectorCtx } from "./useInspectorCtx";
import { useEditorCtx } from "./useEditorCtx";
import { useGraphPersistence } from "./useGraphPersistence";

interface Status {
  loading: boolean;
  saving: boolean;
  dirty: boolean;
  error: string | null;
}

const INITIAL_STATUS: Status = { loading: true, saving: false, dirty: false, error: null };

function statusReducer(state: Status, patch: Partial<Status>): Status {
  return { ...state, ...patch };
}

export function AutomationsTab() {
  const [automations, setAutomations] = useState<Automation[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  // Rete id of the block whose settings are open in the inspector.
  // Selected canvas blocks (Rete ids). One → inspector opens; many → bulk ops.
  const [selectedNodeIds, setSelectedNodeIds] = useState<string[]>([]);
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number; onNode: boolean } | null>(null);
  const [status, patchStatus] = useReducer(statusReducer, INITIAL_STATUS);
  const { loading, saving, dirty, error } = status;
  const setError = useCallback((error: string | null) => patchStatus({ error }), []);
  const setSaving = useCallback((saving: boolean) => patchStatus({ saving }), []);
  const setDirty = useCallback((dirty: boolean) => patchStatus({ dirty }), []);

  // These never participate in render — they only feed Rete's NodeViews via
  // refs, so we hold them as refs (no re-render churn) and notify listeners
  // explicitly when they change.
  const devicesRef = useRef<DeviceSummary[]>([]);
  const hooksRef = useRef<HookSummary[]>([]);
  const variableNamesRef = useRef<string[]>([]);
  const selectedIdRef = useRef<string | null>(null);
  const selectedNodeIdsRef = useRef<string[]>([]);
  selectedNodeIdsRef.current = selectedNodeIds;
  const editorContainerRef = useRef<HTMLDivElement | null>(null);
  const editorApiRef = useRef<CreateEditorResult | null>(null);
  const editorReadyRef = useRef(false);
  const pendingLoadRef = useRef<{ id: string | null } | null>(null);

  // The inspector edits exactly one block; hidden during a multi-selection.
  const inspectorNodeId = selectedNodeIds.length === 1 ? selectedNodeIds[0] : null;

  // NodeViews live in their own React roots (Rete renders each node with a
  // standalone createRoot), so updates here don't automatically re-render the
  // inline pickers inside each block. Listeners are stored on a ref and
  // invoked whenever the underlying data changes.
  const ctxListenersRef = useRef(new Set<() => void>());
  const notifyCtx = useCallback(() => {
    for (const cb of ctxListenersRef.current) cb();
  }, []);

  // Update the selection and re-highlight the canvas (the isolated NodeViews
  // re-read it via notifyCtx).
  const selectNodes = useCallback(
    (ids: string[]) => {
      selectedNodeIdsRef.current = ids;
      setSelectedNodeIds(ids);
      notifyCtx();
    },
    [notifyCtx],
  );
  const selectNode = useCallback(
    (reteId: string | null) => selectNodes(reteId ? [reteId] : []),
    [selectNodes],
  );
  const deleteSelectedNodes = useCallback(() => {
    const api = editorApiRef.current;
    const ids = selectedNodeIdsRef.current;
    if (!api || ids.length === 0) return;
    selectNodes([]);
    // Sequential: connections between two selected nodes mustn't be removed
    // concurrently by both nodes' teardown.
    ids
      .reduce<Promise<unknown>>(
        (p, id) => p.then(() => api.removeNode(id).catch(() => {})),
        Promise.resolve(),
      )
      .then(() => setDirty(true));
  }, [selectNodes, setDirty]);

  const editorCtx = useEditorCtx({
    devicesRef,
    hooksRef,
    variableNamesRef,
    selectedIdRef,
    selectedNodeIdsRef,
    editorApiRef,
    ctxListenersRef,
    selectNode,
    selectNodes,
    deleteSelected: deleteSelectedNodes,
    onContextMenu: setContextMenu,
  });

  // Initial load
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [a, d, h] = await Promise.all([listAutomations(), listDevices(), listHooks()]);
        if (cancelled) return;
        setAutomations(a);
        devicesRef.current = d;
        hooksRef.current = h;
        notifyCtx();
        if (a.length > 0) setSelectedId(a[0].id);
      } catch (e) {
        if (!cancelled) setError(String(e));
      } finally {
        if (!cancelled) patchStatus({ loading: false });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [notifyCtx, setError]);

  const selected = useMemo(
    () => automations.find((a) => a.id === selectedId) ?? null,
    [automations, selectedId],
  );

  const { markDirty, loadGraph, handleSave } = useGraphPersistence({
    editorApiRef,
    selected,
    setAutomations,
    setSaving,
    setError,
    setDirty,
  });

  // Keep the variable-name pool + selected id current for autocomplete /
  // preview, and notify the isolated NodeViews so their pickers refresh.
  useEffect(() => {
    variableNamesRef.current = Object.keys(selected?.variables ?? {});
    selectedIdRef.current = selected?.id ?? null;
    notifyCtx();
  }, [selected, notifyCtx]);

  // Imperative load — kept off the render path so we don't need editorReady
  // as a state variable that triggers re-renders. Held in a ref so the mount
  // effect's `.then` callback can always invoke the latest version without
  // having to depend on it (which would otherwise re-create the editor on
  // every automations update).
  const loadIntoEditor = useCallback(
    async (id: string | null) => {
      const api = editorApiRef.current;
      const target = id == null ? null : automations.find((a) => a.id === id) ?? null;
      if (!api || !editorReadyRef.current) {
        pendingLoadRef.current = { id };
        return;
      }
      // Swapping automations invalidates the old node ids — close the inspector.
      selectNode(null);
      await loadGraph(id, target?.nodes ?? [], target?.edges ?? []);
    },
    [automations, loadGraph, selectNode],
  );
  const loadIntoEditorRef = useRef(loadIntoEditor);
  loadIntoEditorRef.current = loadIntoEditor;

  // Mount the editor once.
  // Refs are captured into locals up-front so the cleanup never touches them
  // by current-name (silences the "ref value will have changed by cleanup
  // time" lint while preserving identical semantics).
  useEffect(() => {
    const container = editorContainerRef.current;
    if (!container) return;
    const apiRef = editorApiRef;
    const readyRef = editorReadyRef;
    const pendingRef = pendingLoadRef;
    let cancelled = false;
    let api: CreateEditorResult | null = null;

    createEditor(container, editorCtx)
      .then((created) => {
        if (cancelled) {
          created.destroy();
          return;
        }
        api = created;
        apiRef.current = created;
        readyRef.current = true;
        created.onChange(() => {
          markDirty();
          notifyCtx();
        });
        const pending = pendingRef.current;
        if (pending) {
          pendingRef.current = null;
          void loadIntoEditorRef.current(pending.id);
        }
      })
      .catch((err) => {
        setError(`editor failed: ${err}`);
      });

    return () => {
      cancelled = true;
      api?.destroy();
      apiRef.current = null;
      readyRef.current = false;
    };
  }, [editorCtx, notifyCtx, setError, markDirty]);

  useEffect(() => {
    void loadIntoEditor(selectedId);
  }, [selectedId, loadIntoEditor]);

  const { handleAdd, handleDelete, handleRename, handleToggleEnabled, handleRenameLocal } =
    useAutomationCrud({ selectedId, setAutomations, setSelectedId, setError });

  const { handleAddNode, handleCanvasDrop, handleDeleteNode } = useNodeOps({
    editorApiRef,
    selected,
    selectNode,
    setDirty,
    setError,
  });

  const inspectorCtx = useInspectorCtx({
    devicesRef,
    hooksRef,
    variableNamesRef,
    selectedIdRef,
    editorApiRef,
  });

  const { handleExport, handleImport } = useAutomationFiles({
    selected,
    setError,
    onImported: (created) => {
      setAutomations((prev) => [...prev, created]);
      setSelectedId(created.id);
    },
  });

  return (
    <div className={`fb-automations ${inspectorNodeId ? "with-inspector" : ""}`}>
      {loading ? <div className="fb-loading">Loading automations…</div> : null}
      <AutomationsSidebar
        automations={automations}
        selectedId={selectedId}
        onSelect={setSelectedId}
        onAdd={handleAdd}
        onImport={handleImport}
        onToggleEnabled={handleToggleEnabled}
        onDelete={handleDelete}
        onAddNode={handleAddNode}
        canAddNodes={!!selected}
      />
      <main className="fb-canvas-wrap">
        <AutomationToolbar
          selected={selected}
          dirty={dirty}
          saving={saving}
          onLocalRename={handleRenameLocal}
          onCommitRename={handleRename}
          onSave={handleSave}
          onExport={handleExport}
        />
        {error ? <div className="fb-error-bar">{error}</div> : null}
        <div
          className="fb-canvas"
          ref={editorContainerRef}
          onDragOver={(e) => {
            if (e.dataTransfer.types.includes("application/fusebox-node")) {
              e.preventDefault();
              e.dataTransfer.dropEffect = "copy";
            }
          }}
          onDrop={handleCanvasDrop}
        />
      </main>
      {inspectorNodeId && editorApiRef.current ? (
        <NodeInspector
          key={inspectorNodeId}
          nodeId={inspectorNodeId}
          api={editorApiRef.current}
          ctx={inspectorCtx}
          onDirty={markDirty}
          onClose={() => selectNode(null)}
          onDelete={handleDeleteNode}
        />
      ) : null}
      {contextMenu && editorApiRef.current ? (
        <CanvasContextMenu
          x={contextMenu.x}
          y={contextMenu.y}
          onNode={contextMenu.onNode}
          api={editorApiRef.current}
          onDelete={deleteSelectedNodes}
          onClose={() => setContextMenu(null)}
        />
      ) : null}
    </div>
  );
}
