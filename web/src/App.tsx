import { useEffect, useState } from "react";
import { AutomationsTab } from "./automations/AutomationsTab";
import { DevicesTab } from "./devices/DevicesTab";
import { HooksPanel } from "./hooks/HooksPanel";
import { playSwitchSound, preloadSwitchSound } from "./audio";
import { getInitialTheme, setTheme, type ThemeName } from "./theme";

type TabName = "devices" | "hooks" | "automations";

export function App() {
  const [tab, setTab] = useState<TabName>("devices");
  const [theme, setThemeState] = useState<ThemeName>(() => getInitialTheme());

  useEffect(() => {
    setTheme(theme);
    preloadSwitchSound();
  }, [theme]);

  const toggleTheme = () => {
    playSwitchSound();
    setThemeState((current) => (current === "dark" ? "classic" : "dark"));
  };

  return (
    <div className="shell">
      <header className="topbar">
        <div>
          <p className="eyebrow">Fusebox</p>
          <h1>Tapo control panel</h1>
        </div>
        <div className="topbar-actions">
          <nav className="tabs" aria-label="Fusebox sections">
            <button type="button" className={tab === "devices" ? "active" : ""} onClick={() => setTab("devices")}>Devices</button>
            <button type="button" className={tab === "hooks" ? "active" : ""} onClick={() => setTab("hooks")}>Hooks</button>
            <button type="button" className={tab === "automations" ? "active" : ""} onClick={() => setTab("automations")}>Automations</button>
          </nav>
          <button type="button" className="theme-toggle" onClick={toggleTheme}>
            {theme === "dark" ? "Classic" : "Dark"}
          </button>
        </div>
      </header>
      <main>
        {tab === "devices" ? <DevicesTab /> : null}
        {tab === "hooks" ? <HooksPanel /> : null}
        {tab === "automations" ? <AutomationsTab /> : null}
      </main>
    </div>
  );
}
