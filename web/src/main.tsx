import { createRoot, type Root } from "react-dom/client";
import { App } from "./App";
import styleSheet from "./styles.css?inline";

let stylesInjected = false;
function injectStyles() {
  if (stylesInjected) return;
  stylesInjected = true;
  const tag = document.createElement("style");
  tag.dataset.fusebox = "automations";
  tag.textContent = styleSheet;
  document.head.appendChild(tag);
}

declare global {
  interface Window {
    FuseboxAutomations?: {
      mount: (container: HTMLElement) => () => void;
    };
  }
}

const api = {
  mount(container: HTMLElement) {
    injectStyles();
    container.innerHTML = "";
    const root: Root = createRoot(container);
    root.render(<App />);
    return () => root.unmount();
  },
};

if (typeof window !== "undefined") {
  window.FuseboxAutomations = api;
}

export default api;
