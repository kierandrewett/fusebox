import type { RefObject } from "react";
import type { Automation, NodeConfig } from "../types";
import type { CreateEditorResult } from "./createEditor";
import { templateFor } from "./nodes";

interface Options {
  editorApiRef: RefObject<CreateEditorResult | null>;
  selected: Automation | null;
  selectNode: (reteId: string | null) => void;
  setDirty: (dirty: boolean) => void;
  setError: (error: string | null) => void;
}

/** Add (click + drag-drop) and delete handlers for canvas blocks. */
export function useNodeOps({ editorApiRef, selected, selectNode, setDirty, setError }: Options) {
  // Click-to-add: stagger by category column / row so blocks don't stack.
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
      const reteId = await api.addNodeAt(config, x, y);
      setDirty(true);
      selectNode(reteId);
    } catch (err) {
      setError(`add node failed: ${err}`);
    }
  };

  // Accept palette drags over the canvas.
  const handleCanvasDragOver = (e: React.DragEvent) => {
    if (e.dataTransfer.types.includes("application/fusebox-node")) {
      e.preventDefault();
      e.dataTransfer.dropEffect = "copy";
    }
  };

  // Drag-drop from the palette: place the block where it was dropped.
  const handleCanvasDrop = (e: React.DragEvent) => {
    const kind = e.dataTransfer.getData("application/fusebox-node");
    if (!kind) return;
    e.preventDefault();
    const api = editorApiRef.current;
    if (!api || !selected) return;
    try {
      const config = templateFor(kind as NodeConfig["kind"]).defaultConfig();
      void api.addNodeAtClient(config, e.clientX, e.clientY).then((reteId) => {
        setDirty(true);
        selectNode(reteId);
      });
    } catch (err) {
      setError(`add node failed: ${err}`);
    }
  };

  const handleDeleteNode = async (reteId: string) => {
    const api = editorApiRef.current;
    if (!api) return;
    try {
      await api.removeNode(reteId);
      selectNode(null);
      setDirty(true);
    } catch (err) {
      setError(`delete failed: ${err}`);
    }
  };

  return { handleAddNode, handleCanvasDragOver, handleCanvasDrop, handleDeleteNode };
}
