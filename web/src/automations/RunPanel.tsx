import { useSyncExternalStore } from "react";
import type { EditorCtx } from "./NodeView";

interface Props {
  /** Rete id of the selected block. */
  nodeId: string;
  ctx: EditorCtx;
}

const TRUNCATE = 300;

/** Live values for the selected block, read from the always-on background flow
 *  run and refreshed whenever it updates (like a debugger's inspector). */
export function RunPanel({ nodeId, ctx }: Props) {
  // Subscribe to the editor context so this re-renders when the background run
  // publishes new results. The result object is stable between runs, so
  // useSyncExternalStore won't tear or loop.
  const result = useSyncExternalStore(
    (cb) => ctx.subscribeContext?.(cb) ?? (() => {}),
    () => ctx.liveResultFor?.(nodeId) ?? null,
  );

  if (!ctx.liveResultFor) return null;

  return (
    <div className="fb-run-panel">
      <div className="fb-run-head">
        <span className="fb-run-title">Live value</span>
        {result ? (
          <span className={`fb-run-flag ${result.error ? "err" : result.value ? "on" : "off"}`}>
            {result.value === undefined ? "—" : String(result.value)}
          </span>
        ) : null}
      </div>
      {!result ? (
        <p className="fb-node-hint">
          Waiting for the flow to run. Triggers are treated as firing, so this shows where the
          current conditions route. Devices and hooks are never touched.
        </p>
      ) : (
        <>
          {result.action ? <div className="fb-run-action">{result.action}</div> : null}
          {result.error ? <div className="fb-run-node-error">{result.error}</div> : null}
          {Object.entries(result.outputs).map(([k, v]) => (
            <div key={k} className="fb-run-output">
              <span className="fb-run-output-key">{k}</span>
              <span className="fb-run-output-val">
                {v.length > TRUNCATE ? `${v.slice(0, TRUNCATE)}…` : v}
              </span>
            </div>
          ))}
        </>
      )}
    </div>
  );
}
