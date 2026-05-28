import { useState, useSyncExternalStore } from "react";
import { createHook, deleteHook, testHook, updateHook } from "../api";
import type { Hook } from "../types";
import { HookModal } from "./HookModal";
import { getErrorSnapshot, getSnapshot, reloadHooks, subscribe } from "./hookStore";

export function HooksPanel() {
  const hooks = useSyncExternalStore(subscribe, getSnapshot);
  const storeError = useSyncExternalStore(subscribe, getErrorSnapshot);
  const [editing, setEditing] = useState<Hook | null | undefined>(undefined);
  const [actionError, setActionError] = useState<string | null>(null);

  const error = actionError ?? storeError;

  const save = async (input: Partial<Hook>) => {
    setActionError(null);
    try {
      if (editing?.id) await updateHook(editing.id, input);
      else await createHook(input);
      setEditing(undefined);
      await reloadHooks();
    } catch (err) {
      setActionError(String(err));
    }
  };

  return (
    <section className="cabinet" aria-live="polite">
      <section className="hooks-panel" aria-label="Hooks">
        <div className="hooks-header">
          <h2>Hooks</h2>
          <button type="button" className="schedule-add" onClick={() => setEditing(null)}>+ Add hook</button>
        </div>
        {error ? <p className="notice" role="alert">{error}</p> : null}
        {hooks === undefined ? (
          <p className="hooks-empty">Loading hooks…</p>
        ) : hooks.length === 0 ? (
          <p className="hooks-empty">No hooks yet. Hooks fire an HTTP request when any device transitions on/off/online/offline.</p>
        ) : (
          <ul className="hook-list">
            {hooks.map((hook) => (
              <li key={hook.id} className={`hook-item ${hook.enabled ? "" : "disabled"}`}>
                <div className="hook-body">
                  <span className="hook-name">{hook.name}</span>
                  <span className="hook-target">{hook.method} {hook.url}</span>
                  <span className="hook-meta">
                    {hook.event_filter?.length ? hook.event_filter.join(", ") : "all events"}
                    {hook.device_filter?.length ? ` · ${hook.device_filter.join(", ")}` : " · all devices"}
                  </span>
                </div>
                <div className="hook-actions">
                  <button type="button" title="Edit" onClick={() => setEditing(hook)}>✎</button>
                  <button type="button" title="Test" onClick={() => void testHook(hook.id).then(() => reloadHooks())}>▶</button>
                  <button
                    type="button"
                    className="hook-delete"
                    title="Delete"
                    onClick={() => {
                      if (confirm(`Delete hook "${hook.name}"?`)) void deleteHook(hook.id).then(() => reloadHooks());
                    }}
                  >
                    ×
                  </button>
                </div>
              </li>
            ))}
          </ul>
        )}
      </section>
      {editing !== undefined ? (
        <HookModal hook={editing} onSave={save} onClose={() => setEditing(undefined)} />
      ) : null}
    </section>
  );
}
