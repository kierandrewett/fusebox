import { createRoot, type Root } from "react-dom/client";
import { App } from "./App";
import styleSheet from "./styles.css?inline";

let stylesInjected = false;
let root: Root | null = null;

function injectStyles() {
  if (stylesInjected) return;
  stylesInjected = true;
  const tag = document.createElement("style");
  tag.dataset.fusebox = "app";
  tag.textContent = styleSheet;
  document.head.appendChild(tag);
}

export function mount(container: HTMLElement) {
  injectStyles();
  container.innerHTML = "";
  root = createRoot(container);
  root.render(<App />);
  return () => unmount();
}

export function unmount() {
  root?.unmount();
  root = null;
}

declare global {
  interface Window {
    Fusebox?: { mount: typeof mount; unmount: typeof unmount };
  }
}

if (typeof window !== "undefined") {
  window.Fusebox = { mount, unmount };
  const container = document.getElementById("app-root");
  if (container) mount(container);
}
