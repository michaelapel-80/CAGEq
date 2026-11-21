import React from "react";
import ReactDOM from "react-dom/client";
import "./i18n";
import App from "./App";
import { initBiquad } from "./biquad";
import { ScopeWindow } from "./ScopeWindow";
import { ToneWindow } from "./ToneWindow";

// Detached pop-out windows load this same bundle at a hash route — render only that window's own
// content, not the whole app (no second monitor, no duplicated state). Everything else is App.
const hash = window.location.hash.replace(/^#/, "");
const scopeInstrument = hash === "scope" ? "vector" : hash === "timescope" ? "time" : null;
const isScopeWindow = scopeInstrument !== null;
const isToneWindow = hash === "tone";
// Every window loads this same bundle (and so the same App.css), but a scope window's fixed,
// full-viewport view never scrolls — flag it so CSS can skip the main window's reserved
// scrollbar gutter there instead of showing it as a permanent empty bar (see App.css `:root`).
if (isScopeWindow) document.documentElement.dataset.window = "scope";

// Every window draws curves, and every curve is designed by the WebAssembly module
// (`biquad.ts`) — so it must be instantiated before the first render, in every window (each
// is its own JS context). `.then` rather than top-level await: Vite's default build target
// predates it.
void initBiquad().then(() => {
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>{scopeInstrument ? <ScopeWindow instrument={scopeInstrument} /> : isToneWindow ? <ToneWindow /> : <App />}</React.StrictMode>,
  );
});
