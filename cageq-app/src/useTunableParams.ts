import { useEffect, useState } from "react";

function readStored<T extends object>(key: string, defaults: T): T | null {
  try {
    const raw = localStorage.getItem(key);
    // Merged over `defaults` rather than used as-is, so a stored blob from an older version
    // missing a since-added field still gets that field's current default instead of `undefined`
    // leaking into the live params.
    return raw ? { ...defaults, ...JSON.parse(raw) } : null;
  } catch {
    return null; // corrupt/foreign JSON in that key — fall back rather than crash the view
  }
}

/**
 * Backs a scope/analyzer view's on-screen tuning panel (glow, trail, tail, beam, …) with two
 * independent localStorage slots, same lightweight-preference mechanism `cageq-theme` already uses
 * in App.tsx (not the app's real document state, which lives on the backend):
 *
 *  - **Last used** (`${storageKey}-last`), auto-saved on every change, restores exactly where you
 *    left off on the next launch. This is *not* the same thing as the saved default below — that
 *    would clash whenever the two diverge (you tweak something after loading your saved default,
 *    close the app, and now which one should the next launch show?) — so this gets its own slot
 *    and is what actually seeds `params` on mount, falling back to the saved default (not straight
 *    to `defaults`) when it doesn't exist yet — e.g. right after this two-slot split first ships,
 *    so a saved default from before it existed keeps loading exactly as it always did, until
 *    something actually gets saved to (or diverges into) the new slot.
 *  - **Saved default** (`${storageKey}`), written only by an explicit `saveAsDefault()` call —
 *    a deliberate "this is my preset" snapshot, exposed as `savedDefault` so a caller can offer a
 *    "load it back" control (e.g. a third preset button) without needing to reload the view.
 *
 * `resetToFactory` clears the *last-used* slot and returns `params` to the hard-coded `defaults` —
 * it does NOT touch the saved default, which is now a persistent preset in its own right, same as
 * any other curated preset, not something Reset should discard.
 */
export function useTunableParams<T extends object>(storageKey: string, defaults: T) {
  const lastKey = `${storageKey}-last`;
  const [savedDefault, setSavedDefault] = useState<T | null>(() => readStored(storageKey, defaults));
  const [params, setParams] = useState<T>(() => readStored(lastKey, defaults) ?? savedDefault ?? defaults);

  useEffect(() => {
    try {
      localStorage.setItem(lastKey, JSON.stringify(params));
    } catch {
      // Cosmetic preference, not worth surfacing an error for (private-mode/quota localStorage
      // failures are rare and inconsequential here — the view just keeps its in-memory params).
    }
  }, [params, lastKey]);

  const saveAsDefault = () => {
    try {
      localStorage.setItem(storageKey, JSON.stringify(params));
      setSavedDefault(params);
    } catch {
      // Same rationale as the effect above.
    }
  };

  const resetToFactory = () => {
    localStorage.removeItem(lastKey);
    setParams(defaults);
  };

  return { params, setParams, saveAsDefault, resetToFactory, savedDefault };
}
