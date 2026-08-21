import { useState } from "react";

/**
 * Backs a scope/analyzer view's on-screen tuning panel (glow, trail, tail, beam, …) with a single
 * user-saved default per view, persisted in localStorage — same lightweight-preference mechanism
 * `cageq-theme` already uses in App.tsx, not the app's real document state (slot contents, filters,
 * the "resume blob"), which is deliberately kept separate and lives on the backend. Explicitly one
 * slot, not named presets: "save as default" overwrites whatever was there, "reset to factory"
 * clears it back to the hard-coded `defaults` — there's no list to manage.
 *
 * Merged over `defaults` on load (`{ ...defaults, ...stored }`) rather than used as-is, so a saved
 * blob from an older version missing a since-added field still gets that field's current default
 * instead of `undefined` leaking into the live params.
 */
export function useTunableParams<T extends object>(storageKey: string, defaults: T) {
  const [params, setParams] = useState<T>(() => {
    try {
      const raw = localStorage.getItem(storageKey);
      return raw ? { ...defaults, ...JSON.parse(raw) } : defaults;
    } catch {
      return defaults; // corrupt/foreign JSON in that key — fall back rather than crash the view
    }
  });

  const saveAsDefault = () => {
    try {
      localStorage.setItem(storageKey, JSON.stringify(params));
    } catch {
      // Cosmetic preference, not worth surfacing an error for (private-mode/quota localStorage
      // failures are rare and inconsequential here — the view just keeps its in-memory params).
    }
  };

  const resetToFactory = () => {
    localStorage.removeItem(storageKey);
    setParams(defaults);
  };

  return { params, setParams, saveAsDefault, resetToFactory };
}
