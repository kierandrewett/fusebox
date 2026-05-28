import { useEffect, useRef } from "react";
import type { CreateEditorResult } from "./createEditor";

interface Props {
  x: number;
  y: number;
  /** Whether the right-click landed on a node. */
  onNode: boolean;
  api: CreateEditorResult;
  onDelete: () => void;
  onClose: () => void;
}

export function CanvasContextMenu({ x, y, onNode, api, onDelete, onClose }: Props) {
  const ref = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    const onDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    // Defer so the click/contextmenu that opened the menu doesn't close it.
    const timer = window.setTimeout(() => {
      document.addEventListener("pointerdown", onDown);
      document.addEventListener("keydown", onKey);
    }, 0);
    return () => {
      window.clearTimeout(timer);
      document.removeEventListener("pointerdown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  const run = (fn: () => void) => {
    fn();
    onClose();
  };

  const items: Array<{ label: string; run: () => void }> = [];
  if (onNode) {
    items.push({ label: "Copy", run: () => api.copySelection() });
    items.push({
      label: "Duplicate",
      run: () => {
        api.copySelection();
        void api.paste();
      },
    });
    items.push({ label: "Delete", run: onDelete });
  }
  items.push({ label: "Paste", run: () => void api.paste() });
  items.push({ label: "Select all", run: () => api.selectAll() });

  return (
    <div ref={ref} className="fb-context-menu" style={{ left: x, top: y }} role="menu">
      {items.map((it) => (
        <button
          key={it.label}
          type="button"
          role="menuitem"
          className="fb-context-item"
          onClick={() => run(it.run)}
        >
          {it.label}
        </button>
      ))}
    </div>
  );
}
