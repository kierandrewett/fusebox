import { useState, useSyncExternalStore } from "react";
import { createHook, deleteHook, testHook, updateHook } from "../api";
import type { Hook } from "../types";
import { formatRelative } from "../format";
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
                <button
                  type="button"
                  className={`hook-dot ${hook.enabled ? "on" : "off"}`}
                  title={hook.enabled ? "Enabled — click to disable" : "Disabled — click to enable"}
                  aria-label={hook.enabled ? "Disable hook" : "Enable hook"}
                  onClick={() =>
                    void updateHook(hook.id, { enabled: !hook.enabled }).then(() => reloadHooks())
                  }
                />
                <div className="hook-body">
                  <div className="hook-row">
                    <span className="hook-name">{hook.name}</span>
                    <span className={`hook-method m-${hook.method.toLowerCase()}`}>{hook.method}</span>
                  </div>
                  <span className="hook-url">{hook.url}</span>
                  <div className="hook-tags">
                    <span className="hook-tag">
                      {hook.event_filter?.length ? hook.event_filter.join(" / ") : "all events"}
                    </span>
                    <span className="hook-tag">
                      {hook.device_filter?.length ? hook.device_filter.join(" / ") : "all devices"}
                    </span>
                  </div>
                  <div className="hook-status">
                    {hook.last_error ? (
                      <span className="hook-result bad" title={hook.last_error}>
                        failed{hook.last_status_code ? ` · ${hook.last_status_code}` : ""}
                      </span>
                    ) : hook.last_fired_at_ms ? (
                      <span
                        className={`hook-result ${
                          hook.last_status_code && hook.last_status_code < 400 ? "ok" : "bad"
                        }`}
                      >
                        {hook.last_status_code ?? "sent"}
                      </span>
                    ) : (
                      <span className="hook-result idle">never fired</span>
                    )}
                    {hook.last_fired_at_ms ? (
                      <span className="hook-when">
                        {hook.last_event ? `${hook.last_event} · ` : ""}
                        {formatRelative(hook.last_fired_at_ms)}
                      </span>
                    ) : null}
                  </div>
                </div>
                <div className="hook-actions">
                  <button type="button" title="Edit" aria-label="Edit hook" onClick={() => setEditing(hook)}>
                    Edit
                  </button>
                  <button
                    type="button"
                    title="Send a test request"
                    aria-label="Test hook"
                    onClick={() => void testHook(hook.id).then(() => reloadHooks())}
                  >
                    Test
                  </button>
                  <button
                    type="button"
                    className="hook-delete"
                    title="Delete"
                    aria-label="Delete hook"
                    onClick={() => {
                      if (confirm(`Delete hook "${hook.name}"?`)) void deleteHook(hook.id).then(() => reloadHooks());
                    }}
                  >
                    Delete
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
