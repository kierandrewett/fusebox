import { useState } from "react";
import type { RunNodeResult } from "../api";
import type { EditorCtx } from "./NodeView";

interface Props {
  /** Rete id of the selected block. */
  nodeId: string;
  ctx: EditorCtx;
}

const TRUNCATE = 200;

/** A "Test run" button that dry-runs the selected block plus everything
 *  upstream of it, then lists each block's computed value, outputs, and (for
 *  action blocks) the side effect it would have performed. */
export function RunPanel({ nodeId, ctx }: Props) {
  const [running, setRunning] = useState(false);
  const [results, setResults] = useState<RunNodeResult[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  if (!ctx.runNode) return null;

  const run = async () => {
    setRunning(true);
    setError(null);
    try {
      const res = await ctx.runNode!(nodeId);
      if (res.ok) {
        setResults(res.nodes);
      } else {
        setResults(null);
        setError(res.error ?? "run failed");
      }
    } catch (e) {
      setResults(null);
      setError(String(e));
    } finally {
      setRunning(false);
    }
  };

  return (
    <div className="fb-run-panel">
      <button type="button" className="fb-toolbar-btn fb-run-btn" onClick={run} disabled={running}>
        {running ? "Running…" : "Test run"}
      </button>
      <p className="fb-node-hint">
        Dry run: evaluates this block and everything upstream. HTTP requests run for real; devices
        and hooks are not touched.
      </p>
      {error ? <div className="fb-expr-preview-error">{error}</div> : null}
      {results ? (
        <ol className="fb-run-results">
          {results.map((r, i) => {
            // The selected node is the sink of the closure, so it's last.
            const isTarget = i === results.length - 1;
            const cls = r.error ? "err" : r.value ? "on" : "off";
            return (
              <li key={r.node_id} className={isTarget ? "target" : ""}>
                <div className="fb-run-row-head">
                  <span className="fb-run-label">{r.title}</span>
                  <span className={`fb-run-flag ${cls}`}>
                    {r.value === undefined ? "—" : String(r.value)}
                  </span>
                </div>
                {r.action ? <div className="fb-run-action">{r.action}</div> : null}
                {r.error ? <div className="fb-run-node-error">{r.error}</div> : null}
                {Object.entries(r.outputs).map(([k, v]) => (
                  <div key={k} className="fb-run-output">
                    <span className="fb-run-output-key">{k}</span>
                    <span className="fb-run-output-val">
                      {v.length > TRUNCATE ? `${v.slice(0, TRUNCATE)}…` : v}
                    </span>
                  </div>
                ))}
              </li>
            );
          })}
        </ol>
      ) : null}
    </div>
  );
}
