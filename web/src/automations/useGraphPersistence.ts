import { useCallback, useRef } from "react";
import { updateAutomation } from "../api";
import type { Automation, AutomationEdge, AutomationNode } from "../types";
import type { CreateEditorResult } from "./createEditor";
import { saveDraft, loadDraft, clearDraft } from "./drafts";

interface Options {
  editorApiRef: React.MutableRefObject<CreateEditorResult | null>;
  selected: Automation | null;
  setAutomations: React.Dispatch<React.SetStateAction<Automation[]>>;
  setSaving: (saving: boolean) => void;
  setError: (error: string | null) => void;
  setDirty: (dirty: boolean) => void;
}

/** Owns the dirty flag's plumbing: mirrors unsaved edits to a per-automation
 *  local draft (so closing the tab or switching automations doesn't lose
 *  work), restores a draft over the server copy on load, and flushes to the
 *  server (clearing the draft) on save. */
export function useGraphPersistence({
  editorApiRef,
  selected,
  setAutomations,
  setSaving,
  setError,
  setDirty,
}: Options) {
  // Which automation's graph is in the editor, and whether we're mid-load (so
  // the load's own change events aren't mistaken for user edits).
  const loadedIdRef = useRef<string | null>(null);
  const loadingRef = useRef(false);

  const markDirty = useCallback(() => {
    if (loadingRef.current) return;
    setDirty(true);
    const id = loadedIdRef.current;
    const api = editorApiRef.current;
    if (id && api) saveDraft(id, api.serialize());
  }, [editorApiRef, setDirty]);

  // Load the editor from a local draft when one exists (unsaved changes),
  // otherwise the server copy. Dirty-tracking is suppressed during the load.
  const loadGraph = useCallback(
    async (id: string | null, serverNodes: AutomationNode[], serverEdges: AutomationEdge[]) => {
      const api = editorApiRef.current;
      if (!api) return;
      const draft = id == null ? null : loadDraft(id);
      loadingRef.current = true;
      try {
        await api.load(draft?.nodes ?? serverNodes, draft?.edges ?? serverEdges);
      } finally {
        loadingRef.current = false;
      }
      loadedIdRef.current = id;
      setDirty(!!draft);
    },
    [editorApiRef, setDirty],
  );

  const handleSave = useCallback(async () => {
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
      clearDraft(selected.id);
      setDirty(false);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  }, [selected, editorApiRef, setAutomations, setSaving, setError, setDirty]);

  return { markDirty, loadGraph, handleSave };
}
