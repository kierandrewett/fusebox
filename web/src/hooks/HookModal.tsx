import { useState } from "react";
import type { Hook } from "../types";

interface Props {
  hook?: Hook | null;
  onSave: (input: Partial<Hook>) => Promise<void>;
  onClose: () => void;
}

const METHODS = ["POST", "GET", "PUT", "PATCH", "DELETE"] as const;

export function HookModal({ hook, onSave, onClose }: Props) {
  const [name, setName] = useState(hook?.name ?? "");
  const [url, setUrl] = useState(hook?.url ?? "");
  const [method, setMethod] = useState(hook?.method ?? "POST");
  const [enabled, setEnabled] = useState(hook?.enabled ?? true);
  const [body, setBody] = useState(hook?.body ?? "");

  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    void onSave({
      name,
      url,
      method,
      enabled,
      body: body.length > 0 ? body : null,
      headers: hook?.headers ?? {},
      event_filter: hook?.event_filter ?? [],
      device_filter: hook?.device_filter ?? [],
    });
  };

  return (
    <div className="modal-backdrop" onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <form className="modal" onSubmit={submit}>
        <h3>{hook?.id ? "Edit hook" : "New hook"}</h3>
        <label>
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} required />
        </label>
        <label>
          URL
          <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://example.com/webhook" required />
        </label>
        <label>
          Method
          <select value={method} onChange={(e) => setMethod(e.target.value)}>
            {METHODS.map((m) => <option key={m} value={m}>{m}</option>)}
          </select>
        </label>
        <label>
          Body (optional)
          <textarea value={body} onChange={(e) => setBody(e.target.value)} rows={3} placeholder="{}" />
        </label>
        <label>
          <input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} />
          {" "}Enabled
        </label>
        <div className="modal-footer">
          <button type="button" onClick={onClose}>Cancel</button>
          <button type="submit" className="scan-button">Save</button>
        </div>
      </form>
    </div>
  );
}
