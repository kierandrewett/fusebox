import { useEffect, useRef } from "react";
import { Chart, LineController, LineElement, PointElement, LinearScale, TimeScale, Tooltip, Legend, type ChartConfiguration } from "chart.js";
import type { UsageHistoryResponse } from "../types";

Chart.register(LineController, LineElement, PointElement, LinearScale, TimeScale, Tooltip, Legend);

export function UsageChart({ history }: { history: UsageHistoryResponse | null }) {
  const ref = useRef<HTMLCanvasElement | null>(null);
  useEffect(() => {
    if (!ref.current || !history) return;
    const config: ChartConfiguration = {
      type: "line",
      data: { datasets: [{ label: "Total", data: history.total.map((p) => ({ x: p.timestamp_ms, y: p.value })), borderColor: "#c19b55", backgroundColor: "rgba(193,155,85,0.2)", tension: 0.25 }] },
      options: { responsive: true, parsing: false, scales: { x: { type: "linear" }, y: { beginAtZero: true } } },
    };
    const chart = new Chart(ref.current, config);
    return () => chart.destroy();
  }, [history]);
  return <canvas ref={ref} aria-label="Energy usage chart" role="img" />;
}
