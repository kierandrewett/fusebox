import type { Automation, Device, DeviceListResponse, DeviceSummary, Hook, HookSummary, UsageHistoryResponse } from "./types";

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
  const res = await fetch(`${BASE}/api/automations`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ name }) });
  return jsonOrThrow<Automation>(res);
}

export async function updateAutomation(id: string, patch: Partial<Pick<Automation, "name" | "enabled" | "nodes" | "edges">>): Promise<Automation> {
  const res = await fetch(`${BASE}/api/automations/${id}`, { method: "PATCH", headers: { "content-type": "application/json" }, body: JSON.stringify(patch) });
  return jsonOrThrow<Automation>(res);
}

export async function deleteAutomation(id: string): Promise<void> {
  const res = await fetch(`${BASE}/api/automations/${id}`, { method: "DELETE" });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
}

export async function listDeviceResponse(): Promise<DeviceListResponse> {
  const res = await fetch(`${BASE}/api/devices`);
  return jsonOrThrow<DeviceListResponse>(res);
}

export async function listDevices(): Promise<DeviceSummary[]> {
  const body = await listDeviceResponse();
  return body.devices.map((d) => ({ name: d.name, nickname: d.nickname ?? d.name, ip: d.ip ?? "", model: d.model ?? "" }));
}

export async function scanDevices(): Promise<DeviceListResponse> {
  const res = await fetch(`${BASE}/api/scan`, { method: "POST" });
  return jsonOrThrow<DeviceListResponse>(res);
}

export async function setDevicePower(name: string, on: boolean): Promise<Device> {
  const res = await fetch(`${BASE}/api/devices/${encodeURIComponent(name)}/power`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ on }) });
  return jsonOrThrow<Device>(res);
}

export async function toggleDevice(name: string): Promise<Device> {
  const res = await fetch(`${BASE}/api/devices/${encodeURIComponent(name)}/toggle`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({}) });
  return jsonOrThrow<Device>(res);
}

export async function releaseDeviceOverride(name: string): Promise<Device> {
  const res = await fetch(`${BASE}/api/devices/${encodeURIComponent(name)}/release-override`, { method: "POST" });
  return jsonOrThrow<Device>(res);
}

export async function energyHistory(range = "7d"): Promise<UsageHistoryResponse> {
  const res = await fetch(`${BASE}/api/energy/history.json?range=${encodeURIComponent(range)}`);
  return jsonOrThrow<UsageHistoryResponse>(res);
}

export async function listHooks(): Promise<HookSummary[]> {
  const res = await fetch(`${BASE}/api/hooks`);
  const body = await jsonOrThrow<{ hooks: Hook[] }>(res);
  return body.hooks.map((h) => ({ id: h.id, name: h.name }));
}

export async function listHookDetails(): Promise<Hook[]> {
  const res = await fetch(`${BASE}/api/hooks`);
  const body = await jsonOrThrow<{ hooks: Hook[] }>(res);
  return body.hooks;
}

export async function createHook(input: Partial<Hook>): Promise<Hook> {
  const res = await fetch(`${BASE}/api/hooks`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(input) });
  return jsonOrThrow<Hook>(res);
}

export async function updateHook(id: string, input: Partial<Hook>): Promise<Hook> {
  const res = await fetch(`${BASE}/api/hooks/${id}`, { method: "PATCH", headers: { "content-type": "application/json" }, body: JSON.stringify(input) });
  return jsonOrThrow<Hook>(res);
}

export async function deleteHook(id: string): Promise<void> {
  const res = await fetch(`${BASE}/api/hooks/${id}`, { method: "DELETE" });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
}

export async function testHook(id: string): Promise<Hook> {
  const res = await fetch(`${BASE}/api/hooks/${id}/test`, { method: "POST" });
  return jsonOrThrow<Hook>(res);
}
