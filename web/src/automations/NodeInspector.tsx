import { useState } from "react";
import type { NodeConfig } from "../types";
import type { CreateEditorResult, FlowNode } from "./createEditor";
import { NodeBody, type EditorCtx } from "./NodeView";
import { templateFor, iconFor } from "./nodes";

interface Props {
  /** Rete node id of the selected block. */
  nodeId: string;
  api: CreateEditorResult;
  ctx: EditorCtx;
  onDirty: () => void;
  onClose: () => void;
  onDelete: (nodeId: string) => void;
}

export function NodeInspector({ nodeId, api, ctx, onDirty, onClose, onDelete }: Props) {
  const node = api.editor.getNode(nodeId) as unknown as FlowNode | undefined;
  // Local mirror of the node's config so edits re-render the inspector. The
  // parent keys this component on nodeId, so it re-initialises per selection.
  const [config, setConfig] = useState<NodeConfig | null>(node?.config ?? null);

  if (!node || !config) {
    return (
      <aside className="fb-inspector" aria-label="Block settings">
        <div className="fb-inspector-empty">Block not found.</div>
      </aside>
    );
  }

  const tpl = templateFor(config.kind);
  const update = (next: NodeConfig) => {
    node.config = next;
    setConfig(next);
    // Re-render the canvas card so its summary reflects the change.
    void api.area.update("node", nodeId);
    onDirty();
  };

  return (
    <aside className="fb-inspector" aria-label="Block settings">
      <header className="fb-inspector-head">
        <span className={`fb-node-icon fb-node-icon-${tpl.category}`} aria-hidden="true">
          {iconFor(config.kind)}
        </span>
        <div className="fb-inspector-titles">
          <span className="fb-inspector-title">{tpl.label}</span>
          <span className="fb-inspector-desc">{tpl.description}</span>
        </div>
        <button
          type="button"
          className="fb-inspector-close"
          aria-label="Close settings"
          onClick={onClose}
        >
          ×
        </button>
      </header>
      <div className="fb-inspector-body">
        <NodeBody nodeId={nodeId} config={config} update={update} ctx={ctx} />
      </div>
      <footer className="fb-inspector-foot">
        <button type="button" className="fb-node-delete" onClick={() => onDelete(nodeId)}>
          Delete block
        </button>
      </footer>
    </aside>
  );
}
