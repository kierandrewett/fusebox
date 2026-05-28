import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import { listAutomations, updateAutomation, previewExpression, listDevices, listHooks } from "../api";
import type { Automation, DeviceSummary, HookSummary } from "../types";
import { createEditor, type CreateEditorResult } from "./createEditor";
import { AutomationsSidebar } from "./AutomationsSidebar";
import { AutomationToolbar } from "./AutomationToolbar";
import { NodeInspector } from "./NodeInspector";
import type { EditorCtx } from "./NodeView";
import { useAutomationFiles } from "./useAutomationFiles";
import { useAutomationCrud } from "./useAutomationCrud";
import { useNodeOps } from "./useNodeOps";

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
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
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
  const selectedNodeIdRef = useRef<string | null>(null);
  selectedNodeIdRef.current = selectedNodeId;
  const editorContainerRef = useRef<HTMLDivElement | null>(null);
  const editorApiRef = useRef<CreateEditorResult | null>(null);
  const editorReadyRef = useRef(false);
  const pendingLoadRef = useRef<{ id: string | null } | null>(null);

  // NodeViews live in their own React roots (Rete renders each node with a
  // standalone createRoot), so updates here don't automatically re-render the
  // inline pickers inside each block. Listeners are stored on a ref and
  // invoked whenever the underlying data changes.
  const ctxListenersRef = useRef(new Set<() => void>());
  const notifyCtx = useCallback(() => {
    for (const cb of ctxListenersRef.current) cb();
  }, []);

  // Selecting a block opens the inspector and re-highlights the canvas (the
  // isolated NodeViews re-read selectedNodeId via notifyCtx).
  const selectNode = useCallback(
    (reteId: string | null) => {
      selectedNodeIdRef.current = reteId;
      setSelectedNodeId(reteId);
      notifyCtx();
    },
    [notifyCtx],
  );

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
      await api.load(target?.nodes ?? [], target?.edges ?? []);
      setDirty(false);
    },
    [automations, setDirty, selectNode],
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
    const listenersRef = ctxListenersRef;
    let cancelled = false;
    let api: CreateEditorResult | null = null;

    createEditor(container, {
      devices: () => devicesRef.current,
      hooks: () => hooksRef.current,
      variableNames: () => variableNamesRef.current,
      previewExpression: (upstreamId, expression) => {
        const id = selectedIdRef.current;
        if (!id) {
          return Promise.resolve({ ok: false, error: "no automation selected", input_fields: [] });
        }
        return previewExpression(id, upstreamId, expression);
      },
      listNodes: () => apiRef.current?.listNodes() ?? [],
      selectNode: (reteId) => selectNode(reteId),
      selectedNodeId: () => selectedNodeIdRef.current,
      subscribeContext: (cb) => {
        listenersRef.current.add(cb);
        return () => listenersRef.current.delete(cb);
      },
    })
      .then((created) => {
        if (cancelled) {
          created.destroy();
          return;
        }
        api = created;
        apiRef.current = created;
        readyRef.current = true;
        created.onChange(() => {
          setDirty(true);
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
  }, [notifyCtx, setError, setDirty, selectNode]);

  useEffect(() => {
    void loadIntoEditor(selectedId);
  }, [selectedId, loadIntoEditor]);

  const { handleAdd, handleDelete, handleRename, handleToggleEnabled, handleRenameLocal } =
    useAutomationCrud({ selectedId, setAutomations, setSelectedId, setError });

  const handleSave = async () => {
    if (!selected) return;
    const api = editorApiRef.current;
    if (!api) return;
    setSaving(true);
    setError(null);
    try {
      const graph = api.serialize();
      const updated = await updateAutomation(selected.id, {
        nodes: graph.nodes,
        edges: graph.edges,
      });
      setAutomations((prev) => prev.map((a) => (a.id === selected.id ? updated : a)));
      setDirty(false);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const { handleAddNode, handleCanvasDrop, handleDeleteNode } = useNodeOps({
    editorApiRef,
    selected,
    selectNode,
    setDirty,
    setError,
  });

  // Context the inspector's NodeBody uses; delegates upstream lookups +
  // preview to the live editor api and reads device/hook/variable pools.
  const inspectorCtx: EditorCtx = {
    devices: () => devicesRef.current,
    hooks: () => hooksRef.current,
    variableNames: () => variableNamesRef.current,
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
  };

  const { handleExport, handleImport } = useAutomationFiles({
    selected,
    setError,
    onImported: (created) => {
      setAutomations((prev) => [...prev, created]);
      setSelectedId(created.id);
    },
  });

  return (
    <div className={`fb-automations ${selectedNodeId ? "with-inspector" : ""}`}>
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
      {selectedNodeId && editorApiRef.current ? (
        <NodeInspector
          key={selectedNodeId}
          nodeId={selectedNodeId}
          api={editorApiRef.current}
          ctx={inspectorCtx}
          onDirty={() => setDirty(true)}
          onClose={() => selectNode(null)}
          onDelete={handleDeleteNode}
        />
      ) : null}
    </div>
  );
}
