import { useEffect, useMemo, useRef, useState } from "react";
import {
  listAutomations,
  createAutomation,
  updateAutomation,
  deleteAutomation,
  listDevices,
  listHooks,
} from "../api";
import type { Automation, DeviceSummary, HookSummary, NodeConfig } from "../types";
import { NODE_TEMPLATES } from "./nodes";
import { createEditor, type CreateEditorResult } from "./createEditor";

export function AutomationsTab() {
  const [automations, setAutomations] = useState<Automation[]>([]);
  const [devices, setDevices] = useState<DeviceSummary[]>([]);
  const [hooks, setHooks] = useState<HookSummary[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const editorContainerRef = useRef<HTMLDivElement | null>(null);
  const editorApiRef = useRef<CreateEditorResult | null>(null);

  const devicesRef = useRef<DeviceSummary[]>([]);
  const hooksRef = useRef<HookSummary[]>([]);
  devicesRef.current = devices;
  hooksRef.current = hooks;

  // Initial load
  useEffect(() => {
    (async () => {
      try {
        const [a, d, h] = await Promise.all([listAutomations(), listDevices(), listHooks()]);
        setAutomations(a);
        setDevices(d);
        setHooks(h);
        if (a.length > 0) setSelectedId(a[0].id);
      } catch (e) {
        setError(String(e));
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  // Mount the editor once
  useEffect(() => {
    const container = editorContainerRef.current;
    if (!container) return;
    let cancelled = false;
    let api: CreateEditorResult | null = null;

    createEditor(container, {
      devices: () => devicesRef.current,
      hooks: () => hooksRef.current,
    }).then((created) => {
      if (cancelled) {
        created.destroy();
        return;
      }
      api = created;
      editorApiRef.current = created;
      created.onChange(() => setDirty(true));
    });

    return () => {
      cancelled = true;
      api?.destroy();
      editorApiRef.current = null;
    };
  }, []);

  // Load the selected automation into the editor
  const selected = useMemo(
    () => automations.find((a) => a.id === selectedId) ?? null,
    [automations, selectedId],
  );
  useEffect(() => {
    const api = editorApiRef.current;
    if (!api || !selected) return;
    (async () => {
      await api.load(selected.nodes, selected.edges);
      setDirty(false);
    })();
  }, [selected?.id]);

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
    if (!api) return;
    const x = 200 + Math.random() * 80;
    const y = 80 + Math.random() * 60;
    await api.addNodeAt(config, x, y);
    setDirty(true);
  };

  if (loading) return <div className="fb-loading">Loading automations…</div>;

  return (
    <div className="fb-automations">
      <aside className="fb-sidebar">
        <div className="fb-sidebar-section">
          <header className="fb-sidebar-header">
            <h3>Automations</h3>
            <button type="button" onClick={handleAdd}>
              + New
            </button>
          </header>
          <ul className="fb-auto-list">
            {automations.length === 0 && (
              <li className="fb-auto-empty">No automations yet.</li>
            )}
            {automations.map((a) => (
              <li
                key={a.id}
                className={a.id === selectedId ? "selected" : ""}
                onClick={() => setSelectedId(a.id)}
              >
                <div className="fb-auto-row">
                  <span className={`fb-status-dot ${a.enabled ? "on" : "off"}`} />
                  <span className="fb-auto-name">{a.name}</span>
                  <button
                    className="fb-icon-btn"
                    type="button"
                    title={a.enabled ? "Disable" : "Enable"}
                    onClick={(e) => {
                      e.stopPropagation();
                      handleToggleEnabled(a.id, !a.enabled);
                    }}
                  >
                    {a.enabled ? "⏸" : "▶"}
                  </button>
                  <button
                    className="fb-icon-btn"
                    type="button"
                    title="Delete"
                    onClick={(e) => {
                      e.stopPropagation();
                      handleDelete(a.id);
                    }}
                  >
                    ×
                  </button>
                </div>
                {a.status.last_error ? (
                  <div className="fb-auto-error">⚠ {a.status.last_error}</div>
                ) : null}
              </li>
            ))}
          </ul>
        </div>

        <div className="fb-sidebar-section">
          <header className="fb-sidebar-header">
            <h3>Add block</h3>
          </header>
          <Palette onAdd={handleAddNode} disabled={!selected} />
        </div>
      </aside>

      <main className="fb-canvas-wrap">
        <header className="fb-canvas-toolbar">
          {selected ? (
            <>
              <input
                className="fb-rename"
                type="text"
                value={selected.name}
                onChange={(e) =>
                  setAutomations((prev) =>
                    prev.map((a) => (a.id === selected.id ? { ...a, name: e.target.value } : a)),
                  )
                }
                onBlur={(e) => handleRename(selected.id, e.target.value)}
              />
              <div className="fb-toolbar-spacer" />
              <span className="fb-toolbar-status">
                {dirty ? "Unsaved changes" : "Saved"}
              </span>
              <button
                type="button"
                onClick={handleSave}
                disabled={!dirty || saving}
                className="fb-save-btn"
              >
                {saving ? "Saving…" : "Save"}
              </button>
            </>
          ) : (
            <span className="fb-canvas-empty">Pick an automation, or click + New.</span>
          )}
        </header>
        {error ? <div className="fb-error-bar">{error}</div> : null}
        <div className="fb-canvas" ref={editorContainerRef} />
      </main>
    </div>
  );
}

function Palette({
  onAdd,
  disabled,
}: {
  onAdd: (config: NodeConfig) => void;
  disabled: boolean;
}) {
  const categories: Array<{ key: "trigger" | "logic" | "action"; label: string }> = [
    { key: "trigger", label: "Triggers" },
    { key: "logic", label: "Logic" },
    { key: "action", label: "Actions" },
  ];
  return (
    <div className="fb-palette">
      {categories.map((cat) => (
        <div key={cat.key} className={`fb-palette-cat fb-palette-${cat.key}`}>
          <h4>{cat.label}</h4>
          <div className="fb-palette-items">
            {NODE_TEMPLATES.filter((t) => t.category === cat.key).map((t) => (
              <button
                key={t.kind}
                type="button"
                disabled={disabled}
                onClick={() => onAdd(t.defaultConfig())}
                title={t.description}
              >
                {t.label}
              </button>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}
