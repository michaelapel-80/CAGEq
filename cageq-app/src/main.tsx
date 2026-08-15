import React from "react";
import ReactDOM from "react-dom/client";
import "./i18n";
import App from "./App";
import { ScopeWindow } from "./ScopeWindow";

// The detached vectorscope window loads this same bundle at `index.html#scope` — render only the
// scope there, not the whole app (no second monitor, no duplicated state). Everything else is App.
const isScopeWindow = window.location.hash.replace(/^#/, "") === "scope";
// Both windows load this same bundle (and so the same App.css), but the scope window's fixed,
// full-viewport view never scrolls — flag it so CSS can skip the main window's reserved
// scrollbar gutter there instead of showing it as a permanent empty bar (see App.css `:root`).
if (isScopeWindow) document.documentElement.dataset.window = "scope";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{isScopeWindow ? <ScopeWindow /> : <App />}</React.StrictMode>,
);
