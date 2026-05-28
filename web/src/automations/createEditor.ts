import { NodeEditor, ClassicPreset, GetSchemes } from "rete";
import { AreaPlugin, AreaExtensions, Drag } from "rete-area-plugin";
import { ConnectionPlugin, Presets as ConnectionPresets } from "rete-connection-plugin";
import { ReactPlugin, Presets as ReactPresets, ReactArea2D } from "rete-react-plugin";
import { getDOMSocketPosition } from "rete-render-utils";
import { createRoot } from "react-dom/client";

import type { AutomationNode, AutomationEdge, NodeConfig } from "../types";
import { templateFor } from "./nodes";
import { NodeView } from "./NodeView";

const signalSocket = new ClassicPreset.Socket("signal");

const NODE_WIDTHS: Partial<Record<NodeConfig["kind"], number>> = {
  cron_trigger: 280,
  interval_trigger: 280,
  device_event_trigger: 260,
  between: 260,
  http_request: 300,
  if_condition: 280,
  logic_and: 200,
  logic_or: 200,
  logic_not: 200,
  debounce: 240,
  expression: 300,
  set_variable: 280,
  get_variable: 240,
  set_device: 260,
  toggle_device: 240,
  fire_hook: 260,
};

export class FlowNode extends ClassicPreset.Node {
  width: number;
  height = 120;
  config: NodeConfig;

  constructor(config: NodeConfig) {
    super(templateFor(config.kind).label);
    this.config = config;
    this.width = NODE_WIDTHS[config.kind] ?? 240;
    const tpl = templateFor(config.kind);
    if (tpl.hasInput) {
      this.addInput("in", new ClassicPreset.Input(signalSocket, "in", true));
    }
    for (const out of tpl.outputs) {
      this.addOutput(out.key, new ClassicPreset.Output(signalSocket, out.label));
    }
  }
}

export type FlowConnection = ClassicPreset.Connection<ClassicPreset.Node, ClassicPreset.Node>;
export type Schemes = GetSchemes<ClassicPreset.Node, FlowConnection>;
type AreaExtra = ReactArea2D<Schemes>;

export interface CreateEditorResult {
  editor: NodeEditor<Schemes>;
  area: AreaPlugin<Schemes, AreaExtra>;
  destroy: () => void;
  serialize: () => { nodes: AutomationNode[]; edges: AutomationEdge[] };
  load: (nodes: AutomationNode[], edges: AutomationEdge[]) => Promise<void>;
  addNodeAt: (config: NodeConfig, x: number, y: number) => Promise<string>;
  /** Add a node at a screen point (drag-and-drop from the palette). */
  addNodeAtClient: (config: NodeConfig, clientX: number, clientY: number) => Promise<string>;
  removeNode: (nodeId: string) => Promise<void>;
  /** Snapshot of currently-loaded nodes for picker dropdowns (IF block). */
  listNodes: () => { id: string; kind: NodeConfig["kind"]; label: string }[];
  findUpstreamKind: (targetReteId: string) => NodeConfig["kind"] | null;
  findUpstreamLogicalId: (targetReteId: string) => string | null;
  /** Copy the current selection to the clipboard. */
  copySelection: () => void;
  /** Paste the clipboard (offset + selected). */
  paste: () => Promise<void>;
  /** Select every node. */
  selectAll: () => void;
  onChange: (cb: () => void) => () => void;
}

export interface EditorContext {
  devices: () => { name: string; nickname: string }[];
  hooks: () => { id: string; name: string }[];
  /** Snapshot of the editor's currently-loaded nodes; powers the IF picker. */
  listNodes?: () => { id: string; kind: string; label: string }[];
  /** Find the kind of the node wired to `targetReteId`'s "in" socket.
   *  Returns null when nothing is connected. Used by the IF block to
   *  populate its field dropdown with the upstream's data outputs. */
  findUpstreamKind?: (targetReteId: string) => NodeConfig["kind"] | null;
  /** Logical id of the node wired to `targetReteId`'s IN socket, for
   *  resolving live outputs when previewing an expression. */
  findUpstreamLogicalId?: (targetReteId: string) => string | null;
  /** Names of the current automation's variables (for $-autocomplete). */
  variableNames?: () => string[];
  /** Evaluate an expression against the automation's live state for preview. */
  previewExpression?: (
    upstreamId: string | null,
    expression: string,
  ) => Promise<{ ok: boolean; result_text?: string; error?: string; input_fields: string[] }>;
  /** Select a single node (click) — opens the inspector. */
  selectNode?: (reteId: string) => void;
  /** Replace the selection with a set of nodes (rubber-band). */
  selectNodes?: (reteIds: string[]) => void;
  /** Whether a node is currently selected, for the canvas highlight. */
  isSelected?: (reteId: string) => boolean;
  /** Delete all currently-selected nodes (Delete key). */
  deleteSelected?: () => void;
  /** Right-click: open the canvas context menu at the given screen point. */
  onContextMenu?: (at: { x: number; y: number; onNode: boolean }) => void;
  /** Subscribe to changes in devices/hooks. Returns an unsubscribe. */
  subscribeContext: (cb: () => void) => () => void;
}

export async function createEditor(
  container: HTMLElement,
  ctx: EditorContext,
): Promise<CreateEditorResult> {
  const editor = new NodeEditor<Schemes>();
  const area = new AreaPlugin<Schemes, AreaExtra>(container);
  const connection = new ConnectionPlugin<Schemes, AreaExtra>();
  const react = new ReactPlugin<Schemes, AreaExtra>({ createRoot });

  AreaExtensions.selectableNodes(area, AreaExtensions.selector(), {
    accumulating: AreaExtensions.accumulateOnCtrl(),
  });

  react.addPreset(
    ReactPresets.classic.setup({
      // Anchor connections at the centre of each pin chip (the default adds a
      // ±12px horizontal offset meant for left→right layouts, which pushed
      // the line off the YES/NO chips). Nudge slightly toward the outer edge.
      socketPositionWatcher: getDOMSocketPosition<Schemes, AreaExtra>({
        offset: (position, _nodeId, side) => ({
          x: position.x,
          y: position.y + (side === "input" ? -8 : 8),
        }),
      }),
      customize: {
        node() {
          return NodeView as any;
        },
      },
    }),
  );

  // Draw connections as a vertical curve (down out of the source, up into the
  // target) instead of the classic horizontal bezier — our blocks flow
  // top→bottom, so the default sideways control points made an S-bend and the
  // line appeared to leave from between the YES/NO pins.
  react.addPipe((context) => {
    if (context && (context as any).type === "connectionpath") {
      const points = (context as any).data?.points;
      if (Array.isArray(points) && points[0] && points[1]) {
        (context as any).data.path = verticalConnectionPath(points[0], points[1]);
      }
    }
    return context;
  });

  connection.addPreset(ConnectionPresets.classic.setup());

  editor.use(area);
  area.use(connection);
  area.use(react);

  AreaExtensions.simpleNodesOrder(area);

  // Pan with the middle mouse button (touch still pans), freeing left-drag on
  // the background for rubber-band selection below.
  area.area.setDragHandler(
    new Drag({
      down: (e) => !(e.pointerType === "mouse" && e.button !== 1),
      move: () => true,
    }),
  );
  // Suppress the browser's middle-click autoscroll.
  const preventAutoscroll = (e: PointerEvent) => {
    if (e.button === 1) e.preventDefault();
  };
  container.addEventListener("pointerdown", preventAutoscroll);

  // Rubber-band selection: left-drag on the empty background draws a box and
  // selects the nodes it touches.
  let band: { x0: number; y0: number; el: HTMLDivElement } | null = null;
  const bandRect = () => {
    if (!band) return null;
    const r = container.getBoundingClientRect();
    return { x0: band.x0 - r.left, y0: band.y0 - r.top, rect: r };
  };
  const drawBand = (cx: number, cy: number) => {
    const info = bandRect();
    if (!band || !info) return;
    const x1 = cx - info.rect.left;
    const y1 = cy - info.rect.top;
    const left = Math.min(info.x0, x1);
    const top = Math.min(info.y0, y1);
    band.el.style.left = `${left}px`;
    band.el.style.top = `${top}px`;
    band.el.style.width = `${Math.abs(x1 - info.x0)}px`;
    band.el.style.height = `${Math.abs(y1 - info.y0)}px`;
  };
  const onBandMove = (e: PointerEvent) => drawBand(e.clientX, e.clientY);
  const onBandUp = (e: PointerEvent) => {
    window.removeEventListener("pointermove", onBandMove);
    if (!band) return;
    const sel = new DOMRect(
      Math.min(band.x0, e.clientX),
      Math.min(band.y0, e.clientY),
      Math.abs(e.clientX - band.x0),
      Math.abs(e.clientY - band.y0),
    );
    band.el.remove();
    band = null;
    // Which nodes intersect the box (client coords)?
    const hit: string[] = [];
    for (const n of editor.getNodes()) {
      const view = area.nodeViews.get(n.id);
      const el = view?.element;
      if (!el) continue;
      const b = el.getBoundingClientRect();
      const overlaps =
        b.left < sel.right && b.right > sel.left && b.top < sel.bottom && b.bottom > sel.top;
      if (overlaps) hit.push(n.id);
    }
    ctx.selectNodes?.(hit);
  };
  const onBandDown = (e: PointerEvent) => {
    if (e.button !== 0) return;
    const target = e.target as HTMLElement;
    // Ignore drags that start on a node, a pin, or any control.
    if (target.closest(".fb-node, .fb-pin")) return;
    const el = document.createElement("div");
    el.className = "fb-rubberband";
    container.appendChild(el);
    band = { x0: e.clientX, y0: e.clientY, el };
    drawBand(e.clientX, e.clientY);
    window.addEventListener("pointermove", onBandMove);
    window.addEventListener("pointerup", onBandUp, { once: true });
  };
  container.addEventListener("pointerdown", onBandDown);

  // Keyboard shortcuts, scoped to this editor's lifetime (torn down with the
  // automation tab) and ignored while typing in a field.
  const onKeyDown = (e: KeyboardEvent) => {
    const t = e.target as HTMLElement | null;
    if (t && t.closest("input, textarea, select, [contenteditable='true']")) return;
    const meta = e.ctrlKey || e.metaKey;
    const key = e.key.toLowerCase();
    if (e.key === "Delete" || e.key === "Backspace") {
      ctx.deleteSelected?.();
    } else if (meta && key === "c") {
      copySelection();
    } else if (meta && key === "v") {
      void paste();
    } else if (meta && key === "a") {
      e.preventDefault();
      selectAll();
    }
  };
  window.addEventListener("keydown", onKeyDown);

  // Right-click opens a context menu. If it lands on a node that isn't part
  // of the current selection, select just that node first.
  const nodeIdFromElement = (el: HTMLElement): string | null => {
    for (const [id, view] of area.nodeViews) {
      if (view.element && view.element.contains(el)) return id;
    }
    return null;
  };
  const onContextMenu = (e: MouseEvent) => {
    e.preventDefault();
    const nodeId = nodeIdFromElement(e.target as HTMLElement);
    if (nodeId && !ctx.isSelected?.(nodeId)) ctx.selectNode?.(nodeId);
    ctx.onContextMenu?.({ x: e.clientX, y: e.clientY, onNode: !!nodeId });
  };
  container.addEventListener("contextmenu", onContextMenu);

  const listeners = new Set<() => void>();
  const notify = () => listeners.forEach((l) => l());

  editor.addPipe((ctx) => {
    if (
      ctx.type === "nodecreated" ||
      ctx.type === "noderemoved" ||
      ctx.type === "connectioncreated" ||
      ctx.type === "connectionremoved"
    ) {
      notify();
    }
    return ctx;
  });

  // Track the node currently grabbed by the pointer so we can move the whole
  // selection together when one of several selected nodes is dragged. (Clicks
  // are turned into single-selection by NodeView's own pointerup handler, so
  // we deliberately don't select on nodepicked — that would collapse a
  // multi-selection the moment you start dragging it.)
  let draggedNodeId: string | null = null;
  area.addPipe((context) => {
    const type = (context as any).type as string;
    if (type === "nodedragged") notify();
    if (type === "nodepicked") {
      draggedNodeId = (context as any).data?.id ?? null;
    } else if (type === "pointerup") {
      draggedNodeId = null;
    } else if (type === "nodetranslated") {
      const { id, position, previous } = (context as any).data;
      if (id === draggedNodeId && ctx.isSelected?.(id)) {
        const dx = position.x - previous.x;
        const dy = position.y - previous.y;
        if (dx !== 0 || dy !== 0) {
          for (const n of editor.getNodes()) {
            if (n.id === id || !ctx.isSelected?.(n.id)) continue;
            const view = area.nodeViews.get(n.id);
            if (view) {
              void area.translate(n.id, { x: view.position.x + dx, y: view.position.y + dy });
            }
          }
        }
      }
    }
    return context;
  });

  const idMap = new Map<string, string>(); // logical id -> rete id

  // Resolve "what's wired to this node's IN socket". Powers the IF block's
  // field dropdown and the expression preview's upstream lookup.
  const upstreamConnection = (targetReteId: string) =>
    editor.getConnections().find(
      (c: any) => c.target === targetReteId && (c.targetInput ?? "in") === "in",
    );
  const findUpstreamKind = (targetReteId: string): NodeConfig["kind"] | null => {
    const conn = upstreamConnection(targetReteId);
    if (!conn) return null;
    const source = editor.getNode(conn.source);
    if (!source) return null;
    return (source as unknown as FlowNode).config.kind;
  };
  const findUpstreamLogicalId = (targetReteId: string): string | null => {
    const conn = upstreamConnection(targetReteId);
    if (!conn) return null;
    return reverseLookup(idMap, conn.source) ?? conn.source;
  };

  // Add a node at the given area-space position. Returns the Rete id.
  const placeNode = async (config: NodeConfig, x: number, y: number): Promise<string> => {
    const node = new FlowNode(config);
    await editor.addNode(node as unknown as ClassicPreset.Node);
    await area.translate(node.id, { x, y });
    idMap.set(crypto.randomUUID(), node.id);
    notify();
    return node.id;
  };

  const selectedReteIds = (): string[] =>
    editor.getNodes().reduce<string[]>((acc, n) => {
      if (ctx.isSelected?.(n.id)) acc.push(n.id);
      return acc;
    }, []);

  // Copy/paste clipboard: configs + positions of the selected nodes, plus the
  // edges wholly within the selection (referenced by index).
  type Clipboard = {
    nodes: { config: NodeConfig; x: number; y: number }[];
    edges: { from: number; to: number; fromSocket: string; toSocket: string }[];
  };
  let clipboard: Clipboard | null = null;
  const PASTE_OFFSET = 40;

  const copySelection = () => {
    const ids = selectedReteIds();
    if (ids.length === 0) return;
    const indexOf = new Map(ids.map((id, i) => [id, i] as const));
    const nodes = ids.map((id) => {
      const flow = editor.getNode(id) as unknown as FlowNode;
      const pos = area.nodeViews.get(id)?.position ?? { x: 0, y: 0 };
      return { config: structuredClone(flow.config), x: pos.x, y: pos.y };
    });
    const edges = editor.getConnections().reduce<Clipboard["edges"]>((acc, c: any) => {
      if (indexOf.has(c.source) && indexOf.has(c.target)) {
        acc.push({
          from: indexOf.get(c.source)!,
          to: indexOf.get(c.target)!,
          fromSocket: c.sourceOutput ?? "out",
          toSocket: c.targetInput ?? "in",
        });
      }
      return acc;
    }, []);
    clipboard = { nodes, edges };
  };

  const paste = async () => {
    if (!clipboard || clipboard.nodes.length === 0) return;
    const snapshot = clipboard;
    const newIds = await Promise.all(
      snapshot.nodes.map((n) =>
        placeNode(structuredClone(n.config), n.x + PASTE_OFFSET, n.y + PASTE_OFFSET),
      ),
    );
    await Promise.all(
      snapshot.edges.map((e) => {
        const source = editor.getNode(newIds[e.from]);
        const target = editor.getNode(newIds[e.to]);
        if (!source || !target) return Promise.resolve();
        return editor.addConnection(
          new ClassicPreset.Connection(source, e.fromSocket, target, e.toSocket) as FlowConnection,
        );
      }),
    );
    // Cascade subsequent pastes so they don't stack on the same spot.
    clipboard = {
      nodes: snapshot.nodes.map((n) => ({ ...n, x: n.x + PASTE_OFFSET, y: n.y + PASTE_OFFSET })),
      edges: snapshot.edges,
    };
    ctx.selectNodes?.(newIds);
    notify();
  };

  const selectAll = () => ctx.selectNodes?.(editor.getNodes().map((n) => n.id));

  const result: CreateEditorResult = {
    editor,
    area,
    findUpstreamKind,
    findUpstreamLogicalId,
    copySelection,
    paste,
    selectAll,
    destroy: () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("pointermove", onBandMove);
      container.removeEventListener("pointerdown", preventAutoscroll);
      container.removeEventListener("pointerdown", onBandDown);
      container.removeEventListener("contextmenu", onContextMenu);
      band?.el.remove();
      area.destroy();
    },
    serialize() {
      const nodes: AutomationNode[] = [];
      for (const n of editor.getNodes()) {
        const view = area.nodeViews.get(n.id);
        const pos = view?.position ?? { x: 0, y: 0 };
        const logicalId = reverseLookup(idMap, n.id) ?? n.id;
        const flow = n as unknown as FlowNode;
        nodes.push({ id: logicalId, config: flow.config, x: pos.x, y: pos.y });
      }
      const edges: AutomationEdge[] = editor.getConnections().map((c: any) => ({
        id: c.id,
        source_node: reverseLookup(idMap, c.source) ?? c.source,
        target_node: reverseLookup(idMap, c.target) ?? c.target,
        source_socket: c.sourceOutput ?? "out",
        target_socket: c.targetInput ?? "in",
      }));
      return { nodes, edges };
    },
    load(nodes, edges) {
      // The four batches are pipeline stages; each must finish before the next
      // can start (connections out → nodes out → nodes in → connections in).
      // We chain them as Promise continuations so the linter doesn't read the
      // intermediate awaits as "independent" operations that could parallelise.
      return Promise.all(
        editor.getConnections().map((c) => editor.removeConnection(c.id)),
      )
        .then(() =>
          Promise.all(editor.getNodes().map((n) => editor.removeNode(n.id))),
        )
        .then(() => {
          idMap.clear();
          return Promise.all(
            nodes.map((n) => {
              const node = new FlowNode(n.config);
              return editor
                .addNode(node as unknown as ClassicPreset.Node)
                .then(() => area.translate(node.id, { x: n.x, y: n.y }))
                .then(() => {
                  idMap.set(n.id, node.id);
                });
            }),
          );
        })
        .then(() =>
          Promise.all(
            edges.map((e) => {
              const sourceId = idMap.get(e.source_node) ?? e.source_node;
              const targetId = idMap.get(e.target_node) ?? e.target_node;
              const source = editor.getNode(sourceId);
              const target = editor.getNode(targetId);
              if (!source || !target) return Promise.resolve();
              const sourceSocket = e.source_socket ?? "out";
              const targetSocket = e.target_socket ?? "in";
              return editor.addConnection(
                new ClassicPreset.Connection(source, sourceSocket, target, targetSocket) as FlowConnection,
              );
            }),
          ),
        )
        .then(() => AreaExtensions.zoomAt(area, editor.getNodes()))
        .then(() => undefined);
    },
    async addNodeAt(config, x, y) {
      const id = await placeNode(config, x, y);
      // Keep the view centred on whatever's currently on the canvas so
      // click-added nodes don't end up offscreen.
      await AreaExtensions.zoomAt(area, editor.getNodes());
      return id;
    },
    addNodeAtClient(config, clientX, clientY) {
      // Translate a screen point (where the user dropped) into area space,
      // accounting for the current pan/zoom, and drop the node there without
      // recentering the view.
      const rect = container.getBoundingClientRect();
      const tf = (area as any).area?.transform ?? { x: 0, y: 0, k: 1 };
      const k = tf.k || 1;
      const x = (clientX - rect.left - tf.x) / k;
      const y = (clientY - rect.top - tf.y) / k;
      return placeNode(config, x, y);
    },
    listNodes() {
      const result: { id: string; kind: NodeConfig["kind"]; label: string }[] = [];
      for (const n of editor.getNodes()) {
        const logical = reverseLookup(idMap, n.id) ?? n.id;
        const flow = n as unknown as FlowNode;
        result.push({ id: logical, kind: flow.config.kind, label: flow.label });
      }
      return result;
    },
    async removeNode(nodeId) {
      // Remove any connections touching this node first; Rete throws if
      // you try to remove a node that still has edges.
      const connections = editor.getConnections().filter(
        (c) => c.source === nodeId || c.target === nodeId,
      );
      await Promise.all(connections.map((c) => editor.removeConnection(c.id)));
      await editor.removeNode(nodeId);
      // Drop the id mapping so serialize() doesn't keep a stale entry.
      for (const [logical, rete] of idMap.entries()) {
        if (rete === nodeId) idMap.delete(logical);
      }
      notify();
    },
    onChange(cb) {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
  };

  // Make context accessible to nodes for device/hook pickers + IF lookups.
  const enrichedCtx: EditorContext = { ...ctx, findUpstreamKind, findUpstreamLogicalId };
  (container as any).__fuseboxCtx = enrichedCtx;
  (container as any).__fuseboxEditor = result;

  return result;
}

function reverseLookup<K, V>(map: Map<K, V>, value: V): K | undefined {
  for (const [k, v] of map.entries()) if (v === value) return k;
  return undefined;
}

/** A mostly-vertical cubic bezier: leaves the source going straight down and
 *  enters the target going straight up. Far less bendy than the classic
 *  horizontal path for our top→bottom block layout. */
function verticalConnectionPath(a: { x: number; y: number }, b: { x: number; y: number }): string {
  const dy = Math.abs(b.y - a.y);
  const k = Math.max(20, Math.min(dy * 0.6, 80));
  return `M ${a.x} ${a.y} C ${a.x} ${a.y + k} ${b.x} ${b.y - k} ${b.x} ${b.y}`;
}
