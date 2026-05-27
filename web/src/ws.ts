import type { DeviceListResponse } from "./types";

export function subscribeDevices(onMessage: (message: DeviceListResponse) => void) {
  let closed = false;
  let socket: WebSocket | null = null;
  let timer: number | undefined;

  const connect = () => {
    const protocol = window.location.protocol === "https:" ? "wss" : "ws";
    socket = new WebSocket(`${protocol}://${window.location.host}/ws/devices`);
    socket.onmessage = (event) => onMessage(JSON.parse(event.data) as DeviceListResponse);
    socket.onclose = () => {
      if (!closed) timer = window.setTimeout(connect, 2_000);
    };
  };

  connect();
  return () => {
    closed = true;
    if (timer) window.clearTimeout(timer);
    socket?.close();
  };
}
