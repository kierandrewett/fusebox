import { useEffect, useReducer } from "react";
import type { Hook } from "../types";

interface Props {
  hook?: Hook | null;
  onSave: (input: Partial<Hook>) => Promise<void>;
  onClose: () => void;
}

const METHODS = ["POST", "GET", "PUT", "PATCH", "DELETE"] as const;

interface FormState {
  name: string;
  url: string;
  method: string;
  enabled: boolean;
  body: string;
}

type FormAction = { [K in keyof FormState]: { field: K; value: FormState[K] } }[keyof FormState];

function reducer(state: FormState, action: FormAction): FormState {
  return { ...state, [action.field]: action.value } as FormState;
}

function initialState(hook?: Hook | null): FormState {
  return {
    name: hook?.name ?? "",
    url: hook?.url ?? "",
    method: hook?.method ?? "POST",
    enabled: hook?.enabled ?? true,
    body: hook?.body ?? "",
  };
}

export function HookModal({ hook, onSave, onClose }: Props) {
  const [form, dispatch] = useReducer(reducer, hook, initialState);

  // Close on Escape from anywhere — the form is the focus target but keydowns
  // on a non-interactive element trip lint warnings, so listen at the document.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    void onSave({
      name: form.name,
      url: form.url,
      method: form.method,
      enabled: form.enabled,
      body: form.body.length > 0 ? form.body : null,
      headers: hook?.headers ?? {},
      event_filter: hook?.event_filter ?? [],
      device_filter: hook?.device_filter ?? [],
    });
  };

  return (
    <div className="modal-backdrop">
      <button
        type="button"
        className="modal-backdrop-button"
        aria-label="Close dialog"
        onClick={onClose}
      />
      <form
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-label={hook?.id ? "Edit hook" : "New hook"}
        onSubmit={submit}
      >
        <h3>{hook?.id ? "Edit hook" : "New hook"}</h3>
        <label>
          Name
          <input
            aria-label="Hook name"
            value={form.name}
            onChange={(e) => dispatch({ field: "name", value: e.target.value })}
            required
          />
        </label>
        <label>
          URL
          <input
            aria-label="Hook URL"
            value={form.url}
            onChange={(e) => dispatch({ field: "url", value: e.target.value })}
            placeholder="https://example.com/webhook"
            required
          />
        </label>
        <label>
          Method
          <select
            aria-label="HTTP method"
            value={form.method}
            onChange={(e) => dispatch({ field: "method", value: e.target.value })}
          >
            {METHODS.map((m) => <option key={m} value={m}>{m}</option>)}
          </select>
        </label>
        <label>
          Body (optional)
          <textarea
            aria-label="Request body"
            value={form.body}
            onChange={(e) => dispatch({ field: "body", value: e.target.value })}
            rows={3}
            placeholder="{}"
          />
        </label>
        <label>
          <input
            aria-label="Enabled"
            type="checkbox"
            checked={form.enabled}
            onChange={(e) => dispatch({ field: "enabled", value: e.target.checked })}
          />
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
