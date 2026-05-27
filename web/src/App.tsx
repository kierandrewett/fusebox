import { useCallback, useEffect, useState } from "react";
import { AutomationsTab } from "./automations/AutomationsTab";
import { DevicesTab } from "./devices/DevicesTab";
import { HooksPanel } from "./hooks/HooksPanel";
import { playSwitchSound, preloadSwitchSound } from "./audio";
import { getInitialTheme, setTheme, type ThemeName } from "./theme";
import { scanDevices } from "./api";
import type { DeviceListResponse } from "./types";

type TabName = "devices" | "hooks" | "automations";

export function App() {
  const [tab, setTab] = useState<TabName>("devices");
  const [theme, setThemeState] = useState<ThemeName>(() => getInitialTheme());
  const [scanning, setScanning] = useState(false);
  const [scanResult, setScanResult] = useState<DeviceListResponse | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    setTheme(theme);
    preloadSwitchSound();
  }, [theme]);

  const toggleTheme = useCallback(() => {
    setThemeState((current) => (current === "dark" ? "classic" : "dark"));
  }, []);

  const handleScan = useCallback(async () => {
    setScanning(true);
    setNotice(null);
    try {
      const payload = await scanDevices();
      setScanResult(payload);
      if (payload.scan_error) setNotice(payload.scan_error);
    } catch (err) {
      setNotice(String(err));
    } finally {
      setScanning(false);
    }
  }, []);

  return (
    <main className={`shell ${tab === "automations" ? "shell-wide" : ""}`}>
      <header className="header">
        <h1>Fusebox</h1>
        <div className="header-actions">
          <button
            type="button"
            className="theme-button"
            aria-pressed={theme === "dark"}
            onClick={() => {
              playSwitchSound();
              toggleTheme();
            }}
          >
            {theme === "dark" ? "Classic mode" : "Dark mode"}
          </button>
          <a className="export-link" href="/api/energy/export.xlsx" download title="Download a workbook generated from Tapo history readings">
            Export xlsx
          </a>
          <button
            type="button"
            className="scan-button"
            disabled={scanning}
            onClick={handleScan}
          >
            {scanning ? "Scanning" : "Scan now"}
          </button>
        </div>
      </header>

      <nav className="tab-bar" role="tablist" aria-label="Sections">
        <button
          type="button"
          role="tab"
          className="tab-button"
          aria-selected={tab === "devices"}
          onClick={() => setTab("devices")}
        >
          Devices
        </button>
        <button
          type="button"
          role="tab"
          className="tab-button"
          aria-selected={tab === "hooks"}
          onClick={() => setTab("hooks")}
        >
          Hooks
        </button>
        <button
          type="button"
          role="tab"
          className="tab-button"
          aria-selected={tab === "automations"}
          onClick={() => setTab("automations")}
        >
          Automations
        </button>
      </nav>

      {notice ? <p className="notice" role="status">{notice}</p> : null}

      {tab === "devices" ? <DevicesTab scanOverride={scanResult} onClearScanOverride={() => setScanResult(null)} /> : null}
      {tab === "hooks" ? <HooksPanel /> : null}
      {tab === "automations" ? <AutomationsTab /> : null}
    </main>
  );
}
