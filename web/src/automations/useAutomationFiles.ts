import { exportAutomation, importAutomation } from "../api";
import type { Automation } from "../types";

interface Options {
  selected: Automation | null;
  setError: (error: string | null) => void;
  onImported: (created: Automation) => void;
}

/** Export (download) and import (upload) handlers for automations. */
export function useAutomationFiles({ selected, setError, onImported }: Options) {
  const handleExport = async () => {
    if (!selected) return;
    setError(null);
    try {
      const data = await exportAutomation(selected.id);
      const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const slug =
        (selected.name || "automation")
          .toLowerCase()
          .replace(/[^a-z0-9]+/g, "-")
          .replace(/(^-|-$)/g, "") || "automation";
      const a = document.createElement("a");
      a.href = url;
      a.download = `${slug}.fusebox.json`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      setError(String(e));
    }
  };

  const handleImport = async (file: File) => {
    setError(null);
    try {
      const payload = JSON.parse(await file.text());
      onImported(await importAutomation(payload));
    } catch (e) {
      setError(`import failed: ${e instanceof Error ? e.message : String(e)}`);
    }
  };

  return { handleExport, handleImport };
}
