import { NodeEditor, ClassicPreset, GetSchemes } from "rete";
import { AreaPlugin, AreaExtensions } from "rete-area-plugin";
import { ConnectionPlugin, Presets as ConnectionPresets } from "rete-connection-plugin";
import { ReactPlugin, Presets as ReactPresets, ReactArea2D } from "rete-react-plugin";
import { createRoot } from "react-dom/client";

import type { AutomationNode, AutomationEdge, NodeConfig } from "../types";
import { templateFor } from "./nodes";
import { NodeView } from "./NodeView";

const signalSocket = new ClassicPreset.Socket("signal");

const NODE_WIDTHS: Partial<Record<NodeConfig["kind"], number>> = {
  cron_trigger: 280,
  interval_trigger: 280,
  device_event_trigger: 260,
  http_probe: 300,
  logic_and: 200,
  logic_or: 200,
  logic_not: 200,
  debounce: 240,
  set_device: 260,
  toggle_device: 240,
  fire_hook: 260,
};

export class FlowNode extends ClassicPreset.Node {
  width: number;
  height = 120;
  config: NodeConfig;
  expanded: boolean;
  onChange?: () => void;

  constructor(config: NodeConfig, expanded = false) {
    super(templateFor(config.kind).label);
    this.config = config;
    this.expanded = expanded;
    this.width = NODE_WIDTHS[config.kind] ?? 240;
    const tpl = templateFor(config.kind);
    if (tpl.hasInput) {
      this.addInput("in", new ClassicPreset.Input(signalSocket, "in", true));
    }
    if (tpl.hasOutput) {
      this.addOutput("out", new ClassicPreset.Output(signalSocket, "out"));
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
  removeNode: (nodeId: string) => Promise<void>;
  onChange: (cb: () => void) => () => void;
}

export async function createEditor(
  container: HTMLElement,
  ctx: { devices: () => { name: string; nickname: string }[]; hooks: () => { id: string; name: string }[] },
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
      customize: {
        node() {
          return NodeView as any;
        },
      },
    }),
  );

  connection.addPreset(ConnectionPresets.classic.setup());

  editor.use(area);
  area.use(connection);
  area.use(react);

  AreaExtensions.simpleNodesOrder(area);

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

  area.addPipe((ctx) => {
    if (ctx.type === "nodedragged") notify();
    return ctx;
  });

  const idMap = new Map<string, string>(); // logical id -> rete id

  const result: CreateEditorResult = {
    editor,
    area,
    destroy: () => area.destroy(),
    serialize() {
      const nodes: AutomationNode[] = [];
      for (const n of editor.getNodes()) {
        const view = area.nodeViews.get(n.id);
        const pos = view?.position ?? { x: 0, y: 0 };
        const logicalId = reverseLookup(idMap, n.id) ?? n.id;
        const flow = n as unknown as FlowNode;
        nodes.push({ id: logicalId, config: flow.config, x: pos.x, y: pos.y });
      }
      const edges: AutomationEdge[] = editor.getConnections().map((c) => ({
        id: c.id,
        source_node: reverseLookup(idMap, c.source) ?? c.source,
        target_node: reverseLookup(idMap, c.target) ?? c.target,
      }));
      return { nodes, edges };
    },
    async load(nodes, edges) {
      for (const c of [...editor.getConnections()]) await editor.removeConnection(c.id);
      for (const n of [...editor.getNodes()]) await editor.removeNode(n.id);
      idMap.clear();
      for (const n of nodes) {
        const node = new FlowNode(n.config);
        await editor.addNode(node as unknown as ClassicPreset.Node);
        await area.translate(node.id, { x: n.x, y: n.y });
        idMap.set(n.id, node.id);
      }
      for (const e of edges) {
        const sourceId = idMap.get(e.source_node) ?? e.source_node;
        const targetId = idMap.get(e.target_node) ?? e.target_node;
        const source = editor.getNode(sourceId);
        const target = editor.getNode(targetId);
        if (!source || !target) continue;
        await editor.addConnection(
          new ClassicPreset.Connection(source, "out", target, "in") as FlowConnection,
        );
      }
      await AreaExtensions.zoomAt(area, editor.getNodes());
    },
    async addNodeAt(config, x, y) {
      // Freshly-added nodes open expanded so the user can configure
      // them immediately. Loaded nodes start collapsed (see load()).
      const node = new FlowNode(config, true);
      await editor.addNode(node as unknown as ClassicPreset.Node);
      await area.translate(node.id, { x, y });
      const logicalId = crypto.randomUUID();
      idMap.set(logicalId, node.id);
      // Keep the view centred on whatever's currently on the canvas so
      // newly added nodes don't end up offscreen.
      await AreaExtensions.zoomAt(area, editor.getNodes());
      notify();
      return logicalId;
    },
    async removeNode(nodeId) {
      // Remove any connections touching this node first; Rete throws if
      // you try to remove a node that still has edges.
      const connections = editor.getConnections().filter(
        (c) => c.source === nodeId || c.target === nodeId,
      );
      for (const c of connections) await editor.removeConnection(c.id);
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

  // Make context accessible to nodes for device/hook pickers
  (container as any).__fuseboxCtx = ctx;
  (container as any).__fuseboxEditor = result;

  return result;
}

function reverseLookup<K, V>(map: Map<K, V>, value: V): K | undefined {
  for (const [k, v] of map.entries()) if (v === value) return k;
  return undefined;
}
