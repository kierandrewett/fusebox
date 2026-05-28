import { useEffect, useRef } from "react";
import type { Chart as ChartType, ChartConfiguration } from "chart.js";
import type { UsageHistoryResponse } from "../types";

// Heavy library — loaded lazily so it doesn't block the initial bundle.
let chartCtorPromise: Promise<typeof ChartType> | null = null;
async function loadChart(): Promise<typeof ChartType> {
  if (!chartCtorPromise) {
    chartCtorPromise = import("chart.js").then((mod) => {
      mod.Chart.register(
        mod.LineController,
        mod.LineElement,
        mod.PointElement,
        mod.LinearScale,
        mod.Tooltip,
        mod.Legend,
        mod.Filler,
      );
      return mod.Chart;
    });
  }
  return chartCtorPromise;
}

export type HistoryRange =
  | "5m" | "30m" | "1h" | "6h" | "12h"
  | "1d" | "3d" | "7d" | "30d"
  | "3m" | "6m" | "1y" | "ytd" | "all";

const RANGE_BUTTONS: { key: HistoryRange; label: string }[] = [
  { key: "5m", label: "5m" }, { key: "30m", label: "30m" },
  { key: "1h", label: "1h" }, { key: "6h", label: "6h" },
  { key: "12h", label: "12h" }, { key: "1d", label: "1d" },
  { key: "3d", label: "3d" }, { key: "7d", label: "7d" },
  { key: "30d", label: "30d" }, { key: "3m", label: "3m" },
  { key: "6m", label: "6m" }, { key: "1y", label: "1y" },
  { key: "ytd", label: "YTD" }, { key: "all", label: "All" },
];

interface Props {
  history: UsageHistoryResponse | null;
  range: HistoryRange;
  onRangeChange: (range: HistoryRange) => void;
}

export function UsageChart({ history, range, onRangeChange }: Props) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const chartRef = useRef<ChartType | null>(null);

  const hasData = !!history && history.totals.length > 0;

  useEffect(() => {
    if (!canvasRef.current) return;
    if (chartRef.current) {
      chartRef.current.destroy();
      chartRef.current = null;
    }
    if (!history || history.totals.length === 0) return;

    let cancelled = false;
    const unit = (history as any).unit ?? "W";
    const config: ChartConfiguration = {
      type: "line",
      data: {
        datasets: [
          {
            label: "Total",
            data: history.totals.map((p) => ({ x: p.timestamp_ms, y: p.value })),
            borderColor: "#c19b55",
            backgroundColor: "rgba(193, 155, 85, 0.18)",
            tension: 0.25,
            fill: true,
            pointRadius: 0,
          },
          ...history.series.map((s, i) => ({
            label: s.device_name,
            data: s.points.map((p) => ({ x: p.timestamp_ms, y: p.value })),
            borderColor: PALETTE[i % PALETTE.length],
            backgroundColor: "transparent",
            tension: 0.25,
            pointRadius: 0,
          })),
        ],
      },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        parsing: false,
        scales: {
          x: {
            type: "linear",
            ticks: { color: "rgba(229,216,182,0.6)", maxTicksLimit: 8, callback: (v) => formatTick(Number(v)) },
            grid: { color: "rgba(229,216,182,0.08)" },
          },
          y: {
            beginAtZero: true,
            ticks: { color: "rgba(229,216,182,0.6)", callback: (v) => `${v} ${unit}` },
            grid: { color: "rgba(229,216,182,0.08)" },
          },
        },
        plugins: {
          legend: {
            labels: {
              color: "rgba(229,216,182,0.7)",
              usePointStyle: true,
              pointStyle: "line",
              boxWidth: 22,
              boxHeight: 2,
            },
          },
          tooltip: {
            mode: "index",
            intersect: false,
            callbacks: {
              title: (items) => {
                const ms = items[0]?.parsed?.x;
                if (typeof ms !== "number") return "";
                return formatTooltipTime(ms);
              },
              label: (item) => {
                const series = item.dataset.label ?? "";
                const value = item.parsed.y;
                if (typeof value !== "number") return series;
                return `${series}: ${formatValue(value)} ${unit}`;
              },
            },
          },
        },
      },
    };
    void loadChart().then((ChartCtor) => {
      if (cancelled || !canvasRef.current) return;
      chartRef.current = new ChartCtor(canvasRef.current, config);
    });
    return () => {
      cancelled = true;
      chartRef.current?.destroy();
      chartRef.current = null;
    };
  }, [history]);

  const rangeLabel = (history as any)?.range_label ?? `${range} usage`;

  return (
    <section className="usage-panel" aria-labelledby="usage-title">
      <div className="usage-header">
        <h2 id="usage-title">{rangeLabel}</h2>
        <span>{hasData ? `${history?.series.length ?? 0} series` : "No data"}</span>
      </div>
      <div className="usage-range-controls" aria-label="Energy usage history range">
        {RANGE_BUTTONS.map((b) => (
          <button
            key={b.key}
            type="button"
            className="range-button"
            aria-pressed={range === b.key}
            onClick={() => onRangeChange(b.key)}
          >
            {b.label}
          </button>
        ))}
      </div>
      <div className="usage-chart-container">
        <canvas ref={canvasRef} className="usage-chart" aria-label="Usage history for each energy-monitoring plug over the selected range." role="img" />
      </div>
      {!hasData ? <p className="usage-empty">No power history yet.</p> : null}
    </section>
  );
}

const PALETTE = ["#e5b75b", "#7bb7ff", "#f06b5c", "#c99cff", "#62d6d1", "#ff9d66", "#b6e36a", "#f38ad3"];

function formatTick(ms: number): string {
  const d = new Date(ms);
  const now = Date.now();
  const span = Math.abs(now - ms);
  if (span < 24 * 3600 * 1000) {
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  return d.toLocaleDateString([], { day: "2-digit", month: "short" });
}

function formatTooltipTime(ms: number): string {
  const d = new Date(ms);
  return d.toLocaleString([], {
    weekday: "short",
    day: "2-digit",
    month: "short",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function formatValue(value: number): string {
  if (Math.abs(value) >= 1000) return value.toFixed(0);
  if (Math.abs(value) >= 100) return value.toFixed(0);
  if (Math.abs(value) >= 10) return value.toFixed(1);
  return value.toFixed(2);
}
