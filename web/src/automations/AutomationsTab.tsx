import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import {
  listAutomations,
  createAutomation,
  updateAutomation,
  deleteAutomation,
  exportAutomation,
  importAutomation,
  previewExpression,
  listDevices,
  listHooks,
} from "../api";
import type { Automation, DeviceSummary, HookSummary, NodeConfig } from "../types";
import { createEditor, type CreateEditorResult } from "./createEditor";
import { templateFor } from "./nodes";
import { AutomationsSidebar } from "./AutomationsSidebar";
import { AutomationToolbar } from "./AutomationToolbar";

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
      await api.load(target?.nodes ?? [], target?.edges ?? []);
      setDirty(false);
    },
    [automations, setDirty],
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
  }, [notifyCtx, setError, setDirty]);

  useEffect(() => {
    void loadIntoEditor(selectedId);
  }, [selectedId, loadIntoEditor]);

  const handleAdd = async () => {
    setError(null);
    try {
      const created = await createAutomation("Untitled automation");
      setAutomations((prev) => [...prev, created]);
      setSelectedId(created.id);
    } catch (e) {
      setError(String(e));
    }
  };

  const handleDelete = async (id: string) => {
    if (!confirm("Delete this automation?")) return;
    try {
      await deleteAutomation(id);
      setAutomations((prev) => prev.filter((a) => a.id !== id));
      if (selectedId === id) setSelectedId(null);
    } catch (e) {
      setError(String(e));
    }
  };

  const handleRename = async (id: string, name: string) => {
    try {
      const updated = await updateAutomation(id, { name });
      setAutomations((prev) => prev.map((a) => (a.id === id ? updated : a)));
    } catch (e) {
      setError(String(e));
    }
  };

  const handleToggleEnabled = async (id: string, enabled: boolean) => {
    try {
      const updated = await updateAutomation(id, { enabled });
      setAutomations((prev) => prev.map((a) => (a.id === id ? updated : a)));
    } catch (e) {
      setError(String(e));
    }
  };

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

  const handleAddNode = async (config: NodeConfig) => {
    const api = editorApiRef.current;
    if (!api) {
      setError("editor not ready");
      return;
    }
    const existing = api.editor.getNodes().length;
    const cat = templateFor(config.kind).category;
    const column = cat === "trigger" ? 0 : cat === "logic" ? 1 : 2;
    const x = 60 + column * 280;
    const y = 60 + (existing % 4) * 160;
    try {
      await api.addNodeAt(config, x, y);
      setDirty(true);
    } catch (err) {
      setError(`add node failed: ${err}`);
    }
  };

  const handleRenameLocal = (id: string, name: string) => {
    setAutomations((prev) => prev.map((a) => (a.id === id ? { ...a, name } : a)));
  };

  const handleExport = async () => {
    if (!selected) return;
    setError(null);
    try {
      const data = await exportAutomation(selected.id);
      const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const slug = (selected.name || "automation")
        .toLowerCase()
        .replace(/[^a-z0-9]+/g, "-")
        .replace(/(^-|-$)/g, "") || "automation";
      const a = document.createElement("a");
      a.href = url;
      a.download = `${slug}.fusebox.json`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      setError(String(e));
    }
  };

  const handleImport = async (file: File) => {
    setError(null);
    try {
      const text = await file.text();
      const payload = JSON.parse(text);
      const created = await importAutomation(payload);
      setAutomations((prev) => [...prev, created]);
      setSelectedId(created.id);
    } catch (e) {
      setError(`import failed: ${e instanceof Error ? e.message : String(e)}`);
    }
  };

  return (
    <div className="fb-automations">
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
        <div className="fb-canvas" ref={editorContainerRef} />
      </main>
    </div>
  );
}
