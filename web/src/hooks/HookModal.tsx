import { useState } from "react";
import type { Hook } from "../types";

export function HookModal({ hook, onSave, onClose }: { hook?: Hook | null; onSave: (input: Partial<Hook>) => Promise<void>; onClose: () => void }) {
  const [name, setName] = useState(hook?.name ?? "");
  const [url, setUrl] = useState(hook?.url ?? "");
  const [method, setMethod] = useState(hook?.method ?? "POST");
  return <div className="modal-backdrop"><form className="modal" onSubmit={(event) => { event.preventDefault(); void onSave({ name, url, method, enabled: hook?.enabled ?? true, events: hook?.events ?? [], device_names: hook?.device_names ?? [] }); }}><h2>{hook ? "Edit hook" : "New hook"}</h2><label>Name<input value={name} onChange={(e) => setName(e.target.value)} required /></label><label>URL<input value={url} onChange={(e) => setUrl(e.target.value)} required /></label><label>Method<input value={method} onChange={(e) => setMethod(e.target.value)} required /></label><footer><button type="button" onClick={onClose}>Cancel</button><button type="submit">Save</button></footer></form></div>;
}
