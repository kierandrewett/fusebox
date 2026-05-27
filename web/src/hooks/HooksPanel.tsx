import { useEffect, useState } from "react";
import { createHook, deleteHook, listHookDetails, testHook, updateHook } from "../api";
import type { Hook } from "../types";
import { HookModal } from "./HookModal";

export function HooksPanel() {
  const [hooks, setHooks] = useState<Hook[]>([]);
  const [editing, setEditing] = useState<Hook | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);
  const load = () => listHookDetails().then(setHooks).catch((err) => setError(String(err)));
  useEffect(() => { load(); }, []);
  const save = async (input: Partial<Hook>) => {
    if (editing?.id) await updateHook(editing.id, input); else await createHook(input);
    setEditing(undefined);
    load();
  };
  return <section className="panel"><header className="panel-header"><div><h2>Hooks</h2><p>{hooks.length} configured hooks</p></div><button type="button" onClick={() => setEditing(null)}>New hook</button></header>{error ? <p className="error">{error}</p> : null}<div className="hook-list">{hooks.map((hook) => <article className="hook-card" key={hook.id}><h3>{hook.name}</h3><p>{hook.method} {hook.url}</p><p>{hook.enabled ? "Enabled" : "Disabled"}</p><footer><button type="button" onClick={() => setEditing(hook)}>Edit</button><button type="button" onClick={() => void testHook(hook.id).then(load)}>Test</button><button type="button" onClick={() => void deleteHook(hook.id).then(load)}>Delete</button></footer></article>)}</div>{editing !== undefined ? <HookModal hook={editing} onSave={save} onClose={() => setEditing(undefined)} /> : null}</section>;
}
