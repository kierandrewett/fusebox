import type { AutomationEdge, AutomationNode } from "../types";

// Unsaved editor changes are mirrored to localStorage per automation, so
// closing the tab (or switching automations) doesn't lose progress. A draft
// exists only while there are unsaved changes; it's cleared on save/delete.

export interface GraphDraft {
  nodes: AutomationNode[];
  edges: AutomationEdge[];
}

const PREFIX = "fusebox:draft:";

export function saveDraft(automationId: string, graph: GraphDraft): void {
  try {
    localStorage.setItem(PREFIX + automationId, JSON.stringify(graph));
  } catch {
    // Storage unavailable / full — drafts are best-effort.
  }
}

export function loadDraft(automationId: string): GraphDraft | null {
  try {
    const raw = localStorage.getItem(PREFIX + automationId);
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    if (parsed && Array.isArray(parsed.nodes) && Array.isArray(parsed.edges)) {
      return parsed as GraphDraft;
    }
    return null;
  } catch {
    return null;
  }
}

export function clearDraft(automationId: string): void {
  try {
    localStorage.removeItem(PREFIX + automationId);
  } catch {
    // ignore
  }
}
