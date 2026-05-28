import { useRef, type ReactElement } from "react";
import type { Automation, NodeConfig } from "../types";
import { NODE_TEMPLATES } from "./nodes";

interface Props {
  automations: Automation[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  onAdd: () => void;
  onImport: (file: File) => void;
  onToggleEnabled: (id: string, enabled: boolean) => void;
  onDelete: (id: string) => void;
  onAddNode: (config: NodeConfig) => void;
  canAddNodes: boolean;
}

export function AutomationsSidebar({
  automations,
  selectedId,
  onSelect,
  onAdd,
  onImport,
  onToggleEnabled,
  onDelete,
  onAddNode,
  canAddNodes,
}: Props) {
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  return (
    <aside className="fb-sidebar">
      <div className="fb-sidebar-section">
        <header className="fb-sidebar-header">
          <h3>Automations</h3>
          <div className="fb-sidebar-actions">
            <button
              type="button"
              title="Import an automation from a JSON file"
              onClick={() => fileInputRef.current?.click()}
            >
              Import
            </button>
            <button type="button" onClick={onAdd}>
              + New
            </button>
          </div>
          <input
            ref={fileInputRef}
            type="file"
            accept="application/json,.json"
            aria-label="Import automation file"
            hidden
            onChange={(e) => {
              const file = e.target.files?.[0];
              if (file) onImport(file);
              e.target.value = "";
            }}
          />
        </header>
        <ul className="fb-auto-list">
          {automations.length === 0 && (
            <li className="fb-auto-empty">No automations yet.</li>
          )}
          {automations.map((a) => (
            <li key={a.id} className={a.id === selectedId ? "selected" : ""}>
              <div className="fb-auto-row">
                <button
                  type="button"
                  className="fb-auto-pick"
                  aria-pressed={a.id === selectedId}
                  onClick={() => onSelect(a.id)}
                >
                  <span className={`fb-status-dot ${a.enabled ? "on" : "off"}`} />
                  <span className="fb-auto-name">{a.name}</span>
                </button>
                <button
                  className="fb-icon-btn"
                  type="button"
                  title={a.enabled ? "Disable" : "Enable"}
                  onClick={() => onToggleEnabled(a.id, !a.enabled)}
                >
                  {a.enabled ? "⏸" : "▶"}
                </button>
                <button
                  className="fb-icon-btn"
                  type="button"
                  title="Delete"
                  onClick={() => onDelete(a.id)}
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
        <Palette onAdd={onAddNode} disabled={!canAddNodes} />
      </div>
    </aside>
  );
}

const CATEGORIES: Array<{ key: "trigger" | "logic" | "action"; label: string }> = [
  { key: "trigger", label: "Triggers" },
  { key: "logic", label: "Logic" },
  { key: "action", label: "Actions" },
];

function Palette({
  onAdd,
  disabled,
}: {
  onAdd: (config: NodeConfig) => void;
  disabled: boolean;
}) {
  return (
    <div className="fb-palette">
      {CATEGORIES.map((cat) => (
        <div key={cat.key} className={`fb-palette-cat fb-palette-${cat.key}`}>
          <h4>{cat.label}</h4>
          <div className="fb-palette-items">
            {NODE_TEMPLATES.reduce<ReactElement[]>((acc, t) => {
              if (t.category === cat.key) {
                acc.push(
                  <button
                    key={t.kind}
                    type="button"
                    disabled={disabled}
                    onClick={() => onAdd(t.defaultConfig())}
                    title={t.description}
                  >
                    {t.label}
                  </button>,
                );
              }
              return acc;
            }, [])}
          </div>
        </div>
      ))}
    </div>
  );
}
