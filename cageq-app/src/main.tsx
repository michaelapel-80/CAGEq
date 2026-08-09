import React from "react";
import ReactDOM from "react-dom/client";
import "./i18n";
import App from "./App";
import { ScopeWindow } from "./ScopeWindow";

// The detached vectorscope window loads this same bundle at `index.html#scope` — render only the
// scope there, not the whole app (no second monitor, no duplicated state). Everything else is App.
const isScopeWindow = window.location.hash.replace(/^#/, "") === "scope";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{isScopeWindow ? <ScopeWindow /> : <App />}</React.StrictMode>,
);
