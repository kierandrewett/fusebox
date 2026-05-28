import type { Automation } from "../types";

interface Props {
  selected: Automation | null;
  dirty: boolean;
  saving: boolean;
  onLocalRename: (id: string, name: string) => void;
  onCommitRename: (id: string, name: string) => void;
  onSave: () => void;
  onExport: () => void;
}

export function AutomationToolbar({
  selected,
  dirty,
  saving,
  onLocalRename,
  onCommitRename,
  onSave,
  onExport,
}: Props) {
  return (
    <header className="fb-canvas-toolbar">
      {selected ? (
        <>
          <input
            className="fb-rename"
            type="text"
            aria-label="Automation name"
            value={selected.name}
            onChange={(e) => onLocalRename(selected.id, e.target.value)}
            onBlur={(e) => onCommitRename(selected.id, e.target.value)}
          />
          <div className="fb-toolbar-spacer" />
          <span className="fb-toolbar-status">
            {dirty ? "Unsaved changes" : "Saved"}
          </span>
          <button
            type="button"
            onClick={onExport}
            className="fb-toolbar-btn"
            title="Download this automation as a JSON file"
          >
            Export
          </button>
          <button
            type="button"
            onClick={onSave}
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
  );
}
