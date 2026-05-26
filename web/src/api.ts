import type { Automation, DeviceSummary, HookSummary } from "./types";

const BASE = "";

async function jsonOrThrow<T>(res: Response): Promise<T> {
  if (!res.ok) {
    let detail = "";
    try {
      const body = await res.json();
      detail = body?.error?.message ?? body?.message ?? "";
    } catch {
      // ignore
    }
    throw new Error(`${res.status} ${res.statusText}${detail ? ": " + detail : ""}`);
  }
  return res.json() as Promise<T>;
}

export async function listAutomations(): Promise<Automation[]> {
  const res = await fetch(`${BASE}/api/automations`);
  const body = await jsonOrThrow<{ automations: Automation[] }>(res);
  return body.automations;
}

export async function createAutomation(name: string): Promise<Automation> {
  const res = await fetch(`${BASE}/api/automations`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name }),
  });
  return jsonOrThrow<Automation>(res);
}

export async function updateAutomation(
  id: string,
  patch: Partial<Pick<Automation, "name" | "enabled" | "nodes" | "edges">>,
): Promise<Automation> {
  const res = await fetch(`${BASE}/api/automations/${id}`, {
    method: "PATCH",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(patch),
  });
  return jsonOrThrow<Automation>(res);
}

export async function deleteAutomation(id: string): Promise<void> {
  const res = await fetch(`${BASE}/api/automations/${id}`, { method: "DELETE" });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
}

export async function listDevices(): Promise<DeviceSummary[]> {
  const res = await fetch(`${BASE}/api/devices`);
  const body = await jsonOrThrow<{ devices: any[] }>(res);
  return body.devices.map((d) => ({
    name: d.name,
    nickname: d.nickname ?? d.name,
    ip: d.ip ?? "",
    model: d.model ?? "",
  }));
}

export async function listHooks(): Promise<HookSummary[]> {
  const res = await fetch(`${BASE}/api/hooks`);
  const body = await jsonOrThrow<{ hooks: any[] }>(res);
  return body.hooks.map((h) => ({ id: h.id, name: h.name }));
}
