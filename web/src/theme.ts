export type ThemeName = "classic" | "dark";
const STORAGE_KEY = "fusebox-theme";

export function getInitialTheme(): ThemeName {
  const stored = localStorage.getItem(STORAGE_KEY);
  if (stored === "classic" || stored === "dark") return stored;
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "classic";
}

export function setTheme(theme: ThemeName) {
  document.documentElement.dataset.theme = theme;
  localStorage.setItem(STORAGE_KEY, theme);
  const themeColor = document.getElementById("theme-color");
  if (themeColor) themeColor.setAttribute("content", theme === "dark" ? "#201d19" : "#d7c39a");
}
