import React from "react";
import ReactDOM from "react-dom/client";
import "./i18n";
import App from "./App";
import { ScopeWindow } from "./ScopeWindow";
import { ToneWindow } from "./ToneWindow";

// Detached pop-out windows load this same bundle at a hash route — render only that window's own
// content, not the whole app (no second monitor, no duplicated state). Everything else is App.
const hash = window.location.hash.replace(/^#/, "");
const isScopeWindow = hash === "scope";
const isToneWindow = hash === "tone";
// Both windows load this same bundle (and so the same App.css), but the scope window's fixed,
// full-viewport view never scrolls — flag it so CSS can skip the main window's reserved
// scrollbar gutter there instead of showing it as a permanent empty bar (see App.css `:root`).
if (isScopeWindow) document.documentElement.dataset.window = "scope";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{isScopeWindow ? <ScopeWindow /> : isToneWindow ? <ToneWindow /> : <App />}</React.StrictMode>,
);
